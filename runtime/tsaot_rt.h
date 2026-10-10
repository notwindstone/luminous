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

/* Called once while the context is created. */
int tsaot_lock_builtins(JSContext *ctx);
int tsaot_runtime_init(JSContext *ctx, JSValue *state, JSValue *init_fn, JSValue *cells_obj);

#endif
