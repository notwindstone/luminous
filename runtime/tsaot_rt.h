/* tsaot runtime: helpers for Cranelift-compiled code and the generated glue. */
#ifndef TSAOT_RT_H
#define TSAOT_RT_H

#include <stdint.h>
#include "quickjs.h"

/* Native stack budget for one entry from JS into native code (must match
 * tsaot_codegen::NATIVE_STACK_BUDGET; the glue asserts it). QuickJS keeps
 * its own usage under JS_DEFAULT_STACK_SIZE, so the two together stay well
 * within a default 8 MiB thread stack. */
#define TSAOT_NATIVE_STACK (2u * 1024 * 1024)

/* Set by each JS entry; checked on every native function entry. */
extern uintptr_t tsaot_stack_limit;
/* Set on native stack overflow while the native frames unwind. */
extern uint8_t tsaot_unwinding;

/* Called from native code. */
double tsaot_pow(double a, double b);
double tsaot_round(double a);
int32_t tsaot_toint32_slow(double d);
void tsaot_stack_overflow(void);
void tsaot_throw_stack_overflow(JSContext *ctx);

/* Called once while the context is created. */
int tsaot_lock_builtins(JSContext *ctx);

#endif
