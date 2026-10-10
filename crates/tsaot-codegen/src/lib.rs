//! Lowers typed IR to native code with Cranelift and emits a relocatable
//! object file.
//!
//! Every IR function `i` produces two symbols:
//! - `tsaot_fn_<i>(JSContext *ctx, params...)`: the function itself. `number`
//!   / `boolean` values are passed unboxed (`double` / zero-extended `uint8_t`),
//!   other values as `JSValue`s (two 64-bit words, owned by the callee).
//!   Native callers call this directly.
//! - `tsaot_fast_<i>`: its entry from JS, a QuickJS-NG `JSCFunctionFast` that
//!   the interpreter's call opcodes invoke directly on their stack slots. The
//!   type guard, unboxing and boxing are all inline machine code here.
//!
//! Exceptions propagate between native functions through the
//! `tsaot_unwinding` flag: a function that exits with an exception sets it,
//! and every caller checks it after each native call. Stack overflow is just
//! another exception (a `RangeError`, thrown when the stack pointer crosses
//! `tsaot_stack_limit`).

mod lower;

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{F64, I8, I32, I64};
use cranelift_codegen::ir::{AbiParam, Block, BlockArg, InstBuilder, MemFlagsData, Signature, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use tsaot_front::ir::{self, Expr, ExprKind, MathFn, Stmt, Ty};

use lower::{Env, Exit, Exits, Lower, V, clif_types, from_values};

pub fn symbol_name(id: ir::FuncId) -> String {
    format!("tsaot_fn_{id}")
}

pub fn fast_symbol_name(id: ir::FuncId) -> String {
    format!("tsaot_fast_{id}")
}

/// Native stack available below the point where the runtime is initialized.
/// QuickJS keeps its own usage under `JS_DEFAULT_STACK_SIZE` (1 MiB), so both
/// fit a default 8 MiB stack.
pub const NATIVE_STACK_BUDGET: u64 = 4 * 1024 * 1024;

/// QuickJS-NG `JSValue` layout on 64-bit targets without NaN boxing: a
/// 16-byte struct with the payload union at offset 0 and an `int64_t` tag at
/// offset 8. The generated C glue `_Static_assert`s all of these.
pub mod jsvalue {
    pub const SIZE: usize = 16;
    pub const TAG_OFFSET: i32 = 8;
    pub const TAG_STRING: i64 = -7;
    pub const TAG_OBJECT: i64 = -1;
    pub const TAG_INT: i64 = 0;
    pub const TAG_BOOL: i64 = 1;
    pub const TAG_NULL: i64 = 2;
    pub const TAG_UNDEFINED: i64 = 3;
    pub const TAG_UNINITIALIZED: i64 = 4;
    pub const TAG_EXCEPTION: i64 = 6;
    pub const TAG_FLOAT64: i64 = 8;
}

/// Engine memory layout read by inline fast paths: the `JS_LAYOUT_*`
/// constants of the engine patch, which the generated C glue
/// `_Static_assert`s against these values.
pub mod layout {
    pub const REF_COUNT: i32 = -4;
    pub const STRING_LEN: i32 = 0;
    pub const OBJ_CLASS_ID: i32 = 18;
    pub const OBJ_SHAPE: i32 = 24;
    pub const OBJ_PROP: i32 = 32;
    pub const OBJ_TYPED_ARRAY: i32 = 48;
    pub const OBJ_ARRAY_BUFFER: i32 = 48;
    pub const OBJ_ARRAY_VALUES: i32 = 56;
    pub const OBJ_ARRAY_COUNT: i32 = 64;
    pub const TA_BUFFER: i32 = 24;
    pub const ABUF_IMMUTABLE: i32 = 9;
    pub const CLASS_ARRAY: i64 = 2;
    pub const CLASS_TYPED_ARRAY: i64 = 22;

    pub const ALL: &[(&str, i64)] = &[
        ("REF_COUNT", REF_COUNT as i64),
        ("STRING_LEN", STRING_LEN as i64),
        ("OBJ_CLASS_ID", OBJ_CLASS_ID as i64),
        ("OBJ_SHAPE", OBJ_SHAPE as i64),
        ("OBJ_PROP", OBJ_PROP as i64),
        ("OBJ_TYPED_ARRAY", OBJ_TYPED_ARRAY as i64),
        ("OBJ_ARRAY_BUFFER", OBJ_ARRAY_BUFFER as i64),
        ("OBJ_ARRAY_VALUES", OBJ_ARRAY_VALUES as i64),
        ("OBJ_ARRAY_COUNT", OBJ_ARRAY_COUNT as i64),
        ("TA_BUFFER", TA_BUFFER as i64),
        ("ABUF_IMMUTABLE", ABUF_IMMUTABLE as i64),
        ("CLASS_ARRAY", CLASS_ARRAY),
        ("CLASS_TYPED_ARRAY", CLASS_TYPED_ARRAY),
    ];
}

pub struct Options {
    /// Cranelift `opt_level`: "none", "speed" or "speed_and_size".
    pub opt_level: String,
}

impl Default for Options {
    fn default() -> Self {
        Options { opt_level: "speed".into() }
    }
}

/// Compile `m` to an object file for the host.
pub fn compile(m: &ir::Module, opts: &Options) -> Result<Vec<u8>> {
    let mut flags = settings::builder();
    flags.set("opt_level", &opts.opt_level)?;
    flags.set("is_pic", "true")?;
    let isa = cranelift_native::builder()
        .map_err(|e| anyhow!("unsupported host: {e}"))?
        .finish(settings::Flags::new(flags))?;
    let builder = ObjectBuilder::new(isa, "tsaot", cranelift_module::default_libcall_names())?;
    let mut module = ObjectModule::new(builder);

    let mut cg = Codegen::new(&mut module, m)?;
    for (i, f) in m.funcs.iter().chain(&m.closures).enumerate() {
        cg.define(i, f).with_context(|| format!("compiling `{}`", f.name))?;
        cg.define_fast(i, f).with_context(|| format!("compiling the entry of `{}`", f.name))?;
    }
    cg.define_ics()?;
    Ok(module.finish().emit()?)
}

/// ABI parameters of a value of type `t`.
fn abi(t: Ty) -> Vec<AbiParam> {
    match t {
        Ty::Bool => vec![AbiParam::new(I8).uext()],
        t => clif_types(t).into_iter().map(AbiParam::new).collect(),
    }
}

/// External functions callable from generated code: engine API, runtime
/// helpers and libm. `P` stands for the pointer type.
const HELPERS: &[(&str, &[Arg], &[Arg])] = {
    use Arg::*;
    &[
        ("JS_DupValue", &[P, W, W], &[W, W]),
        ("JS_FreeValue", &[P, W, W], &[]),
        ("JS_BinaryOperator", &[P, Int, W, W, W, W], &[W, W]),
        ("JS_UnaryOperator", &[P, Int, W, W], &[W, W]),
        ("JS_ToBool", &[P, W, W], &[Int]),
        ("JS_ToString", &[P, W, W], &[W, W]),
        ("JS_GetProperty", &[P, W, W, Int], &[W, W]),
        ("JS_SetProperty", &[P, W, W, Int, W, W], &[Int]),
        ("JS_GetPropertyValue", &[P, W, W, W, W], &[W, W]),
        ("JS_DeleteProperty", &[P, W, W, Int, Int], &[Int]),
        ("JS_CallConstructor", &[P, W, W, Int, P], &[W, W]),
        ("JS_GetGlobalVariable", &[P, Int], &[W, W]),
        ("JS_SetGlobalVariable", &[P, Int, W, W], &[Int]),
        ("JS_NewObject", &[P], &[W, W]),
        ("JS_GetException", &[P], &[W, W]),
        ("JS_Throw", &[P, W, W], &[W, W]),
        ("tsaot_set_elem", &[P, P], &[Int]),
        ("tsaot_ic_get", &[P, W, W, Int, P], &[W, W]),
        ("tsaot_ic_set", &[P, W, W, Int, W, W, P], &[Int]),
        ("tsaot_delete_elem", &[P, P], &[Int]),
        ("tsaot_call", &[P, P, Int, P], &[W, W]),
        ("tsaot_global_or_undefined", &[P, Int], &[W, W]),
        ("tsaot_define_field", &[P, P, Int, W, W], &[Int]),
        ("tsaot_new_array", &[P, Int, P], &[W, W]),
        ("tsaot_throw_uninitialized", &[P, Int], &[]),
        ("tsaot_stack_overflow", &[P], &[]),
        ("tsaot_throw_uninitialized_atom", &[P, Int], &[]),
        ("tsaot_box_new", &[P, P], &[W, W]),
        ("tsaot_make_closure", &[P, Int, Int, P], &[W, W]),
        ("tsaot_async_start", &[P, Int, Int, P, P], &[W, W]),
        ("tsaot_async_step", &[P, P, Int, P, Int], &[]),
        ("tsaot_await", &[P, P, W, W, Int], &[Int]),
        ("tsaot_pow", &[D, D], &[D]),
        ("tsaot_toint32_slow", &[D], &[Int]),
        ("fmod", &[D, D], &[D]),
    ]
};

#[derive(Clone, Copy)]
enum Arg {
    /// Pointer.
    P,
    /// 64-bit word (half of a `JSValue`).
    W,
    /// C `int` / `JSAtom`.
    Int,
    /// `double`.
    D,
}

struct Codegen<'m> {
    module: &'m mut ObjectModule,
    funcs: Vec<FuncId>,
    fast_funcs: Vec<FuncId>,
    helpers: HashMap<&'static str, FuncId>,
    data: [DataId; 7],
    /// Inline caches allocated so far (`tsaot_ics` entries).
    ics: std::rc::Rc<std::cell::Cell<usize>>,
    closure_base: usize,
    fctx: FunctionBuilderContext,
}

impl<'m> Codegen<'m> {
    fn new(module: &'m mut ObjectModule, m: &ir::Module) -> Result<Self> {
        let ptr = module.target_config().pointer_type();
        let mut funcs = Vec::new();
        for (i, f) in m.funcs.iter().chain(&m.closures).enumerate() {
            let sig = native_signature(module, f);
            funcs.push(module.declare_function(&symbol_name(i), Linkage::Export, &sig)?);
        }
        let fast_sig = fast_signature(module);
        let mut fast_funcs = Vec::new();
        for i in 0..m.funcs.len() + m.closures.len() {
            fast_funcs.push(module.declare_function(&fast_symbol_name(i), Linkage::Export, &fast_sig)?);
        }
        let mut helpers = HashMap::new();
        let ty = |a: &Arg| match a {
            Arg::P => ptr,
            Arg::W => I64,
            Arg::Int => I32,
            Arg::D => F64,
        };
        for &(name, params, rets) in HELPERS {
            let mut sig = module.make_signature();
            sig.params.extend(params.iter().map(|a| AbiParam::new(ty(a))));
            sig.returns.extend(rets.iter().map(|a| AbiParam::new(ty(a))));
            helpers.insert(name, module.declare_function(name, Linkage::Import, &sig)?);
        }
        // libm functions used by Math intrinsics.
        for mf in all_math(m) {
            if let Some(sym) = mf.symbol()
                && !helpers.contains_key(sym)
            {
                let mut sig = module.make_signature();
                sig.params.extend(vec![AbiParam::new(F64); mf.arity()]);
                sig.returns.push(AbiParam::new(F64));
                helpers.insert(sym, module.declare_function(sym, Linkage::Import, &sig)?);
            }
        }
        let mut data = Vec::new();
        for name in
            ["tsaot_stack_limit", "tsaot_unwinding", "tsaot_cells", "tsaot_strings", "tsaot_atoms", "tsaot_ta_shapes"]
        {
            data.push(module.declare_data(name, Linkage::Import, true, false)?);
        }
        // Defined here, once all sites are known (`define_ics`).
        data.push(module.declare_data("tsaot_ics", Linkage::Export, true, false)?);
        Ok(Codegen {
            module,
            funcs,
            fast_funcs,
            helpers,
            data: data.try_into().unwrap(),
            ics: Default::default(),
            closure_base: m.funcs.len(),
            fctx: FunctionBuilderContext::new(),
        })
    }

    fn env(&mut self, func: &mut cranelift_codegen::ir::Function) -> Env {
        let helpers = self.helpers.iter().map(|(&n, &id)| (n, self.module.declare_func_in_func(id, func))).collect();
        let funcs = self.funcs.iter().map(|&id| self.module.declare_func_in_func(id, func)).collect();
        let [stack_limit, unwinding, cells, strings, atoms, ta_shapes, ics] =
            self.data.map(|d| self.module.declare_data_in_func(d, func));
        Env {
            helpers,
            funcs,
            stack_limit,
            unwinding,
            cells,
            strings,
            atoms,
            ta_shapes,
            ics,
            next_ic: self.ics.clone(),
            closure_base: self.closure_base,
        }
    }

    /// Defines the inline cache table (`TsaotIC tsaot_ics[]`, 16 bytes per
    /// entry, zeroed) and its length `tsaot_nics`.
    fn define_ics(&mut self) -> Result<()> {
        let n = self.ics.get();
        let mut table = cranelift_module::DataDescription::new();
        table.define_zeroinit(n.max(1) * 16);
        table.set_align(8);
        self.module.define_data(self.data[6], &table)?;
        let count = self.module.declare_data("tsaot_nics", Linkage::Export, false, false)?;
        let mut desc = cranelift_module::DataDescription::new();
        desc.define((n as i32).to_le_bytes().to_vec().into_boxed_slice());
        self.module.define_data(count, &desc)?;
        Ok(())
    }

    /// Defines `tsaot_fn_<index>`, the function itself, for native callers.
    fn define(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        if f.is_async {
            return self.define_step(index, f);
        }
        let ptr = self.module.target_config().pointer_type();
        let mut ctx = self.module.make_context();
        ctx.func.signature = native_signature(self.module, f);
        let env = self.env(&mut ctx.func);
        let mut fctx = std::mem::take(&mut self.fctx);
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let js_ctx = raw[0];
        let mut params = Vec::new();
        let data = f.is_closure.then(|| raw[1]);
        let mut k = if f.is_closure { 2 } else { 1 };
        let this = f.has_this.then(|| (raw[k], raw[k + 1]));
        if f.has_this {
            k += 2;
        }
        for &t in &f.params {
            let n = clif_types(t).len();
            params.push(from_values(t, &raw[k..k + n]));
            k += n;
        }
        let exits = Exits { ret: Exit::Return, unwind: Exit::Return };
        let mut l = Lower::new(b, env, js_ctx, ptr, f, &params, data, exits);
        l.this = this;
        l.stack_check();
        let mut b = l.body(&f.body);
        b.seal_all_blocks();
        b.finalize(self.module.target_config());
        self.module.define_function(self.funcs[index], &mut ctx).map_err(|e| anyhow!("{e:?}"))?;
        self.module.clear_context(&mut ctx);
        self.fctx = fctx;
        Ok(())
    }

    /// Defines the step function of async function `index` (as
    /// `tsaot_fn_<index>`): runs the body from the start (state 0) or from
    /// the await it suspended at, until it completes or suspends again.
    fn define_step(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        let ptr = self.module.target_config().pointer_type();
        let mut ctx = self.module.make_context();
        ctx.func.signature = native_signature(self.module, f);
        let env = self.env(&mut ctx.func);
        let mut fctx = std::mem::take(&mut self.fctx);
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let (js_ctx, slots, state, value, is_throw) = (raw[0], raw[1], raw[2], raw[3], raw[4]);
        let exits = Exits { ret: Exit::Return, unwind: Exit::Return };
        let mut l = Lower::new(b, env, js_ctx, ptr, f, &[], None, exits);
        l.stack_check();
        l.enter_async(slots, state, value, is_throw, f);
        let mut b = l.body(&f.body);
        b.seal_all_blocks();
        b.finalize(self.module.target_config());
        self.module.define_function(self.funcs[index], &mut ctx).map_err(|e| anyhow!("{e:?}"))?;
        self.module.clear_context(&mut ctx);
        self.fctx = fctx;
        Ok(())
    }

    /// The JS entry of async function `index`: type guards as for other
    /// functions, then a new frame holding the arguments (and captured boxes,
    /// and `this`), the first step, and the promise as the result.
    fn define_async_entry(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        let ptr = self.module.target_config().pointer_type();
        let mut ctx = self.module.make_context();
        ctx.func.signature = fast_signature(self.module);
        let env = self.env(&mut ctx.func);
        let mut fctx = std::mem::take(&mut self.fctx);
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let mem = MemFlagsData::trusted();
        let entry = b.create_block();
        let decline = b.create_block();
        b.set_cold_block(decline);
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let (js_ctx, this_val, argc, argv, data) = (raw[0], (raw[1], raw[2]), raw[3], raw[4], raw[5]);

        let required = f.params.iter().rposition(|&t| t != Ty::Any).map_or(0, |i| i + 1);
        if required > 0 {
            let enough = b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, argc, required as i64);
            let ok = b.create_block();
            b.ins().brif(enough, ok, &[], decline, &[]);
            b.switch_to_block(ok);
        }
        let mut params = vec![V::Void; f.params.len()];
        for (k, &t) in f.params.iter().enumerate() {
            if t != Ty::Any {
                params[k] = unbox_arg(&mut b, t, argv, k, decline);
            }
        }

        // The frame and the promise.
        let (layout, this_slot, nslots) = lower::frame_layout(f);
        let out = b.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            32,
            4,
        ));
        let promise_addr = b.ins().stack_addr(ptr, out, 0);
        let slots_addr = b.ins().stack_addr(ptr, out, 16);
        let fi = b.ins().iconst(I32, index as i64);
        let ns = b.ins().iconst(I32, nslots as i64);
        let start = b.ins().call(env.helpers["tsaot_async_start"], &[js_ctx, fi, ns, promise_addr, slots_addr]);
        let frame = b.inst_results(start).to_vec();
        let failed = b.ins().icmp_imm_s(IntCC::Equal, frame[1], jsvalue::TAG_EXCEPTION);
        let exception = b.create_block();
        let ok = b.create_block();
        b.set_cold_block(exception);
        b.ins().brif(failed, exception, &[], ok, &[]);
        b.switch_to_block(ok);
        let slots = b.ins().load(ptr, mem, slots_addr, 0);
        let dup = env.helpers["JS_DupValue"];
        let store = |b: &mut FunctionBuilder, s: usize, p: Value, t: Value| {
            let at = (s * jsvalue::SIZE) as i32;
            b.ins().store(mem, p, slots, at);
            b.ins().store(mem, t, slots, at + jsvalue::TAG_OFFSET);
        };
        let dup_value = |b: &mut FunctionBuilder, p: Value, t: Value| {
            let rc = b.ins().icmp_imm_s(IntCC::SignedLessThan, t, 0);
            let yes = b.create_block();
            let join = b.create_block();
            b.ins().brif(rc, yes, &[], join, &[]);
            b.switch_to_block(yes);
            b.ins().call(dup, &[js_ctx, p, t]);
            b.ins().jump(join, &[]);
            b.switch_to_block(join);
        };
        for (k, &t) in f.params.iter().enumerate() {
            let s = layout[k];
            let local_ty = f.locals[k];
            let (p, tag) = match (t, params[k]) {
                (Ty::Any | Ty::Arr(_), _) => {
                    let (p, tag) = load_arg(&mut b, argc, argv, k);
                    dup_value(&mut b, p, tag);
                    (p, tag)
                }
                (_, V::Num(d)) if local_ty == Ty::Any => box_number(&mut b, d),
                (_, V::Num(d)) => {
                    (b.ins().bitcast(I64, MemFlagsData::new(), d), b.ins().iconst(I64, jsvalue::TAG_FLOAT64))
                }
                (_, V::Bool(x)) => (b.ins().uextend(I64, x), b.ins().iconst(I64, jsvalue::TAG_BOOL)),
                _ => unreachable!(),
            };
            store(&mut b, s, p, tag);
        }
        if f.is_closure {
            // Captured boxes: new references for the frame.
            for (c, &l) in f.captures.iter().enumerate() {
                let at = (2 * c * jsvalue::SIZE) as i32;
                let p = b.ins().load(I64, mem, data, at);
                let t = b.ins().load(I64, mem, data, at + jsvalue::TAG_OFFSET);
                let sp = b.ins().load(I64, mem, data, at + jsvalue::SIZE as i32);
                dup_value(&mut b, p, t);
                store(&mut b, layout[l], p, t);
                let ft = b.ins().iconst(I64, jsvalue::TAG_FLOAT64);
                store(&mut b, layout[l] + 1, sp, ft);
            }
        }
        if let Some(s) = this_slot {
            dup_value(&mut b, this_val.0, this_val.1);
            store(&mut b, s, this_val.0, this_val.1);
        }
        // First step, then drop this entry's reference to the frame.
        let undef_slot = b.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            16,
            4,
        ));
        let zero = b.ins().iconst(I64, 0);
        let undef = b.ins().iconst(I64, jsvalue::TAG_UNDEFINED);
        b.ins().stack_store(ptr, zero, undef_slot, 0);
        b.ins().stack_store(ptr, undef, undef_slot, jsvalue::TAG_OFFSET);
        let vaddr = b.ins().stack_addr(ptr, undef_slot, 0);
        let st0 = b.ins().iconst(I32, 0);
        let no = b.ins().iconst(I32, 0);
        b.ins().call(env.helpers["tsaot_async_step"], &[js_ctx, slots, st0, vaddr, no]);
        b.ins().call(env.helpers["JS_FreeValue"], &[js_ctx, frame[0], frame[1]]);
        let pp = b.ins().load(I64, mem, promise_addr, 0);
        let pt = b.ins().load(I64, mem, promise_addr, jsvalue::TAG_OFFSET);
        b.ins().return_(&[pp, pt]);

        b.switch_to_block(exception);
        let z = b.ins().iconst(I64, 0);
        let t = b.ins().iconst(I64, jsvalue::TAG_EXCEPTION);
        b.ins().return_(&[z, t]);

        b.switch_to_block(decline);
        let z = b.ins().iconst(I64, 0);
        let t = b.ins().iconst(I64, jsvalue::TAG_UNINITIALIZED);
        b.ins().return_(&[z, t]);

        b.seal_all_blocks();
        b.finalize(self.module.target_config());
        self.module.define_function(self.fast_funcs[index], &mut ctx).map_err(|e| anyhow!("{e:?}"))?;
        self.module.clear_context(&mut ctx);
        self.fctx = fctx;
        Ok(())
    }

    /// Defines `tsaot_fast_<index>`, the JS-facing entry of native function
    /// `index`, with the engine's `JSCFunctionFast` signature:
    ///
    /// ```c
    /// JSValue tsaot_fast_i(JSContext *ctx, JSValueConst this_val, int argc,
    ///                      JSValueConst *argv, JSValueConst *func_data);
    /// ```
    ///
    /// The whole boundary is inline machine code: argument tags are checked
    /// and payloads unboxed straight from the interpreter's stack slots, the
    /// body runs (inlined, unless it is large), and the boxed result is
    /// returned in registers. Ill-typed `number` / `boolean` arguments return
    /// `JS_UNINITIALIZED`, which makes the engine call the interpreted
    /// fallback instead; an exception returns `JS_EXCEPTION`.
    fn define_fast(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        if f.is_async {
            return self.define_async_entry(index, f);
        }
        let ptr = self.module.target_config().pointer_type();
        let mut ctx = self.module.make_context();
        ctx.func.signature = fast_signature(self.module);
        let env = self.env(&mut ctx.func);
        let unwinding = env.unwinding;
        let callee = env.funcs[index];
        let mut fctx = std::mem::take(&mut self.fctx);
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let mem = MemFlagsData::trusted();

        let entry = b.create_block();
        let decline = b.create_block();
        let exception = b.create_block();
        let boxing = b.create_block();
        b.set_cold_block(decline);
        b.set_cold_block(exception);
        for t in clif_types(f.ret) {
            b.append_block_param(boxing, t);
        }
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let (js_ctx, argc, argv) = (raw[0], raw[3], raw[4]);
        let data = f.is_closure.then(|| raw[5]);
        let this = f.has_this.then(|| (raw[1], raw[2]));

        // Guards first (declining must not leak references): enough
        // arguments for every unboxed parameter, and their tags.
        let required = f.params.iter().rposition(|&t| t != Ty::Any).map_or(0, |i| i + 1);
        if required > 0 {
            let enough = b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, argc, required as i64);
            let ok = b.create_block();
            b.ins().brif(enough, ok, &[], decline, &[]);
            b.switch_to_block(ok);
        }
        let mut params = vec![V::Void; f.params.len()];
        for (k, &t) in f.params.iter().enumerate() {
            if t != Ty::Any {
                params[k] = unbox_arg(&mut b, t, argv, k, decline);
            }
        }
        // Then the JS-valued parameters: new references, `undefined` if absent.
        for (k, &t) in f.params.iter().enumerate() {
            if matches!(t, Ty::Any | Ty::Arr(_)) {
                let (p, tag) = load_arg(&mut b, argc, argv, k);
                let rc = b.ins().icmp_imm_s(IntCC::SignedLessThan, tag, 0);
                let dup = b.create_block();
                let join = b.create_block();
                b.ins().brif(rc, dup, &[], join, &[]);
                b.switch_to_block(dup);
                let dupf = env.helpers["JS_DupValue"];
                b.ins().call(dupf, &[js_ctx, p, tag]);
                b.ins().jump(join, &[]);
                b.switch_to_block(join);
                params[k] = V::Any(p, tag);
            }
        }

        let mut b = if ir_size(f) <= INLINE_LIMIT {
            // The body itself, with `return` jumping to the boxing code. It
            // needs no stack check: entries from JS are shallow.
            let exits = Exits { ret: Exit::Jump(boxing), unwind: Exit::Jump(exception) };
            let mut l = Lower::new(b, env, js_ctx, ptr, f, &params, data, exits);
            l.this = this;
            l.body(&f.body)
        } else {
            let mut args = vec![js_ctx];
            args.extend(data);
            if let Some((p, t)) = this {
                args.extend([p, t]);
            }
            for p in &params {
                args.extend(p.values());
            }
            let call = b.ins().call(callee, &args);
            let r: Vec<BlockArg> = b.inst_results(call).iter().map(|&v| BlockArg::Value(v)).collect();
            let flag_addr = b.ins().symbol_value(ptr, unwinding);
            let flag = b.ins().load(I8, mem, flag_addr, 0);
            b.ins().brif(flag, exception, &[], boxing, &r);
            b
        };

        b.switch_to_block(boxing);
        let rv = b.block_params(boxing).to_vec();
        let (payload, tag) = match f.ret {
            Ty::Void => (b.ins().iconst(I64, 0), b.ins().iconst(I64, jsvalue::TAG_UNDEFINED)),
            Ty::Bool => (b.ins().uextend(I64, rv[0]), b.ins().iconst(I64, jsvalue::TAG_BOOL)),
            Ty::F64 => box_number(&mut b, rv[0]),
            Ty::Any | Ty::Arr(_) => (rv[0], rv[1]),
            // The checker boxes these returns.
            Ty::MaybeF64 => unreachable!("MaybeF64 return type"),
        };
        // Returned in registers: the result never touches memory on its way
        // back into the interpreter.
        b.ins().return_(&[payload, tag]);

        b.switch_to_block(exception);
        let flag_addr = b.ins().symbol_value(ptr, unwinding);
        let zero8 = b.ins().iconst(I8, 0);
        b.ins().store(mem, zero8, flag_addr, 0);
        let payload = b.ins().iconst(I64, 0);
        let tag = b.ins().iconst(I64, jsvalue::TAG_EXCEPTION);
        b.ins().return_(&[payload, tag]);

        b.switch_to_block(decline);
        let payload = b.ins().iconst(I64, 0);
        let tag = b.ins().iconst(I64, jsvalue::TAG_UNINITIALIZED);
        b.ins().return_(&[payload, tag]);

        b.seal_all_blocks();
        b.finalize(self.module.target_config());
        self.module.define_function(self.fast_funcs[index], &mut ctx).map_err(|e| anyhow!("{e:?}"))?;
        self.module.clear_context(&mut ctx);
        self.fctx = fctx;
        Ok(())
    }
}

fn native_signature(module: &ObjectModule, f: &ir::Func) -> Signature {
    let ptr = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    if f.is_async {
        // The step function: (ctx, frame slots, state, value delivered on
        // resumption, whether to throw it) -> completion value, or
        // JS_UNINITIALIZED (suspended), or JS_EXCEPTION.
        sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(ptr),
            AbiParam::new(I32),
            AbiParam::new(ptr),
            AbiParam::new(I32),
        ]);
        sig.returns.extend([AbiParam::new(I64), AbiParam::new(I64)]);
        return sig;
    }
    sig.params.push(AbiParam::new(ptr));
    if f.is_closure {
        // The closure's data slots.
        sig.params.push(AbiParam::new(ptr));
    }
    if f.has_this {
        // The receiver (borrowed).
        sig.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
    }
    for &t in &f.params {
        sig.params.extend(abi(t));
    }
    sig.returns.extend(abi(f.ret));
    sig
}

fn fast_signature(module: &ObjectModule) -> Signature {
    let ptr = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    // (ctx, this_val [2 words], argc, argv, func_data)
    sig.params.extend([
        AbiParam::new(ptr),
        AbiParam::new(I64),
        AbiParam::new(I64),
        AbiParam::new(I32),
        AbiParam::new(ptr),
        AbiParam::new(ptr),
    ]);
    // A 16-byte JSValue struct is returned in two integer registers (rax:rdx on
    // x86-64 SysV, x0:x1 on AArch64), which is what two I64 returns lower to.
    sig.returns.extend([AbiParam::new(I64), AbiParam::new(I64)]);
    sig
}

/// Argument `k` from the interpreter's slots, borrowed; `undefined` if
/// `k >= argc`.
fn load_arg(b: &mut FunctionBuilder, argc: Value, argv: Value, k: usize) -> (Value, Value) {
    let mem = MemFlagsData::trusted();
    let present = b.ins().icmp_imm_s(IntCC::SignedGreaterThan, argc, k as i64);
    let yes = b.create_block();
    let join = b.create_block();
    let p = b.append_block_param(join, I64);
    let t = b.append_block_param(join, I64);
    let zero = b.ins().iconst(I64, 0);
    let undef = b.ins().iconst(I64, jsvalue::TAG_UNDEFINED);
    b.ins().brif(present, yes, &[], join, &[BlockArg::Value(zero), BlockArg::Value(undef)]);
    b.switch_to_block(yes);
    let at = (k * jsvalue::SIZE) as i32;
    let lp = b.ins().load(I64, mem, argv, at);
    let lt = b.ins().load(I64, mem, argv, at + jsvalue::TAG_OFFSET);
    b.ins().jump(join, &[BlockArg::Value(lp), BlockArg::Value(lt)]);
    b.switch_to_block(join);
    (p, t)
}

/// Reads an unboxed argument straight from the interpreter's `JSValue`
/// slot, branching to `decline` on a type mismatch.
fn unbox_arg(b: &mut FunctionBuilder, t: Ty, argv: Value, k: usize, decline: Block) -> V {
    let mem = MemFlagsData::trusted();
    let at = (k * jsvalue::SIZE) as i32;
    let tag = b.ins().load(I64, mem, argv, at + jsvalue::TAG_OFFSET);
    match t {
        Ty::F64 => {
            let is_int = b.ins().icmp_imm_s(IntCC::Equal, tag, jsvalue::TAG_INT);
            let int_blk = b.create_block();
            let not_int = b.create_block();
            let float_blk = b.create_block();
            let join = b.create_block();
            let v = b.append_block_param(join, F64);
            b.ins().brif(is_int, int_blk, &[], not_int, &[]);
            b.switch_to_block(int_blk);
            let i = b.ins().load(I32, mem, argv, at);
            let d = b.ins().fcvt_from_sint(F64, i);
            b.ins().jump(join, &[BlockArg::Value(d)]);
            b.switch_to_block(not_int);
            let is_float = b.ins().icmp_imm_s(IntCC::Equal, tag, jsvalue::TAG_FLOAT64);
            b.ins().brif(is_float, float_blk, &[], decline, &[]);
            b.switch_to_block(float_blk);
            let d = b.ins().load(F64, mem, argv, at);
            b.ins().jump(join, &[BlockArg::Value(d)]);
            b.switch_to_block(join);
            V::Num(v)
        }
        Ty::Bool => {
            let is_bool = b.ins().icmp_imm_s(IntCC::Equal, tag, jsvalue::TAG_BOOL);
            let ok = b.create_block();
            b.ins().brif(is_bool, ok, &[], decline, &[]);
            b.switch_to_block(ok);
            let i = b.ins().load(I32, mem, argv, at);
            V::Bool(b.ins().icmp_imm_s(IntCC::NotEqual, i, 0))
        }
        Ty::Arr(kind) => {
            // Only checked: the entry passes it on as a JS value.
            let is_obj = b.ins().icmp_imm_s(IntCC::Equal, tag, jsvalue::TAG_OBJECT);
            let obj = b.create_block();
            let ok = b.create_block();
            b.ins().brif(is_obj, obj, &[], decline, &[]);
            b.switch_to_block(obj);
            let p = b.ins().load(I64, mem, argv, at);
            let class = b.ins().uload16(I32, mem, p, layout::OBJ_CLASS_ID);
            let want = layout::CLASS_TYPED_ARRAY + kind as i64;
            let is_kind = b.ins().icmp_imm_s(IntCC::Equal, class, want);
            b.ins().brif(is_kind, ok, &[], decline, &[]);
            b.switch_to_block(ok);
            V::Void
        }
        _ => unreachable!(),
    }
}

/// `Lower::box_number` for code outside a lowered body.
fn box_number(b: &mut FunctionBuilder, d: Value) -> (Value, Value) {
    let u = b.ins().bitcast(I64, MemFlagsData::new(), d);
    let e = b.ins().ushr_imm_u(u, 52);
    let e = b.ins().band_imm_u(e, 0x7ff);
    let e = b.ins().iadd_imm_s(e, -1023);
    let small = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, e, 30);
    let frac = b.ins().ishl_imm_u(u, 12);
    let frac = b.ins().ishl(frac, e);
    let small_int = b.ins().icmp_imm_s(IntCC::Equal, frac, 0);
    let is_zero = b.ins().icmp_imm_s(IntCC::Equal, u, 0);
    let is_min = b.ins().icmp_imm_s(IntCC::Equal, u, 0xc1e0_0000_0000_0000_u64 as i64);
    let big_int = b.ins().bor(is_zero, is_min);
    let is_int = b.ins().select(small, small_int, big_int);
    let i = b.ins().fcvt_to_sint_sat(I32, d);
    let int_payload = b.ins().uextend(I64, i);
    let payload = b.ins().select(is_int, int_payload, u);
    let int_tag = b.ins().iconst(I64, jsvalue::TAG_INT);
    let float_tag = b.ins().iconst(I64, jsvalue::TAG_FLOAT64);
    (payload, b.ins().select(is_int, int_tag, float_tag))
}

/// Bodies up to this many IR nodes are inlined into their JS entry.
const INLINE_LIMIT: usize = 256;

fn ir_size(f: &ir::Func) -> usize {
    let mut n = 0;
    walk(&f.body, &mut |_| n += 1);
    n
}

fn all_math(m: &ir::Module) -> Vec<MathFn> {
    let mut out = Vec::new();
    for f in &m.funcs {
        walk(&f.body, &mut |e| {
            if let ExprKind::Math(mf, _) = &e.kind
                && !out.contains(mf)
            {
                out.push(*mf);
            }
        });
    }
    out
}

pub(crate) use tsaot_front::ir::visit as walk_expr;

/// Visit every expression in `body`.
fn walk(body: &[Stmt], f: &mut impl FnMut(&Expr)) {
    for st in body {
        match st {
            Stmt::Expr(x) | Stmt::Throw(x) | Stmt::Await { value: x, .. } => walk_expr(x, f),
            Stmt::Return(x) => x.iter().for_each(|x| walk_expr(x, f)),
            Stmt::If(c, a, b) => {
                walk_expr(c, f);
                walk(a, f);
                walk(b, f);
            }
            Stmt::Loop { test, body, update, .. } => {
                test.iter().chain(update).for_each(|x| walk_expr(x, f));
                walk(body, f);
            }
            Stmt::Switch { cases, .. } => {
                for (t, b) in cases {
                    t.iter().for_each(|x| walk_expr(x, f));
                    walk(b, f);
                }
            }
            Stmt::Block(b) => walk(b, f),
            Stmt::Try { body, catch, finally } => {
                walk(body, f);
                if let Some((_, c)) = catch {
                    walk(c, f);
                }
                if let Some(fin) = finally {
                    walk(fin, f);
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::NewBoxes(_) => {}
        }
    }
}
