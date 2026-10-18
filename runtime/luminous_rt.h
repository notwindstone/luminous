/* luminous runtime: helpers for Cranelift-compiled code and the generated glue. */
#ifndef LUMINOUS_RT_H
#define LUMINOUS_RT_H

#include <stddef.h>
#include <stdint.h>
#include "quickjs.h"

/* Native stack budget below the frame that initializes the runtime (must
 * match luminous_codegen::NATIVE_STACK_BUDGET; the glue asserts it). QuickJS
 * keeps its own usage under JS_DEFAULT_STACK_SIZE, so the two together stay
 * well within a default 8 MiB thread stack. */
#define LUM_NATIVE_STACK (4u * 1024 * 1024)

/* Checked on every native function entry. */
extern uintptr_t lum_stack_limit;
/* Set while an exception propagates out of native functions. */
extern uint8_t lum_unwinding;

/* Per-program tables, defined by the generated glue. */
extern JSValue lum_cells[];
extern JSValue lum_strings[];
extern JSAtom lum_atoms[];
/* Record types (see ir::specialize_fields): per record, the shapes of
 * its objects once learned (LUM_RECORD_SHAPES of them, the first one
 * learned first; native code reads all of them, regions only the first),
 * then how many times learning failed (defined by the native code); and
 * its fields (count, then atom indices, from lum_record_index[r]). */
extern void *lum_record_shapes[] __attribute__((weak));
#define LUM_RECORD_SHAPES 4
/* Words per record in lum_record_shapes (lower::RECORD_STRIDE). */
#define LUM_RECORD_STRIDE (LUM_RECORD_SHAPES + 1)
/* Failed learning stops after this many tries. */
#define LUM_RECORD_TRIES 64
extern const int32_t lum_record_fields[] __attribute__((weak));
extern const int32_t lum_record_index[] __attribute__((weak));
extern const int32_t lum_nrecords __attribute__((weak));
extern const char *const lum_cell_names[];
extern const char *const lum_atom_names[];
extern const char *const lum_string_data[];
extern const size_t lum_string_lens[];
extern const int lum_mutable_cells[];
extern const unsigned char lum_mutable_const[];
extern const int lum_ncells, lum_nstrings, lum_natoms, lum_nmutable;
/* Private name values and home objects of classes (filled by the classes
 * as they are defined). */
extern JSValue lum_privates[];
extern JSValue lum_homes[];
extern const int lum_nprivates, lum_nhomes;

/* Called from native code. */
double lum_pow(double a, double b);
double lum_round(double a);
int32_t lum_toint32_slow(double d);
void lum_stack_overflow(JSContext *ctx);
void lum_throw_uninitialized(JSContext *ctx, int cell);
int lum_set_elem(JSContext *ctx, JSValue *v);
int lum_delete_elem(JSContext *ctx, JSValue *v);
JSValue lum_call(JSContext *ctx, JSValue *fthis, int argc, JSValue *argv);
JSValue lum_global_or_undefined(JSContext *ctx, JSAtom name);
int lum_define_field(JSContext *ctx, JSValue *obj, JSAtom prop, JSValue val);
JSValue lum_new_array(JSContext *ctx, int count, JSValue *values);
JSValue lum_box_new(JSContext *ctx, JSValue **slot);
void lum_throw_uninitialized_atom(JSContext *ctx, JSAtom name);
/* Defined by the generated glue. */
JSValue lum_make_closure(JSContext *ctx, int fn, int n, JSValue *data);
/* Step functions of async functions (NULL for others), by function index. */
typedef JSValue LumStep(JSContext *ctx, JSValue *slots, int state, JSValue *value, int is_throw);
extern LumStep *const lum_steps[];
JSValue lum_async_start(JSContext *ctx, int fn, int nslots, JSValue *promise, JSValue **slots);
void lum_async_step(JSContext *ctx, JSValue *slots, int state, JSValue *value, int is_throw);
JSValue lum_async_settled(JSContext *ctx, JSValue v);
JSValue lum_await(JSContext *ctx, JSValue *slots, JSValue value, int state);

/* Pristine objects and their shapes: typed arrays by JSTypedArrayEnum,
 * then an array; then the original array iteration objects. */
#define LUM_TYPED_ARRAYS 12
#define LUM_SHAPE_ARRAY 12
#define LUM_PIN_ARRAY_PROTO 13
#define LUM_PIN_ARRAY_VALUES 14
#define LUM_PIN_ARRAY_ITERATOR_PROTO 15
#define LUM_PIN_ARRAY_ITERATOR_NEXT 16
#define LUM_PIN_SYMBOL_ITERATOR 17
#define LUM_PIN_ASYNC_FUNCTION_PROTO 18
#define LUM_PIN_GENERATOR_FUNCTION_PROTO 19
#define LUM_PIN_GENERATOR_NEXT 20
#define LUM_PIN_ASYNC_GENERATOR_FUNCTION_PROTO 21
#define LUM_PIN_PROMISE 22
#define LUM_PIN_PROMISE_RESOLVE 23
#define LUM_PIN_STRING_PROTO 24
#define LUM_PIN_CHAR_CODE_AT 25
#define LUM_PIN_ARRAY_PUSH 26
/* Array.prototype.forEach, map, filter, some, every, find, findIndex
 * (native loops). */
#define LUM_PIN_ARRAY_LOOPS 27
#define LUM_PIN_ARRAY_SORT 34
/* String.prototype.slice, substring, indexOf, includes, startsWith,
 * endsWith (JS_CallStringFn). */
#define LUM_PIN_STRING_FNS 35
/* Map.prototype.get, set, has, Set.prototype.add, has (JS_CallMapFn). */
#define LUM_PIN_MAP_FNS 41
/* JSON.stringify, JSON.parse. */
#define LUM_PIN_JSON_STRINGIFY 46
#define LUM_PIN_JSON_PARSE 47
#define LUM_PINS 48
JSValue lum_struct_stringify(JSContext *ctx, JSValueConst v);
int lum_is_promise_resolve(JSValue promise, JSValue resolve);
int lum_gen_iter_ok(JSContext *ctx, JSValue iter, JSValue next);
JSValue lum_function_kind(JSContext *ctx, JSValue func, JSValueConst fallback, int kind);
int lum_array_iter_ok(JSContext *ctx);
JSValue lum_regexp(JSContext *ctx, int pattern, int flags);
extern JSValue lum_pins[];
/* The shapes of the pristine objects (up to LUM_SHAPE_ARRAY), then the
 * pointers of the other pins. */
extern void *lum_shapes[];

/* Inline caches, one per property access site in native code (defined by
 * it; absent without native code): of an own data property (kind 0), or of
 * a data property (1), getter (2) or setter (3) of the first or second
 * prototype, or of an own getter (4) or setter (5). Cached getters and
 * setters are read from the holder at each use (they can be removed
 * without a shape change). */
typedef struct LumIC {
    void *shape; /* kind 0: the object's shape (referenced), else NULL */
    uint32_t slot;
    uint32_t kind;
    void *proto_shape; /* kinds 1-3: the holder's shape (referenced) */
    void *mid_shape;   /* a holder two levels up: the first prototype's shape */
    /* Other kinds: the object's shape (referenced). Native code checks an
     * own data property (the usual hit) with one comparison. */
    void *alt_shape;
    union {
        /* A polymorphic site's other entries (own and prototype data
         * properties of other shapes), which the misses try before a
         * lookup: up to LUM_POLY of them (an empty one has no shapes),
         * or NULL. */
        struct LumIC *poly;
        /* Such an entry of a data property three prototypes up: the second
         * prototype's shape (referenced), checked after mid_shape. */
        void *mid2_shape;
    };
    /* Kinds 1-3: what the holder held when cached (the data property's
     * value, or the getter or setter object), and js_proto_epoch then
     * (else 0). The prototypes up to the holder are watched
     * (JS_WatchPrototype): while the counter is the same, an object of
     * alt_shape has this, and native code checks nothing else. */
    uint64_t epoch;
    JSValue value;
    /* That value's JS entry if it is a native function (JSCFunctionFast),
     * else NULL: what native code checks a predicted call by. */
    void *entry;
    /* How many times the entry was made again after a change to a watched
     * prototype (lum_ic_refresh): past LUM_IC_REFRESHES, it keeps no epoch
     * (a program that keeps changing its prototypes). */
    uint32_t refreshes;
} LumIC;
#define LUM_IC_REFRESHES 64
void lum_ic_refresh(LumIC *ic, void *p);
#define LUM_POLY 4
#define LUM_IC_OWN 0
#define LUM_IC_PROTO_DATA 1
#define LUM_IC_PROTO_GETTER 2
#define LUM_IC_PROTO_SETTER 3
#define LUM_IC_OWN_GETTER 4
#define LUM_IC_OWN_SETTER 5
/* A field of an `aot type` object: `slot` holds the layout (low 16 bits)
 * and the field's offset (high 16 bits), `kind` bits 8-15 the field and
 * bits 16-17 its storage. */
#define LUM_IC_STRUCT 7
/* A data property of String.prototype read from a string: `shape` is NULL,
 * `proto_shape` String.prototype's shape. */
#define LUM_IC_STRING 8
/* `instanceof`'s right operand: `alt_shape` is its shape, `kind` holds its
 * class in bits 16-31, `slot` is its "prototype" (see lum_instanceof). */
#define LUM_IC_INSTANCEOF 9
/* An addition of the property at a write site, in a polymorphic entry:
 * `alt_shape` the object's shape before, `proto_shape` after (both
 * referenced), valid while js_proto_epoch is `epoch` (see
 * JS_CacheAddProperty). */
#define LUM_IC_ADD 10
/* A property objects of a shape do not have, nor their prototypes, in a
 * polymorphic entry: `alt_shape` (referenced), valid while js_proto_epoch is
 * `epoch` (see JS_CacheAbsentProperty). */
#define LUM_IC_ABSENT 11
extern LumIC lum_ics[] __attribute__((weak));
extern const int32_t lum_nics __attribute__((weak));
/* Per native function: the shape of its closures (referenced) and its name
 * as an atom, both made with its first closure; then, if native code makes
 * the next ones inline (JS_CFunctionDataTemplate), their cell class and
 * size in `info` (else 0) and what each starts with. */
typedef struct LumFnCache {
    void *shape;
    JSAtom name;
    uint32_t info;
    JSFunctionTemplate tmpl;
} LumFnCache;
extern LumFnCache lum_fn_caches[] __attribute__((weak));
extern const int32_t lum_nfn_caches __attribute__((weak));
JSValue lum_new_closure(JSContext *ctx, LumFnCache *c, JSCFunctionFast *fast, JSCFunctionData *slow,
                          const char *name, int length, int magic, int n, JSValue *data, int kind);
JSValue lum_ic_get(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic);
int lum_instanceof(JSContext *ctx, JSValue v, JSValue ctor, LumIC *ic);
JSValue lum_ic_own_data(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic);
void lum_record_learn(JSContext *ctx, JSValue obj, int r);
int lum_char_code_at_ok(JSContext *ctx);
int lum_array_push_ok(JSContext *ctx);
int lum_region_push(JSContext *ctx, JSValue arr, JSValue v);
int lum_ic_set(JSContext *ctx, JSValue obj, JSAtom prop, JSValue val, LumIC *ic);
JSValue lum_private_get_ic(JSContext *ctx, JSValue obj, JSValue name, LumIC *ic);
int lum_private_set_ic(JSContext *ctx, JSValue *v, JSValue val, LumIC *ic);
JSValue lum_ic_get_proto(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic);
JSValue lum_ic_get_cached(JSContext *ctx, JSValue obj, JSAtom prop, LumIC *ic);
int lum_ic_set_proto(JSContext *ctx, JSValue obj, JSAtom prop, JSValue val, LumIC *ic);
JSValue lum_call_getter(JSContext *ctx, void *getter, JSValue this_obj);
JSValue lum_super_get_ic(JSContext *ctx, JSValue home, JSValue this_obj, JSAtom prop, LumIC *ic);
int lum_call_setter(JSContext *ctx, void *setter, JSValue this_obj, JSValue val);
int lum_define_accessor(JSContext *ctx, JSValue *obj, JSAtom prop, JSValue fn, int is_setter);
JSValue lum_new_object(JSContext *ctx, LumIC *ic, int n, JSAtom *atoms, JSValue *values);
JSValue lum_new_instance(JSContext *ctx, LumIC *ic, JSValue proto, int n, JSAtom *atoms, JSValue *values);
JSValue lum_global_get(JSContext *ctx, JSAtom prop, LumIC *ic);

/* Spread, rest and iteration. */
int lum_array_push(JSContext *ctx, JSValue arr, JSValue v);
int lum_array_hole(JSContext *ctx, JSValue arr);
int lum_array_spread(JSContext *ctx, JSValue arr, JSValue iterable);
JSValue lum_apply(JSContext *ctx, JSValue *v);
JSValue lum_construct(JSContext *ctx, JSValue f, JSValue args);
JSValue lum_rest(JSContext *ctx, int argc, JSValue *argv, int start);
JSValue lum_arguments(JSContext *ctx, int argc, JSValue *argv, int unused);
JSValue lum_super_construct(JSContext *ctx, int argc, JSValue *v);
JSValue lum_super_construct_array(JSContext *ctx, JSValue *v);
JSValue lum_class_setup(JSContext *ctx, JSValue *v, int has_parent);
int lum_define_method(JSContext *ctx, JSValue *v, JSAtom key, int kind);
int lum_define_method_computed(JSContext *ctx, JSValue *v, int kind);
JSValue lum_new_from_target(JSContext *ctx, JSValue new_target);
JSValue lum_object_rest(JSContext *ctx, JSValue *v, int nkeys);
int lum_define_computed(JSContext *ctx, JSValue *v, JSValue val);
int lum_object_spread(JSContext *ctx, JSValue obj, JSValue source);
int lum_array_iter_close(JSContext *ctx, JSValue arr, int32_t idx, int is_throw);
JSValue lum_super_get(JSContext *ctx, JSValue *v);
JSValue lum_for_in_get(JSContext *ctx, JSValue *v);
int lum_private_set(JSContext *ctx, JSValue *v, JSValue val);
int lum_private_define(JSContext *ctx, JSValue *v, JSValue val);
JSValue lum_new_instance_full(JSContext *ctx, LumIC *ic, JSValue proto, int n, JSAtom *atoms, JSValue *values,
                              JSValue *extra, int nprivate);
int lum_super_set(JSContext *ctx, JSValue *v, JSValue val);
JSValue lum_gen_start(JSContext *ctx, int fn, int nslots, JSValue **slots);
JSValue lum_gen_new(JSContext *ctx, JSValue frame, JSValueConst *func_data);
JSValue lum_yield_star(JSContext *ctx, JSValue *v, int mode, int *kind);
JSValue lum_async_gen_new(JSContext *ctx, JSValue frame, JSValueConst *func_data);
JSValue lum_async_yield_star(JSContext *ctx, JSValue *v, int mode, int *kind);
JSValue lum_async_yield_star_result(JSContext *ctx, JSValue r, int *done);
int lum_throw_iterator_throw(JSContext *ctx);
JSValue lum_for_await_result(JSContext *ctx, JSValue r, int *done);
int lum_check_object(JSContext *ctx, JSValue v);
void lum_iter_close_quiet(JSContext *ctx, JSValue iter);

/* Called once while the context is created. */
int lum_lock_builtins(JSContext *ctx);
int lum_runtime_init(JSContext *ctx, JSValue *state, JSValue *init_fn, JSValue *cells_obj);

#endif

/* ---- `aot type` objects (see luminous-front's `structs`) ---- */

/* Field storage, as ir::Storage. */
#define LUM_ST_INT32 0
#define LUM_ST_F64 1
#define LUM_ST_BOOL 2
#define LUM_ST_VALUE 3
/* Field types in a member, as ir::FieldType. */
#define LUM_FT_ABSENT 0
#define LUM_FT_INT 1
#define LUM_FT_NUMBER 2
#define LUM_FT_BOOL 3
#define LUM_FT_STRING 4
#define LUM_FT_LITERALS 5
#define LUM_FT_STRUCT 6
#define LUM_FT_ARRAY 7 /* an array of objects of `layout` (LumStructArray) */

typedef struct LumFieldDesc {
    const char *name;
    uint8_t storage;
    uint8_t simple; /* same type in every member, present in all */
    uint16_t offset;
} LumFieldDesc;

typedef struct LumTypeDesc {
    uint8_t kind;
    uint8_t nullable;  /* or `null` */
    uint16_t layout;   /* LUM_FT_STRUCT, LUM_FT_ARRAY */
    uint16_t nlits;
    uint32_t lit_base; /* LUM_FT_LITERALS: first in lum_struct_lits */
} LumTypeDesc;

typedef struct LumMemberDesc {
    uint32_t mask;
    uint32_t optional; /* fields that may be absent */
    const LumTypeDesc *types; /* per field */
} LumMemberDesc;

typedef struct LumLayoutDesc {
    const char *name;
    uint32_t nfields;
    uint32_t size;
    uint32_t nmembers;
    uint32_t validate; /* writes or literals may change the member matched */
    uint32_t optional; /* fields optional in some member: may be added, deleted */
    const LumFieldDesc *fields;
    const LumMemberDesc *members;
} LumLayoutDesc;

/* An object's data (its opaque): the layout, the fields present, then the
 * fields at their offsets. */
typedef struct LumStruct {
    uint32_t layout;
    uint32_t mask;
    uint64_t data[];
} LumStruct;

extern const LumLayoutDesc lum_layouts[] __attribute__((weak));
extern const int32_t lum_nlayouts __attribute__((weak));
/* Per layout: the function that writes its objects' JSON (generated with
 * the layouts; NULL when a key needs escapes). */
extern JSJSONRecordFunc *const lum_layout_json[] __attribute__((weak));
/* The JSON of an object of an `aot type` (`s`) the general way, from field
 * `from` on (`first`: no field written yet; `from` 0: from its `{`). */
int lum_struct_json_from(JSContext *ctx, JSJSONWriter *w, const LumStruct *s, int depth, uint32_t from, int first);
/* Whether a field holding v is written by JSON.stringify (not undefined, a
 * symbol or a function). */
static inline int lum_json_present(JSContext *ctx, JSValueConst v)
{
    int tag = JS_VALUE_GET_NORM_TAG(v);
    return tag != JS_TAG_UNDEFINED && tag != JS_TAG_SYMBOL && !(tag == JS_TAG_OBJECT && JS_IsFunction(ctx, v));
}
extern const char *const lum_struct_lits[] __attribute__((weak));
extern const int32_t lum_nstruct_lits __attribute__((weak));
extern uint32_t lum_struct_class;

JSValue lum_struct_new(JSContext *ctx, int layout, uint32_t mask);
int lum_struct_put(JSContext *ctx, JSValue obj, int field, JSValue v);
int lum_struct_validate(JSContext *ctx, JSValue obj);
int lum_struct_set(JSContext *ctx, JSValue obj, int field, JSValue v);
JSValue lum_struct_from(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv);
extern int lum_int_wrapping __attribute__((weak));
JSValue lum_struct_coerce(JSContext *ctx, int layout, int field, JSValue v);

/* An array of objects of one `aot type` layout (its opaque): elements are
 * objects of that layout or `undefined` (holes). */
typedef struct LumStructArray {
    uint32_t layout;
    uint32_t len;
    uint32_t cap;
    uint32_t pad;
    JSValue *items;
} LumStructArray;
extern uint32_t lum_sarr_class;
JSValue lum_sarr_from(JSContext *ctx, int layout, JSValue src);
int lum_sarr_set(JSContext *ctx, JSValue obj, int64_t idx, JSValue v);
int64_t lum_sarr_push(JSContext *ctx, JSValue obj, JSValue v);
JSValue lum_sarr_js(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv);

/* `aot type`s from JSON (desc: layout * 2, + 1 for an array of its objects). */
JSValue lum_struct_convert(JSContext *ctx, JSValue v, int32_t desc);
JSValue lum_struct_as(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv);
JSValue lum_json_parse_as(JSContext *ctx, JSValueConst text, int32_t desc);
JSValue lum_json_parse_utf8_as(JSContext *ctx, const uint8_t *buf, size_t len, int32_t desc);
JSValue lum_json_method_as(JSContext *ctx, JSValueConst recv, int32_t desc);
void lum_set_json_hook(JSContext *ctx, JSValueConst fn);
