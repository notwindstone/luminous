//! A backend-neutral SSA IR. The lowering (`lower.rs`, and the entries in
//! `lib.rs`) builds it through a `FunctionBuilder` modeled on Cranelift's;
//! `clif.rs` translates it to Cranelift IR and `llvm.rs` to LLVM IR.
//!
//! - Values are typed scalars. Pointers are `I64`; booleans (comparison
//!   results) are `I8` zero or one, and any nonzero integer is true as a
//!   branch or `select` condition.
//! - Blocks take parameters (SSA phis), passed by `jump` and `brif`.
//! - Variables are mutable locals; each backend turns them into SSA values
//!   (Cranelift's SSA builder, LLVM's mem2reg). A variable read before any
//!   definition reads zero.
//! - Integer arithmetic wraps, shift amounts are taken modulo the width,
//!   float-to-int conversions saturate (NaN gives 0), `fmin` / `fmax`
//!   propagate NaN and order -0 below +0: Cranelift's semantics, which the
//!   LLVM translation keeps.

use std::rc::Rc;

/// The type of a value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Type {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    /// Two `I64` lanes: a whole `JSValue` (only stored, see
    /// `Ins::stack_store_pair`).
    I64X2,
}

impl Type {
    pub fn bits(self) -> u32 {
        match self {
            Type::I8 => 8,
            Type::I16 => 16,
            Type::I32 | Type::F32 => 32,
            Type::I64 | Type::F64 => 64,
            Type::I64X2 => 128,
        }
    }

    pub fn bytes(self) -> u32 {
        self.bits() / 8
    }

    pub fn is_int(self) -> bool {
        matches!(self, Type::I8 | Type::I16 | Type::I32 | Type::I64)
    }

    pub fn is_float(self) -> bool {
        matches!(self, Type::F32 | Type::F64)
    }

    /// `v` truncated to this integer type's width, sign-extended back: the
    /// canonical form of an integer constant of the type.
    pub fn canonical(self, v: i64) -> i64 {
        let shift = 64 - self.bits().min(64);
        (v << shift) >> shift
    }
}

pub mod types {
    use super::Type;
    pub const I8: Type = Type::I8;
    pub const I16: Type = Type::I16;
    pub const I32: Type = Type::I32;
    pub const I64: Type = Type::I64;
    pub const F32: Type = Type::F32;
    pub const F64: Type = Type::F64;
    pub const I64X2: Type = Type::I64X2;
}

macro_rules! entity {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub struct $name(pub(crate) u32);
        impl $name {
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    )*};
}

entity!(
    Value,
    Block,
    Inst,
    Variable,
    StackSlot,
    /// A function of the module (defined or imported).
    FuncId,
    /// A data object of the module (defined or imported).
    DataId,
    /// A signature for indirect calls.
    SigRef,
);

/// Functions are referenced by their module-level id.
pub type FuncRef = FuncId;
/// Data objects are referenced by their module-level id.
pub type GlobalValue = DataId;

/// Integer comparisons.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntCC {
    Equal,
    NotEqual,
    SignedLessThan,
    SignedGreaterThanOrEqual,
    SignedGreaterThan,
    SignedLessThanOrEqual,
    UnsignedLessThan,
    UnsignedGreaterThanOrEqual,
    UnsignedGreaterThan,
    UnsignedLessThanOrEqual,
}

/// Float comparisons. `NotEqual` is "unordered or not equal" (true for
/// NaN); the others are ordered (false for NaN) except `Unordered`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FloatCC {
    Ordered,
    Unordered,
    Equal,
    NotEqual,
    OrderedNotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

/// What a memory access touches, for alias analysis: accesses of two
/// different known regions never overlap. `Other` may overlap anything.
/// (Calls are not accesses: they may touch any memory but the stack slots
/// whose address they are not given.)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Region {
    #[default]
    Other,
    /// Reference counts, in the allocator header before a `JSValue`'s
    /// pointer.
    RefCount,
    /// `JSValue` words: properties (data or accessor), array elements,
    /// cells, boxes, closure data, async frames, argument slots.
    Slot,
    /// The engine's metadata, which native code only reads: object, shape,
    /// string, typed array, array buffer and function data headers.
    Header,
    /// Typed array elements.
    Elem,
    /// `aot type` objects' storage, and arrays' of them (`TsaotStruct`,
    /// `TsaotStructArray`, not their items).
    Struct,
    /// The inline caches (`tsaot_ics`), which native code only reads.
    Ic,
    /// Tables that only the runtime writes while native code runs
    /// (`tsaot_privates`, `tsaot_homes`).
    Table,
    /// Tables filled before any native code runs and never written again
    /// (`tsaot_atoms`, `tsaot_strings`, `tsaot_shapes`, the class ids).
    Const,
    /// `tsaot_unwinding`.
    Flag,
    /// `tsaot_stack_limit`.
    Limit,
}

impl Region {
    pub const ALL: [Region; 10] = [
        Region::RefCount,
        Region::Slot,
        Region::Header,
        Region::Elem,
        Region::Struct,
        Region::Ic,
        Region::Table,
        Region::Const,
        Region::Flag,
        Region::Limit,
    ];
}

/// Flags of a memory access. Every access is to valid, naturally aligned
/// memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MemFlags {
    pub region: Region,
}

impl MemFlags {
    pub fn new() -> Self {
        MemFlags::default()
    }

    pub fn trusted() -> Self {
        MemFlags::default()
    }

    pub const fn of(region: Region) -> Self {
        MemFlags { region }
    }
}

/// How a narrow integer argument or result is extended to a register.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ArgExt {
    None,
    Uext,
    Sext,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AbiParam {
    pub ty: Type,
    pub ext: ArgExt,
    /// A C pointer (an `I64` here): LLVM declares it as `ptr`.
    pub ptr: bool,
}

impl AbiParam {
    pub fn new(ty: Type) -> Self {
        AbiParam { ty, ext: ArgExt::None, ptr: false }
    }

    pub fn uext(self) -> Self {
        AbiParam { ext: ArgExt::Uext, ..self }
    }

    /// A pointer argument.
    pub fn pointer() -> Self {
        AbiParam { ptr: true, ..AbiParam::new(Type::I64) }
    }
}

/// A C-ABI signature.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Signature {
    pub params: Vec<AbiParam>,
    pub returns: Vec<AbiParam>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Linkage {
    /// Defined elsewhere.
    Import,
    /// Defined here, visible to other objects.
    Export,
    /// Defined here, only referenced by this module's code.
    Local,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackSlotKind {
    ExplicitSlot,
}

/// A stack slot: `size` bytes aligned to `1 << align_shift`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StackSlotData {
    pub size: u32,
    pub align_shift: u8,
}

impl StackSlotData {
    pub fn new(_kind: StackSlotKind, size: u32, align_shift: u8) -> Self {
        StackSlotData { size, align_shift }
    }
}

/// An argument passed to a block parameter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockArg {
    Value(Value),
}

impl BlockArg {
    pub fn value(self) -> Value {
        match self {
            BlockArg::Value(v) => v,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    Ineg,
    Bnot,
    Iabs,
    Clz,
    Fneg,
    Fabs,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    /// Round to nearest, ties to even.
    Nearest,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    Iadd,
    Isub,
    Imul,
    Band,
    Bor,
    Bxor,
    /// `a & !b`.
    BandNot,
    Ishl,
    Ushr,
    Sshr,
    /// Signed remainder; the divisor is never 0, and `MIN % -1` is 0.
    Srem,
    Fadd,
    Fsub,
    Fmul,
    Fdiv,
    Fmin,
    Fmax,
    Fcopysign,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConvOp {
    Uextend,
    Sextend,
    Ireduce,
    FromSint,
    FromUint,
    /// Saturating; NaN gives 0.
    ToSintSat,
    ToUintSat,
    Fpromote,
    Fdemote,
    Bitcast,
}

#[derive(Clone, Debug)]
pub enum InstData {
    Iconst {
        ty: Type,
        imm: i64,
    },
    /// Bits of an `F64`.
    F64const(u64),
    Unary {
        op: UnOp,
        arg: Value,
    },
    Binary {
        op: BinOp,
        a: Value,
        b: Value,
    },
    Conv {
        op: ConvOp,
        ty: Type,
        arg: Value,
    },
    Icmp {
        cc: IntCC,
        a: Value,
        b: Value,
    },
    Fcmp {
        cc: FloatCC,
        a: Value,
        b: Value,
    },
    Select {
        cond: Value,
        a: Value,
        b: Value,
    },
    /// Signed add / subtract with an overflow flag: results `(sum, I8 flag)`.
    Overflow {
        sub: bool,
        a: Value,
        b: Value,
    },
    /// A load of `mem` bytes extended (`signed` or not) to `ty`.
    Load {
        ty: Type,
        mem: Type,
        signed: bool,
        flags: MemFlags,
        addr: Value,
        off: i32,
    },
    /// A store of `val`, truncated to `mem`.
    Store {
        mem: Type,
        flags: MemFlags,
        val: Value,
        addr: Value,
        off: i32,
    },
    StackAddr {
        slot: StackSlot,
        off: i32,
    },
    StackLoad {
        ty: Type,
        slot: StackSlot,
        off: i32,
    },
    StackStore {
        val: Value,
        slot: StackSlot,
        off: i32,
    },
    /// One 16-byte store of `lo` and `hi` (a `JSValue`): C code copies
    /// `JSValue`s with 16-byte loads, which two 8-byte stores could not
    /// forward to.
    StackStorePair {
        lo: Value,
        hi: Value,
        slot: StackSlot,
        off: i32,
    },
    SymbolValue {
        data: DataId,
    },
    FuncAddr {
        func: FuncId,
    },
    StackPointer,
    Call {
        func: FuncId,
        args: Vec<Value>,
    },
    CallIndirect {
        sig: SigRef,
        callee: Value,
        args: Vec<Value>,
    },
    UseVar {
        var: Variable,
    },
    DefVar {
        var: Variable,
        val: Value,
    },
    Jump {
        dest: Block,
        args: Vec<Value>,
    },
    Brif {
        cond: Value,
        then: Block,
        then_args: Vec<Value>,
        els: Block,
        else_args: Vec<Value>,
    },
    Return {
        vals: Vec<Value>,
    },
    Trap,
}

impl InstData {
    pub fn is_terminator(&self) -> bool {
        matches!(self, InstData::Jump { .. } | InstData::Brif { .. } | InstData::Return { .. } | InstData::Trap)
    }

    /// The blocks this terminator may go to, with their arguments.
    pub fn successors(&self) -> Vec<(Block, &[Value])> {
        match self {
            InstData::Jump { dest, args } => vec![(*dest, args)],
            InstData::Brif { then, then_args, els, else_args, .. } => vec![(*then, then_args), (*els, else_args)],
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BlockData {
    pub params: Vec<Value>,
    pub insts: Vec<Inst>,
    pub cold: bool,
}

/// A function body.
#[derive(Clone, Debug)]
pub struct Function {
    pub sig: Signature,
    pub insts: Vec<InstData>,
    pub results: Vec<Vec<Value>>,
    pub value_types: Vec<Type>,
    pub blocks: Vec<BlockData>,
    /// The blocks with instructions, in the order they were started; the
    /// entry block first.
    pub layout: Vec<Block>,
    pub entry: Block,
    pub vars: Vec<Type>,
    pub slots: Vec<StackSlotData>,
    /// Not worth vectorizing (LLVM): long and branchy, like async step
    /// functions, whose frame spills the SLP vectorizer is slow on.
    pub no_vectorize: bool,
}

impl Function {
    pub fn value_type(&self, v: Value) -> Type {
        self.value_types[v.index()]
    }

    pub fn inst(&self, i: Inst) -> &InstData {
        &self.insts[i.index()]
    }

    pub fn inst_results(&self, i: Inst) -> &[Value] {
        &self.results[i.index()]
    }

    /// The terminator of block `b` (every block in the layout has one).
    pub fn terminator(&self, b: Block) -> &InstData {
        let last = *self.blocks[b.index()].insts.last().expect("filled block");
        self.inst(last)
    }
}

/// The signatures of a module's functions and indirect calls, for builders.
#[derive(Debug, Default)]
pub struct Decls {
    pub funcs: Vec<Signature>,
    pub sigs: Vec<Signature>,
}

/// Builds a `Function`, block by block.
pub struct FunctionBuilder {
    pub func: Function,
    decls: Rc<Decls>,
    current: Option<Block>,
    entry: Option<Block>,
}

impl FunctionBuilder {
    pub fn new(sig: Signature, decls: Rc<Decls>) -> Self {
        FunctionBuilder {
            func: Function {
                sig,
                insts: Vec::new(),
                results: Vec::new(),
                value_types: Vec::new(),
                blocks: Vec::new(),
                layout: Vec::new(),
                entry: Block(0),
                vars: Vec::new(),
                slots: Vec::new(),
                no_vectorize: false,
            },
            decls,
            current: None,
            entry: None,
        }
    }

    pub fn create_block(&mut self) -> Block {
        self.func.blocks.push(BlockData::default());
        Block(self.func.blocks.len() as u32 - 1)
    }

    pub fn switch_to_block(&mut self, b: Block) {
        debug_assert!(
            self.current.is_none_or(|c| self.func.blocks[c.index()].insts.is_empty() || self.is_filled()),
            "switching away from an unfinished block"
        );
        self.current = Some(b);
    }

    pub fn current_block(&self) -> Option<Block> {
        self.current
    }

    pub fn set_cold_block(&mut self, b: Block) {
        self.func.blocks[b.index()].cold = true;
    }

    fn new_value(&mut self, ty: Type) -> Value {
        self.func.value_types.push(ty);
        Value(self.func.value_types.len() as u32 - 1)
    }

    pub fn append_block_param(&mut self, b: Block, ty: Type) -> Value {
        let v = self.new_value(ty);
        self.func.blocks[b.index()].params.push(v);
        v
    }

    /// Makes `b` the entry block, with the function's parameters.
    pub fn append_block_params_for_function_params(&mut self, b: Block) {
        assert!(self.entry.is_none(), "one entry block");
        self.entry = Some(b);
        let tys: Vec<Type> = self.func.sig.params.iter().map(|p| p.ty).collect();
        for ty in tys {
            self.append_block_param(b, ty);
        }
    }

    pub fn block_params(&self, b: Block) -> &[Value] {
        &self.func.blocks[b.index()].params
    }

    pub fn inst_results(&self, i: Inst) -> &[Value] {
        self.func.inst_results(i)
    }

    pub fn value_type(&self, v: Value) -> Type {
        self.func.value_type(v)
    }

    pub fn declare_var(&mut self, ty: Type) -> Variable {
        self.func.vars.push(ty);
        Variable(self.func.vars.len() as u32 - 1)
    }

    pub fn def_var(&mut self, var: Variable, val: Value) {
        debug_assert_eq!(self.func.vars[var.index()], self.value_type(val), "def_var type");
        self.push(InstData::DefVar { var, val }, &[]);
    }

    pub fn use_var(&mut self, var: Variable) -> Value {
        let ty = self.func.vars[var.index()];
        self.push1(InstData::UseVar { var }, ty)
    }

    pub fn create_sized_stack_slot(&mut self, data: StackSlotData) -> StackSlot {
        self.func.slots.push(data);
        StackSlot(self.func.slots.len() as u32 - 1)
    }

    /// Whether the current block ends with a terminator.
    pub fn is_filled(&self) -> bool {
        let Some(cur) = self.current else { return false };
        self.func.blocks[cur.index()].insts.last().is_some_and(|&i| self.func.inst(i).is_terminator())
    }

    pub fn ins(&mut self) -> Ins<'_> {
        Ins { b: self }
    }

    fn push(&mut self, data: InstData, result_types: &[Type]) -> Inst {
        let cur = self.current.expect("no current block");
        debug_assert!(!self.is_filled(), "instruction after a terminator");
        if self.func.blocks[cur.index()].insts.is_empty() {
            self.func.layout.push(cur);
        }
        let inst = Inst(self.func.insts.len() as u32);
        self.func.insts.push(data);
        let results = result_types.iter().map(|&t| self.new_value(t)).collect();
        self.func.results.push(results);
        self.func.blocks[cur.index()].insts.push(inst);
        inst
    }

    fn push1(&mut self, data: InstData, ty: Type) -> Value {
        let inst = self.push(data, &[ty]);
        self.func.results[inst.index()][0]
    }

    /// The finished function. Every block that was started ends with a
    /// terminator, and branches only go to such blocks.
    pub fn finish(mut self) -> Function {
        let entry = self.entry.expect("an entry block");
        self.func.entry = entry;
        if let Some(i) = self.func.layout.iter().position(|&b| b == entry) {
            self.func.layout.remove(i);
        }
        self.func.layout.insert(0, entry);
        let started: Vec<bool> = self.func.blocks.iter().map(|b| !b.insts.is_empty()).collect();
        for &b in &self.func.layout {
            let term = self.func.terminator(b);
            assert!(term.is_terminator(), "block {b:?} is not terminated");
            for (dest, args) in term.successors() {
                assert!(started[dest.index()], "branch to the empty block {dest:?}");
                assert_eq!(args.len(), self.func.blocks[dest.index()].params.len(), "arguments of {dest:?}");
            }
        }
        self.func
    }
}

/// Instruction constructors, as `FunctionBuilder::ins()`.
pub struct Ins<'a> {
    b: &'a mut FunctionBuilder,
}

impl Ins<'_> {
    fn ty(&self, v: Value) -> Type {
        self.b.value_type(v)
    }

    pub fn iconst(self, ty: Type, imm: impl Into<i64>) -> Value {
        let imm = ty.canonical(imm.into());
        self.b.push1(InstData::Iconst { ty, imm }, ty)
    }

    pub fn f64const(self, v: f64) -> Value {
        self.b.push1(InstData::F64const(v.to_bits()), Type::F64)
    }

    fn unary(self, op: UnOp, arg: Value) -> Value {
        let ty = self.ty(arg);
        self.b.push1(InstData::Unary { op, arg }, ty)
    }

    fn binary(self, op: BinOp, a: Value, b: Value) -> Value {
        let ty = self.ty(a);
        debug_assert!(
            matches!(op, BinOp::Ishl | BinOp::Ushr | BinOp::Sshr) || ty == self.ty(b),
            "{op:?} operand types"
        );
        self.b.push1(InstData::Binary { op, a, b }, ty)
    }

    fn conv(self, op: ConvOp, ty: Type, arg: Value) -> Value {
        self.b.push1(InstData::Conv { op, ty, arg }, ty)
    }

    fn imm(&mut self, like: Value, imm: i64) -> Value {
        let ty = self.ty(like);
        let imm = ty.canonical(imm);
        self.b.push1(InstData::Iconst { ty, imm }, ty)
    }

    /// An immediate zero-extended from... nothing: immediates are 64-bit and
    /// truncated to the operand's type either way.
    fn imm_u(&mut self, like: Value, imm: i64) -> Value {
        self.imm(like, imm)
    }

    pub fn iadd(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Iadd, a, b)
    }
    pub fn isub(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Isub, a, b)
    }
    pub fn imul(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Imul, a, b)
    }
    pub fn band(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Band, a, b)
    }
    pub fn bor(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Bor, a, b)
    }
    pub fn bxor(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Bxor, a, b)
    }
    pub fn band_not(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::BandNot, a, b)
    }
    pub fn ishl(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Ishl, a, b)
    }
    pub fn ushr(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Ushr, a, b)
    }
    pub fn sshr(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Sshr, a, b)
    }
    pub fn srem(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Srem, a, b)
    }
    pub fn fadd(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Fadd, a, b)
    }
    pub fn fsub(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Fsub, a, b)
    }
    pub fn fmul(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Fmul, a, b)
    }
    pub fn fdiv(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Fdiv, a, b)
    }
    pub fn fmin(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Fmin, a, b)
    }
    pub fn fmax(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Fmax, a, b)
    }
    pub fn fcopysign(self, a: Value, b: Value) -> Value {
        self.binary(BinOp::Fcopysign, a, b)
    }

    pub fn iadd_imm_s(mut self, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm(a, imm.into());
        self.binary(BinOp::Iadd, a, c)
    }
    pub fn imul_imm_s(mut self, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm(a, imm.into());
        self.binary(BinOp::Imul, a, c)
    }
    pub fn band_imm_u(mut self, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm_u(a, imm.into());
        self.binary(BinOp::Band, a, c)
    }
    pub fn bxor_imm_u(mut self, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm_u(a, imm.into());
        self.binary(BinOp::Bxor, a, c)
    }
    pub fn ishl_imm_u(mut self, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm_u(a, imm.into());
        self.binary(BinOp::Ishl, a, c)
    }
    pub fn ushr_imm_u(mut self, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm_u(a, imm.into());
        self.binary(BinOp::Ushr, a, c)
    }

    pub fn ineg(self, a: Value) -> Value {
        self.unary(UnOp::Ineg, a)
    }
    pub fn bnot(self, a: Value) -> Value {
        self.unary(UnOp::Bnot, a)
    }
    pub fn iabs(self, a: Value) -> Value {
        self.unary(UnOp::Iabs, a)
    }
    pub fn clz(self, a: Value) -> Value {
        self.unary(UnOp::Clz, a)
    }
    pub fn fneg(self, a: Value) -> Value {
        self.unary(UnOp::Fneg, a)
    }
    pub fn fabs(self, a: Value) -> Value {
        self.unary(UnOp::Fabs, a)
    }
    pub fn sqrt(self, a: Value) -> Value {
        self.unary(UnOp::Sqrt, a)
    }
    pub fn floor(self, a: Value) -> Value {
        self.unary(UnOp::Floor, a)
    }
    pub fn ceil(self, a: Value) -> Value {
        self.unary(UnOp::Ceil, a)
    }
    pub fn trunc(self, a: Value) -> Value {
        self.unary(UnOp::Trunc, a)
    }
    pub fn nearest(self, a: Value) -> Value {
        self.unary(UnOp::Nearest, a)
    }

    pub fn icmp(self, cc: IntCC, a: Value, b: Value) -> Value {
        debug_assert_eq!(self.ty(a), self.ty(b), "icmp operand types");
        self.b.push1(InstData::Icmp { cc, a, b }, Type::I8)
    }
    pub fn icmp_imm_s(mut self, cc: IntCC, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm(a, imm.into());
        self.icmp(cc, a, c)
    }
    pub fn icmp_imm_u(mut self, cc: IntCC, a: Value, imm: impl Into<i64>) -> Value {
        let c = self.imm_u(a, imm.into());
        self.icmp(cc, a, c)
    }
    pub fn fcmp(self, cc: FloatCC, a: Value, b: Value) -> Value {
        self.b.push1(InstData::Fcmp { cc, a, b }, Type::I8)
    }

    pub fn select(self, cond: Value, a: Value, b: Value) -> Value {
        let ty = self.ty(a);
        debug_assert_eq!(ty, self.ty(b), "select operand types");
        self.b.push1(InstData::Select { cond, a, b }, ty)
    }

    pub fn uextend(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::Uextend, ty, a)
    }
    pub fn sextend(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::Sextend, ty, a)
    }
    pub fn ireduce(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::Ireduce, ty, a)
    }
    pub fn fcvt_from_sint(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::FromSint, ty, a)
    }
    pub fn fcvt_from_uint(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::FromUint, ty, a)
    }
    pub fn fcvt_to_sint_sat(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::ToSintSat, ty, a)
    }
    pub fn fcvt_to_uint_sat(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::ToUintSat, ty, a)
    }
    pub fn fpromote(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::Fpromote, ty, a)
    }
    pub fn fdemote(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::Fdemote, ty, a)
    }
    pub fn bitcast(self, ty: Type, a: Value) -> Value {
        self.conv(ConvOp::Bitcast, ty, a)
    }

    fn overflow(self, sub: bool, a: Value, b: Value) -> (Value, Value) {
        let ty = self.ty(a);
        let inst = self.b.push(InstData::Overflow { sub, a, b }, &[ty, Type::I8]);
        let r = &self.b.func.results[inst.index()];
        (r[0], r[1])
    }
    pub fn sadd_overflow(self, a: Value, b: Value) -> (Value, Value) {
        self.overflow(false, a, b)
    }
    pub fn ssub_overflow(self, a: Value, b: Value) -> (Value, Value) {
        self.overflow(true, a, b)
    }

    fn load_ext(self, ty: Type, mem: Type, signed: bool, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.b.push1(InstData::Load { ty, mem, signed, flags, addr, off }, ty)
    }
    pub fn load(self, ty: Type, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.load_ext(ty, ty, false, flags, addr, off)
    }
    pub fn uload8(self, ty: Type, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.load_ext(ty, Type::I8, false, flags, addr, off)
    }
    pub fn sload8(self, ty: Type, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.load_ext(ty, Type::I8, true, flags, addr, off)
    }
    pub fn uload16(self, ty: Type, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.load_ext(ty, Type::I16, false, flags, addr, off)
    }
    pub fn sload16(self, ty: Type, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.load_ext(ty, Type::I16, true, flags, addr, off)
    }
    /// A `u32` zero-extended to `I64`.
    pub fn uload32(self, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.load_ext(Type::I64, Type::I32, false, flags, addr, off)
    }
    /// An `i32` sign-extended to `I64`.
    pub fn sload32(self, flags: MemFlags, addr: Value, off: i32) -> Value {
        self.load_ext(Type::I64, Type::I32, true, flags, addr, off)
    }

    pub fn store(self, flags: MemFlags, val: Value, addr: Value, off: i32) -> Inst {
        let mem = self.ty(val);
        self.b.push(InstData::Store { mem, flags, val, addr, off }, &[])
    }
    pub fn istore8(self, flags: MemFlags, val: Value, addr: Value, off: i32) -> Inst {
        self.b.push(InstData::Store { mem: Type::I8, flags, val, addr, off }, &[])
    }
    pub fn istore16(self, flags: MemFlags, val: Value, addr: Value, off: i32) -> Inst {
        self.b.push(InstData::Store { mem: Type::I16, flags, val, addr, off }, &[])
    }

    pub fn stack_addr(self, _ptr: Type, slot: StackSlot, off: i32) -> Value {
        self.b.push1(InstData::StackAddr { slot, off }, Type::I64)
    }
    pub fn stack_load(self, _ptr: Type, ty: Type, slot: StackSlot, off: i32) -> Value {
        self.b.push1(InstData::StackLoad { ty, slot, off }, ty)
    }
    pub fn stack_store(self, _ptr: Type, val: Value, slot: StackSlot, off: i32) -> Inst {
        self.b.push(InstData::StackStore { val, slot, off }, &[])
    }
    pub fn stack_store_pair(self, lo: Value, hi: Value, slot: StackSlot, off: i32) -> Inst {
        self.b.push(InstData::StackStorePair { lo, hi, slot, off }, &[])
    }

    pub fn symbol_value(self, _ptr: Type, data: DataId) -> Value {
        self.b.push1(InstData::SymbolValue { data }, Type::I64)
    }
    pub fn func_addr(self, _ptr: Type, func: FuncId) -> Value {
        self.b.push1(InstData::FuncAddr { func }, Type::I64)
    }
    pub fn get_stack_pointer(self, _ptr: Type) -> Value {
        self.b.push1(InstData::StackPointer, Type::I64)
    }

    pub fn call(self, func: FuncId, args: &[Value]) -> Inst {
        let decls = self.b.decls.clone();
        let sig = &decls.funcs[func.index()];
        debug_assert_eq!(sig.params.len(), args.len(), "call arguments");
        let rets: Vec<Type> = sig.returns.iter().map(|r| r.ty).collect();
        self.b.push(InstData::Call { func, args: args.to_vec() }, &rets)
    }

    pub fn call_indirect(self, sig: SigRef, callee: Value, args: &[Value]) -> Inst {
        let decls = self.b.decls.clone();
        let rets: Vec<Type> = decls.sigs[sig.index()].returns.iter().map(|r| r.ty).collect();
        self.b.push(InstData::CallIndirect { sig, callee, args: args.to_vec() }, &rets)
    }

    pub fn jump(self, dest: Block, args: &[BlockArg]) -> Inst {
        let args = args.iter().map(|a| a.value()).collect();
        self.b.push(InstData::Jump { dest, args }, &[])
    }

    pub fn brif(self, cond: Value, then: Block, then_args: &[BlockArg], els: Block, else_args: &[BlockArg]) -> Inst {
        let then_args = then_args.iter().map(|a| a.value()).collect();
        let else_args = else_args.iter().map(|a| a.value()).collect();
        self.b.push(InstData::Brif { cond, then, then_args, els, else_args }, &[])
    }

    pub fn return_(self, vals: &[Value]) -> Inst {
        self.b.push(InstData::Return { vals: vals.to_vec() }, &[])
    }

    pub fn trap(self) -> Inst {
        self.b.push(InstData::Trap, &[])
    }
}

/// A multi-way branch on an integer, as Cranelift's `Switch`.
#[derive(Default)]
pub struct Switch {
    entries: Vec<(u128, Block)>,
}

impl Switch {
    pub fn new() -> Self {
        Switch::default()
    }

    pub fn set_entry(&mut self, k: u128, b: Block) {
        self.entries.push((k, b));
    }

    /// Branch on `val` to the entries' blocks, else to `default`.
    pub fn emit(self, b: &mut FunctionBuilder, val: Value, default: Block) {
        for (k, dest) in self.entries {
            let hit = b.ins().icmp_imm_s(IntCC::Equal, val, k as i64);
            let next = b.create_block();
            b.ins().brif(hit, dest, &[], next, &[]);
            b.switch_to_block(next);
        }
        b.ins().jump(default, &[]);
    }
}

/// Initial contents of a data object defined by the module.
#[derive(Clone, Debug)]
pub enum DataInit {
    Zeroed { size: usize },
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct FuncDecl {
    pub name: String,
    pub sig: Signature,
    pub linkage: Linkage,
    pub body: Option<Function>,
}

#[derive(Clone, Debug)]
pub struct DataDecl {
    pub name: String,
    pub linkage: Linkage,
    pub writable: bool,
    pub align: u32,
    pub init: Option<DataInit>,
}

/// A module: functions and data objects, defined or imported.
#[derive(Default)]
pub struct Module {
    pub funcs: Vec<FuncDecl>,
    pub data: Vec<DataDecl>,
    pub sigs: Vec<Signature>,
    decls: Option<Rc<Decls>>,
}

impl Module {
    pub fn new() -> Self {
        Module::default()
    }

    pub fn declare_function(&mut self, name: &str, linkage: Linkage, sig: &Signature) -> FuncId {
        assert!(self.funcs.iter().all(|f| f.name != name), "function {name} declared twice");
        self.decls = None;
        self.funcs.push(FuncDecl { name: name.to_string(), sig: sig.clone(), linkage, body: None });
        FuncId(self.funcs.len() as u32 - 1)
    }

    pub fn declare_data(&mut self, name: &str, linkage: Linkage, writable: bool) -> DataId {
        self.data.push(DataDecl { name: name.to_string(), linkage, writable, align: 8, init: None });
        DataId(self.data.len() as u32 - 1)
    }

    pub fn import_signature(&mut self, sig: Signature) -> SigRef {
        if let Some(i) = self.sigs.iter().position(|s| *s == sig) {
            return SigRef(i as u32);
        }
        self.decls = None;
        self.sigs.push(sig);
        SigRef(self.sigs.len() as u32 - 1)
    }

    /// A builder for a function of signature `sig`.
    pub fn builder(&mut self, sig: Signature) -> FunctionBuilder {
        let decls = self
            .decls
            .get_or_insert_with(|| {
                Rc::new(Decls { funcs: self.funcs.iter().map(|f| f.sig.clone()).collect(), sigs: self.sigs.clone() })
            })
            .clone();
        FunctionBuilder::new(sig, decls)
    }

    pub fn define_function(&mut self, id: FuncId, f: Function) {
        let decl = &mut self.funcs[id.index()];
        assert!(decl.linkage != Linkage::Import && decl.body.is_none(), "{} defined twice", decl.name);
        debug_assert_eq!(decl.sig, f.sig, "{}: signature", decl.name);
        decl.body = Some(f);
    }

    pub fn define_data(&mut self, id: DataId, init: DataInit, align: u32) {
        let d = &mut self.data[id.index()];
        assert!(d.linkage != Linkage::Import && d.init.is_none(), "{} defined twice", d.name);
        d.init = Some(init);
        d.align = align;
    }
}
