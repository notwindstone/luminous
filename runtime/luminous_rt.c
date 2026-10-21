#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "luminous_rt.h"

uintptr_t lum_stack_limit;
uint8_t lum_unwinding;

/* Same as QuickJS js_math_pow: JS differs from IEEE pow for |a| == 1 with a
 * non-finite exponent. */
double lum_pow(double a, double b)
{
    if (!isfinite(b) && fabs(a) == 1)
        return NAN;
    if (b == 2)
        return a * a; /* as the engine's pow */
    return pow(a, b);
}

/* Same as QuickJS js_math_round (round half up, preserving -0). */
double lum_round(double a)
{
    union { double d; uint64_t u64; } u;
    uint64_t frac_mask, one;
    unsigned int e, s;

    u.d = a;
    e = (u.u64 >> 52) & 0x7ff;
    if (e < 1023) {
        if (e == (1023 - 1) && u.u64 != 0xbfe0000000000000) {
            u.u64 = (u.u64 & ((uint64_t)1 << 63)) | ((uint64_t)1023 << 52);
        } else {
            u.u64 &= (uint64_t)1 << 63;
        }
    } else if (e < (1023 + 52)) {
        s = u.u64 >> 63;
        one = (uint64_t)1 << (52 - (e - 1023));
        frac_mask = one - 1;
        u.u64 += (one >> 1) - s;
        u.u64 &= ~frac_mask;
    }
    return u.d;
}

/* ToInt32 for |d| >= 2^63, infinities and NaN (the inline fast path handles
 * the rest). */
int32_t lum_toint32_slow(double d)
{
    union { double d; uint64_t u64; } u = { .d = d };
    int e = (u.u64 >> 52) & 0x7ff;
    uint64_t m;

    if (e == 0x7ff || e < 1023)       /* NaN, Infinity, |d| < 1 */
        return 0;
    if (e > 1023 + 52 + 31)           /* all low 32 bits of the integer are 0 */
        return 0;
    m = (u.u64 & (((uint64_t)1 << 52) - 1)) | ((uint64_t)1 << 52);
    if (e >= 1023 + 52)
        m <<= e - (1023 + 52);
    else
        m >>= (1023 + 52) - e;
    if (u.u64 >> 63)
        m = -m;
    return (int32_t)(uint32_t)m;
}

/* Stack overflow detected by native code (same error as the interpreter's). */
void lum_stack_overflow(JSContext *ctx)
{
    JS_ThrowRangeError(ctx, "Maximum call stack size exceeded");
}

void lum_throw_uninitialized(JSContext *ctx, int cell)
{
    JS_ThrowReferenceError(ctx, "%s is not initialized", lum_cell_names[cell]);
}

/* ---- operations with more JSValue arguments than fit in registers ---- */

/* obj[key] = val. Takes ownership of key and val. */
int lum_set_elem(JSContext *ctx, JSValue *v)
{
    return JS_SetPropertyValue(ctx, v[0], v[1], v[2], JS_PROP_THROW);
}

/* delete obj[key]. Borrows both. */
int lum_delete_elem(JSContext *ctx, JSValue *v)
{
    JSAtom atom = JS_ValueToAtom(ctx, v[1]);
    int ret;
    if (atom == JS_ATOM_NULL)
        return -1;
    ret = JS_DeleteProperty(ctx, v[0], atom, JS_PROP_THROW);
    JS_FreeAtom(ctx, atom);
    return ret;
}

/* f.call(this, ...argv) with fthis = { f, this }. Borrows everything. */
JSValue lum_call(JSContext *ctx, JSValue *fthis, int argc, JSValue *argv)
{
    return JS_CallFast(ctx, fthis[0], fthis[1], argc, (JSValueConst *)argv);
}

/* The global `name`, or undefined (as in `typeof name`). */
JSValue lum_global_or_undefined(JSContext *ctx, JSAtom name)
{
    JSValue global = JS_GetGlobalObject(ctx);
    JSValue ret = JS_GetProperty(ctx, global, name);
    JS_FreeValue(ctx, global);
    return ret;
}

/* Object literal field. Takes ownership of val. */
int lum_define_field(JSContext *ctx, JSValue *obj, JSAtom prop, JSValue val)
{
    return JS_DefinePropertyValue(ctx, *obj, prop, val, JS_PROP_C_W_E | JS_PROP_THROW);
}

/* Array literal. Takes ownership of the elements. An empty one has room
   for a few in its own cell: it is mostly made to be pushed to. */
JSValue lum_new_array(JSContext *ctx, int count, JSValue *values)
{
    if (count == 0)
        return JS_NewArrayWithRoom(ctx, 4);
    return JS_NewArrayFrom(ctx, count, values);
}

/* ---- boxes: variables captured by closures ---- */

static JSClassID lum_box_class;
/* The shape of boxes (referenced), made with the first one. */
static void *lum_box_shape;

typedef struct LumBox {
    JSValue value;
} LumBox;

static void lum_box_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    LumBox *b = JS_GetOpaque(val, lum_box_class);
    if (b)
        JS_MarkValue(rt, b->value, mark_func);
}

/* A new box holding an uninitialized binding; *slot receives the address
   of its value, which stays valid as long as the box object lives. The
   value is in the object's own cell (nothing to finalize), which a word
   pointing into it keeps alive: native code may hold only *slot, and
   stores there are the object's (its write barrier). */
JSValue lum_box_new(JSContext *ctx, JSValue **slot)
{
    void *data;
    JSValue obj;
    if (lum_box_shape) {
        obj = JS_NewObjectClassDataShaped(ctx, lum_box_shape, lum_box_class, sizeof(LumBox), &data);
    } else {
        obj = JS_NewObjectClassData(ctx, lum_box_class, sizeof(LumBox), false, &data);
        if (!JS_IsException(obj))
            lum_box_shape = JS_DupObjectShape(obj);
    }
    if (JS_IsException(obj))
        return obj;
    ((LumBox *)data)->value = JS_UNINITIALIZED;
    *slot = &((LumBox *)data)->value;
    return obj;
}

void lum_throw_uninitialized_atom(JSContext *ctx, JSAtom name)
{
    const char *s = JS_AtomToCString(ctx, name);
    JS_ThrowReferenceError(ctx, "%s is not initialized", s ? s : "lexical variable");
    JS_FreeCString(ctx, s);
}

/* ---- async functions ---- */

/* The heap frame of a running async function: its locals while it is
   suspended, and its promise. */
typedef struct LumFrame {
    void *self;               /* the frame object (not a counted reference) */
    JSValue promise;          /* async: its promise, settled by the frame */
    int fn, nslots;
    int state;                /* where to resume */
    /* async: resumed (by its resumption job, which does nothing after
       it: the entry runs it only until it first suspends): its awaits
       may continue at once (see lum_await) */
    int in_job;
    JSValue slots[];
} LumFrame;

static JSClassID lum_frame_class;
/* The shape of frame objects, from the first (referenced). */
static void *lum_frame_shape;

/* After a store into cell `p` (an object) of a value that may be young:
 * the write barrier, if it is old and not remembered yet (the test
 * inline). */
static inline void lum_wb(JSContext *ctx, void *p)
{
    if ((((uint8_t *)p)[JS_LAYOUT_GC_FLAGS] & 0xc0) == 0x80)
        JS_GCWriteBarrier(JS_GetRuntime(ctx), p);
}

static LumFrame *lum_frame_of(JSValue *slots)
{
    return (LumFrame *)((char *)slots - offsetof(LumFrame, slots));
}

/* A new frame object for function `fn`, its slots `undefined`: the frame
 * in the object's own cell (one allocation, nothing to finalize, and no
 * zeroing of slots that are then made `undefined`). */
static JSValue lum_frame_new(JSContext *ctx, int fn, int nslots, LumFrame **pf)
{
    void *data;
    size_t size = sizeof(LumFrame) + nslots * sizeof(JSValue);
    JSValue obj;
    LumFrame *f;
    int i;
    if (lum_frame_shape) {
        obj = JS_NewObjectClassDataShaped(ctx, lum_frame_shape, lum_frame_class, size, &data);
    } else {
        obj = JS_NewObjectClassData(ctx, lum_frame_class, size, true, &data);
        if (!JS_IsException(obj))
            lum_frame_shape = JS_DupObjectShape(obj);
    }
    if (JS_IsException(obj))
        return obj;
    f = data;
    f->self = JS_VALUE_GET_PTR(obj);
    f->promise = JS_UNDEFINED;
    f->fn = fn;
    f->nslots = nslots;
    f->state = 0;
    f->in_job = 0;
    for (i = 0; i < nslots; i++)
        f->slots[i] = JS_UNDEFINED;
    *pf = f;
    return obj;
}

/* (While the step function runs, the slots hold the values of its last
   suspension.) */
static void lum_frame_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    LumFrame *f = JS_GetOpaque(val, lum_frame_class);
    int i;
    if (!f)
        return;
    JS_MarkValue(rt, f->promise, mark_func);
    for (i = 0; i < f->nslots; i++)
        JS_MarkValue(rt, f->slots[i], mark_func);
}

/* A new frame for async function `fn` (returned), its promise and slots. */
JSValue lum_async_start(JSContext *ctx, int fn, int nslots, JSValue *promise, JSValue **slots)
{
    LumFrame *f;
    JSValue obj = lum_frame_new(ctx, fn, nslots, &f);
    if (JS_IsException(obj))
        return obj;
    /* (No resolving functions: only the frame settles it.) */
    *promise = JS_NewPendingPromise(ctx);
    if (JS_IsException(*promise)) {
        JS_FreeValue(ctx, obj);
        return JS_EXCEPTION;
    }
    f->promise = JS_DupValue(ctx, *promise);
    /* (the frame object may have survived a collection since made) */
    lum_wb(ctx, JS_VALUE_GET_PTR(obj));
    *slots = f->slots;
    return obj;
}

/* Run the async function from `state` (0: start) with `value` delivered at
   its await (thrown there if `is_throw`); settle its promise on completion. */
void lum_async_step(JSContext *ctx, JSValue *slots, int state, JSValue *value, int is_throw)
{
    LumFrame *f = lum_frame_of(slots);
    JSValue r = lum_steps[f->fn](ctx, slots, state, value, is_throw);
    JSValue arg;
    int is_reject;
    /* (the slots: written by the step) */
    lum_wb(ctx, f->self);
    if (JS_IsUninitialized(r))
        return; /* suspended at an await */
    is_reject = JS_IsException(r);
    arg = is_reject ? JS_GetException(ctx) : r;
    JS_SettlePromise(ctx, f->promise, arg, is_reject);
    JS_FreeValue(ctx, arg);
}

/* The promise of a call of an async function that completed without
 * suspending (one that never awaits): settled with its result `v`
 * (taken), as its frame would settle it; rejected with the exception if
 * `v` is JS_EXCEPTION. */
JSValue lum_async_settled(JSContext *ctx, JSValue v)
{
    int is_reject = JS_IsException(v);
    JSValue promise;
    if (is_reject)
        v = JS_GetException(ctx);
    promise = JS_NewPromiseSettledWith(ctx, v, is_reject);
    JS_FreeValue(ctx, v);
    return promise;
}

/* The job that resumes a frame (argv[0]) with the value its await settled
 * with (argv[1]), thrown there if it was rejected (argv[2]): queued where
 * the reaction job to the awaited promise would be. */
static JSValue lum_resume_job(JSContext *ctx, int argc, JSValueConst *argv)
{
    LumFrame *f = JS_GetOpaque(argv[0], lum_frame_class);
    JSValue value = argv[1];
    f->in_job = 1;
    lum_async_step(ctx, f->slots, f->state, &value, JS_VALUE_GET_BOOL(argv[2]));
    return JS_UNDEFINED;
}

/* `await value` at resume point `state`. Takes ownership of `value`.
   Returns the value to continue with at once (a new reference) when
   suspending could not be told apart: the frame runs last in its
   resumption job, no other job is pending, and the value is settled (see
   JS_AwaitNativeOrValue). Else registers the continuation and returns
   JS_UNINITIALIZED (suspended), or JS_EXCEPTION. */
JSValue lum_await(JSContext *ctx, JSValue *slots, JSValue value, int state)
{
    LumFrame *f = lum_frame_of(slots);
    f->state = state;
    /* (The reaction resumes the frame from its job: no functions.) */
    return JS_AwaitNativeOrValue(ctx, value, lum_resume_job, JS_MKPTR(JS_TAG_OBJECT, f->self), f->in_job);
}

/* ---- module state: cells, string constants, atoms ---- */

static JSClassID lum_state_class;
static int lum_structs_init(JSContext *ctx);
static void lum_structs_free(JSRuntime *rt);
/* `aot type`s from JSON (see lum_struct_convert). Per field
 * (lum_struct_atom_base): the desc + 1 of the objects or arrays it holds
 * in every member that has it, else 0. */
static int32_t *lum_field_sub;
/* Per field: LUM_JPF_* (what typed parsing may store as is). */
static uint8_t *lum_field_jp;
#define LUM_JPF_STRING 1 /* any string (a member holds strings) */
#define LUM_JPF_NULL 2   /* null (a member's is nullable) */
static int lum_field_sub_init(JSContext *ctx, int total);
/* `r.json()` where its result is converted: (r, desc) => a promise, or
 * undefined (then r.json() is called). Set by the host. */
static JSValue lum_json_hook = JS_UNDEFINED;
static JSAtom lum_atom_json;
static int lum_sarr_init(JSContext *ctx);
/* `Symbol.iterator` and `next`, made with the pins. */
static JSAtom lum_atom_iterator = JS_ATOM_NULL, lum_atom_next = JS_ATOM_NULL, lum_atom_string_length = JS_ATOM_NULL,
              lum_atom_char_code_at = JS_ATOM_NULL, lum_atom_push = JS_ATOM_NULL;
/* `length`, for arrays of `aot type` objects. */
static JSAtom lum_atom_length = JS_ATOM_NULL;

static int lum_ic_count(void);

/* (No native code, no record types.) */
static int lum_record_count(void)
{
    return &lum_nrecords ? lum_nrecords : 0;
}

static int lum_fn_cache_count(void)
{
    return &lum_nfn_caches ? lum_nfn_caches : 0;
}

/* The bytecode of regular expression literals, by the string constant of
   their pattern (with the flags of the first one evaluated: another
   literal of that pattern is compiled each time): compiled when first
   evaluated. */
static JSValue *lum_regexp_bc;
static int *lum_regexp_flags;

/* String.prototype's shape and charCodeAt's slot in it (see
   lum_char_code_at_ok). */
static void *lum_cca_shape;
static int lum_cca_slot;

/* The caches given shapes since those of the others were made old: a minor
   collection, which need not mark old cells, marks the shapes of these
   only (some 16,000 caches in a program with Beyond's library and a
   framework: marking them all was a fifth of each minor collection). Each
   is listed once (lum_ic_listed), until its shapes are old; all of them
   are marked if the list could not grow (lum_young_all). */
static uint32_t *lum_young_ics;
static uint32_t lum_nyoung_ics, lum_young_cap;
static uint8_t *lum_ic_listed;
static bool lum_young_all;

static void lum_state_finalizer(JSRuntime *rt, JSValueConst val)
{
    int i;
    lum_structs_free(rt);
    for (i = 0; i < lum_fn_cache_count(); i++) {
        if (lum_fn_caches[i].shape)
            JS_FreeCachedShape(rt, lum_fn_caches[i].shape);
        lum_fn_caches[i].shape = NULL;
        lum_fn_caches[i].info = 0;
        if (lum_fn_caches[i].name != JS_ATOM_NULL)
            JS_FreeAtomRT(rt, lum_fn_caches[i].name);
        lum_fn_caches[i].name = JS_ATOM_NULL;
    }
    if (lum_cca_shape)
        JS_FreeCachedShape(rt, lum_cca_shape);
    lum_cca_shape = NULL;
    if (lum_frame_shape)
        JS_FreeCachedShape(rt, lum_frame_shape);
    lum_frame_shape = NULL;
    if (lum_box_shape)
        JS_FreeCachedShape(rt, lum_box_shape);
    lum_box_shape = NULL;
    for (i = 0; i < lum_ncells; i++) {
        JS_FreeValueRT(rt, lum_cells[i]);
        lum_cells[i] = JS_UNINITIALIZED;
    }
    for (i = 0; i < lum_nstrings; i++) {
        JS_FreeValueRT(rt, lum_strings[i]);
        lum_strings[i] = JS_UNDEFINED;
    }
    if (lum_regexp_bc) {
        js_free_rt(rt, lum_regexp_bc);
        js_free_rt(rt, lum_regexp_flags);
        lum_regexp_bc = NULL;
        lum_regexp_flags = NULL;
    }
    for (i = 0; i < lum_nprivates; i++) {
        JS_FreeValueRT(rt, lum_privates[i]);
        lum_privates[i] = JS_UNDEFINED;
    }
    for (i = 0; i < lum_nhomes; i++) {
        JS_FreeValueRT(rt, lum_homes[i]);
        lum_homes[i] = JS_UNDEFINED;
    }
    for (i = 0; i < lum_natoms; i++)
        JS_FreeAtomRT(rt, lum_atoms[i]);
    JS_FreeAtomRT(rt, lum_atom_iterator);
    JS_FreeAtomRT(rt, lum_atom_next);
    JS_FreeAtomRT(rt, lum_atom_string_length);
    JS_FreeAtomRT(rt, lum_atom_char_code_at);
    JS_FreeAtomRT(rt, lum_atom_push);
    lum_atom_iterator = lum_atom_next = lum_atom_string_length = lum_atom_char_code_at = JS_ATOM_NULL;
    lum_atom_push = JS_ATOM_NULL;
    for (i = 0; i < LUM_PINS; i++) {
        JS_FreeValueRT(rt, lum_pins[i]);
        lum_pins[i] = JS_UNDEFINED;
    }
    for (i = 0; i < lum_record_count() * LUM_RECORD_STRIDE; i++) {
        if (i % LUM_RECORD_STRIDE < LUM_RECORD_SHAPES && lum_record_shapes[i])
            JS_FreeCachedShape(rt, lum_record_shapes[i]);
        lum_record_shapes[i] = NULL;
    }
    for (i = 0; i < lum_ic_count(); i++) {
        if (lum_ics[i].shape)
            JS_FreeCachedShape(rt, lum_ics[i].shape);
        lum_ics[i].shape = NULL;
        if (lum_ics[i].proto_shape)
            JS_FreeCachedShape(rt, lum_ics[i].proto_shape);
        lum_ics[i].proto_shape = NULL;
        if (lum_ics[i].mid_shape)
            JS_FreeCachedShape(rt, lum_ics[i].mid_shape);
        lum_ics[i].mid_shape = NULL;
        if (lum_ics[i].alt_shape)
            JS_FreeCachedShape(rt, lum_ics[i].alt_shape);
        lum_ics[i].alt_shape = NULL;
        if (lum_ics[i].poly) {
            int k;
            for (k = 0; k < LUM_POLY; k++) {
                LumIC *e = &lum_ics[i].poly[k];
                if (e->shape)
                    JS_FreeCachedShape(rt, e->shape);
                if (e->alt_shape)
                    JS_FreeCachedShape(rt, e->alt_shape);
                if (e->proto_shape)
                    JS_FreeCachedShape(rt, e->proto_shape);
                if (e->mid_shape)
                    JS_FreeCachedShape(rt, e->mid_shape);
                if (e->mid2_shape)
                    JS_FreeCachedShape(rt, e->mid2_shape);
            }
            js_free_rt(rt, lum_ics[i].poly);
            lum_ics[i].poly = NULL;
        }
    }
    js_free_rt(rt, lum_young_ics);
    js_free_rt(rt, lum_ic_listed);
    lum_young_ics = NULL;
    lum_ic_listed = NULL;
    lum_nyoung_ics = lum_young_cap = 0;
    lum_young_all = false;
}

/* The cells are reachable from native code at any time, so they are roots
   for the cycle collector: marking them through this object (which the
   module keeps alive) lets cycles that go through cells be collected once
   the module itself goes away. */
static void lum_state_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func);
static void lum_mark_strings(JSRuntime *rt, JS_MarkFunc *mark_func);

/* The tables above, among the collector's roots from the start (before the
   state object holds them), with the strings. */
static void lum_mark_statics(JSRuntime *rt, void *opaque, JS_MarkFunc *mark_func)
{
    lum_state_mark(rt, JS_UNDEFINED, mark_func);
    lum_mark_strings(rt, mark_func);
}

static void lum_mark_shape(JSRuntime *rt, void *shape, JS_MarkFunc *mark_func)
{
    if (shape)
        JS_MarkCachedShape(rt, shape, mark_func);
}

static void lum_ic_mark(JSRuntime *rt, LumIC *ic, JS_MarkFunc *mark_func)
{
    lum_mark_shape(rt, ic->shape, mark_func);
    lum_mark_shape(rt, ic->proto_shape, mark_func);
    lum_mark_shape(rt, ic->mid_shape, mark_func);
    lum_mark_shape(rt, ic->alt_shape, mark_func);
    if (ic->poly) {
        int k;
        for (k = 0; k < LUM_POLY; k++) {
            LumIC *e = &ic->poly[k];
            lum_mark_shape(rt, e->shape, mark_func);
            lum_mark_shape(rt, e->alt_shape, mark_func);
            lum_mark_shape(rt, e->proto_shape, mark_func);
            lum_mark_shape(rt, e->mid_shape, mark_func);
            lum_mark_shape(rt, e->mid2_shape, mark_func);
        }
    }
}

static bool lum_old_shape(void *shape)
{
    return !shape || JS_IsOldCell(shape);
}

/* Whether the shapes `ic` holds are old. */
static bool lum_ic_old(const LumIC *ic)
{
    int k;
    if (!lum_old_shape(ic->shape) || !lum_old_shape(ic->proto_shape) || !lum_old_shape(ic->mid_shape) ||
        !lum_old_shape(ic->alt_shape))
        return false;
    for (k = 0; ic->poly && k < LUM_POLY; k++) {
        const LumIC *e = &ic->poly[k];
        if (!lum_old_shape(e->shape) || !lum_old_shape(e->alt_shape) || !lum_old_shape(e->proto_shape) ||
            !lum_old_shape(e->mid_shape) || !lum_old_shape(e->mid2_shape))
            return false;
    }
    return true;
}


static void lum_ic_note(JSRuntime *rt, LumIC *ic)
{
    ptrdiff_t i = ic - lum_ics;
    if (lum_young_all)
        return;
    if (i < 0 || i >= lum_ic_count()) {
        lum_young_all = true;
        return;
    }
    if (!lum_ic_listed && !(lum_ic_listed = js_mallocz_rt(rt, lum_ic_count()))) {
        lum_young_all = true;
        return;
    }
    if (lum_ic_listed[i])
        return;
    if (lum_nyoung_ics == lum_young_cap) {
        uint32_t cap = lum_young_cap ? 2 * lum_young_cap : 256;
        uint32_t *a = js_realloc_rt(rt, lum_young_ics, cap * sizeof(*a));
        if (!a) {
            lum_young_all = true;
            return;
        }
        lum_young_ics = a;
        lum_young_cap = cap;
    }
    lum_ic_listed[i] = 1;
    lum_young_ics[lum_nyoung_ics++] = (uint32_t)i;
}

static void lum_state_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    int kind = JS_GCMarkKind(rt, mark_func);
    uint32_t k, n;
    int i;
    for (i = 0; i < lum_fn_cache_count(); i++) {
        if (lum_fn_caches[i].shape)
            JS_MarkCachedShape(rt, lum_fn_caches[i].shape, mark_func);
    }
    if (lum_cca_shape)
        JS_MarkCachedShape(rt, lum_cca_shape, mark_func);
    if (lum_frame_shape)
        JS_MarkCachedShape(rt, lum_frame_shape, mark_func);
    for (i = 0; i < lum_ncells; i++)
        JS_MarkValue(rt, lum_cells[i], mark_func);
    for (i = 0; i < lum_nprivates; i++)
        JS_MarkValue(rt, lum_privates[i], mark_func);
    for (i = 0; i < lum_nhomes; i++)
        JS_MarkValue(rt, lum_homes[i], mark_func);
    for (i = 0; i < LUM_PINS; i++)
        JS_MarkValue(rt, lum_pins[i], mark_func);
    JS_MarkValue(rt, lum_json_hook, mark_func);
    for (i = 0; i < lum_record_count() * LUM_RECORD_STRIDE; i++) {
        if (i % LUM_RECORD_STRIDE < LUM_RECORD_SHAPES && lum_record_shapes[i])
            JS_MarkCachedShape(rt, lum_record_shapes[i], mark_func);
    }
    if (kind == JS_GC_MARK_MINOR && !lum_young_all) {
        for (k = n = 0; k < lum_nyoung_ics; k++) {
            uint32_t j = lum_young_ics[k];
            lum_ic_mark(rt, &lum_ics[j], mark_func);
            if (lum_ic_old(&lum_ics[j]))
                lum_ic_listed[j] = 0;
            else
                lum_young_ics[n++] = j;
        }
        lum_nyoung_ics = n;
        return;
    }
    for (i = 0; i < lum_ic_count(); i++)
        lum_ic_mark(rt, &lum_ics[i], mark_func);
    /* (A major collection makes the cells it finds old.) */
    if (kind == JS_GC_MARK_MAJOR) {
        for (k = 0; k < lum_nyoung_ics; k++)
            lum_ic_listed[lum_young_ics[k]] = 0;
        lum_nyoung_ics = 0;
        lum_young_all = false;
    }
}

static JSValue lum_cell_get(JSContext *ctx, JSValueConst this_val, int argc,
                              JSValueConst *argv, int cell)
{
    if (JS_IsUninitialized(lum_cells[cell]))
        return lum_throw_uninitialized(ctx, cell), JS_EXCEPTION;
    return JS_DupValue(ctx, lum_cells[cell]);
}

static JSValue lum_cell_set(JSContext *ctx, JSValueConst this_val, int argc,
                              JSValueConst *argv, int cell)
{
    JSValue old = lum_cells[cell];
    if (JS_IsUninitialized(old))
        return lum_throw_uninitialized(ctx, cell), JS_EXCEPTION;
    lum_cells[cell] = JS_DupValue(ctx, argc > 0 ? argv[0] : JS_UNDEFINED);
    JS_FreeValue(ctx, old);
    return JS_UNDEFINED;
}

/* The setter of a constant's cell: as assigning to a `const`. */
static JSValue lum_cell_set_const(JSContext *ctx, JSValueConst this_val, int argc,
                                    JSValueConst *argv, int cell)
{
    if (JS_IsUninitialized(lum_cells[cell]))
        return lum_throw_uninitialized(ctx, cell), JS_EXCEPTION;
    return JS_ThrowTypeError(ctx, "'%s' is read-only", lum_cell_names[cell]);
}

/* __lum_init(cell, value): the binding's declaration ran; returns value. */
static JSValue lum_cell_init(JSContext *ctx, JSValueConst this_val, int argc,
                               JSValueConst *argv)
{
    int32_t cell;
    JSValue old;
    if (argc < 2 || JS_ToInt32(ctx, &cell, argv[0]) || cell < 0 || cell >= lum_ncells)
        return JS_ThrowRangeError(ctx, "__lum_init: bad cell");
    old = lum_cells[cell];
    lum_cells[cell] = JS_DupValue(ctx, argv[1]);
    JS_FreeValue(ctx, old);
    return JS_DupValue(ctx, argv[1]);
}

/* Set up the module state; returns the objects the module exports, or -1. */
int lum_runtime_init(JSContext *ctx, JSValue *state, JSValue *init_fn, JSValue *cells_obj)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    JSClassDef def = { "LumState", .finalizer = lum_state_finalizer, .gc_mark = lum_state_mark };
    int i;

    /* Native code allocates objects as the engine's layout says. */
    if (!JS_LayoutCheck()) {
        fprintf(stderr, "luminous: the engine's object layout is not the one native code was compiled for\n");
        abort();
    }
    if (JS_AddGCMarkHook(rt, lum_mark_statics, NULL) < 0)
        return -1;
    if (lum_lock_builtins(ctx) < 0)
        return -1;
    /* Absolute limit for native frames, below the stack in use now. */
    lum_stack_limit = (uintptr_t)__builtin_frame_address(0) - LUM_NATIVE_STACK;

    for (i = 0; i < lum_natoms; i++) {
        lum_atoms[i] = JS_NewAtom(ctx, lum_atom_names[i]);
        if (lum_atoms[i] == JS_ATOM_NULL)
            return -1;
    }
    for (i = 0; i < lum_nstrings; i++) {
        /* As atom strings: using one as a property key (computed keys,
           `super.x`) then needs no hashing. */
        JSAtom a = JS_NewAtomLen(ctx, lum_string_data[i], lum_string_lens[i]);
        if (a == JS_ATOM_NULL)
            return -1;
        lum_strings[i] = JS_AtomToString(ctx, a);
        JS_FreeAtom(ctx, a);
        if (JS_IsException(lum_strings[i]))
            return -1;
    }
    for (i = 0; i < lum_ncells; i++)
        lum_cells[i] = JS_UNINITIALIZED;

    {
        JSClassDef box_def = { "LumBox", .gc_mark = lum_box_mark };
        JS_NewClassID(rt, &lum_box_class);
        if (JS_NewClass(rt, lum_box_class, &box_def) < 0)
            return -1;
    }
    {
        JSClassDef frame_def = { "LumFrame", .gc_mark = lum_frame_mark };
        JS_NewClassID(rt, &lum_frame_class);
        if (JS_NewClass(rt, lum_frame_class, &frame_def) < 0)
            return -1;
    }
    if (lum_structs_init(ctx) < 0)
        return -1;
    JS_NewClassID(rt, &lum_state_class);
    if (JS_NewClass(rt, lum_state_class, &def) < 0)
        return -1;
    *state = JS_NewObjectClass(ctx, lum_state_class);
    if (JS_IsException(*state))
        return -1;
    *init_fn = JS_NewCFunction(ctx, lum_cell_init, "__lum_init", 2);

    /* Accessors through which interpreted code uses mutable cells. */
    *cells_obj = JS_NewObjectProto(ctx, JS_NULL);
    for (i = 0; i < lum_nmutable; i++) {
        int cell = lum_mutable_cells[i];
        JSAtom name = JS_NewAtom(ctx, lum_cell_names[cell]);
        JSValue get = JS_NewCFunctionMagic(ctx, lum_cell_get, lum_cell_names[cell], 0,
                                           JS_CFUNC_generic_magic, cell);
        JSValue set = JS_NewCFunctionMagic(ctx, lum_mutable_const[i] ? lum_cell_set_const : lum_cell_set,
                                           lum_cell_names[cell], 1, JS_CFUNC_generic_magic, cell);
        int r = JS_DefinePropertyGetSet(ctx, *cells_obj, name, get, set, 0);
        JS_FreeAtom(ctx, name);
        if (r < 0)
            return -1;
    }
    return 0;
}

/* Make the builtins that native code relies on immutable, before any user
 * code runs: every own property of Math, the Number predicates, and the
 * global Math / Number / typed array constructor bindings become
 * non-writable and non-configurable, so patching them throws a TypeError
 * (all luminous modules are strict). Adding new properties stays possible.
 *
 * Each typed array prototype also gets the inherited `length` getter as a
 * locked own property. Then the `length` of a typed array whose shape is
 * that of a fresh instance (no own properties, original prototype) is
 * always its element count, which native code reads inline.
 */
JSValue lum_pins[LUM_PINS];
void *lum_shapes[LUM_PINS];

int lum_lock_builtins(JSContext *ctx)
{
    static const char src[] =
        "(function (g) {\n"
        "  'use strict';\n"
        "  var lock = function (o, keys) {\n"
        "    for (var i = 0; i < keys.length; i++)\n"
        "      Object.defineProperty(o, keys[i], { writable: false, configurable: false });\n"
        "  };\n"
        "  lock(g.Math, Reflect.ownKeys(g.Math));\n"
        "  lock(g.Number, ['isNaN', 'isFinite', 'isInteger', 'isSafeInteger']);\n"
        "  lock(g, ['Math', 'Number']);\n"
        "  var names = ['Uint8ClampedArray', 'Int8Array', 'Uint8Array', 'Int16Array', 'Uint16Array',\n"
        "    'Int32Array', 'Uint32Array', 'BigInt64Array', 'BigUint64Array', 'Float16Array',\n"
        "    'Float32Array', 'Float64Array'];\n"
        "  var length = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(g.Int8Array.prototype), 'length');\n"
        "  var fresh = [];\n"
        "  for (var i = 0; i < names.length; i++) {\n"
        "    var C = g[names[i]];\n"
        "    Object.defineProperty(C.prototype, 'length', { get: length.get, enumerable: false, configurable: false });\n"
        "    lock(g, [names[i]]);\n"
        "    fresh.push(new C(0));\n"
        "  }\n"
        "  var arrayIteratorProto = Object.getPrototypeOf([][Symbol.iterator]());\n"
        "  var protos = [g.Object.prototype, g.Array.prototype, g.String.prototype, g.Number.prototype,\n"
        "    g.Boolean.prototype, g.Function.prototype, g.Map.prototype, g.Set.prototype,\n"
        "    g.WeakMap.prototype, g.WeakSet.prototype, g.Promise.prototype, g.RegExp.prototype,\n"
        "    g.Date.prototype, g.Symbol.prototype, g.Error.prototype, arrayIteratorProto,\n"
        "    Object.getPrototypeOf(arrayIteratorProto), Object.getPrototypeOf(new g.Map().entries()),\n"
        "    Object.getPrototypeOf(new g.Set().values()), Object.getPrototypeOf(''[Symbol.iterator]())];\n"
        "  var statics = ['Object', 'Array', 'Number', 'String', 'Boolean', 'Symbol', 'Function', 'Promise',\n"
        "    'Map', 'Set', 'WeakMap', 'WeakSet', 'Date', 'RegExp', 'Error', 'BigInt', 'ArrayBuffer',\n"
        "    'JSON', 'Math', 'Reflect'];\n"
        "  for (var i = 0; i < statics.length; i++)\n"
        "    if (g[statics[i]]) protos.push(g[statics[i]]);\n"
        "  for (var i = 0; i < protos.length; i++) {\n"
        "    var keys = Reflect.ownKeys(protos[i]);\n"
        "    for (var j = 0; j < keys.length; j++)\n"
        "      Object.getOwnPropertyDescriptor(protos[i], keys[j]);\n"
        "  }\n"
        "  fresh.push([], g.Array.prototype, g.Array.prototype[Symbol.iterator],\n"
        "             arrayIteratorProto, arrayIteratorProto.next, Symbol.iterator,\n"
        "             Object.getPrototypeOf(async function () {}),\n"
        "             Object.getPrototypeOf(function* () {}),\n"
        "             Object.getPrototypeOf(function* () {}).prototype.next,\n"
        "             Object.getPrototypeOf(async function* () {}),\n"
        "             g.Promise, g.Promise.resolve, g.String.prototype, g.String.prototype.charCodeAt,\n"
        "             g.Array.prototype.push, g.Array.prototype.forEach, g.Array.prototype.map,\n"
        "             g.Array.prototype.filter, g.Array.prototype.some, g.Array.prototype.every,\n"
        "             g.Array.prototype.find, g.Array.prototype.findIndex, g.Array.prototype.sort,\n"
        "             g.String.prototype.slice, g.String.prototype.substring, g.String.prototype.indexOf,\n"
        "             g.String.prototype.includes, g.String.prototype.startsWith, g.String.prototype.endsWith,\n"
        "             g.Map.prototype.get, g.Map.prototype.set, g.Map.prototype.has, g.Set.prototype.add,\n"
        "             g.Set.prototype.has, g.JSON.stringify, g.JSON.parse,\n"
        "             g.Object.keys, g.Object.create, g.String.prototype.lastIndexOf);\n"
        "  fresh.push(protos);\n"
        "  return fresh;\n"
        "})(globalThis);\n";
    JSValue r = JS_Eval(ctx, src, sizeof(src) - 1, "<luminous>", JS_EVAL_TYPE_GLOBAL);
    int i;
    if (JS_IsException(r))
        return -1;
    for (i = 0; i < LUM_PINS; i++) {
        lum_pins[i] = JS_GetPropertyUint32(ctx, r, i);
        if (JS_IsException(lum_pins[i]))
            return JS_FreeValue(ctx, r), -1;
        if (i <= LUM_SHAPE_ARRAY)
            lum_shapes[i] = *(void **)((char *)JS_VALUE_GET_PTR(lum_pins[i]) + JS_LAYOUT_OBJ_SHAPE);
        else
            lum_shapes[i] = JS_VALUE_GET_PTR(lum_pins[i]);
    }
    /* Their members all created (above), the builtin prototypes, and the
       constructors and namespaces (`Object.create`, `JSON.stringify`),
       keep their shapes: their methods can be cached. */
    {
        JSValue protos = JS_GetPropertyUint32(ctx, r, LUM_PINS);
        int64_t n = 0;
        if (JS_GetLength(ctx, protos, &n) < 0)
            return JS_FreeValue(ctx, protos), JS_FreeValue(ctx, r), -1;
        for (i = 0; i < n; i++) {
            JSValue p = JS_GetPropertyUint32(ctx, protos, i);
            JS_ShareObjectShape(ctx, p);
            JS_FreeValue(ctx, p);
        }
        JS_FreeValue(ctx, protos);
    }
    lum_atom_iterator = JS_ValueToAtom(ctx, lum_pins[LUM_PIN_SYMBOL_ITERATOR]);
    lum_atom_next = JS_NewAtom(ctx, "next");
    lum_atom_string_length = JS_NewAtom(ctx, "length");
    lum_atom_char_code_at = JS_NewAtom(ctx, "charCodeAt");
    lum_atom_push = JS_NewAtom(ctx, "push");
    JS_FreeValue(ctx, r);
    return 0;
}

/* ---- inline caches ---- */
static LumStruct *lum_struct_of(JSValueConst v);
static JSValue lum_struct_get_atom(JSContext *ctx, LumStruct *s, JSAtom prop);
static int lum_field_index(int layout, JSAtom prop);
static JSValue lum_field_get(JSContext *ctx, LumStruct *s, const LumFieldDesc *f);

static int lum_ic_count(void)
{
    return &lum_nics ? lum_nics : 0;
}

/* Whether the polymorphic entries of a site (LumIC.poly) may keep entry
   `e`: an own data property, or a data property of a prototype. */
static bool lum_ic_polymorphic(const LumIC *e)
{
    return e->shape || (e->alt_shape && (e->kind == LUM_IC_PROTO_DATA || e->kind == LUM_IC_ADD ||
                                         e->kind == LUM_IC_ABSENT || e->kind == LUM_IC_DEEP_GETTER));
}

/* What a LUM_IC_ABSENT entry reads (only ever read). */
static JSValue lum_absent = JS_UNDEFINED;

/* The address of the property that cache entry `e` holds for object `p`
   of shape `shape`, or NULL (another shape, or a holder that changed): as
   native code finds it. */
/* The address of the property a prototype's entry `e` (of a polymorphic
   site) holds for an object of `shape` (its alt_shape), through the cached
   shapes of the prototypes, or NULL. */
static JSValue *lum_ic_chain_slot(const LumIC *e, void *shape)
{
    char *holder = *(char **)((char *)shape + JS_LAYOUT_SHAPE_PROTO);
    if (e->mid_shape) {
        void *first = *(void **)(holder + JS_LAYOUT_OBJ_SHAPE);
        if (first != e->mid_shape)
            return NULL;
        holder = *(char **)((char *)first + JS_LAYOUT_SHAPE_PROTO);
        if (e->mid2_shape) {
            void *second = *(void **)(holder + JS_LAYOUT_OBJ_SHAPE);
            if (second != e->mid2_shape)
                return NULL;
            holder = *(char **)((char *)second + JS_LAYOUT_SHAPE_PROTO);
        }
    }
    if (*(void **)(holder + JS_LAYOUT_OBJ_SHAPE) != e->proto_shape)
        return NULL;
    return (JSValue *)*(char **)(holder + JS_LAYOUT_OBJ_PROP) + e->slot;
}

static JSValue *lum_ic_entry_slot(const LumIC *e, char *p, void *shape)
{
    if (e->shape != shape) {
        if (e->alt_shape != shape)
            return NULL;
        if (e->kind == LUM_IC_ABSENT)
            return e->epoch == js_proto_epoch ? &lum_absent : NULL;
        if (e->kind != LUM_IC_PROTO_DATA)
            return NULL;
        return lum_ic_chain_slot(e, shape);
    }
    return (JSValue *)*(char **)(p + JS_LAYOUT_OBJ_PROP) + e->slot;
}

/* The address of the property a prototype entry (kinds 1-3) of `ic` holds
   for object `p` (of its alt_shape), found through the cached shapes, or
   NULL. */
static JSValue *lum_ic_entry_holder_slot(const LumIC *ic, char *p)
{
    void *shape = *(void **)(p + JS_LAYOUT_OBJ_SHAPE);
    char *holder;
    if (ic->alt_shape != shape)
        return NULL;
    holder = *(char **)((char *)shape + JS_LAYOUT_SHAPE_PROTO);
    if (ic->mid_shape) {
        void *first = *(void **)(holder + JS_LAYOUT_OBJ_SHAPE);
        if (first != ic->mid_shape)
            return NULL;
        holder = *(char **)((char *)first + JS_LAYOUT_SHAPE_PROTO);
    }
    if (*(void **)(holder + JS_LAYOUT_OBJ_SHAPE) != ic->proto_shape)
        return NULL;
    return (JSValue *)*(char **)(holder + JS_LAYOUT_OBJ_PROP) + ic->slot;
}

/* The polymorphic entry of `ic` holding obj's property, else NULL. */
static inline JSValue *lum_ic_poly_slot(LumIC *ic, JSValueConst obj)
{
    char *p;
    void *shape;
    int k;
    if (!ic->poly || JS_VALUE_GET_TAG(obj) != JS_TAG_OBJECT)
        return NULL;
    p = JS_VALUE_GET_PTR(obj);
    shape = *(void **)(p + JS_LAYOUT_OBJ_SHAPE);
    for (k = 0; k < LUM_POLY; k++) {
        JSValue *at = lum_ic_entry_slot(&ic->poly[k], p, shape);
        if (at)
            return at;
    }
    return NULL;
}

/* The address of the accessor property a LUM_IC_DEEP_GETTER entry of `ic`
   holds for `obj`, or NULL. */
static JSValue *lum_ic_poly_getter(LumIC *ic, JSValueConst obj)
{
    void *shape;
    int k;
    if (!ic->poly || JS_VALUE_GET_TAG(obj) != JS_TAG_OBJECT)
        return NULL;
    shape = *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE);
    for (k = 0; k < LUM_POLY; k++) {
        const LumIC *e = &ic->poly[k];
        if (e->kind == LUM_IC_DEEP_GETTER && e->alt_shape == shape) {
            JSValue *at = lum_ic_chain_slot(e, shape);
            if (at)
                return at;
        }
    }
    return NULL;
}

/* For native code, where the primary entry of `ic` misses on object `p`:
   the address of the property one of its polymorphic entries holds, else
   NULL. */
JSValue *lum_ic_poly_addr(LumIC *ic, void *p)
{
    return lum_ic_poly_slot(ic, JS_MKPTR(JS_TAG_OBJECT, p));
}

/* obj.prop = val (val taken) through a polymorphic entry of write site
   `ic` holding obj's own (writable) data property; whether there was one. */
static bool lum_ic_poly_set(JSContext *ctx, LumIC *ic, JSValueConst obj, JSValue val)
{
    char *p;
    void *shape;
    int k;
    if (!ic->poly || JS_VALUE_GET_TAG(obj) != JS_TAG_OBJECT)
        return false;
    p = JS_VALUE_GET_PTR(obj);
    shape = *(void **)(p + JS_LAYOUT_OBJ_SHAPE);
    for (k = 0; k < LUM_POLY; k++) {
        if (ic->poly[k].shape == shape) {
            JSValue *at = (JSValue *)*(char **)(p + JS_LAYOUT_OBJ_PROP) + ic->poly[k].slot;
            JSValue old = *at;
            *at = val;
            JS_FreeValue(ctx, old);
            if (JS_VALUE_HAS_REF_COUNT(val))
                JS_GCWriteBarrier(JS_GetRuntime(ctx), p);
            return true;
        }
    }
    return false;
}

/* obj.prop = val (val taken) through an addition that write site `ic`
   cached (a LUM_IC_ADD entry among its polymorphic ones): 1 if made, 0 if
   none applies (val not taken), -1 (an exception). */
static int lum_ic_add(JSContext *ctx, LumIC *ic, JSValueConst obj, JSValue val)
{
    void *shape;
    int k;
    if (JS_VALUE_GET_TAG(obj) != JS_TAG_OBJECT)
        return 0;
    shape = *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE);
    /* (The primary entry: where native code found the properties do not
       fit, or the epoch moved.) */
    if (ic->alt_shape == shape && ic->kind == LUM_IC_ADD)
        return ic->epoch == js_proto_epoch ? JS_AddCachedProperty(ctx, obj, ic->proto_shape, val) : 0;
    if (!ic->poly)
        return 0;
    for (k = 0; k < LUM_POLY; k++) {
        LumIC *e = &ic->poly[k];
        if (e->alt_shape == shape && e->kind == LUM_IC_ADD)
            return e->epoch == js_proto_epoch ? JS_AddCachedProperty(ctx, obj, e->proto_shape, val) : 0;
    }
    return 0;
}

/* Cache the addition of `prop` that obj.prop = v made to `obj`, whose shape
   was `before` (held): in a polymorphic entry of `ic` (native code takes
   the primary one's `alt_shape` for an accessor's), the one of `before` or
   a free one. Returns whether it could. */
/* The polymorphic entry of `ic` for an epoch-checked entry of kind `kind`
   (LUM_IC_ADD, LUM_IC_ABSENT) and shape `shape`: the one there is (made
   again after a change to a watched prototype: *prefreshes counts it), else
   a free one; NULL if there is none, or it was made again too many times. */
static LumIC *lum_ic_epoch_entry(JSRuntime *rt, LumIC *ic, void *shape, uint32_t kind, uint32_t *prefreshes)
{
    LumIC *e = NULL;
    int k;
    *prefreshes = 0;
    if (!ic->poly)
        ic->poly = js_mallocz_rt(rt, LUM_POLY * sizeof(LumIC));
    for (k = 0; ic->poly && k < LUM_POLY; k++) {
        LumIC *f = &ic->poly[k];
        if (f->alt_shape == shape && f->kind == kind) {
            *prefreshes = f->refreshes + 1;
            return *prefreshes > LUM_IC_REFRESHES ? NULL : f;
        }
        if (!e && !lum_ic_polymorphic(f))
            e = f;
    }
    return e;
}

/* Fill entry `e` (of lum_ic_epoch_entry) with the shapes it takes. */
static void lum_ic_epoch_store(JSRuntime *rt, LumIC *ic, LumIC *e, void *shape, void *proto_shape, int slot,
                               uint32_t kind, uint32_t refreshes)
{
    if (e->alt_shape)
        JS_FreeCachedShape(rt, e->alt_shape);
    if (e->proto_shape)
        JS_FreeCachedShape(rt, e->proto_shape);
    e->alt_shape = shape;
    e->proto_shape = proto_shape;
    e->slot = slot;
    e->kind = kind;
    e->epoch = js_proto_epoch;
    e->refreshes = refreshes;
    lum_ic_note(rt, ic);
}

static bool lum_ic_fill_add(JSContext *ctx, JSValue obj, JSAtom prop, void *before, LumIC *ic)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    uint32_t refreshes;
    LumIC *e;
    void *after;
    int slot;
    /* The first one in the primary entry, which native code checks inline
       (or the one there, made again). */
    if (!ic->shape && (!ic->alt_shape || (ic->alt_shape == before && ic->kind == LUM_IC_ADD))) {
        refreshes = ic->alt_shape ? ic->refreshes + 1 : 0;
        e = refreshes > LUM_IC_REFRESHES ? NULL : ic;
    } else {
        e = lum_ic_epoch_entry(rt, ic, before, LUM_IC_ADD, &refreshes);
    }
    if (!e || (slot = JS_CacheAddProperty(ctx, obj, prop, before, &after)) < 0)
        return false;
    lum_ic_epoch_store(rt, ic, e, JS_DupSharedShape(before), after, slot, LUM_IC_ADD, refreshes);
    return true;
}

/* Cache that objects of obj's shape do not have `prop`, nor their
   prototypes (in a polymorphic entry of `ic`, as lum_ic_fill_add's; at a
   site whose native code reads the primary one's kinds, there if it is
   free). Returns whether it could. */
static bool lum_ic_fill_absent(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic, bool primary)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    uint32_t refreshes;
    LumIC *e;
    void *shape, *before;
    if (JS_VALUE_GET_TAG(obj) != JS_TAG_OBJECT)
        return false;
    before = *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE);
    /* (Native code reads `undefined` from the primary entry: its value,
       while the epoch holds.) */
    if (primary && !ic->shape && (!ic->alt_shape || (ic->alt_shape == before && ic->kind == LUM_IC_ABSENT))) {
        refreshes = ic->alt_shape ? ic->refreshes + 1 : 0;
        e = refreshes > LUM_IC_REFRESHES ? NULL : ic;
    } else {
        e = lum_ic_epoch_entry(rt, ic, before, LUM_IC_ABSENT, &refreshes);
    }
    if (!e || JS_CacheAbsentProperty(ctx, obj, prop, &shape) < 0)
        return false;
    lum_ic_epoch_store(rt, ic, e, shape, NULL, 0, LUM_IC_ABSENT, refreshes);
    if (e == ic) {
        ic->value = JS_UNDEFINED;
        ic->entry = NULL;
    }
    return true;
}

/* Whether a polymorphic site has no room for another shape: it caches
   none then (its misses do not evict each other). */
static bool lum_ic_full(const LumIC *ic)
{
    int k;
    if (!ic->poly || !lum_ic_polymorphic(ic))
        return false;
    for (k = 0; k < LUM_POLY; k++) {
        if (!lum_ic_polymorphic(&ic->poly[k]))
            return false;
    }
    return true;
}

/* Point the cache at a new entry (taking the shape references). An own or
   prototype data property it held for another shape moves to its
   polymorphic entries, if there is room. */
static void lum_ic_store(JSContext *ctx, LumIC *ic, void *shape, void *proto_shape, void *mid_shape,
                           int slot, int kind)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    if (shape && lum_ic_polymorphic(ic) && (ic->shape ? ic->shape : ic->alt_shape) != shape) {
        int k;
        if (!ic->poly)
            ic->poly = js_mallocz_rt(rt, LUM_POLY * sizeof(LumIC));
        for (k = 0; ic->poly && k < LUM_POLY; k++) {
            if (!lum_ic_polymorphic(&ic->poly[k])) {
                ic->poly[k] = *ic;
                ic->poly[k].poly = NULL;
                ic->shape = ic->alt_shape = ic->proto_shape = ic->mid_shape = NULL;
                break;
            }
        }
    }
    if (ic->shape)
        JS_FreeCachedShape(rt, ic->shape);
    if (ic->proto_shape)
        JS_FreeCachedShape(rt, ic->proto_shape);
    if (ic->mid_shape)
        JS_FreeCachedShape(rt, ic->mid_shape);
    if (ic->alt_shape)
        JS_FreeCachedShape(rt, ic->alt_shape);
    /* (See LumIC.) */
    ic->shape = kind == LUM_IC_OWN ? shape : NULL;
    ic->alt_shape = kind == LUM_IC_OWN ? NULL : shape;
    ic->proto_shape = proto_shape;
    ic->mid_shape = mid_shape;
    ic->slot = slot;
    ic->kind = kind;
    ic->epoch = 0;
    ic->value = JS_UNDEFINED;
    ic->entry = NULL;
    lum_ic_note(rt, ic);
}

/* Cache an own data property (mode 0: read, 1: write) or accessor (2: its
   getter, 3: its setter); returns whether it could. */
static bool lum_ic_fill(JSContext *ctx, JSValue obj, JSAtom prop, int mode, LumIC *ic)
{
    void *shape;
    int slot = JS_CacheOwnProperty(ctx, obj, prop, mode, &shape);
    if (slot < 0)
        return false;
    lum_ic_store(ctx, ic, shape, NULL, NULL, slot, mode < 2 ? LUM_IC_OWN : LUM_IC_OWN_GETTER + mode - 2);
    return true;
}

/* Cache a data property of the third prototype (a method of a class two
   superclasses up) in a polymorphic entry of `ic` (native code checks
   only two prototypes); returns whether it could. */
static bool lum_ic_fill_deep(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    void *shapes[4];
    int k, slot = JS_CacheDeepProtoProperty(ctx, obj, prop, shapes);
    if (slot < 0)
        return false;
    if (!ic->poly)
        ic->poly = js_mallocz_rt(rt, LUM_POLY * sizeof(LumIC));
    for (k = 0; ic->poly && k < LUM_POLY; k++) {
        LumIC *e = &ic->poly[k];
        if (!lum_ic_polymorphic(e)) {
            e->shape = NULL;
            e->alt_shape = shapes[0];
            e->mid_shape = shapes[1];
            e->mid2_shape = shapes[2];
            e->proto_shape = shapes[3];
            e->slot = slot;
            e->kind = LUM_IC_PROTO_DATA;
            lum_ic_note(rt, ic);
            return true;
        }
    }
    for (k = 0; k < 4; k++)
        JS_FreeCachedShape(rt, shapes[k]);
    return false;
}

/* Cache a getter of the first to third prototype (one the site's entry
   could not take: the third's, as %TypedArray%.prototype's `byteLength`
   for a Buffer, or of another shape) in a polymorphic entry of `ic`
   (LUM_IC_DEEP_GETTER); returns whether it could. */
static bool lum_ic_fill_deep_getter(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    void *shapes[4];
    int depth, k, slot;
    /* (Watched, the prototypes have shared shapes, which it can cache.) */
    JS_WatchProtoChain(ctx, obj, prop, 3);
    slot = JS_CacheProtoGetter(ctx, obj, prop, shapes, &depth);
    if (slot < 0)
        return false;
    if (!ic->poly)
        ic->poly = js_mallocz_rt(rt, LUM_POLY * sizeof(LumIC));
    for (k = 0; ic->poly && k < LUM_POLY; k++) {
        LumIC *e = &ic->poly[k];
        if (!lum_ic_polymorphic(e)) {
            e->shape = NULL;
            e->alt_shape = shapes[0];
            e->mid_shape = depth >= 2 ? shapes[1] : NULL;
            e->mid2_shape = depth == 3 ? shapes[2] : NULL;
            e->proto_shape = shapes[depth];
            e->slot = slot;
            e->kind = LUM_IC_DEEP_GETTER;
            lum_ic_note(rt, ic);
            return true;
        }
    }
    for (k = 0; k <= depth; k++)
        JS_FreeCachedShape(rt, shapes[k]);
    return false;
}

static void lum_ic_keep_value(LumIC *ic, char *p);

/* Cache a property of a prototype (kind: LUM_IC_PROTO_x); returns
   whether it could. The entry also keeps what the holder has, valid while
   js_proto_epoch is unchanged (see LumIC). */
static bool lum_ic_fill_proto(JSContext *ctx, JSValue obj, JSAtom prop, int kind, LumIC *ic)
{
    void *shape, *proto_shape, *mid_shape;
    int slot;
    JS_WatchProtoChain(ctx, obj, prop, 2);
    slot = JS_CacheProtoProperty(ctx, obj, prop, kind - 1, &shape, &proto_shape, &mid_shape);
    if (slot < 0)
        return false;
    lum_ic_store(ctx, ic, shape, proto_shape, mid_shape, slot, kind);
    lum_ic_keep_value(ic, JS_VALUE_GET_PTR(obj));
    return true;
}

/* A prototype entry (kinds 1-3) of `ic`: keep what its holder has for
   object `p` (of its alt_shape), with the current js_proto_epoch, if the
   shapes on the way are the cached ones; else no epoch (see LumIC). */
/* The getter (which 0) or setter (1) of the accessor property at `at`
   (`JSProperty.u.getset`: two compressed references), or NULL. */
static void *lum_accessor(void *at, int which)
{
#if defined(JS_COMPRESSED_POINTERS)
    return (void *)((uintptr_t)((uint32_t *)at)[which] << 3);
#else
    return ((void **)at)[which];
#endif
}

static void lum_ic_keep_value(LumIC *ic, char *p)
{
    JSValue *at = lum_ic_entry_holder_slot(ic, p);
    JSValue v;
    ic->epoch = 0;
    ic->entry = NULL;
    if (!at || ic->kind < LUM_IC_PROTO_DATA || ic->kind > LUM_IC_PROTO_SETTER)
        return;
    if (ic->kind == LUM_IC_PROTO_DATA)
        v = *at;
    else
        v = JS_MKPTR(JS_TAG_OBJECT, lum_accessor(at, ic->kind == LUM_IC_PROTO_GETTER ? 0 : 1));
    if (JS_VALUE_GET_TAG(v) == JS_TAG_OBJECT) {
        char *f = JS_VALUE_GET_PTR(v);
        /* (An accessor half can be missing.) */
        if (!f)
            return;
        if (*(uint16_t *)(f + JS_LAYOUT_OBJ_CLASS_ID) == JS_LAYOUT_CLASS_C_FUNCTION_DATA)
            ic->entry = *(void **)(*(char **)(f + JS_LAYOUT_OBJ_FUNC_DATA) + JS_LAYOUT_FDATA_FAST);
    }
    ic->value = v;
    ic->epoch = js_proto_epoch;
}

/* For native code: where entry `ic` (of a prototype's property) has an
   older epoch than js_proto_epoch, and object `p` its alt_shape: keep what
   the holder has now, if the shapes on the way still hold (the prototypes
   up to it are still watched); else, or once it has been made again too
   many times, no epoch: native code then checks the shapes. */
void lum_ic_refresh(LumIC *ic, void *p)
{
    /* (An absent property's entry is made again where the property is
       looked up: lum_ic_get_proto.) */
    if (ic->kind == LUM_IC_ABSENT) {
        ic->epoch = 0;
        return;
    }
    if (++ic->refreshes > LUM_IC_REFRESHES) {
        ic->epoch = 0;
        return;
    }
    lum_ic_keep_value(ic, p);
}

/* Learn another shape of record r from obj if it has the record's fields
 * first, in order, as writable data properties (of a shared shape): the
 * objects of a class and its subclasses, of literals with more fields. */
void lum_record_learn(JSContext *ctx, JSValue obj, int r)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    const int *f = lum_record_fields + lum_record_index[r];
    void **known = &lum_record_shapes[r * LUM_RECORD_STRIDE];
    uintptr_t *tries = (uintptr_t *)&known[LUM_RECORD_SHAPES];
    void *shape = NULL, *s;
    int i, k;
    if (known[LUM_RECORD_SHAPES - 1] || *tries >= LUM_RECORD_TRIES)
        return;
    if (JS_VALUE_GET_TAG(obj) != JS_TAG_OBJECT) {
        ++*tries;
        return;
    }
    for (k = 0; known[k]; k++) {
        if (known[k] == *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE))
            return;
    }
    for (i = 0; i < f[0]; i++) {
        int slot = JS_CacheOwnProperty(ctx, obj, lum_atoms[f[1 + i]], 1, &s);
        if (slot < 0)
            break;
        if (shape)
            JS_FreeCachedShape(rt, s);
        else
            shape = s;
        if (slot != i)
            break;
    }
    if (i == f[0]) {
        known[k] = shape;
    } else {
        if (shape)
            JS_FreeCachedShape(rt, shape);
        ++*tries;
    }
}

/* Whether String.prototype.charCodeAt is still the builtin, as an own
 * data property: speculative regions read characters directly then. Each
 * region's guard asks, so the prototype's shape (shared: any change to it
 * makes another) and the property's slot in it are kept: while the shape
 * is the same, only the slot's value is compared. */
int lum_char_code_at_ok(JSContext *ctx)
{
    JSValueConst proto = lum_pins[LUM_PIN_STRING_PROTO];
    JSValueConst fn = lum_pins[LUM_PIN_CHAR_CODE_AT];
    char *p = JS_VALUE_GET_PTR(proto);

    if (lum_cca_shape && *(void **)(p + JS_LAYOUT_OBJ_SHAPE) == lum_cca_shape) {
        JSValue v = ((JSValue *)*(char **)(p + JS_LAYOUT_OBJ_PROP))[lum_cca_slot];
        return JS_VALUE_GET_TAG(v) == JS_TAG_OBJECT && JS_VALUE_GET_PTR(v) == JS_VALUE_GET_PTR(fn);
    }
    if (!JS_OwnDataIs(ctx, proto, lum_atom_char_code_at, fn))
        return 0;
    if (lum_cca_shape) {
        JS_FreeCachedShape(JS_GetRuntime(ctx), lum_cca_shape);
        lum_cca_shape = NULL;
    }
    if (JS_ShareObjectShape(ctx, proto) == 0) {
        void *s;
        int k = JS_CacheOwnProperty(ctx, proto, lum_atom_char_code_at, 0, &s);
        if (k >= 0) {
            lum_cca_shape = s;
            lum_cca_slot = k;
        }
    }
    return 1;
}

/* Whether Array.prototype.push is still the builtin, as an own data
 * property, and the array prototypes have no index properties: speculative
 * regions append to arrays directly then (`lum_region_push`). */
int lum_array_push_ok(JSContext *ctx)
{
    return *((uint8_t *)ctx + JS_LAYOUT_CTX_STD_ARRAY_PROTO) &&
           JS_OwnDataIs(ctx, lum_pins[LUM_PIN_ARRAY_PROTO], lum_atom_push, lum_pins[LUM_PIN_ARRAY_PUSH]);
}

/* arr.push(v) (v taken) in a speculative region, for an array without
 * room for it, which the region's guard found `push` appends to: through
 * the engine's fast path (which makes room), else the builtin. Returns -1
 * on an exception (out of memory). */
int lum_region_push(JSContext *ctx, JSValue arr, JSValue v)
{
    JSValue r = JS_ArrayPushFast(ctx, arr, v);
    if (JS_VALUE_GET_TAG(r) == JS_TAG_UNINITIALIZED)
        r = JS_Call(ctx, lum_pins[LUM_PIN_ARRAY_PUSH], arr, 1, (JSValueConst *)&v);
    JS_FreeValue(ctx, v);
    if (JS_IsException(r))
        return -1;
    JS_FreeValue(ctx, r);
    return 0;
}

/* A cache miss reading own data property obj.prop (obj borrowed), where
 * no getter may run: its value (a new reference), cached, or undefined if
 * it is not one. */
JSValue lum_ic_own_data(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic)
{
    JSValue *at = lum_ic_poly_slot(ic, obj);
    if (at)
        return JS_DupValue(ctx, *at);
    if (lum_ic_full(ic)) {
        uint32_t hint = 0;
        JSValue v = JS_GetOwnDataHint(ctx, obj, prop, &hint);
        return JS_IsUninitialized(v) ? JS_UNDEFINED : v;
    }
    if (!lum_ic_fill(ctx, obj, prop, 0, ic))
        return JS_UNDEFINED;
    return JS_GetProperty(ctx, obj, prop);
}

static __attribute__((noinline)) int lum_instanceof_miss(JSContext *ctx, JSValue v, JSValue ctor, LumIC *ic)
{
    void *shape;
    int slot, r = JS_IsInstanceOf(ctx, v, ctor);
    if (r < 0 || ic->refreshes >= LUM_IC_REFRESHES)
        return r;
    slot = JS_CacheInstanceOf(ctx, ctor, &shape);
    if (slot >= 0) {
        char *c = JS_VALUE_GET_PTR(ctor);
        ic->refreshes++;
        lum_ic_store(ctx, ic, shape, NULL, NULL, slot,
                     LUM_IC_INSTANCEOF | (uint32_t)*(uint16_t *)(c + JS_LAYOUT_OBJ_CLASS_ID) << 16);
        ic->epoch = js_proto_epoch;
    }
    return r;
}

/* `v instanceof ctor` (both borrowed): 1, 0, or -1 (an exception). With
 * the constructor's shape and class cached (kind LUM_IC_INSTANCEOF), while
 * js_proto_epoch is unchanged, its "prototype" is read from the cached slot
 * and looked for on v's prototype chain (see JS_CacheInstanceOf). */
int lum_instanceof(JSContext *ctx, JSValue v, JSValue ctor, LumIC *ic)
{
    if (JS_VALUE_GET_TAG(ctor) == JS_TAG_OBJECT) {
        char *c = JS_VALUE_GET_PTR(ctor);
        if (*(void **)(c + JS_LAYOUT_OBJ_SHAPE) == ic->alt_shape && ic->epoch == js_proto_epoch &&
            ic->kind == (LUM_IC_INSTANCEOF | (uint32_t)*(uint16_t *)(c + JS_LAYOUT_OBJ_CLASS_ID) << 16)) {
            JSValue proto = ((JSValue *)*(char **)(c + JS_LAYOUT_OBJ_PROP))[ic->slot];
            if (JS_VALUE_GET_TAG(proto) == JS_TAG_OBJECT) {
                char *target = JS_VALUE_GET_PTR(proto), *p;
                if (JS_VALUE_GET_TAG(v) != JS_TAG_OBJECT)
                    return 0;
                for (p = JS_VALUE_GET_PTR(v);;) {
                    char *q = *(char **)(*(char **)(p + JS_LAYOUT_OBJ_SHAPE) + JS_LAYOUT_SHAPE_PROTO);
                    if (q == target)
                        return 1;
                    if (!q) {
                        /* (A proxy's prototype is its handler's to tell.) */
                        if (*(uint16_t *)(p + JS_LAYOUT_OBJ_CLASS_ID) != JS_LAYOUT_CLASS_PROXY)
                            return 0;
                        break;
                    }
                    p = q;
                }
            }
        }
    }
    return lum_instanceof_miss(ctx, v, ctor, ic);
}

/* A cache miss reading obj.prop (obj borrowed). */
JSValue lum_ic_get(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic)
{
    JSValue *at = lum_ic_poly_slot(ic, obj);
    JSValue v;
    if (at)
        return JS_DupValue(ctx, *at);
    v = JS_GetProperty(ctx, obj, prop);
    if (!JS_IsException(v) && !lum_ic_full(ic) && !lum_ic_fill(ctx, obj, prop, 0, ic) && JS_IsUndefined(v))
        lum_ic_fill_absent(ctx, obj, prop, ic, false);
    return v;
}

/* A cache miss writing obj.prop = val (obj borrowed, val consumed). Only
 * writes that find the property are cached: a site that adds it sees the
 * object's shape from before. */
int lum_ic_set(JSContext *ctx, JSValue obj, JSAtom prop, JSValue val, LumIC *ic)
{
    void *before = NULL;
    int r;
    if (lum_ic_poly_set(ctx, ic, obj, val))
        return 0;
    if ((r = lum_ic_add(ctx, ic, obj, val)))
        return r < 0 ? -1 : 0;
    /* (A shared shape, held: an addition may free it.) */
    if (JS_VALUE_GET_TAG(obj) == JS_TAG_OBJECT && !lum_ic_full(ic))
        before = JS_DupSharedShape(*(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE));
    r = JS_SetProperty(ctx, obj, prop, val);
    if (before) {
        if (r >= 0) {
            if (before == *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE))
                lum_ic_fill(ctx, obj, prop, 1, ic);
            else
                lum_ic_fill_add(ctx, obj, prop, before, ic);
        }
        JS_FreeCachedShape(JS_GetRuntime(ctx), before);
    }
    return r;
}

/* ---- spread, rest and iteration ---- */

/* arr[arr.length] = v (array literal element). Takes ownership of v. */
int lum_array_push(JSContext *ctx, JSValue arr, JSValue v)
{
    int64_t len;
    if (JS_GetLength(ctx, arr, &len) < 0) {
        JS_FreeValue(ctx, v);
        return -1;
    }
    return JS_DefinePropertyValueUint32(ctx, arr, (uint32_t)len, v, JS_PROP_C_W_E | JS_PROP_THROW);
}

/* An array literal hole: only the length grows. */
int lum_array_hole(JSContext *ctx, JSValue arr)
{
    int64_t len;
    if (JS_GetLength(ctx, arr, &len) < 0)
        return -1;
    return JS_SetPropertyStr(ctx, arr, "length", JS_NewInt64(ctx, len + 1));
}

/* ...iterable in an array literal (or argument list). Borrows both. */
int lum_array_spread(JSContext *ctx, JSValue arr, JSValue iterable)
{
    JSValue iter, next, v;
    int64_t len;
    int done;
    if (JS_GetLength(ctx, arr, &len) < 0)
        return -1;
    iter = JS_GetIteratorObject(ctx, iterable, false);
    if (JS_IsException(iter))
        return -1;
    next = JS_GetPropertyStr(ctx, iter, "next");
    if (JS_IsException(next)) {
        JS_FreeValue(ctx, iter);
        return -1;
    }
    for (;;) {
        v = JS_IteratorStep(ctx, iter, next, 0, NULL, &done);
        if (JS_IsException(v))
            break;
        if (done) {
            JS_FreeValue(ctx, iter);
            JS_FreeValue(ctx, next);
            return 0;
        }
        if (JS_DefinePropertyValueUint32(ctx, arr, (uint32_t)len++, v, JS_PROP_C_W_E | JS_PROP_THROW) < 0) {
            JS_IteratorCloseCompletion(ctx, iter, true);
            break;
        }
    }
    JS_FreeValue(ctx, iter);
    JS_FreeValue(ctx, next);
    return -1;
}

/* The elements of an array built for a call. */
static JSValue *lum_array_values(JSContext *ctx, JSValue arr, int *pcount)
{
    int64_t len, i;
    JSValue *tab;
    if (JS_GetLength(ctx, arr, &len) < 0)
        return NULL;
    if (len > 65535) {
        JS_ThrowRangeError(ctx, "too many arguments in function call (only 65535 allowed)");
        return NULL;
    }
    tab = JS_MallocTraced(ctx, sizeof(JSValue) * (len > 0 ? len : 1));
    if (!tab)
        return NULL;
    for (i = 0; i < len; i++) {
        tab[i] = JS_GetPropertyInt64(ctx, arr, i);
        if (JS_IsException(tab[i])) {
            while (i-- > 0)
                JS_FreeValue(ctx, tab[i]);
            js_free(ctx, tab);
            return NULL;
        }
    }
    *pcount = (int)len;
    return tab;
}

/* f.apply(this, args) with v = { f, this, args }. Borrows everything. */
JSValue lum_apply(JSContext *ctx, JSValue *v)
{
    int argc, i;
    JSValue *argv = lum_array_values(ctx, v[2], &argc), ret;
    if (!argv)
        return JS_EXCEPTION;
    ret = JS_Call(ctx, v[0], v[1], argc, (JSValueConst *)argv);
    for (i = 0; i < argc; i++)
        JS_FreeValue(ctx, argv[i]);
    js_free(ctx, argv);
    return ret;
}

/* new f(...args). Borrows both. */
JSValue lum_construct(JSContext *ctx, JSValue f, JSValue args)
{
    int argc, i;
    JSValue *argv = lum_array_values(ctx, args, &argc), ret;
    if (!argv)
        return JS_EXCEPTION;
    ret = JS_CallConstructor(ctx, f, argc, (JSValueConst *)argv);
    for (i = 0; i < argc; i++)
        JS_FreeValue(ctx, argv[i]);
    js_free(ctx, argv);
    return ret;
}

static JSValue lum_super_new(JSContext *ctx, JSValue cls, JSValue new_target, int argc, JSValue *argv);

/* `lum_super_construct` with the arguments in an array: v = { class,
   new.target, arguments }. */
JSValue lum_super_construct_array(JSContext *ctx, JSValue *v)
{
    int argc, i;
    JSValue *argv = lum_array_values(ctx, v[2], &argc), ret;
    if (!argv)
        return JS_EXCEPTION;
    ret = lum_super_new(ctx, v[0], v[1], argc, argv);
    for (i = 0; i < argc; i++)
        JS_FreeValue(ctx, argv[i]);
    js_free(ctx, argv);
    return ret;
}

/* Rest parameter: the arguments from `start` on, in a new array. */
JSValue lum_rest(JSContext *ctx, int argc, JSValue *argv, int start)
{
    int i, n = argc > start ? argc - start : 0;
    JSValue *tab = js_malloc(ctx, sizeof(JSValue) * (n > 0 ? n : 1)), arr;
    if (!tab)
        return JS_EXCEPTION;
    for (i = 0; i < n; i++)
        tab[i] = JS_DupValue(ctx, argv[start + i]);
    arr = JS_NewArrayFrom(ctx, n, tab);
    js_free(ctx, tab);
    return arr;
}

/* A function's `arguments` (as `lum_rest`'s calls are made). */
JSValue lum_arguments(JSContext *ctx, int argc, JSValue *argv, int unused)
{
    (void)unused;
    return JS_NewArguments(ctx, argc, (JSValueConst *)argv);
}

/* `super(...args)` in the native constructor of class `cls` whose
   superclass has none: the superclass (`cls`'s prototype, as it is now)
   constructed with `new_target`. v = { cls, new_target, args... }. */
JSValue lum_super_construct(JSContext *ctx, int argc, JSValue *v)
{
    return lum_super_new(ctx, v[0], v[1], argc, v + 2);
}

static JSValue lum_super_new(JSContext *ctx, JSValue cls, JSValue new_target, int argc, JSValue *argv)
{
    JSValue parent = JS_GetPrototype(ctx, cls), r;
    if (JS_IsException(parent))
        return parent;
    /* (As the engine's `super(...)`: no check of its own.) */
    r = JS_CallConstructor2(ctx, parent, new_target, argc, (JSValueConst *)argv);
    JS_FreeValue(ctx, parent);
    return r;
}

/* Object rest: v = { source, keys... }, nkeys keys (borrowed). */
JSValue lum_object_rest(JSContext *ctx, JSValue *v, int nkeys)
{
    JSValue excluded = JS_NewObject(ctx), target;
    int i;
    if (JS_IsException(excluded))
        return excluded;
    for (i = 0; i < nkeys; i++) {
        JSAtom atom = JS_ValueToAtom(ctx, v[1 + i]);
        int r = atom == JS_ATOM_NULL ? -1
                : JS_DefinePropertyValue(ctx, excluded, atom, JS_NULL, JS_PROP_C_W_E | JS_PROP_THROW);
        JS_FreeAtom(ctx, atom);
        if (r < 0) {
            JS_FreeValue(ctx, excluded);
            return JS_EXCEPTION;
        }
    }
    target = JS_NewObject(ctx);
    if (!JS_IsException(target) && JS_CopyDataPropertiesExcluding(ctx, target, v[0], excluded) < 0) {
        JS_FreeValue(ctx, target);
        target = JS_EXCEPTION;
    }
    JS_FreeValue(ctx, excluded);
    return target;
}

/* obj[key] = val in an object literal: v = { obj, key } (borrowed), val
 * taken. The key is already a property key. */
int lum_define_computed(JSContext *ctx, JSValue *v, JSValue val)
{
    JSAtom atom = JS_ValueToAtom(ctx, v[1]);
    int ret;
    if (atom == JS_ATOM_NULL) {
        JS_FreeValue(ctx, val);
        return -1;
    }
    ret = JS_DefinePropertyValue(ctx, v[0], atom, val, JS_PROP_C_W_E | JS_PROP_THROW);
    JS_FreeAtom(ctx, atom);
    return ret;
}

/* `[key]() {}` (kind 0), `get [key]() {}` (1) or `set [key](v) {}` (2) in an
   object literal: v[0] the object, v[1] the key (a property key), `fn` the
   function (taken), named as the key ("[description]" for a symbol), with
   "get " or "set " before for an accessor. */
int lum_define_computed_method(JSContext *ctx, JSValue *v, JSValue fn, int kind)
{
    JSAtom atom = JS_ValueToAtom(ctx, v[1]);
    JSValue parts[3], name;
    int n = 0, ret;
    if (atom == JS_ATOM_NULL) {
        JS_FreeValue(ctx, fn);
        return -1;
    }
    if (kind)
        parts[n++] = JS_NewString(ctx, kind == 1 ? "get " : "set ");
    if (JS_IsSymbol(v[1])) {
        JSValue desc = JS_GetPropertyStr(ctx, v[1], "description");
        if (JS_IsException(desc))
            goto fail_parts;
        if (!JS_IsUndefined(desc)) {
            JSValue b[3] = {JS_NewString(ctx, "["), desc, JS_NewString(ctx, "]")};
            parts[n++] = JS_ConcatStrings(ctx, b, 3);
        } else {
            parts[n++] = JS_NewString(ctx, "");
        }
    } else {
        parts[n++] = JS_AtomToString(ctx, atom);
    }
    name = n == 1 ? parts[0] : JS_ConcatStrings(ctx, parts, n);
    if (JS_IsException(name))
        goto fail;
    if (JS_DefinePropertyValueStr(ctx, fn, "name", name, JS_PROP_CONFIGURABLE) < 0)
        goto fail;
    if (kind == 0) {
        ret = JS_DefinePropertyValue(ctx, v[0], atom, fn, JS_PROP_C_W_E | JS_PROP_THROW);
    } else {
        int flags = JS_PROP_HAS_ENUMERABLE | JS_PROP_ENUMERABLE | JS_PROP_HAS_CONFIGURABLE |
                    JS_PROP_CONFIGURABLE | JS_PROP_THROW | (kind == 2 ? JS_PROP_HAS_SET : JS_PROP_HAS_GET);
        ret = JS_DefineProperty(ctx, v[0], atom, JS_UNDEFINED, kind == 2 ? JS_UNDEFINED : fn,
                                kind == 2 ? fn : JS_UNDEFINED, flags);
        JS_FreeValue(ctx, fn);
    }
    JS_FreeAtom(ctx, atom);
    return ret;
fail_parts:
    while (n > 0)
        JS_FreeValue(ctx, parts[--n]);
fail:
    JS_FreeAtom(ctx, atom);
    JS_FreeValue(ctx, fn);
    return -1;
}

/* `...source` in an object literal. Borrows both. */
int lum_object_spread(JSContext *ctx, JSValue obj, JSValue source)
{
    return JS_CopyDataPropertiesExcluding(ctx, obj, source, JS_UNDEFINED);
}

/* Close the iteration of `arr` that native code did directly, at `idx`:
 * as closing an array iterator at that position, which runs a `return`
 * method only if one was added to the iterator's prototype chain. */
int lum_array_iter_close(JSContext *ctx, JSValue arr, int32_t idx, int is_throw)
{
    JSValue it = JS_NewArrayIteratorAt(ctx, arr, (uint32_t)idx);
    int r;
    if (JS_IsException(it))
        return -1;
    r = JS_IteratorCloseCompletion(ctx, it, is_throw);
    JS_FreeValue(ctx, it);
    return r;
}

/* Whether own property `prop` of `obj` is still the data property `value`
 * (checked without calling any getter). */

/* Whether iterating an array with the shape of a fresh one is still the
 * original array iteration: Array.prototype[Symbol.iterator] and the
 * array iterator's `next` are unchanged (both are read once, when an
 * iteration starts). */
int lum_array_iter_ok(JSContext *ctx)
{
    return JS_OwnDataIs(ctx, lum_pins[LUM_PIN_ARRAY_PROTO], lum_atom_iterator,
                        lum_pins[LUM_PIN_ARRAY_VALUES]) &&
           JS_OwnDataIs(ctx, lum_pins[LUM_PIN_ARRAY_ITERATOR_PROTO], lum_atom_next,
                        lum_pins[LUM_PIN_ARRAY_ITERATOR_NEXT]);
}

/* ---- generators ---- */

/* A new frame for generator function `fn` (returned) and its slots. */
JSValue lum_gen_start(JSContext *ctx, int fn, int nslots, JSValue **slots)
{
    LumFrame *f;
    JSValue obj = lum_frame_new(ctx, fn, nslots, &f);
    if (JS_IsException(obj))
        return obj;
    *slots = f->slots;
    return obj;
}

/* Resume a native generator (JSNativeGeneratorStep). A suspended step
 * function returns JS_UNINITIALIZED with 2 * state + kind as its payload,
 * having stored what it yields in *value. */
static JSValue lum_gen_step(JSContext *ctx, JSValueConst frame, int magic, JSValueConst value,
                              int *pkind)
{
    LumFrame *f = JS_GetOpaque(frame, lum_frame_class);
    JSValue io = value, r;
    r = lum_steps[f->fn](ctx, f->slots, f->state, &io, magic);
    /* (the slots: written by the step) */
    lum_wb(ctx, f->self);
    if (JS_IsUninitialized(r)) {
        /* suspended: 4 * state + kind (0: yield, 1: yield an iterator
           result as is, 3: await) */
        f->state = JS_VALUE_GET_INT(r) >> 2;
        *pkind = JS_VALUE_GET_INT(r) & 3;
        return io;
    }
    *pkind = 2;
    return r;
}

/* The generator object for `frame` (taken), once the parameters are bound
 * (up to the initial suspension); `func_data` is the data of the function
 * object called. */
JSValue lum_gen_new(JSContext *ctx, JSValue frame, JSValueConst *func_data)
{
    LumFrame *f = JS_GetOpaque(frame, lum_frame_class);
    JSValue io = JS_UNDEFINED, r;
    r = lum_steps[f->fn](ctx, f->slots, 0, &io, 0);
    lum_wb(ctx, f->self);
    if (JS_IsException(r)) {
        JS_FreeValue(ctx, frame);
        return r;
    }
    f->state = JS_VALUE_GET_INT(r) >> 2;
    return JS_NewNativeGenerator(ctx, JS_GetFunctionOfData(func_data), lum_gen_step, frame);
}

/* lum_gen_new for an async generator. */
JSValue lum_async_gen_new(JSContext *ctx, JSValue frame, JSValueConst *func_data)
{
    LumFrame *f = JS_GetOpaque(frame, lum_frame_class);
    JSValue io = JS_UNDEFINED, r;
    r = lum_steps[f->fn](ctx, f->slots, 0, &io, 0);
    lum_wb(ctx, f->self);
    if (JS_IsException(r)) {
        JS_FreeValue(ctx, frame);
        return r;
    }
    f->state = JS_VALUE_GET_INT(r) >> 2;
    return JS_NewNativeAsyncGenerator(ctx, JS_GetFunctionOfData(func_data), lum_gen_step, frame);
}

/* `yield*` in an async generator, as the interpreter does it: resume the
 * inner iterator (v = { iterator, next method, value }, the value taken) as
 * the generator was resumed (`mode`: next / return / throw = 0 / 1 / 2; a
 * returned value is already awaited). Sets *kind: 0 for an inner result to
 * await, 2 for a value to return (no `return` method), 4 for the result of
 * closing the iterator, to await before throwing (no `throw` method). */
JSValue lum_async_yield_star(JSContext *ctx, JSValue *v, int mode, int *kind)
{
    JSValue iter = v[0], received = v[2], method, r;
    *kind = 0;
    if (mode == 0) {
        r = JS_Call(ctx, v[1], iter, 1, (JSValueConst *)&received);
        JS_FreeValue(ctx, received);
        return r;
    }
    method = JS_GetPropertyStr(ctx, iter, mode == 2 ? "throw" : "return");
    if (JS_IsException(method)) {
        JS_FreeValue(ctx, received);
        return method;
    }
    if (!JS_IsUndefined(method) && !JS_IsNull(method)) {
        r = JS_Call(ctx, method, iter, 1, (JSValueConst *)&received);
        JS_FreeValue(ctx, method);
        JS_FreeValue(ctx, received);
        return r;
    }
    if (mode == 1) {
        *kind = 2;
        return received;
    }
    /* No `throw`: close the iterator (awaited), then a TypeError. */
    JS_FreeValue(ctx, received);
    method = JS_GetPropertyStr(ctx, iter, "return");
    if (JS_IsException(method))
        return method;
    if (JS_IsUndefined(method) || JS_IsNull(method))
        return lum_throw_iterator_throw(ctx), JS_EXCEPTION;
    r = JS_Call(ctx, method, iter, 0, NULL);
    JS_FreeValue(ctx, method);
    *kind = 4;
    return r;
}

/* An awaited inner result of `yield*` (taken): its value, and whether it
 * is done (*done). TypeError if not an object. */
JSValue lum_async_yield_star_result(JSContext *ctx, JSValue r, int *done)
{
    JSValue d, value;
    if (!JS_IsObject(r)) {
        JS_FreeValue(ctx, r);
        return JS_ThrowTypeError(ctx, "not an object");
    }
    d = JS_GetPropertyStr(ctx, r, "done");
    if (JS_IsException(d)) {
        JS_FreeValue(ctx, r);
        return d;
    }
    *done = JS_ToBool(ctx, d);
    JS_FreeValue(ctx, d);
    value = JS_GetPropertyStr(ctx, r, "value");
    JS_FreeValue(ctx, r);
    return value;
}

int lum_throw_iterator_throw(JSContext *ctx)
{
    JS_ThrowTypeError(ctx, "iterator does not have a throw method");
    return -1;
}

/* One step of `yield*`, as the interpreter does it: resume the inner
 * iterator (v = { iterator, next method, value }, the value taken) as the
 * generator was resumed (`mode`: next / return / throw = 0 / 1 / 2). Sets
 * *kind: 0 for an iterator result to yield as is, 1 for the value of the
 * `yield*` (inner iterator done), 2 for a value to return. */
JSValue lum_yield_star(JSContext *ctx, JSValue *v, int mode, int *kind)
{
    JSValue iter = v[0], received = v[2], method, r, done;
    int is_done;
    if (mode == 0) {
        r = JS_Call(ctx, v[1], iter, 1, (JSValueConst *)&received);
    } else {
        method = JS_GetPropertyStr(ctx, iter, mode == 2 ? "throw" : "return");
        if (JS_IsException(method)) {
            JS_FreeValue(ctx, received);
            return method;
        }
        if (JS_IsUndefined(method) || JS_IsNull(method)) {
            if (mode == 1) {
                *kind = 2;
                return received;
            }
            /* No `throw`: close the iterator, then a TypeError. */
            JS_FreeValue(ctx, received);
            method = JS_GetPropertyStr(ctx, iter, "return");
            if (JS_IsException(method))
                return method;
            if (!JS_IsUndefined(method) && !JS_IsNull(method)) {
                r = JS_Call(ctx, method, iter, 0, NULL);
                JS_FreeValue(ctx, method);
                if (JS_IsException(r))
                    return r;
                JS_FreeValue(ctx, r);
            }
            return JS_ThrowTypeError(ctx, "iterator does not have a throw method");
        }
        r = JS_Call(ctx, method, iter, 1, (JSValueConst *)&received);
        JS_FreeValue(ctx, method);
    }
    JS_FreeValue(ctx, received);
    if (JS_IsException(r))
        return r;
    if (!JS_IsObject(r)) {
        JS_FreeValue(ctx, r);
        return JS_ThrowTypeError(ctx, "not an object");
    }
    done = JS_GetPropertyStr(ctx, r, "done");
    if (JS_IsException(done)) {
        JS_FreeValue(ctx, r);
        return done;
    }
    is_done = JS_ToBool(ctx, done);
    JS_FreeValue(ctx, done);
    if (is_done < 0) {
        JS_FreeValue(ctx, r);
        return JS_EXCEPTION;
    }
    if (!is_done) {
        *kind = 0;
        return r;
    }
    *kind = mode == 1 ? 2 : 1;
    done = JS_GetPropertyStr(ctx, r, "value");
    JS_FreeValue(ctx, r);
    return done;
}

/* Make the new function object `func` (returned) look like an async
 * function (kind 1), generator function (kind 2) or async generator function
 * (kind 3): the same prototype as
 * its interpreted `fallback`, or the intrinsic one for closures (no
 * fallback); generator functions also get their `prototype` object. */
JSValue lum_function_kind(JSContext *ctx, JSValue func, JSValueConst fallback, int kind)
{
    JSValue proto, gen_proto;
    int r;
    if (kind == 0 || JS_IsException(func))
        return func;
    if (kind == 4) {
        /* An ordinary function: a constructor with a `prototype` (made when
           first read). */
        if (JS_SetCFunctionDataConstructor(ctx, func) < 0)
            goto fail;
        return func;
    }
    if (JS_IsUndefined(fallback))
        proto = JS_DupValue(ctx, lum_pins[kind == 1   ? LUM_PIN_ASYNC_FUNCTION_PROTO
                                            : kind == 2 ? LUM_PIN_GENERATOR_FUNCTION_PROTO
                                                        : LUM_PIN_ASYNC_GENERATOR_FUNCTION_PROTO]);
    else
        proto = JS_GetPrototype(ctx, fallback);
    if (JS_IsException(proto))
        goto fail;
    r = JS_SetPrototype(ctx, func, proto);
    if (r < 0 || kind == 1) {
        JS_FreeValue(ctx, proto);
        if (r < 0)
            goto fail;
        return func;
    }
    if (JS_IsUndefined(fallback)) {
        JSValue base = JS_GetPropertyStr(ctx, proto, "prototype");
        gen_proto = JS_IsException(base) ? base : JS_NewObjectProto(ctx, base);
        JS_FreeValue(ctx, base);
    } else {
        gen_proto = JS_GetPropertyStr(ctx, fallback, "prototype");
    }
    JS_FreeValue(ctx, proto);
    if (JS_IsException(gen_proto) ||
        JS_DefinePropertyValueStr(ctx, func, "prototype", gen_proto, JS_PROP_WRITABLE | JS_PROP_THROW) < 0)
        goto fail;
    return func;
 fail:
    JS_FreeValue(ctx, func);
    return JS_EXCEPTION;
}

/* ---- classes inside functions ---- */

/* A class's constructor v[0] (kind 5), given its superclass v[1] if
   has_parent (`extends`): its prototype, as the class definition makes it,
   which is returned; the constructor's prototype the superclass. */
JSValue lum_class_setup(JSContext *ctx, JSValue *v, int has_parent)
{
    JSValue F = v[0], parent = v[1], parent_proto, proto;
    if (!has_parent) {
        proto = JS_NewObject(ctx);
    } else if (JS_IsNull(parent)) {
        proto = JS_NewObjectProto(ctx, JS_NULL);
    } else {
        if (!JS_IsConstructor(ctx, parent))
            return JS_ThrowTypeError(ctx, "parent class must be constructor");
        parent_proto = JS_GetPropertyStr(ctx, parent, "prototype");
        if (JS_IsException(parent_proto))
            return parent_proto;
        if (!JS_IsNull(parent_proto) && !JS_IsObject(parent_proto)) {
            JS_FreeValue(ctx, parent_proto);
            return JS_ThrowTypeError(ctx, "parent prototype must be an object or null");
        }
        if (JS_SetPrototype(ctx, F, parent) < 0) {
            JS_FreeValue(ctx, parent_proto);
            return JS_EXCEPTION;
        }
        proto = JS_NewObjectProto(ctx, parent_proto);
        JS_FreeValue(ctx, parent_proto);
    }
    if (JS_IsException(proto))
        return proto;
    if (JS_DefinePropertyValueStr(ctx, proto, "constructor", JS_DupValue(ctx, F),
                                  JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE | JS_PROP_THROW) < 0 ||
        JS_DefinePropertyValueStr(ctx, F, "prototype", JS_DupValue(ctx, proto), JS_PROP_THROW) < 0) {
        JS_FreeValue(ctx, proto);
        return JS_EXCEPTION;
    }
    return proto;
}

/* A class's method (kind 0), getter (1) or setter (2) v[1] defined on v[0]
   (its prototype, or the class) as `key`, as the class definition defines
   them: not enumerable. */
int lum_define_method(JSContext *ctx, JSValue *v, JSAtom key, int kind)
{
    if (kind == 0)
        return JS_DefinePropertyValue(ctx, v[0], key, JS_DupValue(ctx, v[1]),
                                      JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE | JS_PROP_THROW);
    return JS_DefineProperty(ctx, v[0], key, JS_UNDEFINED, kind == 1 ? v[1] : JS_UNDEFINED,
                             kind == 2 ? v[1] : JS_UNDEFINED,
                             (kind == 1 ? JS_PROP_HAS_GET : JS_PROP_HAS_SET) | JS_PROP_HAS_CONFIGURABLE |
                                 JS_PROP_CONFIGURABLE | JS_PROP_HAS_ENUMERABLE | JS_PROP_THROW);
}

/* The same with a computed key v[2] (a property key): the function named
   for it, as the class definition names it. */
int lum_define_method_computed(JSContext *ctx, JSValue *v, int kind)
{
    JSValue name;
    JSAtom key;
    int r;
    if (JS_IsSymbol(v[2])) {
        JSValue desc = JS_GetPropertyStr(ctx, v[2], "description");
        if (JS_IsException(desc))
            return -1;
        if (JS_IsUndefined(desc)) {
            name = JS_NewString(ctx, "");
        } else {
            JSValue parts[3] = { JS_NewString(ctx, "["), desc, JS_NewString(ctx, "]") };
            name = JS_ConcatStrings(ctx, parts, 3);
        }
    } else {
        name = JS_ToString(ctx, v[2]);
    }
    if (!JS_IsException(name) && kind) {
        JSValue parts[2] = { JS_NewString(ctx, kind == 1 ? "get " : "set "), name };
        name = JS_ConcatStrings(ctx, parts, 2);
    }
    if (JS_IsException(name) ||
        JS_DefinePropertyValueStr(ctx, v[1], "name", name, JS_PROP_CONFIGURABLE | JS_PROP_THROW) < 0)
        return -1;
    key = JS_ValueToAtom(ctx, v[2]);
    if (key == JS_ATOM_NULL)
        return -1;
    r = lum_define_method(ctx, v, key, kind);
    JS_FreeAtom(ctx, key);
    return r;
}

/* The instance a base class's constructor makes for `new_target`: an
   object of its `prototype` (of Object.prototype if that is not an
   object). */
JSValue lum_new_from_target(JSContext *ctx, JSValue new_target)
{
    static JSAtom prototype = JS_ATOM_NULL;
    JSValue proto, obj;
    if (prototype == JS_ATOM_NULL)
        prototype = JS_NewAtom(ctx, "prototype"); /* (predefined: the same in every runtime) */
    proto = JS_GetProperty(ctx, new_target, prototype);
    if (JS_IsException(proto))
        return proto;
    if (!JS_IsObject(proto)) {
        JS_FreeValue(ctx, proto);
        return JS_NewObject(ctx);
    }
    obj = JS_NewObjectProto(ctx, proto);
    JS_FreeValue(ctx, proto);
    return obj;
}

/* ---- super ---- */

/* super[key] with v = { home object, this, key } (borrowed). */
JSValue lum_super_get(JSContext *ctx, JSValue *v)
{
    JSValue base = JS_GetPrototype(ctx, v[0]), r;
    JSAtom atom;
    if (JS_IsException(base))
        return base;
    atom = JS_ValueToAtom(ctx, v[2]);
    if (atom == JS_ATOM_NULL) {
        JS_FreeValue(ctx, base);
        return JS_EXCEPTION;
    }
    r = JS_GetPropertyReceiver(ctx, base, atom, v[1]);
    JS_FreeAtom(ctx, atom);
    JS_FreeValue(ctx, base);
    return r;
}

/* super[key] = val, v as for lum_super_get; val taken. */
int lum_super_set(JSContext *ctx, JSValue *v, JSValue val)
{
    JSValue base = JS_GetPrototype(ctx, v[0]);
    JSAtom atom;
    int r;
    if (JS_IsException(base)) {
        JS_FreeValue(ctx, val);
        return -1;
    }
    atom = JS_ValueToAtom(ctx, v[2]);
    if (atom == JS_ATOM_NULL) {
        JS_FreeValue(ctx, base);
        JS_FreeValue(ctx, val);
        return -1;
    }
    r = JS_SetPropertyReceiver(ctx, base, atom, val, v[1], JS_PROP_THROW);
    JS_FreeAtom(ctx, atom);
    JS_FreeValue(ctx, base);
    return r;
}

/* obj.#name = val with v = { obj, name } (borrowed); val taken. (A third
 * JSValue argument would not fit in argument registers.) */
int lum_private_set(JSContext *ctx, JSValue *v, JSValue val)
{
    return JS_PrivateSet(ctx, v[0], v[1], val);
}

/* A native constructor's private field v[1] defined on v[0] (val taken). */
int lum_private_define(JSContext *ctx, JSValue *v, JSValue val)
{
    return JS_PrivateDefine(ctx, v[0], v[1], val);
}

/* obj[key] in a for-in: v = { iterator, obj, key } (borrowed). */
JSValue lum_for_in_get(JSContext *ctx, JSValue *v)
{
    /* (v[3]: the index after the key in a shape's shared iterator, else
       undefined) */
    if (JS_VALUE_GET_TAG(v[3]) == JS_TAG_INT)
        return JS_ForInGetAt(ctx, v[0], JS_VALUE_GET_INT(v[3]) - 1, v[1], v[2]);
    return JS_ForInGet(ctx, v[0], v[1], v[2]);
}

/* Whether an iteration with this iterator and `next` method can resume a
 * native generator directly: the original %GeneratorPrototype%.next. */
int lum_gen_iter_ok(JSContext *ctx, JSValue iter, JSValue next)
{
    JSValue orig = lum_pins[LUM_PIN_GENERATOR_NEXT];
    return JS_IsNativeGenerator(iter) && JS_VALUE_GET_NORM_TAG(next) == JS_VALUE_GET_NORM_TAG(orig) &&
           JS_VALUE_GET_PTR(next) == JS_VALUE_GET_PTR(orig);
}

/* ---- for await ---- */

/* An awaited iterator result of `for await` (borrowed): its value, and
 * whether it is done (*done). */
JSValue lum_for_await_result(JSContext *ctx, JSValue r, int *done)
{
    JSValue d;
    if (!JS_IsObject(r))
        return JS_ThrowTypeError(ctx, "iterator must return an object");
    d = JS_GetPropertyStr(ctx, r, "done");
    if (JS_IsException(d))
        return d;
    *done = JS_ToBool(ctx, d);
    JS_FreeValue(ctx, d);
    return JS_GetPropertyStr(ctx, r, "value");
}

/* `__proto__: proto` in an object literal (`obj` new, both borrowed): its
   prototype, if `proto` is an object or null. */
int lum_literal_proto(JSContext *ctx, JSValue obj, JSValue proto)
{
    if (!JS_IsObject(proto) && !JS_IsNull(proto))
        return 0;
    return JS_SetPrototype(ctx, obj, proto) < 0 ? -1 : 0;
}

/* Whether object `v` (borrowed) is callable: typeof's "function" for the
   classes native code does not know. */
int lum_is_function(JSContext *ctx, JSValue v)
{
    return JS_IsFunction(ctx, v);
}

/* TypeError unless `v` (borrowed) is an object. */
int lum_check_object(JSContext *ctx, JSValue v)
{
    if (JS_IsObject(v))
        return 0;
    JS_ThrowTypeError(ctx, "not an object");
    return -1;
}

/* Leaving a `for await` by an exception: call the iterator's `return`
 * method (not awaited), ignoring what it does, as the interpreter does. */
void lum_iter_close_quiet(JSContext *ctx, JSValue iter)
{
    JSValue method, r;
    if (JS_IsUndefined(iter))
        return;
    method = JS_GetPropertyStr(ctx, iter, "return");
    if (JS_IsException(method)) {
        JS_FreeValue(ctx, JS_GetException(ctx));
        return;
    }
    if (JS_IsUndefined(method) || JS_IsNull(method))
        return;
    r = JS_Call(ctx, method, iter, 0, NULL);
    JS_FreeValue(ctx, method);
    if (JS_IsException(r))
        JS_FreeValue(ctx, JS_GetException(ctx));
    else
        JS_FreeValue(ctx, r);
}

/* ---- private fields through inline caches ---- */

/* A cache miss reading private field `name` (a private symbol) of obj (both
 * borrowed). */
JSValue lum_private_get_ic(JSContext *ctx, JSValue obj, JSValue name, LumIC *ic)
{
    JSValue v = JS_PrivateGet(ctx, obj, name);
    if (!JS_IsException(v) && JS_IsObject(obj)) {
        JSAtom a = JS_ValueToAtom(ctx, name);
        if (a != JS_ATOM_NULL) {
            lum_ic_fill(ctx, obj, a, 0, ic);
            JS_FreeAtom(ctx, a);
        }
    }
    return v;
}

/* A cache miss writing a private field: v = { obj, name } (borrowed), val
 * taken. */
int lum_private_set_ic(JSContext *ctx, JSValue *v, JSValue val, LumIC *ic)
{
    int r = JS_PrivateSet(ctx, v[0], v[1], val);
    if (r >= 0 && JS_IsObject(v[0])) {
        JSAtom a = JS_ValueToAtom(ctx, v[1]);
        if (a != JS_ATOM_NULL) {
            lum_ic_fill(ctx, v[0], a, 1, ic);
            JS_FreeAtom(ctx, a);
        }
    }
    return r;
}

/* ---- inline caches of prototype properties ---- */

/* Codegen reads entries at these offsets (lower::IC_SIZE). */
_Static_assert(sizeof(LumIC) == 80 && offsetof(LumIC, slot) == 8 && offsetof(LumIC, kind) == 12 &&
                   offsetof(LumIC, proto_shape) == 16 && offsetof(LumIC, mid_shape) == 24 &&
                   offsetof(LumIC, alt_shape) == 32 && offsetof(LumIC, poly) == 40 &&
                   offsetof(LumIC, epoch) == 48 && offsetof(LumIC, value) == 56 && offsetof(LumIC, entry) == 64,
               "LumIC layout");

/* Sites that never cache (as for objects with unshared shapes) stop trying
 * after this many misses, as megamorphic sites in JSC. An empty entry
 * counts them in `slot`. Reads at such a site look for an own data property at the
 * index where they last found it (in `kind`). */
#define LUM_IC_GIVE_UP 64

static bool lum_ic_gave_up(LumIC *ic)
{
    return !ic->shape && !ic->alt_shape && !ic->proto_shape && ic->slot >= LUM_IC_GIVE_UP;
}

static void lum_ic_count_miss(LumIC *ic)
{
    if (!ic->shape && !ic->alt_shape && !ic->proto_shape)
        ic->slot++;
}

/* A cache miss reading obj.prop (obj borrowed) at a site that also caches
 * the direct prototype's data properties (methods) and getters. */
/* obj.prop (obj borrowed) through `ic` as native code reads it: an own data
 * property of its shape, a prototype's data property or getter while
 * js_proto_epoch is unchanged; else as a miss. For native code's cold paths
 * (a field of a record type, where the object is not of a shape it learned). */
JSValue lum_ic_get_cached(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic)
{
    if (JS_VALUE_GET_TAG(obj) == JS_TAG_OBJECT) {
        char *p = JS_VALUE_GET_PTR(obj);
        void *shape = *(void **)(p + JS_LAYOUT_OBJ_SHAPE);
        if (shape == ic->shape)
            return JS_DupValue(ctx, ((JSValue *)*(char **)(p + JS_LAYOUT_OBJ_PROP))[ic->slot]);
        if (shape == ic->alt_shape && ic->epoch == js_proto_epoch) {
            if (ic->kind == LUM_IC_PROTO_DATA)
                return JS_DupValue(ctx, ic->value);
            if (ic->kind == LUM_IC_PROTO_GETTER)
                return lum_call_getter(ctx, JS_VALUE_GET_PTR(ic->value), obj);
        }
    }
    return lum_ic_get_proto(ctx, obj, prop, ic);
}

JSValue lum_ic_get_proto(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic)
{
    JSValue v;
    LumStruct *s = lum_struct_of(obj);
    if (s) {
        /* An object of an `aot type`: its field, read directly, and
           cached (LUM_IC_STRUCT) when it is one. */
        int fi = lum_field_index(s->layout, prop);
        if (fi < 0 || !(s->mask & (1u << fi)))
            return JS_GetProperty(ctx, obj, prop);
        const LumFieldDesc *f = &lum_layouts[s->layout].fields[fi];
        lum_ic_store(ctx, ic, JS_DupObjectShape(obj), NULL, NULL, s->layout | f->offset << 16,
                       LUM_IC_STRUCT | fi << 8 | f->storage << 16);
        return lum_field_get(ctx, s, f);
    }
    if (JS_VALUE_GET_TAG(obj) == JS_TAG_STRING) {
        /* A method of a string: a data property of String.prototype (not
           one strings have: an index or `length`). */
        uint32_t idx;
        void *shape;
        int slot;
        v = JS_GetProperty(ctx, obj, prop);
        if (!JS_IsException(v) && !lum_ic_gave_up(ic) && prop != lum_atom_string_length &&
            !JS_AtomToArrayIndex(ctx, &idx, prop)) {
            slot = JS_CacheOwnProperty(ctx, lum_pins[LUM_PIN_STRING_PROTO], prop, 0, &shape);
            if (slot >= 0)
                lum_ic_store(ctx, ic, NULL, shape, NULL, slot, LUM_IC_STRING);
            else
                lum_ic_count_miss(ic);
        }
        return v;
    }
    if (lum_ic_gave_up(ic)) {
        /* `kind` holds the index of the property last found */
        v = JS_GetOwnDataHint(ctx, obj, prop, &ic->kind);
        if (!JS_IsUninitialized(v))
            return v;
        return JS_GetProperty(ctx, obj, prop);
    }
    {
        JSValue *at = lum_ic_poly_slot(ic, obj);
        void *g;
        if (at)
            return JS_DupValue(ctx, *at);
        at = lum_ic_poly_getter(ic, obj);
        if (at && (g = lum_accessor(at, 0)))
            return lum_call_getter(ctx, g, obj);
    }
    v = JS_GetProperty(ctx, obj, prop);
    if (lum_ic_full(ic))
        return v;
    if (!JS_IsException(v) && !lum_ic_fill(ctx, obj, prop, 0, ic) &&
        !lum_ic_fill(ctx, obj, prop, 2, ic) &&
        !lum_ic_fill_proto(ctx, obj, prop, LUM_IC_PROTO_DATA, ic) &&
        !lum_ic_fill_proto(ctx, obj, prop, LUM_IC_PROTO_GETTER, ic) &&
        !lum_ic_fill_deep(ctx, obj, prop, ic) &&
        !lum_ic_fill_deep_getter(ctx, obj, prop, ic) &&
        !(JS_IsUndefined(v) && lum_ic_fill_absent(ctx, obj, prop, ic, true)))
        lum_ic_count_miss(ic);
    return v;
}

/* A cache miss writing obj.prop = val (obj borrowed, val taken) at a site
 * that also caches the direct prototype's setters. */
int lum_ic_set_proto(JSContext *ctx, JSValue obj, JSAtom prop, JSValue val, LumIC *ic)
{
    void *before = NULL;
    int r;
    if (lum_ic_poly_set(ctx, ic, obj, val))
        return 0;
    if ((r = lum_ic_add(ctx, ic, obj, val)))
        return r < 0 ? -1 : 0;
    /* (A shared shape, held: an addition may free it.) */
    if (JS_VALUE_GET_TAG(obj) == JS_TAG_OBJECT && !lum_ic_gave_up(ic) && !lum_ic_full(ic))
        before = JS_DupSharedShape(*(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE));
    r = JS_SetProperty(ctx, obj, prop, val);
    if (before) {
        if (r >= 0 &&
            (before == *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE)
                 ? !lum_ic_fill(ctx, obj, prop, 1, ic) && !lum_ic_fill(ctx, obj, prop, 3, ic) &&
                       !lum_ic_fill_proto(ctx, obj, prop, LUM_IC_PROTO_SETTER, ic)
                 : !lum_ic_fill_add(ctx, obj, prop, before, ic)))
            lum_ic_count_miss(ic);
        JS_FreeCachedShape(JS_GetRuntime(ctx), before);
    } else if (r >= 0 && JS_VALUE_GET_TAG(obj) == JS_TAG_OBJECT && !lum_ic_gave_up(ic)) {
        /* (An object's own shape: nothing to cache.) */
        lum_ic_count_miss(ic);
    }
    return r;
}

/* Call a cached getter (a function object) on this_obj (borrowed), as the
 * engine does: holding the function while it runs. */
JSValue lum_call_getter(JSContext *ctx, void *getter, JSValue this_obj)
{
    /* (%TypedArray%.prototype's `length` and `byteLength`: read directly.) */
    JSValue f = JS_TypedArrayGetterValue(this_obj, getter);
    if (!JS_IsUninitialized(f))
        return f;
    f = JS_DupValue(ctx, JS_MKPTR(JS_TAG_OBJECT, getter));
    JSValue r = JS_CallFast(ctx, f, this_obj, 0, NULL);
    JS_FreeValue(ctx, f);
    return r;
}

/* Call a cached setter with val (taken). */
int lum_call_setter(JSContext *ctx, void *setter, JSValue this_obj, JSValue val)
{
    JSValue f = JS_DupValue(ctx, JS_MKPTR(JS_TAG_OBJECT, setter));
    JSValue r = JS_CallFast(ctx, f, this_obj, 1, (JSValueConst *)&val);
    JS_FreeValue(ctx, f);
    JS_FreeValue(ctx, val);
    if (JS_IsException(r))
        return -1;
    JS_FreeValue(ctx, r);
    return 0;
}

/* `super.prop` with receiver this_obj (both borrowed; home: the home
 * object), caching where it is found among the home object's prototypes. */
JSValue lum_super_get_ic(JSContext *ctx, JSValue home, JSValue this_obj, JSAtom prop, LumIC *ic)
{
    JSValue base = JS_GetPrototype(ctx, home), r;
    if (JS_IsException(base))
        return base;
    r = JS_GetPropertyReceiver(ctx, base, prop, this_obj);
    JS_FreeValue(ctx, base);
    if (!JS_IsException(r) && !lum_ic_fill_proto(ctx, home, prop, LUM_IC_PROTO_DATA, ic))
        lum_ic_fill_proto(ctx, home, prop, LUM_IC_PROTO_GETTER, ic);
    return r;
}

/* `get name() {}` / `set name(v) {}` in an object literal: an enumerable,
 * configurable accessor (merged with the other half if defined). Takes
 * ownership of fn. */
int lum_define_accessor(JSContext *ctx, JSValue *obj, JSAtom prop, JSValue fn, int is_setter)
{
    int flags = JS_PROP_HAS_ENUMERABLE | JS_PROP_ENUMERABLE | JS_PROP_HAS_CONFIGURABLE | JS_PROP_CONFIGURABLE |
                JS_PROP_THROW | (is_setter ? JS_PROP_HAS_SET : JS_PROP_HAS_GET);
    int r = JS_DefineProperty(ctx, *obj, prop, JS_UNDEFINED, is_setter ? JS_UNDEFINED : fn,
                              is_setter ? fn : JS_UNDEFINED, flags);
    JS_FreeValue(ctx, fn);
    return r;
}

static JSValue lum_new_object_proto(JSContext *ctx, LumIC *ic, JSValue obj, int n, JSAtom *atoms,
                                      JSValue *values);

/* An object literal { atoms[i]: values[i] } (values taken) at an allocation
 * site that caches the literal's final shape in `ic`: later objects are
 * made with it directly (JS_NewObjectWithCachedShape). */
JSValue lum_new_object(JSContext *ctx, LumIC *ic, int n, JSAtom *atoms, JSValue *values)
{
    return lum_new_object_proto(ctx, ic, JS_NewObject(ctx), n, atoms, values);
}

/* An instance of a class with a native constructor: { atoms[i]: values[i] }
 * (values taken) with prototype proto (borrowed), as lum_new_object. */
JSValue lum_new_instance(JSContext *ctx, LumIC *ic, JSValue proto, int n, JSAtom *atoms, JSValue *values)
{
    return lum_new_object_proto(ctx, ic, JS_NewObjectProto(ctx, proto), n, atoms, values);
}

/* A new instance of a class with private state, as a native constructor
 * makes it: its public fields (n, named by atoms), then the brand of the
 * class whose home object is extra[0] (none if it is undefined), then its
 * private fields (named by extra[1..]). values: the public fields', the
 * brand's (undefined) if any, the private fields' (all taken). The site
 * caches the final shape, with which native code makes the next ones. */
JSValue lum_new_instance_full(JSContext *ctx, LumIC *ic, JSValue proto, int n, JSAtom *atoms, JSValue *values,
                              JSValue *extra, int nprivate)
{
    int brand = !JS_IsUndefined(extra[0]), i;
    JSValue *pv = values + n + brand;
    JSValue obj = JS_NewObjectProto(ctx, proto);
    void *shape;
    for (i = 0; i < n; i++) {
        if (JS_IsException(obj) ||
            JS_DefinePropertyValue(ctx, obj, atoms[i], values[i], JS_PROP_C_W_E | JS_PROP_THROW) < 0)
            goto fail;
    }
    if (brand && JS_PrivateAddBrand(ctx, obj, extra[0]) < 0)
        goto fail;
    for (i = 0; i < nprivate; i++) {
        if (JS_PrivateDefine(ctx, obj, extra[1 + i], pv[i]) < 0)
            goto fail;
    }
    if (JS_CachePlainShape(ctx, obj, n + brand + nprivate, &shape) == 0) {
        lum_ic_store(ctx, ic, shape, NULL, NULL, 0, LUM_IC_OWN);
        ic->slot = JS_ShapeAllocInfo(JS_GetRuntime(ctx), shape);
    }
    return obj;
fail:
    /* (The values are the collector's.) */
    JS_FreeValue(ctx, obj);
    return JS_EXCEPTION;
}

static JSValue lum_new_object_proto(JSContext *ctx, LumIC *ic, JSValue obj, int n, JSAtom *atoms,
                                      JSValue *values)
{
    void *shape;
    int i;
    for (i = 0; i < n; i++) {
        if (JS_IsException(obj) ||
            JS_DefinePropertyValue(ctx, obj, atoms[i], values[i], JS_PROP_C_W_E | JS_PROP_THROW) < 0) {
            if (JS_IsException(obj))
                JS_FreeValue(ctx, values[i]);
            while (++i < n)
                JS_FreeValue(ctx, values[i]);
            JS_FreeValue(ctx, obj);
            return JS_EXCEPTION;
        }
    }
    if (JS_CachePlainShape(ctx, obj, n, &shape) == 0) {
        lum_ic_store(ctx, ic, shape, NULL, NULL, 0, LUM_IC_OWN);
        /* How native code allocates the next ones inline (an empty one
           with room for the properties added next). */
        ic->slot = JS_ShapeAllocInfoRoom(JS_GetRuntime(ctx), shape, n ? 0 : LUM_OBJECT_ROOM);
    }
    return obj;
}

/* Codegen reads these (lower::FN_CACHE_SIZE). */
_Static_assert(sizeof(LumFnCache) == 80 && offsetof(LumFnCache, info) == 12 && offsetof(LumFnCache, tmpl) == 16,
               "LumFnCache layout");

/* A closure of a native function (data borrowed): made with the shape of
 * the function's first closure, as JSC allocates functions with a known
 * structure. Of `kind` 0 (an arrow function or method) or 1 (async: of
 * AsyncFunction.prototype), its closures stay as made (no `prototype` of
 * their own), and native code makes the next ones inline; of another
 * kind, lum_function_kind finishes it. */
JSValue lum_new_closure(JSContext *ctx, LumFnCache *c, JSCFunctionFast *fast, JSCFunctionData *slow,
                          const char *name, int length, int magic, int n, JSValue *data, int kind)
{
    JSValue f;
    if (c->name == JS_ATOM_NULL) {
        c->name = JS_NewAtom(ctx, name);
        if (c->name == JS_ATOM_NULL)
            return JS_EXCEPTION;
    }
    f = JS_NewCFunctionDataShaped(ctx, fast, slow, c->name, length, magic, n, (JSValueConst *)data,
                                  kind == 1 ? lum_pins[LUM_PIN_ASYNC_FUNCTION_PROTO] : JS_UNDEFINED, &c->shape);
    if ((kind == 0 || kind == 1) && !c->info && c->shape && !JS_IsException(f))
        c->info = JS_CFunctionDataTemplate(ctx, fast, slow, c->name, length, magic, n, c->shape, &c->tmpl);
    return f;
}

/* Whether `promise.resolve` was the intrinsic Promise.resolve (both
 * borrowed): then `await Promise.resolve(x)` is `await x`. */
int lum_is_promise_resolve(JSValue promise, JSValue resolve)
{
    return JS_VALUE_GET_TAG(promise) == JS_TAG_OBJECT && JS_VALUE_GET_TAG(resolve) == JS_TAG_OBJECT &&
           JS_VALUE_GET_PTR(promise) == JS_VALUE_GET_PTR(lum_pins[LUM_PIN_PROMISE]) &&
           JS_VALUE_GET_PTR(resolve) == JS_VALUE_GET_PTR(lum_pins[LUM_PIN_PROMISE_RESOLVE]);
}

/* A global variable read: the global object's property is looked for at
 * the index where this site last found it (in `slot`). While there are no
 * global let/const definitions, an own data property of the global object
 * is cached, as an own property read's (native code reads it inline). */
JSValue lum_global_get(JSContext *ctx, JSAtom prop, LumIC *ic)
{
    JSValue v = JS_GetGlobalVariableHint(ctx, prop, &ic->slot);
    JSValue var_obj = *(JSValue *)((char *)ctx + JS_LAYOUT_CTX_GLOBAL_VAR_OBJ);
    JSValue global = *(JSValue *)((char *)ctx + JS_LAYOUT_CTX_GLOBAL_OBJ);
    void *var_shape = *(void **)((char *)JS_VALUE_GET_PTR(var_obj) + JS_LAYOUT_OBJ_SHAPE);
    if (!JS_IsException(v) && *(uint32_t *)((char *)var_shape + JS_LAYOUT_SHAPE_PROP_COUNT) == 0) {
        /* (Redefined properties, as lum_lock_builtins's, leave it a
           shape of its own, which caches cannot hold.) */
        JS_ShareObjectShape(ctx, global);
        lum_ic_fill(ctx, global, prop, 0, ic);
    }
    return v;
}

/* ---- `aot type` objects ----
 *
 * An object of an `aot type` is an object of class lum_struct_class with
 * its fields in a LumStruct (its opaque), laid out as lum_layouts says.
 * Native code reads and writes the fields there directly. To JS it looks
 * like a plain object whose fields are its own enumerable, writable data
 * properties, but it keeps to its type: fields cannot be added, deleted or
 * redefined, and a field only takes values of its type (else TypeError).
 * An `int` field wraps other integers to int32, as `int` variables do. */

uint32_t lum_struct_class;
/* Per layout field (flat, layout by layout): its name as an atom. */
static JSAtom *lum_struct_atoms;
static uint32_t *lum_struct_atom_base;
/* Per layout field (as lum_struct_atoms): its JSON key, `,"name":` (then 8
   zero bytes), or NULL when the name needs escapes or is not ASCII
   (written from its atom). */
typedef struct LumJSONKey {
    char *text;
    uint32_t len;
} LumJSONKey;
static LumJSONKey *lum_struct_json_keys;
/* lum_struct_lits as JS strings. */
static JSValue *lum_struct_lit_values;

static int lum_layout_count(void)
{
    return &lum_nlayouts ? lum_nlayouts : 0;
}

static inline void *lum_field_at(LumStruct *s, const LumFieldDesc *f)
{
    return (uint8_t *)s->data + f->offset;
}

static size_t lum_field_size(const LumFieldDesc *f)
{
    static const size_t sizes[] = { 4, 8, 1, sizeof(JSValue) };
    return sizes[f->storage];
}

static LumStruct *lum_struct_of(JSValueConst v)
{
    if (JS_VALUE_GET_TAG(v) != JS_TAG_OBJECT || JS_GetClassID(v) != lum_struct_class)
        return NULL;
    return JS_GetOpaque(v, lum_struct_class);
}

/* The index of field `prop` of a layout, or -1. */
static int lum_field_index(int layout, JSAtom prop)
{
    const LumLayoutDesc *l = &lum_layouts[layout];
    uint32_t i;
    for (i = 0; i < l->nfields; i++) {
        if (lum_struct_atoms[lum_struct_atom_base[layout] + i] == prop)
            return i;
    }
    return -1;
}

static JSValue lum_field_get(JSContext *ctx, LumStruct *s, const LumFieldDesc *f)
{
    void *at = lum_field_at(s, f);
    switch (f->storage) {
    case LUM_ST_INT32:
        return JS_NewInt32(ctx, *(int32_t *)at);
    case LUM_ST_F64:
        return JS_NewNumber(ctx, *(double *)at);
    case LUM_ST_BOOL:
        return JS_NewBool(ctx, *(uint8_t *)at);
    default:
        return JS_DupValue(ctx, *(JSValue *)at);
    }
}

static LumStructArray *lum_sarr_of(JSValueConst v);

/* An absent field's storage: `undefined`, or zero. */
static void lum_field_clear(LumStruct *s, const LumFieldDesc *f)
{
    if (f->storage == LUM_ST_VALUE)
        *(JSValue *)lum_field_at(s, f) = JS_UNDEFINED;
    else
        memset(lum_field_at(s, f), 0, lum_field_size(f));
}

static bool lum_value_fits(JSContext *ctx, const LumTypeDesc *t, JSValueConst v)
{
    int tag = JS_VALUE_GET_NORM_TAG(v);
    LumStruct *s;
    LumStructArray *a;
    uint32_t i;
    if (tag == JS_TAG_NULL && t->nullable)
        return true;
    switch (t->kind) {
    case LUM_FT_INT:
        if (tag == JS_TAG_INT)
            return true;
        if (tag == JS_TAG_FLOAT64) {
            double d = JS_VALUE_GET_FLOAT64(v);
            return d == (double)(int32_t)d && !(d == 0 && signbit(d)) && d >= INT32_MIN && d <= INT32_MAX;
        }
        return false;
    case LUM_FT_NUMBER:
        return tag == JS_TAG_INT || tag == JS_TAG_FLOAT64;
    case LUM_FT_BOOL:
        return tag == JS_TAG_BOOL;
    case LUM_FT_STRING:
        return tag == JS_TAG_STRING;
    case LUM_FT_LITERALS:
        if (tag != JS_TAG_STRING)
            return false;
        for (i = 0; i < t->nlits; i++) {
            if (JS_IsStrictEqual(ctx, v, lum_struct_lit_values[t->lit_base + i]))
                return true;
        }
        return false;
    case LUM_FT_STRUCT:
        s = lum_struct_of(v);
        return s && s->layout == t->layout;
    case LUM_FT_ARRAY:
        a = lum_sarr_of(v);
        return a && a->layout == t->layout;
    default:
        return false;
    }
}

/* Whether field `fi` of layout `l` may hold `v` (in some member). */
static bool lum_union_fits(JSContext *ctx, const LumLayoutDesc *l, int fi, JSValueConst v)
{
    uint32_t m;
    for (m = 0; m < l->nmembers; m++) {
        const LumTypeDesc *t = &l->members[m].types[fi];
        if (t->kind != LUM_FT_ABSENT && lum_value_fits(ctx, t, v))
            return true;
    }
    return false;
}

static void lum_field_error(JSContext *ctx, const LumLayoutDesc *l, int fi)
{
    JS_ThrowTypeError(ctx, "`%s` of aot type `%s` cannot hold this value", l->fields[fi].name, l->name);
}

/* Store v (taken) in field fi of s, the data of obj, checked against the
 * field's type; the old value of a JS-valued field goes to *old (owned). */
static int lum_put_raw(JSContext *ctx, JSValueConst obj, LumStruct *s, int fi, JSValue v, JSValue *old)
{
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    const LumFieldDesc *f = &l->fields[fi];
    void *at = lum_field_at(s, f);
    *old = JS_UNDEFINED;
    if (JS_IsUndefined(v) && (l->optional & (1u << fi))) {
        /* An optional field given `undefined`: absent. */
        if (f->storage == LUM_ST_VALUE)
            *old = *(JSValue *)at;
        lum_field_clear(s, f);
        s->mask &= ~(1u << fi);
        return 0;
    }
    if (f->storage == LUM_ST_INT32) {
        /* Integers wrap to int32 (unless `--no-int-wrapping`). */
        int tag = JS_VALUE_GET_NORM_TAG(v);
        double d = tag == JS_TAG_INT ? JS_VALUE_GET_INT(v) : tag == JS_TAG_FLOAT64 ? JS_VALUE_GET_FLOAT64(v) : NAN;
        int32_t i;
        if (!(d == trunc(d)) || isinf(d)) {
            JS_FreeValue(ctx, v);
            return lum_field_error(ctx, l, fi), -1;
        }
        if (JS_ToInt32(ctx, &i, v) < 0)
            return -1;
        if (i != d && &lum_int_wrapping && !lum_int_wrapping) {
            JS_FreeValue(ctx, v);
            return lum_field_error(ctx, l, fi), -1;
        }
        *(int32_t *)at = i;
        JS_FreeValue(ctx, v);
        return 0;
    }
    if (!lum_union_fits(ctx, l, fi, v)) {
        JS_FreeValue(ctx, v);
        return lum_field_error(ctx, l, fi), -1;
    }
    switch (f->storage) {
    case LUM_ST_F64:
        *(double *)at = JS_VALUE_GET_NORM_TAG(v) == JS_TAG_INT ? JS_VALUE_GET_INT(v) : JS_VALUE_GET_FLOAT64(v);
        break;
    case LUM_ST_BOOL:
        *(uint8_t *)at = JS_VALUE_GET_BOOL(v);
        break;
    default:
        *old = *(JSValue *)at;
        *(JSValue *)at = v;
        if (JS_VALUE_HAS_REF_COUNT(v))
            JS_GCWriteBarrier(JS_GetRuntime(ctx), JS_VALUE_GET_PTR(obj));
        return 0;
    }
    JS_FreeValue(ctx, v);
    return 0;
}

/* Whether s matches a member: has its fields (but optional ones), with its
 * types. */
static bool lum_struct_matches(JSContext *ctx, LumStruct *s)
{
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    uint32_t m, fi;
    for (m = 0; m < l->nmembers; m++) {
        const LumMemberDesc *mem = &l->members[m];
        bool ok = (mem->mask & ~mem->optional & ~s->mask) == 0;
        for (fi = 0; ok && fi < l->nfields; fi++) {
            if (!(mem->mask & s->mask & (1u << fi)))
                continue;
            JSValue v = lum_field_get(ctx, s, &l->fields[fi]);
            ok = lum_value_fits(ctx, &mem->types[fi], v);
            JS_FreeValue(ctx, v);
        }
        if (ok)
            return true;
    }
    return false;
}

/* lum_struct_matches for s whose fields were all stored checked (each
 * fits a member): with one member, that its required fields are there. */
static bool lum_struct_complete(JSContext *ctx, LumStruct *s)
{
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    if (l->nmembers == 1)
        return (l->members[0].mask & ~l->members[0].optional & ~s->mask) == 0;
    return lum_struct_matches(ctx, s);
}

/* A new object of a layout, its data in *ps. */
static JSValue lum_struct_alloc(JSContext *ctx, int layout, uint32_t mask, LumStruct **ps)
{
    const LumLayoutDesc *l = &lum_layouts[layout];
    void *data;
    JSValue obj = JS_NewObjectClassData(ctx, lum_struct_class, sizeof(LumStruct) + l->size, false, &data);
    LumStruct *s = data;
    uint32_t i;
    if (JS_IsException(obj))
        return obj;
    s->layout = layout;
    s->mask = mask;
    for (i = 0; i < l->nfields; i++) {
        if (l->fields[i].storage == LUM_ST_VALUE)
            *(JSValue *)lum_field_at(s, &l->fields[i]) = JS_UNDEFINED;
    }
    *ps = s;
    return obj;
}

JSValue lum_struct_new(JSContext *ctx, int layout, uint32_t mask)
{
    LumStruct *s;
    return lum_struct_alloc(ctx, layout, mask, &s);
}

/* A field of a new object (from native code), checked against its type. */
int lum_struct_put(JSContext *ctx, JSValue obj, int field, JSValue v)
{
    JSValue old;
    int r = lum_put_raw(ctx, obj, JS_GetOpaque(obj, lum_struct_class), field, v, &old);
    JS_FreeValue(ctx, old);
    return r;
}

/* A new object (from native code) must match a member of its type. */
int lum_struct_validate(JSContext *ctx, JSValue obj)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    if (lum_struct_matches(ctx, s))
        return 0;
    JS_ThrowTypeError(ctx, "not an object of aot type `%s`", lum_layouts[s->layout].name);
    return -1;
}

/* A write of field `field` (v taken): it must be present, or optional
 * (`undefined` makes it absent), and the object must still match a member
 * afterwards (else it is left as it was). */
int lum_struct_set(JSContext *ctx, JSValue obj, int field, JSValue v)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    const LumFieldDesc *f = &l->fields[field];
    uint32_t bit = 1u << field, mask = s->mask;
    uint64_t saved[2];
    JSValue old;
    if (!(mask & bit) && !(l->optional & bit)) {
        JS_FreeValue(ctx, v);
        JS_ThrowTypeError(ctx, "cannot add `%s` to an object of aot type `%s`", f->name, l->name);
        return -1;
    }
    memcpy(saved, lum_field_at(s, f), lum_field_size(f));
    s->mask |= bit;
    if (lum_put_raw(ctx, obj, s, field, v, &old) < 0) {
        s->mask = mask;
        return -1;
    }
    if (s->mask != mask ? !lum_struct_matches(ctx, s) : l->validate && !f->simple && !lum_struct_matches(ctx, s)) {
        if (f->storage == LUM_ST_VALUE)
            JS_FreeValue(ctx, *(JSValue *)lum_field_at(s, f));
        memcpy(lum_field_at(s, f), saved, lum_field_size(f));
        s->mask = mask;
        /* (`old` is back in the field.) */
        JS_ThrowTypeError(ctx, "the object would not be of aot type `%s`", l->name);
        return -1;
    }
    JS_FreeValue(ctx, old);
    return 0;
}

/* A field read of an object of an `aot type` from a generic access: its
 * value, or JS_UNINITIALIZED if not an own field. */
static JSValue lum_struct_get_atom(JSContext *ctx, LumStruct *s, JSAtom prop)
{
    int fi = lum_field_index(s->layout, prop);
    if (fi < 0 || !(s->mask & (1u << fi)))
        return JS_UNINITIALIZED;
    return lum_field_get(ctx, s, &lum_layouts[s->layout].fields[fi]);
}

static int lum_struct_get_own(JSContext *ctx, JSPropertyDescriptor *desc, JSValueConst obj, JSAtom prop)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    JSValue v = lum_struct_get_atom(ctx, s, prop);
    if (JS_IsUninitialized(v))
        return 0;
    if (desc) {
        desc->flags = JS_PROP_ENUMERABLE | JS_PROP_WRITABLE;
        if (lum_layouts[s->layout].optional & (1u << lum_field_index(s->layout, prop)))
            desc->flags |= JS_PROP_CONFIGURABLE;
        desc->value = v;
        desc->getter = JS_UNDEFINED;
        desc->setter = JS_UNDEFINED;
    } else {
        JS_FreeValue(ctx, v);
    }
    return 1;
}

static int lum_struct_own_names(JSContext *ctx, JSPropertyEnum **ptab, uint32_t *plen, JSValueConst obj)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    uint32_t i, n = 0;
    JSPropertyEnum *tab = js_malloc(ctx, sizeof(JSPropertyEnum) * (l->nfields ? l->nfields : 1));
    if (!tab)
        return -1;
    for (i = 0; i < l->nfields; i++) {
        if (s->mask & (1u << i)) {
            tab[n].atom = JS_DupAtom(ctx, lum_struct_atoms[lum_struct_atom_base[s->layout] + i]);
            tab[n].is_enumerable = true;
            n++;
        }
    }
    *ptab = tab;
    *plen = n;
    return 0;
}

/* `delete`: of an optional field, if the object still matches a member. */
static int lum_struct_delete(JSContext *ctx, JSValueConst obj, JSAtom prop)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    int fi = lum_field_index(s->layout, prop);
    uint32_t bit, mask = s->mask;
    uint64_t saved[2];
    JSValue old;
    if (fi < 0 || !(mask & (bit = 1u << fi)))
        return true;
    if (!(l->optional & bit))
        return false;
    memcpy(saved, lum_field_at(s, &l->fields[fi]), lum_field_size(&l->fields[fi]));
    lum_put_raw(ctx, obj, s, fi, JS_UNDEFINED, &old);
    if (!lum_struct_matches(ctx, s)) {
        memcpy(lum_field_at(s, &l->fields[fi]), saved, lum_field_size(&l->fields[fi]));
        s->mask = mask;
        return false;
    }
    JS_FreeValue(ctx, old);
    return true;
}

static int lum_struct_define(JSContext *ctx, JSValueConst obj, JSAtom prop, JSValueConst val,
                               JSValueConst getter, JSValueConst setter, int flags)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    int fi = lum_field_index(s->layout, prop);
    if (fi < 0 || !((s->mask | l->optional) & (1u << fi))) {
        JS_ThrowTypeError(ctx, "cannot add a property to an object of aot type `%s`", l->name);
        return -1;
    }
    /* (An optional field can be deleted: it is configurable.) */
    if ((flags & (JS_PROP_HAS_GET | JS_PROP_HAS_SET)) ||
        ((flags & JS_PROP_HAS_CONFIGURABLE) && !(flags & JS_PROP_CONFIGURABLE) != !(l->optional & (1u << fi))) ||
        ((flags & JS_PROP_HAS_ENUMERABLE) && !(flags & JS_PROP_ENUMERABLE)) ||
        ((flags & JS_PROP_HAS_WRITABLE) && !(flags & JS_PROP_WRITABLE))) {
        JS_ThrowTypeError(ctx, "cannot redefine `%s` of an object of aot type `%s`", l->fields[fi].name, l->name);
        return -1;
    }
    if (!(flags & JS_PROP_HAS_VALUE))
        return true;
    if (lum_struct_set(ctx, obj, fi, JS_DupValue(ctx, val)) < 0)
        return -1;
    return true;
}

static void lum_struct_finalizer(JSRuntime *rt, JSValueConst val)
{
    LumStruct *s = JS_GetOpaque(val, lum_struct_class);
    const LumLayoutDesc *l;
    uint32_t i;
    if (!s)
        return;
    l = &lum_layouts[s->layout];
    for (i = 0; i < l->nfields; i++) {
        if (l->fields[i].storage == LUM_ST_VALUE)
            JS_FreeValueRT(rt, *(JSValue *)lum_field_at(s, &l->fields[i]));
    }
    /* The data is part of the object's allocation. */
}

static void lum_struct_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    LumStruct *s = JS_GetOpaque(val, lum_struct_class);
    const LumLayoutDesc *l;
    uint32_t i;
    if (!s)
        return;
    l = &lum_layouts[s->layout];
    for (i = 0; i < l->nfields; i++) {
        if (l->fields[i].storage == LUM_ST_VALUE)
            JS_MarkValue(rt, *(JSValue *)lum_field_at(s, &l->fields[i]), mark_func);
    }
}

static const JSClassExoticMethods lum_struct_exotic = {
    .get_own_property = lum_struct_get_own,
    .get_own_property_names = lum_struct_own_names,
    .delete_property = lum_struct_delete,
    .define_own_property = lum_struct_define,
};

/* JSON.stringify of an object of an `aot type` (see JS_SetJSONWriter):
 * its fields as the general algorithm would write them (in layout order,
 * as Object.keys gives them), read from the struct. */
static int lum_json_key(JSContext *ctx, JSJSONWriter *w, uint32_t k, bool first)
{
    const LumJSONKey *key = &lum_struct_json_keys[k];
    JSValue name;
    int r;
    if (key->text)
        return JS_JSONWrite8(w, key->text + first, key->len - first);
    if (!first && JS_JSONWrite8(w, ",", 1) < 0)
        return -1;
    name = JS_AtomToString(ctx, lum_struct_atoms[k]);
    if (JS_IsException(name))
        return -1;
    r = JS_JSONWriteValue(ctx, w, name, 0);
    JS_FreeValue(ctx, name);
    if (r <= 0)
        return -1;
    return JS_JSONWrite8(w, ":", 1);
}

int lum_struct_json_from(JSContext *ctx, JSJSONWriter *w, const LumStruct *s, int depth, uint32_t from, int first)
{
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    uint32_t base = lum_struct_atom_base[s->layout], i;
    int r;
    if (from == 0 && JS_JSONWrite8(w, "{", 1) < 0)
        return -1;
    for (i = from; i < l->nfields; i++) {
        const LumFieldDesc *f = &l->fields[i];
        void *at = lum_field_at((LumStruct *)s, f);
        if (!(s->mask & (1u << i)))
            continue;
        if (f->storage == LUM_ST_VALUE && !lum_json_present(ctx, *(JSValue *)at))
            continue;
        if (lum_json_key(ctx, w, base + i, first) < 0)
            return -1;
        first = false;
        switch (f->storage) {
        case LUM_ST_INT32:
            r = JS_JSONWriteInt(w, *(int32_t *)at);
            break;
        case LUM_ST_F64:
            r = JS_JSONWriteNumber(ctx, w, *(double *)at);
            break;
        case LUM_ST_BOOL:
            r = *(uint8_t *)at ? JS_JSONWrite8(w, "true", 4) : JS_JSONWrite8(w, "false", 5);
            break;
        default:
            r = JS_JSONWriteValue(ctx, w, *(JSValue *)at, depth + 1);
            if (r <= 0)
                return r;
            continue;
        }
        if (r < 0)
            return -1;
    }
    return JS_JSONWrite8(w, "}", 1) < 0 ? -1 : 1;
}

/* The layout's generated writer, else the general way. */
static int lum_struct_json(JSContext *ctx, JSJSONWriter *w, JSValueConst obj, int depth)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    JSJSONRecordFunc *fn = lum_layout_json ? lum_layout_json[s->layout] : NULL;
    if (fn)
        return fn(ctx, w, s, depth);
    return lum_struct_json_from(ctx, w, s, depth, 0, true);
}

/* JSON.stringify(v) by the original builtin, where v is statically an
 * object of an `aot type` (or undefined): written from its fields
 * directly, else as JS_JSONStringifyData. */
JSValue lum_struct_stringify(JSContext *ctx, JSValueConst v)
{
    LumStruct *s = JS_GetOpaque(v, lum_struct_class);
    JSJSONRecordFunc *fn = s && lum_layout_json ? lum_layout_json[s->layout] : NULL;
    if (fn)
        return JS_JSONStringifyWith(ctx, v, fn, s);
    return JS_JSONStringifyData(ctx, v);
}

/* The JSON keys of the layouts' fields. */
static int lum_struct_json_init(JSContext *ctx, int total)
{
    int n = lum_layout_count(), i;
    uint32_t f;
    lum_struct_json_keys = js_mallocz(ctx, sizeof(LumJSONKey) * (total ? total : 1));
    if (!lum_struct_json_keys)
        return -1;
    for (i = 0; i < n; i++) {
        for (f = 0; f < lum_layouts[i].nfields; f++) {
            const char *name = lum_layouts[i].fields[f].name;
            size_t len = strlen(name), j;
            LumJSONKey *key = &lum_struct_json_keys[lum_struct_atom_base[i] + f];
            for (j = 0; j < len; j++) {
                unsigned char c = name[j];
                if (c < 0x20 || c >= 0x7f || c == '"' || c == '\\')
                    break;
            }
            if (j < len)
                continue;
            /* (Zeros after it: typed parsing reads it 8 bytes at a time.) */
            key->text = js_mallocz(ctx, len + 4 + 8);
            if (!key->text)
                return -1;
            key->text[0] = ',';
            key->text[1] = '"';
            memcpy(key->text + 2, name, len);
            key->text[len + 2] = '"';
            key->text[len + 3] = ':';
            key->len = len + 4;
        }
    }
    return JS_SetJSONWriter(lum_struct_class, lum_struct_json);
}

/* __lum_struct(k, obj): an object of layout k with the own enumerable
 * properties of obj (a fresh object literal), checked. */
JSValue lum_struct_from(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    int32_t k;
    JSPropertyEnum *tab;
    uint32_t len, i, mask = 0;
    JSValue obj;
    if (argc < 2 || JS_ToInt32(ctx, &k, argv[0]) < 0 || k < 0 || k >= lum_layout_count())
        return JS_ThrowTypeError(ctx, "__lum_struct: bad layout");
    if (JS_GetOwnPropertyNames(ctx, &tab, &len, argv[1], JS_GPN_STRING_MASK | JS_GPN_ENUM_ONLY) < 0)
        return JS_EXCEPTION;
    for (i = 0; i < len; i++) {
        int fi = lum_field_index(k, tab[i].atom);
        if (fi < 0) {
            JS_FreePropertyEnum(ctx, tab, len);
            return JS_ThrowTypeError(ctx, "not an object of aot type `%s`", lum_layouts[k].name);
        }
        mask |= 1u << fi;
    }
    obj = lum_struct_new(ctx, k, mask);
    for (i = 0; i < len && !JS_IsException(obj); i++) {
        JSValue v = JS_GetProperty(ctx, argv[1], tab[i].atom);
        if (JS_IsException(v) ||
            lum_struct_put(ctx, obj, lum_field_index(k, tab[i].atom), v) < 0) {
            JS_FreeValue(ctx, obj);
            obj = JS_EXCEPTION;
        }
    }
    JS_FreePropertyEnum(ctx, tab, len);
    if (!JS_IsException(obj) && lum_struct_validate(ctx, obj) < 0) {
        JS_FreeValue(ctx, obj);
        return JS_EXCEPTION;
    }
    return obj;
}

/* The class, field atoms and literal strings (when the runtime starts). */
static int lum_structs_init(JSContext *ctx)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    JSClassDef def = { "Object", .finalizer = lum_struct_finalizer, .gc_mark = lum_struct_mark,
                       .exotic = (JSClassExoticMethods *)&lum_struct_exotic };
    int n = lum_layout_count(), i, total = 0;
    uint32_t f;
    JSValue proto;
    if (n == 0)
        return 0;
    JS_NewClassID(rt, &lum_struct_class);
    if (JS_NewClass(rt, lum_struct_class, &def) < 0)
        return -1;
    proto = JS_NewObject(ctx);
    if (JS_IsException(proto))
        return -1;
    JS_SetClassProto(ctx, lum_struct_class, JS_GetPrototype(ctx, proto));
    JS_FreeValue(ctx, proto);
    lum_struct_atom_base = js_mallocz(ctx, sizeof(uint32_t) * n);
    for (i = 0; i < n; i++) {
        lum_struct_atom_base[i] = total;
        total += lum_layouts[i].nfields;
    }
    lum_struct_atoms = js_mallocz(ctx, sizeof(JSAtom) * (total ? total : 1));
    for (i = 0; i < n; i++) {
        for (f = 0; f < lum_layouts[i].nfields; f++)
            lum_struct_atoms[lum_struct_atom_base[i] + f] = JS_NewAtom(ctx, lum_layouts[i].fields[f].name);
    }
    if (lum_struct_json_init(ctx, total) < 0 || lum_sarr_init(ctx) < 0 || lum_field_sub_init(ctx, total) < 0)
        return -1;
    lum_struct_lit_values = js_mallocz(ctx, sizeof(JSValue) * (lum_nstruct_lits ? lum_nstruct_lits : 1));
    for (i = 0; i < lum_nstruct_lits; i++)
        lum_struct_lit_values[i] = JS_NewString(ctx, lum_struct_lits[i]);
    return 0;
}

static void lum_structs_free(JSRuntime *rt)
{
    int n = lum_layout_count(), i, total = 0;
    if (!lum_struct_atoms)
        return;
    for (i = 0; i < n; i++)
        total += lum_layouts[i].nfields;
    for (i = 0; i < total; i++)
        JS_FreeAtomRT(rt, lum_struct_atoms[i]);
    for (i = 0; lum_struct_json_keys && i < total; i++)
        js_free_rt(rt, lum_struct_json_keys[i].text);
    js_free_rt(rt, lum_struct_json_keys);
    lum_struct_json_keys = NULL;
    js_free_rt(rt, lum_field_sub);
    lum_field_sub = NULL;
    js_free_rt(rt, lum_field_jp);
    lum_field_jp = NULL;
    JS_FreeValueRT(rt, lum_json_hook);
    lum_json_hook = JS_UNDEFINED;
    JS_FreeAtomRT(rt, lum_atom_json);
    lum_atom_json = JS_ATOM_NULL;
    for (i = 0; i < lum_nstruct_lits; i++)
        JS_FreeValueRT(rt, lum_struct_lit_values[i]);
    js_free_rt(rt, lum_struct_atoms);
    js_free_rt(rt, lum_struct_atom_base);
    js_free_rt(rt, lum_struct_lit_values);
    JS_FreeAtomRT(rt, lum_atom_length);
    lum_atom_length = JS_ATOM_NULL;
    lum_struct_atoms = NULL;
}

/* `v` (taken) as field `field` of layout `layout` would store it (see
 * StructCoerce): an int32 for an `int`, else v if of the field's type;
 * TypeError if not. */
JSValue lum_struct_coerce(JSContext *ctx, int layout, int field, JSValue v)
{
    const LumLayoutDesc *l = &lum_layouts[layout];
    if (l->fields[field].storage == LUM_ST_INT32) {
        int tag = JS_VALUE_GET_NORM_TAG(v);
        double d = tag == JS_TAG_INT ? JS_VALUE_GET_INT(v) : tag == JS_TAG_FLOAT64 ? JS_VALUE_GET_FLOAT64(v) : NAN;
        int32_t i;
        if (!(d == trunc(d)) || isinf(d) || JS_ToInt32(ctx, &i, v) < 0 ||
            (i != d && &lum_int_wrapping && !lum_int_wrapping)) {
            JS_FreeValue(ctx, v);
            lum_field_error(ctx, l, field);
            return JS_EXCEPTION;
        }
        return JS_NewInt32(ctx, i);
    }
    if (!lum_union_fits(ctx, l, field, v)) {
        JS_FreeValue(ctx, v);
        lum_field_error(ctx, l, field);
        return JS_EXCEPTION;
    }
    return v;
}

/* ---- arrays of `aot type` objects ----
 *
 * An array declared `T[]` (T an `aot type`) made by an array literal or
 * `new Array(n)` is an object of class lum_sarr_class: its elements are
 * objects of T's layout or `undefined` (holes), which native code relies
 * on. To JS it is an array (Array.isArray, JSON, Array.prototype
 * methods); storing anything else in it is a TypeError. */

uint32_t lum_sarr_class;

static LumStructArray *lum_sarr_of(JSValueConst v)
{
    return JS_GetOpaque(v, lum_sarr_class);
}

/* LumStructArray.flags (Object.freeze and Object.seal, of the whole
 * array): its elements read-only, not deletable; its length read-only. */
#define LUM_SARR_ELEMS_RO 1
#define LUM_SARR_SEALED 2
#define LUM_SARR_LENGTH_RO 4

static int lum_sarr_read_only(JSContext *ctx, uint32_t idx)
{
    JS_ThrowTypeError(ctx, "'%u' is read-only", idx);
    return -1;
}

static int lum_sarr_reserve(JSContext *ctx, LumStructArray *a, uint64_t n)
{
    uint32_t cap, i;
    JSValue *items;
    if (n <= a->cap)
        return 0;
    if (n > UINT32_MAX) {
        JS_ThrowRangeError(ctx, "invalid array length");
        return -1;
    }
    cap = a->cap * 2 > n ? a->cap * 2 : (uint32_t)n;
    if (cap < 4)
        cap = 4;
    items = js_realloc(ctx, a->items, sizeof(JSValue) * cap);
    if (!items)
        return -1;
    for (i = a->cap; i < cap; i++)
        items[i] = JS_UNDEFINED;
    a->items = items;
    a->cap = cap;
    return 0;
}

/* The new length (from JS): elements past it are dropped. */
static int lum_sarr_set_length(JSContext *ctx, LumStructArray *a, uint32_t len)
{
    uint32_t i;
    if (len != a->len && (a->flags & LUM_SARR_LENGTH_RO)) {
        JS_ThrowTypeError(ctx, "'length' is read-only");
        return -1;
    }
    if (len < a->len && (a->flags & LUM_SARR_SEALED)) {
        JS_ThrowTypeError(ctx, "could not delete property");
        return -1;
    }
    if (len > a->len && lum_sarr_reserve(ctx, a, len) < 0)
        return -1;
    for (i = len; i < a->len; i++) {
        JS_FreeValue(ctx, a->items[i]);
        a->items[i] = JS_UNDEFINED;
    }
    a->len = len;
    return 0;
}

static JSValue lum_sarr_new(JSContext *ctx, int layout, uint32_t len)
{
    void *data;
    JSValue obj = JS_NewObjectClassData(ctx, lum_sarr_class, sizeof(LumStructArray), true, &data);
    LumStructArray *a = data;
    if (JS_IsException(obj))
        return obj;
    a->layout = layout;
    a->len = a->cap = a->flags = 0;
    a->items = NULL;
    if (lum_sarr_set_length(ctx, a, len) < 0) {
        JS_FreeValue(ctx, obj);
        return JS_EXCEPTION;
    }
    return obj;
}

/* Store v (taken) at idx: an object of the layout, or undefined. */
int lum_sarr_set(JSContext *ctx, JSValue obj, int64_t idx, JSValue v)
{
    LumStructArray *a = lum_sarr_of(obj);
    LumStruct *s = lum_struct_of(v);
    if (!(JS_IsUndefined(v) || (s && s->layout == a->layout))) {
        JS_FreeValue(ctx, v);
        JS_ThrowTypeError(ctx, "an array of aot type `%s` cannot hold this value", lum_layouts[a->layout].name);
        return -1;
    }
    if (idx < 0 || idx >= UINT32_MAX) {
        JS_FreeValue(ctx, v);
        JS_ThrowRangeError(ctx, "invalid array index");
        return -1;
    }
    if (a->flags || (idx >= a->len && !JS_IsExtensible(ctx, obj))) {
        /* (Frozen or sealed, or not extensible: as JS would refuse it.) */
        if (idx < a->len ? (a->flags & LUM_SARR_ELEMS_RO) != 0 : 1) {
            JS_FreeValue(ctx, v);
            if (idx < a->len)
                return lum_sarr_read_only(ctx, idx);
            JS_ThrowTypeError(ctx, "object is not extensible");
            return -1;
        }
    }
    if (idx >= a->len) {
        if (lum_sarr_reserve(ctx, a, idx + 1) < 0) {
            JS_FreeValue(ctx, v);
            return -1;
        }
        a->len = idx + 1;
    }
    JS_FreeValue(ctx, a->items[idx]);
    a->items[idx] = v;
    if (JS_VALUE_HAS_REF_COUNT(v))
        JS_GCWriteBarrier(JS_GetRuntime(ctx), JS_VALUE_GET_PTR(obj));
    return 0;
}

/* `a.push(v)` (v taken): the new length, or -1. */
int64_t lum_sarr_push(JSContext *ctx, JSValue obj, JSValue v)
{
    LumStructArray *a = lum_sarr_of(obj);
    if (lum_sarr_set(ctx, obj, a->len, v) < 0)
        return -1;
    return a->len;
}

/* An array of layout `layout` with the elements of src (taken; an array
 * literal or `new Array(n)`, as both halves make them). */
JSValue lum_sarr_from(JSContext *ctx, int layout, JSValue src)
{
    int64_t len, i;
    JSValue obj;
    LumStructArray *a = lum_sarr_of(src);
    if (a && a->layout == (uint32_t)layout)
        return src;
    if (JS_GetLength(ctx, src, &len) < 0) {
        JS_FreeValue(ctx, src);
        return JS_EXCEPTION;
    }
    obj = lum_sarr_new(ctx, layout, len > UINT32_MAX ? UINT32_MAX : (uint32_t)len);
    {
        /* A fast array (what literals and `new Array(n)` make): its stored
           elements, then holes. */
        JSValue *values;
        uint32_t count;
        if (!JS_IsException(obj) && JS_GetFastArrayValues(ctx, src, &values, &count) && count <= len) {
            for (i = 0; i < count && !JS_IsException(obj); i++) {
                if (lum_sarr_set(ctx, obj, i, JS_DupValue(ctx, values[i])) < 0) {
                    JS_FreeValue(ctx, obj);
                    obj = JS_EXCEPTION;
                }
            }
            JS_FreeValue(ctx, src);
            return obj;
        }
    }
    for (i = 0; i < len && !JS_IsException(obj); i++) {
        JSValue v = JS_GetPropertyInt64(ctx, src, i);
        if (JS_IsException(v) || lum_sarr_set(ctx, obj, i, v) < 0) {
            JS_FreeValue(ctx, obj);
            obj = JS_EXCEPTION;
        }
    }
    JS_FreeValue(ctx, src);
    return obj;
}

/* __lum_sarr(k, array) */
JSValue lum_sarr_js(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    int32_t k;
    if (argc < 2 || JS_ToInt32(ctx, &k, argv[0]) < 0 || k < 0 || k >= lum_layout_count())
        return JS_ThrowTypeError(ctx, "__lum_sarr: bad layout");
    return lum_sarr_from(ctx, k, JS_DupValue(ctx, argv[1]));
}

static int lum_sarr_get_own(JSContext *ctx, JSPropertyDescriptor *desc, JSValueConst obj, JSAtom prop)
{
    LumStructArray *a = lum_sarr_of(obj);
    uint32_t idx;
    if (prop == lum_atom_length) {
        if (desc) {
            desc->flags = a->flags & LUM_SARR_LENGTH_RO ? 0 : JS_PROP_WRITABLE;
            desc->value = JS_NewUint32(ctx, a->len);
            desc->getter = desc->setter = JS_UNDEFINED;
        }
        return 1;
    }
    if (!JS_AtomToArrayIndex(ctx, &idx, prop) || idx >= a->len || JS_IsUndefined(a->items[idx]))
        return 0;
    if (desc) {
        desc->flags = JS_PROP_ENUMERABLE | (a->flags & LUM_SARR_ELEMS_RO ? 0 : JS_PROP_WRITABLE) |
            (a->flags & LUM_SARR_SEALED ? 0 : JS_PROP_CONFIGURABLE);
        desc->value = JS_DupValue(ctx, a->items[idx]);
        desc->getter = desc->setter = JS_UNDEFINED;
    }
    return 1;
}

static int lum_sarr_own_names(JSContext *ctx, JSPropertyEnum **ptab, uint32_t *plen, JSValueConst obj)
{
    LumStructArray *a = lum_sarr_of(obj);
    uint32_t i, n = 0;
    JSPropertyEnum *tab = js_malloc(ctx, sizeof(JSPropertyEnum) * ((size_t)a->len + 1));
    if (!tab)
        return -1;
    for (i = 0; i < a->len; i++) {
        if (!JS_IsUndefined(a->items[i])) {
            tab[n].atom = JS_NewAtomUInt32(ctx, i);
            tab[n].is_enumerable = true;
            n++;
        }
    }
    tab[n].atom = JS_DupAtom(ctx, lum_atom_length);
    tab[n].is_enumerable = false;
    *ptab = tab;
    *plen = n + 1;
    return 0;
}

static int lum_sarr_delete(JSContext *ctx, JSValueConst obj, JSAtom prop)
{
    LumStructArray *a = lum_sarr_of(obj);
    uint32_t idx;
    if (prop == lum_atom_length)
        return false;
    if (JS_AtomToArrayIndex(ctx, &idx, prop) && idx < a->len) {
        if ((a->flags & LUM_SARR_SEALED) && !JS_IsUndefined(a->items[idx]))
            return false;
        JS_FreeValue(ctx, a->items[idx]);
        a->items[idx] = JS_UNDEFINED;
    }
    return true;
}

static int lum_sarr_define(JSContext *ctx, JSValueConst obj, JSAtom prop, JSValueConst val,
                             JSValueConst getter, JSValueConst setter, int flags)
{
    LumStructArray *a = lum_sarr_of(obj);
    const char *name = lum_layouts[a->layout].name;
    uint32_t idx;
    if (prop == lum_atom_length) {
        double d;
        if ((flags & (JS_PROP_HAS_GET | JS_PROP_HAS_SET)) ||
            ((flags & JS_PROP_HAS_CONFIGURABLE) && (flags & JS_PROP_CONFIGURABLE)) ||
            ((flags & JS_PROP_HAS_WRITABLE) && (flags & JS_PROP_WRITABLE) && (a->flags & LUM_SARR_LENGTH_RO))) {
            JS_ThrowTypeError(ctx, "cannot redefine the length of an array of aot type `%s`", name);
            return -1;
        }
        if (flags & JS_PROP_HAS_VALUE) {
            if (JS_ToFloat64(ctx, &d, val) < 0)
                return -1;
            if (!(d >= 0 && d <= UINT32_MAX && d == trunc(d))) {
                JS_ThrowRangeError(ctx, "invalid array length");
                return -1;
            }
            if (lum_sarr_set_length(ctx, a, (uint32_t)d) < 0)
                return -1;
        }
        /* (Read-only: as Object.freeze makes it.) */
        if ((flags & JS_PROP_HAS_WRITABLE) && !(flags & JS_PROP_WRITABLE))
            a->flags |= LUM_SARR_LENGTH_RO;
        return true;
    }
    if (!JS_AtomToArrayIndex(ctx, &idx, prop)) {
        JS_ThrowTypeError(ctx, "cannot add a property to an array of aot type `%s`", name);
        return -1;
    }
    {
        /* An element made read-only or not configurable: only of the whole
         * array (Object.freeze, Object.seal: after preventExtensions); not
         * the other way back. */
        int ro = (flags & JS_PROP_HAS_WRITABLE) && !(flags & JS_PROP_WRITABLE);
        int sealed = (flags & JS_PROP_HAS_CONFIGURABLE) && !(flags & JS_PROP_CONFIGURABLE);
        if ((flags & (JS_PROP_HAS_GET | JS_PROP_HAS_SET)) ||
            ((flags & JS_PROP_HAS_ENUMERABLE) && !(flags & JS_PROP_ENUMERABLE)) ||
            ((ro || sealed) && (idx >= a->len || JS_IsExtensible(ctx, obj))) ||
            ((flags & JS_PROP_HAS_WRITABLE) && (flags & JS_PROP_WRITABLE) && (a->flags & LUM_SARR_ELEMS_RO)) ||
            ((flags & JS_PROP_HAS_CONFIGURABLE) && (flags & JS_PROP_CONFIGURABLE) && (a->flags & LUM_SARR_SEALED))) {
            JS_ThrowTypeError(ctx, "cannot redefine an element of an array of aot type `%s`", name);
            return -1;
        }
        if (flags & JS_PROP_HAS_VALUE) {
            if (idx >= a->len && !JS_IsExtensible(ctx, obj)) {
                JS_ThrowTypeError(ctx, "object is not extensible");
                return -1;
            }
            if ((a->flags & LUM_SARR_ELEMS_RO) && idx < a->len) {
                if (!JS_IsStrictEqual(ctx, val, a->items[idx]))
                    return lum_sarr_read_only(ctx, idx);
            } else if (lum_sarr_set(ctx, obj, idx, JS_DupValue(ctx, val)) < 0) {
                return -1;
            }
        }
        if (ro)
            a->flags |= LUM_SARR_ELEMS_RO;
        if (sealed)
            a->flags |= LUM_SARR_SEALED;
        return true;
    }
}

static void lum_sarr_finalizer(JSRuntime *rt, JSValueConst val)
{
    LumStructArray *a = lum_sarr_of(val);
    uint32_t i;
    if (!a)
        return;
    for (i = 0; i < a->len; i++)
        JS_FreeValueRT(rt, a->items[i]);
    js_free_rt(rt, a->items);
}

static void lum_sarr_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    LumStructArray *a = lum_sarr_of(val);
    uint32_t i;
    if (!a)
        return;
    for (i = 0; i < a->len; i++)
        JS_MarkValue(rt, a->items[i], mark_func);
}

/* JSON.stringify of an array of an `aot type`: its elements (objects of
 * the layout, written as such) and holes as null. */
static int lum_sarr_json(JSContext *ctx, JSJSONWriter *w, JSValueConst obj, int depth)
{
    LumStructArray *a = lum_sarr_of(obj);
    uint32_t i;
    int r;
    if (JS_JSONWrite8(w, "[", 1) < 0)
        return -1;
    for (i = 0; i < a->len; i++) {
        if (i > 0 && JS_JSONWrite8(w, ",", 1) < 0)
            return -1;
        if (JS_IsUndefined(a->items[i])) {
            if (JS_JSONWrite8(w, "null", 4) < 0)
                return -1;
            continue;
        }
        r = JS_JSONWriteValue(ctx, w, a->items[i], depth + 1);
        if (r <= 0)
            return r;
    }
    return JS_JSONWrite8(w, "]", 1) < 0 ? -1 : 1;
}

static const JSClassExoticMethods lum_sarr_exotic = {
    .get_own_property = lum_sarr_get_own,
    .get_own_property_names = lum_sarr_own_names,
    .delete_property = lum_sarr_delete,
    .define_own_property = lum_sarr_define,
};

static int lum_sarr_init(JSContext *ctx)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    JSClassDef def = { "Array", .finalizer = lum_sarr_finalizer, .gc_mark = lum_sarr_mark,
                       .exotic = (JSClassExoticMethods *)&lum_sarr_exotic };
    JSValue arr, proto;
    JS_NewClassID(rt, &lum_sarr_class);
    if (JS_NewClass(rt, lum_sarr_class, &def) < 0)
        return -1;
    arr = JS_NewArray(ctx);
    if (JS_IsException(arr))
        return -1;
    proto = JS_GetPrototype(ctx, arr);
    JS_FreeValue(ctx, arr);
    JS_SetClassProto(ctx, lum_sarr_class, proto);
    JS_SetArrayClass(lum_sarr_class);
    if (JS_SetJSONWriter(lum_sarr_class, lum_sarr_json) < 0)
        return -1;
    lum_atom_length = JS_NewAtom(ctx, "length");
    return 0;
}

/* A regular expression literal's new object. */
JSValue lum_regexp(JSContext *ctx, int pattern, int flags)
{
    JSValue bc = JS_UNDEFINED;
    if (!lum_regexp_bc) {
        lum_regexp_bc = js_mallocz(ctx, sizeof(JSValue) * lum_nstrings);
        lum_regexp_flags = js_mallocz(ctx, sizeof(int) * lum_nstrings);
        if (!lum_regexp_bc || !lum_regexp_flags)
            return JS_EXCEPTION;
        for (int i = 0; i < lum_nstrings; i++)
            lum_regexp_bc[i] = JS_UNDEFINED;
    }
    if (JS_IsUndefined(lum_regexp_bc[pattern])) {
        lum_regexp_flags[pattern] = flags;
        return JS_NewRegExpLiteral(ctx, lum_strings[pattern], lum_strings[flags], &lum_regexp_bc[pattern]);
    }
    if (lum_regexp_flags[pattern] == flags)
        return JS_NewRegExpLiteral(ctx, lum_strings[pattern], lum_strings[flags], &lum_regexp_bc[pattern]);
    return JS_NewRegExpLiteral(ctx, lum_strings[pattern], lum_strings[flags], &bc);
}

/* The string constants (atoms' strings), the `aot type` literal strings
   and the regular expression literals' bytecode. */
/* ---- `aot type`s from JSON: `JSON.parse(text)` and `await r.json()`
 * where an object of an `aot type` (or an array of them) is expected.
 * `desc` names what: layout * 2, + 1 for an array of its objects. ----
 *
 * The value is converted (lum_struct_convert): an object of the layout
 * from an object's own fields of the layout (others ignored; each checked
 * as a store would, then the object as a literal), an array of them from
 * an array, nested objects and arrays the same way. Typed parsing
 * (lum_jp_*) makes the same objects straight from the text, reading only
 * the fields of the layouts and skipping the rest; if it meets anything
 * else (bad JSON, a value a field cannot hold), the general way runs
 * instead, so that its errors are the same. */

static int lum_field_sub_init(JSContext *ctx, int total)
{
    int n = lum_layout_count(), i;
    uint32_t f, m;
    lum_field_sub = js_mallocz(ctx, sizeof(int32_t) * (total ? total : 1));
    lum_field_jp = js_mallocz(ctx, total ? total : 1);
    if (!lum_field_sub || !lum_field_jp)
        return -1;
    for (i = 0; i < n; i++) {
        const LumLayoutDesc *l = &lum_layouts[i];
        for (f = 0; f < l->nfields; f++) {
            int32_t sub = -1;
            uint8_t jpf = 0;
            for (m = 0; m < l->nmembers; m++) {
                const LumTypeDesc *t = &l->members[m].types[f];
                int32_t d;
                if (t->kind == LUM_FT_ABSENT)
                    continue;
                d = t->kind == LUM_FT_STRUCT ? t->layout * 2 + 1 : t->kind == LUM_FT_ARRAY ? t->layout * 2 + 2 : 0;
                sub = sub < 0 || sub == d ? d : 0;
                jpf |= (t->kind == LUM_FT_STRING ? LUM_JPF_STRING : 0) | (t->nullable ? LUM_JPF_NULL : 0);
            }
            lum_field_sub[lum_struct_atom_base[i] + f] = sub < 0 ? 0 : sub;
            /* (Stored as is in a JS-valued field only.) */
            lum_field_jp[lum_struct_atom_base[i] + f] = l->fields[f].storage == LUM_ST_VALUE ? jpf : 0;
        }
    }
    lum_atom_json = JS_NewAtom(ctx, "json");
    return 0;
}

static JSValue lum_convert(JSContext *ctx, JSValue v, int32_t desc, int depth)
{
    uint32_t k = desc >> 1, base, fi;
    const LumLayoutDesc *l = &lum_layouts[k];
    JSValue obj;
    LumStruct *s;
    if (depth > 1000) {
        JS_FreeValue(ctx, v);
        return JS_ThrowRangeError(ctx, "Maximum call stack size exceeded");
    }
    if (desc & 1) {
        LumStructArray *a = lum_sarr_of(v);
        int64_t len, i;
        if (a && a->layout == k)
            return v;
        if (!JS_IsArray(v)) {
            JS_FreeValue(ctx, v);
            return JS_ThrowTypeError(ctx, "not an array of aot type `%s`", l->name);
        }
        if (JS_GetLength(ctx, v, &len) < 0)
            goto fail;
        obj = lum_sarr_new(ctx, k, 0);
        if (JS_IsException(obj))
            goto fail;
        for (i = 0; i < len; i++) {
            JSValue el = JS_GetPropertyInt64(ctx, v, i);
            if (JS_IsException(el))
                goto fail_obj;
            el = lum_convert(ctx, el, k * 2, depth + 1);
            if (JS_IsException(el) || lum_sarr_push(ctx, obj, el) < 0)
                goto fail_obj;
        }
        JS_FreeValue(ctx, v);
        return obj;
    }
    s = lum_struct_of(v);
    if (s && s->layout == k)
        return v;
    if (!JS_IsObject(v)) {
        JS_FreeValue(ctx, v);
        return JS_ThrowTypeError(ctx, "not an object of aot type `%s`", l->name);
    }
    obj = lum_struct_alloc(ctx, k, 0, &s);
    if (JS_IsException(obj))
        goto fail;
    base = lum_struct_atom_base[k];
    for (fi = 0; fi < l->nfields; fi++) {
        JSAtom atom = lum_struct_atoms[base + fi];
        int32_t sub = lum_field_sub[base + fi];
        JSValue fv, old;
        int has = JS_GetOwnProperty(ctx, NULL, v, atom);
        if (has < 0)
            goto fail_obj;
        if (!has)
            continue;
        fv = JS_GetProperty(ctx, v, atom);
        if (JS_IsException(fv))
            goto fail_obj;
        if (sub && JS_IsObject(fv)) {
            fv = lum_convert(ctx, fv, sub - 1, depth + 1);
            if (JS_IsException(fv))
                goto fail_obj;
        }
        s->mask |= 1u << fi;
        if (lum_put_raw(ctx, obj, s, fi, fv, &old) < 0)
            goto fail_obj;
        JS_FreeValue(ctx, old);
    }
    JS_FreeValue(ctx, v);
    if (!lum_struct_complete(ctx, s)) {
        JS_FreeValue(ctx, obj);
        return JS_ThrowTypeError(ctx, "not an object of aot type `%s`", l->name);
    }
    return obj;
 fail_obj:
    JS_FreeValue(ctx, obj);
 fail:
    JS_FreeValue(ctx, v);
    return JS_EXCEPTION;
}

/* `v` (taken) as an object of layout desc >> 1 (an array of them if desc
 * is odd): itself if one, else converted; TypeError if it cannot be. */
JSValue lum_struct_convert(JSContext *ctx, JSValue v, int32_t desc)
{
    return lum_convert(ctx, v, desc, 0);
}

/* __lum_as(desc, v) (interpreted code). */
JSValue lum_struct_as(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    int32_t desc;
    if (argc < 2 || JS_ToInt32(ctx, &desc, argv[0]) < 0 || desc < 0 || (desc >> 1) >= lum_layout_count())
        return JS_ThrowTypeError(ctx, "__lum_as: bad layout");
    return lum_struct_convert(ctx, JS_DupValue(ctx, argv[1]), desc);
}

typedef struct LumJP {
    JSContext *ctx;
    const uint8_t *p, *end;
    bool utf8; /* else Latin-1 (a JS string's characters) */
    uint16_t *buf;
    size_t buf_len, buf_cap;
} LumJP;

static inline bool lum_jp_digit(uint8_t c)
{
    return c >= '0' && c <= '9';
}

static inline void lum_jp_ws(LumJP *jp)
{
    while (jp->p < jp->end && *jp->p <= ' ' &&
           (*jp->p == ' ' || *jp->p == '\n' || *jp->p == '\r' || *jp->p == '\t'))
        jp->p++;
}

/* Character c, after any whitespace: passed. */
static inline bool lum_jp_char(LumJP *jp, uint8_t c)
{
    if (jp->p < jp->end && *jp->p == c)
        return jp->p++, true;
    lum_jp_ws(jp);
    if (jp->p < jp->end && *jp->p == c)
        return jp->p++, true;
    return false;
}

#define LUM_JP_ONES 0x0101010101010101ull
#define LUM_JP_HIGHS 0x8080808080808080ull

/* In string characters, the 8 bytes at p: how many of them need no look
 * (not a quote, backslash, control character or non-ASCII byte). */
static inline int lum_jp_plain8(const uint8_t *p)
{
    uint64_t w, q, b, t;
    memcpy(&w, p, 8);
    q = w ^ ('"' * LUM_JP_ONES);
    b = w ^ ('\\' * LUM_JP_ONES);
    /* (Bit 7 of each such byte; exact up to the first.) */
    t = (((q - LUM_JP_ONES) & ~q) | ((b - LUM_JP_ONES) & ~b) | ((w - 0x20 * LUM_JP_ONES) & ~w) | w) & LUM_JP_HIGHS;
    if (!t)
        return 8;
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
    return __builtin_ctzll(t) >> 3;
#else
    return 0;
#endif
}

static bool lum_jp_lit(LumJP *jp, const char *lit, size_t n)
{
    if ((size_t)(jp->end - jp->p) < n || memcmp(jp->p, lit, n))
        return false;
    jp->p += n;
    return true;
}

/* A UTF-8 sequence at p (< end): its code point and length, or -1 if
 * not valid (overlong, a surrogate, past U+10FFFF, cut). */
static int lum_utf8_char(const uint8_t *p, const uint8_t *end, uint32_t *cp)
{
    uint32_t c = p[0], n, i;
    if (c < 0x80)
        return *cp = c, 1;
    if (c >= 0xc2 && c <= 0xdf)
        n = 2, c &= 0x1f;
    else if (c >= 0xe0 && c <= 0xef)
        n = 3, c &= 0x0f;
    else if (c >= 0xf0 && c <= 0xf4)
        n = 4, c &= 0x07;
    else
        return -1;
    if ((size_t)(end - p) < n)
        return -1;
    for (i = 1; i < n; i++) {
        if ((p[i] & 0xc0) != 0x80)
            return -1;
        c = (c << 6) | (p[i] & 0x3f);
    }
    if ((n == 3 && (c < 0x800 || (c >= 0xd800 && c <= 0xdfff))) || (n == 4 && (c < 0x10000 || c > 0x10ffff)))
        return -1;
    *cp = c;
    return n;
}

static int lum_jp_unit(LumJP *jp, uint32_t u)
{
    if (jp->buf_len == jp->buf_cap) {
        size_t cap = jp->buf_cap ? jp->buf_cap * 2 : 64;
        uint16_t *b = js_realloc(jp->ctx, jp->buf, cap * sizeof(uint16_t));
        if (!b)
            return -1;
        jp->buf = b;
        jp->buf_cap = cap;
    }
    jp->buf[jp->buf_len++] = u;
    return 0;
}

static int lum_jp_hex4(const uint8_t *p, const uint8_t *end, uint32_t *u)
{
    uint32_t v = 0, i;
    if (end - p < 4)
        return -1;
    for (i = 0; i < 4; i++) {
        uint8_t c = p[i];
        v <<= 4;
        if (c >= '0' && c <= '9')
            v |= c - '0';
        else if ((c | 0x20) >= 'a' && (c | 0x20) <= 'f')
            v |= (c | 0x20) - 'a' + 10;
        else
            return -1;
    }
    *u = v;
    return 0;
}

/* The string at jp->p (its opening quote): its value in *out. */
static int lum_jp_string(LumJP *jp, JSValue *out)
{
    const uint8_t *p = jp->p + 1, *start = p, *end = jp->end;
    bool ascii = true, narrow = true;
    size_t i;
    for (;;) {
        uint8_t c;
        if (end - p >= 8) {
            int n = lum_jp_plain8(p);
            p += n;
            if (n == 8)
                continue;
        }
        if (p >= end)
            return -1;
        c = *p;
        if (c == '"')
            break;
        if (c == '\\')
            goto escaped;
        if (c < 0x20)
            return -1;
        ascii &= c < 0x80;
        p++;
    }
    if (ascii || !jp->utf8) {
        *out = JS_NewStringLatin1(jp->ctx, start, p - start);
    } else {
        const uint8_t *q = start;
        uint32_t cp;
        while (q < p) {
            int n = lum_utf8_char(q, p, &cp);
            if (n < 0)
                return -1;
            q += n;
        }
        *out = JS_NewStringLen(jp->ctx, (const char *)start, p - start);
    }
    jp->p = p + 1;
    return JS_IsException(*out) ? -1 : 0;
 escaped:
    /* With escapes: as UTF-16 code units. */
    jp->buf_len = 0;
    for (p = start; p < end && *p != '"';) {
        uint32_t u = *p;
        if (u < 0x20)
            return -1;
        if (u == '\\') {
            if (++p >= end)
                return -1;
            switch (*p++) {
            case '"': u = '"'; break;
            case '\\': u = '\\'; break;
            case '/': u = '/'; break;
            case 'b': u = '\b'; break;
            case 'f': u = '\f'; break;
            case 'n': u = '\n'; break;
            case 'r': u = '\r'; break;
            case 't': u = '\t'; break;
            case 'u':
                if (lum_jp_hex4(p, end, &u) < 0)
                    return -1;
                p += 4;
                break;
            default:
                return -1;
            }
        } else if (u >= 0x80 && jp->utf8) {
            int n = lum_utf8_char(p, end, &u);
            if (n < 0)
                return -1;
            p += n;
            if (u > 0xffff) {
                u -= 0x10000;
                if (lum_jp_unit(jp, 0xd800 | (u >> 10)) < 0)
                    return -1;
                u = 0xdc00 | (u & 0x3ff);
            }
        } else {
            p++;
        }
        if (lum_jp_unit(jp, u) < 0)
            return -1;
    }
    if (p >= end)
        return -1;
    for (i = 0; i < jp->buf_len; i++)
        narrow &= jp->buf[i] < 0x100;
    if (narrow) {
        uint8_t small[256], *b = jp->buf_len <= sizeof(small) ? small : js_malloc(jp->ctx, jp->buf_len);
        if (!b)
            return -1;
        for (i = 0; i < jp->buf_len; i++)
            b[i] = jp->buf[i];
        *out = JS_NewStringLatin1(jp->ctx, b, jp->buf_len);
        if (b != small)
            js_free(jp->ctx, b);
    } else {
        *out = JS_NewStringUTF16(jp->ctx, jp->buf, jp->buf_len);
    }
    jp->p = p + 1;
    return JS_IsException(*out) ? -1 : 0;
}

static const double lum_pow10[] = {
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11,
    1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
};

/* The n characters at b (a JSON number) as a double. */
static __attribute__((noinline)) int lum_jp_strtod(JSContext *ctx, const uint8_t *b, size_t n, double *d)
{
    char small[64], *t = n < sizeof(small) ? small : js_malloc(ctx, n + 1);
    if (!t)
        return -1;
    memcpy(t, b, n);
    t[n] = '\0';
    *d = strtod(t, NULL);
    if (t != small)
        js_free(ctx, t);
    return 0;
}

/* The number at jp->p, as JSON.parse reads it. */
static int lum_jp_number(LumJP *jp, double *d)
{
    const uint8_t *p = jp->p, *start = p, *end = jp->end;
    bool neg = false, all = true;
    uint64_t m = 0; /* the digits (all of them if `all`), times 10^e */
    int nd = 0, e = 0;
    if (p < end && *p == '-')
        neg = true, p++;
    if (p >= end || !lum_jp_digit(*p))
        return -1;
    if (*p == '0' && p + 1 < end && lum_jp_digit(p[1]))
        return -1;
    for (; p < end && lum_jp_digit(*p); p++) {
        if (nd < 19)
            m = m * 10 + (*p - '0'), nd += m != 0;
        else
            all = false;
    }
    if (p < end && *p == '.') {
        if (++p >= end || !lum_jp_digit(*p))
            return -1;
        for (; p < end && lum_jp_digit(*p); p++) {
            if (nd < 19)
                m = m * 10 + (*p - '0'), nd += m != 0, e--;
            else
                all = false;
        }
    }
    if (p < end && (*p == 'e' || *p == 'E')) {
        bool eneg = false;
        int x = 0;
        p++;
        if (p < end && (*p == '+' || *p == '-'))
            eneg = *p++ == '-';
        if (p >= end || !lum_jp_digit(*p))
            return -1;
        for (; p < end && lum_jp_digit(*p); p++) {
            if (x < 100000)
                x = x * 10 + (*p - '0');
        }
        e += eneg ? -x : x;
    }
    if (all && (m == 0 || e == 0 || (nd <= 15 && e >= -22 && e <= 22))) {
        /* Exact: m (below 2^53 but with e 0, then rounded once), and
           10^|e|, so one rounding (Clinger's fast path). */
        double v = (double)m;
        if (m && e)
            v = e < 0 ? v / lum_pow10[-e] : v * lum_pow10[e];
        *d = neg ? -v : v;
    } else if (lum_jp_strtod(jp->ctx, start, p - start, d) < 0) {
        return -1;
    }
    jp->p = p;
    return 0;
}

/* Any value (checked, not made). */
static int lum_jp_skip(LumJP *jp, int depth)
{
    double d;
    if (depth > 1000 || jp->p >= jp->end)
        return -1;
    switch (*jp->p) {
    case '"': {
        const uint8_t *p = jp->p + 1;
        uint32_t u;
        for (;;) {
            if (jp->end - p >= 8) {
                int n = lum_jp_plain8(p);
                p += n;
                if (n == 8)
                    continue;
            }
            if (p >= jp->end || *p < 0x20)
                return -1;
            if (*p == '"')
                break;
            if (*p == '\\') {
                if (++p >= jp->end)
                    return -1;
                if (*p == 'u') {
                    if (lum_jp_hex4(p + 1, jp->end, &u) < 0)
                        return -1;
                    p += 5;
                } else if (strchr("\"\\/bfnrt", *p) && *p) {
                    p++;
                } else {
                    return -1;
                }
            } else if (*p >= 0x80 && jp->utf8) {
                int n = lum_utf8_char(p, jp->end, &u);
                if (n < 0)
                    return -1;
                p += n;
            } else {
                p++;
            }
        }
        jp->p = p + 1;
        return 0;
    }
    case '{':
        jp->p++;
        lum_jp_ws(jp);
        if (jp->p < jp->end && *jp->p == '}')
            return jp->p++, 0;
        for (;;) {
            if (jp->p >= jp->end || *jp->p != '"' || lum_jp_skip(jp, depth + 1) < 0)
                return -1;
            lum_jp_ws(jp);
            if (jp->p >= jp->end || *jp->p++ != ':')
                return -1;
            lum_jp_ws(jp);
            if (lum_jp_skip(jp, depth + 1) < 0)
                return -1;
            lum_jp_ws(jp);
            if (jp->p >= jp->end)
                return -1;
            if (*jp->p == '}')
                return jp->p++, 0;
            if (*jp->p++ != ',')
                return -1;
            lum_jp_ws(jp);
        }
    case '[':
        jp->p++;
        lum_jp_ws(jp);
        if (jp->p < jp->end && *jp->p == ']')
            return jp->p++, 0;
        for (;;) {
            if (lum_jp_skip(jp, depth + 1) < 0)
                return -1;
            lum_jp_ws(jp);
            if (jp->p >= jp->end)
                return -1;
            if (*jp->p == ']')
                return jp->p++, 0;
            if (*jp->p++ != ',')
                return -1;
            lum_jp_ws(jp);
        }
    case 't':
        return lum_jp_lit(jp, "true", 4) ? 0 : -1;
    case 'f':
        return lum_jp_lit(jp, "false", 5) ? 0 : -1;
    case 'n':
        return lum_jp_lit(jp, "null", 4) ? 0 : -1;
    default:
        return lum_jp_number(jp, &d);
    }
}

static int lum_jp_object(LumJP *jp, uint32_t k, JSValue *out, int depth);
static int lum_jp_array(LumJP *jp, uint32_t k, JSValue *out, int depth);

/* Whether the n bytes at a (before end) are those at b (a key's text:
 * readable 8 bytes past them). */
static inline bool lum_jp_eq(const uint8_t *a, const uint8_t *end, const char *b, size_t n)
{
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
    if ((size_t)(end - a) >= ((n + 7) & ~(size_t)7)) {
        uint64_t x, y;
        for (; n >= 8; a += 8, b += 8, n -= 8) {
            memcpy(&x, a, 8);
            memcpy(&y, b, 8);
            if (x != y)
                return false;
        }
        if (!n)
            return true;
        memcpy(&x, a, 8);
        memcpy(&y, b, 8);
        return ((x ^ y) & ((1ull << (n * 8)) - 1)) == 0;
    }
#endif
    if ((size_t)(end - a) < n)
        return false;
    while (n--) {
        if (*a++ != (uint8_t)*b++)
            return false;
    }
    return true;
}

/* The key at jp->p: field *fi of layout k, or -1 if none. Field `next`
 * (after the one before, as in the text JSON.stringify writes) is tried
 * first. */
static int lum_jp_key(LumJP *jp, uint32_t k, uint32_t next, int *fi)
{
    const LumLayoutDesc *l = &lum_layouts[k];
    uint32_t base = lum_struct_atom_base[k], i;
    const uint8_t *p = jp->p + 1, *start = p;
    JSValue key;
    JSAtom atom;
    if (next < l->nfields) {
        const LumJSONKey *jk = &lum_struct_json_keys[base + next];
        /* (`"name"`: the key has no escapes then.) */
        if (jk->text && lum_jp_eq(jp->p, jp->end, jk->text + 1, jk->len - 2)) {
            *fi = next;
            jp->p += jk->len - 2;
            return 0;
        }
    }
    while (p < jp->end && *p != '"' && *p != '\\' && *p >= 0x20 && *p < 0x80)
        p++;
    if (p < jp->end && *p == '"') {
        size_t n = p - start;
        *fi = -1;
        for (i = 0; i < l->nfields; i++) {
            const LumJSONKey *jk = &lum_struct_json_keys[base + i];
            if (jk->text && jk->len - 4 == n && lum_jp_eq(start, jp->end, jk->text + 2, n)) {
                *fi = i;
                break;
            }
        }
        if (*fi < 0) {
            /* (A name that needs escapes in JSON.) */
            for (i = 0; i < l->nfields; i++) {
                if (!lum_struct_json_keys[base + i].text && strlen(l->fields[i].name) == n &&
                    !memcmp(l->fields[i].name, start, n)) {
                    *fi = i;
                    break;
                }
            }
        }
        jp->p = p + 1;
        return 0;
    }
    if (lum_jp_string(jp, &key) < 0)
        return -1;
    atom = JS_ValueToAtom(jp->ctx, key);
    JS_FreeValue(jp->ctx, key);
    if (atom == JS_ATOM_NULL)
        return -1;
    *fi = lum_field_index(k, atom);
    JS_FreeAtom(jp->ctx, atom);
    return 0;
}

/* The value at jp->p for field fi of obj (s its data). */
static int lum_jp_field(LumJP *jp, JSValue obj, LumStruct *s, int fi, int depth)
{
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    const LumFieldDesc *f = &l->fields[fi];
    uint32_t at = lum_struct_atom_base[s->layout] + fi;
    int32_t sub = lum_field_sub[at];
    uint32_t bit = 1u << fi;
    JSValue v, old;
    double d;
    switch (*jp->p) {
    case '{':
        if (!sub || ((sub - 1) & 1) || lum_jp_object(jp, (sub - 1) >> 1, &v, depth + 1) < 0)
            return -1;
        /* (Every member that has it holds these.) */
        goto store;
    case '[':
        if (!sub || !((sub - 1) & 1) || lum_jp_array(jp, (sub - 1) >> 1, &v, depth + 1) < 0)
            return -1;
        goto store;
    case '"':
        if (lum_jp_string(jp, &v) < 0)
            return -1;
        if (lum_field_jp[at] & LUM_JPF_STRING)
            goto store;
        break;
    case 't':
    case 'f':
        if (lum_jp_lit(jp, "true", 4))
            v = JS_TRUE;
        else if (lum_jp_lit(jp, "false", 5))
            v = JS_FALSE;
        else
            return -1;
        if (f->storage == LUM_ST_BOOL) {
            /* (Every member that has it holds a boolean.) */
            *(uint8_t *)lum_field_at(s, f) = JS_VALUE_GET_BOOL(v);
            s->mask |= bit;
            return 0;
        }
        break;
    case 'n':
        if (!lum_jp_lit(jp, "null", 4))
            return -1;
        v = JS_NULL;
        if (lum_field_jp[at] & LUM_JPF_NULL)
            goto store;
        break;
    default:
        if (lum_jp_number(jp, &d) < 0)
            return -1;
        if (f->storage == LUM_ST_F64) {
            /* (Every member that has it holds a number.) */
            *(double *)lum_field_at(s, f) = d;
            s->mask |= bit;
            return 0;
        }
        if (f->storage == LUM_ST_INT32 && d == (double)(int32_t)d && !(d == 0 && signbit(d))) {
            *(int32_t *)lum_field_at(s, f) = (int32_t)d;
            s->mask |= bit;
            return 0;
        }
        v = JS_NewFloat64(jp->ctx, d);
        break;
    }
    s->mask |= bit;
    if (lum_put_raw(jp->ctx, obj, s, fi, v, &old) < 0)
        return -1;
    JS_FreeValue(jp->ctx, old);
    return 0;
 store:
    /* (A value some member's field holds: as lum_put_raw would store it.) */
    s->mask |= bit;
    old = *(JSValue *)lum_field_at(s, f);
    *(JSValue *)lum_field_at(s, f) = v;
    if (JS_VALUE_HAS_REF_COUNT(v))
        JS_GCWriteBarrier(JS_GetRuntime(jp->ctx), JS_VALUE_GET_PTR(obj));
    JS_FreeValue(jp->ctx, old);
    return 0;
}

/* The object at jp->p, as an object of layout k. */
static int lum_jp_object(LumJP *jp, uint32_t k, JSValue *out, int depth)
{
    JSValue obj;
    LumStruct *s;
    if (depth > 1000 || jp->p >= jp->end || *jp->p != '{')
        return -1;
    obj = lum_struct_alloc(jp->ctx, k, 0, &s);
    if (JS_IsException(obj))
        return -1;
    jp->p++;
    lum_jp_ws(jp);
    if (jp->p < jp->end && *jp->p == '}') {
        jp->p++;
    } else {
        uint32_t next = 0;
        for (;;) {
            int fi;
            if (jp->p >= jp->end || *jp->p != '"' || lum_jp_key(jp, k, next, &fi) < 0)
                goto fail;
            if (fi >= 0)
                next = fi + 1;
            if (!lum_jp_char(jp, ':'))
                goto fail;
            lum_jp_ws(jp);
            if (fi < 0 ? lum_jp_skip(jp, depth + 1) < 0 : lum_jp_field(jp, obj, s, fi, depth) < 0)
                goto fail;
            if (lum_jp_char(jp, ','))
                lum_jp_ws(jp);
            else if (lum_jp_char(jp, '}'))
                break;
            else
                goto fail;
        }
    }
    if (!lum_struct_complete(jp->ctx, s))
        goto fail;
    *out = obj;
    return 0;
 fail:
    JS_FreeValue(jp->ctx, obj);
    return -1;
}

/* The array at jp->p, as an array of objects of layout k. */
static int lum_jp_array(LumJP *jp, uint32_t k, JSValue *out, int depth)
{
    JSValue arr, el;
    if (depth > 1000 || jp->p >= jp->end || *jp->p != '[')
        return -1;
    arr = lum_sarr_new(jp->ctx, k, 0);
    if (JS_IsException(arr))
        return -1;
    jp->p++;
    lum_jp_ws(jp);
    if (jp->p < jp->end && *jp->p == ']') {
        jp->p++;
        *out = arr;
        return 0;
    }
    for (;;) {
        if (lum_jp_object(jp, k, &el, depth + 1) < 0 || lum_sarr_push(jp->ctx, arr, el) < 0)
            goto fail;
        lum_jp_ws(jp);
        if (jp->p >= jp->end)
            goto fail;
        if (*jp->p == ']') {
            jp->p++;
            break;
        }
        if (*jp->p++ != ',')
            goto fail;
        lum_jp_ws(jp);
    }
    *out = arr;
    return 0;
 fail:
    JS_FreeValue(jp->ctx, arr);
    return -1;
}

/* Typed parsing of buf: the value, or JS_UNINITIALIZED if it met anything
 * the general way must handle (no exception then). */
static JSValue lum_jp_run(JSContext *ctx, const uint8_t *buf, size_t len, bool utf8, int32_t desc)
{
    LumJP jp = { ctx, buf, buf + len, utf8, NULL, 0, 0 };
    JSValue v = JS_UNDEFINED;
    int r;
    lum_jp_ws(&jp);
    r = desc & 1 ? lum_jp_array(&jp, desc >> 1, &v, 0) : lum_jp_object(&jp, desc >> 1, &v, 0);
    lum_jp_ws(&jp);
    if (r == 0 && jp.p != jp.end) {
        JS_FreeValue(ctx, v);
        r = -1;
    }
    js_free(ctx, jp.buf);
    if (r < 0) {
        if (JS_HasException(ctx))
            JS_FreeValue(ctx, JS_GetException(ctx));
        return JS_UNINITIALIZED;
    }
    return v;
}

/* JSON.parse(text) (the original) as desc: typed parsing of its
 * characters, else the general way. */
JSValue lum_json_parse_as(JSContext *ctx, JSValueConst text, int32_t desc)
{
    size_t len;
    const uint8_t *b = JS_GetStringLatin1(text, &len);
    JSValue v;
    if (b) {
        v = lum_jp_run(ctx, b, len, false, desc);
        if (!JS_IsUninitialized(v))
            return v;
    }
    v = JS_Call(ctx, lum_pins[LUM_PIN_JSON_PARSE], JS_UNDEFINED, 1, &text);
    if (JS_IsException(v))
        return v;
    return lum_struct_convert(ctx, v, desc);
}

/* Typed parsing of UTF-8 JSON (a body): the value, or JS_UNINITIALIZED
 * (then the host parses it the general way and converts it). */
JSValue lum_json_parse_utf8_as(JSContext *ctx, const uint8_t *buf, size_t len, int32_t desc)
{
    return lum_jp_run(ctx, buf, len, true, desc);
}

/* `r.json()` whose result is converted to desc: the host's typed parsing
 * (lum_set_json_hook) if it takes r, else r.json(). */
JSValue lum_json_method_as(JSContext *ctx, JSValueConst recv, int32_t desc)
{
    if (!JS_IsUndefined(lum_json_hook)) {
        JSValueConst args[2] = { recv, JS_NewInt32(ctx, desc) };
        JSValue r = JS_Call(ctx, lum_json_hook, JS_UNDEFINED, 2, args);
        if (!JS_IsUndefined(r))
            return r;
    }
    return JS_Invoke(ctx, recv, lum_atom_json, 0, NULL);
}

/* Object.create(proto) at a site (f: the method read from this_obj): while
 * f is the builtin, a new object of prototype proto (null or an object),
 * whose empty shape the site caches in `ic` for native code to make the
 * next ones inline with (and room for properties: LUM_OBJECT_ROOM); else
 * the call. */
JSValue lum_object_create(JSContext *ctx, LumIC *ic, JSValueConst f, JSValueConst this_obj, JSValueConst proto)
{
    JSValue obj;
    void *shape;
    if (JS_VALUE_GET_TAG(f) == JS_TAG_OBJECT &&
        JS_VALUE_GET_PTR(f) == JS_VALUE_GET_PTR(lum_pins[LUM_PIN_OBJECT_CREATE]) &&
        (JS_IsNull(proto) || JS_IsObject(proto))) {
        obj = JS_NewObjectProto(ctx, proto);
        if (!JS_IsException(obj) && JS_CachePlainShape(ctx, obj, 0, &shape) == 0) {
            lum_ic_store(ctx, ic, shape, NULL, NULL, 0, LUM_IC_OWN);
            ic->slot = JS_ShapeAllocInfoRoom(JS_GetRuntime(ctx), shape, LUM_OBJECT_ROOM);
        }
        return obj;
    }
    /* (Held while it runs: it may replace itself.) */
    f = JS_DupValue(ctx, f);
    obj = JS_Call(ctx, f, this_obj, 1, &proto);
    JS_FreeValue(ctx, f);
    return obj;
}

/* Whether `o.prop = v` on an ordinary extensible object whose prototype
 * is Object.prototype, without `prop`, adds it as an own data property:
 * Object.prototype has no setter nor read-only property `prop` (see
 * ir::virtualize_objects). Object.prototype is watched: the answer is kept
 * in `ic->epoch` while js_proto_epoch is unchanged. */
int lum_plain_add_ok(JSContext *ctx, LumIC *ic, JSAtom prop)
{
    JSValue obj = JS_NewObject(ctx), proto;
    JSPropertyDescriptor d;
    int watched, r;
    if (JS_IsException(obj)) {
        JS_FreeValue(ctx, JS_GetException(ctx));
        return 0;
    }
    proto = JS_GetPrototype(ctx, obj);
    JS_FreeValue(ctx, obj);
    watched = JS_WatchPrototype(ctx, proto) == 0;
    r = JS_GetOwnProperty(ctx, &d, proto, prop);
    JS_FreeValue(ctx, proto);
    if (r < 0) {
        JS_FreeValue(ctx, JS_GetException(ctx));
        return 0;
    }
    if (r) {
        int ok = !(d.flags & JS_PROP_GETSET) && (d.flags & JS_PROP_WRITABLE);
        JS_FreeValue(ctx, d.value);
        JS_FreeValue(ctx, d.getter);
        JS_FreeValue(ctx, d.setter);
        if (!ok)
            return 0;
    }
    if (watched)
        ic->epoch = js_proto_epoch;
    return 1;
}

/* A new object without a prototype (see ir::virtualize_objects). */
JSValue lum_null_object(JSContext *ctx)
{
    return JS_NewObjectProto(ctx, JS_NULL);
}

/* `f.call(this_obj, v).length` with f read as `Object.keys`: counted
 * without the array while f is the builtin (an object of an `aot type`:
 * its fields; an ordinary object or a dense array: JS_CountOwnKeys). */
JSValue lum_keys_length(JSContext *ctx, JSValueConst f, JSValueConst this_obj, JSValueConst v)
{
    JSValue arr, len;
    if (JS_VALUE_GET_TAG(f) == JS_TAG_OBJECT &&
        JS_VALUE_GET_PTR(f) == JS_VALUE_GET_PTR(lum_pins[LUM_PIN_OBJECT_KEYS])) {
        LumStruct *s = lum_struct_of(v);
        int64_t n = s ? __builtin_popcount(s->mask) : JS_CountOwnKeys(ctx, v);
        if (n >= 0)
            return JS_NewInt64(ctx, n);
    }
    /* (Held while it runs: it may replace itself.) */
    f = JS_DupValue(ctx, f);
    arr = JS_Call(ctx, f, this_obj, 1, &v);
    JS_FreeValue(ctx, f);
    if (JS_IsException(arr))
        return arr;
    len = JS_GetProperty(ctx, arr, lum_atom_string_length);
    JS_FreeValue(ctx, arr);
    return len;
}

void lum_set_json_hook(JSContext *ctx, JSValueConst fn)
{
    JSValue old = lum_json_hook;
    lum_json_hook = JS_DupValue(ctx, fn);
    JS_FreeValue(ctx, old);
}


static void lum_mark_strings(JSRuntime *rt, JS_MarkFunc *mark_func)
{
    int i;
    for (i = 0; i < lum_nstrings; i++)
        JS_MarkValue(rt, lum_strings[i], mark_func);
    if (lum_regexp_bc) {
        for (i = 0; i < lum_nstrings; i++)
            JS_MarkValue(rt, lum_regexp_bc[i], mark_func);
    }
    if (lum_struct_lit_values) {
        for (i = 0; i < lum_nstruct_lits; i++)
            JS_MarkValue(rt, lum_struct_lit_values[i], mark_func);
    }
}
