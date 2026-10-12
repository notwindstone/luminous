//! The Cranelift backend: translates the backend-neutral IR (`mir.rs`) to
//! Cranelift IR and emits an object file for the host.
//!
//! The neutral IR follows Cranelift's semantics, so this is a replay through
//! Cranelift's `FunctionBuilder`: blocks in the same order, variables through
//! its SSA builder.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{self as cl, InstBuilder, MemFlagsData};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, Module as _};
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::mir::{self, ArgExt, BinOp, ConvOp, InstData, Type, UnOp};

/// Compile `m` to an object file for the host. `opt_level` is Cranelift's:
/// "none", "speed" or "speed_and_size".
pub fn emit(m: &mir::Module, opt_level: &str) -> Result<Vec<u8>> {
    let mut flags = settings::builder();
    flags.set("opt_level", opt_level)?;
    flags.set("is_pic", "true")?;
    // Hot functions start on a cache line: their speed then does not
    // depend on where the code before them happens to end.
    flags.set("log2_min_function_alignment", "6")?;
    let isa = cranelift_native::builder()
        .map_err(|e| anyhow!("unsupported host: {e}"))?
        .finish(settings::Flags::new(flags))?;
    let builder = ObjectBuilder::new(isa, "tsaot", cranelift_module::default_libcall_names())?;
    let mut module = ObjectModule::new(builder);

    let mut funcs = Vec::new();
    for f in &m.funcs {
        let s = signature(&module, &f.sig);
        funcs.push(module.declare_function(&f.name, linkage(f.linkage), &s)?);
    }
    let mut data = Vec::new();
    for d in &m.data {
        data.push(module.declare_data(&d.name, linkage(d.linkage), d.writable, false)?);
    }

    let mut fctx = FunctionBuilderContext::new();
    let mut ctx = module.make_context();
    for (i, f) in m.funcs.iter().enumerate() {
        let Some(body) = &f.body else { continue };
        ctx.func.signature = signature(&module, &f.sig);
        let b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let mut t = Translator {
            b,
            module: &mut module,
            m,
            f: body,
            funcs: &funcs,
            data: &data,
            vals: vec![None; body.value_types.len()],
            blocks: Vec::new(),
            vars: Vec::new(),
            slots: Vec::new(),
            frefs: HashMap::new(),
            gvs: HashMap::new(),
            sigs: HashMap::new(),
        };
        t.function();
        let Translator { mut b, .. } = t;
        b.seal_all_blocks();
        b.finalize(module.target_config());
        module.define_function(funcs[i], &mut ctx).map_err(|e| anyhow!("{}: {e:?}", f.name))?;
        module.clear_context(&mut ctx);
    }
    for (i, d) in m.data.iter().enumerate() {
        let Some(init) = &d.init else { continue };
        let mut desc = DataDescription::new();
        match init {
            mir::DataInit::Zeroed { size } => desc.define_zeroinit(*size),
            mir::DataInit::Bytes(bytes) => desc.define(bytes.clone().into_boxed_slice()),
        }
        desc.set_align(d.align as u64);
        module.define_data(data[i], &desc)?;
    }
    Ok(module.finish().emit()?)
}

fn linkage(l: mir::Linkage) -> cranelift_module::Linkage {
    match l {
        mir::Linkage::Import => cranelift_module::Linkage::Import,
        mir::Linkage::Export => cranelift_module::Linkage::Export,
        mir::Linkage::Local => cranelift_module::Linkage::Local,
    }
}

fn ty(t: Type) -> cl::Type {
    use cranelift_codegen::ir::types;
    match t {
        Type::I8 => types::I8,
        Type::I16 => types::I16,
        Type::I32 => types::I32,
        Type::I64 => types::I64,
        Type::F32 => types::F32,
        Type::F64 => types::F64,
        Type::I64X2 => types::I64X2,
    }
}

fn signature(module: &ObjectModule, s: &mir::Signature) -> cl::Signature {
    let param = |p: &mir::AbiParam| {
        let a = cl::AbiParam::new(ty(p.ty));
        match p.ext {
            ArgExt::None => a,
            ArgExt::Uext => a.uext(),
            ArgExt::Sext => a.sext(),
        }
    };
    let mut sig = module.make_signature();
    sig.params.extend(s.params.iter().map(param));
    sig.returns.extend(s.returns.iter().map(param));
    sig
}

fn int_cc(cc: mir::IntCC) -> IntCC {
    match cc {
        mir::IntCC::Equal => IntCC::Equal,
        mir::IntCC::NotEqual => IntCC::NotEqual,
        mir::IntCC::SignedLessThan => IntCC::SignedLessThan,
        mir::IntCC::SignedGreaterThanOrEqual => IntCC::SignedGreaterThanOrEqual,
        mir::IntCC::SignedGreaterThan => IntCC::SignedGreaterThan,
        mir::IntCC::SignedLessThanOrEqual => IntCC::SignedLessThanOrEqual,
        mir::IntCC::UnsignedLessThan => IntCC::UnsignedLessThan,
        mir::IntCC::UnsignedGreaterThanOrEqual => IntCC::UnsignedGreaterThanOrEqual,
        mir::IntCC::UnsignedGreaterThan => IntCC::UnsignedGreaterThan,
        mir::IntCC::UnsignedLessThanOrEqual => IntCC::UnsignedLessThanOrEqual,
    }
}

fn float_cc(cc: mir::FloatCC) -> FloatCC {
    match cc {
        mir::FloatCC::Ordered => FloatCC::Ordered,
        mir::FloatCC::Unordered => FloatCC::Unordered,
        mir::FloatCC::Equal => FloatCC::Equal,
        mir::FloatCC::NotEqual => FloatCC::NotEqual,
        mir::FloatCC::OrderedNotEqual => FloatCC::OrderedNotEqual,
        mir::FloatCC::LessThan => FloatCC::LessThan,
        mir::FloatCC::LessThanOrEqual => FloatCC::LessThanOrEqual,
        mir::FloatCC::GreaterThan => FloatCC::GreaterThan,
        mir::FloatCC::GreaterThanOrEqual => FloatCC::GreaterThanOrEqual,
    }
}

struct Translator<'a, 'b> {
    b: FunctionBuilder<'b>,
    module: &'a mut ObjectModule,
    m: &'a mir::Module,
    f: &'a mir::Function,
    funcs: &'a [cranelift_module::FuncId],
    data: &'a [cranelift_module::DataId],
    vals: Vec<Option<cl::Value>>,
    blocks: Vec<cl::Block>,
    vars: Vec<Variable>,
    slots: Vec<cl::StackSlot>,
    frefs: HashMap<mir::FuncId, cl::FuncRef>,
    gvs: HashMap<mir::DataId, cl::GlobalValue>,
    sigs: HashMap<mir::SigRef, cl::SigRef>,
}

impl Translator<'_, '_> {
    fn v(&self, v: mir::Value) -> cl::Value {
        self.vals[v.index()].unwrap_or_else(|| panic!("{v:?} used before its definition"))
    }

    fn vs(&self, vs: &[mir::Value]) -> Vec<cl::Value> {
        vs.iter().map(|&v| self.v(v)).collect()
    }

    fn args(&self, vs: &[mir::Value]) -> Vec<cl::BlockArg> {
        vs.iter().map(|&v| cl::BlockArg::Value(self.v(v))).collect()
    }

    fn set(&mut self, v: mir::Value, x: cl::Value) {
        self.vals[v.index()] = Some(x);
    }

    fn fref(&mut self, f: mir::FuncId) -> cl::FuncRef {
        if let Some(&r) = self.frefs.get(&f) {
            return r;
        }
        let r = self.module.declare_func_in_func(self.funcs[f.index()], self.b.func);
        self.frefs.insert(f, r);
        r
    }

    fn gv(&mut self, d: mir::DataId) -> cl::GlobalValue {
        if let Some(&g) = self.gvs.get(&d) {
            return g;
        }
        let g = self.module.declare_data_in_func(self.data[d.index()], self.b.func);
        self.gvs.insert(d, g);
        g
    }

    fn sig(&mut self, s: mir::SigRef) -> cl::SigRef {
        if let Some(&r) = self.sigs.get(&s) {
            return r;
        }
        let sig = signature(self.module, &self.m.sigs[s.index()]);
        let r = self.b.import_signature(sig);
        self.sigs.insert(s, r);
        r
    }

    fn function(&mut self) {
        let f = self.f;
        self.blocks = (0..f.blocks.len()).map(|_| self.b.create_block()).collect();
        self.vars = f.vars.iter().map(|&t| self.b.declare_var(ty(t))).collect();
        self.slots = f
            .slots
            .iter()
            .map(|s| {
                self.b.create_sized_stack_slot(cl::StackSlotData::new(
                    cl::StackSlotKind::ExplicitSlot,
                    s.size,
                    s.align_shift,
                ))
            })
            .collect();
        for (i, data) in f.blocks.iter().enumerate() {
            let blk = self.blocks[i];
            if data.cold {
                self.b.set_cold_block(blk);
            }
            if i == f.entry.index() {
                self.b.append_block_params_for_function_params(blk);
                let params = self.b.block_params(blk).to_vec();
                for (&p, x) in data.params.iter().zip(params) {
                    self.set(p, x);
                }
            } else {
                for &p in &data.params {
                    let x = self.b.append_block_param(blk, ty(f.value_type(p)));
                    self.set(p, x);
                }
            }
        }
        for &blk in &f.layout {
            self.b.switch_to_block(self.blocks[blk.index()]);
            for &inst in &f.blocks[blk.index()].insts {
                self.inst(inst);
            }
        }
    }

    fn inst(&mut self, inst: mir::Inst) {
        let f = self.f;
        let results = f.inst_results(inst);
        let mem = MemFlagsData::trusted();
        let r = match f.inst(inst) {
            &InstData::Iconst { ty: t, imm } => {
                let bits = t.bits();
                let imm = if bits < 64 { imm & ((1i64 << bits) - 1) } else { imm };
                self.b.ins().iconst(ty(t), imm)
            }
            &InstData::F64const(bits) => self.b.ins().f64const(f64::from_bits(bits)),
            &InstData::Unary { op, arg } => {
                let a = self.v(arg);
                let ins = self.b.ins();
                match op {
                    UnOp::Ineg => ins.ineg(a),
                    UnOp::Bnot => ins.bnot(a),
                    UnOp::Iabs => ins.iabs(a),
                    UnOp::Clz => ins.clz(a),
                    UnOp::Fneg => ins.fneg(a),
                    UnOp::Fabs => ins.fabs(a),
                    UnOp::Sqrt => ins.sqrt(a),
                    UnOp::Floor => ins.floor(a),
                    UnOp::Ceil => ins.ceil(a),
                    UnOp::Trunc => ins.trunc(a),
                    UnOp::Nearest => ins.nearest(a),
                }
            }
            &InstData::Binary { op, a, b } => {
                let (a, b) = (self.v(a), self.v(b));
                let ins = self.b.ins();
                match op {
                    BinOp::Iadd => ins.iadd(a, b),
                    BinOp::Isub => ins.isub(a, b),
                    BinOp::Imul => ins.imul(a, b),
                    BinOp::Band => ins.band(a, b),
                    BinOp::Bor => ins.bor(a, b),
                    BinOp::Bxor => ins.bxor(a, b),
                    BinOp::BandNot => ins.band_not(a, b),
                    BinOp::Ishl => ins.ishl(a, b),
                    BinOp::Ushr => ins.ushr(a, b),
                    BinOp::Sshr => ins.sshr(a, b),
                    BinOp::Srem | BinOp::SremPositive => ins.srem(a, b),
                    BinOp::Fadd => ins.fadd(a, b),
                    BinOp::Fsub => ins.fsub(a, b),
                    BinOp::Fmul => ins.fmul(a, b),
                    BinOp::Fdiv => ins.fdiv(a, b),
                    BinOp::Fmin => ins.fmin(a, b),
                    BinOp::Fmax => ins.fmax(a, b),
                    BinOp::Fcopysign => ins.fcopysign(a, b),
                }
            }
            &InstData::Conv { op, ty: t, arg } => {
                let (a, t) = (self.v(arg), ty(t));
                let ins = self.b.ins();
                match op {
                    ConvOp::Uextend => ins.uextend(t, a),
                    ConvOp::Sextend => ins.sextend(t, a),
                    ConvOp::Ireduce => ins.ireduce(t, a),
                    ConvOp::FromSint => ins.fcvt_from_sint(t, a),
                    ConvOp::FromUint => ins.fcvt_from_uint(t, a),
                    ConvOp::ToSintSat | ConvOp::ToSintAny => ins.fcvt_to_sint_sat(t, a),
                    ConvOp::ToUintSat => ins.fcvt_to_uint_sat(t, a),
                    ConvOp::Fpromote => ins.fpromote(t, a),
                    ConvOp::Fdemote => ins.fdemote(t, a),
                    ConvOp::Bitcast => ins.bitcast(t, MemFlagsData::new(), a),
                }
            }
            &InstData::Icmp { cc, a, b } => {
                let (a, b) = (self.v(a), self.v(b));
                self.b.ins().icmp(int_cc(cc), a, b)
            }
            &InstData::Fcmp { cc, a, b } => {
                let (a, b) = (self.v(a), self.v(b));
                self.b.ins().fcmp(float_cc(cc), a, b)
            }
            &InstData::Select { cond, a, b } => {
                let (c, a, b) = (self.v(cond), self.v(a), self.v(b));
                self.b.ins().select(c, a, b)
            }
            &InstData::Overflow { sub, a, b } => {
                let (a, b) = (self.v(a), self.v(b));
                let (r, o) = if sub { self.b.ins().ssub_overflow(a, b) } else { self.b.ins().sadd_overflow(a, b) };
                self.set(results[1], o);
                r
            }
            &InstData::Load { ty: t, mem: m, signed, addr, off, .. } => {
                let a = self.v(addr);
                let ins = self.b.ins();
                match (m, signed) {
                    _ if m == t => ins.load(ty(t), mem, a, off),
                    (Type::I8, false) => ins.uload8(ty(t), mem, a, off),
                    (Type::I8, true) => ins.sload8(ty(t), mem, a, off),
                    (Type::I16, false) => ins.uload16(ty(t), mem, a, off),
                    (Type::I16, true) => ins.sload16(ty(t), mem, a, off),
                    (Type::I32, false) => ins.uload32(mem, a, off),
                    (Type::I32, true) => ins.sload32(mem, a, off),
                    _ => unreachable!("load of {m:?} as {t:?}"),
                }
            }
            &InstData::Store { mem: m, val, addr, off, .. } => {
                let (x, a) = (self.v(val), self.v(addr));
                let ins = self.b.ins();
                match m {
                    _ if m == f.value_type(val) => ins.store(mem, x, a, off),
                    Type::I8 => ins.istore8(mem, x, a, off),
                    Type::I16 => ins.istore16(mem, x, a, off),
                    Type::I32 => ins.istore32(mem, x, a, off),
                    _ => unreachable!("store of {m:?}"),
                };
                return;
            }
            &InstData::StackAddr { slot, off } => {
                let s = self.slots[slot.index()];
                self.b.ins().stack_addr(cl::types::I64, s, off)
            }
            &InstData::StackLoad { ty: t, slot, off } => {
                let s = self.slots[slot.index()];
                self.b.ins().stack_load(cl::types::I64, ty(t), s, off)
            }
            &InstData::StackStore { val, slot, off } => {
                let (x, s) = (self.v(val), self.slots[slot.index()]);
                self.b.ins().stack_store(cl::types::I64, x, s, off);
                return;
            }
            &InstData::StackStorePair { lo, hi, slot, off } => {
                let (lo, hi, s) = (self.v(lo), self.v(hi), self.slots[slot.index()]);
                let v = self.b.ins().scalar_to_vector(cl::types::I64X2, lo);
                let v = self.b.ins().insertlane(v, hi, 1);
                self.b.ins().stack_store(cl::types::I64, v, s, off);
                return;
            }
            &InstData::SymbolValue { data } => {
                let g = self.gv(data);
                self.b.ins().symbol_value(cl::types::I64, g)
            }
            &InstData::FuncAddr { func } => {
                let r = self.fref(func);
                self.b.ins().func_addr(cl::types::I64, r)
            }
            InstData::StackPointer => self.b.ins().get_stack_pointer(cl::types::I64),
            InstData::Call { func, args } => {
                let r = self.fref(*func);
                let args = self.vs(args);
                let call = self.b.ins().call(r, &args);
                let rs = self.b.inst_results(call).to_vec();
                for (&v, x) in results.iter().zip(rs) {
                    self.set(v, x);
                }
                return;
            }
            InstData::CallIndirect { sig, callee, args } => {
                let s = self.sig(*sig);
                let callee = self.v(*callee);
                let args = self.vs(args);
                let call = self.b.ins().call_indirect(s, callee, &args);
                let rs = self.b.inst_results(call).to_vec();
                for (&v, x) in results.iter().zip(rs) {
                    self.set(v, x);
                }
                return;
            }
            &InstData::UseVar { var } => self.b.use_var(self.vars[var.index()]),
            &InstData::DefVar { var, val } => {
                let x = self.v(val);
                self.b.def_var(self.vars[var.index()], x);
                return;
            }
            InstData::Jump { dest, args } => {
                let args = self.args(args);
                self.b.ins().jump(self.blocks[dest.index()], &args);
                return;
            }
            InstData::Brif { cond, then, then_args, els, else_args } => {
                let c = self.v(*cond);
                let (ta, ea) = (self.args(then_args), self.args(else_args));
                self.b.ins().brif(c, self.blocks[then.index()], &ta, self.blocks[els.index()], &ea);
                return;
            }
            InstData::Return { vals } => {
                let vals = self.vs(vals);
                self.b.ins().return_(&vals);
                return;
            }
            InstData::Trap => {
                self.b.ins().trap(cl::TrapCode::user(1).unwrap());
                return;
            }
        };
        self.set(results[0], r);
    }
}
