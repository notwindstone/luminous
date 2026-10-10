/* tsaot runtime: helpers shared by Cranelift-compiled code and the generated
 * JS entry stubs. */
#ifndef TSAOT_RT_H
#define TSAOT_RT_H

#include <stdbool.h>
#include <stdint.h>
#include "quickjs.h"

/* Native stack budget for one entry from JS into native code. QuickJS keeps
 * its own usage under JS_DEFAULT_STACK_SIZE, so the two together stay well
 * within a default 8 MiB thread stack. */
#ifndef TSAOT_NATIVE_STACK
#define TSAOT_NATIVE_STACK (2u * 1024 * 1024)
#endif

extern uintptr_t tsaot_stack_limit;
extern uint8_t tsaot_unwinding;

/* Called from native code. */
double tsaot_pow(double a, double b);
double tsaot_round(double a);
int32_t tsaot_toint32_slow(double d);
void tsaot_stack_overflow(void);

/* Called once while the context is created. */
int tsaot_lock_builtins(JSContext *ctx);

/* Entry-stub helpers. */
static inline bool tsaot_arg_f64(JSValueConst v, double *out)
{
    int tag = JS_VALUE_GET_NORM_TAG(v);
    if (tag == JS_TAG_INT) {
        *out = JS_VALUE_GET_INT(v);
        return true;
    }
    if (tag == JS_TAG_FLOAT64) {
        *out = JS_VALUE_GET_FLOAT64(v);
        return true;
    }
    return false;
}

static inline bool tsaot_arg_bool(JSValueConst v, bool *out)
{
    if (JS_VALUE_GET_NORM_TAG(v) == JS_TAG_BOOL) {
        *out = JS_VALUE_GET_BOOL(v);
        return true;
    }
    return false;
}

static inline JSValue tsaot_ret_f64(JSContext *ctx, double d)
{
    /* Prefer the int representation, as the interpreter does, so values
     * flowing back into interpreted code stay on its fast paths. */
    if (d >= INT32_MIN && d <= INT32_MAX) {
        int32_t i = (int32_t)d;
        if ((double)i == d && !(i == 0 && __builtin_signbit(d)))
            return JS_NewInt32(ctx, i);
    }
    return JS_NewFloat64(ctx, d);
}

static inline void tsaot_enter(void)
{
    tsaot_stack_limit = (uintptr_t)__builtin_frame_address(0) - TSAOT_NATIVE_STACK;
}

/* True if native code unwound because of a stack overflow. */
static inline bool tsaot_leave(void)
{
    if (__builtin_expect(tsaot_unwinding, 0)) {
        tsaot_unwinding = 0;
        return true;
    }
    return false;
}

#endif
