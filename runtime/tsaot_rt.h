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
/* Record types (see ir::specialize_fields): per record, its shape once
 * learned (defined by the native code), and its fields (count, then atom
 * indices, from tsaot_record_index[r]). */
extern void *tsaot_record_shapes[] __attribute__((weak));
extern const int32_t tsaot_record_fields[] __attribute__((weak));
extern const int32_t tsaot_record_index[] __attribute__((weak));
extern const int32_t tsaot_nrecords __attribute__((weak));
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
#define TSAOT_PIN_ASYNC_GENERATOR_FUNCTION_PROTO 21
#define TSAOT_PIN_PROMISE 22
#define TSAOT_PIN_PROMISE_RESOLVE 23
#define TSAOT_PIN_STRING_PROTO 24
#define TSAOT_PIN_CHAR_CODE_AT 25
#define TSAOT_PIN_ARRAY_PUSH 26
/* Array.prototype.forEach, map, filter, some, every, find, findIndex
 * (native loops). */
#define TSAOT_PIN_ARRAY_LOOPS 27
#define TSAOT_PIN_ARRAY_SORT 34
/* String.prototype.slice, substring, indexOf, includes, startsWith,
 * endsWith (JS_CallStringFn). */
#define TSAOT_PIN_STRING_FNS 35
/* Map.prototype.get, set, has, Set.prototype.add, has (JS_CallMapFn). */
#define TSAOT_PIN_MAP_FNS 41
#define TSAOT_PINS 46
int tsaot_is_promise_resolve(JSValue promise, JSValue resolve);
int tsaot_gen_iter_ok(JSContext *ctx, JSValue iter, JSValue next);
JSValue tsaot_function_kind(JSContext *ctx, JSValue func, JSValueConst fallback, int kind);
int tsaot_array_iter_ok(JSContext *ctx);
extern JSValue tsaot_pins[];
/* The shapes of the pristine objects (up to TSAOT_SHAPE_ARRAY), then the
 * pointers of the other pins. */
extern void *tsaot_shapes[];

/* Inline caches, one per property access site in native code (defined by
 * it; absent without native code): of an own data property (kind 0), or of
 * a data property (1), getter (2) or setter (3) of the first or second
 * prototype, or of an own getter (4) or setter (5). Cached getters and
 * setters are read from the holder at each use (they can be removed
 * without a shape change). */
typedef struct TsaotIC {
    void *shape; /* kind 0: the object's shape (referenced), else NULL */
    uint32_t slot;
    uint32_t kind;
    void *proto_shape; /* kinds 1-3: the holder's shape (referenced) */
    void *mid_shape;   /* a holder two levels up: the first prototype's shape */
    /* Other kinds: the object's shape (referenced). Native code checks an
     * own data property (the usual hit) with one comparison. */
    void *alt_shape;
} TsaotIC;
#define TSAOT_IC_OWN 0
#define TSAOT_IC_PROTO_DATA 1
#define TSAOT_IC_PROTO_GETTER 2
#define TSAOT_IC_PROTO_SETTER 3
#define TSAOT_IC_OWN_GETTER 4
#define TSAOT_IC_OWN_SETTER 5
/* A field of an `aot type` object: `slot` holds the layout (low 16 bits)
 * and the field's offset (high 16 bits), `kind` bits 8-15 the field and
 * bits 16-17 its storage. */
#define TSAOT_IC_STRUCT 7
/* A data property of String.prototype read from a string: `shape` is NULL,
 * `proto_shape` String.prototype's shape. */
#define TSAOT_IC_STRING 8
extern TsaotIC tsaot_ics[] __attribute__((weak));
extern const int32_t tsaot_nics __attribute__((weak));
/* Per native function: the shape of its closures (referenced) and its name
 * as an atom, both made with its first closure. */
typedef struct TsaotFnCache {
    void *shape;
    JSAtom name;
} TsaotFnCache;
extern TsaotFnCache tsaot_fn_caches[] __attribute__((weak));
extern const int32_t tsaot_nfn_caches __attribute__((weak));
JSValue tsaot_new_closure(JSContext *ctx, TsaotFnCache *c, JSCFunctionFast *fast, JSCFunctionData *slow,
                          const char *name, int length, int magic, int n, JSValue *data);
JSValue tsaot_ic_get(JSContext *ctx, JSValue obj, JSAtom prop, TsaotIC *ic);
JSValue tsaot_ic_own_data(JSContext *ctx, JSValue obj, JSAtom prop, TsaotIC *ic);
void tsaot_record_learn(JSContext *ctx, JSValue obj, int r);
int tsaot_char_code_at_ok(JSContext *ctx);
int tsaot_array_push_ok(JSContext *ctx);
int tsaot_region_push(JSContext *ctx, JSValue arr, JSValue v);
int tsaot_ic_set(JSContext *ctx, JSValue obj, JSAtom prop, JSValue val, TsaotIC *ic);
JSValue tsaot_private_get_ic(JSContext *ctx, JSValue obj, JSValue name, TsaotIC *ic);
int tsaot_private_set_ic(JSContext *ctx, JSValue *v, JSValue val, TsaotIC *ic);
JSValue tsaot_ic_get_proto(JSContext *ctx, JSValue obj, JSAtom prop, TsaotIC *ic);
int tsaot_ic_set_proto(JSContext *ctx, JSValue obj, JSAtom prop, JSValue val, TsaotIC *ic);
JSValue tsaot_call_getter(JSContext *ctx, void *getter, JSValue this_obj);
JSValue tsaot_super_get_ic(JSContext *ctx, JSValue home, JSValue this_obj, JSAtom prop, TsaotIC *ic);
int tsaot_call_setter(JSContext *ctx, void *setter, JSValue this_obj, JSValue val);
int tsaot_define_accessor(JSContext *ctx, JSValue *obj, JSAtom prop, JSValue fn, int is_setter);
JSValue tsaot_new_object(JSContext *ctx, TsaotIC *ic, int n, JSAtom *atoms, JSValue *values);
JSValue tsaot_new_instance(JSContext *ctx, TsaotIC *ic, JSValue proto, int n, JSAtom *atoms, JSValue *values);
JSValue tsaot_global_get(JSContext *ctx, JSAtom prop, TsaotIC *ic);

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
JSValue tsaot_async_gen_new(JSContext *ctx, JSValue frame, JSValueConst *func_data);
JSValue tsaot_async_yield_star(JSContext *ctx, JSValue *v, int mode, int *kind);
JSValue tsaot_async_yield_star_result(JSContext *ctx, JSValue r, int *done);
int tsaot_throw_iterator_throw(JSContext *ctx);
JSValue tsaot_for_await_result(JSContext *ctx, JSValue r, int *done);
int tsaot_check_object(JSContext *ctx, JSValue v);
void tsaot_iter_close_quiet(JSContext *ctx, JSValue iter);

/* Called once while the context is created. */
int tsaot_lock_builtins(JSContext *ctx);
int tsaot_runtime_init(JSContext *ctx, JSValue *state, JSValue *init_fn, JSValue *cells_obj);

#endif

/* ---- `aot type` objects (see tsaot-front's `structs`) ---- */

/* Field storage, as ir::Storage. */
#define TSAOT_ST_INT32 0
#define TSAOT_ST_F64 1
#define TSAOT_ST_BOOL 2
#define TSAOT_ST_VALUE 3
/* Field types in a member, as ir::FieldType. */
#define TSAOT_FT_ABSENT 0
#define TSAOT_FT_INT 1
#define TSAOT_FT_NUMBER 2
#define TSAOT_FT_BOOL 3
#define TSAOT_FT_STRING 4
#define TSAOT_FT_LITERALS 5
#define TSAOT_FT_STRUCT 6

typedef struct TsaotFieldDesc {
    const char *name;
    uint8_t storage;
    uint8_t simple; /* same type in every member, present in all */
    uint16_t offset;
} TsaotFieldDesc;

typedef struct TsaotTypeDesc {
    uint8_t kind;
    uint8_t nlits;
    uint16_t layout;   /* TSAOT_FT_STRUCT */
    uint32_t lit_base; /* TSAOT_FT_LITERALS: first in tsaot_struct_lits */
} TsaotTypeDesc;

typedef struct TsaotMemberDesc {
    uint32_t mask;
    const TsaotTypeDesc *types; /* per field */
} TsaotMemberDesc;

typedef struct TsaotLayoutDesc {
    const char *name;
    uint32_t nfields;
    uint32_t size;
    uint32_t nmembers;
    uint32_t validate; /* writes or literals may change the member matched */
    const TsaotFieldDesc *fields;
    const TsaotMemberDesc *members;
} TsaotLayoutDesc;

/* An object's data (its opaque): the layout, the fields present, then the
 * fields at their offsets. */
typedef struct TsaotStruct {
    uint32_t layout;
    uint32_t mask;
    uint64_t data[];
} TsaotStruct;

extern const TsaotLayoutDesc tsaot_layouts[] __attribute__((weak));
extern const int32_t tsaot_nlayouts __attribute__((weak));
extern const char *const tsaot_struct_lits[] __attribute__((weak));
extern const int32_t tsaot_nstruct_lits __attribute__((weak));
extern uint32_t tsaot_struct_class;

JSValue tsaot_struct_new(JSContext *ctx, int layout, uint32_t mask);
int tsaot_struct_put(JSContext *ctx, JSValue obj, int field, JSValue v);
int tsaot_struct_validate(JSContext *ctx, JSValue obj);
int tsaot_struct_set(JSContext *ctx, JSValue obj, int field, JSValue v);
JSValue tsaot_struct_from(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv);
extern int tsaot_int_wrapping __attribute__((weak));
JSValue tsaot_struct_coerce(JSContext *ctx, int layout, int field, JSValue v);

/* An array of objects of one `aot type` layout (its opaque): elements are
 * objects of that layout or `undefined` (holes). */
typedef struct TsaotStructArray {
    uint32_t layout;
    uint32_t len;
    uint32_t cap;
    uint32_t pad;
    JSValue *items;
} TsaotStructArray;
extern uint32_t tsaot_sarr_class;
JSValue tsaot_sarr_from(JSContext *ctx, int layout, JSValue src);
int tsaot_sarr_set(JSContext *ctx, JSValue obj, int64_t idx, JSValue v);
int64_t tsaot_sarr_push(JSContext *ctx, JSValue obj, JSValue v);
JSValue tsaot_sarr_js(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv);
