#include <math.h>

#include "tsaot_rt.h"

uintptr_t tsaot_stack_limit;
uint8_t tsaot_unwinding;

/* Same as QuickJS js_math_pow: JS differs from IEEE pow for |a| == 1 with a
 * non-finite exponent. */
double tsaot_pow(double a, double b)
{
    if (!isfinite(b) && fabs(a) == 1)
        return NAN;
    return pow(a, b);
}

/* Same as QuickJS js_math_round (round half up, preserving -0). */
double tsaot_round(double a)
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
int32_t tsaot_toint32_slow(double d)
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
void tsaot_stack_overflow(JSContext *ctx)
{
    JS_ThrowRangeError(ctx, "Maximum call stack size exceeded");
}

void tsaot_throw_uninitialized(JSContext *ctx, int cell)
{
    JS_ThrowReferenceError(ctx, "%s is not initialized", tsaot_cell_names[cell]);
}

/* ---- operations with more JSValue arguments than fit in registers ---- */

/* obj[key] = val. Takes ownership of key and val. */
int tsaot_set_elem(JSContext *ctx, JSValue *v)
{
    return JS_SetPropertyValue(ctx, v[0], v[1], v[2], JS_PROP_THROW);
}

/* delete obj[key]. Borrows both. */
int tsaot_delete_elem(JSContext *ctx, JSValue *v)
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
JSValue tsaot_call(JSContext *ctx, JSValue *fthis, int argc, JSValue *argv)
{
    return JS_Call(ctx, fthis[0], fthis[1], argc, (JSValueConst *)argv);
}

/* The global `name`, or undefined (as in `typeof name`). */
JSValue tsaot_global_or_undefined(JSContext *ctx, JSAtom name)
{
    JSValue global = JS_GetGlobalObject(ctx);
    JSValue ret = JS_GetProperty(ctx, global, name);
    JS_FreeValue(ctx, global);
    return ret;
}

/* Object literal field. Takes ownership of val. */
int tsaot_define_field(JSContext *ctx, JSValue *obj, JSAtom prop, JSValue val)
{
    return JS_DefinePropertyValue(ctx, *obj, prop, val, JS_PROP_C_W_E | JS_PROP_THROW);
}

/* Array literal. Takes ownership of the elements. */
JSValue tsaot_new_array(JSContext *ctx, int count, JSValue *values)
{
    return JS_NewArrayFrom(ctx, count, values);
}

/* ---- boxes: variables captured by closures ---- */

static JSClassID tsaot_box_class;

typedef struct TsaotBox {
    JSValue value;
} TsaotBox;

static void tsaot_box_finalizer(JSRuntime *rt, JSValueConst val)
{
    TsaotBox *b = JS_GetOpaque(val, tsaot_box_class);
    if (b) {
        JS_FreeValueRT(rt, b->value);
        js_free_rt(rt, b);
    }
}

static void tsaot_box_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    TsaotBox *b = JS_GetOpaque(val, tsaot_box_class);
    if (b)
        JS_MarkValue(rt, b->value, mark_func);
}

/* A new box holding an uninitialized binding; *slot receives the address
   of its value, which stays valid as long as the box object lives. */
JSValue tsaot_box_new(JSContext *ctx, JSValue **slot)
{
    TsaotBox *b = js_malloc(ctx, sizeof(*b));
    JSValue obj;
    if (!b)
        return JS_EXCEPTION;
    b->value = JS_UNINITIALIZED;
    obj = JS_NewObjectClass(ctx, tsaot_box_class);
    if (JS_IsException(obj)) {
        js_free(ctx, b);
        return obj;
    }
    JS_SetOpaque(obj, b);
    *slot = &b->value;
    return obj;
}

void tsaot_throw_uninitialized_atom(JSContext *ctx, JSAtom name)
{
    const char *s = JS_AtomToCString(ctx, name);
    JS_ThrowReferenceError(ctx, "%s is not initialized", s ? s : "lexical variable");
    JS_FreeCString(ctx, s);
}

/* ---- async functions ---- */

/* The heap frame of a running async function: its locals while it is
   suspended, and its promise's resolving functions. */
typedef struct TsaotFrame {
    void *self;               /* the frame object (not a counted reference) */
    JSValue resolve, reject;
    int fn, nslots;
    JSValue slots[];
} TsaotFrame;

static JSClassID tsaot_frame_class;

static TsaotFrame *tsaot_frame_of(JSValue *slots)
{
    return (TsaotFrame *)((char *)slots - offsetof(TsaotFrame, slots));
}

static void tsaot_frame_finalizer(JSRuntime *rt, JSValueConst val)
{
    TsaotFrame *f = JS_GetOpaque(val, tsaot_frame_class);
    int i;
    if (!f)
        return;
    JS_FreeValueRT(rt, f->resolve);
    JS_FreeValueRT(rt, f->reject);
    for (i = 0; i < f->nslots; i++)
        JS_FreeValueRT(rt, f->slots[i]);
    js_free_rt(rt, f);
}

/* While the step function runs, it has moved the locals out of the slots
   (they hold `undefined`), so this only sees values the frame owns. */
static void tsaot_frame_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    TsaotFrame *f = JS_GetOpaque(val, tsaot_frame_class);
    int i;
    if (!f)
        return;
    JS_MarkValue(rt, f->resolve, mark_func);
    JS_MarkValue(rt, f->reject, mark_func);
    for (i = 0; i < f->nslots; i++)
        JS_MarkValue(rt, f->slots[i], mark_func);
}

/* A new frame for async function `fn` (returned), its promise and slots. */
JSValue tsaot_async_start(JSContext *ctx, int fn, int nslots, JSValue *promise, JSValue **slots)
{
    TsaotFrame *f = js_mallocz(ctx, sizeof(*f) + nslots * sizeof(JSValue));
    JSValue funcs[2], obj;
    int i;
    if (!f)
        return JS_EXCEPTION;
    f->fn = fn;
    f->nslots = nslots;
    f->resolve = f->reject = JS_UNDEFINED;
    for (i = 0; i < nslots; i++)
        f->slots[i] = JS_UNDEFINED;
    obj = JS_NewObjectClass(ctx, tsaot_frame_class);
    if (JS_IsException(obj)) {
        js_free(ctx, f);
        return obj;
    }
    JS_SetOpaque(obj, f);
    f->self = JS_VALUE_GET_PTR(obj);
    *promise = JS_NewPromiseCapability(ctx, funcs);
    if (JS_IsException(*promise)) {
        JS_FreeValue(ctx, obj);
        return JS_EXCEPTION;
    }
    f->resolve = funcs[0];
    f->reject = funcs[1];
    *slots = f->slots;
    return obj;
}

/* Run the async function from `state` (0: start) with `value` delivered at
   its await (thrown there if `is_throw`); settle its promise on completion. */
void tsaot_async_step(JSContext *ctx, JSValue *slots, int state, JSValue *value, int is_throw)
{
    TsaotFrame *f = tsaot_frame_of(slots);
    JSValue r = tsaot_steps[f->fn](ctx, slots, state, value, is_throw);
    JSValue settle, arg, ret;
    if (JS_IsUninitialized(r))
        return; /* suspended at an await */
    if (JS_IsException(r)) {
        settle = f->reject;
        arg = JS_GetException(ctx);
    } else {
        settle = f->resolve;
        arg = r;
    }
    ret = JS_Call(ctx, settle, JS_UNDEFINED, 1, (JSValueConst *)&arg);
    JS_FreeValue(ctx, arg);
    JS_FreeValue(ctx, ret);
}

static JSValue tsaot_async_resume(JSContext *ctx, JSValueConst this_val, int argc,
                                  JSValueConst *argv, int is_throw, JSValueConst *data)
{
    TsaotFrame *f = JS_GetOpaque(data[0], tsaot_frame_class);
    JSValue value = argc > 0 ? argv[0] : JS_UNDEFINED;
    tsaot_async_step(ctx, f->slots, JS_VALUE_GET_INT(data[1]), &value, is_throw);
    return JS_UNDEFINED;
}

/* `await value` at resume point `state`: continue the frame when the value
   settles. Takes ownership of `value`. Returns -1 on exception. */
int tsaot_await(JSContext *ctx, JSValue *slots, JSValue value, int state)
{
    TsaotFrame *f = tsaot_frame_of(slots);
    JSValue data[2], on_fulfilled, on_rejected;
    int ret = -1;
    data[0] = JS_DupValue(ctx, JS_MKPTR(JS_TAG_OBJECT, f->self));
    data[1] = JS_NewInt32(ctx, state);
    on_fulfilled = JS_NewCFunctionData(ctx, tsaot_async_resume, 1, 0, 2, data);
    on_rejected = JS_NewCFunctionData(ctx, tsaot_async_resume, 1, 1, 2, data);
    if (!JS_IsException(on_fulfilled) && !JS_IsException(on_rejected))
        ret = JS_AwaitValue(ctx, value, on_fulfilled, on_rejected);
    JS_FreeValue(ctx, on_fulfilled);
    JS_FreeValue(ctx, on_rejected);
    JS_FreeValue(ctx, data[0]);
    JS_FreeValue(ctx, value);
    return ret;
}

/* ---- module state: cells, string constants, atoms ---- */

static JSClassID tsaot_state_class;

static void tsaot_state_finalizer(JSRuntime *rt, JSValueConst val)
{
    int i;
    for (i = 0; i < tsaot_ncells; i++) {
        JS_FreeValueRT(rt, tsaot_cells[i]);
        tsaot_cells[i] = JS_UNINITIALIZED;
    }
    for (i = 0; i < tsaot_nstrings; i++) {
        JS_FreeValueRT(rt, tsaot_strings[i]);
        tsaot_strings[i] = JS_UNDEFINED;
    }
    for (i = 0; i < tsaot_natoms; i++)
        JS_FreeAtomRT(rt, tsaot_atoms[i]);
    for (i = 0; i < TSAOT_TYPED_ARRAYS; i++) {
        JS_FreeValueRT(rt, tsaot_ta_pins[i]);
        tsaot_ta_pins[i] = JS_UNDEFINED;
    }
}

/* The cells are reachable from native code at any time, so they are roots
   for the cycle collector: marking them through this object (which the
   module keeps alive) lets cycles that go through cells be collected once
   the module itself goes away. */
static void tsaot_state_mark(JSRuntime *rt, JSValueConst val, JS_MarkFunc *mark_func)
{
    int i;
    for (i = 0; i < tsaot_ncells; i++)
        JS_MarkValue(rt, tsaot_cells[i], mark_func);
    for (i = 0; i < TSAOT_TYPED_ARRAYS; i++)
        JS_MarkValue(rt, tsaot_ta_pins[i], mark_func);
}

static JSValue tsaot_cell_get(JSContext *ctx, JSValueConst this_val, int argc,
                              JSValueConst *argv, int cell)
{
    if (JS_IsUninitialized(tsaot_cells[cell]))
        return tsaot_throw_uninitialized(ctx, cell), JS_EXCEPTION;
    return JS_DupValue(ctx, tsaot_cells[cell]);
}

static JSValue tsaot_cell_set(JSContext *ctx, JSValueConst this_val, int argc,
                              JSValueConst *argv, int cell)
{
    JSValue old = tsaot_cells[cell];
    if (JS_IsUninitialized(old))
        return tsaot_throw_uninitialized(ctx, cell), JS_EXCEPTION;
    tsaot_cells[cell] = JS_DupValue(ctx, argc > 0 ? argv[0] : JS_UNDEFINED);
    JS_FreeValue(ctx, old);
    return JS_UNDEFINED;
}

/* __tsaot_init(cell, value): the binding's declaration ran; returns value. */
static JSValue tsaot_cell_init(JSContext *ctx, JSValueConst this_val, int argc,
                               JSValueConst *argv)
{
    int32_t cell;
    JSValue old;
    if (argc < 2 || JS_ToInt32(ctx, &cell, argv[0]) || cell < 0 || cell >= tsaot_ncells)
        return JS_ThrowRangeError(ctx, "__tsaot_init: bad cell");
    old = tsaot_cells[cell];
    tsaot_cells[cell] = JS_DupValue(ctx, argv[1]);
    JS_FreeValue(ctx, old);
    return JS_DupValue(ctx, argv[1]);
}

/* Set up the module state; returns the objects the module exports, or -1. */
int tsaot_runtime_init(JSContext *ctx, JSValue *state, JSValue *init_fn, JSValue *cells_obj)
{
    JSRuntime *rt = JS_GetRuntime(ctx);
    JSClassDef def = { "TsaotState", .finalizer = tsaot_state_finalizer, .gc_mark = tsaot_state_mark };
    int i;

    if (tsaot_lock_builtins(ctx) < 0)
        return -1;
    /* Absolute limit for native frames, below the stack in use now. */
    tsaot_stack_limit = (uintptr_t)__builtin_frame_address(0) - TSAOT_NATIVE_STACK;

    for (i = 0; i < tsaot_natoms; i++) {
        tsaot_atoms[i] = JS_NewAtom(ctx, tsaot_atom_names[i]);
        if (tsaot_atoms[i] == JS_ATOM_NULL)
            return -1;
    }
    for (i = 0; i < tsaot_nstrings; i++) {
        tsaot_strings[i] = JS_NewStringLen(ctx, tsaot_string_data[i], tsaot_string_lens[i]);
        if (JS_IsException(tsaot_strings[i]))
            return -1;
    }
    for (i = 0; i < tsaot_ncells; i++)
        tsaot_cells[i] = JS_UNINITIALIZED;

    {
        JSClassDef box_def = { "TsaotBox", .finalizer = tsaot_box_finalizer, .gc_mark = tsaot_box_mark };
        JS_NewClassID(rt, &tsaot_box_class);
        if (JS_NewClass(rt, tsaot_box_class, &box_def) < 0)
            return -1;
    }
    {
        JSClassDef frame_def = { "TsaotFrame", .finalizer = tsaot_frame_finalizer, .gc_mark = tsaot_frame_mark };
        JS_NewClassID(rt, &tsaot_frame_class);
        if (JS_NewClass(rt, tsaot_frame_class, &frame_def) < 0)
            return -1;
    }
    JS_NewClassID(rt, &tsaot_state_class);
    if (JS_NewClass(rt, tsaot_state_class, &def) < 0)
        return -1;
    *state = JS_NewObjectClass(ctx, tsaot_state_class);
    if (JS_IsException(*state))
        return -1;
    *init_fn = JS_NewCFunction(ctx, tsaot_cell_init, "__tsaot_init", 2);

    /* Accessors through which interpreted code uses mutable cells. */
    *cells_obj = JS_NewObjectProto(ctx, JS_NULL);
    for (i = 0; i < tsaot_nmutable; i++) {
        int cell = tsaot_mutable_cells[i];
        JSAtom name = JS_NewAtom(ctx, tsaot_cell_names[cell]);
        JSValue get = JS_NewCFunctionMagic(ctx, tsaot_cell_get, tsaot_cell_names[cell], 0,
                                           JS_CFUNC_generic_magic, cell);
        JSValue set = JS_NewCFunctionMagic(ctx, tsaot_cell_set, tsaot_cell_names[cell], 1,
                                           JS_CFUNC_generic_magic, cell);
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
 * (all tsaot modules are strict). Adding new properties stays possible.
 *
 * Each typed array prototype also gets the inherited `length` getter as a
 * locked own property. Then the `length` of a typed array whose shape is
 * that of a fresh instance (no own properties, original prototype) is
 * always its element count, which native code reads inline. */
JSValue tsaot_ta_pins[TSAOT_TYPED_ARRAYS];
void *tsaot_ta_shapes[TSAOT_TYPED_ARRAYS];

int tsaot_lock_builtins(JSContext *ctx)
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
        "  return fresh;\n"
        "})(globalThis);\n";
    JSValue r = JS_Eval(ctx, src, sizeof(src) - 1, "<tsaot>", JS_EVAL_TYPE_GLOBAL);
    int i;
    if (JS_IsException(r))
        return -1;
    for (i = 0; i < TSAOT_TYPED_ARRAYS; i++) {
        tsaot_ta_pins[i] = JS_GetPropertyUint32(ctx, r, i);
        if (JS_IsException(tsaot_ta_pins[i]))
            return JS_FreeValue(ctx, r), -1;
        tsaot_ta_shapes[i] = *(void **)((char *)JS_VALUE_GET_PTR(tsaot_ta_pins[i]) + JS_LAYOUT_OBJ_SHAPE);
    }
    JS_FreeValue(ctx, r);
    return 0;
}
