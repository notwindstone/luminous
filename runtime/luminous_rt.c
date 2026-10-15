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

/* Array literal. Takes ownership of the elements. */
JSValue lum_new_array(JSContext *ctx, int count, JSValue *values)
{
    return JS_NewArrayFrom(ctx, count, values);
}

/* ---- boxes: variables captured by closures ---- */

static JSClassID lum_box_class;

typedef struct LumBox {
    JSValue value;
} LumBox;

static void lum_box_finalizer(JSRuntime *rt, JSValueConst val)
{
    LumBox *b = JS_GetOpaque(val, lum_box_class);
    if (b) {
        JS_FreeValueRT(rt, b->value);
        js_free_rt(rt, b);
    }
}

static void lum_box_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    LumBox *b = JS_GetOpaque(val, lum_box_class);
    /* (the box, a traced block: its value with it) */
    if (b)
        mark_func(rt, (JSGCObjectHeader *)b);
}

/* A new box holding an uninitialized binding; *slot receives the address
   of its value, which stays valid as long as the box object lives. */
JSValue lum_box_new(JSContext *ctx, JSValue **slot)
{
    /* (Traced: native code may hold only *slot.) */
    LumBox *b = JS_MallocTraced(ctx, sizeof(*b));
    JSValue obj;
    if (!b)
        return JS_EXCEPTION;
    b->value = JS_UNINITIALIZED;
    obj = JS_NewObjectClass(ctx, lum_box_class);
    if (JS_IsException(obj)) {
        js_free(ctx, b);
        return obj;
    }
    JS_SetOpaque(obj, b);
    *slot = &b->value;
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
   suspended, and its promise's resolving functions. */
typedef struct LumFrame {
    void *self;               /* the frame object (not a counted reference) */
    JSValue promise;          /* async: its promise, settled by the frame */
    /* async: the reactions to its awaits, created at the first one (an
       async function waits on one value at a time) */
    JSValue on_settled[2];
    int fn, nslots;
    int state;                /* where to resume */
    JSValue slots[];
} LumFrame;

static JSClassID lum_frame_class;

static LumFrame *lum_frame_of(JSValue *slots)
{
    return (LumFrame *)((char *)slots - offsetof(LumFrame, slots));
}

static void lum_frame_finalizer(JSRuntime *rt, JSValueConst val)
{
    LumFrame *f = JS_GetOpaque(val, lum_frame_class);
    int i;
    if (!f)
        return;
    JS_FreeValueRT(rt, f->promise);
    JS_FreeValueRT(rt, f->on_settled[0]);
    JS_FreeValueRT(rt, f->on_settled[1]);
    for (i = 0; i < f->nslots; i++)
        JS_FreeValueRT(rt, f->slots[i]);
    js_free_rt(rt, f);
}

/* While the step function runs, it has moved the locals out of the slots
   (they hold `undefined`), so this only sees values the frame owns. */
static void lum_frame_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    LumFrame *f = JS_GetOpaque(val, lum_frame_class);
    int i;
    if (!f)
        return;
    JS_MarkValue(rt, f->promise, mark_func);
    JS_MarkValue(rt, f->on_settled[0], mark_func);
    JS_MarkValue(rt, f->on_settled[1], mark_func);
    for (i = 0; i < f->nslots; i++)
        JS_MarkValue(rt, f->slots[i], mark_func);
}

/* A new frame for async function `fn` (returned), its promise and slots. */
JSValue lum_async_start(JSContext *ctx, int fn, int nslots, JSValue *promise, JSValue **slots)
{
    LumFrame *f = js_mallocz(ctx, sizeof(*f) + nslots * sizeof(JSValue));
    JSValue obj;
    int i;
    if (!f)
        return JS_EXCEPTION;
    f->fn = fn;
    f->nslots = nslots;
    f->promise = JS_UNDEFINED;
    f->on_settled[0] = f->on_settled[1] = JS_UNDEFINED;
    for (i = 0; i < nslots; i++)
        f->slots[i] = JS_UNDEFINED;
    obj = JS_NewObjectClass(ctx, lum_frame_class);
    if (JS_IsException(obj)) {
        js_free(ctx, f);
        return obj;
    }
    JS_SetOpaque(obj, f);
    f->self = JS_VALUE_GET_PTR(obj);
    /* (No resolving functions: only the frame settles it.) */
    *promise = JS_NewPendingPromise(ctx);
    if (JS_IsException(*promise)) {
        JS_FreeValue(ctx, obj);
        return JS_EXCEPTION;
    }
    f->promise = JS_DupValue(ctx, *promise);
    /* (the frame object may have survived a collection since made) */
    JS_GCWriteBarrier(JS_GetRuntime(ctx), JS_VALUE_GET_PTR(obj));
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
    /* (the slots, the reactions: written by the step) */
    JS_GCWriteBarrier(JS_GetRuntime(ctx), f->self);
    if (JS_IsUninitialized(r))
        return; /* suspended at an await */
    is_reject = JS_IsException(r);
    arg = is_reject ? JS_GetException(ctx) : r;
    JS_SettlePromise(ctx, f->promise, arg, is_reject);
    JS_FreeValue(ctx, arg);
    /* break the frame <-> reactions cycle */
    JS_FreeValue(ctx, f->on_settled[0]);
    JS_FreeValue(ctx, f->on_settled[1]);
    f->on_settled[0] = f->on_settled[1] = JS_UNDEFINED;
}

/* The shape of the functions resuming frames. */
static LumFnCache lum_resume_cache;

static JSValue lum_async_resume(JSContext *ctx, JSValueConst this_val, int argc,
                                  JSValueConst *argv, int is_throw, JSValueConst *data)
{
    LumFrame *f = JS_GetOpaque(data[0], lum_frame_class);
    JSValue value = argc > 0 ? argv[0] : JS_UNDEFINED;
    lum_async_step(ctx, f->slots, f->state, &value, is_throw);
    return JS_UNDEFINED;
}

/* The job that resumes a frame (argv[0]) awaiting a primitive (argv[1]),
 * queued where the promise reaction would be. */
static JSValue lum_await_job(JSContext *ctx, int argc, JSValueConst *argv)
{
    LumFrame *f = JS_GetOpaque(argv[0], lum_frame_class);
    JSValue value = argv[1];
    lum_async_step(ctx, f->slots, f->state, &value, 0);
    return JS_UNDEFINED;
}

/* `await value` at resume point `state`: continue the frame when the value
   settles. Takes ownership of `value`. Returns -1 on exception. */
int lum_await(JSContext *ctx, JSValue *slots, JSValue value, int state)
{
    LumFrame *f = lum_frame_of(slots);
    JSValue self;
    int i, ret = -1;
    f->state = state;
    /* A primitive: the frame resumes from a job of its own. */
    ret = JS_EnqueueAwaitJob(ctx, value, lum_await_job, JS_MKPTR(JS_TAG_OBJECT, f->self));
    if (ret <= 0)
        goto done;
    ret = -1;
    /* The reactions never reach user code, so one pair serves every await. */
    for (i = 0; i < 2; i++) {
        if (!JS_IsUndefined(f->on_settled[i]))
            continue;
        self = JS_MKPTR(JS_TAG_OBJECT, f->self);
        f->on_settled[i] = lum_new_closure(ctx, &lum_resume_cache, NULL, lum_async_resume, "", 1, i, 1, &self);
        if (JS_IsException(f->on_settled[i])) {
            f->on_settled[i] = JS_UNDEFINED;
            goto done;
        }
    }
    ret = JS_AwaitValue(ctx, value, f->on_settled[0], f->on_settled[1]);
 done:
    JS_FreeValue(ctx, value);
    return ret;
}

/* ---- module state: cells, string constants, atoms ---- */

static JSClassID lum_state_class;
static int lum_structs_init(JSContext *ctx);
static void lum_structs_free(JSRuntime *rt);
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

static void lum_state_finalizer(JSRuntime *rt, JSValueConst val)
{
    int i;
    lum_structs_free(rt);
    for (i = 0; i < lum_fn_cache_count(); i++) {
        if (lum_fn_caches[i].shape)
            JS_FreeCachedShape(rt, lum_fn_caches[i].shape);
        lum_fn_caches[i].shape = NULL;
        if (lum_fn_caches[i].name != JS_ATOM_NULL)
            JS_FreeAtomRT(rt, lum_fn_caches[i].name);
        lum_fn_caches[i].name = JS_ATOM_NULL;
    }
    if (lum_resume_cache.shape)
        JS_FreeCachedShape(rt, lum_resume_cache.shape);
    lum_resume_cache.shape = NULL;
    if (lum_cca_shape)
        JS_FreeCachedShape(rt, lum_cca_shape);
    lum_cca_shape = NULL;
    if (lum_resume_cache.name != JS_ATOM_NULL)
        JS_FreeAtomRT(rt, lum_resume_cache.name);
    lum_resume_cache.name = JS_ATOM_NULL;
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

static void lum_state_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    int i;
    for (i = 0; i < lum_fn_cache_count(); i++) {
        if (lum_fn_caches[i].shape)
            JS_MarkCachedShape(rt, lum_fn_caches[i].shape, mark_func);
    }
    if (lum_resume_cache.shape)
        JS_MarkCachedShape(rt, lum_resume_cache.shape, mark_func);
    for (i = 0; i < lum_ncells; i++)
        JS_MarkValue(rt, lum_cells[i], mark_func);
    for (i = 0; i < lum_nprivates; i++)
        JS_MarkValue(rt, lum_privates[i], mark_func);
    for (i = 0; i < lum_nhomes; i++)
        JS_MarkValue(rt, lum_homes[i], mark_func);
    for (i = 0; i < LUM_PINS; i++)
        JS_MarkValue(rt, lum_pins[i], mark_func);
    for (i = 0; i < lum_record_count() * LUM_RECORD_STRIDE; i++) {
        if (i % LUM_RECORD_STRIDE < LUM_RECORD_SHAPES && lum_record_shapes[i])
            JS_MarkCachedShape(rt, lum_record_shapes[i], mark_func);
    }
    for (i = 0; i < lum_ic_count(); i++) {
        if (lum_ics[i].shape)
            JS_MarkCachedShape(rt, lum_ics[i].shape, mark_func);
        if (lum_ics[i].proto_shape)
            JS_MarkCachedShape(rt, lum_ics[i].proto_shape, mark_func);
        if (lum_ics[i].mid_shape)
            JS_MarkCachedShape(rt, lum_ics[i].mid_shape, mark_func);
        if (lum_ics[i].alt_shape)
            JS_MarkCachedShape(rt, lum_ics[i].alt_shape, mark_func);
        if (lum_ics[i].poly) {
            int k;
            for (k = 0; k < LUM_POLY; k++) {
                LumIC *e = &lum_ics[i].poly[k];
                if (e->shape)
                    JS_MarkCachedShape(rt, e->shape, mark_func);
                if (e->alt_shape)
                    JS_MarkCachedShape(rt, e->alt_shape, mark_func);
                if (e->proto_shape)
                    JS_MarkCachedShape(rt, e->proto_shape, mark_func);
                if (e->mid_shape)
                    JS_MarkCachedShape(rt, e->mid_shape, mark_func);
                if (e->mid2_shape)
                    JS_MarkCachedShape(rt, e->mid2_shape, mark_func);
            }
        }
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
        JSClassDef box_def = { "LumBox", .finalizer = lum_box_finalizer, .gc_mark = lum_box_mark };
        JS_NewClassID(rt, &lum_box_class);
        if (JS_NewClass(rt, lum_box_class, &box_def) < 0)
            return -1;
    }
    {
        JSClassDef frame_def = { "LumFrame", .finalizer = lum_frame_finalizer, .gc_mark = lum_frame_mark };
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
        "             g.Set.prototype.has);\n"
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
    /* Their members all created (above), the builtin prototypes keep their
       shapes: their methods can be cached. */
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
    return e->shape || (e->alt_shape && e->kind == LUM_IC_PROTO_DATA);
}

/* The address of the property that cache entry `e` holds for object `p`
   of shape `shape`, or NULL (another shape, or a holder that changed): as
   native code finds it. */
static JSValue *lum_ic_entry_slot(const LumIC *e, char *p, void *shape)
{
    char *holder = p;
    if (e->shape != shape) {
        if (e->alt_shape != shape || e->kind != LUM_IC_PROTO_DATA)
            return NULL;
        holder = *(char **)((char *)shape + JS_LAYOUT_SHAPE_PROTO);
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
    }
    return (JSValue *)*(char **)(holder + JS_LAYOUT_OBJ_PROP) + e->slot;
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
            return true;
        }
    }
    for (k = 0; k < 4; k++)
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
        v = JS_MKPTR(JS_TAG_OBJECT, ((void **)at)[ic->kind == LUM_IC_PROTO_GETTER ? 0 : 1]);
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

/* A cache miss reading obj.prop (obj borrowed). */
JSValue lum_ic_get(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic)
{
    JSValue *at = lum_ic_poly_slot(ic, obj);
    JSValue v;
    if (at)
        return JS_DupValue(ctx, *at);
    v = JS_GetProperty(ctx, obj, prop);
    if (!JS_IsException(v) && !lum_ic_full(ic))
        lum_ic_fill(ctx, obj, prop, 0, ic);
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
    if (JS_VALUE_GET_TAG(obj) == JS_TAG_OBJECT)
        before = *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE);
    r = JS_SetProperty(ctx, obj, prop, val);
    if (r >= 0 && before && before == *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE) &&
        !lum_ic_full(ic))
        lum_ic_fill(ctx, obj, prop, 1, ic);
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
    LumFrame *f = js_mallocz(ctx, sizeof(*f) + nslots * sizeof(JSValue));
    JSValue obj;
    int i;
    if (!f)
        return JS_EXCEPTION;
    f->fn = fn;
    f->nslots = nslots;
    f->promise = JS_UNDEFINED;
    for (i = 0; i < nslots; i++)
        f->slots[i] = JS_UNDEFINED;
    obj = JS_NewObjectClass(ctx, lum_frame_class);
    if (JS_IsException(obj)) {
        js_free(ctx, f);
        return obj;
    }
    JS_SetOpaque(obj, f);
    f->self = JS_VALUE_GET_PTR(obj);
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
    JS_GCWriteBarrier(JS_GetRuntime(ctx), f->self);
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
    JS_GCWriteBarrier(JS_GetRuntime(ctx), f->self);
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
    JS_GCWriteBarrier(JS_GetRuntime(ctx), f->self);
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

/* obj[key] in a for-in: v = { iterator, obj, key } (borrowed). */
JSValue lum_for_in_get(JSContext *ctx, JSValue *v)
{
    return JS_ForInGet(ctx, v[0], v[1], v[2]);
}

/* Whether an iteration with this iterator and `next` method can resume a
 * native generator directly: the original %GeneratorPrototype%.next. */
int lum_gen_iter_ok(JSContext *ctx, JSValue iter, JSValue next)
{
    JSValue orig = lum_pins[LUM_PIN_GENERATOR_NEXT];
    return JS_IsNativeGenerator(iter) && JS_VALUE_GET_TAG(next) == JS_VALUE_GET_TAG(orig) &&
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
_Static_assert(sizeof(LumIC) == 88 && offsetof(LumIC, slot) == 8 && offsetof(LumIC, kind) == 12 &&
                   offsetof(LumIC, proto_shape) == 16 && offsetof(LumIC, mid_shape) == 24 &&
                   offsetof(LumIC, alt_shape) == 32 && offsetof(LumIC, poly) == 40 &&
                   offsetof(LumIC, epoch) == 48 && offsetof(LumIC, value) == 56 && offsetof(LumIC, entry) == 72,
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
        if (at)
            return JS_DupValue(ctx, *at);
    }
    v = JS_GetProperty(ctx, obj, prop);
    if (lum_ic_full(ic))
        return v;
    if (!JS_IsException(v) && !lum_ic_fill(ctx, obj, prop, 0, ic) &&
        !lum_ic_fill(ctx, obj, prop, 2, ic) &&
        !lum_ic_fill_proto(ctx, obj, prop, LUM_IC_PROTO_DATA, ic) &&
        !lum_ic_fill_proto(ctx, obj, prop, LUM_IC_PROTO_GETTER, ic) &&
        !lum_ic_fill_deep(ctx, obj, prop, ic))
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
    if (JS_VALUE_GET_TAG(obj) == JS_TAG_OBJECT)
        before = *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE);
    r = JS_SetProperty(ctx, obj, prop, val);
    if (r >= 0 && before && !lum_ic_gave_up(ic) && !lum_ic_full(ic) &&
        before == *(void **)((char *)JS_VALUE_GET_PTR(obj) + JS_LAYOUT_OBJ_SHAPE) &&
        !lum_ic_fill(ctx, obj, prop, 1, ic) &&
        !lum_ic_fill(ctx, obj, prop, 3, ic) &&
        !lum_ic_fill_proto(ctx, obj, prop, LUM_IC_PROTO_SETTER, ic))
        lum_ic_count_miss(ic);
    return r;
}

/* Call a cached getter (a function object) on this_obj (borrowed), as the
 * engine does: holding the function while it runs. */
JSValue lum_call_getter(JSContext *ctx, void *getter, JSValue this_obj)
{
    JSValue f = JS_DupValue(ctx, JS_MKPTR(JS_TAG_OBJECT, getter));
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
        /* How native code allocates the next ones inline. */
        ic->slot = JS_ShapeAllocInfo(JS_GetRuntime(ctx), shape);
    }
    return obj;
}

/* A closure of a native function (data borrowed): made with the shape of
 * the function's first closure, as JSC allocates functions with a known
 * structure. */
JSValue lum_new_closure(JSContext *ctx, LumFnCache *c, JSCFunctionFast *fast, JSCFunctionData *slow,
                          const char *name, int length, int magic, int n, JSValue *data)
{
    if (c->name == JS_ATOM_NULL) {
        c->name = JS_NewAtom(ctx, name);
        if (c->name == JS_ATOM_NULL)
            return JS_EXCEPTION;
    }
    return JS_NewCFunctionDataShaped(ctx, fast, slow, c->name, length, magic, n, (JSValueConst *)data, &c->shape);
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
/* Per layout field (as lum_struct_atoms): its JSON key, `,"name":`, or NULL
   when the name needs escapes or is not ASCII (written from its atom). */
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
    static const size_t sizes[] = { 4, 8, 1, 16 };
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

static bool lum_value_fits(JSContext *ctx, const LumTypeDesc *t, JSValueConst v)
{
    int tag = JS_VALUE_GET_TAG(v);
    LumStruct *s;
    uint32_t i;
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

/* Store v (taken) in field fi of s, checked against the field's type; the
 * old value of a JS-valued field goes to *old (owned). */
static int lum_put_raw(JSContext *ctx, LumStruct *s, int fi, JSValue v, JSValue *old)
{
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    const LumFieldDesc *f = &l->fields[fi];
    void *at = lum_field_at(s, f);
    *old = JS_UNDEFINED;
    if (f->storage == LUM_ST_INT32) {
        /* Integers wrap to int32 (unless `--no-int-wrapping`). */
        int tag = JS_VALUE_GET_TAG(v);
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
        *(double *)at = JS_VALUE_GET_TAG(v) == JS_TAG_INT ? JS_VALUE_GET_INT(v) : JS_VALUE_GET_FLOAT64(v);
        break;
    case LUM_ST_BOOL:
        *(uint8_t *)at = JS_VALUE_GET_BOOL(v);
        break;
    default:
        *old = *(JSValue *)at;
        *(JSValue *)at = v;
        /* (the object, whose data the struct is) */
        if (JS_VALUE_HAS_REF_COUNT(v))
            JS_GCWriteBarrier(JS_GetRuntime(ctx), (char *)s - JS_LAYOUT_OBJ_SIZE);
        return 0;
    }
    JS_FreeValue(ctx, v);
    return 0;
}

/* Whether s matches a member: has its fields, with its types. */
static bool lum_struct_matches(JSContext *ctx, LumStruct *s)
{
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    uint32_t m, fi;
    for (m = 0; m < l->nmembers; m++) {
        const LumMemberDesc *mem = &l->members[m];
        bool ok = (mem->mask & ~s->mask) == 0;
        for (fi = 0; ok && fi < l->nfields; fi++) {
            if (!(mem->mask & (1u << fi)))
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

JSValue lum_struct_new(JSContext *ctx, int layout, uint32_t mask)
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
    return obj;
}

/* A field of a new object (from native code), checked against its type. */
int lum_struct_put(JSContext *ctx, JSValue obj, int field, JSValue v)
{
    JSValue old;
    int r = lum_put_raw(ctx, JS_GetOpaque(obj, lum_struct_class), field, v, &old);
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

/* A write of field `field` (v taken): it must be present, and the object
 * must still match a member afterwards (else it is left as it was). */
int lum_struct_set(JSContext *ctx, JSValue obj, int field, JSValue v)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    const LumFieldDesc *f = &l->fields[field];
    uint64_t saved[2];
    JSValue old;
    if (!(s->mask & (1u << field))) {
        JS_FreeValue(ctx, v);
        JS_ThrowTypeError(ctx, "cannot add `%s` to an object of aot type `%s`", f->name, l->name);
        return -1;
    }
    memcpy(saved, lum_field_at(s, f), lum_field_size(f));
    if (lum_put_raw(ctx, s, field, v, &old) < 0)
        return -1;
    if (l->validate && !f->simple && !lum_struct_matches(ctx, s)) {
        if (f->storage == LUM_ST_VALUE)
            JS_FreeValue(ctx, *(JSValue *)lum_field_at(s, f));
        memcpy(lum_field_at(s, f), saved, lum_field_size(f));
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

static int lum_struct_delete(JSContext *ctx, JSValueConst obj, JSAtom prop)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    int fi = lum_field_index(s->layout, prop);
    return !(fi >= 0 && (s->mask & (1u << fi)));
}

static int lum_struct_define(JSContext *ctx, JSValueConst obj, JSAtom prop, JSValueConst val,
                               JSValueConst getter, JSValueConst setter, int flags)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    int fi = lum_field_index(s->layout, prop);
    if (fi < 0 || !(s->mask & (1u << fi))) {
        JS_ThrowTypeError(ctx, "cannot add a property to an object of aot type `%s`", l->name);
        return -1;
    }
    if ((flags & (JS_PROP_HAS_GET | JS_PROP_HAS_SET)) ||
        ((flags & JS_PROP_HAS_CONFIGURABLE) && (flags & JS_PROP_CONFIGURABLE)) ||
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

static int lum_struct_json(JSContext *ctx, JSJSONWriter *w, JSValueConst obj, int depth)
{
    LumStruct *s = JS_GetOpaque(obj, lum_struct_class);
    const LumLayoutDesc *l = &lum_layouts[s->layout];
    uint32_t base = lum_struct_atom_base[s->layout], i;
    bool first = true;
    int r;
    if (JS_JSONWrite8(w, "{", 1) < 0)
        return -1;
    for (i = 0; i < l->nfields; i++) {
        const LumFieldDesc *f = &l->fields[i];
        void *at = lum_field_at(s, f);
        if (!(s->mask & (1u << i)))
            continue;
        if (f->storage == LUM_ST_VALUE) {
            JSValueConst v = *(JSValue *)at;
            int tag = JS_VALUE_GET_TAG(v);
            if (tag == JS_TAG_UNDEFINED || tag == JS_TAG_SYMBOL || JS_IsFunction(ctx, v))
                continue;
        }
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
            key->text = js_malloc(ctx, len + 4);
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
    if (lum_struct_json_init(ctx, total) < 0 || lum_sarr_init(ctx) < 0)
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
        int tag = JS_VALUE_GET_TAG(v);
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
    a->len = a->cap = a->pad = 0;
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
            desc->flags = JS_PROP_WRITABLE;
            desc->value = JS_NewUint32(ctx, a->len);
            desc->getter = desc->setter = JS_UNDEFINED;
        }
        return 1;
    }
    if (!JS_AtomToArrayIndex(ctx, &idx, prop) || idx >= a->len || JS_IsUndefined(a->items[idx]))
        return 0;
    if (desc) {
        desc->flags = JS_PROP_C_W_E;
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
            ((flags & JS_PROP_HAS_WRITABLE) && !(flags & JS_PROP_WRITABLE))) {
            JS_ThrowTypeError(ctx, "cannot redefine the length of an array of aot type `%s`", name);
            return -1;
        }
        if (!(flags & JS_PROP_HAS_VALUE))
            return true;
        if (JS_ToFloat64(ctx, &d, val) < 0)
            return -1;
        if (!(d >= 0 && d <= UINT32_MAX && d == trunc(d))) {
            JS_ThrowRangeError(ctx, "invalid array length");
            return -1;
        }
        return lum_sarr_set_length(ctx, a, (uint32_t)d) < 0 ? -1 : true;
    }
    if (!JS_AtomToArrayIndex(ctx, &idx, prop)) {
        JS_ThrowTypeError(ctx, "cannot add a property to an array of aot type `%s`", name);
        return -1;
    }
    if ((flags & (JS_PROP_HAS_GET | JS_PROP_HAS_SET)) ||
        ((flags & JS_PROP_HAS_WRITABLE) && !(flags & JS_PROP_WRITABLE)) ||
        ((flags & JS_PROP_HAS_ENUMERABLE) && !(flags & JS_PROP_ENUMERABLE)) ||
        ((flags & JS_PROP_HAS_CONFIGURABLE) && !(flags & JS_PROP_CONFIGURABLE))) {
        JS_ThrowTypeError(ctx, "cannot redefine an element of an array of aot type `%s`", name);
        return -1;
    }
    if (!(flags & JS_PROP_HAS_VALUE))
        return true;
    return lum_sarr_set(ctx, obj, idx, JS_DupValue(ctx, val)) < 0 ? -1 : true;
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
