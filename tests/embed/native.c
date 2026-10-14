/* A native C module and the hooks of an embedder, for tests/embed.rs. */
#include "quickjs.h"
#include <stdio.h>
static int ticks;
static JSValue twice(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
  int32_t x; if (JS_ToInt32(ctx, &x, argv[0])) return JS_EXCEPTION;
  return JS_NewInt32(ctx, 2 * x);
}
static const JSCFunctionListEntry fns[] = { JS_CFUNC_DEF("twice", 1, twice) };
static int init(JSContext *ctx, JSModuleDef *m) { return JS_SetModuleExportList(ctx, m, fns, 1); }
JSModuleDef *js_init_module_demo(JSContext *ctx, const char *name) {
  JSModuleDef *m = JS_NewCModule(ctx, name, init);
  if (m) JS_AddModuleExportList(ctx, m, fns, 1);
  return m;
}
int demo_init(JSContext *ctx, int argc, char **argv) { printf("demo_init: %d args\n", argc); return 0; }
int demo_loop(JSContext *ctx) {
  /* Run the jobs (promises) until none are left. */
  JSContext *c;
  for (;;) { int r = JS_ExecutePendingJob(JS_GetRuntime(ctx), &c); if (r <= 0) { if (r < 0) return 1; break; } ticks++; }
  printf("demo_loop: ran %d jobs\n", ticks);
  return 0;
}
