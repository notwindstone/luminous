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
 * global Math / Number bindings become non-writable and non-configurable, so
 * patching them throws a TypeError (all tsaot modules are strict). Adding new
 * properties stays possible. */
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
        "})(globalThis);\n";
    JSValue r = JS_Eval(ctx, src, sizeof(src) - 1, "<tsaot>", JS_EVAL_TYPE_GLOBAL);
    if (JS_IsException(r))
        return -1;
    JS_FreeValue(ctx, r);
    return 0;
}
