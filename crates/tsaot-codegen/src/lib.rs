//! Lowers typed IR to native code with Cranelift and emits a relocatable
//! object file.
//!
//! Native ABI: every IR function `i` becomes the symbol `tsaot_fn_<i>` taking
//! and returning plain scalars (`double` for `number`, zero-extended `uint8_t`
//! for `boolean`). JS-facing entry stubs live in the generated C glue.
//!
//! Stack overflow: each function compares the stack pointer against the
//! runtime's `tsaot_stack_limit` on entry. On overflow it calls
//! `tsaot_stack_overflow()` (which sets `tsaot_unwinding`) and returns. After
//! every native call the caller checks `tsaot_unwinding` and returns
//! immediately, so the whole native stack unwinds back to the entry stub,
//! which turns it into a JS `RangeError`.

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
        let stack_limit = module.declare_data("tsaot_stack_limit", Linkage::Import, true, false)?;
        let unwinding = module.declare_data("tsaot_unwinding", Linkage::Import, true, false)?;
        Ok(Codegen {
            module,
            funcs,
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

    fn define(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        // Declare every helper up front so lowering does not need `&mut module`.
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

        let mut ctx = self.module.make_context();
        ctx.func.signature = self.signature(f);
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut self.fctx);

        let helper_refs: HashMap<&str, FuncRef> =
            helpers.iter().map(|(&n, &id)| (n, self.module.declare_func_in_func(id, b.func))).collect();
        let func_refs: Vec<FuncRef> =
            self.funcs.iter().map(|&id| self.module.declare_func_in_func(id, b.func)).collect();
        let stack_limit = self.module.declare_data_in_func(self.stack_limit, b.func);
        let unwinding = self.module.declare_data_in_func(self.unwinding, b.func);
        let ptr = self.module.target_config().pointer_type();

        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let vars: Vec<Variable> = f.locals.iter().map(|&t| b.declare_var(clif_ty(t))).collect();
        let params: Vec<Value> = b.block_params(entry).to_vec();
        for (i, &t) in f.locals.iter().enumerate() {
            let v = if i < params.len() { params[i] } else { zero(&mut b, t) };
            b.def_var(vars[i], v);
        }

        let mut l = Lower {
            b,
            ret: f.ret,
            vars,
            func_refs,
            helpers: helper_refs,
            unwinding,
            ptr,
            loops: HashMap::new(),
            unwind_block: None,
        };

        // Stack check.
        let limit_addr = l.b.ins().symbol_value(ptr, stack_limit);
        let limit = l.b.ins().load(ptr, MemFlagsData::trusted(), limit_addr, 0);
        let sp = l.b.ins().get_stack_pointer(ptr);
        let overflow = l.b.ins().icmp(IntCC::UnsignedLessThan, sp, limit);
        let overflow_block = l.b.create_block();
        let body_block = l.b.create_block();
        l.b.ins().brif(overflow, overflow_block, &[], body_block, &[]);
        l.b.switch_to_block(overflow_block);
        l.b.set_cold_block(overflow_block);
        let so = l.helpers["tsaot_stack_overflow"];
        l.b.ins().call(so, &[]);
        let ub = l.unwind_block();
        l.b.ins().jump(ub, &[]);
        l.b.switch_to_block(body_block);

        l.stmts(&f.body);
        if !l.is_filled() {
            // Only reachable for `void` functions; the checker rejects
            // non-void functions that can fall off the end.
            l.ret_default();
        }
        if let Some(ub) = l.unwind_block {
            l.b.switch_to_block(ub);
            l.ret_default();
        }
        l.b.seal_all_blocks();
        let config = self.module.target_config();
        l.b.finalize(config);

        self.module.define_function(self.funcs[index], &mut ctx).map_err(|e| anyhow!("{e:?}"))?;
        self.module.clear_context(&mut ctx);
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
    unwinding: GlobalValue,
    ptr: Type,
    /// loop id -> (continue target, break target)
    loops: HashMap<ir::LoopId, (Block, Block)>,
    unwind_block: Option<Block>,
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

    fn ret_default(&mut self) {
        if self.ret == Ty::Void {
            self.b.ins().return_(&[]);
        } else {
            let z = zero(&mut self.b, self.ret);
            self.b.ins().return_(&[z]);
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
                    (Ty::Void, _) => self.b.ins().return_(&[]),
                    (_, Some(v)) => self.b.ins().return_(&[v]),
                    (_, None) => unreachable!("checker guarantees a value"),
                };
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
