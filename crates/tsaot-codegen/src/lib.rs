//! Lowers typed IR to native code with Cranelift and emits a relocatable
//! object file.
//!
//! Every IR function `i` produces two symbols:
//! - `tsaot_fn_<i>`: the function itself, taking and returning plain scalars
//!   (`double` for `number`, zero-extended `uint8_t` for `boolean`). Native
//!   callers call this directly.
//! - `tsaot_fast_<i>`: its entry from JS, a QuickJS-NG `JSCFunctionFast` that
//!   the interpreter's call opcodes invoke directly on their stack slots. The
//!   type guard, unboxing and boxing are all inline machine code here.
//!
//! Stack overflow: each function compares the stack pointer against the
//! runtime's `tsaot_stack_limit` on entry. On overflow it calls
//! `tsaot_stack_overflow()` (which sets `tsaot_unwinding`) and returns. After
//! every native call the caller checks `tsaot_unwinding` and returns
//! immediately, so the whole native stack unwinds back to the JS entry,
//! which throws the interpreter's `RangeError`.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F32, F64, I8, I32, I64};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, FuncRef, GlobalValue, InstBuilder, MemFlagsData, Signature, Type, Value,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use tsaot_front::ir::{self, BinOp, CmpOp, Expr, ExprKind, LogicalOp, MathFn, Stmt, Ty, UnOp};

pub fn symbol_name(id: ir::FuncId) -> String {
    format!("tsaot_fn_{id}")
}

pub fn fast_symbol_name(id: ir::FuncId) -> String {
    format!("tsaot_fast_{id}")
}

/// Native stack available to one entry from JS. QuickJS keeps its own usage
/// under `JS_DEFAULT_STACK_SIZE` (1 MiB), so both fit a default 8 MiB stack.
pub const NATIVE_STACK_BUDGET: u64 = 2 * 1024 * 1024;

/// QuickJS-NG `JSValue` layout on 64-bit targets without NaN boxing: a
/// 16-byte struct with the payload union at offset 0 and an `int64_t` tag at
/// offset 8. The generated C glue `_Static_assert`s all of these.
pub mod jsvalue {
    pub const SIZE: usize = 16;
    pub const TAG_OFFSET: i32 = 8;
    pub const TAG_INT: i64 = 0;
    pub const TAG_BOOL: i64 = 1;
    pub const TAG_UNDEFINED: i64 = 3;
    pub const TAG_UNINITIALIZED: i64 = 4;
    pub const TAG_EXCEPTION: i64 = 6;
    pub const TAG_FLOAT64: i64 = 8;
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
    for (i, f) in m.funcs.iter().enumerate() {
        cg.define(i, f).with_context(|| format!("compiling `{}`", f.name))?;
        cg.define_fast(i, f).with_context(|| format!("compiling the entry of `{}`", f.name))?;
    }
    Ok(module.finish().emit()?)
}

fn clif_ty(t: Ty) -> Type {
    match t {
        Ty::F64 => F64,
        Ty::Bool => I8,
        Ty::Void => unreachable!("void has no machine type"),
    }
}

fn abi(t: Ty) -> AbiParam {
    match t {
        Ty::Bool => AbiParam::new(I8).uext(),
        t => AbiParam::new(clif_ty(t)),
    }
}

struct Codegen<'m> {
    module: &'m mut ObjectModule,
    funcs: Vec<FuncId>,
    fast_funcs: Vec<FuncId>,
    imports: HashMap<&'static str, FuncId>,
    stack_limit: DataId,
    unwinding: DataId,
    fctx: FunctionBuilderContext,
}

impl<'m> Codegen<'m> {
    fn new(module: &'m mut ObjectModule, m: &ir::Module) -> Result<Self> {
        let mut funcs = Vec::new();
        for (i, f) in m.funcs.iter().enumerate() {
            let mut sig = module.make_signature();
            sig.params.extend(f.params.iter().map(|&t| abi(t)));
            if f.ret != Ty::Void {
                sig.returns.push(abi(f.ret));
            }
            funcs.push(module.declare_function(&symbol_name(i), Linkage::Export, &sig)?);
        }
        let fast_sig = fast_signature(module);
        let mut fast_funcs = Vec::new();
        for i in 0..m.funcs.len() {
            fast_funcs.push(module.declare_function(&fast_symbol_name(i), Linkage::Export, &fast_sig)?);
        }
        let stack_limit = module.declare_data("tsaot_stack_limit", Linkage::Import, true, false)?;
        let unwinding = module.declare_data("tsaot_unwinding", Linkage::Import, true, false)?;
        Ok(Codegen {
            module,
            funcs,
            fast_funcs,
            imports: HashMap::new(),
            stack_limit,
            unwinding,
            fctx: FunctionBuilderContext::new(),
        })
    }

    fn import(&mut self, name: &'static str, params: &[Type], ret: Option<Type>) -> Result<FuncId> {
        if let Some(&id) = self.imports.get(name) {
            return Ok(id);
        }
        let mut sig = self.module.make_signature();
        sig.params.extend(params.iter().map(|&t| AbiParam::new(t)));
        sig.returns.extend(ret.map(AbiParam::new));
        let id = self.module.declare_function(name, Linkage::Import, &sig)?;
        self.imports.insert(name, id);
        Ok(id)
    }

    /// Imports every external function `f`'s body may call.
    fn helpers(&mut self, f: &ir::Func) -> Result<HashMap<&'static str, FuncId>> {
        let mut helpers = HashMap::new();
        helpers.insert("fmod", self.import("fmod", &[F64, F64], Some(F64))?);
        helpers.insert("tsaot_pow", self.import("tsaot_pow", &[F64, F64], Some(F64))?);
        helpers.insert("tsaot_toint32_slow", self.import("tsaot_toint32_slow", &[F64], Some(I32))?);
        helpers.insert("tsaot_stack_overflow", self.import("tsaot_stack_overflow", &[], None)?);
        for mf in used_math(f) {
            if let Some(sym) = mf.symbol() {
                let params = vec![F64; mf.arity()];
                helpers.insert(sym, self.import(sym, &params, Some(F64))?);
            }
        }
        Ok(helpers)
    }

    /// Sets up lowering of `f`'s body into `b`'s current block, with the
    /// parameters bound to `params`.
    fn lowerer<'b>(
        &mut self,
        mut b: FunctionBuilder<'b>,
        f: &ir::Func,
        helpers: &HashMap<&'static str, FuncId>,
        params: &[Value],
        exits: Exits,
    ) -> Lower<'b> {
        let helpers = helpers.iter().map(|(&n, &id)| (n, self.module.declare_func_in_func(id, b.func))).collect();
        let func_refs = self.funcs.iter().map(|&id| self.module.declare_func_in_func(id, b.func)).collect();
        let stack_limit = self.module.declare_data_in_func(self.stack_limit, b.func);
        let unwinding = self.module.declare_data_in_func(self.unwinding, b.func);
        let vars: Vec<Variable> = f.locals.iter().map(|&t| b.declare_var(clif_ty(t))).collect();
        for (i, &t) in f.locals.iter().enumerate() {
            let v = if i < params.len() { params[i] } else { zero(&mut b, t) };
            b.def_var(vars[i], v);
        }
        Lower {
            b,
            ret: f.ret,
            vars,
            func_refs,
            helpers,
            stack_limit,
            unwinding,
            ptr: self.module.target_config().pointer_type(),
            loops: HashMap::new(),
            unwind_block: None,
            exits,
        }
    }

    /// Defines `tsaot_fn_<index>`, the function itself, for native callers.
    fn define(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        let helpers = self.helpers(f)?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = self.signature(f);
        let mut fctx = std::mem::take(&mut self.fctx);
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params = b.block_params(entry).to_vec();
        let mut l = self.lowerer(b, f, &helpers, &params, Exits { ret: Exit::Return, unwind: Exit::Return });
        l.stack_check();
        l.body(&f.body);
        l.b.seal_all_blocks();
        l.b.finalize(self.module.target_config());
        self.module.define_function(self.funcs[index], &mut ctx).map_err(|e| anyhow!("{e:?}"))?;
        self.module.clear_context(&mut ctx);
        self.fctx = fctx;
        Ok(())
    }

    /// Defines `tsaot_fast_<index>`, the JS-facing entry of native function
    /// `index`, with the engine's `JSCFunctionFast` signature:
    ///
    /// ```c
    /// JSValue tsaot_fast_i(JSContext *ctx, int argc, JSValueConst *argv);
    /// ```
    ///
    /// The whole boundary is inline machine code: argument tags are checked
    /// and payloads unboxed straight from the interpreter's stack slots, the
    /// body runs (inlined, unless it is large), and the boxed result is
    /// returned in registers. Ill-typed arguments return `JS_UNINITIALIZED`,
    /// which makes the engine call the interpreted fallback instead; a stack
    /// overflow throws a `RangeError` and returns `JS_EXCEPTION`.
    fn define_fast(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        let ptr = self.module.target_config().pointer_type();
        let throw_so = self.import("tsaot_throw_stack_overflow", &[ptr], None)?;
        let helpers = self.helpers(f)?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = fast_signature(self.module);
        let mut fctx = std::mem::take(&mut self.fctx);
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let throw_so = self.module.declare_func_in_func(throw_so, b.func);
        let stack_limit = self.module.declare_data_in_func(self.stack_limit, b.func);
        let unwinding = self.module.declare_data_in_func(self.unwinding, b.func);
        let mem = MemFlagsData::trusted();

        let entry = b.create_block();
        let decline = b.create_block();
        let overflow = b.create_block();
        let boxing = b.create_block();
        b.set_cold_block(decline);
        b.set_cold_block(overflow);
        let result = (f.ret != Ty::Void).then(|| b.append_block_param(boxing, clif_ty(f.ret)));
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let [js_ctx, argc, argv] = b.block_params(entry).try_into().unwrap();

        let enough = b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, argc, f.params.len() as i64);
        let unbox = b.create_block();
        b.ins().brif(enough, unbox, &[], decline, &[]);
        b.switch_to_block(unbox);
        let args = unbox_args(&mut b, &f.params, argv, decline);

        // This entry starts a fresh native stack budget.
        let sp = b.ins().get_stack_pointer(ptr);
        let limit = b.ins().iadd_imm_s(sp, -(NATIVE_STACK_BUDGET as i64));
        let limit_addr = b.ins().symbol_value(ptr, stack_limit);
        b.ins().store(mem, limit, limit_addr, 0);

        let mut b = if ir_size(f) <= INLINE_LIMIT {
            // The body itself, with `return` jumping to the boxing code. It
            // needs no stack check: the budget was just reset.
            let exits = Exits { ret: Exit::Jump(boxing), unwind: Exit::Jump(overflow) };
            let mut l = self.lowerer(b, f, &helpers, &args, exits);
            l.body(&f.body);
            l.b
        } else {
            let callee = self.module.declare_func_in_func(self.funcs[index], b.func);
            let call = b.ins().call(callee, &args);
            let r: Vec<BlockArg> = b.inst_results(call).iter().map(|&v| BlockArg::Value(v)).collect();
            let flag_addr = b.ins().symbol_value(ptr, unwinding);
            let flag = b.ins().load(I8, mem, flag_addr, 0);
            b.ins().brif(flag, overflow, &[], boxing, &r);
            b
        };

        b.switch_to_block(boxing);
        let (payload, tag) = match (f.ret, result) {
            (Ty::Void, _) => (b.ins().iconst(I64, 0), b.ins().iconst(I64, jsvalue::TAG_UNDEFINED)),
            (Ty::Bool, Some(r)) => (b.ins().uextend(I64, r), b.ins().iconst(I64, jsvalue::TAG_BOOL)),
            (Ty::F64, Some(d)) => box_number(&mut b, d),
            _ => unreachable!("non-void function returns a value"),
        };
        // Returned in registers: the result never touches memory on its way
        // back into the interpreter.
        b.ins().return_(&[payload, tag]);

        b.switch_to_block(overflow);
        let flag_addr = b.ins().symbol_value(ptr, unwinding);
        let zero8 = b.ins().iconst(I8, 0);
        b.ins().store(mem, zero8, flag_addr, 0);
        b.ins().call(throw_so, &[js_ctx]);
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

    fn signature(&self, f: &ir::Func) -> Signature {
        let mut sig = self.module.make_signature();
        sig.params.extend(f.params.iter().map(|&t| abi(t)));
        if f.ret != Ty::Void {
            sig.returns.push(abi(f.ret));
        }
        sig
    }
}

fn zero(b: &mut FunctionBuilder, t: Ty) -> Value {
    match t {
        Ty::F64 => b.ins().f64const(0.0),
        Ty::Bool => b.ins().iconst(I8, 0),
        Ty::Void => unreachable!(),
    }
}

fn used_math(f: &ir::Func) -> Vec<MathFn> {
    fn walk_e(e: &Expr, out: &mut Vec<MathFn>) {
        match &e.kind {
            ExprKind::Math(m, args) => {
                if !out.contains(m) {
                    out.push(*m);
                }
                args.iter().for_each(|a| walk_e(a, out));
            }
            ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Local(_) | ExprKind::Update { .. } => {}
            ExprKind::SetLocal(_, x) | ExprKind::Unary(_, x) | ExprKind::Truthy(x) => walk_e(x, out),
            ExprKind::Binary(_, a, b) | ExprKind::Compare(_, a, b) | ExprKind::Logical(_, a, b) => {
                walk_e(a, out);
                walk_e(b, out);
            }
            ExprKind::Cond(a, b, c) => {
                walk_e(a, out);
                walk_e(b, out);
                walk_e(c, out);
            }
            ExprKind::Call(_, xs) | ExprKind::Seq(xs) => xs.iter().for_each(|a| walk_e(a, out)),
        }
    }
    fn walk_s(s: &Stmt, out: &mut Vec<MathFn>) {
        match s {
            Stmt::Expr(e) => walk_e(e, out),
            Stmt::If(c, a, b) => {
                walk_e(c, out);
                a.iter().chain(b).for_each(|s| walk_s(s, out));
            }
            Stmt::Loop { test, body, update, .. } => {
                test.iter().chain(update).for_each(|e| walk_e(e, out));
                body.iter().for_each(|s| walk_s(s, out));
            }
            Stmt::Return(e) => e.iter().for_each(|e| walk_e(e, out)),
            Stmt::Block(b) => b.iter().for_each(|s| walk_s(s, out)),
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }
    let mut out = Vec::new();
    f.body.iter().for_each(|s| walk_s(s, &mut out));
    out
}

struct Lower<'a> {
    b: FunctionBuilder<'a>,
    ret: Ty,
    vars: Vec<Variable>,
    func_refs: Vec<FuncRef>,
    helpers: HashMap<&'static str, FuncRef>,
    stack_limit: GlobalValue,
    unwinding: GlobalValue,
    ptr: Type,
    /// loop id -> (continue target, break target)
    loops: HashMap<ir::LoopId, (Block, Block)>,
    unwind_block: Option<Block>,
    exits: Exits,
}

/// Where lowered code goes when it leaves the body.
#[derive(Clone, Copy)]
enum Exit {
    /// Return from the Cranelift function.
    Return,
    /// Jump to a block: for `ret`, with the return value as its parameter
    /// (if not `void`); for `unwind`, without parameters.
    Jump(Block),
}

#[derive(Clone, Copy)]
struct Exits {
    /// `return` statements.
    ret: Exit,
    /// Stack-overflow unwinding.
    unwind: Exit,
}

/// Bodies up to this many IR nodes are inlined into their JS entry.
const INLINE_LIMIT: usize = 256;

fn ir_size(f: &ir::Func) -> usize {
    fn e(x: &Expr) -> usize {
        1 + match &x.kind {
            ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Local(_) | ExprKind::Update { .. } => 0,
            ExprKind::SetLocal(_, a) | ExprKind::Unary(_, a) | ExprKind::Truthy(a) => e(a),
            ExprKind::Binary(_, a, b) | ExprKind::Compare(_, a, b) | ExprKind::Logical(_, a, b) => e(a) + e(b),
            ExprKind::Cond(a, b, c) => e(a) + e(b) + e(c),
            ExprKind::Call(_, xs) | ExprKind::Math(_, xs) | ExprKind::Seq(xs) => xs.iter().map(e).sum(),
        }
    }
    fn s(x: &Stmt) -> usize {
        1 + match x {
            Stmt::Expr(x) => e(x),
            Stmt::If(c, a, b) => e(c) + a.iter().chain(b).map(s).sum::<usize>(),
            Stmt::Loop { test, body, update, .. } => {
                test.iter().chain(update).map(e).sum::<usize>() + body.iter().map(s).sum::<usize>()
            }
            Stmt::Return(x) => x.iter().map(e).sum(),
            Stmt::Block(b) => b.iter().map(s).sum(),
            Stmt::Break(_) | Stmt::Continue(_) => 0,
        }
    }
    f.body.iter().map(s).sum()
}

/// Reads the arguments of a JS call straight from the interpreter's
/// `JSValue` slots, branching to `decline` on any type mismatch.
fn unbox_args(b: &mut FunctionBuilder, params: &[Ty], argv: Value, decline: Block) -> Vec<Value> {
    let mem = MemFlagsData::trusted();
    let mut args = Vec::with_capacity(params.len());
    for (k, &t) in params.iter().enumerate() {
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
                args.push(v);
            }
            Ty::Bool => {
                let is_bool = b.ins().icmp_imm_s(IntCC::Equal, tag, jsvalue::TAG_BOOL);
                let ok = b.create_block();
                b.ins().brif(is_bool, ok, &[], decline, &[]);
                b.switch_to_block(ok);
                let i = b.ins().load(I32, mem, argv, at);
                args.push(b.ins().icmp_imm_s(IntCC::NotEqual, i, 0));
            }
            Ty::Void => unreachable!(),
        }
    }
    args
}

/// Boxes a number like the interpreter's `js_number()`: int32-valued doubles
/// (except -0) become `JS_TAG_INT`, everything else `JS_TAG_FLOAT64`.
///
/// The int32 test is QuickJS's `double_is_int32`, done on the bit pattern
/// with integer ops only. The obvious test (convert to int and back, then
/// compare) is a ~20-cycle chain of dependent float conversions that sits
/// right on the latency path back into the interpreter; here the conversion
/// that produces the int payload runs in parallel with the test instead.
/// Returns `(payload, tag)`.
fn box_number(b: &mut FunctionBuilder, d: Value) -> (Value, Value) {
    let u = b.ins().bitcast(I64, MemFlagsData::new(), d);
    // Unbiased exponent; negative exponents wrap to huge unsigned values.
    let e = b.ins().ushr_imm_u(u, 52);
    let e = b.ins().band_imm_u(e, 0x7ff);
    let e = b.ins().iadd_imm_s(e, -1023);
    // |d| in [1, 2^31): integral iff no mantissa bits below the binary point.
    let small = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, e, 30);
    let frac = b.ins().ishl_imm_u(u, 12);
    let frac = b.ins().ishl(frac, e);
    let small_int = b.ins().icmp_imm_s(IntCC::Equal, frac, 0);
    // Otherwise only +0 and INT32_MIN are int32 (this rejects -0).
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

impl Lower<'_> {
    fn is_filled(&self) -> bool {
        let cur = self.b.current_block().unwrap();
        self.b.func.layout.last_inst(cur).is_some_and(|i| self.b.func.dfg.insts[i].opcode().is_terminator())
    }

    /// After a terminator, continue emitting into a fresh unreachable block.
    fn after_terminator(&mut self) {
        let dead = self.b.create_block();
        self.b.switch_to_block(dead);
    }

    fn unwind_block(&mut self) -> Block {
        *self.unwind_block.get_or_insert_with(|| {
            let blk = self.b.create_block();
            self.b.set_cold_block(blk);
            blk
        })
    }

    /// Leave the body with `value` (`None` for `void`).
    fn exit_with(&mut self, value: Option<Value>) {
        let args: Vec<Value> = value.into_iter().collect();
        match self.exits.ret {
            Exit::Return => self.b.ins().return_(&args),
            Exit::Jump(blk) => {
                let args: Vec<BlockArg> = args.into_iter().map(BlockArg::Value).collect();
                self.b.ins().jump(blk, &args)
            }
        };
    }

    /// Leave the body with a placeholder value; used where the checker
    /// guarantees the code is unreachable or the result is ignored.
    fn exit_default(&mut self) {
        let v = (self.ret != Ty::Void).then(|| zero(&mut self.b, self.ret));
        self.exit_with(v);
    }

    /// Entry check of a function called from native code.
    fn stack_check(&mut self) {
        let limit_addr = self.b.ins().symbol_value(self.ptr, self.stack_limit);
        let limit = self.b.ins().load(self.ptr, MemFlagsData::trusted(), limit_addr, 0);
        let sp = self.b.ins().get_stack_pointer(self.ptr);
        let overflow = self.b.ins().icmp(IntCC::UnsignedLessThan, sp, limit);
        let overflow_block = self.b.create_block();
        let body_block = self.b.create_block();
        self.b.ins().brif(overflow, overflow_block, &[], body_block, &[]);
        self.b.switch_to_block(overflow_block);
        self.b.set_cold_block(overflow_block);
        let so = self.helpers["tsaot_stack_overflow"];
        self.b.ins().call(so, &[]);
        let ub = self.unwind_block();
        self.b.ins().jump(ub, &[]);
        self.b.switch_to_block(body_block);
    }

    /// Lower a whole function body, including its exits.
    fn body(&mut self, body: &[Stmt]) {
        self.stmts(body);
        if !self.is_filled() {
            // Only reachable for `void` functions (or dead code): the checker
            // rejects non-void functions that can fall off the end.
            self.exit_default();
        }
        if let Some(ub) = self.unwind_block {
            self.b.switch_to_block(ub);
            match self.exits.unwind {
                // The caller sees `tsaot_unwinding` and ignores the value.
                Exit::Return => {
                    let v: Vec<Value> =
                        (self.ret != Ty::Void).then(|| zero(&mut self.b, self.ret)).into_iter().collect();
                    self.b.ins().return_(&v);
                }
                Exit::Jump(blk) => {
                    self.b.ins().jump(blk, &[]);
                }
            }
        }
    }

    // ---- statements ----

    fn stmts(&mut self, list: &[Stmt]) {
        for s in list {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Expr(e) => {
                self.expr(e);
            }
            Stmt::Block(b) => self.stmts(b),
            Stmt::If(c, then, els) => {
                let cv = self.expr(c).unwrap();
                let tb = self.b.create_block();
                let eb = self.b.create_block();
                let join = self.b.create_block();
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                self.b.switch_to_block(tb);
                self.stmts(then);
                if !self.is_filled() {
                    self.b.ins().jump(join, &[]);
                }
                self.b.switch_to_block(eb);
                self.stmts(els);
                if !self.is_filled() {
                    self.b.ins().jump(join, &[]);
                }
                self.b.switch_to_block(join);
            }
            Stmt::Loop { id, test, post_test, body, update } => {
                let header = self.b.create_block();
                let body_b = self.b.create_block();
                let cont = self.b.create_block();
                let exit = self.b.create_block();
                self.loops.insert(*id, (cont, exit));
                self.b.ins().jump(header, &[]);

                self.b.switch_to_block(header);
                match test {
                    Some(t) if !post_test => {
                        let v = self.expr(t).unwrap();
                        self.b.ins().brif(v, body_b, &[], exit, &[]);
                    }
                    _ => {
                        self.b.ins().jump(body_b, &[]);
                    }
                }

                self.b.switch_to_block(body_b);
                self.stmts(body);
                if !self.is_filled() {
                    self.b.ins().jump(cont, &[]);
                }

                self.b.switch_to_block(cont);
                if let Some(u) = update {
                    self.expr(u);
                }
                match test {
                    Some(t) if *post_test => {
                        let v = self.expr(t).unwrap();
                        self.b.ins().brif(v, header, &[], exit, &[]);
                    }
                    _ => {
                        self.b.ins().jump(header, &[]);
                    }
                }
                self.b.switch_to_block(exit);
            }
            Stmt::Break(id) => {
                let (_, exit) = self.loops[id];
                self.b.ins().jump(exit, &[]);
                self.after_terminator();
            }
            Stmt::Continue(id) => {
                let (cont, _) = self.loops[id];
                self.b.ins().jump(cont, &[]);
                self.after_terminator();
            }
            Stmt::Return(e) => {
                let v = e.as_ref().and_then(|e| self.expr(e));
                match (self.ret, v) {
                    (Ty::Void, _) => self.exit_with(None),
                    (_, Some(v)) => self.exit_with(Some(v)),
                    (_, None) => unreachable!("checker guarantees a value"),
                }
                self.after_terminator();
            }
        }
    }

    // ---- expressions ----

    /// Lower an expression; `None` for `void`.
    fn expr(&mut self, e: &Expr) -> Option<Value> {
        Some(match &e.kind {
            ExprKind::Num(v) => self.b.ins().f64const(*v),
            ExprKind::Bool(v) => self.b.ins().iconst(I8, *v as i64),
            ExprKind::Local(l) => self.b.use_var(self.vars[*l]),
            ExprKind::SetLocal(l, x) => {
                let v = self.expr(x).unwrap();
                self.b.def_var(self.vars[*l], v);
                v
            }
            ExprKind::Update { local, delta, prefix } => {
                let old = self.b.use_var(self.vars[*local]);
                let d = self.b.ins().f64const(*delta);
                let new = self.b.ins().fadd(old, d);
                self.b.def_var(self.vars[*local], new);
                if *prefix { new } else { old }
            }
            ExprKind::Unary(op, x) => {
                let v = self.expr(x).unwrap();
                match op {
                    UnOp::Neg => self.b.ins().fneg(v),
                    UnOp::Not => self.b.ins().bxor_imm_u(v, 1),
                    UnOp::BitNot => {
                        let i = self.toint32(v);
                        let r = self.b.ins().bnot(i);
                        self.b.ins().fcvt_from_sint(F64, r)
                    }
                }
            }
            ExprKind::Truthy(x) => {
                let v = self.expr(x).unwrap();
                match x.ty {
                    Ty::Bool => v,
                    _ => {
                        let z = self.b.ins().f64const(0.0);
                        // Ordered: NaN is falsy.
                        self.b.ins().fcmp(FloatCC::OrderedNotEqual, v, z)
                    }
                }
            }
            ExprKind::Binary(op, x, y) => {
                let a = self.expr(x).unwrap();
                let c = self.expr(y).unwrap();
                self.binary(*op, a, c)
            }
            ExprKind::Compare(op, x, y) => {
                let a = self.expr(x).unwrap();
                let c = self.expr(y).unwrap();
                match x.ty {
                    Ty::F64 => {
                        let cc = match op {
                            CmpOp::Lt => FloatCC::LessThan,
                            CmpOp::Le => FloatCC::LessThanOrEqual,
                            CmpOp::Gt => FloatCC::GreaterThan,
                            CmpOp::Ge => FloatCC::GreaterThanOrEqual,
                            CmpOp::Eq => FloatCC::Equal,
                            CmpOp::Ne => FloatCC::NotEqual,
                        };
                        self.b.ins().fcmp(cc, a, c)
                    }
                    _ => {
                        let cc = if *op == CmpOp::Eq { IntCC::Equal } else { IntCC::NotEqual };
                        self.b.ins().icmp(cc, a, c)
                    }
                }
            }
            ExprKind::Logical(op, x, y) => {
                let t = clif_ty(e.ty);
                let a = self.expr(x).unwrap();
                let truth = self.truthy(a, x.ty);
                let rhs = self.b.create_block();
                let join = self.b.create_block();
                let res = self.b.append_block_param(join, t);
                match op {
                    LogicalOp::And => self.b.ins().brif(truth, rhs, &[], join, &[BlockArg::Value(a)]),
                    LogicalOp::Or => self.b.ins().brif(truth, join, &[BlockArg::Value(a)], rhs, &[]),
                };
                self.b.switch_to_block(rhs);
                let c = self.expr(y).unwrap();
                self.b.ins().jump(join, &[BlockArg::Value(c)]);
                self.b.switch_to_block(join);
                res
            }
            ExprKind::Cond(c, x, y) => {
                let cv = self.expr(c).unwrap();
                let tb = self.b.create_block();
                let eb = self.b.create_block();
                let join = self.b.create_block();
                let res = (e.ty != Ty::Void).then(|| self.b.append_block_param(join, clif_ty(e.ty)));
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                for (blk, arm) in [(tb, x), (eb, y)] {
                    self.b.switch_to_block(blk);
                    let v = self.expr(arm);
                    let args: Vec<BlockArg> = v.into_iter().map(BlockArg::Value).collect();
                    self.b.ins().jump(join, &args);
                }
                self.b.switch_to_block(join);
                return res;
            }
            ExprKind::Call(id, args) => {
                let vals: Vec<Value> = args.iter().map(|a| self.expr(a).unwrap()).collect();
                let call = self.b.ins().call(self.func_refs[*id], &vals);
                let result = self.b.inst_results(call).first().copied();
                // Propagate stack-overflow unwinding.
                let addr = self.b.ins().symbol_value(self.ptr, self.unwinding);
                let flag = self.b.ins().load(I8, MemFlagsData::trusted(), addr, 0);
                let ub = self.unwind_block();
                let cont = self.b.create_block();
                self.b.ins().brif(flag, ub, &[], cont, &[]);
                self.b.switch_to_block(cont);
                return result;
            }
            ExprKind::Math(f, args) => {
                let vals: Vec<Value> = args.iter().map(|a| self.expr(a).unwrap()).collect();
                self.math(*f, &vals)
            }
            ExprKind::Seq(xs) => {
                let mut last = None;
                for x in xs {
                    last = self.expr(x);
                }
                return last;
            }
        })
    }

    fn truthy(&mut self, v: Value, ty: Ty) -> Value {
        match ty {
            Ty::Bool => v,
            _ => {
                let z = self.b.ins().f64const(0.0);
                self.b.ins().fcmp(FloatCC::OrderedNotEqual, v, z)
            }
        }
    }

    fn call_helper(&mut self, name: &str, args: &[Value]) -> Value {
        let f = self.helpers[name];
        let call = self.b.ins().call(f, args);
        self.b.inst_results(call)[0]
    }

    fn binary(&mut self, op: BinOp, a: Value, c: Value) -> Value {
        match op {
            BinOp::Add => self.b.ins().fadd(a, c),
            BinOp::Sub => self.b.ins().fsub(a, c),
            BinOp::Mul => self.b.ins().fmul(a, c),
            BinOp::Div => self.b.ins().fdiv(a, c),
            BinOp::Rem => self.rem(a, c),
            BinOp::Pow => self.call_helper("tsaot_pow", &[a, c]),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Sar | BinOp::Shr => {
                let x = self.toint32(a);
                let y = self.toint32(c);
                // Cranelift shifts take the amount modulo 32, as JS does.
                let r = match op {
                    BinOp::BitAnd => self.b.ins().band(x, y),
                    BinOp::BitOr => self.b.ins().bor(x, y),
                    BinOp::BitXor => self.b.ins().bxor(x, y),
                    BinOp::Shl => self.b.ins().ishl(x, y),
                    BinOp::Sar => self.b.ins().sshr(x, y),
                    BinOp::Shr => {
                        let r = self.b.ins().ushr(x, y);
                        return self.b.ins().fcvt_from_uint(F64, r);
                    }
                    _ => unreachable!(),
                };
                self.b.ins().fcvt_from_sint(F64, r)
            }
        }
    }

    /// JS `%`. When both operands are integer-valued (and the divisor is not 0
    /// or -1) an integer remainder is exact; `copysign` restores JS's rule
    /// that the result has the dividend's sign, including `-4 % 2 === -0`.
    /// Everything else uses C `fmod`, which matches JS exactly.
    fn rem(&mut self, a: Value, c: Value) -> Value {
        let ai = self.b.ins().fcvt_to_sint_sat(I64, a);
        let ci = self.b.ins().fcvt_to_sint_sat(I64, c);
        let af = self.b.ins().fcvt_from_sint(F64, ai);
        let cf = self.b.ins().fcvt_from_sint(F64, ci);
        let a_int = self.b.ins().fcmp(FloatCC::Equal, af, a);
        let c_int = self.b.ins().fcmp(FloatCC::Equal, cf, c);
        let c_abs = self.b.ins().iabs(ci);
        // |c| > 1 excludes 0 (NaN result) and -1 (srem overflow on INT64_MIN).
        let c_ok = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, c_abs, 1);
        // Values >= 2^63 saturate to i64::MAX, which converts back to 2^63
        // and would pass the round-trip check.
        let a_unsat = self.b.ins().icmp_imm_s(IntCC::NotEqual, ai, i64::MAX);
        let c_unsat = self.b.ins().icmp_imm_s(IntCC::NotEqual, ci, i64::MAX);
        let ok = self.b.ins().band(a_int, c_int);
        let ok = self.b.ins().band(ok, c_ok);
        let unsat = self.b.ins().band(a_unsat, c_unsat);
        let ok = self.b.ins().band(ok, unsat);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, F64);
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let r = self.b.ins().srem(ai, ci);
        let rf = self.b.ins().fcvt_from_sint(F64, r);
        let rf = self.b.ins().fcopysign(rf, a);
        self.b.ins().jump(join, &[BlockArg::Value(rf)]);
        self.b.switch_to_block(slow);
        self.b.set_cold_block(slow);
        let r = self.call_helper("fmod", &[a, c]);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    /// JS ToInt32. Exact for |x| < 2^63 via a saturating i64 conversion and
    /// truncation; everything else (huge values, infinities, NaN) goes to the
    /// runtime.
    fn toint32(&mut self, x: Value) -> Value {
        let ax = self.b.ins().fabs(x);
        let lim = self.b.ins().f64const(9_223_372_036_854_775_808.0);
        let small = self.b.ins().fcmp(FloatCC::LessThan, ax, lim);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I32);
        self.b.ins().brif(small, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let i = self.b.ins().fcvt_to_sint_sat(I64, x);
        let r = self.b.ins().ireduce(I32, i);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(slow);
        self.b.set_cold_block(slow);
        let r = self.call_helper("tsaot_toint32_slow", &[x]);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    fn math(&mut self, f: MathFn, a: &[Value]) -> Value {
        if let Some(sym) = f.symbol() {
            return self.call_helper(sym, a);
        }
        fn ins<'s, 'b>(l: &'s mut Lower<'b>) -> cranelift_frontend::FuncInstBuilder<'s, 'b> {
            l.b.ins()
        }
        match f {
            MathFn::Abs => ins(self).fabs(a[0]),
            MathFn::Sqrt => ins(self).sqrt(a[0]),
            MathFn::Floor => ins(self).floor(a[0]),
            MathFn::Ceil => ins(self).ceil(a[0]),
            MathFn::Trunc => ins(self).trunc(a[0]),
            MathFn::Fround => {
                let s = ins(self).fdemote(F32, a[0]);
                ins(self).fpromote(F64, s)
            }
            // Cranelift fmin/fmax propagate NaN and order -0 < +0, like JS.
            MathFn::Min => ins(self).fmin(a[0], a[1]),
            MathFn::Max => ins(self).fmax(a[0], a[1]),
            MathFn::Imul => {
                let x = self.toint32(a[0]);
                let y = self.toint32(a[1]);
                let r = ins(self).imul(x, y);
                ins(self).fcvt_from_sint(F64, r)
            }
            MathFn::Clz32 => {
                let x = self.toint32(a[0]);
                let r = ins(self).clz(x);
                ins(self).fcvt_from_uint(F64, r)
            }
            MathFn::Sign => {
                // Returns x itself for ±0 and NaN.
                let z = ins(self).f64const(0.0);
                let one = ins(self).f64const(1.0);
                let neg = ins(self).f64const(-1.0);
                let lt = ins(self).fcmp(FloatCC::LessThan, a[0], z);
                let gt = ins(self).fcmp(FloatCC::GreaterThan, a[0], z);
                let r = ins(self).select(lt, neg, a[0]);
                ins(self).select(gt, one, r)
            }
            MathFn::IsNaN => ins(self).fcmp(FloatCC::Unordered, a[0], a[0]),
            MathFn::IsFinite => self.is_finite(a[0]),
            MathFn::IsInteger => self.is_integer(a[0]),
            MathFn::IsSafeInteger => {
                let int = self.is_integer(a[0]);
                let ax = ins(self).fabs(a[0]);
                let max = ins(self).f64const(9007199254740991.0);
                let safe = ins(self).fcmp(FloatCC::LessThanOrEqual, ax, max);
                ins(self).band(int, safe)
            }
            _ => unreachable!("{f:?} has a symbol"),
        }
    }

    fn is_finite(&mut self, x: Value) -> Value {
        let ax = self.b.ins().fabs(x);
        let inf = self.b.ins().f64const(f64::INFINITY);
        self.b.ins().fcmp(FloatCC::LessThan, ax, inf)
    }

    fn is_integer(&mut self, x: Value) -> Value {
        let fin = self.is_finite(x);
        let t = self.b.ins().trunc(x);
        let eq = self.b.ins().fcmp(FloatCC::Equal, t, x);
        self.b.ins().band(fin, eq)
    }
}

fn fast_signature(module: &ObjectModule) -> Signature {
    let ptr = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.extend([AbiParam::new(ptr), AbiParam::new(I32), AbiParam::new(ptr)]);
    // A 16-byte JSValue struct is returned in two integer registers (rax:rdx on
    // x86-64 SysV, x0:x1 on AArch64), which is what two I64 returns lower to.
    sig.returns.extend([AbiParam::new(I64), AbiParam::new(I64)]);
    sig
}
