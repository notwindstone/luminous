/* tsaot runtime: helpers for Cranelift-compiled code and the generated glue. */
#ifndef TSAOT_RT_H
#define TSAOT_RT_H

#include <stddef.h>
#include <stdint.h>
#include "quickjs.h"

/* Native stack budget below the frame that initializes the runtime (must
 * match tsaot_codegen::NATIVE_STACK_BUDGET; the glue asserts it). QuickJS
 * keeps its own usage under JS_DEFAULT_STACK_SIZE, so the two together stay
 * well within a default 8 MiB thread stack. */
#define TSAOT_NATIVE_STACK (4u * 1024 * 1024)

/* Checked on every native function entry. */
extern uintptr_t tsaot_stack_limit;
/* Set while an exception propagates out of native functions. */
extern uint8_t tsaot_unwinding;

/* Per-program tables, defined by the generated glue. */
extern JSValue tsaot_cells[];
extern JSValue tsaot_strings[];
extern JSAtom tsaot_atoms[];
extern const char *const tsaot_cell_names[];
extern const char *const tsaot_atom_names[];
extern const char *const tsaot_string_data[];
extern const size_t tsaot_string_lens[];
extern const int tsaot_mutable_cells[];
extern const int tsaot_ncells, tsaot_nstrings, tsaot_natoms, tsaot_nmutable;
/* Private name values and home objects of classes (filled by the classes
 * as they are defined). */
extern JSValue tsaot_privates[];
extern JSValue tsaot_homes[];
extern const int tsaot_nprivates, tsaot_nhomes;

/* Called from native code. */
double tsaot_pow(double a, double b);
double tsaot_round(double a);
int32_t tsaot_toint32_slow(double d);
void tsaot_stack_overflow(JSContext *ctx);
void tsaot_throw_uninitialized(JSContext *ctx, int cell);
int tsaot_set_elem(JSContext *ctx, JSValue *v);
int tsaot_delete_elem(JSContext *ctx, JSValue *v);
JSValue tsaot_call(JSContext *ctx, JSValue *fthis, int argc, JSValue *argv);
JSValue tsaot_global_or_undefined(JSContext *ctx, JSAtom name);
int tsaot_define_field(JSContext *ctx, JSValue *obj, JSAtom prop, JSValue val);
JSValue tsaot_new_array(JSContext *ctx, int count, JSValue *values);
JSValue tsaot_box_new(JSContext *ctx, JSValue **slot);
void tsaot_throw_uninitialized_atom(JSContext *ctx, JSAtom name);
/* Defined by the generated glue. */
JSValue tsaot_make_closure(JSContext *ctx, int fn, int n, JSValue *data);
/* Step functions of async functions (NULL for others), by function index. */
typedef JSValue TsaotStep(JSContext *ctx, JSValue *slots, int state, JSValue *value, int is_throw);
extern TsaotStep *const tsaot_steps[];
JSValue tsaot_async_start(JSContext *ctx, int fn, int nslots, JSValue *promise, JSValue **slots);
void tsaot_async_step(JSContext *ctx, JSValue *slots, int state, JSValue *value, int is_throw);
int tsaot_await(JSContext *ctx, JSValue *slots, JSValue value, int state);

/* Pristine objects and their shapes: typed arrays by JSTypedArrayEnum,
 * then an array; then the original array iteration objects. */
#define TSAOT_TYPED_ARRAYS 12
#define TSAOT_SHAPE_ARRAY 12
#define TSAOT_PIN_ARRAY_PROTO 13
#define TSAOT_PIN_ARRAY_VALUES 14
#define TSAOT_PIN_ARRAY_ITERATOR_PROTO 15
#define TSAOT_PIN_ARRAY_ITERATOR_NEXT 16
#define TSAOT_PIN_SYMBOL_ITERATOR 17
#define TSAOT_PIN_ASYNC_FUNCTION_PROTO 18
#define TSAOT_PIN_GENERATOR_FUNCTION_PROTO 19
#define TSAOT_PIN_GENERATOR_NEXT 20
#define TSAOT_PINS 21
int tsaot_gen_iter_ok(JSContext *ctx, JSValue iter, JSValue next);
JSValue tsaot_function_kind(JSContext *ctx, JSValue func, JSValueConst fallback, int kind);
int tsaot_array_iter_ok(JSContext *ctx);
extern JSValue tsaot_pins[];
extern void *tsaot_shapes[];

/* Inline caches of own data properties, one per property access site in
 * native code (defined by it; absent without native code). */
typedef struct TsaotIC {
    void *shape; /* a referenced shape, or NULL */
    uint32_t slot;
    uint32_t unused;
} TsaotIC;
extern TsaotIC tsaot_ics[] __attribute__((weak));
extern const int32_t tsaot_nics __attribute__((weak));
JSValue tsaot_ic_get(JSContext *ctx, JSValue obj, JSAtom prop, TsaotIC *ic);
int tsaot_ic_set(JSContext *ctx, JSValue obj, JSAtom prop, JSValue val, TsaotIC *ic);

/* Spread, rest and iteration. */
int tsaot_array_push(JSContext *ctx, JSValue arr, JSValue v);
int tsaot_array_hole(JSContext *ctx, JSValue arr);
int tsaot_array_spread(JSContext *ctx, JSValue arr, JSValue iterable);
JSValue tsaot_apply(JSContext *ctx, JSValue *v);
JSValue tsaot_construct(JSContext *ctx, JSValue f, JSValue args);
JSValue tsaot_rest(JSContext *ctx, int argc, JSValue *argv, int start);
JSValue tsaot_object_rest(JSContext *ctx, JSValue *v, int nkeys);
int tsaot_define_computed(JSContext *ctx, JSValue *v, JSValue val);
int tsaot_object_spread(JSContext *ctx, JSValue obj, JSValue source);
int tsaot_array_iter_close(JSContext *ctx, JSValue arr, int32_t idx, int is_throw);
JSValue tsaot_super_get(JSContext *ctx, JSValue *v);
JSValue tsaot_for_in_get(JSContext *ctx, JSValue *v);
int tsaot_private_set(JSContext *ctx, JSValue *v, JSValue val);
int tsaot_super_set(JSContext *ctx, JSValue *v, JSValue val);
JSValue tsaot_gen_start(JSContext *ctx, int fn, int nslots, JSValue **slots);
JSValue tsaot_gen_new(JSContext *ctx, JSValue frame, JSValueConst *func_data);
JSValue tsaot_yield_star(JSContext *ctx, JSValue *v, int mode, int *kind);

/* Called once while the context is created. */
int tsaot_lock_builtins(JSContext *ctx);
int tsaot_runtime_init(JSContext *ctx, JSValue *state, JSValue *init_fn, JSValue *cells_obj);

#endif
