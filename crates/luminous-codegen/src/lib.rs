//! Lowers typed IR to native code and emits a relocatable object file, with
//! either of two backends:
//! - Cranelift (`clif.rs`): fast compilation, in process;
//! - LLVM (`llvm.rs`): LLVM IR compiled by `clang -O3`, slower to compile
//!   but optimized much further.
//!
//! The lowering (`lower.rs`) builds a backend-neutral SSA IR (`mir.rs`),
//! which each backend translates.
//!
//! Every IR function `i` produces two symbols:
//! - `lum_fn_<i>(JSContext *ctx, params...)`: the function itself. `number`
//!   / `boolean` values are passed unboxed (`double` / zero-extended `uint8_t`),
//!   other values as `JSValue`s (two 64-bit words, owned by the callee).
//!   Native callers call this directly.
//! - `lum_fast_<i>`: its entry from JS, a QuickJS-NG `JSCFunctionFast` that
//!   the interpreter's call opcodes invoke directly on their stack slots. The
//!   type guard, unboxing and boxing are all inline machine code here.
//!
//! Exceptions propagate between native functions through the
//! `lum_unwinding` flag: a function that exits with an exception sets it,
//! and every caller checks it after each native call. Stack overflow is just
//! another exception (a `RangeError`, thrown when the stack pointer crosses
//! `lum_stack_limit`).

mod clif;
pub mod llvm;
mod lower;
pub mod mir;

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use luminous_front::ir::{self, Expr, ExprKind, MathFn, Stmt, Ty};
use mir::types::{F64, I8, I32, I64};
use mir::{
    AbiParam, Block, BlockArg, DataId, FloatCC, FuncId, FuncRef, FunctionBuilder, IntCC, Linkage, MemFlags, SigRef,
    Signature, StackSlotData, StackSlotKind, Value,
};

use lower::{Env, Exit, Exits, Lower, M_CONST, M_FLAG, M_HEADER, M_SLOT, M_STRUCT, V, clif_types, from_values};

pub fn symbol_name(id: ir::FuncId) -> String {
    format!("lum_fn_{id}")
}

pub fn fast_symbol_name(id: ir::FuncId) -> String {
    format!("lum_fast_{id}")
}

/// Native stack available below the point where the runtime is initialized.
/// QuickJS keeps its own usage under `JS_DEFAULT_STACK_SIZE` (1 MiB), so both
/// fit a default 8 MiB stack.
pub const NATIVE_STACK_BUDGET: u64 = 4 * 1024 * 1024;

/// `JSValue`s (the engine's `JS_COMPRESSED_POINTERS`): 8 bytes, NaN-boxed.
/// A double is its own bits (no NaN but those `val::float` allows); any
/// other value has a high half no double has (its tag plus `TAG_BASE`:
/// 0xffffffef to 0xffffffff) and its payload in the low half: an int32, a
/// bool, or a reference (the address of a cell / 8; cells are below 32
/// GB). The generated C glue `_Static_assert`s these.
pub mod jsvalue {
    pub const SIZE: usize = 8;
    /// The lowest tag (`JS_TAG_FIRST`): the tags of references are
    /// `FIRST_TAG..0`.
    pub const FIRST_TAG: i64 = -9;
    pub const TAG_BIG_INT: i64 = -9;
    pub const TAG_SYMBOL: i64 = -8;
    pub const TAG_STRING: i64 = -7;
    pub const TAG_STRING_ROPE: i64 = -6;
    pub const TAG_OBJECT: i64 = -1;
    pub const TAG_INT: i64 = 0;
    pub const TAG_BOOL: i64 = 1;
    pub const TAG_NULL: i64 = 2;
    pub const TAG_UNDEFINED: i64 = 3;
    pub const TAG_UNINITIALIZED: i64 = 4;
    pub const TAG_EXCEPTION: i64 = 6;
    pub const TAG_SHORT_BIG_INT: i64 = 7;
    pub const TAG_FLOAT64: i64 = 8;
    /// What a tag's high half is above the tag (`JS_TAG_BASE`).
    pub const TAG_BASE: u32 = 0xffff_fff8;
    /// The bits of the double NaN values hold.
    pub const CANONICAL_NAN_BITS: i64 = 0x7ff8_0000_0000_0000;

    /// The value of a tag that is no float's, with a 32-bit payload.
    pub const fn val_const(tag: i64, payload: i64) -> i64 {
        (((tag as u32).wrapping_add(TAG_BASE) as u64) << 32 | (payload as u64 & 0xffff_ffff)) as i64
    }

    /// Values below this are floats.
    pub const FLOATS_END: i64 = val_const(FIRST_TAG, 0);
}

/// Values (see `jsvalue`) in IR: their tests, decoding and making.
pub(crate) mod val {
    use super::jsvalue::*;
    use crate::mir::types::{F64, I32, I64};
    use crate::mir::{FunctionBuilder, IntCC, MemFlags, Value};

    /// The high half of `w` (its tag's, but for a float), as an `I32`:
    /// compared with 32-bit immediates.
    fn high(b: &mut FunctionBuilder, w: Value) -> Value {
        let hi = b.ins().ushr_imm_u(w, 32);
        b.ins().ireduce(I32, hi)
    }

    /// What `high` is for tag `tag`, sign-extended.
    const fn tag_high(tag: i64) -> i64 {
        (val_const(tag, 0) >> 32) as i32 as i64
    }

    /// Whether `w` has tag `tag`.
    pub fn is(b: &mut FunctionBuilder, w: Value, tag: i64) -> Value {
        if tag == TAG_FLOAT64 {
            return is_float(b, w);
        }
        let hi = high(b, w);
        b.ins().icmp_imm_s(IntCC::Equal, hi, tag_high(tag))
    }

    /// Whether `w` is a float (a double, its high half below the tags').
    pub fn is_float(b: &mut FunctionBuilder, w: Value) -> Value {
        b.ins().icmp_imm_u(IntCC::UnsignedLessThan, w, FLOATS_END)
    }

    /// Whether the value at `addr + off` is a float, from its high half
    /// alone (a 32-bit load, little-endian): the value itself, read where
    /// it is used, then goes straight to a float register, not through
    /// the integer one this test would otherwise have loaded it into.
    pub fn slot_is_float(b: &mut FunctionBuilder, flags: MemFlags, addr: Value, off: i32) -> Value {
        let hi = b.ins().uload32(flags, addr, off + 4);
        b.ins().icmp_imm_u(IntCC::UnsignedLessThan, hi, (FLOATS_END as u64 >> 32) as i64)
    }

    /// Whether `w` is a number: an int32 or a float.
    pub fn is_number(b: &mut FunctionBuilder, w: Value) -> Value {
        let float = is_float(b, w);
        let int = is(b, w, TAG_INT);
        b.ins().bor(int, float)
    }

    /// Whether `w` has a tag in `lo..=hi` (one compare, after moving
    /// `lo`'s values to 0).
    pub fn tag_in(b: &mut FunctionBuilder, w: Value, lo: i64, hi: i64) -> Value {
        let h = high(b, w);
        let x = b.ins().iadd_imm_s(h, tag_high(lo).wrapping_neg());
        b.ins().icmp_imm_u(IntCC::UnsignedLessThan, x, hi - lo + 1)
    }

    /// Whether `w` is a reference (to a cell).
    pub fn is_ref(b: &mut FunctionBuilder, w: Value) -> Value {
        tag_in(b, w, FIRST_TAG, TAG_OBJECT)
    }

    /// The address a reference `w` holds. (64-bit throughout: the
    /// collector takes the low half of a word for a reference only under
    /// a reference's tag, or alone, never as a 32-bit value beside
    /// another.)
    pub fn ptr(b: &mut FunctionBuilder, w: Value) -> Value {
        let lo = b.ins().band_imm_u(w, 0xffff_ffffi64);
        b.ins().ishl_imm_u(lo, 3)
    }

    /// A reference to the cell at `p`, tagged `tag`.
    pub fn mk_ref(b: &mut FunctionBuilder, tag: i64, p: Value) -> Value {
        let r = b.ins().ushr_imm_u(p, 3);
        b.ins().bor_imm_u(r, val_const(tag, 0))
    }

    /// The int32 payload of `w` (an int, a bool), sign-extended to `I64`.
    pub fn int(b: &mut FunctionBuilder, w: Value) -> Value {
        let i = b.ins().ireduce(I32, w);
        b.ins().sextend(I64, i)
    }

    /// The int value of `i` (an integer in the int32 range, or a `u32`
    /// below 2^31, in an `I64`).
    pub fn mk_int(b: &mut FunctionBuilder, i: Value) -> Value {
        let lo = b.ins().band_imm_u(i, 0xffff_ffffi64);
        b.ins().bor_imm_u(lo, val_const(TAG_INT, 0))
    }

    /// The double a float value `w` holds: its bits.
    pub fn f64(b: &mut FunctionBuilder, w: Value) -> Value {
        b.ins().bitcast(F64, w)
    }

    /// The number in `w`, an int32 or a float.
    pub fn number_of(b: &mut FunctionBuilder, w: Value) -> Value {
        let i = b.ins().ireduce(I32, w);
        let fi = b.ins().fcvt_from_sint(F64, i);
        let ff = f64(b, w);
        let is_int = is(b, w, TAG_INT);
        b.ins().select(is_int, fi, ff)
    }

    /// The float value of `d`: its bits. Some NaNs' bits would be other
    /// values' (those whose high half is a tag's, 0xffffffef and up):
    /// native code has none. The engine's values hold only the canonical
    /// NaN, typed array loads make theirs that one (`Lower::load_element`),
    /// and arithmetic makes the default NaN or passes on one of those
    /// (`MAYBE_F64_UNDEFINED`'s high half is the canonical one's too).
    pub fn float(b: &mut FunctionBuilder, d: Value) -> Value {
        b.ins().bitcast(I64, d)
    }

    /// The bool value of `x` (an `I8` 0 or 1).
    pub fn boolean(b: &mut FunctionBuilder, x: Value) -> Value {
        let x = b.ins().uextend(I64, x);
        b.ins().bor_imm_u(x, val_const(TAG_BOOL, 0))
    }
}

/// Engine memory layout read by inline fast paths: the `JS_LAYOUT_*`
/// constants of the engine patch, which the generated C glue
/// `_Static_assert`s against these values.
pub mod layout {
    pub const GC_FLAGS: i32 = -5;
    pub const STRING_LEN: i32 = 0;
    pub const OBJ_CLASS_ID: i32 = 2;
    pub const OBJ_SHAPE: i32 = 8;
    pub const SHAPE_PROTO: i32 = 32;
    pub const OBJ_PROP: i32 = 16;
    pub const OBJ_TYPED_ARRAY: i32 = 24;
    pub const OBJ_ARRAY_BUFFER: i32 = 24;
    pub const OBJ_ARRAY_VALUES: i32 = 32;
    pub const OBJ_ARRAY_COUNT: i32 = 40;
    pub const TA_BUFFER: i32 = 24;
    pub const ABUF_IMMUTABLE: i32 = 9;
    pub const CLASS_ARRAY: i64 = 2;
    pub const CLASS_TYPED_ARRAY: i64 = 22;
    pub const OBJ_FUNC_DATA: i32 = 24;
    pub const FDATA_FAST: i32 = 8;
    pub const FDATA_DATA: i32 = 32;
    pub const CLASS_C_FUNCTION_DATA: i64 = 15;
    pub const OBJ_OPAQUE: i32 = 24;
    // Stepping a shape's shared for-in iterator (see `JS_ForInStart`).
    pub const FORIN_SHAPE: i32 = 24;
    pub const FORIN_KEYS: i32 = 40;
    // Making closures inline (see `JS_CFunctionDataTemplate`).
    pub const FDATA_SELF: i32 = 16;
    pub const OBJ_CLASS_SIZE: i32 = 48;
    pub const FTMPL_FLAGS: i32 = 0;
    pub const FTMPL_SHAPE: i32 = 8;
    pub const FTMPL_PROPS: i32 = 16;
    pub const FTMPL_RECORD: i32 = 32;
    pub const CLASS_C_FUNCTION: i64 = 12;
    // `typeof`: below the proxy's class, the callable classes are
    // `CLASS_C_FUNCTION..=CLASS_GENERATOR_FUNCTION`.
    pub const CLASS_GENERATOR_FUNCTION: i64 = 17;
    pub const CLASS_PROXY: i64 = 51;
    pub const STRING_KIND: i32 = 4;
    pub const STRING_DATA: i32 = 24;
    // Allocating plain objects inline (see `JS_ShapeAllocInfo`).
    pub const CTX_RT: i32 = 0;
    pub const OBJ_FLAGS: i32 = 0;
    pub const OBJ_PLAIN_FLAGS: i64 = 0x10002;
    pub const OBJ_WEAK_REF: i32 = 4;
    pub const OBJ_SIZE: i32 = 24;
    // Appending to a fast array inline.
    pub const OBJ_ARRAY_SIZE: i32 = 24;
    pub const OBJ_APPEND_MASK: i64 = 0xffff_0012;
    pub const OBJ_APPEND_FLAGS: i64 = 0x2_0012;
    pub const CTX_STD_ARRAY_PROTO: i32 = 32;
    // Reading global variables inline.
    pub const CTX_GLOBAL_OBJ: i32 = 288;
    pub const CTX_GLOBAL_VAR_OBJ: i32 = 296;
    pub const SHAPE_PROP_COUNT: i32 = 16;
    pub const SHAPE_PROP_SIZE: i32 = 12;
    pub const CLASS_MAP: i64 = 36;
    pub const CLASS_SET: i64 = 37;
    // Allocating GC cells inline.
    pub const RT_GC_RUNS: i32 = 2288;
    pub const GC_RUN_SIZE: i32 = 16;
    pub const GC_CELL_WORD: i64 = 0xfe0000;

    pub const ALL: &[(&str, i64)] = &[
        ("GC_FLAGS", GC_FLAGS as i64),
        ("STRING_LEN", STRING_LEN as i64),
        ("OBJ_CLASS_ID", OBJ_CLASS_ID as i64),
        ("OBJ_SHAPE", OBJ_SHAPE as i64),
        ("SHAPE_PROTO", SHAPE_PROTO as i64),
        ("OBJ_PROP", OBJ_PROP as i64),
        ("OBJ_TYPED_ARRAY", OBJ_TYPED_ARRAY as i64),
        ("OBJ_ARRAY_BUFFER", OBJ_ARRAY_BUFFER as i64),
        ("OBJ_ARRAY_VALUES", OBJ_ARRAY_VALUES as i64),
        ("OBJ_ARRAY_COUNT", OBJ_ARRAY_COUNT as i64),
        ("TA_BUFFER", TA_BUFFER as i64),
        ("ABUF_IMMUTABLE", ABUF_IMMUTABLE as i64),
        ("CLASS_ARRAY", CLASS_ARRAY),
        ("CLASS_TYPED_ARRAY", CLASS_TYPED_ARRAY),
        ("OBJ_FUNC_DATA", OBJ_FUNC_DATA as i64),
        ("FDATA_FAST", FDATA_FAST as i64),
        ("FDATA_DATA", FDATA_DATA as i64),
        ("CLASS_C_FUNCTION_DATA", CLASS_C_FUNCTION_DATA),
        ("OBJ_OPAQUE", OBJ_OPAQUE as i64),
        ("FORIN_SHAPE", FORIN_SHAPE as i64),
        ("FORIN_KEYS", FORIN_KEYS as i64),
        ("FDATA_SELF", FDATA_SELF as i64),
        ("OBJ_CLASS_SIZE", OBJ_CLASS_SIZE as i64),
        ("FTMPL_FLAGS", FTMPL_FLAGS as i64),
        ("FTMPL_SHAPE", FTMPL_SHAPE as i64),
        ("FTMPL_PROPS", FTMPL_PROPS as i64),
        ("FTMPL_RECORD", FTMPL_RECORD as i64),
        ("CLASS_C_FUNCTION", CLASS_C_FUNCTION),
        ("CLASS_GENERATOR_FUNCTION", CLASS_GENERATOR_FUNCTION),
        ("CLASS_PROXY", CLASS_PROXY),
        ("STRING_KIND", STRING_KIND as i64),
        ("STRING_DATA", STRING_DATA as i64),
        ("CTX_RT", CTX_RT as i64),
        ("OBJ_FLAGS", OBJ_FLAGS as i64),
        ("OBJ_PLAIN_FLAGS", OBJ_PLAIN_FLAGS),
        ("OBJ_WEAK_REF", OBJ_WEAK_REF as i64),
        ("OBJ_SIZE", OBJ_SIZE as i64),
        ("OBJ_ARRAY_SIZE", OBJ_ARRAY_SIZE as i64),
        ("OBJ_APPEND_MASK", OBJ_APPEND_MASK),
        ("OBJ_APPEND_FLAGS", OBJ_APPEND_FLAGS),
        ("CTX_STD_ARRAY_PROTO", CTX_STD_ARRAY_PROTO as i64),
        ("CTX_GLOBAL_OBJ", CTX_GLOBAL_OBJ as i64),
        ("CTX_GLOBAL_VAR_OBJ", CTX_GLOBAL_VAR_OBJ as i64),
        ("SHAPE_PROP_SIZE", SHAPE_PROP_SIZE as i64),
        ("SHAPE_PROP_COUNT", SHAPE_PROP_COUNT as i64),
        ("CLASS_MAP", CLASS_MAP),
        ("CLASS_SET", CLASS_SET),
        ("RT_GC_RUNS", RT_GC_RUNS as i64),
        ("GC_RUN_SIZE", GC_RUN_SIZE as i64),
        ("GC_CELL_WORD", GC_CELL_WORD),
    ];
}

/// The index of `lum_ics` among `Codegen::data`.
const DATA_ICS: usize = 11;

/// Which code generator compiles the native functions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Backend {
    /// Cranelift: compiles fast, in process.
    #[default]
    Cranelift,
    /// LLVM, through `clang`: compiles slower, optimizes much further.
    Llvm,
}

impl std::str::FromStr for Backend {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "cranelift" | "clif" => Ok(Backend::Cranelift),
            "llvm" => Ok(Backend::Llvm),
            _ => Err(format!("unknown backend `{s}` (expected `cranelift` or `llvm`)")),
        }
    }
}

pub struct Options {
    pub backend: Backend,
    /// Cranelift `opt_level`: "none", "speed" or "speed_and_size".
    pub opt_level: String,
    /// LLVM: how to run clang.
    pub llvm: llvm::Options,
}

impl Default for Options {
    fn default() -> Self {
        Options { backend: Backend::Cranelift, opt_level: "speed".into(), llvm: Default::default() }
    }
}

/// What a backend produced for the native functions.
pub enum Output {
    /// Relocatable object files, linked together.
    Objects(Vec<Vec<u8>>),
    /// LLVM modules (textual IR), compiled by `llvm::compile_parts` and
    /// linked together.
    LlvmIr(Vec<String>),
}

/// Lower `m` to the backend-neutral IR.
pub fn lower_module(m: &ir::Module) -> Result<mir::Module> {
    let mut cg = Codegen::new(m);
    for (i, f) in m.funcs.iter().chain(&m.closures).enumerate() {
        cg.define(i, f).with_context(|| format!("compiling `{}`", f.name))?;
        cg.define_fast(i, f).with_context(|| format!("compiling the entry of `{}`", f.name))?;
    }
    let mut cmps: Vec<(usize, FuncId)> = cg.sort_cmps.iter().map(|(&i, &id)| (i, id)).collect();
    cmps.sort();
    for (i, id) in cmps {
        let f = m.funcs.iter().chain(&m.closures).nth(i).expect("comparator");
        cg.define_as(id, i, f, true).with_context(|| format!("compiling the comparator `{}`", f.name))?;
        cg.define_sorter(cg.sorters[&i], id, f);
    }
    cg.define_ics();
    Ok(cg.module)
}

/// Compile `m` for the host with the backend `opts` selects.
pub fn compile(m: &ir::Module, opts: &Options) -> Result<Output> {
    let t0 = std::time::Instant::now();
    let module = lower_module(m)?;
    time("lowering", t0);
    let t0 = std::time::Instant::now();
    let out = match opts.backend {
        Backend::Cranelift => Output::Objects(clif::emit_parts(&module, &opts.opt_level, opts.llvm.jobs)?),
        Backend::Llvm => Output::LlvmIr(llvm::emit_parts(&module, &opts.llvm, opts.llvm.jobs)?),
    };
    time("backend", t0);
    Ok(out)
}

/// With $LUMINOUS_TIMES, prints how long a phase of a build took.
pub fn time(phase: &str, since: std::time::Instant) {
    if std::env::var_os("LUMINOUS_TIMES").is_some() {
        eprintln!("luminous: {phase}: {:.2}s", since.elapsed().as_secs_f64());
    }
}

/// `lum_sort_<i>`'s signature: (ctx, values, room for half of them and
/// one more, count) -> 1 if the comparator threw, else 0.
fn sorter_signature() -> Signature {
    Signature {
        params: vec![AbiParam::pointer(), AbiParam::pointer(), AbiParam::pointer(), AbiParam::new(I64)],
        returns: vec![AbiParam::new(I32)],
    }
}

/// JS value `i` of the array at `a`.
fn elem(b: &mut FunctionBuilder, a: Value, i: Value) -> Value {
    let off = b.ins().imul_imm_s(i, jsvalue::SIZE as i64);
    let at = b.ins().iadd(a, off);
    b.ins().load(I64, M_SLOT, at, 0)
}

fn set_elem(b: &mut FunctionBuilder, a: Value, i: Value, w: Value) {
    let off = b.ins().imul_imm_s(i, jsvalue::SIZE as i64);
    let at = b.ins().iadd(a, off);
    b.ins().store(M_SLOT, w, at, 0);
}

/// Copy `n` JS values from `src` to `dst` (in ascending order).
fn copy_elems(b: &mut FunctionBuilder, dst: Value, src: Value, n: Value) {
    let k = b.declare_var(I64);
    let zero = b.ins().iconst(I64, 0);
    b.def_var(k, zero);
    let head = b.create_block();
    let body = b.create_block();
    let out = b.create_block();
    b.ins().jump(head, &[]);
    b.switch_to_block(head);
    let kv = b.use_var(k);
    let more = b.ins().icmp(IntCC::SignedLessThan, kv, n);
    b.ins().brif(more, body, &[], out, &[]);
    b.switch_to_block(body);
    let kv = b.use_var(k);
    let v = elem(b, src, kv);
    set_elem(b, dst, kv, v);
    let k1 = b.ins().iadd_imm_s(kv, 1);
    b.def_var(k, k1);
    b.ins().jump(head, &[]);
    b.switch_to_block(out);
}

/// A merge sort's comparator calls (see `Codegen::define_sorter`).
struct Sorter<'a> {
    cmp: FuncId,
    f: &'a ir::Func,
    unwinding: mir::DataId,
    to_float: FuncId,
    ctx: Value,
}

impl Sorter<'_> {
    /// Whether `x` orders after `y` (an `I8`), else (the comparator threw)
    /// to `fail`.
    fn after(&self, b: &mut FunctionBuilder, x: Value, y: Value, fail: Block) -> Value {
        let mut args = vec![self.ctx];
        if self.f.is_closure {
            args.push(b.ins().iconst(I64, 0));
        }
        for (&t, w) in self.f.params.iter().zip([x, y]) {
            match t {
                // (Numbers: `JS_SortNative` checked that every value is one.)
                Ty::F64 => args.push(val::number_of(b, w)),
                _ => args.push(w),
            }
        }
        let call = b.ins().call(self.cmp, &args);
        let r = b.inst_results(call).to_vec();
        let flag_addr = b.ins().symbol_value(I64, self.unwinding);
        let flag = b.ins().load(I8, M_FLAG, flag_addr, 0);
        let ok = b.create_block();
        b.ins().brif(flag, fail, &[], ok, &[]);
        b.switch_to_block(ok);
        let zero = b.ins().f64const(0.0);
        if self.f.ret == Ty::F64 {
            return b.ins().fcmp(FloatCC::GreaterThan, r[0], zero);
        }
        // A JS value: an int32, a float, else converted (which may throw).
        let w = r[0];
        let join = b.create_block();
        let res = b.append_block_param(join, I8);
        let not_int = b.create_block();
        let is_int = val::is(b, w, jsvalue::TAG_INT);
        let int_b = b.create_block();
        b.ins().brif(is_int, int_b, &[], not_int, &[]);
        b.switch_to_block(int_b);
        let i = b.ins().ireduce(I32, w);
        let pos = b.ins().icmp_imm_s(IntCC::SignedGreaterThan, i, 0);
        b.ins().jump(join, &[BlockArg::Value(pos)]);
        b.switch_to_block(not_int);
        let is_float = val::is_float(b, w);
        let float_b = b.create_block();
        let other = b.create_block();
        b.set_cold_block(other);
        b.ins().brif(is_float, float_b, &[], other, &[]);
        b.switch_to_block(float_b);
        let d = val::f64(b, w);
        let pos = b.ins().fcmp(FloatCC::GreaterThan, d, zero);
        b.ins().jump(join, &[BlockArg::Value(pos)]);
        b.switch_to_block(other);
        let slot = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let out = b.ins().stack_addr(I64, slot, 0);
        let rc = b.ins().call(self.to_float, &[self.ctx, out, w]);
        let rc = b.inst_results(rc)[0];
        let threw = b.ins().icmp_imm_s(IntCC::SignedLessThan, rc, 0);
        let conv = b.create_block();
        b.ins().brif(threw, fail, &[], conv, &[]);
        b.switch_to_block(conv);
        let d = b.ins().stack_load(I64, F64, slot, 0);
        let pos = b.ins().fcmp(FloatCC::GreaterThan, d, zero);
        b.ins().jump(join, &[BlockArg::Value(pos)]);
        b.switch_to_block(join);
        res
    }
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
        ("JS_DupValue", &[P, W], &[W]),
        ("JS_FreeValue", &[P, W], &[]),
        ("JS_GCWriteBarrier", &[P, P], &[]),
        ("JS_BinaryOperator", &[P, Int, W, W], &[W]),
        ("JS_AddValues", &[P, W, W], &[W]),
        ("JS_UnaryOperator", &[P, Int, W], &[W]),
        ("JS_ToBool", &[P, W], &[Int]),
        ("JS_ToFloat64", &[P, P, W], &[Int]),
        ("JS_ToString", &[P, W], &[W]),
        ("JS_GetProperty", &[P, W, Int], &[W]),
        ("JS_JSONStringifyData", &[P, W], &[W]),
        ("lum_struct_stringify", &[P, W], &[W]),
        ("JS_SetProperty", &[P, W, Int, W], &[Int]),
        ("JS_DefinePropertyValue", &[P, W, Int, W, Int], &[Int]),
        ("JS_GetPropertyValue", &[P, W, W], &[W]),
        ("JS_DeleteProperty", &[P, W, Int, Int], &[Int]),
        ("JS_CallConstructor", &[P, W, Int, P], &[W]),
        ("JS_GetGlobalVariable", &[P, Int], &[W]),
        ("JS_SetGlobalVariable", &[P, Int, W], &[Int]),
        ("JS_NewObject", &[P], &[W]),
        ("JS_GetException", &[P], &[W]),
        ("JS_Throw", &[P, W], &[W]),
        ("lum_set_elem", &[P, P], &[Int]),
        ("lum_ic_get", &[P, W, Int, P], &[W]),
        ("lum_plain_add_ok", &[P, P, Int], &[Int]),
        ("lum_null_object", &[P], &[W]),
        ("JS_GetIteratorObject", &[P, W, Int], &[W]),
        ("JS_ForInStart", &[P, W, P], &[W]),
        ("JS_ForInHas", &[P, W, W, Int], &[Int]),
        ("JS_IteratorStep", &[P, W, W, Int, P, P], &[W]),
        ("JS_ForInStep", &[P, W, P], &[W]),
        ("JS_IteratorCloseCompletion", &[P, W, Int], &[Int]),
        ("JS_ToObject", &[P, W], &[W]),
        ("JS_ToPropertyKey", &[P, W], &[W]),
        ("JS_NewArray", &[P], &[W]),
        ("lum_array_push", &[P, W, W], &[Int]),
        ("lum_array_hole", &[P, W], &[Int]),
        ("lum_array_spread", &[P, W, W], &[Int]),
        ("lum_apply", &[P, P], &[W]),
        ("lum_construct", &[P, W, W], &[W]),
        ("lum_rest", &[P, Int, P, Int], &[W]),
        ("lum_arguments", &[P, Int, P, Int], &[W]),
        ("lum_super_construct", &[P, Int, P], &[W]),
        ("lum_super_construct_array", &[P, P], &[W]),
        ("JS_GetFunctionOfData", &[P], &[W]),
        ("lum_class_setup", &[P, P, Int], &[W]),
        ("lum_define_method", &[P, P, Int, Int], &[Int]),
        ("lum_define_method_computed", &[P, P, Int], &[Int]),
        ("lum_new_from_target", &[P, W], &[W]),
        ("lum_object_rest", &[P, P, Int], &[W]),
        ("lum_define_computed", &[P, P, W], &[Int]),
        ("lum_object_spread", &[P, W, W], &[Int]),
        ("lum_array_iter_close", &[P, W, Int, Int], &[Int]),
        ("lum_array_iter_ok", &[P], &[Int]),
        ("lum_regexp", &[P, Int, Int], &[W]),
        ("lum_gen_iter_ok", &[P, W, W], &[Int]),
        ("JS_NativeGeneratorNext", &[P, W, P], &[W]),
        ("JS_ArrayAppendFast", &[P, W, W], &[Int]),
        ("lum_for_in_get", &[P, P], &[W]),
        ("JS_PrivateGet", &[P, W, W], &[W]),
        ("lum_private_set", &[P, P, W], &[Int]),
        ("JS_PrivateCheckBrand", &[P, W, W], &[Int]),
        ("JS_PrivateIn", &[P, W, W], &[Int]),
        ("lum_private_define", &[P, P, W], &[Int]),
        ("JS_PrivateAddBrand", &[P, W, W], &[Int]),
        ("lum_super_get", &[P, P], &[W]),
        ("lum_super_set", &[P, P, W], &[Int]),
        ("lum_gen_start", &[P, Int, Int, P], &[W]),
        ("lum_gen_new", &[P, W, P], &[W]),
        ("lum_async_gen_new", &[P, W, P], &[W]),
        ("lum_async_yield_star", &[P, P, Int, P], &[W]),
        ("lum_async_yield_star_result", &[P, W, P], &[W]),
        ("lum_throw_iterator_throw", &[P], &[Int]),
        ("lum_for_await_result", &[P, W, P], &[W]),
        ("lum_check_object", &[P, W], &[Int]),
        ("lum_is_function", &[P, W], &[Int]),
        ("lum_iter_close_quiet", &[P, W], &[]),
        ("lum_yield_star", &[P, P, Int, P], &[W]),
        ("lum_ic_set", &[P, W, Int, W, P], &[Int]),
        ("lum_private_get_ic", &[P, W, W, P], &[W]),
        ("lum_ic_get_proto", &[P, W, Int, P], &[W]),
        ("lum_ic_poly_addr", &[P, P], &[P]),
        ("lum_ic_set_proto", &[P, W, Int, W, P], &[Int]),
        ("lum_call_getter", &[P, P, W], &[W]),
        ("lum_define_accessor", &[P, P, Int, W, Int], &[Int]),
        ("lum_new_object", &[P, P, Int, P, P], &[W]),
        ("JS_NewObjectWithCachedShape", &[P, P, P], &[W]),
        ("lum_global_get", &[P, Int, P], &[W]),
        ("lum_struct_new", &[P, Int, Int], &[W]),
        ("lum_struct_put", &[P, W, Int, W], &[Int]),
        ("lum_struct_validate", &[P, W], &[Int]),
        ("lum_struct_set", &[P, W, Int, W], &[Int]),
        ("lum_struct_coerce", &[P, Int, Int, W], &[W]),
        ("lum_struct_convert", &[P, W, Int], &[W]),
        ("lum_json_parse_as", &[P, W, Int], &[W]),
        ("lum_json_method_as", &[P, W, Int], &[W]),
        ("lum_keys_length", &[P, W, W, W], &[W]),
        ("lum_object_create", &[P, P, W, W, W], &[W]),
        ("JS_NewObjectWithCachedShapeRoom", &[P, P, P, Int], &[W]),
        ("lum_sarr_from", &[P, Int, W], &[W]),
        ("lum_sarr_push", &[P, W, W], &[W]),
        ("lum_super_get_ic", &[P, W, W, Int, P], &[W]),
        ("lum_call_setter", &[P, P, W, W], &[Int]),
        ("lum_private_set_ic", &[P, P, W, P], &[Int]),
        ("lum_delete_elem", &[P, P], &[Int]),
        ("lum_call", &[P, P, Int, P], &[W]),
        ("JS_CallBuiltin", &[P, W, W, Int, P], &[W]),
        ("JS_IsStrictEqual", &[P, W, W], &[Int]),
        ("JS_ArrayPushFast", &[P, W, W], &[W]),
        ("JS_ArraySpeciesCreateRoom", &[P, W, P, P], &[W]),
        ("JS_TryGetElement", &[P, W, P, P], &[W]),
        ("JS_DefineElement", &[P, W, P, W], &[Int]),
        ("JS_SortNative", &[P, W, P, Int], &[Int]),
        ("lum_ic_own_data", &[P, W, Int, P], &[W]),
        ("lum_instanceof", &[P, W, W, P], &[Int]),
        ("lum_ic_get_cached", &[P, W, Int, P], &[W]),
        ("lum_record_learn", &[P, W, Int], &[]),
        ("lum_char_code_at_ok", &[P], &[Int]),
        ("lum_ic_refresh", &[P, W], &[]),
        ("lum_array_push_ok", &[P], &[Int]),
        ("lum_region_push", &[P, W, W], &[Int]),
        ("JS_CallStringFn", &[P, W, Int, Int, P], &[W]),
        ("JS_CallMapFn", &[P, W, Int, Int, P], &[W]),
        ("JS_ConcatStrings", &[P, P, Int], &[W]),
        ("JS_AddValuesN", &[P, P, Int], &[W]),
        ("lum_new_instance", &[P, P, W, Int, P, P], &[W]),
        ("lum_new_instance_full", &[P, P, W, Int, P, P, P, Int], &[W]),
        ("lum_global_or_undefined", &[P, Int], &[W]),
        ("lum_define_field", &[P, P, Int, W], &[Int]),
        ("lum_new_array", &[P, Int, P], &[W]),
        ("lum_throw_uninitialized", &[P, Int], &[]),
        ("lum_stack_overflow", &[P], &[]),
        ("lum_throw_uninitialized_atom", &[P, Int], &[]),
        ("lum_box_new", &[P, P], &[W]),
        ("lum_make_closure", &[P, Int, Int, P], &[W]),
        ("lum_async_start", &[P, Int, Int, P, P], &[W]),
        ("lum_async_step", &[P, P, Int, P, Int], &[]),
        ("lum_await", &[P, P, W, Int], &[W]),
        ("lum_async_settled", &[P, W], &[W]),
        ("lum_is_promise_resolve", &[W, W], &[Int]),
        ("lum_pow", &[D, D], &[D]),
        ("lum_toint32_slow", &[D], &[Int]),
        ("fmod", &[D, D], &[D]),
    ]
};

#[derive(Clone, Copy)]
enum Arg {
    /// Pointer.
    P,
    /// 64-bit word: a `JSValue`.
    W,
    /// C `int` / `JSAtom`.
    Int,
    /// `double`.
    D,
}

struct Codegen {
    module: mir::Module,
    funcs: Vec<FuncId>,
    /// Native `sort` comparators (by function): a copy whose JS-valued
    /// parameters are borrowed (`lum_cmp_<i>`).
    sort_cmps: HashMap<usize, FuncId>,
    /// Their merge sorts (`lum_sort_<i>`: see `define_sorter`).
    sorters: HashMap<usize, FuncId>,
    fast_funcs: Vec<FuncId>,
    helpers: HashMap<&'static str, FuncId>,
    data: [DataId; 14],
    fast_sig: SigRef,
    /// Inline caches allocated so far (`lum_ics` entries).
    ics: std::rc::Rc<std::cell::Cell<usize>>,
    closure_base: usize,
    /// Parameter types, by `FuncId`.
    params: std::rc::Rc<Vec<Vec<Ty>>>,
    /// `value_captures`, by `ClosureId`.
    value_captures: std::rc::Rc<Vec<Vec<bool>>>,
    /// The atoms `Promise` and `resolve`, if used.
    promise_resolve: Option<(ir::AtomId, ir::AtomId)>,
    /// The atom `parse`, if used.
    json_parse: Option<ir::AtomId>,
    methods: std::rc::Rc<HashMap<ir::AtomId, lower::Method>>,
    nothrow: std::rc::Rc<Vec<bool>>,
    /// Return types, by `FuncId`.
    rets: std::rc::Rc<Vec<Ty>>,
    /// `aot type` layouts.
    layouts: std::rc::Rc<Vec<ir::Layout>>,
    int_wrapping: bool,
    /// The string constants' text.
    texts: std::rc::Rc<Vec<String>>,
    /// The string constants `lower::TYPEOF_NAMES`, if all are used.
    typeof_names: Option<[ir::StrId; 8]>,
}

impl Codegen {
    fn new(m: &ir::Module) -> Self {
        let mut module = mir::Module::new();
        let mut funcs = Vec::new();
        for (i, f) in m.funcs.iter().chain(&m.closures).enumerate() {
            // The step functions of async functions and generators are
            // referenced by the C glue; the others only by native code.
            let linkage = if f.suspends() { Linkage::Export } else { Linkage::Local };
            funcs.push(module.declare_function(&symbol_name(i), linkage, &native_signature(f)));
        }
        let mut sort_cmps = HashMap::new();
        let mut sorters = HashMap::new();
        for f in m.funcs.iter().chain(&m.closures) {
            ir::visit_stmts(&f.body, &mut |e| {
                if let ir::ExprKind::ArraySort { func, .. } = e.kind
                    && !sort_cmps.contains_key(&func)
                    && let Some(g) = m.funcs.iter().chain(&m.closures).nth(func)
                    && !g.is_async
                    && !g.is_generator
                {
                    let id = module.declare_function(&format!("lum_cmp_{func}"), Linkage::Local, &native_signature(g));
                    sort_cmps.insert(func, id);
                    // (Its merge sort's calls run it inline.)
                    module.set_always_inline(id);
                    let id = module.declare_function(&format!("lum_sort_{func}"), Linkage::Local, &sorter_signature());
                    sorters.insert(func, id);
                }
            });
        }
        let fast_sig = fast_signature();
        let mut fast_funcs = Vec::new();
        for i in 0..m.funcs.len() + m.closures.len() {
            fast_funcs.push(module.declare_function(&fast_symbol_name(i), Linkage::Export, &fast_sig));
        }
        let fast_sig = module.import_signature(fast_sig);
        let mut helpers = HashMap::new();
        let ty = |a: &Arg| match a {
            Arg::P => I64,
            Arg::W => I64,
            Arg::Int => I32,
            Arg::D => F64,
        };
        for &(name, params, rets) in HELPERS {
            let param = |a: &Arg| if matches!(a, Arg::P) { AbiParam::pointer() } else { AbiParam::new(ty(a)) };
            let sig =
                Signature { params: params.iter().map(param).collect(), returns: rets.iter().map(param).collect() };
            helpers.insert(name, module.declare_function(name, Linkage::Import, &sig));
        }
        // libm functions used by Math intrinsics.
        for mf in all_math(m) {
            if let Some(sym) = mf.symbol()
                && !helpers.contains_key(sym)
            {
                let sig = Signature { params: vec![AbiParam::new(F64); mf.arity()], returns: vec![AbiParam::new(F64)] };
                helpers.insert(sym, module.declare_function(sym, Linkage::Import, &sig));
            }
        }
        let mut data = Vec::new();
        for name in [
            "lum_stack_limit",
            "lum_unwinding",
            "lum_cells",
            "lum_strings",
            "lum_atoms",
            "lum_shapes",
            "lum_privates",
            "lum_homes",
            "lum_struct_class",
            "lum_sarr_class",
            "lum_fn_caches",
        ] {
            data.push(module.declare_data(name, Linkage::Import, true));
        }
        // Defined here, once all sites are known (`define_ics`).
        assert_eq!(data.len(), DATA_ICS);
        data.push(module.declare_data("lum_ics", Linkage::Export, true));
        // Record types: their shapes once learned, and their fields (per
        // record: the field count, then the fields' atom indices).
        data.push(module.declare_data("lum_record_shapes", Linkage::Export, true));
        let nrec = m.records.len();
        let shapes = data[data.len() - 1];
        module.define_data(shapes, mir::DataInit::Zeroed { size: nrec.max(1) * lower::RECORD_STRIDE }, 8);
        let mut fields: Vec<i32> = Vec::new();
        let mut index: Vec<i32> = Vec::new();
        for r in &m.records {
            index.push(fields.len() as i32);
            fields.push(r.fields.len() as i32);
            fields.extend(r.fields.iter().map(|&a| a as i32));
        }
        let ints = |v: &[i32]| {
            let mut b: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            b.resize(b.len().max(4), 0);
            b
        };
        let d = module.declare_data("lum_record_fields", Linkage::Export, false);
        module.define_data(d, mir::DataInit::Bytes(ints(&fields)), 4);
        let d = module.declare_data("lum_record_index", Linkage::Export, false);
        module.define_data(d, mir::DataInit::Bytes(ints(&index)), 4);
        let d = module.declare_data("lum_nrecords", Linkage::Export, false);
        module.define_data(d, mir::DataInit::Bytes((nrec as i32).to_le_bytes().to_vec()), 4);
        // The engine's counter of changes to watched prototypes (see `LumIC`).
        data.push(module.declare_data("js_proto_epoch", Linkage::Import, true));
        Codegen {
            params: std::rc::Rc::new(m.funcs.iter().chain(&m.closures).map(|f| f.params.clone()).collect()),
            value_captures: std::rc::Rc::new(m.closures.iter().map(|f| f.value_captures.clone()).collect()),
            rets: std::rc::Rc::new(m.funcs.iter().chain(&m.closures).map(|f| f.ret).collect()),
            layouts: std::rc::Rc::new(m.layouts.clone()),
            int_wrapping: m.int_wrapping,
            texts: std::rc::Rc::new(m.strings.clone()),
            typeof_names: lower::TYPEOF_NAMES
                .iter()
                .map(|n| m.strings.iter().position(|s| s == n))
                .collect::<Option<Vec<_>>>()
                .map(|v| v.try_into().unwrap()),
            promise_resolve: m
                .atoms
                .iter()
                .position(|a| a == "Promise")
                .zip(m.atoms.iter().position(|a| a == "resolve")),
            json_parse: m.atoms.iter().position(|a| a == "parse"),
            nothrow: std::rc::Rc::new(m.funcs.iter().map(ir::nothrow_leaf).collect()),
            methods: std::rc::Rc::new(
                m.atoms.iter().enumerate().filter_map(|(i, a)| lower::Method::named(a).map(|k| (i, k))).collect(),
            ),
            module,
            funcs,
            sort_cmps,
            sorters,
            fast_funcs,
            helpers,
            data: data.try_into().unwrap(),
            fast_sig,
            ics: Default::default(),
            closure_base: m.funcs.len(),
        }
    }

    fn env(&self) -> Env {
        let [
            stack_limit,
            unwinding,
            cells,
            strings,
            atoms,
            shapes,
            privates,
            homes,
            struct_class,
            sarr_class,
            fn_caches,
            ics,
            record_shapes,
            proto_epoch,
        ] = self.data;
        Env {
            helpers: self.helpers.clone(),
            funcs: self.funcs.clone(),
            sorters: self.sorters.clone(),
            stack_limit,
            unwinding,
            cells,
            strings,
            atoms,
            shapes,
            privates,
            homes,
            struct_class,
            sarr_class,
            fn_caches,
            ics,
            record_shapes,
            proto_epoch,
            next_ic: self.ics.clone(),
            closure_base: self.closure_base,
            params: self.params.clone(),
            value_captures: self.value_captures.clone(),
            promise_resolve: self.promise_resolve,
            json_parse: self.json_parse,
            methods: self.methods.clone(),
            nothrow: self.nothrow.clone(),
            fast: self.fast_funcs.clone(),
            rets: self.rets.clone(),
            layouts: self.layouts.clone(),
            int_wrapping: self.int_wrapping,
            texts: self.texts.clone(),
            typeof_names: self.typeof_names,
            fast_sig: self.fast_sig,
        }
    }

    /// Defines the inline cache table (`LumIC lum_ics[]`, `IC_SIZE` bytes per
    /// entry, zeroed) and its length `lum_nics`.
    /// Defines `lum_sort_<i>` (`id`, `sorter_signature`): the engine's
    /// stable merge sort of `JS_SortNative` (insertion sorts of up to 8
    /// values, halves merged through `tmp`) with comparator `cmp` (`f`'s
    /// copy with borrowed parameters) called directly, for the backend to
    /// inline. A value orders after another where the comparator's result
    /// for the two is above 0 (NaN is not); where it throws, the sort
    /// stops with every value still in the array, and returns 1.
    fn define_sorter(&mut self, id: FuncId, cmp: FuncId, f: &ir::Func) {
        let env = self.env();
        let mut b = self.module.builder(sorter_signature());
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let (ctx, a, tmp, n) = (raw[0], raw[1], raw[2], raw[3]);
        let sorter = Sorter { cmp, f, unwinding: env.unwinding, to_float: env.helpers["JS_ToFloat64"], ctx };
        let fail = b.create_block();
        b.set_cold_block(fail);
        let small = b.ins().icmp_imm_s(IntCC::SignedLessThanOrEqual, n, 8);
        let insertion = b.create_block();
        let merge = b.create_block();
        b.ins().brif(small, insertion, &[], merge, &[]);

        // Insertion sort: each value moved down past those after it.
        b.switch_to_block(insertion);
        let i = b.declare_var(I64);
        let j = b.declare_var(I64);
        let one = b.ins().iconst(I64, 1);
        b.def_var(i, one);
        let head = b.create_block();
        let body = b.create_block();
        let inner = b.create_block();
        let compare = b.create_block();
        let shift = b.create_block();
        let place = b.create_block();
        let done = b.create_block();
        let place_fail = b.create_block();
        b.set_cold_block(place_fail);
        b.ins().jump(head, &[]);
        b.switch_to_block(head);
        let iv = b.use_var(i);
        let more = b.ins().icmp(IntCC::SignedLessThan, iv, n);
        b.ins().brif(more, body, &[], done, &[]);
        b.switch_to_block(body);
        let iv = b.use_var(i);
        let x = elem(&mut b, a, iv);
        b.def_var(j, iv);
        b.ins().jump(inner, &[]);
        b.switch_to_block(inner);
        let jv = b.use_var(j);
        let positive = b.ins().icmp_imm_s(IntCC::SignedGreaterThan, jv, 0);
        b.ins().brif(positive, compare, &[], place, &[]);
        b.switch_to_block(compare);
        let jv = b.use_var(j);
        let j1 = b.ins().iadd_imm_s(jv, -1);
        let y = elem(&mut b, a, j1);
        let after = sorter.after(&mut b, y, x, place_fail);
        b.ins().brif(after, shift, &[], place, &[]);
        b.switch_to_block(shift);
        let jv = b.use_var(j);
        let j1 = b.ins().iadd_imm_s(jv, -1);
        let y = elem(&mut b, a, j1);
        set_elem(&mut b, a, jv, y);
        b.def_var(j, j1);
        b.ins().jump(inner, &[]);
        b.switch_to_block(place);
        let jv = b.use_var(j);
        set_elem(&mut b, a, jv, x);
        let iv = b.use_var(i);
        let i1 = b.ins().iadd_imm_s(iv, 1);
        b.def_var(i, i1);
        b.ins().jump(head, &[]);
        b.switch_to_block(place_fail);
        let jv = b.use_var(j);
        set_elem(&mut b, a, jv, x);
        b.ins().jump(fail, &[]);
        b.switch_to_block(done);
        let zero = b.ins().iconst(I32, 0);
        b.ins().return_(&[zero]);

        // Merge sort: both halves sorted, then merged (unless already in
        // order), the first through `tmp`.
        b.switch_to_block(merge);
        let m = b.ins().ushr_imm_u(n, 1);
        let r = b.ins().call(id, &[ctx, a, tmp, m]);
        let r = b.inst_results(r)[0];
        let next = b.create_block();
        b.ins().brif(r, fail, &[], next, &[]);
        b.switch_to_block(next);
        let off = b.ins().imul_imm_s(m, jsvalue::SIZE as i64);
        let a2 = b.ins().iadd(a, off);
        let n2 = b.ins().isub(n, m);
        let r = b.ins().call(id, &[ctx, a2, tmp, n2]);
        let r = b.inst_results(r)[0];
        let next = b.create_block();
        b.ins().brif(r, fail, &[], next, &[]);
        b.switch_to_block(next);
        let m1 = b.ins().iadd_imm_s(m, -1);
        let last = elem(&mut b, a, m1);
        let first = elem(&mut b, a, m);
        let after = sorter.after(&mut b, last, first, fail);
        let merging = b.create_block();
        b.ins().brif(after, merging, &[], done, &[]);
        b.switch_to_block(merging);
        copy_elems(&mut b, tmp, a, m);
        let (i, j, k) = (b.declare_var(I64), b.declare_var(I64), b.declare_var(I64));
        let zero = b.ins().iconst(I64, 0);
        b.def_var(i, zero);
        b.def_var(j, m);
        b.def_var(k, zero);
        let head = b.create_block();
        let both = b.create_block();
        let compare = b.create_block();
        let take_b = b.create_block();
        let take_a = b.create_block();
        let rest = b.create_block();
        let rest_fail = b.create_block();
        b.set_cold_block(rest_fail);
        b.ins().jump(head, &[]);
        b.switch_to_block(head);
        let iv = b.use_var(i);
        let more = b.ins().icmp(IntCC::SignedLessThan, iv, m);
        b.ins().brif(more, both, &[], rest, &[]);
        b.switch_to_block(both);
        let jv = b.use_var(j);
        let more = b.ins().icmp(IntCC::SignedLessThan, jv, n);
        b.ins().brif(more, compare, &[], rest, &[]);
        b.switch_to_block(compare);
        let iv = b.use_var(i);
        let jv = b.use_var(j);
        let x = elem(&mut b, tmp, iv);
        let y = elem(&mut b, a, jv);
        let after = sorter.after(&mut b, x, y, rest_fail);
        b.ins().brif(after, take_b, &[], take_a, &[]);
        for (blk, from_b) in [(take_b, true), (take_a, false)] {
            b.switch_to_block(blk);
            let kv = b.use_var(k);
            let (src, var) = if from_b { (a, j) } else { (tmp, i) };
            let at = b.use_var(var);
            let v = elem(&mut b, src, at);
            set_elem(&mut b, a, kv, v);
            let k1 = b.ins().iadd_imm_s(kv, 1);
            b.def_var(k, k1);
            let at1 = b.ins().iadd_imm_s(at, 1);
            b.def_var(var, at1);
            b.ins().jump(head, &[]);
        }
        // What is left of the first half (after the comparator threw too:
        // every value back in the array).
        for (blk, threw) in [(rest, false), (rest_fail, true)] {
            b.switch_to_block(blk);
            let iv = b.use_var(i);
            let kv = b.use_var(k);
            let left = b.ins().isub(m, iv);
            let src = b.ins().imul_imm_s(iv, jsvalue::SIZE as i64);
            let src = b.ins().iadd(tmp, src);
            let dst = b.ins().imul_imm_s(kv, jsvalue::SIZE as i64);
            let dst = b.ins().iadd(a, dst);
            copy_elems(&mut b, dst, src, left);
            if threw {
                b.ins().jump(fail, &[]);
            } else {
                b.ins().jump(done, &[]);
            }
        }

        b.switch_to_block(fail);
        let one = b.ins().iconst(I32, 1);
        b.ins().return_(&[one]);
        self.module.define_function(id, b.finish());
    }

    fn define_ics(&mut self) {
        let n = self.ics.get();
        self.module.define_data(self.data[DATA_ICS], mir::DataInit::Zeroed { size: n.max(1) * lower::IC_SIZE }, 8);
        let count = self.module.declare_data("lum_nics", Linkage::Export, false);
        self.module.define_data(count, mir::DataInit::Bytes((n as i32).to_le_bytes().to_vec()), 4);
    }

    /// Defines `lum_fn_<index>`, the function itself, for native callers.
    fn define(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        if f.suspends() {
            return self.define_step(index, f);
        }
        self.define_as(self.funcs[index], index, f, false)
    }

    /// Defines native function `index` as `id`; with `borrowed`, its
    /// JS-valued parameters that it never assigns are borrowed (a native
    /// `sort`'s comparator: the engine holds the elements).
    fn define_as(&mut self, id: FuncId, index: usize, f: &ir::Func, borrowed: bool) -> Result<()> {
        let ptr = I64;
        let env = self.env();
        let mut b = self.module.builder(native_signature(f));
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let js_ctx = raw[0];
        let mut params = Vec::new();
        let data = f.is_closure.then(|| raw[1]);
        let mut k = if f.is_closure { 2 } else { 1 };
        let this = f.has_this.then(|| raw[k]);
        if f.has_this {
            k += 1;
        }
        for &t in &f.params {
            let n = clif_types(t).len();
            params.push(from_values(t, &raw[k..k + n]));
            k += n;
        }
        let exits = Exits { ret: Exit::Return, unwind: Exit::Return };
        let leaf = self.nothrow.get(index).copied().unwrap_or(false);
        let mut l = Lower::new(b, env, js_ctx, ptr, f, &params, data, exits);
        l.this = this;
        if borrowed {
            l.borrow_params(f);
        }
        // (A leaf adds one frame: the caller's check covers it.)
        if !leaf {
            l.stack_check();
        }
        let b = l.body(&f.body);
        self.module.define_function(id, b.finish());
        Ok(())
    }

    /// Defines the step function of async function `index` (as
    /// `lum_fn_<index>`): runs the body from the start (state 0) or from
    /// the await it suspended at, until it completes or suspends again.
    fn define_step(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        let ptr = I64;
        let env = self.env();
        let mut b = self.module.builder(native_signature(f));
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let (js_ctx, slots, state, value, is_throw) = (raw[0], raw[1], raw[2], raw[3], raw[4]);
        let exits = Exits { ret: Exit::Return, unwind: Exit::Return };
        let mut l = Lower::new(b, env, js_ctx, ptr, f, &[], None, exits);
        l.stack_check();
        l.enter_async(slots, state, value, is_throw, f);
        let b = l.body(&f.body);
        self.module.define_function(self.funcs[index], b.finish());
        Ok(())
    }

    /// The JS entry of async function `index`: type guards as for other
    /// functions, then a new frame holding the arguments (and captured boxes,
    /// and `this`), the first step, and the promise as the result.
    fn define_async_entry(&mut self, index: usize, f: &ir::Func) -> Result<()> {
        let ptr = I64;
        let env = self.env();
        let mut b = self.module.builder(fast_signature());
        let mem = MemFlags::trusted();
        let entry = b.create_block();
        let decline = b.create_block();
        b.set_cold_block(decline);
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let raw = b.block_params(entry).to_vec();
        let (js_ctx, this_val, argc, argv, data) = (raw[0], raw[1], raw[2], raw[3], raw[4]);

        let required = f.params.iter().rposition(|&t| t != Ty::Any).map_or(0, |i| i + 1);
        if required > 0 {
            let enough = b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, argc, required as i64);
            let ok = b.create_block();
            b.ins().brif(enough, ok, &[], decline, &[]);
            b.switch_to_block(ok);
        }
        let mut params = vec![V::Void; f.params.len()];
        let struct_class = b.ins().symbol_value(ptr, env.struct_class);
        let sarr_class = b.ins().symbol_value(ptr, env.sarr_class);
        for (k, &t) in f.params.iter().enumerate() {
            if t != Ty::Any {
                params[k] = unbox_arg(&mut b, t, argv, k, decline, struct_class, sarr_class);
            }
        }

        // The frame and the promise.
        let (layout, this_slot, nslots) = lower::frame_layout(f);
        let out = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 32, 4));
        let promise_addr = b.ins().stack_addr(ptr, out, 0);
        let slots_addr = b.ins().stack_addr(ptr, out, 16);
        let fi = b.ins().iconst(I32, index as i64);
        let ns = b.ins().iconst(I32, nslots as i64);
        let start = if f.is_generator {
            b.ins().call(env.helpers["lum_gen_start"], &[js_ctx, fi, ns, slots_addr])
        } else {
            b.ins().call(env.helpers["lum_async_start"], &[js_ctx, fi, ns, promise_addr, slots_addr])
        };
        let frame = b.inst_results(start)[0];
        let failed = b.ins().icmp_imm_s(IntCC::Equal, frame, jsvalue::val_const(jsvalue::TAG_EXCEPTION, 0));
        let exception = b.create_block();
        let ok = b.create_block();
        b.set_cold_block(exception);
        b.ins().brif(failed, exception, &[], ok, &[]);
        b.switch_to_block(ok);
        let slots = b.ins().load(ptr, mem, slots_addr, 0);
        let store = |b: &mut FunctionBuilder, s: usize, w: Value| {
            b.ins().store(M_SLOT, w, slots, (s * jsvalue::SIZE) as i32);
        };
        // The rest array and the arguments object; if they cannot be
        // allocated, drop the frame (the collector frees the rest array).
        let made: Vec<(usize, &str)> = [(f.rest_param(), "lum_rest"), (f.arguments_param(), "lum_arguments")]
            .into_iter()
            .filter_map(|(k, h)| Some((k?, h)))
            .collect();
        for &(k, helper) in &made {
            let failed = b.create_block();
            b.set_cold_block(failed);
            params[k] = rest_arg(&mut b, env.helpers[helper], js_ctx, argc, argv, k, failed);
            let cont = b.create_block();
            b.ins().jump(cont, &[]);
            b.switch_to_block(failed);
            b.ins().call(env.helpers["JS_FreeValue"], &[js_ctx, frame]);
            b.ins().jump(exception, &[]);
            b.switch_to_block(cont);
        }
        for (k, &t) in f.params.iter().enumerate() {
            let s = layout[k];
            let local_ty = f.locals[k];
            // (As `spill_locals` stores them: JS values, numbers as floats.)
            let w = match (t, params[k]) {
                (_, V::Any(w)) if made.iter().any(|&(j, _)| j == k) => w,
                (Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_), _) => {
                    load_arg(&mut b, argc, argv, k)
                }
                (_, V::Num(d)) if local_ty == Ty::Any => box_number(&mut b, d),
                (_, V::Num(d)) => val::float(&mut b, d),
                (_, V::Bool(x)) => val::boolean(&mut b, x),
                (_, V::Int(i)) if local_ty == Ty::Any => val::mk_int(&mut b, i),
                (_, V::Int(i)) => {
                    let d = b.ins().fcvt_from_sint(F64, i);
                    val::float(&mut b, d)
                }
                _ => unreachable!(),
            };
            store(&mut b, s, w);
        }
        if f.is_closure {
            // Captured boxes: the box object, and its value's address.
            for (c, &l) in f.captures.iter().enumerate() {
                let at = (2 * c * jsvalue::SIZE) as i32;
                let w = b.ins().load(I64, M_SLOT, data, at);
                let sp = b.ins().load(I64, M_SLOT, data, at + jsvalue::SIZE as i32);
                store(&mut b, layout[l], w);
                store(&mut b, layout[l] + 1, sp);
            }
        }
        if let Some(s) = this_slot {
            store(&mut b, s, this_val);
        }
        if f.is_generator {
            // The generator object, which takes the frame (after binding
            // the parameters).
            let new = if f.is_async { "lum_async_gen_new" } else { "lum_gen_new" };
            let call = b.ins().call(env.helpers[new], &[js_ctx, frame, data]);
            let r = b.inst_results(call).to_vec();
            b.ins().return_(&r);
        } else {
            // First step, then drop this entry's reference to the frame.
            let undef_slot = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
            let undef = b.ins().iconst(I64, jsvalue::val_const(jsvalue::TAG_UNDEFINED, 0));
            b.ins().stack_store(ptr, undef, undef_slot, 0);
            let vaddr = b.ins().stack_addr(ptr, undef_slot, 0);
            let st0 = b.ins().iconst(I32, 0);
            let no = b.ins().iconst(I32, 0);
            b.ins().call(env.helpers["lum_async_step"], &[js_ctx, slots, st0, vaddr, no]);
            b.ins().call(env.helpers["JS_FreeValue"], &[js_ctx, frame]);
            let pw = b.ins().load(I64, mem, promise_addr, 0);
            b.ins().return_(&[pw]);
        }

        b.switch_to_block(exception);
        let e = b.ins().iconst(I64, jsvalue::val_const(jsvalue::TAG_EXCEPTION, 0));
        b.ins().return_(&[e]);

        b.switch_to_block(decline);
        let d = b.ins().iconst(I64, jsvalue::val_const(jsvalue::TAG_UNINITIALIZED, 0));
        b.ins().return_(&[d]);

        self.module.define_function(self.fast_funcs[index], b.finish());
        Ok(())
    }

    /// Defines `lum_fast_<index>`, the JS-facing entry of native function
    /// `index`, with the engine's `JSCFunctionFast` signature:
    ///
    /// ```c
    /// JSValue lum_fast_i(JSContext *ctx, JSValueConst this_val, int argc,
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
        if f.suspends() {
            return self.define_async_entry(index, f);
        }
        let ptr = I64;
        let env = self.env();
        let unwinding = env.unwinding;
        let callee = env.funcs[index];
        let settled = env.helpers["lum_async_settled"];
        let mut b = self.module.builder(fast_signature());

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
        let (js_ctx, argc, argv) = (raw[0], raw[2], raw[3]);
        let data = f.is_closure.then(|| raw[4]);
        let this = f.has_this.then(|| raw[1]);

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
        let struct_class = b.ins().symbol_value(ptr, env.struct_class);
        let sarr_class = b.ins().symbol_value(ptr, env.sarr_class);
        for (k, &t) in f.params.iter().enumerate() {
            if t != Ty::Any {
                params[k] = unbox_arg(&mut b, t, argv, k, decline, struct_class, sarr_class);
            }
        }
        // A rest parameter: a new array (before any other new reference, so
        // that failing to allocate it leaks nothing); the arguments object
        // (the collector frees the rest array if it cannot be made).
        if let Some(k) = f.rest_param() {
            params[k] = rest_arg(&mut b, env.helpers["lum_rest"], js_ctx, argc, argv, k, exception);
        }
        if let Some(k) = f.arguments_param() {
            params[k] = rest_arg(&mut b, env.helpers["lum_arguments"], js_ctx, argc, argv, k, exception);
        }
        // Then the JS-valued parameters: new references, `undefined` if absent.
        for (k, &t) in f.params.iter().enumerate() {
            if matches!(t, Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_))
                && f.rest_param() != Some(k)
                && f.arguments_param() != Some(k)
            {
                params[k] = V::Any(load_arg(&mut b, argc, argv, k));
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
            args.extend(this);
            for p in &params {
                args.extend(p.values());
            }
            let call = b.ins().call(callee, &args);
            let r: Vec<BlockArg> = b.inst_results(call).iter().map(|&v| BlockArg::Value(v)).collect();
            let flag_addr = b.ins().symbol_value(ptr, unwinding);
            let flag = b.ins().load(I8, M_FLAG, flag_addr, 0);
            b.ins().brif(flag, exception, &[], boxing, &r);
            b
        };

        b.switch_to_block(boxing);
        let rv = b.block_params(boxing).to_vec();
        let w = match f.ret {
            Ty::Void => b.ins().iconst(I64, jsvalue::val_const(jsvalue::TAG_UNDEFINED, 0)),
            Ty::Bool => val::boolean(&mut b, rv[0]),
            Ty::F64 => box_number(&mut b, rv[0]),
            Ty::I32 => val::mk_int(&mut b, rv[0]),
            Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_) => rv[0],
            // The checker boxes these returns.
            Ty::MaybeF64 => unreachable!("MaybeF64 return type"),
        };
        // Returned in a register: the result never touches memory on its
        // way back into the interpreter. (An async function that never
        // awaits: the promise of its result.)
        if f.is_async {
            let call = b.ins().call(settled, &[js_ctx, w]);
            let r = b.inst_results(call).to_vec();
            b.ins().return_(&r);
        } else {
            b.ins().return_(&[w]);
        }

        b.switch_to_block(exception);
        let flag_addr = b.ins().symbol_value(ptr, unwinding);
        let zero8 = b.ins().iconst(I8, 0);
        b.ins().store(M_FLAG, zero8, flag_addr, 0);
        let e = b.ins().iconst(I64, jsvalue::val_const(jsvalue::TAG_EXCEPTION, 0));
        if f.is_async {
            // (Rejected with the exception.)
            let call = b.ins().call(settled, &[js_ctx, e]);
            let r = b.inst_results(call).to_vec();
            b.ins().return_(&r);
        } else {
            b.ins().return_(&[e]);
        }

        b.switch_to_block(decline);
        let d = b.ins().iconst(I64, jsvalue::val_const(jsvalue::TAG_UNINITIALIZED, 0));
        b.ins().return_(&[d]);

        self.module.define_function(self.fast_funcs[index], b.finish());
        Ok(())
    }
}

fn native_signature(f: &ir::Func) -> Signature {
    let mut sig = Signature::default();
    if f.suspends() {
        // The step function: (ctx, frame slots, state, value delivered on
        // resumption, whether to throw it) -> completion value, or
        // JS_UNINITIALIZED (suspended), or JS_EXCEPTION.
        sig.params.extend([
            AbiParam::pointer(),
            AbiParam::pointer(),
            AbiParam::new(I32),
            AbiParam::pointer(),
            AbiParam::new(I32),
        ]);
        sig.returns.push(AbiParam::new(I64));
        return sig;
    }
    sig.params.push(AbiParam::pointer());
    if f.is_closure {
        // The closure's data slots.
        sig.params.push(AbiParam::pointer());
    }
    if f.has_this {
        // The receiver (borrowed).
        sig.params.push(AbiParam::new(I64));
    }
    for &t in &f.params {
        sig.params.extend(abi(t));
    }
    sig.returns.extend(abi(f.ret));
    sig
}

fn fast_signature() -> Signature {
    let mut sig = Signature::default();
    // (ctx, this_val, argc, argv, func_data)
    sig.params.extend([
        AbiParam::pointer(),
        AbiParam::new(I64),
        AbiParam::new(I32),
        AbiParam::pointer(),
        AbiParam::pointer(),
    ]);
    sig.returns.push(AbiParam::new(I64));
    sig
}

/// Argument `k` from the interpreter's slots, borrowed; `undefined` if
/// `k >= argc`.
fn load_arg(b: &mut FunctionBuilder, argc: Value, argv: Value, k: usize) -> Value {
    let present = b.ins().icmp_imm_s(IntCC::SignedGreaterThan, argc, k as i64);
    let yes = b.create_block();
    let join = b.create_block();
    let w = b.append_block_param(join, I64);
    let undef = b.ins().iconst(I64, jsvalue::val_const(jsvalue::TAG_UNDEFINED, 0));
    b.ins().brif(present, yes, &[], join, &[BlockArg::Value(undef)]);
    b.switch_to_block(yes);
    let lw = b.ins().load(I64, M_SLOT, argv, (k * jsvalue::SIZE) as i32);
    b.ins().jump(join, &[BlockArg::Value(lw)]);
    b.switch_to_block(join);
    w
}

/// Reads an unboxed argument straight from the interpreter's `JSValue`
/// slot, branching to `decline` on a type mismatch.
#[allow(clippy::too_many_arguments)]
fn unbox_arg(
    b: &mut FunctionBuilder,
    t: Ty,
    argv: Value,
    k: usize,
    decline: Block,
    struct_class: Value,
    sarr_class: Value,
) -> V {
    let w = b.ins().load(I64, M_SLOT, argv, (k * jsvalue::SIZE) as i32);
    match t {
        Ty::F64 => {
            let is_int = val::is(b, w, jsvalue::TAG_INT);
            let int_blk = b.create_block();
            let not_int = b.create_block();
            let float_blk = b.create_block();
            let join = b.create_block();
            let v = b.append_block_param(join, F64);
            b.ins().brif(is_int, int_blk, &[], not_int, &[]);
            b.switch_to_block(int_blk);
            let i = b.ins().ireduce(I32, w);
            let d = b.ins().fcvt_from_sint(F64, i);
            b.ins().jump(join, &[BlockArg::Value(d)]);
            b.switch_to_block(not_int);
            let is_float = val::is_float(b, w);
            b.ins().brif(is_float, float_blk, &[], decline, &[]);
            b.switch_to_block(float_blk);
            let d = val::f64(b, w);
            b.ins().jump(join, &[BlockArg::Value(d)]);
            b.switch_to_block(join);
            V::Num(v)
        }
        // An int32: an int, or a double that is one exactly (not -0).
        Ty::I32 => {
            let is_int = val::is(b, w, jsvalue::TAG_INT);
            let int_blk = b.create_block();
            let not_int = b.create_block();
            let float_blk = b.create_block();
            let join = b.create_block();
            let v = b.append_block_param(join, I64);
            b.ins().brif(is_int, int_blk, &[], not_int, &[]);
            b.switch_to_block(int_blk);
            let i = val::int(b, w);
            b.ins().jump(join, &[BlockArg::Value(i)]);
            b.switch_to_block(not_int);
            let is_float = val::is_float(b, w);
            b.ins().brif(is_float, float_blk, &[], decline, &[]);
            b.switch_to_block(float_blk);
            let d = val::f64(b, w);
            let i = b.ins().fcvt_to_sint_sat(I32, d);
            let back = b.ins().fcvt_from_sint(F64, i);
            let same = b.ins().fcmp(FloatCC::Equal, back, d);
            // -0 converts to 0 and back to +0, which compares equal: check
            // the bits.
            let bits = b.ins().bitcast(I64, d);
            let negzero = b.ins().icmp_imm_s(IntCC::Equal, bits, i64::MIN);
            let ok = b.ins().band_not(same, negzero);
            let i = b.ins().sextend(I64, i);
            let exact = b.create_block();
            b.ins().brif(ok, exact, &[], decline, &[]);
            b.switch_to_block(exact);
            b.ins().jump(join, &[BlockArg::Value(i)]);
            b.switch_to_block(join);
            V::Int(v)
        }
        Ty::Bool => {
            let is_bool = val::is(b, w, jsvalue::TAG_BOOL);
            let ok = b.create_block();
            b.ins().brif(is_bool, ok, &[], decline, &[]);
            b.switch_to_block(ok);
            let i = b.ins().ireduce(I32, w);
            V::Bool(b.ins().icmp_imm_s(IntCC::NotEqual, i, 0))
        }
        Ty::Arr(kind) => {
            // Only checked: the entry passes it on as a JS value.
            let is_obj = val::is(b, w, jsvalue::TAG_OBJECT);
            let obj = b.create_block();
            let ok = b.create_block();
            b.ins().brif(is_obj, obj, &[], decline, &[]);
            b.switch_to_block(obj);
            let p = val::ptr(b, w);
            let class = b.ins().uload16(I32, M_HEADER, p, layout::OBJ_CLASS_ID);
            let want = layout::CLASS_TYPED_ARRAY + kind as i64;
            let is_kind = b.ins().icmp_imm_s(IntCC::Equal, class, want);
            b.ins().brif(is_kind, ok, &[], decline, &[]);
            b.switch_to_block(ok);
            V::Void
        }
        Ty::StructArr(k) => {
            // Only checked: an array of objects of this layout.
            let is_obj = val::is(b, w, jsvalue::TAG_OBJECT);
            let obj = b.create_block();
            b.ins().brif(is_obj, obj, &[], decline, &[]);
            b.switch_to_block(obj);
            let p = val::ptr(b, w);
            let ok = sarr_check(b, p, k, sarr_class);
            let yes = b.create_block();
            b.ins().brif(ok, yes, &[], decline, &[]);
            b.switch_to_block(yes);
            V::Void
        }
        Ty::Struct(k) => {
            // Only checked: an object of the class with this layout.
            let is_obj = val::is(b, w, jsvalue::TAG_OBJECT);
            let obj = b.create_block();
            b.ins().brif(is_obj, obj, &[], decline, &[]);
            b.switch_to_block(obj);
            let p = val::ptr(b, w);
            let ok = struct_check(b, p, k, struct_class);
            let yes = b.create_block();
            b.ins().brif(ok, yes, &[], decline, &[]);
            b.switch_to_block(yes);
            V::Void
        }
        Ty::StructOrUndef(k) => {
            // Only checked: an object of this layout, `null` or `undefined`.
            let is_obj = val::is(b, w, jsvalue::TAG_OBJECT);
            let obj = b.create_block();
            let other = b.create_block();
            let yes = b.create_block();
            b.ins().brif(is_obj, obj, &[], other, &[]);
            b.switch_to_block(other);
            let is_undef = val::is(b, w, jsvalue::TAG_UNDEFINED);
            let is_null = val::is(b, w, jsvalue::TAG_NULL);
            let nullish = b.ins().bor(is_undef, is_null);
            b.ins().brif(nullish, yes, &[], decline, &[]);
            b.switch_to_block(obj);
            let p = val::ptr(b, w);
            let ok = struct_check(b, p, k, struct_class);
            b.ins().brif(ok, yes, &[], decline, &[]);
            b.switch_to_block(yes);
            V::Void
        }
        _ => unreachable!(),
    }
}

/// Whether object `p` is of `aot type` layout `k` (`struct_class`: the
/// address of `lum_struct_class`). Its class first: the opaque of any
/// other object may be anything.
pub(crate) fn struct_check(b: &mut FunctionBuilder, p: Value, k: u32, struct_class: Value) -> Value {
    let join = b.create_block();
    let r = b.append_block_param(join, I8);
    let class = b.ins().uload16(I32, M_HEADER, p, layout::OBJ_CLASS_ID);
    let want = b.ins().load(I32, M_CONST, struct_class, 0);
    let same_class = b.ins().icmp(IntCC::Equal, class, want);
    let data = b.create_block();
    let no = b.ins().iconst(I8, 0);
    b.ins().brif(same_class, data, &[], join, &[BlockArg::Value(no)]);
    b.switch_to_block(data);
    let s = b.ins().load(I64, M_HEADER, p, layout::OBJ_OPAQUE);
    let l = b.ins().load(I32, M_STRUCT, s, 0);
    let same = b.ins().icmp_imm_s(IntCC::Equal, l, k as i64);
    b.ins().jump(join, &[BlockArg::Value(same)]);
    b.switch_to_block(join);
    r
}

/// Whether object `p` is an array of objects of layout `k` (`sarr_class`:
/// the address of `lum_sarr_class`).
pub(crate) fn sarr_check(b: &mut FunctionBuilder, p: Value, k: u32, sarr_class: Value) -> Value {
    struct_check(b, p, k, sarr_class)
}

/// `Lower::box_number` for code outside a lowered body.
fn box_number(b: &mut FunctionBuilder, d: Value) -> Value {
    let u = b.ins().bitcast(I64, d);
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
    let int_w = b.ins().uextend(I64, i);
    let int_w = val::mk_int(b, int_w);
    let float_w = val::float(b, d);
    b.ins().select(is_int, int_w, float_w)
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
    for f in m.funcs.iter().chain(&m.closures) {
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

pub(crate) use luminous_front::ir::visit as walk_expr;

/// Visit every expression in `body`.
fn walk(body: &[Stmt], f: &mut impl FnMut(&Expr)) {
    for st in body {
        match st {
            Stmt::Expr(x) | Stmt::Throw(x) | Stmt::Await { value: x, .. } | Stmt::Yield { value: x, .. } => {
                walk_expr(x, f)
            }
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
            Stmt::IterGuard { body, .. } => walk(body, f),
            Stmt::Try { body, catch, finally, .. } => {
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

/// The rest parameter `k`: the arguments from `k` on, in a new array.
fn rest_arg(
    b: &mut FunctionBuilder,
    rest: FuncRef,
    js_ctx: Value,
    argc: Value,
    argv: Value,
    k: usize,
    exception: Block,
) -> V {
    let start = b.ins().iconst(I32, k as i64);
    let call = b.ins().call(rest, &[js_ctx, argc, argv, start]);
    let r = b.inst_results(call)[0];
    let failed = b.ins().icmp_imm_s(IntCC::Equal, r, jsvalue::val_const(jsvalue::TAG_EXCEPTION, 0));
    let ok = b.create_block();
    b.ins().brif(failed, exception, &[], ok, &[]);
    b.switch_to_block(ok);
    V::Any(r)
}
