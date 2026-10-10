#include <math.h>
#include <string.h>
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

void tsaot_stack_overflow(void)
{
    tsaot_unwinding = 1;
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
