//! Lowering of function bodies to the backend-neutral IR (`mir.rs`).
//!
//! Values: `F64` / `Bool` are plain IR values. `Any` is a QuickJS
//! `JSValue`: one `I64`, NaN-boxed (a double its own bits, any other
//! value its tag in the high half and its payload, a reference the
//! address / 8, in the low one: see `jsvalue`). It is
//! *owned*: every `Any` expression evaluates to a new reference that the
//! consumer must either store, pass on to a callee that takes ownership,
//! or free.
//!
//! Exceptions: helpers report them by returning `JS_EXCEPTION` (or a negative
//! int); native callees by setting `lum_unwinding`. Either way control goes
//! to an exception edge, which frees the owned temporaries that are live at
//! that point (`Lower::temps`) and jumps to the innermost handler: a `catch`
//! or `finally` block, or the function's exception exit, which frees all
//! `Any` locals and propagates.

use std::collections::{HashMap, HashSet};

use crate::mir::types::{F32, F64, I8, I32, I64};
use crate::mir::{
    Block, BlockArg, FloatCC, FuncRef, FunctionBuilder, GlobalValue, Ins, IntCC, MemFlags, Region, SigRef, StackSlot,
    StackSlotData, StackSlotKind, Type, Value, Variable,
};
use luminous_front::ir::{
    self, AnyBinOp, AnyCmpOp, AnyUnOp, ArrKind, BinOp, CmpOp, Expr, ExprKind, Key, LogicalOp, LoopKind,
    MAYBE_F64_UNDEFINED, MathFn, Stmt, Ty, UnOp,
};

use crate::jsvalue::*;
use crate::{layout, val};

/// Typed array kinds (`JSTypedArrayEnum`), each with a pristine shape.
const TYPED_ARRAYS: i64 = 12;

/// `JSBinaryOperatorEnum` / `JSUnaryOperatorEnum` of the engine patch.
mod op {
    pub const ADD: i64 = 0;
    pub const SUB: i64 = 1;
    pub const MUL: i64 = 2;
    pub const DIV: i64 = 3;
    pub const MOD: i64 = 4;
    pub const POW: i64 = 5;
    pub const SHL: i64 = 6;
    pub const SAR: i64 = 7;
    pub const SHR: i64 = 8;
    pub const AND: i64 = 9;
    pub const OR: i64 = 10;
    pub const XOR: i64 = 11;
    pub const LT: i64 = 12;
    pub const LTE: i64 = 13;
    pub const GT: i64 = 14;
    pub const GTE: i64 = 15;
    pub const EQ: i64 = 16;
    pub const NEQ: i64 = 17;
    pub const STRICT_EQ: i64 = 18;
    pub const STRICT_NEQ: i64 = 19;
    pub const IN: i64 = 20;
    pub const INSTANCEOF: i64 = 21;
    pub const NEG: i64 = 0;
    pub const PLUS: i64 = 1;
    pub const BITNOT: i64 = 2;
    pub const INC: i64 = 3;
    pub const DEC: i64 = 4;
    pub const TONUMERIC: i64 = 5;
    pub const TYPEOF: i64 = 6;
}

/// `JS_PROP_THROW`.
const PROP_THROW: i64 = 1 << 14;

/// Memory accesses by region (see `mir::Region`).
pub(crate) const M_RC: MemFlags = MemFlags::of(Region::RefCount);
pub(crate) const M_SLOT: MemFlags = MemFlags::of(Region::Slot);
pub(crate) const M_HEADER: MemFlags = MemFlags::of(Region::Header);
pub(crate) const M_ELEM: MemFlags = MemFlags::of(Region::Elem);
pub(crate) const M_STRUCT: MemFlags = MemFlags::of(Region::Struct);
pub(crate) const M_IC: MemFlags = MemFlags::of(Region::Ic);
pub(crate) const M_TABLE: MemFlags = MemFlags::of(Region::Table);
pub(crate) const M_CONST: MemFlags = MemFlags::of(Region::Const);
pub(crate) const M_FLAG: MemFlags = MemFlags::of(Region::Flag);
pub(crate) const M_LIMIT: MemFlags = MemFlags::of(Region::Limit);
pub(crate) const M_ALLOC: MemFlags = MemFlags::of(Region::Alloc);

/// A lowered value.
#[derive(Clone, Copy, Debug)]
pub(crate) enum V {
    Num(Value),
    /// An `I32` value as a sign-extended `i64`: only where values cross a
    /// call, a return or a block boundary (see `Lower::abi`).
    Int(Value),
    Bool(Value),
    /// A `JSValue` (see `jsvalue`).
    Any(Value),
    Void,
}

impl V {
    fn num(self) -> Value {
        match self {
            V::Num(v) => v,
            other => panic!("expected a number, got {other:?}"),
        }
    }
    fn boolean(self) -> Value {
        match self {
            V::Bool(v) => v,
            other => panic!("expected a boolean, got {other:?}"),
        }
    }
    fn any(self) -> Value {
        match self {
            V::Any(w) => w,
            other => panic!("expected a JS value, got {other:?}"),
        }
    }
    /// As IR values (block arguments, returns).
    pub(crate) fn values(self) -> Vec<Value> {
        match self {
            V::Num(v) | V::Bool(v) | V::Int(v) | V::Any(v) => vec![v],
            V::Void => vec![],
        }
    }
}

/// IR types of a value of type `t`.
pub(crate) fn clif_types(t: Ty) -> Vec<Type> {
    match t {
        Ty::I32 => vec![I64],
        Ty::F64 | Ty::MaybeF64 => vec![F64],
        Ty::Bool => vec![I8],
        Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_) => vec![I64],
        Ty::Void => vec![],
    }
}

/// Whether values of type `t` are `JSValue`s.
pub(crate) fn is_js(t: Ty) -> bool {
    matches!(t, Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_))
}

/// Rebuild a `V` of type `t` from IR values.
pub(crate) fn from_values(t: Ty, vs: &[Value]) -> V {
    match t {
        Ty::I32 => V::Int(vs[0]),
        Ty::F64 | Ty::MaybeF64 => V::Num(vs[0]),
        Ty::Bool => V::Bool(vs[0]),
        Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_) => V::Any(vs[0]),
        Ty::Void => V::Void,
    }
}

#[derive(Clone, Copy)]
enum LVar {
    One(Variable),
    /// An `F64` local that only ever holds integers (see `int_locals`), as
    /// an `i64`.
    Int(Variable),
    /// A JS value.
    Val(Variable),
    /// Captured by closures: the box object (owned unless the box comes
    /// from the closure's own captures) and a pointer to its value slot.
    Boxed {
        obj: Variable,
        slot: Variable,
        owned: bool,
    },
}

/// Where lowered code goes when it leaves the body.
#[derive(Clone, Copy)]
pub(crate) enum Exit {
    /// Return from the IR function (on exception: set
    /// `lum_unwinding` and return placeholder values).
    Return,
    /// Jump to a block: for `ret`, with the return value as its parameters;
    /// for `unwind`, without parameters.
    Jump(Block),
}

#[derive(Clone, Copy)]
pub(crate) struct Exits {
    pub ret: Exit,
    pub unwind: Exit,
}

/// References to the module's functions and data.
/// Builtin methods that calls reaching the original (pinned) function run
/// inline, for receivers they suit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// `String.prototype.charCodeAt`.
    CharCodeAt,
    /// `Array.prototype.push` (one argument).
    ArrayPush,
    /// `String.prototype.slice`, `substring`, `indexOf`, `includes`,
    /// `startsWith`, `endsWith` (in this order; `JS_CallStringFn`), with
    /// at most two arguments.
    StringFn(i32),
    /// `Map.prototype.get`, `set`, `has` (or `Set.prototype.has`),
    /// `Set.prototype.add` (`JS_CallMapFn`), with at most two arguments.
    MapFn(i32),
    /// `JSON.stringify` (one argument).
    JsonStringify,
}

impl Method {
    pub(crate) fn named(name: &str) -> Option<Method> {
        match name {
            "charCodeAt" => Some(Method::CharCodeAt),
            "push" => Some(Method::ArrayPush),
            "slice" => Some(Method::StringFn(0)),
            "substring" => Some(Method::StringFn(1)),
            "indexOf" => Some(Method::StringFn(2)),
            "includes" => Some(Method::StringFn(3)),
            "startsWith" => Some(Method::StringFn(4)),
            "endsWith" => Some(Method::StringFn(5)),
            "get" => Some(Method::MapFn(0)),
            "set" => Some(Method::MapFn(1)),
            "has" => Some(Method::MapFn(2)),
            "add" => Some(Method::MapFn(3)),
            "stringify" => Some(Method::JsonStringify),
            _ => None,
        }
    }
}

/// Indices in `lum_shapes` (see `luminous_rt.h`).
/// The shape of a fresh array (`[]`): `length` writable, own properties
/// none else, the original prototype.
const SHAPE_ARRAY: i32 = 12;
const PIN_STRING_PROTO: i32 = 24;
const PIN_CHAR_CODE_AT: i32 = 25;
const PIN_ARRAY_PUSH: i32 = 26;
/// `Array.prototype.forEach`, `map`, `filter`, `some`, `every`, `find`,
/// `findIndex` (in `LoopKind` order).
const PIN_ARRAY_LOOPS: i32 = 27;
/// `Array.prototype.sort`.
const PIN_ARRAY_SORT: i32 = 34;
/// The methods of `Method::StringFn`.
const PIN_STRING_FNS: i32 = 35;
/// `Map.prototype.get`, `set`, `has`, `Set.prototype.add`, `has`.
const PIN_MAP_FNS: i32 = 41;
/// `JSON.stringify`.
const PIN_JSON_STRINGIFY: i32 = 46;
/// `LUM_IC_STRING`: a method of String.prototype read from a string.
const IC_STRING: i64 = 8;

pub(crate) struct Env {
    pub helpers: HashMap<&'static str, FuncRef>,
    pub funcs: Vec<FuncRef>,
    /// The merge sorts calling native `sort` comparators
    /// (`Codegen::define_sorter`).
    pub sorters: HashMap<usize, FuncRef>,
    pub stack_limit: GlobalValue,
    pub unwinding: GlobalValue,
    pub cells: GlobalValue,
    pub strings: GlobalValue,
    pub atoms: GlobalValue,
    /// `lum_shapes`: the shapes of a pristine typed array (by kind), then
    /// of a pristine array.
    pub shapes: GlobalValue,
    /// `lum_ics`: inline caches, and the next free one.
    pub ics: GlobalValue,
    /// `lum_privates` / `lum_homes`: private name values and home objects
    /// of classes (see `ir::Module`).
    pub privates: GlobalValue,
    pub homes: GlobalValue,
    /// `lum_struct_class`: the class of `aot type` objects.
    pub struct_class: GlobalValue,
    /// `lum_sarr_class`: the class of arrays of `aot type` objects.
    pub sarr_class: GlobalValue,
    /// `lum_record_shapes`: per record type, its shape once learned.
    pub record_shapes: GlobalValue,
    /// `lum_fn_caches`: per native function, its closures' shape, and how
    /// native code makes them inline (`FN_CACHE_SIZE` bytes each).
    pub fn_caches: GlobalValue,
    /// `js_proto_epoch`: the engine's count of changes to watched
    /// prototypes (see `LumIC.epoch`).
    pub proto_epoch: GlobalValue,
    pub next_ic: std::rc::Rc<std::cell::Cell<usize>>,
    /// `FuncId` of closure 0.
    pub closure_base: usize,
    /// Parameter types, by `FuncId`.
    pub params: std::rc::Rc<Vec<Vec<Ty>>>,
    /// `value_captures`, by `ClosureId`.
    pub value_captures: std::rc::Rc<Vec<Vec<bool>>>,
    /// The atoms `Promise` and `resolve`, if used.
    pub promise_resolve: Option<(ir::AtomId, ir::AtomId)>,
    /// Atoms naming builtin methods with fast paths.
    pub methods: std::rc::Rc<HashMap<ir::AtomId, Method>>,
    /// Per function: it can neither throw nor call (see `nothrow_leaf`).
    pub nothrow: std::rc::Rc<Vec<bool>>,
    /// JS entries (`lum_fast_<i>`), by `FuncId`.
    pub fast: Vec<FuncRef>,
    /// Return types, by `FuncId`.
    pub rets: std::rc::Rc<Vec<Ty>>,
    /// `aot type` layouts.
    pub layouts: std::rc::Rc<Vec<ir::Layout>>,
    /// Stores into `int` fields wrap.
    pub int_wrapping: bool,
    /// The string constants' text (`ir::Module::strings`).
    pub texts: std::rc::Rc<Vec<String>>,
    /// The signature of `JSCFunctionFast` entries.
    pub fast_sig: SigRef,
}

/// An exception edge: its block, the temporaries to free there, the handler.
type ExceptionEdge = (Block, Vec<Value>, Block);

enum Scope {
    Jump(ir::LoopId),
    /// A `finally` block to run when a jump or return leaves its `try`, with
    /// the handler and scope depths in effect around the `try`.
    Finally {
        body: Vec<Stmt>,
        handlers: usize,
        scopes: usize,
    },
    /// An iterator to close when a jump or return leaves its guard (but not
    /// a `continue` of loop `loop_id`).
    CloseIter {
        iter: ir::LocalId,
        next: ir::LocalId,
        loop_id: Option<ir::LoopId>,
        handlers: usize,
        scopes: usize,
    },
}

pub(crate) struct Lower {
    pub b: FunctionBuilder,
    env: Env,
    ctx: Value,
    ptr: Type,
    ret: Ty,
    locals: Vec<Ty>,
    names: Vec<Option<ir::AtomId>>,
    vars: Vec<LVar>,
    /// `Any` locals: an `F64` variable holding the value's number whenever
    /// it is one (see `num_shadow`).
    shadows: Vec<Option<Variable>>,
    /// `F64` locals only ever assigned characters' codes (`StrCharCode`)
    /// and integer constants: variables holding the code as an integer
    /// (-1 where it is NaN) and whether it is one (see `char_code_locals`).
    code_vars: Vec<Option<(Variable, Variable)>>,
    /// Number "shadows" of JS values: for a payload (of a value made by
    /// arithmetic, or read from an `Any` local), the number it holds when
    /// its tag is a number's. `number_of` uses it instead of converting the
    /// payload, so arithmetic on boxed numbers needs no boxing in between
    /// (the boxing is left unused).
    num_shadow: HashMap<Value, Value>,
    /// For a JS value made where it may or may not be a number (a binary
    /// operation's result: numbers but from its slow path), an `I8` that
    /// says whether it is one (constant on the number paths, so that tests
    /// on it fold away where the value came from one).
    num_known: HashMap<Value, Value>,
    /// Functions read through an inline cache (a prototype's method or
    /// getter): their JS entry (`lum_fast_<i>`), as the cache keeps it, or
    /// 0 where not known (see `fn_entry`).
    fn_entries: HashMap<Value, Value>,
    /// In a speculative region: the property array of each guarded object.
    spec_src: HashMap<ir::LocalId, Value>,
    /// In a speculative region: whether every value its guard checked is
    /// a float (not an int32).
    spec_floats: bool,
    /// The objects of a region's iterations (`ElemGuard`), and whether the
    /// iteration being lowered is the copy where their fields hold floats
    /// (else int32s too: read and written with their tags).
    spec_elems: HashSet<ir::LocalId>,
    /// The elements (of the iterations being lowered) whose fields hold
    /// floats (see `elem_guard`).
    spec_float_elems: HashSet<ir::LocalId>,
    /// loop id -> (continue target, break target)
    loops: HashMap<ir::LoopId, (Option<Block>, Block)>,
    /// Inlined calls being lowered (innermost last): where their `return`s
    /// go, and the value's type.
    inline_rets: Vec<(Block, Ty)>,
    /// A prediction for the next call, getter or setter lowered (see
    /// `ExprKind::Predicted`).
    preds: Vec<Pred>,
    /// The method read being lowered is a predicted call's (one of a
    /// polymorphic call site's methods, or a method its receivers of
    /// several classes inherit: see `prop_get_inner`).
    poly_site: bool,
    /// Owned values to free if an exception occurs now.
    temps: Vec<Value>,
    /// Innermost-last exception handler blocks.
    handlers: Vec<Block>,
    scopes: Vec<Scope>,
    /// Exception edges: (block, temporaries to free, handler).
    pending: Vec<ExceptionEdge>,
    exc_exit: Block,
    ret_block: Block,
    exits: Exits,
    /// Methods: the receiver (borrowed).
    pub(crate) this: Option<Value>,
    /// Closures: their function object's data slots.
    closure_data: Option<Value>,
    async_cx: Option<AsyncCx>,
    /// An async generator: `await` suspends to the engine (kind 3).
    async_gen: bool,
    /// See `ir::Func::ret_slot`.
    ret_slot: Option<ir::LocalId>,
    /// Parameters held by the caller (not released here; see
    /// `borrow_params`).
    borrowed: HashSet<ir::LocalId>,
    /// How many times the function names each local (`ir::local_uses`),
    /// and its number of parameters.
    local_uses: Vec<usize>,
    n_params: usize,
    /// `f64` values that are exact integers by construction (int locals,
    /// integral constants), and their `i64` value.
    int_of: HashMap<Value, Value>,
    /// Of those, the ones in the int32 range.
    i32s: std::collections::HashSet<Value>,
    /// Exact integers known to be below 2^bits in magnitude (int32s are
    /// at most 2^31), and integral constants' values: sums and products
    /// that stay below 2^53 are then exact in integers too.
    int_bits: HashMap<Value, u32>,
    int_consts: HashMap<Value, i64>,
    /// Products of two int32s: their exact `i64` value (the `f64` may be
    /// rounded, which cannot change how it compares to an int32).
    wide_of: HashMap<Value, Value>,
    /// `%` results: their integer value (the `f64` may be -0 for 0), valid
    /// when the flag (the divisor was not 0) is set; else the `f64` is NaN.
    rem_of: HashMap<Value, RemInt>,
    /// Int locals whose current update cannot pass 2^53: loop counters
    /// below a constant bound (see `bounded_counter`).
    bounded: Vec<ir::LocalId>,
    /// Int locals that need no range checks (see `small_ints`).
    small: Vec<bool>,
    /// Int locals always in the int32 range (see `i32_locals`).
    i32_range: Vec<bool>,
    /// Exact integers (in `int_of`) known not to be negative.
    nonneg: std::collections::HashSet<Value>,
    /// Lower bounds of the counters of each loop (see `loop_bounds`), and
    /// of the loops being lowered.
    loop_lo: HashMap<ir::LoopId, Vec<(ir::LocalId, i64)>>,
    active_lo: Vec<(ir::LocalId, i64)>,
    /// Exact integers (in `int_of`) known to be at least this.
    int_lo: HashMap<Value, i64>,
}

/// Integers to compare two numbers by, and the flags under which they are
/// valid, each with `RemInt::nan` (see `Lower::int_pair`).
type IntPair = (Value, Value, Vec<(Value, bool)>);

/// The integer form of a `%` result (see `Lower::rem_of`).
#[derive(Clone, Copy)]
struct RemInt {
    /// The exact remainder, where `ok`.
    int: Value,
    ok: Value,
    /// Where not `ok`, the result is NaN (`% 0`); else just a double, to
    /// compare as one.
    nan: bool,
    /// Below 2^53 in magnitude, so comparable with rounded products.
    small: bool,
    /// Where not `ok`, the integer this holds (a `charCodeAt` past the end:
    /// -1), if a fixed one: a comparison with a constant that gives the
    /// same at it as at NaN then needs no flag.
    none: Option<i64>,
}

impl Lower {
    /// Set up lowering of a body with locals typed `locals`, the first ones
    /// being the parameters (`params`, already in `b`'s current block).
    /// Closures also get `closure_data`, their function object's data slots.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        mut b: FunctionBuilder,
        env: Env,
        ctx: Value,
        ptr: Type,
        f: &ir::Func,
        params: &[V],
        closure_data: Option<Value>,
        exits: Exits,
    ) -> Self {
        let ints = int_locals(f, &predicted_params(f, &env.params));
        let small = small_ints(f, &ints);
        let i32_range = i32_locals(f, &ints);
        let vars: Vec<LVar> = f
            .locals
            .iter()
            .enumerate()
            .map(|(i, &t)| match t {
                _ if ints[i] || t == Ty::I32 => LVar::Int(b.declare_var(I64)),
                _ if f.boxed[i] => LVar::Boxed {
                    obj: b.declare_var(I64),
                    slot: b.declare_var(I64),
                    // Step functions own every box (captures are dup'ed
                    // into their frame).
                    owned: f.suspends() || !f.captures.contains(&i),
                },
                Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_) => {
                    LVar::Val(b.declare_var(I64))
                }
                t => LVar::One(b.declare_var(clif_types(t)[0])),
            })
            .collect();
        let shadows = f
            .locals
            .iter()
            .zip(&vars)
            .map(|(&t, v)| (t == Ty::Any && matches!(v, LVar::Val(..))).then(|| b.declare_var(F64)))
            .collect();
        let codes = char_code_locals(f, &predicted_params(f, &env.params));
        let code_vars = codes
            .iter()
            .zip(&vars)
            .map(|(&c, v)| (c && matches!(v, LVar::One(_))).then(|| (b.declare_var(I64), b.declare_var(I8))))
            .collect();
        let exc_exit = b.create_block();
        b.set_cold_block(exc_exit);
        let ret_block = b.create_block();
        for t in clif_types(f.ret) {
            b.append_block_param(ret_block, t);
        }
        let mut l = Lower {
            b,
            env,
            ctx,
            ptr,
            ret: f.ret,
            locals: f.locals.clone(),
            names: f.local_names.clone(),
            vars,
            shadows,
            code_vars,
            num_shadow: HashMap::new(),
            num_known: HashMap::new(),
            fn_entries: HashMap::new(),
            spec_src: HashMap::new(),
            spec_floats: false,
            spec_elems: HashSet::new(),
            spec_float_elems: HashSet::new(),
            loops: HashMap::new(),
            inline_rets: Vec::new(),
            preds: Vec::new(),
            poly_site: false,
            temps: Vec::new(),
            handlers: Vec::new(),
            scopes: Vec::new(),
            pending: Vec::new(),
            exc_exit,
            ret_block,
            exits,
            this: None,
            closure_data,
            async_cx: None,
            async_gen: false,
            ret_slot: f.ret_slot,
            borrowed: HashSet::new(),
            local_uses: ir::local_uses(&f.body, f.locals.len()),
            n_params: f.params.len(),
            int_of: HashMap::new(),
            i32s: Default::default(),
            wide_of: HashMap::new(),
            int_bits: HashMap::new(),
            int_consts: HashMap::new(),
            rem_of: HashMap::new(),
            bounded: Vec::new(),
            small,
            i32_range,
            nonneg: Default::default(),
            loop_lo: loop_bounds(&f.body),
            active_lo: Vec::new(),
            int_lo: HashMap::new(),
        };
        for (i, &t) in f.locals.iter().enumerate() {
            if let LVar::Boxed { obj, slot, .. } = l.vars[i] {
                // Boxes are created when their scope is entered (or come
                // from the closure, below).
                let zero = l.b.ins().iconst(I64, 0);
                let undef = l.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0));
                l.b.def_var(obj, undef);
                l.b.def_var(slot, zero);
                continue;
            }
            let v = match params.get(i) {
                // A widened parameter is boxed on entry.
                Some(&p) => match (t, p) {
                    (Ty::Any, V::Num(d)) => V::Any(l.box_number(d)),
                    (Ty::Any, V::Int(i)) => {
                        let d = l.int_f64(i);
                        V::Any(l.box_number(d))
                    }
                    (Ty::Any, V::Bool(x)) => V::Any(l.box_bool(x)),
                    (_, p) => p,
                },
                None => l.zero(t),
            };
            // (A character code's local starts as the integer 0 too.)
            if let (V::Num(d), Some(_), None) = (v, l.code_vars[i], params.get(i)) {
                l.int_consts.insert(d, 0);
            }
            l.def_local(i, v);
        }
        // A closure's data slots hold, per captured variable, its box object
        // and the address of the box's value (as a non-reference value: an
        // address's high half is no reference's tag). A value capture holds
        // the value itself, which its slot points at.
        if let Some(data) = closure_data {
            for (k, &cl) in f.captures.iter().enumerate() {
                let LVar::Boxed { obj, slot, .. } = l.vars[cl] else { unreachable!() };
                let at = (2 * k * SIZE) as i32;
                let w = l.b.ins().load(I64, M_SLOT, data, at);
                let s = if f.value_captures[k] {
                    l.b.ins().iadd_imm_s(data, at as i64)
                } else {
                    l.b.ins().load(I64, M_SLOT, data, at + SIZE as i32)
                };
                l.b.def_var(obj, w);
                l.b.def_var(slot, s);
            }
        }
        l
    }

    fn is_filled(&self) -> bool {
        self.b.is_filled()
    }

    /// After a terminator, continue emitting into a fresh unreachable block.
    fn after_terminator(&mut self) {
        let dead = self.b.create_block();
        self.b.switch_to_block(dead);
    }

    fn zero(&mut self, t: Ty) -> V {
        match t {
            Ty::F64 | Ty::MaybeF64 => V::Num(self.b.ins().f64const(0.0)),
            Ty::I32 => V::Int(self.b.ins().iconst(I64, 0)),
            Ty::Bool => V::Bool(self.b.ins().iconst(I8, 0)),
            Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_) => {
                self.konst(0, TAG_UNDEFINED)
            }
            Ty::Void => V::Void,
        }
    }

    /// A value of a tag that is no reference's nor a float's, with a
    /// 32-bit payload.
    fn konst(&mut self, payload: i64, tag: i64) -> V {
        V::Any(self.b.ins().iconst(I64, val_const(tag, payload)))
    }

    // ---- values (see `jsvalue`, `val`) ----

    /// Whether value `w` has tag `tag`.
    fn val_is(&mut self, w: Value, tag: i64) -> Value {
        val::is(&mut self.b, w, tag)
    }

    /// Whether value `w` is a float.
    fn val_is_float(&mut self, w: Value) -> Value {
        val::is_float(&mut self.b, w)
    }

    /// Whether value `w` is a reference (to a cell: an object, string,
    /// symbol, bigint...).
    fn val_is_ref(&mut self, w: Value) -> Value {
        val::is_ref(&mut self.b, w)
    }

    /// The address a reference `w` holds.
    fn val_ptr(&mut self, w: Value) -> Value {
        val::ptr(&mut self.b, w)
    }

    /// A reference to the cell at `p`, tagged `tag`.
    fn mk_ref(&mut self, tag: i64, p: Value) -> Value {
        val::mk_ref(&mut self.b, tag, p)
    }

    /// The int32 payload of `w` (an int, a bool), sign-extended to `I64`.
    fn val_int(&mut self, w: Value) -> Value {
        val::int(&mut self.b, w)
    }

    /// The int value of `i` (an `i64` in the int32 range, or a `u32`
    /// below 2^31).
    fn mk_int(&mut self, i: Value) -> Value {
        val::mk_int(&mut self.b, i)
    }

    /// The double a float value `w` holds.
    fn val_f64(&mut self, w: Value) -> Value {
        val::f64(&mut self.b, w)
    }

    /// A double as a JS value, as is (the interpreter's `js_float64`):
    /// integral values are not made int32s.
    fn box_float(&mut self, d: Value) -> Value {
        let w = self.box_float_raw(d);
        self.num_shadow.insert(w, d);
        w
    }

    /// The float value of `d` (NaN made the one NaN, whose bits are no
    /// other value's).
    fn box_float_raw(&mut self, d: Value) -> Value {
        val::float(&mut self.b, d)
    }

    /// The bool value of `x` (an `I8` 0 or 1).
    fn box_bool(&mut self, x: Value) -> Value {
        val::boolean(&mut self.b, x)
    }

    fn helper(&mut self, name: &str, args: &[Value]) -> Vec<Value> {
        let f = *self.env.helpers.get(name).unwrap_or_else(|| panic!("helper {name} not declared"));
        let call = self.b.ins().call(f, args);
        self.b.inst_results(call).to_vec()
    }

    fn mem() -> MemFlags {
        MemFlags::trusted()
    }

    // ---- locals ----

    fn def_local(&mut self, l: ir::LocalId, v: V) {
        match (self.vars[l], v) {
            (LVar::One(var), V::Num(x)) if self.code_vars[l].is_some() => {
                self.b.def_var(var, x);
                let (iv, okv) = self.code_vars[l].expect("checked");
                let (int, ok) = match (self.rem_of.get(&x), self.int_consts.get(&x)) {
                    (Some(r), _) if r.nan && r.none == Some(-1) => (r.int, r.ok),
                    (_, Some(&c)) => (self.b.ins().iconst(I64, c), self.b.ins().iconst(I8, 1)),
                    _ => unreachable!("local {l}: a character code or an integer constant (see char_code_locals)"),
                };
                self.b.def_var(iv, int);
                self.b.def_var(okv, ok);
            }
            (LVar::One(var), V::Num(x) | V::Bool(x)) => self.b.def_var(var, x),
            // Only ever an integer (by `int_locals`).
            (LVar::Int(var), V::Num(x)) => {
                let i = self.int_of_num(x);
                self.b.def_var(var, i)
            }
            (LVar::Int(var), V::Int(i)) => self.b.def_var(var, i),
            (LVar::Val(var), V::Any(w)) => {
                self.b.def_var(var, w);
                if let Some(sv) = self.shadows[l] {
                    let d = self.number_of(w);
                    self.b.def_var(sv, d);
                }
            }
            _ => panic!("local {l} type mismatch"),
        }
    }

    /// The local's value, borrowed. Boxed locals are checked for their TDZ.
    fn use_local(&mut self, l: ir::LocalId) -> V {
        match (self.vars[l], self.locals[l]) {
            (LVar::One(v), Ty::F64) if self.code_vars[l].is_some() => {
                let (iv, okv) = self.code_vars[l].expect("checked");
                let d = self.b.use_var(v);
                let int = self.b.use_var(iv);
                let ok = self.b.use_var(okv);
                self.rem_of.insert(d, RemInt { int, ok, nan: true, small: true, none: Some(-1) });
                V::Num(d)
            }
            (LVar::One(v), Ty::F64 | Ty::MaybeF64) => V::Num(self.b.use_var(v)),
            (LVar::One(v), Ty::Bool) => V::Bool(self.b.use_var(v)),
            (LVar::Int(v), t) => {
                let i = self.b.use_var(v);
                let d = self.int_f64(i);
                if t == Ty::I32 || self.i32_range[l] {
                    self.i32s.insert(d);
                }
                if let Some(&(_, lo)) = self.active_lo.iter().rev().find(|&&(m, _)| m == l) {
                    self.int_lo.insert(d, lo);
                    if lo >= 0 {
                        self.nonneg.insert(d);
                    }
                }
                V::Num(d)
            }
            (LVar::Val(var), _) => {
                let w = self.b.use_var(var);
                if let Some(sv) = self.shadows[l] {
                    let d = self.b.use_var(sv);
                    self.num_shadow.insert(w, d);
                }
                V::Any(w)
            }
            (LVar::Boxed { .. }, _) => {
                let w = self.load_box(l);
                self.check_tdz(l, w);
                V::Any(w)
            }
            _ => unreachable!(),
        }
    }

    fn box_slot(&mut self, l: ir::LocalId) -> Value {
        let LVar::Boxed { slot, .. } = self.vars[l] else { unreachable!() };
        self.b.use_var(slot)
    }

    /// The value in a boxed local's box, borrowed, without a TDZ check.
    fn load_box(&mut self, l: ir::LocalId) -> Value {
        let slot = self.box_slot(l);
        self.b.ins().load(I64, M_SLOT, slot, 0)
    }

    fn store_box(&mut self, l: ir::LocalId, w: Value) {
        let LVar::Boxed { obj, .. } = self.vars[l] else { unreachable!() };
        let slot = self.box_slot(l);
        self.b.ins().store(M_SLOT, w, slot, 0);
        // (The slot is in the box object's cell.)
        let o = self.b.use_var(obj);
        let p = self.val_ptr(o);
        self.write_barrier(p, w);
    }

    /// ReferenceError if a boxed local is still uninitialized.
    fn check_tdz(&mut self, l: ir::LocalId, w: Value) {
        let uninit = self.b.ins().icmp_imm_s(IntCC::Equal, w, val_const(TAG_UNINITIALIZED, 0));
        let throw = self.b.create_block();
        let cont = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(uninit, throw, &[], cont, &[]);
        self.b.switch_to_block(throw);
        let atom = self.atom(self.names[l].expect("boxed locals are named"));
        let ctx = self.ctx;
        self.helper("lum_throw_uninitialized_atom", &[ctx, atom]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(cont);
    }

    /// Fresh, uninitialized boxes for boxed locals (their scope is entered).
    fn new_boxes(&mut self, ls: &[ir::LocalId], copy_values: bool) {
        for &l in ls {
            let LVar::Boxed { obj, slot, owned } = self.vars[l] else { unreachable!() };
            debug_assert!(owned);
            let old_obj = self.b.use_var(obj);
            let old_val = if copy_values { Some(self.load_box(l)) } else { None };
            let out = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
            let out_addr = self.b.ins().stack_addr(self.ptr, out, 0);
            let ctx = self.ctx;
            let r = self.helper("lum_box_new", &[ctx, out_addr]);
            self.check_value(r[0]);
            let new_slot = self.b.ins().stack_load(self.ptr, I64, out, 0);
            self.b.def_var(obj, r[0]);
            self.b.def_var(slot, new_slot);
            if let Some(v) = old_val {
                // A new binding for the next iteration, with the same value.
                self.dup(v);
                self.store_box(l, v);
            }
            self.free(old_obj);
        }
    }

    /// The JS-valued parameters arrive borrowed: the caller holds them while
    /// the function runs (a native `sort` calls its comparator so, on the
    /// elements it holds). Those the body never assigns are not released;
    /// the others take a reference of their own.
    pub(crate) fn borrow_params(&mut self, f: &ir::Func) {
        for l in 0..f.params.len() {
            if !is_js(self.locals[l]) {
                continue;
            }
            let LVar::Val(..) = self.vars[l] else { unreachable!("parameters are not boxed") };
            let mut assigned = false;
            ir::visit_stmts(&f.body, &mut |e| {
                assigned |= matches!(
                    e.kind,
                    ExprKind::SetLocal(m, _) | ExprKind::InitLocal(m, _) | ExprKind::Update { local: m, .. } if m == l
                )
            });
            if assigned {
                let v = self.use_local(l).any();
                self.dup(v);
            } else {
                self.borrowed.insert(l);
            }
        }
    }

    /// What the function's locals hold, on leaving it. Nothing to do: the
    /// collector traces what is live (`free` is a no-op), and reading every
    /// local here would keep each one live up to the function's exits,
    /// carried around every loop (in a large function, hundreds of values
    /// moved between registers and stack slots per iteration).
    fn free_all_locals(&mut self) {}
    // ---- references ----
    // The engine traces what is reachable (from the native stack too, so
    // values in registers and stack slots are found): taking or dropping
    // a reference costs nothing. The calls stay as the places where values
    // are owned, for the analyses that move them.

    /// A new reference to `v` (borrowed).
    fn dup(&mut self, _v: Value) {}

    /// Release a reference.
    fn free(&mut self, _v: Value) {}

    /// The collector's write barrier, after value `w` is stored in the GC
    /// cell `owner` (an object, or a traced block such as a box): an old
    /// cell (one that survived a collection) not yet remembered is
    /// remembered, for the next minor collection to trace. Its header's
    /// flags byte: `mark` (old) the top bit, `gc_remembered` the next.
    fn write_barrier(&mut self, owner: Value, w: Value) {
        let rc = self.val_is_ref(w);
        let check = self.b.create_block();
        let call = self.b.create_block();
        let join = self.b.create_block();
        self.b.set_cold_block(call);
        self.b.ins().brif(rc, check, &[], join, &[]);
        self.b.switch_to_block(check);
        let flags = self.b.ins().uload8(I32, M_RC, owner, layout::GC_FLAGS);
        let bits = self.b.ins().band_imm_u(flags, 0xc0);
        let old = self.b.ins().icmp_imm_s(IntCC::Equal, bits, 0x80);
        self.b.ins().brif(old, call, &[], join, &[]);
        self.b.switch_to_block(call);
        let ctx = self.ctx;
        let rt = self.b.ins().load(I64, M_CONST, ctx, layout::CTX_RT);
        self.helper("JS_GCWriteBarrier", &[rt, owner]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    fn free_v(&mut self, v: V) {
        if let V::Any(w) = v {
            self.free(w);
        }
    }

    fn hold(&mut self, v: V) {
        if let V::Any(w) = v {
            self.temps.push(w);
        }
    }

    fn release(&mut self, v: V) {
        if let V::Any(w) = v {
            let i = self.temps.iter().rposition(|&x| x == w).expect("released value is held");
            self.temps.remove(i);
        }
    }

    // ---- exceptions ----

    /// A block to branch to when an exception is pending now.
    fn throw_edge(&mut self) -> Block {
        let blk = self.b.create_block();
        self.b.set_cold_block(blk);
        let target = self.handlers.last().copied().unwrap_or(self.exc_exit);
        self.pending.push((blk, self.temps.clone(), target));
        blk
    }

    fn branch_throw(&mut self, cond: Value) {
        let edge = self.throw_edge();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, edge, &[], cont, &[]);
        self.b.switch_to_block(cont);
    }

    /// Check a `JSValue` returned by a helper for `JS_EXCEPTION`.
    fn check_value(&mut self, w: Value) {
        let exc = self.b.ins().icmp_imm_s(IntCC::Equal, w, val_const(TAG_EXCEPTION, 0));
        self.branch_throw(exc);
    }

    /// Check an int returned by a helper (negative: exception).
    fn check_int(&mut self, r: Value) {
        let exc = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, r, 0);
        self.branch_throw(exc);
    }

    fn check_flag(&mut self) {
        let addr = self.b.ins().symbol_value(self.ptr, self.env.unwinding);
        let flag = self.b.ins().load(I8, M_FLAG, addr, 0);
        self.branch_throw(flag);
    }

    fn clear_flag(&mut self) {
        let addr = self.b.ins().symbol_value(self.ptr, self.env.unwinding);
        let zero = self.b.ins().iconst(I8, 0);
        self.b.ins().store(M_FLAG, zero, addr, 0);
    }

    /// Entry check of a function called from native code.
    pub(crate) fn stack_check(&mut self) {
        let limit_addr = self.b.ins().symbol_value(self.ptr, self.env.stack_limit);
        let limit = self.b.ins().load(self.ptr, M_LIMIT, limit_addr, 0);
        let sp = self.b.ins().get_stack_pointer(self.ptr);
        let overflow = self.b.ins().icmp(IntCC::UnsignedLessThan, sp, limit);
        let throw = self.b.create_block();
        let body = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(overflow, throw, &[], body, &[]);
        self.b.switch_to_block(throw);
        let ctx = self.ctx;
        self.helper("lum_stack_overflow", &[ctx]);
        let target = self.exc_exit;
        self.b.ins().jump(target, &[]);
        self.b.switch_to_block(body);
    }

    /// Lower a whole function body, including its exits.
    pub(crate) fn body(mut self, body: &[Stmt]) -> FunctionBuilder {
        self.stmts(body);
        if !self.is_filled() {
            // Falling off the end (or dead code after a terminator).
            let v = if self.ret == Ty::Any { self.konst(0, TAG_UNDEFINED) } else { self.zero(self.ret) };
            let args: Vec<BlockArg> = v.values().into_iter().map(BlockArg::Value).collect();
            let rb = self.ret_block;
            self.b.ins().jump(rb, &args);
        }

        let is_async = self.async_cx.is_some();

        // Normal exit: free the locals, then leave with the return value.
        let rb = self.ret_block;
        self.b.switch_to_block(rb);
        let rv: Vec<Value> = self.b.block_params(rb).to_vec();
        self.free_all_locals();
        if is_async {
            // The value the promise resolves to.
            let ret = self.ret;
            let w = match self.unabi(ret, &rv) {
                V::Num(d) => self.box_number(d),
                V::Bool(x) => self.box_bool(x),
                V::Any(w) => w,
                V::Void => self.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0)),
                V::Int(_) => unreachable!("converted by unabi"),
            };
            self.b.ins().return_(&[w]);
        } else {
            match self.exits.ret {
                Exit::Return => {
                    self.b.ins().return_(&rv);
                }
                Exit::Jump(blk) => {
                    let args: Vec<BlockArg> = rv.into_iter().map(BlockArg::Value).collect();
                    self.b.ins().jump(blk, &args);
                }
            }
        }

        // Exception exit.
        let ee = self.exc_exit;
        self.b.switch_to_block(ee);
        self.free_all_locals();
        if is_async {
            // The exception is pending; the runtime rejects the promise.
            // (The returned tag says so: a native callee's flag is not left
            // set for the next native call to find.)
            self.clear_flag();
            let e = self.b.ins().iconst(I64, val_const(TAG_EXCEPTION, 0));
            self.b.ins().return_(&[e]);
        } else {
            match self.exits.unwind {
                Exit::Return => {
                    let addr = self.b.ins().symbol_value(self.ptr, self.env.unwinding);
                    let one = self.b.ins().iconst(I8, 1);
                    self.b.ins().store(M_FLAG, one, addr, 0);
                    let z = self.zero(self.ret);
                    let vals = z.values();
                    self.b.ins().return_(&vals);
                }
                Exit::Jump(blk) => {
                    self.b.ins().jump(blk, &[]);
                }
            }
        }
        self.emit_dispatch();

        // Exception edges: free their live temporaries, then go to the
        // handler. Edges share the blocks freeing the temporaries they have
        // in common (held temporaries form a stack), as landing pads chain:
        // the code stays linear in the number of throwing points.
        let pending = std::mem::take(&mut self.pending);
        let mut chains: HashMap<(Block, Vec<Value>), Block> = HashMap::new();
        for (blk, temps, target) in pending {
            let to = self.cleanup_chain(target, &temps, &mut chains);
            self.b.switch_to_block(blk);
            self.b.ins().jump(to, &[]);
        }
        self.b
    }

    /// A block that frees `temps` (last first), then goes to `target`.
    fn cleanup_chain(
        &mut self,
        target: Block,
        temps: &[Value],
        chains: &mut HashMap<(Block, Vec<Value>), Block>,
    ) -> Block {
        let Some((&last, rest)) = temps.split_last() else { return target };
        if let Some(&b) = chains.get(&(target, temps.to_vec())) {
            return b;
        }
        let next = self.cleanup_chain(target, rest, chains);
        let b = self.b.create_block();
        self.b.set_cold_block(b);
        self.b.switch_to_block(b);
        self.free(last);
        self.b.ins().jump(next, &[]);
        chains.insert((target, temps.to_vec()), b);
        b
    }

    // ---- statements ----

    /// Jump to `t` if `c` holds, else to `f`: `&&`, `||`, `!` and `?:` of
    /// booleans as jumps, so that no boolean is made at each step (a test
    /// of `a || b || c` is a compare and a branch each).
    fn branch(&mut self, c: &Expr, t: Block, f: Block) {
        match &c.kind {
            ExprKind::Logical(op @ (LogicalOp::And | LogicalOp::Or), x, y)
                if c.ty == Ty::Bool && x.ty == Ty::Bool && y.ty == Ty::Bool =>
            {
                let next = self.b.create_block();
                if *op == LogicalOp::And {
                    self.branch(x, next, f);
                } else {
                    self.branch(x, t, next);
                }
                self.b.switch_to_block(next);
                self.branch(y, t, f);
            }
            ExprKind::Unary(UnOp::Not, x) if x.ty == Ty::Bool => self.branch(x, f, t),
            ExprKind::Cond(k, x, y) if c.ty == Ty::Bool && x.ty == Ty::Bool && y.ty == Ty::Bool => {
                let (xb, yb) = (self.b.create_block(), self.b.create_block());
                self.branch(k, xb, yb);
                self.b.switch_to_block(xb);
                self.branch(x, t, f);
                self.b.switch_to_block(yb);
                self.branch(y, t, f);
            }
            _ => {
                let v = self.expr(c).boolean();
                self.b.ins().brif(v, t, &[], f, &[]);
            }
        }
    }

    fn stmts(&mut self, list: &[Stmt]) {
        for s in list {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Expr(e) => self.effect(e),
            Stmt::NewBoxes(ls) => self.new_boxes(ls, false),
            Stmt::Await { value, target } => self.await_stmt(value, *target),
            Stmt::Yield { value, kind, target } => self.yield_stmt(value, *kind, *target),
            Stmt::Block(b) => self.stmts(b),
            Stmt::If(
                Expr {
                    kind:
                        ExprKind::SpecGuard {
                            objs,
                            distinct,
                            fields,
                            nums,
                            strs,
                            char_code_at,
                            derived,
                            methods,
                            cells,
                            arrays,
                            protos,
                            appends,
                            typed_writes,
                        },
                    ..
                },
                fast,
                slow,
            ) => {
                let guard = SpecGuard {
                    objs,
                    distinct: *distinct,
                    fields,
                    nums,
                    strs,
                    char_code_at: *char_code_at,
                    derived,
                    methods,
                    cells,
                    arrays,
                    protos,
                    appends,
                    typed_writes,
                };
                self.spec_region(&guard, fast, slow)
            }
            Stmt::If(
                Expr { kind: ExprKind::ElemGuard { elems, nums, bounds, derived, fields, distinct }, .. },
                then,
                bail,
            ) => self.elem_guard(elems, nums, bounds, derived, fields, distinct, then, bail),
            Stmt::If(c, then, els) => {
                let tb = self.b.create_block();
                let eb = self.b.create_block();
                let join = self.b.create_block();
                self.branch(c, tb, eb);
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
            Stmt::Loop { id, test, post_test, body, update, per_iteration } => {
                // Each iteration gets its own bindings for captured loop variables.
                self.new_boxes(per_iteration, true);
                let header = self.b.create_block();
                let body_b = self.b.create_block();
                let cont = self.b.create_block();
                let exit = self.b.create_block();
                self.loops.insert(*id, (Some(cont), exit));
                self.b.ins().jump(header, &[]);
                let bounds = self.loop_lo.get(id).cloned().unwrap_or_default();
                let outer_lo = self.active_lo.len();
                self.active_lo.extend(bounds);

                self.b.switch_to_block(header);
                match test {
                    Some(t) if !post_test => self.branch(t, body_b, exit),
                    _ => {
                        self.b.ins().jump(body_b, &[]);
                    }
                }

                self.b.switch_to_block(body_b);
                self.scopes.push(Scope::Jump(*id));
                self.stmts(body);
                self.scopes.pop();
                if !self.is_filled() {
                    self.b.ins().jump(cont, &[]);
                }

                self.b.switch_to_block(cont);
                // What the iteration declared for itself is released now,
                // not when the next iteration declares it again (two
                // iterations' data would be alive at once).
                let fresh = ir::iteration_locals(body, &self.local_uses, |l| {
                    l >= self.n_params
                        && self.ret_slot != Some(l)
                        && is_js(self.locals[l])
                        && matches!(self.vars[l], LVar::Val(..))
                });
                for l in fresh {
                    // (Not read: freeing is a no-op; undefined lets the old
                    // value go, also from the native stack.)
                    let undef = self.konst(0, TAG_UNDEFINED);
                    self.def_local(l, undef);
                }
                let counter = self.bounded_counter(test.as_ref().filter(|_| !post_test), body, update.as_ref());
                self.bounded.extend(counter);
                self.new_boxes(per_iteration, true);
                if let Some(u) = update {
                    self.effect(u);
                }
                if counter.is_some() {
                    self.bounded.pop();
                }
                match test {
                    Some(t) if *post_test => {
                        self.branch(t, header, exit);
                    }
                    // Rotated: a copy of a cheap test at the bottom saves a
                    // jump per iteration.
                    Some(t) if is_cheap(t) => {
                        let v = self.expr(t).boolean();
                        self.b.ins().brif(v, body_b, &[], exit, &[]);
                    }
                    _ => {
                        self.b.ins().jump(header, &[]);
                    }
                }
                self.active_lo.truncate(outer_lo);
                self.b.switch_to_block(exit);
            }
            Stmt::Switch { id, disc, cases } => self.switch(*id, *disc, cases),
            Stmt::Break(id) => {
                self.leave_to(*id, false);
                let (_, exit) = self.loops[id];
                self.b.ins().jump(exit, &[]);
                self.after_terminator();
            }
            Stmt::Continue(id) => {
                self.leave_to(*id, true);
                let (cont, _) = self.loops[id];
                self.b.ins().jump(cont.expect("continue targets a loop"), &[]);
                self.after_terminator();
            }
            Stmt::Return(e) => {
                if let Some(&(join, ty)) = self.inline_rets.last() {
                    // In an inlined call: its value.
                    let v = match e {
                        Some(e) => self.expr(e),
                        None => V::Void,
                    };
                    let args: Vec<BlockArg> = if ty == Ty::Void {
                        self.free_v(v);
                        Vec::new()
                    } else {
                        self.abi(ty, v).into_iter().map(BlockArg::Value).collect()
                    };
                    self.b.ins().jump(join, &args);
                    self.after_terminator();
                    return;
                }
                let v = match e {
                    Some(e) => self.expr(e),
                    None => V::Void,
                };
                let v = if self.ret == Ty::Void {
                    self.free_v(v);
                    V::Void
                } else {
                    v
                };
                self.return_value(v);
            }
            Stmt::Throw(e) => {
                let w = self.expr(e).any();
                let ctx = self.ctx;
                self.helper("JS_Throw", &[ctx, w]);
                let edge = self.throw_edge();
                self.b.ins().jump(edge, &[]);
                self.after_terminator();
            }
            Stmt::Try { body, catch, finally, exc } => self.try_stmt(body, catch.as_ref(), finally.as_ref(), *exc),
            Stmt::IterGuard { iter, next, loop_id, body } => self.iter_guard(*iter, *next, *loop_id, body),
        }
    }

    /// Return `v` (owned), running the `finally` blocks on the way out.
    fn return_value(&mut self, v: V) {
        let v = if self.scopes.iter().any(|s| !matches!(s, Scope::Jump(_))) {
            match (self.ret_slot, v) {
                (_, V::Void) => {
                    self.run_finally_until(0);
                    V::Void
                }
                (None, v) => {
                    self.hold(v);
                    self.run_finally_until(0);
                    self.release(v);
                    v
                }
                (Some(slot), v) => {
                    // The finalizers may suspend: the value waits in a local.
                    let w = self.boxed(v);
                    self.set_local(slot, V::Any(w));
                    self.run_finally_until(0);
                    let w = self.take_local(slot);
                    match self.ret {
                        Ty::F64 | Ty::I32 => V::Num(self.number_of(w)),
                        Ty::Bool => {
                            let x = self.b.ins().ireduce(I8, w);
                            V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, x, 0))
                        }
                        _ => V::Any(w),
                    }
                }
            }
        } else {
            v
        };
        let ret = self.ret;
        let args: Vec<BlockArg> = self.abi(ret, v).into_iter().map(BlockArg::Value).collect();
        let rb = self.ret_block;
        self.b.ins().jump(rb, &args);
        self.after_terminator();
    }

    /// Move the value out of a JS-valued local, leaving `undefined`.
    fn take_local(&mut self, l: ir::LocalId) -> Value {
        let v = self.use_local(l).any();
        let u = self.konst(0, TAG_UNDEFINED);
        self.def_local(l, u);
        v
    }

    /// Run the `finally` blocks between here and jump target `id`.
    fn leave_to(&mut self, id: ir::LoopId, is_continue: bool) {
        let depth = self.scopes.iter().rposition(|s| matches!(s, Scope::Jump(t) if *t == id)).expect("jump target");
        self.unwind(depth + 1, is_continue.then_some(id));
    }

    /// Run every `finally` in `scopes[depth..]`, innermost first, each with
    /// the handlers and scopes that surround its `try`.
    fn run_finally_until(&mut self, depth: usize) {
        self.unwind(depth, None);
    }

    /// Leave `scopes[depth..]`, innermost first: run the `finally` blocks
    /// and close the iterators (except for a `continue` of loop
    /// `continued`), each with the handlers and scopes around it.
    fn unwind(&mut self, depth: usize, continued: Option<ir::LoopId>) {
        let mut i = self.scopes.len();
        while i > depth {
            i -= 1;
            let (handlers, scopes) = match &self.scopes[i] {
                Scope::Jump(_) => continue,
                Scope::CloseIter { loop_id, .. } if continued.is_some() && *loop_id == continued => continue,
                Scope::Finally { handlers, scopes, .. } | Scope::CloseIter { handlers, scopes, .. } => {
                    (*handlers, *scopes)
                }
            };
            let saved_handlers = self.handlers.split_off(handlers);
            let saved_scopes = self.scopes.split_off(scopes);
            match &saved_scopes[i - scopes] {
                Scope::Finally { body, .. } => {
                    let body = body.clone();
                    self.stmts(&body);
                }
                Scope::CloseIter { iter, next, .. } => self.close_iter(*iter, *next),
                Scope::Jump(_) => unreachable!(),
            }
            self.handlers.extend(saved_handlers);
            self.scopes.extend(saved_scopes);
        }
    }

    fn try_stmt(
        &mut self,
        body: &[Stmt],
        catch: Option<&(Option<ir::LocalId>, Vec<Stmt>)>,
        finally: Option<&Vec<Stmt>>,
        exc_slot: Option<ir::LocalId>,
    ) {
        let join = self.b.create_block();
        let fin_handler = finally.map(|_| {
            let blk = self.b.create_block();
            self.b.set_cold_block(blk);
            blk
        });
        let catch_blk = catch.map(|_| self.b.create_block());
        let outer_handlers = self.handlers.len();
        let outer_scopes = self.scopes.len();
        if let Some(f) = finally {
            self.scopes.push(Scope::Finally { body: f.clone(), handlers: outer_handlers, scopes: outer_scopes });
        }
        if let Some(fh) = fin_handler {
            self.handlers.push(fh);
        }
        if let Some(cb) = catch_blk {
            self.handlers.push(cb);
        }

        // try { body }
        self.stmts(body);
        if catch_blk.is_some() {
            self.handlers.pop();
        }
        if !self.is_filled() {
            self.finally_then_jump(finally, join);
        }

        // catch (e) { handler }
        if let (Some(cb), Some((param, handler))) = (catch_blk, catch) {
            self.b.switch_to_block(cb);
            self.clear_flag();
            let ctx = self.ctx;
            let exc = self.helper("JS_GetException", &[ctx]);
            match param {
                Some(l) => self.set_local(*l, V::Any(exc[0])),
                None => self.free(exc[0]),
            }
            self.stmts(handler);
            if !self.is_filled() {
                self.finally_then_jump(finally, join);
            }
        }
        if fin_handler.is_some() {
            self.handlers.pop();
        }
        if finally.is_some() {
            self.scopes.pop();
        }

        // finally on the exception path: save the exception, run the
        // finalizer, rethrow.
        if let (Some(fh), Some(f)) = (fin_handler, finally) {
            self.b.switch_to_block(fh);
            self.clear_flag();
            let ctx = self.ctx;
            let exc = self.helper("JS_GetException", &[ctx]);
            let exc = V::Any(exc[0]);
            let w = match exc_slot {
                // The finalizer may suspend: the exception waits in a local.
                Some(slot) => {
                    self.set_local(slot, exc);
                    self.stmts(f);
                    self.take_local(slot)
                }
                None => {
                    self.hold(exc);
                    self.stmts(f);
                    self.release(exc);
                    exc.any()
                }
            };
            self.helper("JS_Throw", &[ctx, w]);
            let edge = self.throw_edge();
            self.b.ins().jump(edge, &[]);
        }
        self.b.switch_to_block(join);
    }

    fn finally_then_jump(&mut self, finally: Option<&Vec<Stmt>>, join: Block) {
        if let Some(f) = finally {
            // The finalizer runs with the handlers outside the `try`.
            let n = self.scopes.iter().rposition(|s| matches!(s, Scope::Finally { .. })).unwrap();
            let Scope::Finally { handlers, scopes, .. } = self.scopes[n] else { unreachable!() };
            let saved_handlers = self.handlers.split_off(handlers);
            let saved_scopes = self.scopes.split_off(scopes);
            self.stmts(f);
            self.handlers.extend(saved_handlers);
            self.scopes.extend(saved_scopes);
        }
        if !self.is_filled() {
            self.b.ins().jump(join, &[]);
        }
    }

    fn switch(&mut self, id: ir::LoopId, disc: ir::LocalId, cases: &[(Option<Expr>, Vec<Stmt>)]) {
        let exit = self.b.create_block();
        let bodies: Vec<Block> = cases.iter().map(|_| self.b.create_block()).collect();
        self.loops.insert(id, (None, exit));
        // Tests, in order; then the default case (or the exit).
        for (i, (test, _)) in cases.iter().enumerate() {
            let Some(test) = test else { continue };
            let eq = if let ExprKind::Str(id) = test.kind {
                let d = self.use_local(disc);
                self.eq_string_const(d.any(), id)
            } else if let (V::Num(d), ExprKind::Num(_)) = (self.use_local(disc), &test.kind) {
                // A numeric switch (see `check`): numbers compared.
                let k = self.expr(test).num();
                self.b.ins().fcmp(FloatCC::Equal, d, k)
            } else {
                let v = self.expr(test);
                self.hold(v);
                let d = self.use_local(disc);
                let eq = self.strict_eq(d.any(), v.any());
                self.release(v);
                self.free_v(v);
                eq
            };
            let next = self.b.create_block();
            self.b.ins().brif(eq, bodies[i], &[], next, &[]);
            self.b.switch_to_block(next);
        }
        let default = cases.iter().position(|(t, _)| t.is_none()).map_or(exit, |i| bodies[i]);
        self.b.ins().jump(default, &[]);
        self.scopes.push(Scope::Jump(id));
        for (i, (_, body)) in cases.iter().enumerate() {
            self.b.switch_to_block(bodies[i]);
            self.stmts(body);
            if !self.is_filled() {
                let next = bodies.get(i + 1).copied().unwrap_or(exit);
                self.b.ins().jump(next, &[]);
            }
        }
        self.scopes.pop();
        self.b.switch_to_block(exit);
    }

    /// Whether `w` (borrowed) is string constant `id`: by reference, then
    /// length, before comparing characters.
    fn eq_string_const(&mut self, w: Value, id: ir::StrId) -> Value {
        let base = self.b.ins().symbol_value(self.ptr, self.env.strings);
        let kw = self.b.ins().load(I64, M_CONST, base, (id * SIZE) as i32);
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I8);
        let yes = self.b.ins().iconst(I8, 1);
        let no = self.b.ins().iconst(I8, 0);
        let string = self.b.create_block();
        let other_tag = self.b.create_block();
        let slow = self.b.create_block();
        let is_str = self.val_is(w, TAG_STRING);
        self.b.ins().brif(is_str, string, &[], other_tag, &[]);
        // Ropes are compared by the engine; other values differ.
        self.b.switch_to_block(other_tag);
        let rope = self.val_is(w, TAG_STRING_ROPE);
        self.b.ins().brif(rope, slow, &[], join, &[BlockArg::Value(no)]);
        self.b.switch_to_block(string);
        let same = self.b.ins().icmp(IntCC::Equal, w, kw);
        let p = self.val_ptr(w);
        let kp = self.val_ptr(kw);
        let lens = self.b.create_block();
        self.b.ins().brif(same, join, &[BlockArg::Value(yes)], lens, &[]);
        self.b.switch_to_block(lens);
        let la = self.b.ins().uload32(M_HEADER, p, layout::STRING_LEN);
        let la = self.b.ins().band_imm_u(la, 0x7fff_ffff);
        let lb = self.b.ins().uload32(M_CONST, kp, layout::STRING_LEN);
        let lb = self.b.ins().band_imm_u(lb, 0x7fff_ffff);
        let same_len = self.b.ins().icmp(IntCC::Equal, la, lb);
        // A short 8-bit constant: compared with an 8-bit string's
        // characters inline (a few loads, as immediates).
        let text: Vec<u32> = self.env.texts[id].chars().map(|c| c as u32).collect();
        let narrow = !text.is_empty() && text.len() <= 16 && text.iter().all(|&c| c <= 0xff);
        let chars = if narrow { self.b.create_block() } else { slow };
        self.b.ins().brif(same_len, chars, &[], join, &[BlockArg::Value(no)]);
        if narrow {
            self.b.switch_to_block(chars);
            let word = self.b.ins().uload32(M_HEADER, p, layout::STRING_LEN);
            let wide = self.b.ins().ushr_imm_u(word, 31);
            let kind = self.b.ins().uload32(M_HEADER, p, layout::STRING_KIND);
            let kind = self.b.ins().ushr_imm_u(kind, 28);
            let kind = self.b.ins().band_imm_u(kind, 3);
            let other = self.b.ins().bor(wide, kind);
            let flat = self.b.ins().icmp_imm_s(IntCC::Equal, other, 0);
            let cmp = self.b.create_block();
            self.b.ins().brif(flat, cmp, &[], slow, &[]);
            self.b.switch_to_block(cmp);
            let bytes: Vec<u8> = text.iter().map(|&c| c as u8).collect();
            let n = bytes.len();
            // (Loads within the characters: the last of a run of 3, 5, 6 or
            // 7 overlaps the one before.)
            let mut parts: Vec<(usize, usize)> = Vec::new();
            let mut off = 0;
            while off < n {
                let c = (n - off).min(8);
                match c {
                    1 | 2 | 4 | 8 => parts.push((off, c)),
                    3 => parts.extend([(off, 2), (off + 1, 2)]),
                    _ => parts.extend([(off, 4), (off + c - 4, 4)]),
                }
                off += c;
            }
            let mut diff = self.b.ins().iconst(I64, 0);
            for (o, w) in parts {
                let at = layout::STRING_DATA + o as i32;
                let v = match w {
                    1 => self.b.ins().uload8(I64, M_HEADER, p, at),
                    2 => self.b.ins().uload16(I64, M_HEADER, p, at),
                    4 => self.b.ins().uload32(M_HEADER, p, at),
                    _ => self.b.ins().load(I64, M_HEADER, p, at),
                };
                let mut want = [0u8; 8];
                want[..w].copy_from_slice(&bytes[o..o + w]);
                let x = self.b.ins().bxor_imm_u(v, i64::from_le_bytes(want));
                diff = self.b.ins().bor(diff, x);
            }
            let eq = self.b.ins().icmp_imm_s(IntCC::Equal, diff, 0);
            self.b.ins().jump(join, &[BlockArg::Value(eq)]);
        }
        self.b.switch_to_block(slow);
        let ctx = self.ctx;
        let r = self.helper("JS_IsStrictEqual", &[ctx, w, kw]);
        let r = self.b.ins().ireduce(I8, r[0]);
        let r = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    /// `a === b` for borrowed values.
    fn strict_eq(&mut self, a: Value, b: Value) -> Value {
        self.compare_any(AnyCmpOp::StrictEq, a, b)
    }

    // ---- expressions ----

    /// Evaluate for side effects only.
    fn effect(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Box(x) => self.effect(x),
            ExprKind::SetProp(o, a, x) if self.prop_step(o, *a, x).is_some() => {
                let step = self.prop_step(o, *a, x).unwrap();
                self.prop_update(o, *a, step, false);
            }
            ExprKind::SetLocal(l, _) | ExprKind::InitLocal(l, _) if matches!(self.vars[*l], LVar::Int(_)) => {
                self.expr(e);
            }
            ExprKind::SetLocal(l, x) => {
                let v = self.expr(x);
                self.set_local(*l, v);
            }
            ExprKind::InitLocal(l, x) => {
                let v = self.expr(x);
                self.init_local(*l, v);
            }
            _ => {
                let v = self.expr(e);
                self.free_v(v);
            }
        }
    }

    /// Store an owned value in a local, freeing its previous value. Boxed
    /// locals are checked for their TDZ first.
    fn set_local(&mut self, l: ir::LocalId, v: V) {
        if let LVar::Boxed { .. } = self.vars[l] {
            let old = self.load_box(l);
            self.hold(v);
            self.check_tdz(l, old);
            self.release(v);
            self.store_box(l, v.any());
            self.free(old);
        } else if is_js(self.locals[l]) {
            let old = self.use_local(l).any();
            self.def_local(l, v);
            self.free(old);
        } else {
            self.def_local(l, v);
        }
    }

    /// A local's initialization by its declaration (no TDZ check).
    fn init_local(&mut self, l: ir::LocalId, v: V) {
        if let LVar::Boxed { .. } = self.vars[l] {
            let old = self.load_box(l);
            self.store_box(l, v.any());
            self.free(old);
        } else {
            self.set_local(l, v);
        }
    }

    /// An `f64` that holds an integer as that `i64` (exact by construction,
    /// else converted).
    fn int_of_num(&mut self, d: Value) -> Value {
        match self.int_of.get(&d) {
            Some(&i) => i,
            None => self.b.ins().fcvt_to_sint_sat(I64, d),
        }
    }

    /// Values of type `ty` as they cross a block, call or return: `I32`
    /// ones as `i64`s.
    fn abi(&mut self, ty: Ty, v: V) -> Vec<Value> {
        match (ty, v) {
            (Ty::I32, V::Num(d)) => vec![self.int_of_num(d)],
            _ => v.values(),
        }
    }

    /// The inverse of `abi`.
    fn unabi(&mut self, ty: Ty, vs: &[Value]) -> V {
        match from_values(ty, vs) {
            V::Int(i) => {
                let d = self.int_f64(i);
                self.i32s.insert(d);
                V::Num(d)
            }
            v => v,
        }
    }

    /// Integers to compare `a` and `c` by, when that agrees with comparing
    /// the doubles: both exact, or an int32 against a rounded product of
    /// int32s (a rounded product is past 2^53, far from any int32). `%`
    /// results may be -0 for 0, which compares the same.
    /// Also the flags under which they are valid (else a `%` result is NaN,
    /// or a double: see `RemInt`).
    fn int_pair(&self, a: Value, c: Value) -> Option<IntPair> {
        let exact = |v: &Value| match self.rem_of.get(v) {
            Some(r) => Some((r.int, Some((r.ok, r.nan)))),
            None => self.int_of.get(v).map(|&i| (i, None)),
        };
        let small = |v: &Value| self.i32s.contains(v) || self.rem_of.get(v).is_some_and(|r| r.small);
        let (x, y) = match (exact(&a), exact(&c)) {
            (Some(x), Some(y)) => (x, y),
            (None, Some(y)) if small(&c) => ((*self.wide_of.get(&a)?, None), y),
            (Some(x), None) if small(&a) => (x, (*self.wide_of.get(&c)?, None)),
            _ => return None,
        };
        Some((x.0, y.0, x.1.into_iter().chain(y.1).collect()))
    }

    /// `a % c` for int32s `a` (`x`) and `c` (`y`): the integer remainder,
    /// with the dividend's sign (`-4 % 2` is -0); NaN for `% 0`.
    fn int_rem(&mut self, x: Value, y: Value, a: Value, c: Value) -> Value {
        // 32-bit division is much faster than 64-bit (the IR defines
        // INT32_MIN % -1 as 0).
        if self.lo_of(c).is_some_and(|lo| lo >= 1) {
            // Never `% 0`.
            let (x32, y32) = (self.b.ins().ireduce(I32, x), self.b.ins().ireduce(I32, y));
            let r = self.b.ins().srem_positive(x32, y32);
            let r = self.b.ins().sextend(I64, r);
            return self.exact_rem(r, a, 32);
        }
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, F64);
        let r_param = self.b.append_block_param(join, I64);
        let nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, y, 0);
        self.b.ins().brif(nonzero, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let (x32, y32) = (self.b.ins().ireduce(I32, x), self.b.ins().ireduce(I32, y));
        let r = self.b.ins().srem(x32, y32);
        let r = self.b.ins().sextend(I64, r);
        let rf = self.b.ins().fcvt_from_sint(F64, r);
        let rf = self.b.ins().fcopysign(rf, a);
        self.b.ins().jump(join, &[BlockArg::Value(rf), BlockArg::Value(r)]);
        self.b.switch_to_block(slow);
        let nan = self.b.ins().f64const(f64::NAN);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(join, &[BlockArg::Value(nan), BlockArg::Value(zero)]);
        self.b.switch_to_block(join);
        self.rem_of.insert(res, RemInt { int: r_param, ok: nonzero, nan: true, small: true, none: None });
        res
    }

    /// The remainder `r` (an `i64`, exact) of dividend `a` by a divisor
    /// below 2^`bits` in magnitude, as a number: with the dividend's sign
    /// (`-4 % 2` is -0), or an exact integer itself when the dividend is not
    /// negative.
    fn exact_rem(&mut self, r: Value, a: Value, bits: u32) -> Value {
        if self.nonneg.contains(&a) {
            let d = self.int_f64(r);
            self.int_bits.insert(d, bits);
            self.nonneg.insert(d);
            if bits <= 31 {
                self.i32s.insert(d);
            }
            return d;
        }
        let rf = self.b.ins().fcvt_from_sint(F64, r);
        let rf = self.b.ins().fcopysign(rf, a);
        let ok = self.b.ins().iconst(I8, 1);
        self.rem_of.insert(rf, RemInt { int: r, ok, nan: true, small: bits <= 53, none: None });
        rf
    }

    /// A lower bound of `v`, an exact integer: its value for a constant, or
    /// a loop counter's (see `loop_bounds`).
    fn lo_of(&self, v: Value) -> Option<i64> {
        self.int_consts.get(&v).or(self.int_lo.get(&v)).copied()
    }

    /// An int local's value as an `f64` (exact: see `int_locals`).
    fn int_f64(&mut self, i: Value) -> Value {
        let d = self.b.ins().fcvt_from_sint(F64, i);
        self.int_of.insert(d, i);
        d
    }

    /// `x + c` for an int local `x`, rounded as `f64` arithmetic would past
    /// 2^53.
    fn int_add(&mut self, x: Value, c: i64) -> Value {
        let r = self.b.ins().iadd_imm_s(x, c);
        if c == 0 {
            return r;
        }
        // |r| > 2^53, either way: `x` may already be past 2^53 in the
        // other direction (`x += 4; x -= 1`).
        let shifted = self.b.ins().iadd_imm_s(r, INT_EXACT);
        let over = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, shifted, 2 * INT_EXACT);
        let round = self.b.create_block();
        self.b.set_cold_block(round);
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I64);
        self.b.ins().brif(over, round, &[], join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(round);
        let d = self.b.ins().fcvt_from_sint(F64, r);
        let i = self.b.ins().fcvt_to_sint_sat(I64, d);
        self.b.ins().jump(join, &[BlockArg::Value(i)]);
        self.b.switch_to_block(join);
        res
    }

    /// For a loop `for (...; i < K; i++)` (or `<=`, `--` with `>` / `>=`)
    /// over an int local `i` that the body does not assign, with `K` an
    /// integer literal: `i`. Its update then stays within K +/- 256, far
    /// from 2^53.
    fn bounded_counter(&self, test: Option<&Expr>, body: &[Stmt], update: Option<&Expr>) -> Option<ir::LocalId> {
        let ExprKind::Compare(op, x, y) = &test?.kind else { return None };
        let ExprKind::Local(l) = x.kind else { return None };
        let ExprKind::Update { local, delta, .. } = update?.kind else { return None };
        // (A bound that is a constant, or always an int32: a string's
        // length, an int32 local.)
        let small_bound = match y.kind {
            ExprKind::StrLen { .. } => true,
            ExprKind::Local(m) => self.i32_range[m],
            _ => false,
        };
        if !small_bound {
            int_literal(y, (INT_EXACT >> 1) as f64)?;
        }
        let up = matches!(op, CmpOp::Lt | CmpOp::Le) && delta > 0.0;
        let down = matches!(op, CmpOp::Gt | CmpOp::Ge) && delta < 0.0;
        if local != l || !(up || down) || !matches!(self.vars[l], LVar::Int(_)) {
            return None;
        }
        let mut assigned = false;
        ir::visit_stmts(body, &mut |e| assigned |= ir::assigns_local(e, l));
        (!assigned).then_some(l)
    }

    /// The new value of an int local assigned `x` (a form `int_locals`
    /// accepts).
    fn int_value(&mut self, x: &Expr) -> Value {
        if let Some(c) = int_literal(x, INT_EXACT as f64) {
            return self.b.ins().iconst(I64, c);
        }
        // Itself or another int local plus or minus a small integer.
        if let Some((m, c)) = int_offset(x)
            && let LVar::Int(var) = self.vars[m]
        {
            let cur = self.b.use_var(var);
            if self.small[m] || self.locals[m] == Ty::I32 {
                return self.b.ins().iadd_imm_s(cur, c);
            }
            return self.int_add(cur, c);
        }
        if let ExprKind::Local(m) = x.kind
            && let LVar::Int(var) = self.vars[m]
        {
            return self.b.use_var(var);
        }
        // An int by the checker (an `int` local's value) or by `int_locals`.
        let d = self.expr(x).num();
        self.int_of_num(d)
    }

    pub(crate) fn expr(&mut self, e: &Expr) -> V {
        match &e.kind {
            ExprKind::Num(v) => {
                let d = self.b.ins().f64const(*v);
                if let Some(c) = int_literal(e, INT_EXACT as f64) {
                    let i = self.b.ins().iconst(I64, c);
                    self.int_of.insert(d, i);
                    self.int_consts.insert(d, c);
                    self.int_bits.insert(d, 64 - c.unsigned_abs().leading_zeros());
                    if i32::try_from(c).is_ok() {
                        self.i32s.insert(d);
                    }
                    if c >= 0 {
                        self.nonneg.insert(d);
                    }
                }
                V::Num(d)
            }
            ExprKind::Bool(v) => V::Bool(self.b.ins().iconst(I8, *v as i64)),
            ExprKind::Undefined => self.konst(0, TAG_UNDEFINED),
            ExprKind::Null => self.konst(0, TAG_NULL),
            ExprKind::Local(l) => {
                let v = self.use_local(*l);
                if let V::Any(w) = v {
                    self.dup(w);
                }
                v
            }
            ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) if matches!(self.vars[*l], LVar::Int(_)) => {
                let LVar::Int(var) = self.vars[*l] else { unreachable!() };
                let i = self.int_value(x);
                self.b.def_var(var, i);
                V::Num(self.int_f64(i))
            }
            ExprKind::SetLocal(l, x) => {
                let v = self.expr(x);
                if let V::Any(w) = v {
                    // One reference for the local, one for the expression.
                    self.dup(w);
                }
                self.set_local(*l, v);
                v
            }
            ExprKind::This => {
                let w = self.this.expect("`this` in a method");
                self.dup(w);
                V::Any(w)
            }
            ExprKind::InitLocal(l, x) => {
                let v = self.expr(x);
                if let V::Any(w) = v {
                    self.dup(w);
                }
                self.init_local(*l, v);
                v
            }
            ExprKind::Closure(cid, boxes) => {
                // Data slots: per captured variable, its box object and the
                // address of its value (borrowed: the function dups them).
                // A value capture: the value itself (the closure points
                // the second slot at it).
                let by_value = self.env.value_captures[*cid].clone();
                let mut vals = Vec::new();
                for (&bl, &value) in boxes.iter().zip(&by_value) {
                    if value {
                        let v = match self.vars[bl] {
                            LVar::Boxed { .. } => self.load_box(bl),
                            _ => self.use_local(bl).any(),
                        };
                        let zero = self.b.ins().iconst(I64, 0);
                        vals.push(v);
                        vals.push(zero);
                        continue;
                    }
                    let LVar::Boxed { obj, .. } = self.vars[bl] else { unreachable!() };
                    let obj = self.b.use_var(obj);
                    // (A raw address: below the cage's top, an int to the
                    // collector.)
                    let slot = self.box_slot(bl);
                    vals.push(obj);
                    vals.push(slot);
                }
                // Inline once the function's first closure is made (see
                // `alloc_closure`), else made by the runtime.
                let fn_index = self.env.closure_base + cid;
                let call = self.b.create_block();
                self.b.set_cold_block(call);
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let p = self.alloc_closure(fn_index, &vals, call);
                let w = self.mk_ref(TAG_OBJECT, p);
                self.b.ins().jump(join, &[BlockArg::Value(w)]);
                self.b.switch_to_block(call);
                let data = self.values_slot(&vals);
                let index = self.b.ins().iconst(I32, fn_index as i64);
                let n = self.b.ins().iconst(I32, vals.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("lum_make_closure", &[ctx, index, n, data]);
                self.check_value(r[0]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                V::Any(rw)
            }
            ExprKind::Update { local, delta, prefix } => {
                if let LVar::Int(var) = self.vars[*local] {
                    let old = self.b.use_var(var);
                    let new = if self.small[*local] || self.bounded.contains(local) || self.locals[*local] == Ty::I32 {
                        self.b.ins().iadd_imm_s(old, *delta as i64)
                    } else {
                        self.int_add(old, *delta as i64)
                    };
                    self.b.def_var(var, new);
                    return V::Num(self.int_f64(if *prefix { new } else { old }));
                }
                let old = self.use_local(*local).num();
                let d = self.b.ins().f64const(*delta);
                let new = self.b.ins().fadd(old, d);
                self.def_local(*local, V::Num(new));
                V::Num(if *prefix { new } else { old })
            }
            ExprKind::Unary(op, x) => {
                let v = self.expr(x);
                match op {
                    UnOp::Neg => V::Num(self.b.ins().fneg(v.num())),
                    UnOp::Not => V::Bool(self.b.ins().bxor_imm_u(v.boolean(), 1)),
                    UnOp::BitNot => {
                        let i = self.toint32(v.num());
                        let r = self.b.ins().bnot(i);
                        let r = self.b.ins().sextend(I64, r);
                        let d = self.int_f64(r);
                        self.i32s.insert(d);
                        V::Num(d)
                    }
                }
            }
            ExprKind::Truthy(x) => {
                let v = self.expr(x);
                V::Bool(match v {
                    V::Bool(b) => b,
                    V::Num(d) => self.truthy_num(d),
                    V::Any(w) => {
                        let r = self.truthy_any(w);
                        self.free(w);
                        r
                    }
                    V::Void => self.b.ins().iconst(I8, 0),
                    V::Int(_) => unreachable!("an ABI value"),
                })
            }
            ExprKind::Binary(op, x, y) => {
                let a = self.expr(x).num();
                let c = self.expr(y).num();
                V::Num(self.binary(*op, a, c))
            }
            ExprKind::Inline { body, locals } => self.inline_body(body, locals, e.ty),
            ExprKind::StructNew { layout, mask, fields } => {
                // The values first (in order), then the object, which takes them.
                let held: Vec<V> = fields
                    .iter()
                    .map(|(_, x)| {
                        let v = self.expr(x);
                        self.hold(v);
                        v
                    })
                    .collect();
                let ctx = self.ctx;
                let k = self.b.ins().iconst(I32, *layout as i64);
                let m = self.b.ins().iconst(I32, *mask as i64);
                let r = self.helper("lum_struct_new", &[ctx, k, m]);
                self.check_value(r[0]);
                let obj = V::Any(r[0]);
                self.hold(obj);
                for (&(fi, _), &v) in fields.iter().zip(&held) {
                    self.release(v);
                    self.struct_store(*layout, fi, r[0], v, true);
                }
                // Which member it matches may depend on the values.
                let l = &self.env.layouts[*layout as usize];
                let check = l.members.len() > 1 && fields.iter().any(|&(fi, _)| !l.fields[fi as usize].simple);
                if check {
                    let ok = self.helper("lum_struct_validate", &[ctx, r[0]]);
                    self.check_int(ok[0]);
                }
                self.release(obj);
                obj
            }
            ExprKind::StructArrFrom(k, x) => {
                let w = self.expr(x).any();
                let ctx = self.ctx;
                let kv = self.b.ins().iconst(I32, *k as i64);
                let r = self.helper("lum_sarr_from", &[ctx, kv, w]);
                self.check_value(r[0]);
                V::Any(r[0])
            }
            ExprKind::StructArrLen(a) => {
                let (obj, owned) = self.operand(a, &[]);
                let p = self.val_ptr(obj.any());
                let s = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_OPAQUE);
                let len = self.b.ins().uload32(M_STRUCT, s, 4);
                let d = self.int_f64(len);
                self.int_bits.insert(d, 32);
                self.drop_operand(obj, owned);
                V::Num(d)
            }
            ExprKind::StructArrGet(_, a, i) => {
                let (obj, owned) = self.operand(a, &[i]);
                let p = self.val_ptr(obj.any());
                let key = self.expr(i);
                let (ok, idx) = self.elem_index(key);
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let s = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_OPAQUE);
                let len = self.b.ins().uload32(M_STRUCT, s, 4);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, len);
                let inb = self.b.ins().band(inb, ok);
                let hit = self.b.create_block();
                let undef = self.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0));
                self.b.ins().brif(inb, hit, &[], join, &[BlockArg::Value(undef)]);
                self.b.switch_to_block(hit);
                let items = self.b.ins().load(I64, M_STRUCT, s, 16);
                let off = self.b.ins().imul_imm_s(idx, SIZE as i64);
                let at = self.b.ins().iadd(items, off);
                let w = self.b.ins().load(I64, M_SLOT, at, 0);
                self.dup(w);
                self.b.ins().jump(join, &[BlockArg::Value(w)]);
                self.b.switch_to_block(join);
                self.drop_operand(obj, owned);
                V::Any(rw)
            }
            ExprKind::StructArrSet(k, a, i, x) => {
                let (obj, owned) = self.operand(a, &[i, x]);
                let ow = obj.any();
                let op_ = self.val_ptr(ow);
                let key = self.expr(i);
                self.hold(key);
                let v = self.expr(x);
                let vw = self.boxed(v);
                let v = V::Any(vw);
                // One reference for the array, one for the expression.
                self.dup(vw);
                self.hold(v);
                let join = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                if x.ty == Ty::Struct(*k) {
                    // An object of the layout over an element: in place.
                    let (ok, idx) = self.elem_index(key);
                    let s = self.b.ins().load(I64, M_HEADER, op_, layout::OBJ_OPAQUE);
                    let len = self.b.ins().uload32(M_STRUCT, s, 4);
                    let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, len);
                    let inb = self.b.ins().band(inb, ok);
                    let hit = self.b.create_block();
                    self.b.ins().brif(inb, hit, &[], slow, &[]);
                    self.b.switch_to_block(hit);
                    let items = self.b.ins().load(I64, M_STRUCT, s, 16);
                    let off = self.b.ins().imul_imm_s(idx, SIZE as i64);
                    let at = self.b.ins().iadd(items, off);
                    let old = self.b.ins().load(I64, M_SLOT, at, 0);
                    self.b.ins().store(M_SLOT, vw, at, 0);
                    self.write_barrier(op_, vw);
                    self.free(old);
                    self.b.ins().jump(join, &[]);
                } else {
                    self.b.ins().jump(slow, &[]);
                }
                // Anything else: as JS stores it (the array checks it).
                self.b.switch_to_block(slow);
                let kv = self.boxed(key);
                let slot = self.values_slot(&[ow, kv, vw]);
                let ctx = self.ctx;
                let r = self.helper("lum_set_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.release(v);
                self.release(key);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::StructArrPush(_, a, x) => {
                let (obj, owned) = self.operand(a, &[x]);
                let w = obj.any();
                let v = self.expr(x);
                let vw = self.boxed(v);
                let ctx = self.ctx;
                let r = self.helper("lum_sarr_push", &[ctx, w, vw]);
                let failed = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, r[0], 0);
                self.branch_throw(failed);
                let d = self.int_f64(r[0]);
                self.int_bits.insert(d, 32);
                self.drop_operand(obj, owned);
                V::Num(d)
            }
            ExprKind::NonNull(atom, x) => {
                let v = self.expr(x);
                self.hold(v);
                self.non_null(*atom, v.any());
                self.release(v);
                v
            }
            ExprKind::NonNullSet(atom, x) => {
                let v = self.expr(x);
                self.hold(v);
                self.non_null_set(*atom, v.any());
                self.release(v);
                v
            }
            ExprKind::StructCoerce(k, fi, x) => {
                let v = self.expr(x);
                let want = e.ty;
                let storage = self.env.layouts[*k as usize].fields[*fi as usize].storage;
                let join = self.b.create_block();
                let params: Vec<Value> =
                    clif_types(want).into_iter().map(|t| self.b.append_block_param(join, t)).collect();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                match (storage, v) {
                    (ir::Storage::Int32, V::Num(d)) if self.int_of.contains_key(&d) && self.env.int_wrapping => {
                        // An exact integer: it wraps, as `| 0`.
                        let i = self.int_of[&d];
                        let i = self.b.ins().ireduce(I32, i);
                        let i = self.b.ins().sextend(I64, i);
                        self.b.ins().jump(join, &[BlockArg::Value(i)]);
                    }
                    (ir::Storage::Int32, V::Num(d)) => {
                        let i = self.b.ins().fcvt_to_sint_sat(I32, d);
                        let back = self.b.ins().fcvt_from_sint(F64, i);
                        let same = self.b.ins().fcmp(FloatCC::Equal, back, d);
                        let bits = self.b.ins().bitcast(I64, d);
                        let negzero = self.b.ins().icmp_imm_s(IntCC::Equal, bits, i64::MIN);
                        let ok = self.b.ins().band_not(same, negzero);
                        let i = self.b.ins().sextend(I64, i);
                        self.b.ins().brif(ok, join, &[BlockArg::Value(i)], slow, &[]);
                    }
                    (ir::Storage::Int32, V::Any(w)) => {
                        let is_int = self.val_is(w, TAG_INT);
                        let i = self.val_int(w);
                        self.b.ins().brif(is_int, join, &[BlockArg::Value(i)], slow, &[]);
                    }
                    (ir::Storage::F64, V::Any(w)) => {
                        let num = self.is_number(w);
                        let ok = self.b.create_block();
                        self.b.ins().brif(num, ok, &[], slow, &[]);
                        self.b.switch_to_block(ok);
                        let d = self.number_of(w);
                        self.b.ins().jump(join, &[BlockArg::Value(d)]);
                    }
                    (ir::Storage::Bool, V::Any(w)) => {
                        let is_bool = self.val_is(w, TAG_BOOL);
                        let x = self.b.ins().ireduce(I32, w);
                        let x = self.b.ins().icmp_imm_s(IntCC::NotEqual, x, 0);
                        self.b.ins().brif(is_bool, join, &[BlockArg::Value(x)], slow, &[]);
                    }
                    _ => {
                        self.b.ins().jump(slow, &[]);
                    }
                }
                // The runtime converts it, or throws.
                self.b.switch_to_block(slow);
                let bw = self.boxed(v);
                let ctx = self.ctx;
                let kv = self.b.ins().iconst(I32, *k as i64);
                let fv = self.b.ins().iconst(I32, *fi as i64);
                let r = self.helper("lum_struct_coerce", &[ctx, kv, fv, bw]);
                self.check_value(r[0]);
                let args: Vec<BlockArg> = match storage {
                    ir::Storage::Int32 => {
                        let i = self.b.ins().ireduce(I32, r[0]);
                        vec![BlockArg::Value(self.b.ins().sextend(I64, i))]
                    }
                    ir::Storage::F64 => vec![BlockArg::Value(self.number_of(r[0]))],
                    ir::Storage::Bool => {
                        let x = self.b.ins().ireduce(I32, r[0]);
                        vec![BlockArg::Value(self.b.ins().icmp_imm_s(IntCC::NotEqual, x, 0))]
                    }
                    ir::Storage::Value => vec![BlockArg::Value(r[0])],
                };
                self.b.ins().jump(join, &args);
                self.b.switch_to_block(join);
                self.unabi(want, &params)
            }
            ExprKind::StructGet(k, o, fi) => {
                let (obj, owned) = self.operand(o, &[]);
                let p = self.val_ptr(obj.any());
                let v = self.struct_load(*k, *fi, p);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::StructSet(k, o, fi, x) => {
                let (obj, owned) = self.operand(o, &[x]);
                let v = self.expr(x);
                // One reference for the field (the store takes it), one for
                // the expression.
                if let V::Any(w) = v {
                    self.dup(w);
                }
                self.hold(v);
                self.struct_store(*k, *fi, obj.any(), v, false);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::Predicted { expr, func, body, locals } => {
                // Taken first thing by the call, getter or setter `expr`
                // lowers to (not by an update in place); a polymorphic
                // site's predictions are nested, the first one outermost.
                let mut inner = &**expr;
                while let ExprKind::Predicted { expr, .. } = &inner.kind {
                    inner = expr;
                }
                let taken = match &inner.kind {
                    ExprKind::GetProp(..) | ExprKind::CallMethod(..) | ExprKind::CallThis(..) => true,
                    ExprKind::SetProp(o, a, x) => self.prop_step(o, *a, x).is_none(),
                    _ => false,
                };
                if taken {
                    self.preds.push(Pred { func: *func, body: body.clone(), locals: locals.clone() });
                }
                self.expr(expr)
            }
            ExprKind::ArrayLoop { kind, recv, method, callback, func, body, locals } => {
                self.array_loop(*kind, recv, *method, callback, *func, body, locals)
            }
            ExprKind::ArraySort { recv, method, callback, func } => self.array_sort(recv, *method, callback, *func),
            ExprKind::ProtoMethodIs { class, method, func } => V::Bool(self.proto_method_is(*class, *method, *func)),
            ExprKind::ProtoIs { class, parent } => {
                let cw = self.table_value(self.env.homes, 2 * class + 1);
                let pw = self.table_value(self.env.homes, 2 * parent + 1);
                let join = self.b.create_block();
                let r = self.b.append_block_param(join, I8);
                let objs = self.b.create_block();
                let c_obj = self.val_is(cw, TAG_OBJECT);
                let p_obj = self.val_is(pw, TAG_OBJECT);
                let both = self.b.ins().band(c_obj, p_obj);
                let no = self.b.ins().iconst(I8, 0);
                self.b.ins().brif(both, objs, &[], join, &[BlockArg::Value(no)]);
                self.b.switch_to_block(objs);
                let cp = self.val_ptr(cw);
                let pp = self.val_ptr(pw);
                let shape = self.b.ins().load(I64, M_HEADER, cp, layout::OBJ_SHAPE);
                let proto = self.b.ins().load(I64, M_HEADER, shape, layout::SHAPE_PROTO);
                let same = self.b.ins().icmp(IntCC::Equal, proto, pp);
                self.b.ins().jump(join, &[BlockArg::Value(same)]);
                self.b.switch_to_block(join);
                V::Bool(r)
            }
            ExprKind::Lazy(x) => self.expr(x),
            ExprKind::NumArith(op, x, y) => {
                let a = self.expr(x);
                self.hold(a);
                let c = self.expr(y);
                self.hold(c);
                let an = self.number_value(a);
                let cn = self.number_value(c);
                self.release(c);
                self.release(a);
                self.free_v(c);
                self.free_v(a);
                V::Num(self.binary(*op, an, cn))
            }
            ExprKind::Compare(op, x, y) => {
                let a = self.expr(x);
                let c = self.expr(y);
                V::Bool(match (a, c) {
                    (V::Num(a), V::Num(c)) if self.int_pair(a, c).is_some() => {
                        let (ia, ic, mut valid) = self.int_pair(a, c).unwrap();
                        // (An operand whose integer is fixed where it is NaN,
                        // against a constant that compares with that integer as
                        // with NaN: false, or true for `!=`, needs no flag.)
                        let moot = |v: Value, other: Value, left: bool| {
                            let (Some(n), Some(&k)) =
                                (self.rem_of.get(&v).and_then(|r| r.none), self.int_consts.get(&other))
                            else {
                                return false;
                            };
                            let (x, y) = if left { (n, k) } else { (k, n) };
                            let at = match op {
                                CmpOp::Lt => x < y,
                                CmpOp::Le => x <= y,
                                CmpOp::Gt => x > y,
                                CmpOp::Ge => x >= y,
                                CmpOp::Eq => x == y,
                                CmpOp::Ne => x != y,
                            };
                            at == (*op == CmpOp::Ne)
                        };
                        // (`valid` has `a`'s flag first, if it has one: see
                        // `int_pair`.)
                        if self.rem_of.contains_key(&c) && moot(c, a, false) {
                            valid.pop();
                        }
                        if self.rem_of.contains_key(&a) && moot(a, c, true) {
                            valid.remove(0);
                        }
                        let cc = match op {
                            CmpOp::Lt => IntCC::SignedLessThan,
                            CmpOp::Le => IntCC::SignedLessThanOrEqual,
                            CmpOp::Gt => IntCC::SignedGreaterThan,
                            CmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
                            CmpOp::Eq => IntCC::Equal,
                            CmpOp::Ne => IntCC::NotEqual,
                        };
                        let r = self.b.ins().icmp(cc, ia, ic);
                        if valid.iter().all(|&(_, nan)| nan) {
                            // A NaN `%` result: only `!=` holds.
                            valid.into_iter().fold(r, |r, (ok, _)| {
                                if *op == CmpOp::Ne {
                                    let nan = self.b.ins().bxor_imm_u(ok, 1);
                                    self.b.ins().bor(r, nan)
                                } else {
                                    self.b.ins().band(r, ok)
                                }
                            })
                        } else {
                            // Where an integer form is not valid, the doubles
                            // compare.
                            let ok = valid.iter().map(|&(ok, _)| ok).reduce(|x, y| self.b.ins().band(x, y));
                            let f = self.b.ins().fcmp(float_cc(*op), a, c);
                            match ok {
                                Some(ok) => self.b.ins().select(ok, r, f),
                                None => r,
                            }
                        }
                    }
                    (V::Num(a), V::Num(c)) => self.b.ins().fcmp(float_cc(*op), a, c),
                    (V::Bool(a), V::Bool(c)) => {
                        let cc = if *op == CmpOp::Eq { IntCC::Equal } else { IntCC::NotEqual };
                        self.b.ins().icmp(cc, a, c)
                    }
                    _ => unreachable!("compare operand types"),
                })
            }
            ExprKind::Logical(op, x, y) => self.logical(*op, e.ty, x, y),
            ExprKind::Cond(c, x, y) => {
                let cv = self.expr(c).boolean();
                let tb = self.b.create_block();
                let eb = self.b.create_block();
                let join = self.b.create_block();
                let tys = clif_types(e.ty);
                for &t in &tys {
                    self.b.append_block_param(join, t);
                }
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                for (blk, arm) in [(tb, x), (eb, y)] {
                    self.b.switch_to_block(blk);
                    let v = self.expr(arm);
                    let args: Vec<BlockArg> = self.abi(e.ty, v).into_iter().map(BlockArg::Value).collect();
                    self.b.ins().jump(join, &args);
                }
                self.b.switch_to_block(join);
                let params = self.b.block_params(join).to_vec();
                self.unabi(e.ty, &params)
            }
            ExprKind::CallDirectThis(id, this, args) => {
                // (The receiver is borrowed.)
                let recv = self.expr(this);
                self.hold(recv);
                let mut vals = Vec::new();
                let mut held = Vec::new();
                let tys = self.env.params.clone();
                for (a, &t) in args.iter().zip(&tys[*id]) {
                    let v = self.expr(a);
                    self.hold(v);
                    held.push(v);
                    vals.extend(self.abi(t, v));
                }
                for v in held.into_iter().rev() {
                    self.release(v);
                }
                let tw = recv.any();
                let mut call_args = vec![self.ctx, tw];
                call_args.extend(vals);
                let call = self.b.ins().call(self.env.funcs[*id], &call_args);
                let results = self.b.inst_results(call).to_vec();
                if !self.env.nothrow.get(*id).copied().unwrap_or(false) {
                    self.check_flag();
                }
                self.release(recv);
                self.free_v(recv);
                self.unabi(e.ty, &results)
            }
            ExprKind::Call(id, args) => {
                let mut vals = Vec::new();
                let mut held = Vec::new();
                let tys = self.env.params.clone();
                for (a, &t) in args.iter().zip(&tys[*id]) {
                    let v = self.expr(a);
                    self.hold(v);
                    held.push(v);
                    vals.extend(self.abi(t, v));
                }
                // The callee takes ownership of its arguments.
                for v in held.into_iter().rev() {
                    self.release(v);
                }
                let mut call_args = vec![self.ctx];
                call_args.extend(vals);
                let call = self.b.ins().call(self.env.funcs[*id], &call_args);
                let results = self.b.inst_results(call).to_vec();
                if !self.env.nothrow.get(*id).copied().unwrap_or(false) {
                    self.check_flag();
                }
                self.unabi(e.ty, &results)
            }
            ExprKind::CallChecked { func, callee, args, checks } => {
                let tys = self.env.params.clone();
                let held: Vec<V> = args
                    .iter()
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                // The arguments of `aot type`s must be objects of them;
                // those of unboxed parameters, numbers or booleans.
                let sc = self.b.ins().symbol_value(self.ptr, self.env.struct_class);
                let mut direct = held.clone();
                for &(i, check) in checks {
                    let w = held[i].any();
                    let ok = match check {
                        ir::ArgCheck::Struct(k) => {
                            let is_obj = self.val_is(w, TAG_OBJECT);
                            let obj = self.b.create_block();
                            self.b.ins().brif(is_obj, obj, &[], slow, &[]);
                            self.b.switch_to_block(obj);
                            let p = self.val_ptr(w);
                            crate::struct_check(&mut self.b, p, k, sc)
                        }
                        ir::ArgCheck::Number => {
                            direct[i] = V::Num(self.number_of(w));
                            self.is_number(w)
                        }
                        ir::ArgCheck::Bool => {
                            let b = self.b.ins().ireduce(I32, w);
                            direct[i] = V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, b, 0));
                            self.val_is(w, TAG_BOOL)
                        }
                    };
                    let next = self.b.create_block();
                    self.b.ins().brif(ok, next, &[], slow, &[]);
                    self.b.switch_to_block(next);
                }
                // Direct: the callee takes the arguments.
                let mut vals = Vec::new();
                for (v, &t) in direct.iter().zip(&tys[*func]) {
                    vals.extend(self.abi(t, *v));
                }
                for &v in held.iter().rev() {
                    self.release(v);
                }
                let mut call_args = vec![self.ctx];
                call_args.extend(vals);
                let call = self.b.ins().call(self.env.funcs[*func], &call_args);
                let results = self.b.inst_results(call).to_vec();
                if !self.env.nothrow.get(*func).copied().unwrap_or(false) {
                    self.check_flag();
                }
                let ret = self.env.rets[*func];
                let r = self.unabi(ret, &results);
                let w = self.boxed(r);
                self.b.ins().jump(join, &[BlockArg::Value(w)]);
                // Else through the function object (whose entry declines).
                self.b.switch_to_block(slow);
                for &v in &held {
                    self.hold(v);
                }
                let fv = self.expr(callee);
                self.hold(fv);
                let mut vals = vec![fv.any(), self.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0))];
                for &v in &held {
                    let b = self.boxed(v);
                    vals.push(b);
                }
                let slot = self.values_slot(&vals);
                let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
                let argc = self.b.ins().iconst(I32, held.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("lum_call", &[ctx, slot, argc, argv]);
                self.check_value(r[0]);
                self.release(fv);
                self.free_v(fv);
                for &v in held.iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                V::Any(rw)
            }
            ExprKind::Math(f, args) => {
                // JS values are converted once all arguments are evaluated.
                let held: Vec<V> = args
                    .iter()
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<Value> = held.iter().map(|&v| self.number_value(v)).collect();
                for &v in held.iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                V::Num(self.math(*f, &vals)).retype(f.ret(), self)
            }
            ExprKind::Seq(xs) => {
                let (last, rest) = xs.split_last().unwrap();
                for x in rest {
                    self.effect(x);
                }
                self.expr(last)
            }
            ExprKind::Void(x) => {
                self.effect(x);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::Str(id) => {
                let base = self.b.ins().symbol_value(self.ptr, self.env.strings);
                let off = (*id * SIZE) as i32;
                let w = self.b.ins().load(I64, M_CONST, base, off);
                self.dup(w);
                V::Any(w)
            }
            ExprKind::RegExp(pattern, flags) => {
                let ctx = self.ctx;
                let p = self.b.ins().iconst(I32, *pattern as i64);
                let f = self.b.ins().iconst(I32, *flags as i64);
                let r = self.helper("lum_regexp", &[ctx, p, f]);
                self.check_value(r[0]);
                V::Any(r[0])
            }
            ExprKind::Box(x) => match self.expr(x) {
                V::Num(d) if x.ty == Ty::MaybeF64 => V::Any(self.box_maybe(d)),
                V::Num(d) => V::Any(self.box_number(d)),
                V::Bool(b) => V::Any(self.box_bool(b)),
                other => other,
            },
            ExprKind::AsyncIterator(x) => {
                let v = self.expr(x);
                self.hold(v);
                let w = v.any();
                let ctx = self.ctx;
                let is_async = self.b.ins().iconst(I32, 1);
                let r = self.helper("JS_GetIteratorObject", &[ctx, w, is_async]);
                self.check_value(r[0]);
                self.release(v);
                self.free_v(v);
                V::Any(r[0])
            }
            ExprKind::AsyncIterResult(x, done) => {
                let v = self.expr(x);
                self.hold(v);
                let w = v.any();
                let ctx = self.ctx;
                let done_addr = self.done_slot();
                let r = self.helper("lum_for_await_result", &[ctx, w, done_addr]);
                self.check_value(r[0]);
                self.release(v);
                self.free_v(v);
                let d = self.b.ins().load(I32, Self::mem(), done_addr, 0);
                let d = self.b.ins().icmp_imm_s(IntCC::NotEqual, d, 0);
                self.set_local(*done, V::Bool(d));
                V::Any(r[0])
            }
            ExprKind::CheckObject(x) => {
                let v = self.expr(x);
                self.hold(v);
                let w = v.any();
                let ctx = self.ctx;
                let r = self.helper("lum_check_object", &[ctx, w]);
                self.check_int(r[0]);
                self.release(v);
                self.free_v(v);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::IterCloseQuiet(l) => {
                let w = self.use_local(*l).any();
                let ctx = self.ctx;
                self.helper("lum_iter_close_quiet", &[ctx, w]);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::IntOf(x) => {
                // An int32 is not reference-counted: nothing to free.
                let w = self.expr(x).any();
                let i = self.val_int(w);
                let d = self.int_f64(i);
                self.i32s.insert(d);
                V::Num(d)
            }
            ExprKind::AnyUnary(op, x) => {
                let v = self.expr(x).any();
                V::Any(self.unary_any(*op, v))
            }
            ExprKind::AnyBinary(AnyBinOp::Add, ..) if concat_parts(e).is_some() => {
                // A template literal (or a chain of `+` on strings and
                // primitives): its parts, concatenated by one call that
                // copies each once.
                let parts = concat_parts(e).unwrap();
                let vals: Vec<V> = parts
                    .iter()
                    .map(|x| {
                        let v = self.expr(concat_operand(x));
                        self.hold(v);
                        v
                    })
                    .collect();
                for &v in vals.iter().rev() {
                    self.release(v);
                }
                let anys: Vec<Value> = vals.iter().map(|v| v.any()).collect();
                let slot = self.values_slot(&anys);
                let n = self.b.ins().iconst(I32, anys.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("JS_ConcatStrings", &[ctx, slot, n]);
                self.check_value(r[0]);
                V::Any(r[0])
            }
            ExprKind::AnyBinary(op, x, y) => {
                let a = self.expr(x);
                self.hold(a);
                let c = self.expr(y);
                self.hold(c);
                let w = self.binary_any(*op, a.any(), c.any());
                self.release(c);
                self.release(a);
                V::Any(w)
            }
            ExprKind::AnyCompare(op @ (AnyCmpOp::StrictEq | AnyCmpOp::StrictNe), x, y)
                if matches!(x.kind, ExprKind::Undefined | ExprKind::Null)
                    || matches!(y.kind, ExprKind::Undefined | ExprKind::Null) =>
            {
                // Strict equality with `undefined` / `null` is a tag test.
                let (other, konst) =
                    if matches!(y.kind, ExprKind::Undefined | ExprKind::Null) { (x, y) } else { (y, x) };
                let (v, owned) = self.operand(other, &[]);
                let tag = if let ExprKind::Null = konst.kind { TAG_NULL } else { TAG_UNDEFINED };
                let r = match v {
                    V::Any(w) => {
                        let cc = if *op == AnyCmpOp::StrictEq { IntCC::Equal } else { IntCC::NotEqual };
                        self.b.ins().icmp_imm_s(cc, w, val_const(tag, 0))
                    }
                    _ => self.b.ins().iconst(I8, (*op == AnyCmpOp::StrictNe) as i64),
                };
                self.drop_operand(v, owned);
                V::Bool(r)
            }
            ExprKind::AnyCompare(op @ (AnyCmpOp::StrictEq | AnyCmpOp::StrictNe), x, y)
                if matches!(x.kind, ExprKind::Str(_)) || matches!(y.kind, ExprKind::Str(_)) =>
            {
                // With a string constant: by pointer, then length, before
                // comparing characters.
                let (other, id) = match (&x.kind, &y.kind) {
                    (_, ExprKind::Str(id)) => (x, *id),
                    (ExprKind::Str(id), _) => (y, *id),
                    _ => unreachable!(),
                };
                let (v, owned) = self.operand(other, &[]);
                let res = self.eq_string_const(v.any(), id);
                self.drop_operand(v, owned);
                let r = if *op == AnyCmpOp::StrictNe { self.b.ins().bxor_imm_u(res, 1) } else { res };
                V::Bool(r)
            }
            ExprKind::AnyCompare(op @ (AnyCmpOp::Eq | AnyCmpOp::Ne), x, y)
                if matches!(x.kind, ExprKind::Undefined | ExprKind::Null)
                    || matches!(y.kind, ExprKind::Undefined | ExprKind::Null) =>
            {
                // Loose equality with `undefined` / `null`: the value is one of
                // them (no object is "HTMLDDA": only a test harness API makes
                // one, which this runtime never calls).
                let other = if matches!(y.kind, ExprKind::Undefined | ExprKind::Null) { x } else { y };
                let (v, owned) = self.operand(other, &[]);
                let eq = *op == AnyCmpOp::Eq;
                let r = match v {
                    V::Any(w) => {
                        // (Null and undefined: tags 2 and 3, payload 0.)
                        let x = self.b.ins().bor_imm_u(w, 1i64 << 32);
                        let cc = if eq { IntCC::Equal } else { IntCC::NotEqual };
                        self.b.ins().icmp_imm_s(cc, x, val_const(TAG_UNDEFINED, 0))
                    }
                    _ => self.b.ins().iconst(I8, !eq as i64),
                };
                self.drop_operand(v, owned);
                V::Bool(r)
            }
            ExprKind::AnyCompare(op, x, y) => {
                // Boxed numbers are compared unboxed (boxed only to go slow).
                let a = self.unboxed_operand(x);
                self.hold(a);
                let c = self.unboxed_operand(y);
                self.hold(c);
                let r = self.compare_mixed(*op, a, c);
                self.release(c);
                self.release(a);
                self.free_v(c);
                self.free_v(a);
                V::Bool(r)
            }
            ExprKind::Cell(k) => {
                let w = self.load_cell(*k);
                self.dup(w);
                V::Any(w)
            }
            ExprKind::SetCell(k, x) | ExprKind::InitCell(k, x) => {
                let v = self.expr(x);
                self.hold(v);
                // A declaration replaces `uninitialized` (nothing to free).
                let old = if let ExprKind::InitCell(..) = e.kind { self.raw_cell(*k) } else { self.load_cell(*k) };
                let w = v.any();
                self.dup(w);
                let base = self.b.ins().symbol_value(self.ptr, self.env.cells);
                let off = (*k * SIZE) as i32;
                self.b.ins().store(M_SLOT, w, base, off);
                self.free(old);
                self.release(v);
                v
            }
            ExprKind::Global(a) => {
                let atom = self.atom(*a);
                let ctx = self.ctx;
                let ic = self.new_ic();
                // While there are no global let/const definitions, an own
                // data property of the global object (cached as an own
                // property read's, by `lum_global_get`).
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                let vars = self.b.ins().load(I64, M_HEADER, ctx, layout::CTX_GLOBAL_VAR_OBJ);
                let vars = self.val_ptr(vars);
                let vars_shape = self.b.ins().load(I64, M_HEADER, vars, layout::OBJ_SHAPE);
                let count = self.b.ins().uload32(M_HEADER, vars_shape, layout::SHAPE_PROP_COUNT);
                let none = self.b.ins().icmp_imm_s(IntCC::Equal, count, 0);
                let g = self.b.ins().load(I64, M_HEADER, ctx, layout::CTX_GLOBAL_OBJ);
                let g = self.val_ptr(g);
                let shape = self.b.ins().load(I64, M_HEADER, g, layout::OBJ_SHAPE);
                let cached = self.b.ins().load(I64, M_IC, ic, 0);
                let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
                let ok = self.b.ins().band(none, same);
                let hit = self.b.create_block();
                self.b.ins().brif(ok, hit, &[], slow, &[]);
                self.b.switch_to_block(hit);
                let at = self.ic_slot_addr(g, ic);
                let w = self.b.ins().load(I64, M_SLOT, at, 0);
                self.dup(w);
                self.b.ins().jump(join, &[BlockArg::Value(w)]);
                self.b.switch_to_block(slow);
                let r = self.helper("lum_global_get", &[ctx, atom, ic]);
                self.check_value(r[0]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                V::Any(rw)
            }
            ExprKind::GlobalOrUndefined(a) => {
                let atom = self.atom(*a);
                let ctx = self.ctx;
                let r = self.helper("lum_global_or_undefined", &[ctx, atom]);
                self.check_value(r[0]);
                V::Any(r[0])
            }
            ExprKind::SetGlobal(a, x) => {
                let v = self.expr(x);
                self.hold(v);
                let w = v.any();
                self.dup(w);
                let atom = self.atom(*a);
                let ctx = self.ctx;
                let r = self.helper("JS_SetGlobalVariable", &[ctx, atom, w]);
                self.check_int(r[0]);
                self.release(v);
                v
            }
            ExprKind::GetProp(o, a) => {
                let pred = std::mem::take(&mut self.preds);
                let (obj, owned) = self.operand(o, &[]);
                let w = obj.any();
                let r = self.prop_get_from(w, None, *a, pred, false);
                self.drop_operand(obj, owned);
                V::Any(r)
            }
            ExprKind::SetProp(o, a, x) if self.prop_step(o, *a, x).is_some() => {
                let step = self.prop_step(o, *a, x).unwrap();
                self.prop_update(o, *a, step, true)
            }
            ExprKind::SetProp(o, a, x) => {
                let pred = std::mem::take(&mut self.preds);
                let (obj, owned) = self.operand(o, &[x]);
                let v = self.expr(x);
                self.hold(v);
                let vw = v.any();
                // One reference for the property, one for the expression.
                self.dup(vw);
                let w = obj.any();
                self.prop_set_ic(w, *a, vw, pred);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::ShapeGet { obj, atom, record, slot } => {
                let (o, owned) = self.operand(obj, &[]);
                let w = o.any();
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                let at = self.record_slot(w, *record, *slot, slow);
                let vw = self.b.ins().load(I64, M_SLOT, at, 0);
                self.dup(vw);
                self.b.ins().jump(join, &[BlockArg::Value(vw)]);
                self.b.switch_to_block(slow);
                self.record_learn(w, *record);
                // (Through a cache, but out of line: the record's shapes are
                // what usually holds, and this code would weigh on the
                // function's register allocation.)
                let ic = self.new_ic();
                let ctx = self.ctx;
                let atom = self.atom(*atom);
                let r = self.helper("lum_ic_get_cached", &[ctx, w, atom, ic]);
                self.check_value(r[0]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                self.drop_operand(o, owned);
                V::Any(rw)
            }
            ExprKind::ShapeSet { obj, atom, record, slot, value } => {
                let (o, owned) = self.operand(obj, &[value]);
                let v = self.expr(value);
                self.hold(v);
                let vw = v.any();
                // One reference for the property, one for the expression.
                self.dup(vw);
                let w = o.any();
                let join = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                let at = self.record_slot(w, *record, *slot, slow);
                let old = self.b.ins().load(I64, M_SLOT, at, 0);
                self.b.ins().store(M_SLOT, vw, at, 0);
                let p = self.val_ptr(w);
                self.write_barrier(p, vw);
                self.free(old);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(slow);
                self.record_learn(w, *record);
                self.prop_set_ic(w, *atom, vw, Vec::new());
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.release(v);
                self.drop_operand(o, owned);
                v
            }
            ExprKind::SpecGuard { .. } | ExprKind::ElemGuard { .. } => unreachable!("a guard outside its region"),
            ExprKind::RegionLeave(_) => unreachable!("a region's jump not made"),
            ExprKind::RegionTypedGet { arr, idx } => {
                let Ty::Arr(kind) = self.locals[*arr] else { unreachable!("a typed array") };
                let ap = self.use_local(*arr).any();
                let ap = self.val_ptr(ap);
                // (An integer inside it: see `guard_index`.)
                let i = match self.use_local(*idx) {
                    V::Num(d) => match self.int_of.get(&d) {
                        Some(&i) => i,
                        None => self.b.ins().fcvt_to_sint_sat(I64, d),
                    },
                    V::Int(i) => i,
                    other => unreachable!("index {other:?}"),
                };
                let base = self.b.ins().load(I64, M_HEADER, ap, layout::OBJ_ARRAY_VALUES);
                let at = self.b.ins().imul_imm_s(i, kind.size() as i64);
                let at = self.b.ins().iadd(base, at);
                V::Num(self.load_element(kind, at))
            }
            ExprKind::RegionTypedSet { arr, idx, value } => {
                let Ty::Arr(kind) = self.locals[*arr] else { unreachable!("a typed array") };
                let index = self.expr(idx);
                let d = self.expr(value).num();
                let ap = self.use_local(*arr).any();
                let ap = self.val_ptr(ap);
                let (ok, i) = self.elem_index(index);
                let count = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_ARRAY_COUNT);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, i, count);
                let inb = self.b.ins().band(ok, inb);
                let store = self.b.create_block();
                let join = self.b.create_block();
                self.b.ins().brif(inb, store, &[], join, &[]);
                self.b.switch_to_block(store);
                let base = self.b.ins().load(I64, M_HEADER, ap, layout::OBJ_ARRAY_VALUES);
                let at = self.b.ins().imul_imm_s(i, kind.size() as i64);
                let at = self.b.ins().iadd(base, at);
                self.store_element(kind, at, d);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                V::Num(d)
            }
            ExprKind::ArrLen { arr } => {
                let w = self.use_local(*arr).any();
                let p = self.val_ptr(w);
                let n = self.b.ins().uload32(M_HEADER, p, layout::OBJ_ARRAY_COUNT);
                V::Num(self.int_f64(n))
            }
            ExprKind::StrLen { s } => {
                let w = self.use_local(*s).any();
                let p = self.val_ptr(w);
                let head = self.b.ins().uload32(M_CONST, p, layout::STRING_LEN);
                let len = self.b.ins().band_imm_u(head, 0x7fff_ffff);
                // (An int32, not negative: compared as one.)
                let d = self.int_f64(len);
                self.i32s.insert(d);
                self.nonneg.insert(d);
                V::Num(d)
            }
            ExprKind::RegionPush { arr, value } => {
                let vw = self.expr(value).any();
                let aw = self.use_local(*arr).any();
                let ap = self.val_ptr(aw);
                // (As the guard found it: an extensible fast array whose
                // `length` is its element count, an int32.)
                let count = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_ARRAY_COUNT);
                let size = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_ARRAY_SIZE);
                let room = self.b.ins().icmp(IntCC::UnsignedLessThan, count, size);
                let store = self.b.create_block();
                let grow = self.b.create_block();
                let join = self.b.create_block();
                self.b.set_cold_block(grow);
                self.b.ins().brif(room, store, &[], grow, &[]);
                self.b.switch_to_block(store);
                let slot = self.array_slot(ap, count);
                self.b.ins().store(M_SLOT, vw, slot, 0);
                self.write_barrier(ap, vw);
                let k1 = self.b.ins().iadd_imm_s(count, 1);
                self.b.ins().istore32(M_HEADER, k1, ap, layout::OBJ_ARRAY_COUNT);
                let prop = self.b.ins().load(I64, M_HEADER, ap, layout::OBJ_PROP);
                let len = self.mk_int(k1);
                self.b.ins().store(M_SLOT, len, prop, 0);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(grow);
                let ctx = self.ctx;
                let r = self.helper("lum_region_push", &[ctx, aw, vw]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                V::Num(self.b.ins().f64const(0.0))
            }
            ExprKind::StrCharCode { s, idx } => {
                let d = self.expr(idx).num();
                let w = self.use_local(*s).any();
                let p = self.val_ptr(w);
                V::Num(self.flat_char_code(p, d))
            }
            ExprKind::SlotLoad { obj, slot } => {
                let props = self.spec_src[obj];
                let off = (*slot as usize * SIZE) as i32;
                let floats =
                    if self.spec_elems.contains(obj) { self.spec_float_elems.contains(obj) } else { self.spec_floats };
                let w = self.b.ins().load(I64, M_SLOT, props, off);
                if floats { V::Num(self.val_f64(w)) } else { V::Num(self.number_tagged(w)) }
            }
            ExprKind::SlotStore { obj, slot, value, arith } => {
                let d = self.expr(value).num();
                // (The guard's: nothing in the region moves the properties.)
                let props = self.spec_src[obj];
                // A number replaces a number: nothing to free. Where only
                // floats were read, the result is a float, as the
                // interpreter's float arithmetic makes it.
                let elem = self.spec_elems.contains(obj);
                let floats = self.spec_floats && *arith && (!elem || self.spec_float_elems.contains(obj));
                let w = if floats { self.box_float(d) } else { self.box_number(d) };
                let off = (*slot as usize * SIZE) as i32;
                self.b.ins().store(M_SLOT, w, props, off);
                V::Num(d)
            }
            ExprKind::GetElem(o, k) => {
                let (obj, owned) = self.operand(o, &[k]);
                let key = self.expr(k);
                let ow = obj.any();
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                // A fast array element.
                let (ok, idx) = self.elem_index(key);
                let (op_, count) = self.array_part(ow, ok, slow);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let hit = self.b.create_block();
                self.b.ins().brif(inb, hit, &[], slow, &[]);
                self.b.switch_to_block(hit);
                let at = self.array_slot(op_, idx);
                let vw = self.b.ins().load(I64, M_SLOT, at, 0);
                self.dup(vw);
                self.b.ins().jump(join, &[BlockArg::Value(vw)]);
                // Anything else. The key is passed on (JS_GetPropertyValue
                // frees it).
                self.b.switch_to_block(slow);
                let kw = self.boxed(key);
                let ctx = self.ctx;
                let r = self.helper("JS_GetPropertyValue", &[ctx, ow, kw]);
                self.check_value(r[0]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                self.drop_operand(obj, owned);
                V::Any(rw)
            }
            ExprKind::SetElem(o, k, x) => {
                let (obj, owned) = self.operand(o, &[k, x]);
                let key = self.expr(k);
                self.hold(key);
                let v = self.expr(x);
                self.hold(v);
                let vw = v.any();
                // One reference for the property, one for the expression.
                self.dup(vw);
                let ow = obj.any();
                let join = self.b.create_block();
                let slow = self.b.create_block();
                // Overwrite a fast array element (always a writable data
                // property).
                let (ok, idx) = self.elem_index(key);
                let (op_, count) = self.array_part(ow, ok, slow);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let hit = self.b.create_block();
                let beyond = self.b.create_block();
                self.b.ins().brif(inb, hit, &[], beyond, &[]);
                self.b.switch_to_block(hit);
                let at = self.array_slot(op_, idx);
                let old = self.b.ins().load(I64, M_SLOT, at, 0);
                self.b.ins().store(M_SLOT, vw, at, 0);
                self.write_barrier(op_, vw);
                self.free(old);
                self.b.ins().jump(join, &[]);
                // Appending (index == count): the engine's append fast path,
                // which takes the value unless it does not apply (-2).
                self.b.switch_to_block(beyond);
                let is_append = self.b.ins().icmp(IntCC::Equal, idx, count);
                let append = self.b.create_block();
                self.b.ins().brif(is_append, append, &[], slow, &[]);
                self.b.switch_to_block(append);
                let ctx = self.ctx;
                let r = self.helper("JS_ArrayAppendFast", &[ctx, ow, vw]);
                let na = self.b.ins().icmp_imm_s(IntCC::Equal, r[0], -2);
                let appended = self.b.create_block();
                self.b.ins().brif(na, slow, &[], appended, &[]);
                self.b.switch_to_block(appended);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                // Anything else: lum_set_elem takes the key and one
                // reference to the value.
                self.b.switch_to_block(slow);
                let kv = self.boxed(key);
                let slot = self.values_slot(&[ow, kv, vw]);
                self.release(v);
                self.release(key);
                self.hold(v);
                let ctx = self.ctx;
                let r = self.helper("lum_set_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::Length(o, atom) => {
                let (obj, owned) = self.operand(o, &[]);
                let ow = obj.any();
                let op_ = self.val_ptr(ow);
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                let not_obj = self.b.create_block();
                let is_obj = self.val_is(ow, TAG_OBJECT);
                let obj_blk = self.b.create_block();
                self.b.ins().brif(is_obj, obj_blk, &[], not_obj, &[]);
                // Arrays: `length` is always own property 0, a number.
                self.b.switch_to_block(obj_blk);
                let class = self.b.ins().uload16(I32, M_HEADER, op_, layout::OBJ_CLASS_ID);
                let is_array = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_ARRAY);
                let array = self.b.create_block();
                let other = self.b.create_block();
                self.b.ins().brif(is_array, array, &[], other, &[]);
                self.b.switch_to_block(array);
                let prop = self.b.ins().load(I64, M_HEADER, op_, layout::OBJ_PROP);
                let lw = self.b.ins().load(I64, M_SLOT, prop, 0);
                self.b.ins().jump(join, &[BlockArg::Value(lw)]);
                // Typed arrays with a pristine shape: the element count.
                self.b.switch_to_block(other);
                let kind = self.b.ins().iadd_imm_s(class, -layout::CLASS_TYPED_ARRAY);
                let is_ta = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, kind, TYPED_ARRAYS);
                let ta = self.b.create_block();
                self.b.ins().brif(is_ta, ta, &[], slow, &[]);
                self.b.switch_to_block(ta);
                let pristine = self.pristine_shape(op_, kind);
                let ta_len = self.b.create_block();
                self.b.ins().brif(pristine, ta_len, &[], slow, &[]);
                self.b.switch_to_block(ta_len);
                let count = self.b.ins().uload32(M_HEADER, op_, layout::OBJ_ARRAY_COUNT);
                let count = self.mk_int(count);
                self.b.ins().jump(join, &[BlockArg::Value(count)]);
                // Strings: the length in the header.
                self.b.switch_to_block(not_obj);
                let is_str = self.val_is(ow, TAG_STRING);
                let string = self.b.create_block();
                self.b.ins().brif(is_str, string, &[], slow, &[]);
                self.b.switch_to_block(string);
                let head = self.b.ins().uload32(M_HEADER, op_, layout::STRING_LEN);
                let len = self.b.ins().band_imm_u(head, 0x7fff_ffff);
                let len = self.mk_int(len);
                self.b.ins().jump(join, &[BlockArg::Value(len)]);
                self.b.switch_to_block(slow);
                let a = self.atom(*atom);
                let ctx = self.ctx;
                let r = self.helper("JS_GetProperty", &[ctx, ow, a]);
                self.check_value(r[0]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                self.drop_operand(obj, owned);
                V::Any(rw)
            }
            ExprKind::TypedGet(kind, a, i) => {
                let (arr, owned) = self.operand(a, &[i]);
                let index = self.expr(i);
                let p = self.val_ptr(arr.any());
                let join = self.b.create_block();
                let r = self.b.append_block_param(join, F64);
                let (ok, idx) = self.elem_index(index);
                let count = self.b.ins().uload32(M_HEADER, p, layout::OBJ_ARRAY_COUNT);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let inb = self.b.ins().band(ok, inb);
                let hit = self.b.create_block();
                let undef = self.b.ins().f64const(f64::from_bits(MAYBE_F64_UNDEFINED));
                self.b.ins().brif(inb, hit, &[], join, &[BlockArg::Value(undef)]);
                self.b.switch_to_block(hit);
                let base = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_ARRAY_VALUES);
                let at = self.b.ins().imul_imm_s(idx, kind.size() as i64);
                let at = self.b.ins().iadd(base, at);
                let d = self.load_element(*kind, at);
                self.b.ins().jump(join, &[BlockArg::Value(d)]);
                self.b.switch_to_block(join);
                self.drop_operand(arr, owned);
                V::Num(r)
            }
            ExprKind::TypedSet(kind, a, i, x) => {
                let (arr, owned) = self.operand(a, &[i, x]);
                let index = self.expr(i);
                let V::Num(d) = self.expr(x) else { unreachable!("TypedSet value") };
                let aw = arr.any();
                let p = self.val_ptr(aw);
                let join = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                // In bounds, and the buffer is not immutable.
                let (ok, idx) = self.elem_index(index);
                let count = self.b.ins().uload32(M_HEADER, p, layout::OBJ_ARRAY_COUNT);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let inb = self.b.ins().band(ok, inb);
                let check = self.b.create_block();
                self.b.ins().brif(inb, check, &[], slow, &[]);
                self.b.switch_to_block(check);
                let ta = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_TYPED_ARRAY);
                let buf = self.b.ins().load(I64, M_HEADER, ta, layout::TA_BUFFER);
                let abuf = self.b.ins().load(I64, M_HEADER, buf, layout::OBJ_ARRAY_BUFFER);
                let immutable = self.b.ins().uload8(I32, M_HEADER, abuf, layout::ABUF_IMMUTABLE);
                let hit = self.b.create_block();
                self.b.ins().brif(immutable, slow, &[], hit, &[]);
                self.b.switch_to_block(hit);
                let base = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_ARRAY_VALUES);
                let at = self.b.ins().imul_imm_s(idx, kind.size() as i64);
                let at = self.b.ins().iadd(base, at);
                self.store_element(*kind, at, d);
                self.b.ins().jump(join, &[]);
                // Out of bounds or immutable: whatever the engine does.
                self.b.switch_to_block(slow);
                let kv = self.boxed(index);
                let vv = self.box_number(d);
                let slot = self.values_slot(&[aw, kv, vv]);
                let ctx = self.ctx;
                let r = self.helper("lum_set_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.drop_operand(arr, owned);
                V::Num(d)
            }
            ExprKind::DefineField(o, a, x) => {
                let obj = self.expr(o);
                self.hold(obj);
                let v = self.expr(x);
                let vw = v.any();
                let w = obj.any();
                let atom = self.atom(*a);
                // The property takes the value; JS_PROP_C_W_E.
                let flags = self.b.ins().iconst(I32, PROP_THROW | 7);
                let ctx = self.ctx;
                let r = self.helper("JS_DefinePropertyValue", &[ctx, w, atom, vw, flags]);
                self.check_int(r[0]);
                self.release(obj);
                self.free_v(obj);
                V::Void
            }
            ExprKind::PrivateDefine(o, slot, x) => {
                let obj = self.expr(o);
                self.hold(obj);
                let v = self.expr(x);
                let vw = v.any();
                let w = obj.any();
                let nw = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let at = self.values_slot(&[w, nw]);
                // (The field takes the value.)
                let r = self.helper("lum_private_define", &[ctx, at, vw]);
                self.check_int(r[0]);
                self.release(obj);
                self.free_v(obj);
                V::Void
            }
            ExprKind::AddBrand(o, home) => {
                let obj = self.expr(o);
                let w = obj.any();
                let hw = self.table_value(self.env.homes, *home);
                let ctx = self.ctx;
                let r = self.helper("JS_PrivateAddBrand", &[ctx, w, hw]);
                self.check_int(r[0]);
                self.free_v(obj);
                V::Void
            }
            ExprKind::DeleteProp(o, a) => {
                let obj = self.expr(o);
                self.hold(obj);
                let atom = self.atom(*a);
                let w = obj.any();
                let flags = self.b.ins().iconst(I32, PROP_THROW);
                let ctx = self.ctx;
                let r = self.helper("JS_DeleteProperty", &[ctx, w, atom, flags]);
                self.check_int(r[0]);
                self.release(obj);
                self.free_v(obj);
                V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, r[0], 0))
            }
            ExprKind::DeleteElem(o, k) => {
                let obj = self.expr(o);
                self.hold(obj);
                let key = self.expr(k);
                self.hold(key);
                let slot = self.values_slot(&[obj.any(), key.any()]);
                let ctx = self.ctx;
                let r = self.helper("lum_delete_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.release(key);
                self.release(obj);
                self.free_v(key);
                self.free_v(obj);
                V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, r[0], 0))
            }
            ExprKind::CallValue(f, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let undef = self.konst(0, TAG_UNDEFINED);
                self.call_values(fv, undef, args, Vec::new())
            }
            ExprKind::CallThis(f, this, args) => {
                let pred = std::mem::take(&mut self.preds);
                let fv = self.expr(f);
                self.hold(fv);
                let tv = self.expr(this);
                self.hold(tv);
                let r = self.call_values(fv, tv, args, pred);
                self.release(tv);
                self.free_v(tv);
                r
            }
            ExprKind::CallMethod(o, key, args) => {
                let pred = std::mem::take(&mut self.preds);
                // The method is read before the arguments are evaluated.
                let mut later: Vec<&Expr> = args.iter().collect();
                if let Key::Value(k) = key {
                    later.push(k);
                }
                let (obj, owned) = self.operand(o, &later);
                let ow = obj.any();
                let ctx = self.ctx;
                let method = match key {
                    Key::Atom(a) if pred.is_empty() => self.env.methods.get(a).copied(),
                    _ => None,
                };
                // A builtin's fast path compares the method read and calls
                // it only when it is not the builtin: read without a new
                // reference while nothing can run before that (the arguments
                // are pure).
                let fits = |m: Method| match m {
                    Method::ArrayPush | Method::JsonStringify => args.len() == 1,
                    Method::StringFn(_) | Method::MapFn(_) => args.len() <= 2,
                    Method::CharCodeAt => true,
                };
                if let (Some(method), Key::Atom(a)) = (method, key)
                    && args.iter().all(|x| self.pure_arg(x))
                    && fits(method)
                {
                    let (mw, f_owned) = self.prop_get_inner(ow, None, *a, Vec::new(), true, true);
                    let fv = V::Any(mw);
                    let r = match method {
                        Method::CharCodeAt => self.char_code_at(fv, f_owned, obj, args),
                        Method::ArrayPush => self.array_push(fv, f_owned, obj, &args[0]),
                        Method::StringFn(k) => self.string_fn(k, fv, f_owned, obj, args),
                        Method::MapFn(k) => self.map_fn(k, fv, f_owned, obj, args),
                        Method::JsonStringify => self.json_stringify(fv, f_owned, obj, &args[0]),
                    };
                    self.drop_operand(obj, owned);
                    return r;
                }
                let m = match key {
                    Key::Atom(a) => {
                        self.poly_site = !pred.is_empty();
                        let mw = self.method_get(ow, *a);
                        self.poly_site = false;
                        vec![mw]
                    }
                    Key::Value(k) => {
                        let kw = self.expr(k).any();
                        self.helper("JS_GetPropertyValue", &[ctx, ow, kw])
                    }
                };
                self.check_value(m[0]);
                let fv = V::Any(m[0]);
                self.hold(fv);
                let r = match method {
                    Some(Method::CharCodeAt) => self.char_code_at(fv, None, obj, args),
                    Some(Method::ArrayPush) if args.len() == 1 => self.array_push(fv, None, obj, &args[0]),
                    Some(Method::StringFn(k)) if args.len() <= 2 => self.string_fn(k, fv, None, obj, args),
                    Some(Method::MapFn(k)) if args.len() <= 2 => self.map_fn(k, fv, None, obj, args),
                    Some(Method::JsonStringify) if args.len() == 1 => self.json_stringify(fv, None, obj, &args[0]),
                    Some(Method::ArrayPush | Method::StringFn(_) | Method::MapFn(_) | Method::JsonStringify) | None => {
                        self.call_values(fv, obj, args, pred)
                    }
                };
                self.drop_operand(obj, owned);
                r
            }
            ExprKind::New(f, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let held: Vec<V> = args
                    .iter()
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<Value> = held.iter().map(|v| v.any()).collect();
                let argv = self.values_slot(&vals);
                let argc = self.b.ins().iconst(I32, args.len() as i64);
                let fw = fv.any();
                let ctx = self.ctx;
                let r = self.helper("JS_CallConstructor", &[ctx, fw, argc, argv]);
                self.check_value(r[0]);
                for v in held.into_iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                self.release(fv);
                self.free_v(fv);
                V::Any(r[0])
            }
            ExprKind::SuperNew { class, target, args } => {
                // (The class and `new.target`, then the arguments.)
                let held: Vec<V> = [&**class, &**target]
                    .into_iter()
                    .chain(args)
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<Value> = held.iter().map(|v| v.any()).collect();
                let v = self.values_slot(&vals);
                let argc = self.b.ins().iconst(I32, args.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("lum_super_construct", &[ctx, argc, v]);
                self.check_value(r[0]);
                for v in held.into_iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0])
            }
            ExprKind::HomeValue(k) => {
                let w = self.table_value(self.env.homes, *k);
                self.dup(w);
                V::Any(w)
            }
            ExprKind::CurrentFunction => {
                let data = self.closure_data.expect("the function of a closure");
                let r = self.helper("JS_GetFunctionOfData", &[data]);
                self.dup(r[0]);
                V::Any(r[0])
            }
            ExprKind::ClassSetup { ctor, parent } => {
                let fv = self.expr(ctor);
                self.hold(fv);
                let pv = match parent {
                    Some(p) => self.expr(p),
                    None => self.konst(0, TAG_UNDEFINED),
                };
                self.hold(pv);
                let v = self.values_slot(&[fv.any(), pv.any()]);
                let has = self.b.ins().iconst(I32, parent.is_some() as i64);
                let ctx = self.ctx;
                let r = self.helper("lum_class_setup", &[ctx, v, has]);
                self.check_value(r[0]);
                for v in [pv, fv] {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0])
            }
            ExprKind::DefineMethod { obj, key, func, kind } => {
                let ov = self.expr(obj);
                self.hold(ov);
                let kv = match key {
                    ir::Key::Value(k) => Some(self.expr(k)),
                    ir::Key::Atom(_) => None,
                };
                if let Some(kv) = kv {
                    self.hold(kv);
                }
                let fv = self.expr(func);
                self.hold(fv);
                let k = self.b.ins().iconst(I32, *kind as i64);
                let ctx = self.ctx;
                let r = match (key, kv) {
                    (ir::Key::Atom(a), _) => {
                        let v = self.values_slot(&[ov.any(), fv.any()]);
                        let atom = self.atom(*a);
                        self.helper("lum_define_method", &[ctx, v, atom, k])
                    }
                    (_, Some(kv)) => {
                        let v = self.values_slot(&[ov.any(), fv.any(), kv.any()]);
                        self.helper("lum_define_method_computed", &[ctx, v, k])
                    }
                    _ => unreachable!(),
                };
                self.check_int(r[0]);
                for v in [Some(fv), kv, Some(ov)].into_iter().flatten() {
                    self.release(v);
                    self.free_v(v);
                }
                V::Void
            }
            ExprKind::NewFromTarget(x) => self.convert(x, "lum_new_from_target"),
            ExprKind::SuperNewSpread { class, target, args } => {
                let held: Vec<V> = [&**class, &**target, &**args]
                    .into_iter()
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<Value> = held.iter().map(|v| v.any()).collect();
                let v = self.values_slot(&vals);
                let ctx = self.ctx;
                let r = self.helper("lum_super_construct_array", &[ctx, v]);
                self.check_value(r[0]);
                for v in held.into_iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0])
            }
            ExprKind::Object(props) => self.new_object(props, None),
            ExprKind::NewInstance { class, fields } => self.new_object(fields, Some(*class)),
            ExprKind::NewInstanceFull { class, fields, brand, privates } => {
                self.new_object_with(fields, Some(*class), Some((*brand, privates)))
            }
            ExprKind::Array(elems) => {
                let held: Vec<V> = elems
                    .iter()
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<Value> = held.iter().map(|v| v.any()).collect();
                let slot = self.values_slot(&vals);
                // lum_new_array takes ownership of the elements.
                for v in held.into_iter().rev() {
                    self.release(v);
                }
                let n = self.b.ins().iconst(I32, elems.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("lum_new_array", &[ctx, n, slot]);
                self.check_value(r[0]);
                V::Any(r[0])
            }
            ExprKind::ArrayParts(parts) => {
                let ctx = self.ctx;
                let a = self.helper("JS_NewArray", &[ctx]);
                self.check_value(a[0]);
                let arr = V::Any(a[0]);
                self.hold(arr);
                for part in parts {
                    let r = match part {
                        ir::ArrPart::Value(x) => {
                            // Takes ownership of the value.
                            let w = self.expr(x).any();
                            self.helper("lum_array_push", &[ctx, a[0], w])
                        }
                        ir::ArrPart::Spread(x) => {
                            let v = self.expr(x);
                            self.hold(v);
                            let w = v.any();
                            let r = self.helper("lum_array_spread", &[ctx, a[0], w]);
                            self.release(v);
                            self.free_v(v);
                            r
                        }
                        ir::ArrPart::Hole => self.helper("lum_array_hole", &[ctx, a[0]]),
                    };
                    self.check_int(r[0]);
                }
                self.release(arr);
                arr
            }
            ExprKind::ObjectParts(parts) => {
                let ctx = self.ctx;
                let o = self.helper("JS_NewObject", &[ctx]);
                self.check_value(o[0]);
                let obj = V::Any(o[0]);
                self.hold(obj);
                for part in parts {
                    match part {
                        ir::ObjPart::Field(a, x) => {
                            let w = self.expr(x).any();
                            let slot = self.values_slot(&[o[0]]);
                            let atom = self.atom(*a);
                            let r = self.helper("lum_define_field", &[ctx, slot, atom, w]);
                            self.check_int(r[0]);
                        }
                        ir::ObjPart::Computed(k, x) => {
                            // The key is converted before the value is evaluated.
                            let kw = self.expr(k).any();
                            self.hold(V::Any(kw));
                            let key = self.helper("JS_ToPropertyKey", &[ctx, kw]);
                            self.check_value(key[0]);
                            self.release(V::Any(kw));
                            self.free(kw);
                            let key = V::Any(key[0]);
                            self.hold(key);
                            let w = self.expr(x).any();
                            let slot = self.values_slot(&[o[0], key.any()]);
                            let r = self.helper("lum_define_computed", &[ctx, slot, w]);
                            self.check_int(r[0]);
                            self.release(key);
                            self.free_v(key);
                        }
                        ir::ObjPart::Accessor(a, x, setter) => {
                            let w = self.expr(x).any();
                            let slot = self.values_slot(&[o[0]]);
                            let atom = self.atom(*a);
                            let s = self.b.ins().iconst(I32, *setter as i64);
                            let r = self.helper("lum_define_accessor", &[ctx, slot, atom, w, s]);
                            self.check_int(r[0]);
                        }
                        ir::ObjPart::Spread(x) => {
                            let v = self.expr(x);
                            self.hold(v);
                            let w = v.any();
                            let r = self.helper("lum_object_spread", &[ctx, o[0], w]);
                            self.check_int(r[0]);
                            self.release(v);
                            self.free_v(v);
                        }
                    }
                }
                self.release(obj);
                obj
            }
            ExprKind::CallSpread(f, this, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let tv = self.expr(this);
                self.hold(tv);
                let av = self.expr(args);
                self.hold(av);
                let slot = self.values_slot(&[fv.any(), tv.any(), av.any()]);
                let ctx = self.ctx;
                let r = self.helper("lum_apply", &[ctx, slot]);
                self.check_value(r[0]);
                for v in [av, tv, fv] {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0])
            }
            ExprKind::NewSpread(f, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let av = self.expr(args);
                self.hold(av);
                let (fw, aw) = (fv.any(), av.any());
                let ctx = self.ctx;
                let r = self.helper("lum_construct", &[ctx, fw, aw]);
                self.check_value(r[0]);
                for v in [av, fv] {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0])
            }
            ExprKind::IterOpen(x, iter, next, atom) => {
                self.iter_open(x, *iter, *next, *atom);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::ForInIterator(x) => self.convert(x, "JS_ForInIteratorObject"),
            ExprKind::ToObject(x) => self.convert(x, "JS_ToObject"),
            ExprKind::PrivateField(o, slot) => {
                let (obj, owned) = self.operand(o, &[]);
                let ow = obj.any();
                let ic = self.new_ic();
                let join = self.b.create_block();
                let rw = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                // A field of a cached shape (private fields are properties
                // keyed by their private symbol).
                let at = self.ic_hit(ow, ic, slow);
                let vw = self.b.ins().load(I64, M_SLOT, at, 0);
                self.dup(vw);
                self.b.ins().jump(join, &[BlockArg::Value(vw)]);
                self.b.switch_to_block(slow);
                let nw = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("lum_private_get_ic", &[ctx, ow, nw, ic]);
                self.check_value(r[0]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                self.drop_operand(obj, owned);
                V::Any(rw)
            }
            ExprKind::PrivateSetField(o, slot, x) => {
                let (obj, owned) = self.operand(o, &[x]);
                let v = self.expr(x);
                self.hold(v);
                let vw = v.any();
                // One reference for the field (taken), one for the expression.
                self.dup(vw);
                let ow = obj.any();
                let ic = self.new_ic();
                let join = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                let at = self.ic_hit(ow, ic, slow);
                let old = self.b.ins().load(I64, M_SLOT, at, 0);
                self.b.ins().store(M_SLOT, vw, at, 0);
                let op_ = self.val_ptr(ow);
                self.write_barrier(op_, vw);
                self.free(old);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(slow);
                let nw = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let slot = self.values_slot(&[ow, nw]);
                let r = self.helper("lum_private_set_ic", &[ctx, slot, vw, ic]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::PrivateMethod(o, slot) | ExprKind::PrivateGetter(o, slot) => {
                let (obj, owned) = self.operand(o, &[]);
                let ow = obj.any();
                let fw = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("JS_PrivateCheckBrand", &[ctx, ow, fw]);
                self.check_int(r[0]);
                let v = if let ExprKind::PrivateGetter(..) = e.kind {
                    let slot = self.values_slot(&[fw, ow]);
                    let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
                    let argc = self.b.ins().iconst(I32, 0);
                    let r = self.helper("lum_call", &[ctx, slot, argc, argv]);
                    self.check_value(r[0]);
                    V::Any(r[0])
                } else {
                    self.dup(fw);
                    V::Any(fw)
                };
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::PrivateSetter(o, slot, x) => {
                let (obj, owned) = self.operand(o, &[x]);
                let v = self.expr(x);
                self.hold(v);
                let vw = v.any();
                let ow = obj.any();
                let fw = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("JS_PrivateCheckBrand", &[ctx, ow, fw]);
                self.check_int(r[0]);
                let slot = self.values_slot(&[fw, ow, vw]);
                let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
                let argc = self.b.ins().iconst(I32, 1);
                let r = self.helper("lum_call", &[ctx, slot, argc, argv]);
                self.check_value(r[0]);
                self.free(r[0]);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::PrivateIn(o, slot) => {
                let (obj, owned) = self.operand(o, &[]);
                let ow = obj.any();
                let nw = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("JS_PrivateIn", &[ctx, ow, nw]);
                self.check_int(r[0]);
                self.drop_operand(obj, owned);
                V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, r[0], 0))
            }
            ExprKind::SuperGetAtom(home, a) => {
                let hw = self.table_value(self.env.homes, *home);
                let this = self.this.expect("`super` in a method");
                V::Any(self.prop_get_from(hw, Some(this), *a, Vec::new(), false))
            }
            ExprKind::SuperGet(home, k) => {
                let key = self.expr(k);
                self.hold(key);
                let h = self.table_value(self.env.homes, *home);
                let this = self.this.expect("`super` in a method");
                let slot = self.values_slot(&[h, this, key.any()]);
                let ctx = self.ctx;
                let r = self.helper("lum_super_get", &[ctx, slot]);
                self.check_value(r[0]);
                self.release(key);
                self.free_v(key);
                V::Any(r[0])
            }
            ExprKind::SuperSet(home, k, x) => {
                let key = self.expr(k);
                self.hold(key);
                let v = self.expr(x);
                self.hold(v);
                let vw = v.any();
                // One reference for the property (taken), one for the expression.
                self.dup(vw);
                let h = self.table_value(self.env.homes, *home);
                let this = self.this.expect("`super` in a method");
                let slot = self.values_slot(&[h, this, key.any()]);
                let ctx = self.ctx;
                let r = self.helper("lum_super_set", &[ctx, slot, vw]);
                self.check_int(r[0]);
                self.release(v);
                self.release(key);
                self.free_v(key);
                v
            }
            ExprKind::ForInGet(iter, o, k) => {
                let (obj, owned) = self.operand(o, &[k]);
                let (key, key_owned) = self.operand(k, &[]);
                let it = self.use_local(*iter).any();
                let slot = self.values_slot(&[it, obj.any(), key.any()]);
                let ctx = self.ctx;
                let r = self.helper("lum_for_in_get", &[ctx, slot]);
                self.check_value(r[0]);
                self.drop_operand(key, key_owned);
                self.drop_operand(obj, owned);
                V::Any(r[0])
            }
            ExprKind::ToPropKey(x) => self.convert(x, "JS_ToPropertyKey"),
            ExprKind::ArrayPush(arr, x) => {
                // Takes ownership of the value.
                let w = self.expr(x).any();
                let aw = self.use_local(*arr).any();
                let ctx = self.ctx;
                let r = self.helper("lum_array_push", &[ctx, aw, w]);
                self.check_int(r[0]);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::IterStep(iter, next) => self.iter_step(*iter, *next),
            ExprKind::IterClose(iter, next) => {
                self.close_iter(*iter, *next);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::ObjectRest(src, keys) => {
                let sv = self.expr(src);
                self.hold(sv);
                let mut held = vec![sv];
                for k in keys {
                    let v = self.expr(k);
                    self.hold(v);
                    held.push(v);
                }
                let vals: Vec<Value> = held.iter().map(|v| v.any()).collect();
                let slot = self.values_slot(&vals);
                let n = self.b.ins().iconst(I32, keys.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("lum_object_rest", &[ctx, slot, n]);
                self.check_value(r[0]);
                for v in held.into_iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0])
            }
        }
    }

    /// The address of field `fi` of layout `k` in object `p` (of that
    /// layout): past its `LumStruct` header (layout, mask).
    fn struct_field_addr(&mut self, k: u32, fi: u32, p: Value) -> Value {
        let s = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_OPAQUE);
        let off = 8 + self.env.layouts[k as usize].fields[fi as usize].offset;
        self.b.ins().iadd_imm_s(s, off as i64)
    }

    /// The value of a field `fi` it is present in object `p` (its type's
    /// storage; boxed when JS values are what `ty` says).
    fn struct_field_value(&mut self, k: u32, fi: u32, p: Value, boxed: bool) -> V {
        let f = self.env.layouts[k as usize].fields[fi as usize].clone();
        let at = self.struct_field_addr(k, fi, p);
        let v = match f.storage {
            ir::Storage::Int32 => {
                let i = self.b.ins().sload32(M_STRUCT, at, 0);
                let d = self.int_f64(i);
                self.i32s.insert(d);
                V::Num(d)
            }
            ir::Storage::F64 => V::Num(self.b.ins().load(F64, M_STRUCT, at, 0)),
            ir::Storage::Bool => {
                let x = self.b.ins().uload8(I32, M_STRUCT, at, 0);
                V::Bool(self.b.ins().ireduce(I8, x))
            }
            ir::Storage::Value => {
                let w = self.b.ins().load(I64, M_STRUCT, at, 0);
                self.dup(w);
                V::Any(w)
            }
        };
        if boxed && !matches!(v, V::Any(..)) { V::Any(self.boxed(v)) } else { v }
    }

    /// `obj.field` of an object of layout `k` (borrowed): typed if the field
    /// is in every member, else a JS value (`undefined` if absent).
    fn struct_load(&mut self, k: u32, fi: u32, p: Value) -> V {
        if self.env.layouts[k as usize].fields[fi as usize].universal {
            return self.struct_field_value(k, fi, p, false);
        }
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let s = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_OPAQUE);
        let mask = self.b.ins().load(I32, M_STRUCT, s, 4);
        let bit = self.b.ins().band_imm_u(mask, 1i64 << fi);
        let present = self.b.create_block();
        let undef = self.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0));
        self.b.ins().brif(bit, present, &[], join, &[BlockArg::Value(undef)]);
        self.b.switch_to_block(present);
        let w = self.struct_field_value(k, fi, p, true).any();
        self.b.ins().jump(join, &[BlockArg::Value(w)]);
        self.b.switch_to_block(join);
        V::Any(rw)
    }

    /// Store `v` (taken) in field `fi` of `obj` (layout `k`): of a new
    /// object (`fresh`, from a literal) or a write. Numbers and booleans of
    /// the field's type go inline into fields that hold them unboxed when
    /// no member can change; everything else is checked by the runtime.
    fn struct_store(&mut self, k: u32, fi: u32, ow: Value, v: V, fresh: bool) {
        let f = self.env.layouts[k as usize].fields[fi as usize].clone();
        let p = self.val_ptr(ow);
        let join = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let inline = fresh || f.simple;
        let at = self.struct_field_addr(k, fi, p);
        match (f.storage, v) {
            (ir::Storage::Int32, V::Num(d)) if inline => {
                if self.i32s.contains(&d) || (self.env.int_wrapping && self.int_of.contains_key(&d)) {
                    // An int32, or an exact integer that wraps.
                    let i = self.int_of_num(d);
                    let i = self.b.ins().ireduce(I32, i);
                    self.b.ins().store(M_STRUCT, i, at, 0);
                    self.b.ins().jump(join, &[]);
                } else {
                    // An int32 exactly (not -0): else the runtime wraps it
                    // or throws.
                    let i = self.b.ins().fcvt_to_sint_sat(I32, d);
                    let back = self.b.ins().fcvt_from_sint(F64, i);
                    let same = self.b.ins().fcmp(FloatCC::Equal, back, d);
                    let bits = self.b.ins().bitcast(I64, d);
                    let negzero = self.b.ins().icmp_imm_s(IntCC::Equal, bits, i64::MIN);
                    let ok = self.b.ins().band_not(same, negzero);
                    let st = self.b.create_block();
                    self.b.ins().brif(ok, st, &[], slow, &[]);
                    self.b.switch_to_block(st);
                    self.b.ins().store(M_STRUCT, i, at, 0);
                    self.b.ins().jump(join, &[]);
                }
            }
            (ir::Storage::Int32, V::Any(vw)) if inline => {
                let is_int = self.val_is(vw, TAG_INT);
                let st = self.b.create_block();
                self.b.ins().brif(is_int, st, &[], slow, &[]);
                self.b.switch_to_block(st);
                let i = self.b.ins().ireduce(I32, vw);
                self.b.ins().store(M_STRUCT, i, at, 0);
                self.b.ins().jump(join, &[]);
            }
            (ir::Storage::F64, V::Num(d)) if inline => {
                self.b.ins().store(M_STRUCT, d, at, 0);
                self.b.ins().jump(join, &[]);
            }
            (ir::Storage::F64, V::Any(vw)) if inline => {
                let num = self.is_number(vw);
                let st = self.b.create_block();
                self.b.ins().brif(num, st, &[], slow, &[]);
                self.b.switch_to_block(st);
                let d = self.number_of(vw);
                self.b.ins().store(M_STRUCT, d, at, 0);
                self.b.ins().jump(join, &[]);
            }
            (ir::Storage::Bool, V::Bool(x)) if inline => {
                let x = self.b.ins().uextend(I32, x);
                self.b.ins().istore8(M_STRUCT, x, at, 0);
                self.b.ins().jump(join, &[]);
            }
            _ => {
                self.b.ins().jump(slow, &[]);
            }
        }
        self.b.switch_to_block(slow);
        let bw = self.boxed(v);
        let ctx = self.ctx;
        let fv = self.b.ins().iconst(I32, fi as i64);
        let name = if fresh { "lum_struct_put" } else { "lum_struct_set" };
        let r = self.helper(name, &[ctx, ow, fv, bw]);
        self.check_int(r[0]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    /// An inlined body (see `ExprKind::Inline`) of type `ty` over `locals`.
    fn inline_body(&mut self, body: &[Stmt], locals: &[ir::LocalId], ty: Ty) -> V {
        let join = self.b.create_block();
        for t in clif_types(ty) {
            self.b.append_block_param(join, t);
        }
        self.inline_rets.push((join, ty));
        self.stmts(body);
        if !self.is_filled() {
            // Falling off the end.
            let v = self.zero(ty);
            let args: Vec<BlockArg> = self.abi(ty, v).into_iter().map(BlockArg::Value).collect();
            self.b.ins().jump(join, &args);
        }
        self.inline_rets.pop();
        self.b.switch_to_block(join);
        let params = self.b.block_params(join).to_vec();
        let v = self.unabi(ty, &params);
        // The callee's JS values are released as its return would.
        for &l in locals {
            if is_js(self.locals[l]) && !matches!(self.vars[l], LVar::Boxed { .. }) {
                let old = self.use_local(l).any();
                let u = self.konst(0, TAG_UNDEFINED);
                self.def_local(l, u);
                self.free(old);
            }
        }
        v
    }

    /// Whether `fw` is native function `func` (by its JS entry).
    fn is_native(&mut self, fw: Value, func: usize) -> Value {
        let join = self.b.create_block();
        let r = self.b.append_block_param(join, I8);
        let no = self.b.ins().iconst(I8, 0);
        let obj = self.b.create_block();
        let is_obj = self.val_is(fw, TAG_OBJECT);
        self.b.ins().brif(is_obj, obj, &[], join, &[BlockArg::Value(no)]);
        self.b.switch_to_block(obj);
        let fp = self.val_ptr(fw);
        let class = self.b.ins().uload16(I32, M_HEADER, fp, layout::OBJ_CLASS_ID);
        let is_data = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_C_FUNCTION_DATA);
        let data = self.b.create_block();
        self.b.ins().brif(is_data, data, &[], join, &[BlockArg::Value(no)]);
        self.b.switch_to_block(data);
        let rec = self.b.ins().load(I64, M_HEADER, fp, layout::OBJ_FUNC_DATA);
        let entry = self.b.ins().load(I64, M_HEADER, rec, layout::FDATA_FAST);
        let want = self.b.ins().func_addr(self.ptr, self.env.fast[func]);
        let same = self.b.ins().icmp(IntCC::Equal, entry, want);
        self.b.ins().jump(join, &[BlockArg::Value(same)]);
        self.b.switch_to_block(join);
        r
    }

    /// The JS entry (`lum_fast_<i>`) of function `fw`, or 0 if it has
    /// none (not a native function): as the inline cache it was read
    /// through keeps it (`fn_entries`), else read from the function.
    fn fn_entry(&mut self, fw: Value) -> Value {
        let join = self.b.create_block();
        let r = self.b.append_block_param(join, I64);
        let read = self.b.create_block();
        match self.fn_entries.get(&fw).copied() {
            Some(e) => {
                self.b.ins().brif(e, join, &[BlockArg::Value(e)], read, &[]);
            }
            None => {
                self.b.ins().jump(read, &[]);
            }
        }
        self.b.switch_to_block(read);
        let zero = self.b.ins().iconst(I64, 0);
        let obj = self.b.create_block();
        let is_obj = self.val_is(fw, TAG_OBJECT);
        self.b.ins().brif(is_obj, obj, &[], join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(obj);
        let fp = self.val_ptr(fw);
        let class = self.b.ins().uload16(I32, M_HEADER, fp, layout::OBJ_CLASS_ID);
        let is_data = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_C_FUNCTION_DATA);
        let data = self.b.create_block();
        self.b.ins().brif(is_data, data, &[], join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(data);
        let rec = self.b.ins().load(I64, M_HEADER, fp, layout::OBJ_FUNC_DATA);
        let entry = self.b.ins().load(I64, M_HEADER, rec, layout::FDATA_FAST);
        self.b.ins().jump(join, &[BlockArg::Value(entry)]);
        self.b.switch_to_block(join);
        r
    }

    /// For a prediction: whether the function whose JS entry is `entry`
    /// (see `fn_entry`) is the predicted one and `args` (JS values,
    /// borrowed) suit its parameters, as its entry requires. `None` if they
    /// cannot.
    fn pred_guard(&mut self, pred: &Pred, entry: Value, args: &[Value]) -> Option<Value> {
        let params = self.env.params[pred.func].clone();
        let mut checks = Vec::new();
        for (i, &pt) in params.iter().enumerate() {
            match pt {
                Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_) => {}
                Ty::F64 | Ty::MaybeF64 => checks.push((*args.get(i)?, true)),
                Ty::Bool => checks.push((*args.get(i)?, false)),
                _ => return None,
            }
        }
        let want = self.b.ins().func_addr(self.ptr, self.env.fast[pred.func]);
        let mut ok = self.b.ins().icmp(IntCC::Equal, entry, want);
        for (w, number) in checks {
            let c = if number { self.is_number(w) } else { self.val_is(w, TAG_BOOL) };
            ok = self.b.ins().band(ok, c);
        }
        Some(ok)
    }

    /// The predicted function's body inline, with `this` the receiver `recv`
    /// and `args` (JS values, borrowed) its arguments, which `pred_guard`
    /// checked. Its result as a new JS value.
    fn inline_method(&mut self, pred: &Pred, recv: Value, args: &[Value]) -> Value {
        let params = self.env.params[pred.func].clone();
        let ret = self.env.rets[pred.func];
        for (i, &pt) in params.iter().enumerate() {
            let l = pred.locals[i];
            let v = match (pt, args.get(i)) {
                (Ty::F64 | Ty::MaybeF64, Some(&a)) => V::Num(self.number_of(a)),
                (Ty::Bool, Some(&w)) => {
                    let i = self.b.ins().ireduce(I32, w);
                    V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, i, 0))
                }
                (_, Some(&w)) => {
                    self.dup(w);
                    V::Any(w)
                }
                (_, None) => self.konst(0, TAG_UNDEFINED),
            };
            // The local may be `Any` where its parameter is not.
            let v = if is_js(self.locals[l]) && !matches!(v, V::Any(..)) { V::Any(self.boxed(v)) } else { v };
            self.init_local(l, v);
        }
        let outer = self.this.replace(recv);
        let v = self.inline_body(&pred.body, &pred.locals, ret);
        self.this = outer;
        self.boxed(v)
    }

    /// Call `f` through `call_linked`, or inline as `pred` predicts.
    #[allow(clippy::too_many_arguments)]
    fn call_or_inline(
        &mut self,
        preds: &[Pred],
        f: Value,
        this: Value,
        args: &[Value],
        argv: impl FnOnce(&mut Self) -> Value,
        borrowed: bool,
        generic: impl FnOnce(&mut Self, Value) -> Value,
    ) -> Value {
        if preds.is_empty() {
            let argv = argv(self);
            return self.call_linked(f, this, args.len(), argv, borrowed, |l| generic(l, argv));
        }
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        // Each prediction in turn (a polymorphic site has several), then
        // the call.
        let entry = self.fn_entry(f);
        for pred in preds {
            let Some(ok) = self.pred_guard(pred, entry, args) else { continue };
            let inline = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(ok, inline, &[], next, &[]);
            self.b.switch_to_block(inline);
            let w = self.inline_method(pred, this, args);
            self.b.ins().jump(join, &[BlockArg::Value(w)]);
            self.b.switch_to_block(next);
        }
        // (The arguments' array only where the call is made.)
        let argv = argv(self);
        let w = self.call_linked(f, this, args.len(), argv, borrowed, |l| generic(l, argv));
        self.b.ins().jump(join, &[BlockArg::Value(w)]);
        self.b.switch_to_block(join);
        rw
    }

    /// An object literal (`class`: `None`), or an instance of a class with
    /// a native constructor, with these fields.
    fn new_object(&mut self, props: &[(ir::AtomId, Expr)], class: Option<usize>) -> V {
        self.new_object_with(props, class, None)
    }

    /// `new_object`, of an instance with private state too (`private`: the
    /// brand's home slot, and the private fields' slots and values), which
    /// follows its public fields in its shape: the brand's property (its
    /// value `undefined`), then the private fields.
    fn new_object_with(
        &mut self,
        props: &[(ir::AtomId, Expr)],
        class: Option<usize>,
        private: Option<(Option<usize>, &[(usize, Expr)])>,
    ) -> V {
        // The values first (the object is unobservable until made),
        // then the object: an allocation site caches the literal's
        // final shape, and later objects are made with it directly,
        // as JSC's allocation profiles do.
        let mut held: Vec<V> = props
            .iter()
            .map(|(_, x)| {
                let v = V::Any(self.expr(x).any());
                self.hold(v);
                v
            })
            .collect();
        let mut vals: Vec<Value> = held.iter().map(|v| v.any()).collect();
        if let Some((brand, privates)) = private {
            if brand.is_some() {
                vals.push(self.konst(0, TAG_UNDEFINED).any());
            }
            for (_, x) in privates {
                let w = self.expr(x).any();
                let v = V::Any(w);
                self.hold(v);
                held.push(v);
                vals.push(w);
            }
        }
        // Both paths take the values.
        for &v in held.iter().rev() {
            self.release(v);
        }
        let ic = self.new_ic();
        let ctx = self.ctx;
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let shape = self.b.ins().load(I64, M_IC, ic, 0);
        self.b.ins().brif(shape, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let call = self.b.create_block();
        self.b.set_cold_block(call);
        let p = self.alloc_object(ic, shape, call);
        let at = self.b.ins().iadd_imm_s(p, layout::OBJ_SIZE as i64);
        for (i, &w) in vals.iter().enumerate() {
            self.b.ins().store(M_SLOT, w, at, (i * SIZE) as i32);
        }
        let w = self.mk_ref(TAG_OBJECT, p);
        self.b.ins().jump(join, &[BlockArg::Value(w)]);
        self.b.switch_to_block(call);
        let values = self.values_slot(&vals);
        let r = self.helper("JS_NewObjectWithCachedShape", &[ctx, shape, values]);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(slow);
        let values = self.values_slot(&vals);
        let n = props.len();
        let atoms =
            self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (n.max(1) * 4) as u32, 2));
        for (i, (a, _)) in props.iter().enumerate() {
            let atom = self.atom(*a);
            self.b.ins().stack_store(self.ptr, atom, atoms, (i * 4) as i32);
        }
        let atoms = self.b.ins().stack_addr(self.ptr, atoms, 0);
        let count = self.b.ins().iconst(I32, n as i64);
        let r = match (class, private) {
            (None, _) => self.helper("lum_new_object", &[ctx, ic, count, atoms, values]),
            (Some(k), None) => {
                let pw = self.table_value(self.env.homes, 2 * k);
                self.helper("lum_new_instance", &[ctx, ic, pw, count, atoms, values])
            }
            (Some(k), Some((brand, privates))) => {
                // The brand's home object (or `undefined`), then the
                // private names.
                let mut extra = vec![match brand {
                    Some(h) => self.table_value(self.env.homes, h),
                    None => self.konst(0, TAG_UNDEFINED).any(),
                }];
                for &(slot, _) in privates {
                    extra.push(self.table_value(self.env.privates, slot));
                }
                let extra = self.values_slot(&extra);
                let nprivate = self.b.ins().iconst(I32, privates.len() as i64);
                let pw = self.table_value(self.env.homes, 2 * k);
                self.helper("lum_new_instance_full", &[ctx, ic, pw, count, atoms, values, extra, nprivate])
            }
        };
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(join);
        self.check_value(rw);
        V::Any(rw)
    }

    /// A new plain object of the shape `shape` that allocation site `ic`
    /// cached, allocated inline as `JS_NewObjectWithCachedShape`'s fast path
    /// would (the next cell of its class's run in the GC heap, which was
    /// counted allocated when the run began: a collection or the memory
    /// limit is the run's end's to see): its address, with every field but
    /// the properties set. Else (an empty run, or objects of the shape too
    /// big) it goes to `call`.
    fn alloc_object(&mut self, ic: Value, shape: Value, call: Block) -> Value {
        // The site's cell class and cell size (`JS_ShapeAllocInfo`).
        let info = self.b.ins().uload32(M_IC, ic, 8);
        let p = self.alloc_cell(info, call);
        // The object: extensible, of class Object, no weak references (the
        // flags word, then the weak index, 0, in one store), with the shape
        // (pinned: not counted, see `JSShape`) and its properties after
        // it.
        const _: () = assert!(layout::OBJ_WEAK_REF == layout::OBJ_FLAGS + 4);
        let flags = self.b.ins().iconst(I64, layout::OBJ_PLAIN_FLAGS);
        self.b.ins().store(M_HEADER, flags, p, layout::OBJ_FLAGS);
        self.b.ins().store(M_HEADER, shape, p, layout::OBJ_SHAPE);
        let props = self.b.ins().iadd_imm_s(p, layout::OBJ_SIZE as i64);
        self.b.ins().store(M_HEADER, props, p, layout::OBJ_PROP);
        p
    }

    /// A young GC cell of the class and size `info` gives (as
    /// `JS_ShapeAllocInfo`), bumped off its class's run, with its header (a
    /// JS object's, with one reference); else (`info` 0, or no room) to
    /// `call`. The caller sets every field before anything may collect.
    fn alloc_cell(&mut self, info: Value, call: Block) -> Value {
        let alloc = self.b.create_block();
        self.b.ins().brif(info, alloc, &[], call, &[]);
        self.b.switch_to_block(alloc);
        let idx = self.b.ins().band_imm_u(info, 0xff);
        let cell = self.b.ins().ushr_imm_u(info, 8);
        let ctx = self.ctx;
        let rt = self.b.ins().load(I64, M_CONST, ctx, layout::CTX_RT);
        let off = self.b.ins().imul_imm_s(idx, layout::GC_RUN_SIZE as i64);
        let run = self.b.ins().iadd(rt, off);
        let cp = self.b.ins().load(I64, M_ALLOC, run, layout::RT_GC_RUNS);
        let end = self.b.ins().load(I64, M_ALLOC, run, layout::RT_GC_RUNS + 8);
        let next = self.b.ins().iadd(cp, cell);
        let room = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, next, end);
        let bump = self.b.create_block();
        self.b.ins().brif(room, bump, &[], call, &[]);
        self.b.switch_to_block(bump);
        self.b.ins().store(M_ALLOC, next, run, layout::RT_GC_RUNS);
        let p = self.b.ins().iadd_imm_s(cp, 8);
        // Its header: a GC cell of its class, a JS object (unmarked: young)
        // with one reference.
        let word = self.b.ins().bor_imm_u(idx, layout::GC_CELL_WORD | (1 << 32));
        self.b.ins().store(M_RC, word, p, -8);
        p
    }

    /// A closure of native function `index` over `vals` made inline, as
    /// `JS_NewCFunctionDataShaped` makes it from the template its first
    /// one left (`JS_CFunctionDataTemplate`): the object, its `length` and
    /// `name`, then its record and data, in one cell; else (no template
    /// yet, no room) to `call`.
    fn alloc_closure(&mut self, index: usize, vals: &[Value], call: Block) -> Value {
        let caches = self.b.ins().symbol_value(self.ptr, self.env.fn_caches);
        let c = self.b.ins().iadd_imm_s(caches, index as i64 * FN_CACHE_SIZE);
        let info = self.b.ins().uload32(M_IC, c, FN_CACHE_INFO);
        let p = self.alloc_cell(info, call);
        let t = FN_CACHE_TMPL;
        let flags = self.b.ins().load(I64, M_IC, c, t + layout::FTMPL_FLAGS);
        self.b.ins().store(M_HEADER, flags, p, layout::OBJ_FLAGS);
        let shape = self.b.ins().load(I64, M_IC, c, t + layout::FTMPL_SHAPE);
        self.b.ins().store(M_HEADER, shape, p, layout::OBJ_SHAPE);
        let props = self.b.ins().iadd_imm_s(p, layout::OBJ_CLASS_SIZE as i64);
        self.b.ins().store(M_HEADER, props, p, layout::OBJ_PROP);
        let rec = layout::OBJ_CLASS_SIZE + 2 * SIZE as i32;
        let rp = self.b.ins().iadd_imm_s(p, rec as i64);
        self.b.ins().store(M_HEADER, rp, p, layout::OBJ_FUNC_DATA);
        // `length` and `name`, and the record but `self`.
        for k in 0..(2 * SIZE as i32 / 8) {
            let w = self.b.ins().load(I64, M_IC, c, t + layout::FTMPL_PROPS + 8 * k);
            self.b.ins().store(M_SLOT, w, p, layout::OBJ_CLASS_SIZE + 8 * k);
        }
        for k in [0, 1, 3] {
            let w = self.b.ins().load(I64, M_IC, c, t + layout::FTMPL_RECORD + 8 * k);
            self.b.ins().store(M_HEADER, w, p, rec + 8 * k);
        }
        const _: () = assert!(layout::FDATA_SELF == 16);
        self.b.ins().store(M_HEADER, p, p, rec + layout::FDATA_SELF);
        for (k, &w) in vals.iter().enumerate() {
            let at = rec + layout::FDATA_DATA + (SIZE * k) as i32;
            self.b.ins().store(M_SLOT, w, p, at);
        }
        p
    }

    /// `recv.method(callback)` as a native loop over the elements of an
    /// array without holes whose `method` is the original builtin `kind`,
    /// with the callback's `body` inline (see `ExprKind::ArrayLoop`); else
    /// a call. The loop reads each element afresh (the callback may change
    /// the array): past the fast elements, as the builtin does.
    #[allow(clippy::too_many_arguments)]
    fn array_loop(
        &mut self,
        kind: LoopKind,
        recv: &Expr,
        method: ir::AtomId,
        callback: &Expr,
        func: usize,
        body: &[Stmt],
        locals: &[ir::LocalId],
    ) -> V {
        let (obj, owned) = self.operand(recv, &[callback]);
        let ow = obj.any();
        let mw = self.method_get(ow, method);
        self.check_value(mw);
        let f = V::Any(mw);
        self.hold(f);
        let ctx = self.ctx;
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        // The original builtin, an array, no holes.
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let orig = self.b.ins().load(I64, M_CONST, table, (PIN_ARRAY_LOOPS + kind as i32) * 8);
        let orig = self.mk_ref(TAG_OBJECT, orig);
        let ok = self.b.ins().icmp(IntCC::Equal, mw, orig);
        let (op_, count) = self.array_part(ow, ok, slow);
        let length = self.array_length(op_);
        let dense = self.b.ins().icmp(IntCC::Equal, count, length);
        let fast = self.b.create_block();
        self.b.ins().brif(dense, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let len = count;
        // The result array (`map`: with room for every element), and
        // whether the Array constructor made it itself: then no other code
        // holds it, and an element goes where there is room.
        let (out, fresh) = match kind {
            LoopKind::Map | LoopKind::Filter => {
                let n = if kind == LoopKind::Map { len } else { self.b.ins().iconst(I64, -1) };
                let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 4, 2));
                let addr = self.b.ins().stack_addr(self.ptr, slot, 0);
                let r = self.helper("JS_ArraySpeciesCreateRoom", &[ctx, ow, n, addr]);
                self.check_value(r[0]);
                let made = self.b.ins().stack_load(self.ptr, I32, slot, 0);
                let fresh = self.b.ins().icmp_imm_s(IntCC::NotEqual, made, 0);
                let a = V::Any(r[0]);
                self.hold(a);
                (Some(a), Some(fresh))
            }
            _ => (None, None),
        };
        let head = self.b.create_block();
        let k = self.b.append_block_param(head, I64);
        let n = self.b.append_block_param(head, I64);
        let done = self.b.create_block();
        let dw = self.b.append_block_param(done, I64);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(head, &[BlockArg::Value(zero), BlockArg::Value(zero)]);
        self.b.switch_to_block(head);
        let next = self.b.create_block();
        let nk = self.b.append_block_param(next, I64);
        let nn = self.b.append_block_param(next, I64);
        let k1 = self.b.ins().iadd_imm_s(k, 1);
        // Past the end: the result without a match.
        let ew = match (kind, out) {
            (LoopKind::Map | LoopKind::Filter, Some(a)) => a.any(),
            (LoopKind::Some, _) => self.konst(0, TAG_BOOL).any(),
            (LoopKind::Every, _) => self.konst(1, TAG_BOOL).any(),
            (LoopKind::FindIndex, _) => self.konst(-1, TAG_INT).any(),
            _ => self.konst(0, TAG_UNDEFINED).any(),
        };
        let more = self.b.ins().icmp(IntCC::SignedLessThan, k, len);
        let step = self.b.create_block();
        self.b.ins().brif(more, step, &[], done, &[BlockArg::Value(ew)]);
        self.b.switch_to_block(step);
        // The element: in place while the array is fast, else read.
        let elem = self.b.create_block();
        let el = self.b.append_block_param(elem, I64);
        let now = self.b.ins().uload32(M_HEADER, op_, layout::OBJ_ARRAY_COUNT);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, k, now);
        let hit = self.b.create_block();
        let read = self.b.create_block();
        self.b.set_cold_block(read);
        self.b.ins().brif(inb, hit, &[], read, &[]);
        self.b.switch_to_block(hit);
        let at = self.array_slot(op_, k);
        let vw = self.b.ins().load(I64, M_SLOT, at, 0);
        self.dup(vw);
        self.b.ins().jump(elem, &[BlockArg::Value(vw)]);
        self.b.switch_to_block(read);
        let present = self.done_slot();
        let r = self.helper("JS_TryGetElement", &[ctx, ow, k, present]);
        self.check_value(r[0]);
        let has = self.b.ins().load(I32, M_SLOT, present, 0);
        let absent = self.b.ins().icmp_imm_s(IntCC::Equal, has, 0);
        let got = self.b.create_block();
        // (A hole: skipped.)
        self.b.ins().brif(absent, next, &[BlockArg::Value(k1), BlockArg::Value(n)], got, &[]);
        self.b.switch_to_block(got);
        self.b.ins().jump(elem, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(elem);
        let v = V::Any(el);
        self.hold(v);
        // The callback's result, boxed: its body inline when the element
        // has its parameter's type, else a call of the callback.
        let params = self.env.params[func].clone();
        let ret = self.env.rets[func];
        let res = self.b.create_block();
        let resw = self.b.append_block_param(res, I64);
        let inline = self.b.create_block();
        let typed = params.first() == Some(&Ty::F64);
        if typed {
            let call = self.b.create_block();
            self.b.set_cold_block(call);
            let num = self.is_number(el);
            self.b.ins().brif(num, inline, &[], call, &[]);
            self.b.switch_to_block(call);
            let cb = self.expr(callback);
            self.hold(cb);
            let kd = self.b.ins().fcvt_from_sint(F64, k);
            let kv = self.box_number(kd);
            let cw = cb.any();
            let undef = self.konst(0, TAG_UNDEFINED).any();
            let vals = [cw, undef, el, kv, ow];
            let slot = self.values_slot(&vals);
            let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
            let argc = self.b.ins().iconst(I32, 3);
            let gw = self.call_linked(cw, undef, 3, argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
            self.check_value(gw);
            self.release(cb);
            self.free_v(cb);
            self.b.ins().jump(res, &[BlockArg::Value(gw)]);
        } else {
            self.b.ins().jump(inline, &[]);
        }
        self.b.switch_to_block(inline);
        // `forEach` and `map` need the element no more once the body has
        // it: its parameter takes the loop's reference.
        let moved = !typed && matches!(kind, LoopKind::ForEach | LoopKind::Map) && !params.is_empty();
        for (i, &pt) in params.iter().enumerate() {
            let l = locals[i];
            let a = match (i, pt) {
                (0, Ty::F64) => V::Num(self.number_of(el)),
                (0, _) if moved => {
                    self.release(v);
                    V::Any(el)
                }
                (0, _) => {
                    self.dup(el);
                    V::Any(el)
                }
                (1, Ty::F64) => V::Num(self.b.ins().fcvt_from_sint(F64, k)),
                (1, _) => V::Any(self.mk_int(k)),
                _ => {
                    self.dup(ow);
                    V::Any(ow)
                }
            };
            // The local may be `Any` where its parameter is not.
            let a = if is_js(self.locals[l]) && !matches!(a, V::Any(..)) { V::Any(self.boxed(a)) } else { a };
            self.init_local(l, a);
        }
        let r = self.inline_body(body, locals, ret);
        let bw = self.boxed(r);
        self.b.ins().jump(res, &[BlockArg::Value(bw)]);
        self.b.switch_to_block(res);
        let result = resw;
        match kind {
            LoopKind::ForEach => {
                self.free(result);
                if !moved {
                    self.release(v);
                    self.free_v(v);
                }
                self.b.ins().jump(next, &[BlockArg::Value(k1), BlockArg::Value(n)]);
            }
            LoopKind::Map => {
                let aw = out.unwrap().any();
                if !moved {
                    self.release(v);
                    self.free_v(v);
                }
                let define = self.b.create_block();
                self.b.set_cold_block(define);
                // (Its `length` is the source's already.)
                self.append_fresh(fresh.unwrap(), aw, k, resw, false, next, &[k1, n]);
                self.append_inline(aw, Some(k), resw, true, define);
                self.b.ins().jump(next, &[BlockArg::Value(k1), BlockArg::Value(n)]);
                self.b.switch_to_block(define);
                let r = self.helper("JS_DefineElement", &[ctx, aw, k, resw]);
                self.check_int(r[0]);
                self.b.ins().jump(next, &[BlockArg::Value(k1), BlockArg::Value(n)]);
            }
            LoopKind::Filter => {
                let hit = self.truthy_any(result);
                self.free(result);
                let keep = self.b.create_block();
                let drop = self.b.create_block();
                self.b.ins().brif(hit, keep, &[], drop, &[]);
                self.b.switch_to_block(drop);
                self.free(el);
                self.b.ins().jump(next, &[BlockArg::Value(k1), BlockArg::Value(n)]);
                self.b.switch_to_block(keep);
                let aw = out.unwrap().any();
                // (The definition takes the element.)
                self.release(v);
                let n1 = self.b.ins().iadd_imm_s(n, 1);
                let define = self.b.create_block();
                self.b.set_cold_block(define);
                self.append_fresh(fresh.unwrap(), aw, n, el, true, next, &[k1, n1]);
                self.append_inline(aw, Some(n), el, true, define);
                self.b.ins().jump(next, &[BlockArg::Value(k1), BlockArg::Value(n1)]);
                self.b.switch_to_block(define);
                let r = self.helper("JS_DefineElement", &[ctx, aw, n, el]);
                self.check_int(r[0]);
                self.b.ins().jump(next, &[BlockArg::Value(k1), BlockArg::Value(n1)]);
            }
            LoopKind::Some | LoopKind::Every | LoopKind::Find | LoopKind::FindIndex => {
                let hit = self.truthy_any(result);
                self.free(result);
                let stop = if kind == LoopKind::Every { self.b.ins().bxor_imm_u(hit, 1) } else { hit };
                let found = self.b.create_block();
                let go_on = self.b.create_block();
                self.b.ins().brif(stop, found, &[], go_on, &[]);
                self.b.switch_to_block(go_on);
                self.free(el);
                self.b.ins().jump(next, &[BlockArg::Value(k1), BlockArg::Value(n)]);
                self.b.switch_to_block(found);
                let fw = match kind {
                    LoopKind::Find => el,
                    _ => {
                        self.free(el);
                        match kind {
                            LoopKind::Some => self.konst(1, TAG_BOOL).any(),
                            LoopKind::Every => self.konst(0, TAG_BOOL).any(),
                            _ => self.mk_int(k),
                        }
                    }
                };
                self.b.ins().jump(done, &[BlockArg::Value(fw)]);
                self.release(v);
            }
        }
        self.b.switch_to_block(next);
        self.b.ins().jump(head, &[BlockArg::Value(nk), BlockArg::Value(nn)]);
        self.b.switch_to_block(done);
        if let Some(a) = out {
            // (The result: the array, now the expression's.)
            self.release(a);
        }
        self.b.ins().jump(join, &[BlockArg::Value(dw)]);
        // Otherwise a call, with the callback.
        self.b.switch_to_block(slow);
        let cb = self.expr(callback);
        self.hold(cb);
        let cw = cb.any();
        let vals = [mw, ow, cw];
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, 1);
        let gw = self.call_linked(mw, ow, 1, argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
        self.check_value(gw);
        self.release(cb);
        self.free_v(cb);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        self.release(f);
        self.free_v(f);
        self.drop_operand(obj, owned);
        V::Any(rw)
    }

    /// `recv.method(callback)` where `method` may be the original `sort` and
    /// the callback a native comparator: the engine sorts calling it
    /// directly (`JS_SortNative`); else a call.
    fn array_sort(&mut self, recv: &Expr, method: ir::AtomId, callback: &Expr, func: usize) -> V {
        let (obj, owned) = self.operand(recv, &[callback]);
        let ow = obj.any();
        let mw = self.method_get(ow, method);
        self.check_value(mw);
        let f = V::Any(mw);
        self.hold(f);
        let ctx = self.ctx;
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let orig = self.b.ins().load(I64, M_CONST, table, PIN_ARRAY_SORT * 8);
        let orig = self.mk_ref(TAG_OBJECT, orig);
        let ok = self.b.ins().icmp(IntCC::Equal, mw, orig);
        let fast = self.b.create_block();
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let numbers = self.env.params[func].first() == Some(&Ty::F64);
        let numbers = self.b.ins().iconst(I32, numbers as i64);
        let sorter = self.b.ins().func_addr(self.ptr, self.env.sorters[&func]);
        let r = self.helper("JS_SortNative", &[ctx, ow, sorter, numbers])[0];
        self.check_int(r);
        let sorted = self.b.create_block();
        let done = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
        self.b.ins().brif(done, sorted, &[], slow, &[]);
        self.b.switch_to_block(sorted);
        // (`sort` returns the array.)
        self.dup(ow);
        self.b.ins().jump(join, &[BlockArg::Value(ow)]);
        self.b.switch_to_block(slow);
        let cb = self.expr(callback);
        self.hold(cb);
        let cw = cb.any();
        let vals = [mw, ow, cw];
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, 1);
        let gw = self.call_linked(mw, ow, 1, argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
        self.check_value(gw);
        self.release(cb);
        self.free_v(cb);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        self.release(f);
        self.free_v(f);
        self.drop_operand(obj, owned);
        V::Any(rw)
    }

    /// Whether class `class`'s prototype has native function `func` as its
    /// own data property `method`: through an inline cache of own data
    /// properties, filled on a miss without running any getter.
    fn proto_method_is(&mut self, class: usize, method: ir::AtomId, func: usize) -> Value {
        let ic = self.new_ic();
        let pw = self.table_value(self.env.homes, 2 * class);
        let join = self.b.create_block();
        let fw = self.b.append_block_param(join, I64);
        let miss = self.b.create_block();
        self.b.set_cold_block(miss);
        let not_obj = self.b.create_block();
        let is_obj = self.val_is(pw, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], not_obj, &[]);
        self.b.switch_to_block(obj);
        let pp = self.val_ptr(pw);
        let shape = self.b.ins().load(I64, M_HEADER, pp, layout::OBJ_SHAPE);
        let cached = self.b.ins().load(I64, M_IC, ic, 0);
        let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
        let hit = self.b.create_block();
        self.b.ins().brif(same, hit, &[], miss, &[]);
        self.b.switch_to_block(hit);
        let at = self.ic_slot_addr(pp, ic);
        let vw = self.b.ins().load(I64, M_SLOT, at, 0);
        self.b.ins().jump(join, &[BlockArg::Value(vw)]);
        self.b.switch_to_block(miss);
        let ctx = self.ctx;
        let atom = self.atom(method);
        let r = self.helper("lum_ic_own_data", &[ctx, pw, atom, ic]);
        // (Still referenced by the prototype.)
        self.free(r[0]);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(not_obj);
        let undef = self.konst(0, TAG_UNDEFINED).any();
        self.b.ins().jump(join, &[BlockArg::Value(undef)]);
        self.b.switch_to_block(join);
        self.is_native(fw, func)
    }

    /// `p.a = v` (`(p, t)` borrowed; `v` taken by the property) through an
    /// inline cache: an own writable data property of a cached shape, or a
    /// setter (own or of the prototype), else the engine.
    fn prop_set_ic(&mut self, w: Value, a: ir::AtomId, vw: Value, pred: Vec<Pred>) {
        let ic = self.new_ic();
        let join = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        // An own writable data property of a cached shape, or a setter
        // (own or of the prototype).
        let (shape, kind) = self.ic_shape_hit(w, ic, slow);
        let p = self.val_ptr(w);
        let own = self.b.create_block();
        let proto = self.b.create_block();
        let is_own = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 0);
        self.b.ins().brif(is_own, own, &[], proto, &[]);
        self.b.switch_to_block(proto);
        let pat = self.ic_accessor_slot(p, shape, kind, ic, slow);
        let (setter, sw) = self.accessor(pat, true);
        self.nonnull(setter, slow);
        let ctx = self.ctx;
        // The property's reference is dropped after the call.
        let r = self.call_or_inline(
            &pred,
            sw,
            w,
            &[vw],
            |l| l.values_slot(&[vw]),
            true,
            |l, _| {
                l.dup(vw);
                let r = l.helper("lum_call_setter", &[ctx, setter, w, vw]);
                let failed = l.b.ins().icmp_imm_s(IntCC::SignedLessThan, r[0], 0);
                let exc = l.b.ins().iconst(I64, val_const(TAG_EXCEPTION, 0));
                let undef = l.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0));
                l.b.ins().select(failed, exc, undef)
            },
        );
        self.free(vw);
        self.free(r);
        self.check_value(r);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(own);
        let at = self.ic_slot_addr(p, ic);
        let old = self.b.ins().load(I64, M_SLOT, at, 0);
        self.b.ins().store(M_SLOT, vw, at, 0);
        self.write_barrier(p, vw);
        self.free(old);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(slow);
        let atom = self.atom(a);
        let ctx = self.ctx;
        let r = self.helper("lum_ic_set_proto", &[ctx, w, atom, vw, ic]);
        self.check_int(r[0]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    /// The getter (or `setter`) of the accessor property at `pat`: its
    /// address (0 if none), and it as a JS value.
    fn accessor(&mut self, pat: Value, setter: bool) -> (Value, Value) {
        // (`getset`: two 32-bit references.)
        let r = self.b.ins().uload32(M_SLOT, pat, if setter { 4 } else { 0 });
        let addr = self.b.ins().ishl_imm_u(r, 3);
        let w = self.b.ins().bor_imm_u(r, val_const(TAG_OBJECT, 0));
        (addr, w)
    }

    /// The address of slot `slot` of `p` if it is an object with the shape of
    /// record `record` (see `ir::specialize_fields`), else go to `slow`.
    fn record_slot(&mut self, w: Value, record: usize, slot: u32, slow: Block) -> Value {
        let is_obj = self.val_is(w, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let p = self.val_ptr(w);
        let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
        let table = self.b.ins().symbol_value(self.ptr, self.env.record_shapes);
        let base = (record * RECORD_STRIDE) as i32;
        let want = self.b.ins().load(I64, M_IC, table, base);
        let same = self.b.ins().icmp(IntCC::Equal, shape, want);
        let hit = self.b.create_block();
        let others = self.b.create_block();
        self.b.ins().brif(same, hit, &[], others, &[]);
        // The record's other shapes (a subclass's objects).
        self.b.switch_to_block(others);
        let mut any = None;
        for k in 1..RECORD_SHAPES {
            let want = self.b.ins().load(I64, M_IC, table, base + (k * 8) as i32);
            let same = self.b.ins().icmp(IntCC::Equal, shape, want);
            any = Some(match any {
                None => same,
                Some(a) => self.b.ins().bor(a, same),
            });
        }
        self.b.ins().brif(any.unwrap(), hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        let props = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_PROP);
        self.b.ins().iadd_imm_s(props, (slot as usize * SIZE) as i64)
    }

    /// A speculative region (see `luminous_front::spec`): its guard, then
    /// `fast` where it holds, else `slow`. `fast` is lowered twice: for
    /// when every value checked is a float, reading the fields as doubles
    /// and storing floats as the interpreter's float arithmetic does, and
    /// for when some are int32s, converting what it reads and storing
    /// int32s where the numbers are.
    fn spec_region(&mut self, g: &SpecGuard, fast: &[Stmt], slow: &[Stmt]) {
        let SpecGuard { objs, distinct, fields, nums, .. } = *g;
        let slow_b = self.b.create_block();
        let join = self.b.create_block();
        // Strings: flat ones, read with the builtin `charCodeAt`.
        for &l in g.strs {
            let w = self.use_local(l).any();
            let is_str = self.val_is(w, TAG_STRING);
            let next = self.b.create_block();
            self.b.ins().brif(is_str, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
            let p = self.val_ptr(w);
            let word = self.b.ins().uload32(M_CONST, p, layout::STRING_KIND);
            let kind = self.b.ins().ushr_imm_u(word, 28);
            let kind = self.b.ins().band_imm_u(kind, 3);
            let flat = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 0);
            let next = self.b.create_block();
            self.b.ins().brif(flat, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
        }
        if g.char_code_at {
            let ctx = self.ctx;
            let ok = self.helper("lum_char_code_at_ok", &[ctx])[0];
            let ok = self.b.ins().icmp_imm_s(IntCC::NotEqual, ok, 0);
            let next = self.b.create_block();
            self.b.ins().brif(ok, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
        }
        // Arrays the region appends to (`RegionPush`): as `append_inline`
        // finds them, with `push` the builtin (nothing else in the region
        // changes them: they stay so).
        if !g.appends.is_empty() {
            let ctx = self.ctx;
            let ok = self.helper("lum_array_push_ok", &[ctx])[0];
            let ok = self.b.ins().icmp_imm_s(IntCC::NotEqual, ok, 0);
            let next = self.b.create_block();
            self.b.ins().brif(ok, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
            let shapes = self.b.ins().symbol_value(self.ptr, self.env.shapes);
            for &a in g.appends {
                let w = self.use_local(a).any();
                let is_obj = self.val_is(w, TAG_OBJECT);
                let obj = self.b.create_block();
                self.b.ins().brif(is_obj, obj, &[], slow_b, &[]);
                self.b.switch_to_block(obj);
                let p = self.val_ptr(w);
                let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
                let fresh = self.b.ins().load(I64, M_CONST, shapes, SHAPE_ARRAY * 8);
                let same = self.b.ins().icmp(IntCC::Equal, shape, fresh);
                let word = self.b.ins().uload32(M_HEADER, p, layout::OBJ_FLAGS);
                let bits = self.b.ins().band_imm_u(word, layout::OBJ_APPEND_MASK);
                let fast = self.b.ins().icmp_imm_s(IntCC::Equal, bits, layout::OBJ_APPEND_FLAGS);
                let ok = self.b.ins().band(same, fast);
                let array = self.b.create_block();
                self.b.ins().brif(ok, array, &[], slow_b, &[]);
                self.b.switch_to_block(array);
                let count = self.b.ins().uload32(M_HEADER, p, layout::OBJ_ARRAY_COUNT);
                // (`length`: an int value, the count.)
                let prop = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_PROP);
                let len = self.b.ins().load(I64, M_SLOT, prop, 0);
                let want = self.mk_int(count);
                let ok = self.b.ins().icmp(IntCC::Equal, len, want);
                let next = self.b.create_block();
                self.b.ins().brif(ok, next, &[], slow_b, &[]);
                self.b.switch_to_block(next);
            }
        }
        // Typed arrays the region writes: not of an immutable buffer.
        for &a in g.typed_writes {
            let w = self.use_local(a).any();
            let p = self.val_ptr(w);
            let ta = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_TYPED_ARRAY);
            let buf = self.b.ins().load(I64, M_HEADER, ta, layout::TA_BUFFER);
            let abuf = self.b.ins().load(I64, M_HEADER, buf, layout::OBJ_ARRAY_BUFFER);
            let immutable = self.b.ins().uload8(I32, M_HEADER, abuf, layout::ABUF_IMMUTABLE);
            let next = self.b.create_block();
            self.b.ins().brif(immutable, slow_b, &[], next, &[]);
            self.b.switch_to_block(next);
        }
        let table = self.b.ins().symbol_value(self.ptr, self.env.record_shapes);
        let mut ptrs = Vec::new();
        // Arrays whose elements the region's loop reads: dense fast arrays
        // (no code runs in the region: they stay so); typed arrays with a
        // pristine shape (`length` their element count, which nothing in
        // the region changes).
        for &a in g.arrays {
            let w = self.use_local(a).any();
            if let Ty::Arr(kind) = self.locals[a] {
                let p = self.val_ptr(w);
                let k = self.b.ins().iconst(I32, kind as i64);
                let ok = self.pristine_shape(p, k);
                let next = self.b.create_block();
                self.b.ins().brif(ok, next, &[], slow_b, &[]);
                self.b.switch_to_block(next);
                continue;
            }
            let yes = self.b.ins().iconst(I8, 1);
            let (p, count) = self.array_part(w, yes, slow_b);
            // (`length`: an int value, the count.)
            let prop = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_PROP);
            let len = self.b.ins().load(I64, M_SLOT, prop, 0);
            let want = self.mk_int(count);
            let ok = self.b.ins().icmp(IntCC::Equal, len, want);
            let next = self.b.create_block();
            self.b.ins().brif(ok, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
        }
        // Records whose objects (elements) have a class's prototype: their
        // shape's (once known; until then no element has it).
        for &(r, class) in g.protos {
            let want = self.b.ins().load(I64, M_IC, table, (r * RECORD_STRIDE) as i32);
            let known = self.b.ins().icmp_imm_s(IntCC::NotEqual, want, 0);
            let check = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(known, check, &[], next, &[]);
            self.b.switch_to_block(check);
            let proto = self.b.ins().load(I64, M_HEADER, want, layout::SHAPE_PROTO);
            let proto = self.mk_ref(TAG_OBJECT, proto);
            let hw = self.table_value(self.env.homes, 2 * class);
            let ok = self.b.ins().icmp(IntCC::Equal, proto, hw);
            self.b.ins().brif(ok, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
        }
        // Class bindings read by the region (whose TDZ checks it skips).
        for &k in g.cells {
            let base = self.b.ins().symbol_value(self.ptr, self.env.cells);
            let w = self.b.ins().load(I64, M_SLOT, base, (k * SIZE) as i32);
            let set = self.b.ins().icmp_imm_s(IntCC::NotEqual, w, val_const(TAG_UNINITIALIZED, 0));
            let next = self.b.create_block();
            self.b.ins().brif(set, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
        }
        // The objects, then those their fields hold: of their records.
        for &(o, r) in objs {
            let w = if o == ir::THIS_LOCAL { self.this.expect("`this` in a region") } else { self.use_local(o).any() };
            let (p, props) = self.guard_record(w, r, table, slow_b);
            ptrs.push((o, p, props));
        }
        for &(o, of, slot, r) in g.derived {
            let props = ptrs.iter().find(|x| x.0 == of).expect("guarded holder").2;
            let off = (slot as usize * SIZE) as i32;
            let w = self.b.ins().load(I64, M_SLOT, props, off);
            let (p, props) = self.guard_record(w, r, table, slow_b);
            ptrs.push((o, p, props));
        }
        // Methods run inline: the receiver's prototype is the class's, which
        // still has the method.
        for &(o, class, method, func) in g.methods {
            if let Some(o) = o {
                let p = ptrs.iter().find(|x| x.0 == o).expect("guarded receiver").1;
                let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
                let proto = self.b.ins().load(I64, M_HEADER, shape, layout::SHAPE_PROTO);
                let proto = self.mk_ref(TAG_OBJECT, proto);
                let hw = self.table_value(self.env.homes, 2 * class);
                let ok = self.b.ins().icmp(IntCC::Equal, proto, hw);
                let next = self.b.create_block();
                self.b.ins().brif(ok, next, &[], slow_b, &[]);
                self.b.switch_to_block(next);
            }
            let ok = self.proto_method_is(class, method, func);
            let next = self.b.create_block();
            self.b.ins().brif(ok, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
        }
        if distinct {
            let mut same = self.b.ins().iconst(I8, 0);
            for i in 0..ptrs.len() {
                for j in i + 1..ptrs.len() {
                    let eq = self.b.ins().icmp(IntCC::Equal, ptrs[i].1, ptrs[j].1);
                    same = self.b.ins().bor(same, eq);
                }
            }
            let next = self.b.create_block();
            self.b.ins().brif(same, slow_b, &[], next, &[]);
            self.b.switch_to_block(next);
        }
        let props_of = |o: ir::LocalId| ptrs.iter().find(|x| x.0 == o).expect("guarded object").2;
        let field = |l: &mut Self, (o, slot): (ir::LocalId, u32), flags: MemFlags| {
            l.b.ins().load(I64, flags, props_of(o), (slot as usize * SIZE) as i32)
        };
        let locals: Vec<Value> = nums.iter().map(|&(from, _)| self.use_local(from).any()).collect();
        // The locals hold numbers (int32s or floats), converted once.
        if !locals.is_empty() {
            let numbers = self.all_numbers(&locals);
            let next = self.b.create_block();
            self.b.ins().brif(numbers, next, &[], slow_b, &[]);
            self.b.switch_to_block(next);
        }
        let values: Vec<Value> = locals.iter().map(|&w| self.number_tagged(w)).collect();
        // Fields holding floats: each compared on its own (the values are
        // not kept for the int code, which reads them again).
        let floats = self.b.create_block();
        let check = self.b.create_block();
        for &(o, slot) in fields {
            let is_float = val::slot_is_float(&mut self.b, M_SLOT.volatile(), props_of(o), (slot as usize * SIZE) as i32);
            let next = self.b.create_block();
            self.b.ins().brif(is_float, next, &[], check, &[]);
            self.b.switch_to_block(next);
        }
        self.b.ins().jump(floats, &[]);
        for &(o, _, props) in &ptrs {
            self.spec_src.insert(o, props);
        }

        self.b.switch_to_block(floats);
        for (&(_, to), &d) in nums.iter().zip(&values) {
            self.def_local(to, V::Num(d));
        }
        self.spec_floats = true;
        self.stmts(fast);
        if !self.is_filled() {
            self.b.ins().jump(join, &[]);
        }

        // Else int32s and floats.
        self.b.switch_to_block(check);
        let words: Vec<Value> = fields.iter().map(|&f| field(self, f, M_SLOT)).collect();
        let numbers = self.all_numbers(&words);
        let mixed = self.b.create_block();
        self.b.ins().brif(numbers, mixed, &[], slow_b, &[]);
        self.b.switch_to_block(mixed);
        for (&(_, to), &d) in nums.iter().zip(&values) {
            self.def_local(to, V::Num(d));
        }
        self.spec_floats = false;
        self.stmts(fast);
        if !self.is_filled() {
            self.b.ins().jump(join, &[]);
        }

        self.b.switch_to_block(slow_b);
        self.stmts(slow);
        if !self.is_filled() {
            self.b.ins().jump(join, &[]);
        }
        self.b.switch_to_block(join);
    }

    /// The check that starts an iteration of a region's loop (see
    /// `ExprKind::ElemGuard`), then `then`; where it fails, `bail` (which
    /// leaves the loop).
    #[allow(clippy::too_many_arguments)]
    fn elem_guard(
        &mut self,
        elems: &[(ir::LocalId, ir::LocalId, ir::LocalId, usize)],
        nums: &[(ir::LocalId, ir::LocalId, ir::LocalId)],
        bounds: &[(ir::LocalId, ir::LocalId)],
        derived: &[(ir::LocalId, ir::LocalId, u32, usize)],
        fields: &[(ir::LocalId, u32)],
        distinct: &[ir::LocalId],
        then: &[Stmt],
        bail: &[Stmt],
    ) {
        let fail = self.b.create_block();
        self.b.set_cold_block(fail);
        let join = self.b.create_block();
        let table = self.b.ins().symbol_value(self.ptr, self.env.record_shapes);
        let mut ptrs: Vec<(ir::LocalId, Value)> = Vec::new();
        for &(o, arr, idx, record) in elems {
            let w = self.guard_elem(arr, idx, fail);
            let (_, props) = self.guard_record(w, record, table, fail);
            ptrs.push((o, props));
        }
        // Indexes inside the typed arrays the iteration reads.
        for &(arr, idx) in bounds {
            let aw = self.use_local(arr).any();
            let ap = self.val_ptr(aw);
            self.guard_index(ap, idx, fail);
        }
        // Elements read as numbers: int32s or floats.
        for &(to, arr, idx) in nums {
            let w = self.guard_elem(arr, idx, fail);
            let number = self.is_number(w);
            let next = self.b.create_block();
            self.b.ins().brif(number, next, &[], fail, &[]);
            self.b.switch_to_block(next);
            let d = self.number_tagged(w);
            self.def_local(to, V::Num(d));
        }
        for &(o, of, slot, r) in derived {
            let props = ptrs.iter().find(|x| x.0 == of).expect("guarded holder").1;
            let off = (slot as usize * SIZE) as i32;
            let w = self.b.ins().load(I64, M_SLOT, props, off);
            let (_, props) = self.guard_record(w, r, table, fail);
            ptrs.push((o, props));
        }
        // Their fields hold numbers: floats, or int32s too.
        // (The second test only where the first fails.)
        let slots: Vec<(Value, i32)> = fields
            .iter()
            .map(|&(o, slot)| (ptrs.iter().find(|x| x.0 == o).expect("guarded element").1, (slot as usize * SIZE) as i32))
            .collect();
        let mut floats = self.b.ins().iconst(I8, 1);
        for &(props, off) in &slots {
            let f = val::slot_is_float(&mut self.b, M_SLOT, props, off);
            floats = self.b.ins().band(floats, f);
        }
        // Not the region's other objects (compared by property arrays, which
        // objects do not share).
        if !distinct.is_empty() {
            let mut same = self.b.ins().iconst(I8, 0);
            for &(_, props) in &ptrs {
                for d in distinct {
                    let eq = self.b.ins().icmp(IntCC::Equal, props, self.spec_src[d]);
                    same = self.b.ins().bor(same, eq);
                }
            }
            let next = self.b.create_block();
            self.b.ins().brif(same, fail, &[], next, &[]);
            self.b.switch_to_block(next);
        }
        let mine: Vec<ir::LocalId> = ptrs.iter().map(|p| p.0).collect();
        for (o, props) in ptrs {
            self.spec_src.insert(o, props);
            self.spec_elems.insert(o);
        }
        // The iteration where the fields hold floats, else where some hold
        // int32s.
        let float_b = self.b.create_block();
        let check = self.b.create_block();
        let mixed_b = self.b.create_block();
        self.b.ins().brif(floats, float_b, &[], check, &[]);
        self.b.switch_to_block(check);
        let words: Vec<Value> = slots.iter().map(|&(props, off)| self.b.ins().load(I64, M_SLOT, props, off)).collect();
        let numbers = self.all_numbers(&words);
        self.b.ins().brif(numbers, mixed_b, &[], fail, &[]);
        for (block, f) in [(float_b, true), (mixed_b, false)] {
            self.b.switch_to_block(block);
            for &o in &mine {
                if f {
                    self.spec_float_elems.insert(o);
                } else {
                    self.spec_float_elems.remove(&o);
                }
            }
            self.stmts(then);
            if !self.is_filled() {
                self.b.ins().jump(join, &[]);
            }
        }
        for o in &mine {
            self.spec_float_elems.remove(o);
        }
        self.b.switch_to_block(fail);
        self.stmts(bail);
        if !self.is_filled() {
            self.b.ins().jump(join, &[]);
        }
        self.b.switch_to_block(join);
    }

    /// An iteration guard's element `idx` of the dense fast array in local
    /// `arr` (a region guard's `arrays`), the index an integer below its
    /// element count, else to `fail`.
    fn guard_elem(&mut self, arr: ir::LocalId, idx: ir::LocalId, fail: Block) -> Value {
        let aw = self.use_local(arr).any();
        let ap = self.val_ptr(aw);
        let i = self.guard_index(ap, idx, fail);
        let at = self.array_slot(ap, i);
        self.b.ins().load(I64, M_SLOT, at, 0)
    }

    /// The value of index local `idx` as an integer below the element count
    /// of array (or typed array) `ap`, else to `fail`.
    fn guard_index(&mut self, ap: Value, idx: ir::LocalId, fail: Block) -> Value {
        let count = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_ARRAY_COUNT);
        let (i, exact) = match self.use_local(idx) {
            V::Num(d) => match self.int_of.get(&d) {
                Some(&i) => (i, None),
                None => {
                    let i = self.b.ins().fcvt_to_sint_sat(I64, d);
                    let back = self.b.ins().fcvt_from_sint(F64, i);
                    (i, Some(self.b.ins().fcmp(FloatCC::Equal, back, d)))
                }
            },
            V::Int(i) => (i, None),
            other => unreachable!("index {other:?}"),
        };
        let inside = self.b.ins().icmp(IntCC::UnsignedLessThan, i, count);
        let ok = match exact {
            Some(e) => self.b.ins().band(inside, e),
            None => inside,
        };
        let next = self.b.create_block();
        self.b.ins().brif(ok, next, &[], fail, &[]);
        self.b.switch_to_block(next);
        i
    }

    /// A region guard's check that `w` is an object of `record`'s shape
    /// (`table`: the records' shapes), else to `slow`: its address and
    /// property array.
    fn guard_record(&mut self, w: Value, record: usize, table: Value, slow: Block) -> (Value, Value) {
        let is_obj = self.val_is(w, TAG_OBJECT);
        let next = self.b.create_block();
        self.b.ins().brif(is_obj, next, &[], slow, &[]);
        self.b.switch_to_block(next);
        let p = self.val_ptr(w);
        let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
        // (Regions take objects of the shape learned first.)
        let want = self.b.ins().load(I64, M_IC, table, (record * RECORD_STRIDE) as i32);
        let same = self.b.ins().icmp(IntCC::Equal, shape, want);
        let next = self.b.create_block();
        self.b.ins().brif(same, next, &[], slow, &[]);
        self.b.switch_to_block(next);
        (p, self.b.ins().load(I64, M_HEADER, p, layout::OBJ_PROP))
    }

    /// Whether the values `ws` are all numbers (int32s or floats).
    fn all_numbers(&mut self, ws: &[Value]) -> Value {
        let mut all = self.b.ins().iconst(I8, 1);
        for &w in ws {
            let n = self.is_number(w);
            all = self.b.ins().band(all, n);
        }
        all
    }

    /// `charCodeAt` of flat string `p` at position `d` (a number): the code
    /// unit, or NaN outside the string.
    fn flat_char_code(&mut self, p: Value, d: Value) -> Value {
        // ToIntegerOrInfinity (NaN: 0), saturating: outside when negative.
        let idx = match self.int_of.get(&d) {
            Some(&i) => i,
            None => self.b.ins().fcvt_to_sint_sat(I64, d),
        };
        // (A string's length and kind never change: invariant loads.)
        let head = self.b.ins().uload32(M_CONST, p, layout::STRING_LEN);
        let len = self.b.ins().band_imm_u(head, 0x7fff_ffff);
        let inside = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, len);
        let load = self.b.create_block();
        let join = self.b.create_block();
        let r = self.b.append_block_param(join, F64);
        let ri = self.b.append_block_param(join, I64);
        let nan = self.b.ins().f64const(f64::NAN);
        let none = self.b.ins().iconst(I64, -1);
        self.b.ins().brif(inside, load, &[], join, &[BlockArg::Value(nan), BlockArg::Value(none)]);
        self.b.switch_to_block(load);
        // A 16-bit load at the character: an 8-bit string ends with a NUL,
        // so it stays inside; masked to 8 bits for those.
        let wide = self.b.ins().ushr_imm_u(head, 31);
        let off = self.b.ins().ishl(idx, wide);
        let base = self.b.ins().iadd_imm_s(p, layout::STRING_DATA as i64);
        let at = self.b.ins().iadd(base, off);
        let c = self.b.ins().uload16(I32, M_CONST, at, 0);
        let shift = self.b.ins().ishl_imm_u(wide, 3);
        let shift = self.b.ins().ireduce(I32, shift);
        let m8 = self.b.ins().iconst(I32, 0xff);
        let mask = self.b.ins().ishl(m8, shift);
        let mask = self.b.ins().bor_imm_u(mask, 0xff);
        let c = self.b.ins().band(c, mask);
        let c = self.b.ins().uextend(I64, c);
        let cd = self.b.ins().fcvt_from_sint(F64, c);
        self.b.ins().jump(join, &[BlockArg::Value(cd), BlockArg::Value(c)]);
        self.b.switch_to_block(join);
        // (Its code as an integer where the index is inside, else NaN (-1
        // as the integer): compared with integers as integers.)
        self.rem_of.insert(r, RemInt { int: ri, ok: inside, nan: true, small: true, none: Some(-1) });
        r
    }

    /// The double of a JS value known to be an int32 or a float.
    fn number_tagged(&mut self, w: Value) -> Value {
        let is_int = self.val_is(w, TAG_INT);
        let i = self.val_int(w);
        let fi = self.b.ins().fcvt_from_sint(F64, i);
        let ff = self.val_f64(w);
        self.b.ins().select(is_int, fi, ff)
    }

    /// While record `record` has room for another shape (and learning has
    /// not failed too often): learn one from `w`.
    fn record_learn(&mut self, w: Value, record: usize) {
        let table = self.b.ins().symbol_value(self.ptr, self.env.record_shapes);
        let base = (record * RECORD_STRIDE) as i32;
        let last = self.b.ins().load(I64, M_IC, table, base + ((RECORD_SHAPES - 1) * 8) as i32);
        let tries = self.b.ins().load(I64, M_IC, table, base + (RECORD_SHAPES * 8) as i32);
        let room = self.b.ins().icmp_imm_s(IntCC::Equal, last, 0);
        let few = self.b.ins().icmp_imm_s(IntCC::UnsignedLessThan, tries, RECORD_TRIES);
        let unknown = self.b.ins().band(room, few);
        let learn = self.b.create_block();
        let after = self.b.create_block();
        self.b.set_cold_block(learn);
        self.b.ins().brif(unknown, learn, &[], after, &[]);
        self.b.switch_to_block(learn);
        let ctx = self.ctx;
        let r = self.b.ins().iconst(I32, record as i64);
        self.helper("lum_record_learn", &[ctx, w, r]);
        self.b.ins().jump(after, &[]);
        self.b.switch_to_block(after);
    }

    /// Call `f` (held) with `this` (borrowed) and `args`; frees `f`.
    fn call_values(&mut self, f: V, this: V, args: &[Expr], pred: Vec<Pred>) -> V {
        if args.iter().any(|a| lazy_part(a).is_some()) {
            return self.call_lazy(f, this, args, pred);
        }
        let held = self.held_args(args);
        self.call_held(f, this, held, pred)
    }

    /// `call_values` with `Lazy` arguments: evaluated only for the call (an
    /// inline body gets `undefined` for their parameters).
    fn call_lazy(&mut self, f: V, this: V, args: &[Expr], pred: Vec<Pred>) -> V {
        let mut held = Vec::new();
        let undef = self.konst(0, TAG_UNDEFINED).any();
        let mut vals = vec![f.any(), this.any()];
        for a in args {
            if let Some((first, _)) = lazy_part(a) {
                for x in first {
                    self.effect(x);
                }
                vals.push(undef);
            } else {
                let v = self.expr(a);
                self.hold(v);
                held.push(v);
                vals.push(v.any());
            }
        }
        let (stack, slot) = self.values_slot_raw(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, args.len() as i64);
        let ctx = self.ctx;
        let js_args: Vec<Value> = vals[2..].to_vec();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let entry = if pred.is_empty() { None } else { Some(self.fn_entry(f.any())) };
        for p in &pred {
            let Some(ok) = self.pred_guard(p, entry.expect("predictions"), &js_args) else { continue };
            let inline = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(ok, inline, &[], next, &[]);
            self.b.switch_to_block(inline);
            let iw = self.inline_method(p, this.any(), &js_args);
            self.b.ins().jump(join, &[BlockArg::Value(iw)]);
            self.b.switch_to_block(next);
        }
        let mut lazy = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if let Some((_, x)) = lazy_part(a) {
                let v = self.expr(x);
                self.hold(v);
                let w = v.any();
                self.b.ins().stack_store(self.ptr, w, stack, ((2 + i) * SIZE) as i32);
                lazy.push(v);
            }
        }
        let gw = self.call_linked(f.any(), this.any(), args.len(), argv, false, |l| {
            l.helper("lum_call", &[ctx, slot, argc, argv])[0]
        });
        for v in lazy.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        self.check_value(rw);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.release(f);
        self.free_v(f);
        V::Any(rw)
    }

    /// Evaluate call arguments, holding them.
    fn held_args(&mut self, args: &[Expr]) -> Vec<V> {
        args.iter()
            .map(|a| {
                let v = self.expr(a);
                self.hold(v);
                v
            })
            .collect()
    }

    /// `call_values` with the arguments evaluated (and held), which it frees.
    fn call_held(&mut self, f: V, this: V, held: Vec<V>, pred: Vec<Pred>) -> V {
        let mut vals = vec![f.any(), this.any()];
        vals.extend(held.iter().map(|v| v.any()));
        let n = held.len();
        let ctx = self.ctx;
        let js_args: Vec<Value> = vals[2..].to_vec();
        // (The function, `this` and the arguments in a stack slot, only on
        // the path that calls.)
        let make_argv = |l: &mut Self| {
            let slot = l.values_slot(&vals);
            l.b.ins().iadd_imm_s(slot, 2 * SIZE as i64)
        };
        let rw = self.call_or_inline(&pred, f.any(), this.any(), &js_args, make_argv, false, |l, argv| {
            let slot = l.b.ins().iadd_imm_s(argv, -2 * SIZE as i64);
            let argc = l.b.ins().iconst(I32, n as i64);
            l.helper("lum_call", &[ctx, slot, argc, argv])[0]
        });
        self.check_value(rw);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.release(f);
        self.free_v(f);
        V::Any(rw)
    }

    /// `this.charCodeAt(args)` with `f` (held) the method read: inline when
    /// it is the original builtin, `this` a flat string and the position a
    /// number, else called. Frees `f`.
    /// `f_owned`: `f` is borrowed unless this (an `I8`) is set, rather than
    /// held (see `prop_get_inner`).
    fn char_code_at(&mut self, f: V, f_owned: Option<Value>, this: V, args: &[Expr]) -> V {
        // (A number position stays unboxed: boxed only for a call.)
        let held: Vec<V> = args
            .iter()
            .map(|a| {
                let v = self.unboxed_operand(a);
                self.hold(v);
                v
            })
            .collect();
        let fw = f.any();
        let sw = this.any();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let is_str = self.val_is(sw, TAG_STRING);
        let is_obj = self.val_is(fw, TAG_OBJECT);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let orig = self.b.ins().load(I64, M_CONST, table, PIN_CHAR_CODE_AT * 8);
        let orig = self.mk_ref(TAG_OBJECT, orig);
        let same = self.b.ins().icmp(IntCC::Equal, fw, orig);
        let ok = self.b.ins().band(is_str, is_obj);
        let ok = self.b.ins().band(ok, same);
        let string = self.b.create_block();
        self.b.ins().brif(ok, string, &[], slow, &[]);
        self.b.switch_to_block(string);
        let sp = self.val_ptr(sw);
        // ToIntegerOrInfinity of the position (NaN and undefined: 0).
        let (idx, idx_ok) = match held.first() {
            None => (self.b.ins().iconst(I64, 0), None),
            Some(&V::Num(d)) => match self.int_of.get(&d) {
                Some(&i) => (i, None),
                None => (self.b.ins().fcvt_to_sint_sat(I64, d), None),
            },
            Some(&V::Int(i)) => (i, None),
            Some(&V::Bool(b)) => (self.b.ins().uextend(I64, b), None),
            Some(&V::Any(w)) => {
                let i = self.val_int(w);
                let d = self.val_f64(w);
                let di = self.b.ins().fcvt_to_sint_sat(I64, d);
                let is_int = self.val_is(w, TAG_INT);
                (self.b.ins().select(is_int, i, di), Some(self.is_number(w)))
            }
            Some(&V::Void) => unreachable!("void argument"),
        };
        // (A string's length and kind never change: invariant loads.)
        let word = self.b.ins().uload32(M_CONST, sp, layout::STRING_KIND);
        let kind = self.b.ins().ushr_imm_u(word, 28);
        let kind = self.b.ins().band_imm_u(kind, 3);
        let flat = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 0);
        let flat = match idx_ok {
            Some(ok) => self.b.ins().band(flat, ok),
            None => flat,
        };
        let chars = self.b.create_block();
        self.b.ins().brif(flat, chars, &[], slow, &[]);
        self.b.switch_to_block(chars);
        // (`uload32` zero-extends to an `I64`.)
        let head = self.b.ins().uload32(M_CONST, sp, layout::STRING_LEN);
        let len = self.b.ins().band_imm_u(head, 0x7fff_ffff);
        let inside = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, len);
        let load = self.b.create_block();
        let nan = self.b.create_block();
        self.b.ins().brif(inside, load, &[], nan, &[]);
        self.b.switch_to_block(load);
        // A 16-bit load at the character: an 8-bit string ends with a NUL,
        // so it stays inside; masked to 8 bits for those.
        let wide = self.b.ins().ushr_imm_u(head, 31);
        let off = self.b.ins().ishl(idx, wide);
        let base = self.b.ins().iadd_imm_s(sp, layout::STRING_DATA as i64);
        let at = self.b.ins().iadd(base, off);
        let c = self.b.ins().uload16(I32, M_CONST, at, 0);
        let shift = self.b.ins().ishl_imm_u(wide, 3);
        let shift = self.b.ins().ireduce(I32, shift);
        let m8 = self.b.ins().iconst(I32, 0xff);
        let mask = self.b.ins().ishl(m8, shift);
        let mask = self.b.ins().bor_imm_u(mask, 0xff);
        let c = self.b.ins().band(c, mask);
        let c = self.b.ins().uextend(I64, c);
        let c = self.mk_int(c);
        self.b.ins().jump(join, &[BlockArg::Value(c)]);
        self.b.switch_to_block(nan);
        let nan_v = self.b.ins().iconst(I64, CANONICAL_NAN_BITS);
        self.b.ins().jump(join, &[BlockArg::Value(nan_v)]);
        self.b.switch_to_block(slow);
        let mut vals = vec![fw, sw];
        for &v in &held {
            let b = self.boxed(v);
            vals.push(b);
        }
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, held.len() as i64);
        let ctx = self.ctx;
        let js_args: Vec<Value> = vals[2..].to_vec();
        // (A borrowed method is held while it runs.)
        let borrowed = f_owned.is_some();
        if borrowed {
            self.dup(fw);
        }
        let gw =
            self.call_linked(fw, sw, js_args.len(), argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
        if borrowed {
            self.free(fw);
        }
        self.check_value_method(gw, f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.drop_method(f, f_owned);
        V::Any(rw)
    }

    /// `this.<method>(args)` for a `Method::StringFn` method `k`, with `f`
    /// (held) the method read: when it is the original builtin and `this` a
    /// string, the engine's entry runs it without a call frame
    /// (`JS_CallStringFn`); else it is called. Frees `f`.
    /// `f_owned`: as for `char_code_at`.
    fn string_fn(&mut self, k: i32, f: V, f_owned: Option<Value>, this: V, args: &[Expr]) -> V {
        let held: Vec<V> = args
            .iter()
            .map(|a| {
                let v = self.expr(a);
                self.hold(v);
                v
            })
            .collect();
        let fw = f.any();
        let sw = this.any();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let is_str = self.val_is(sw, TAG_STRING);
        let is_obj = self.val_is(fw, TAG_OBJECT);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let orig = self.b.ins().load(I64, M_CONST, table, (PIN_STRING_FNS + k) * 8);
        let orig = self.mk_ref(TAG_OBJECT, orig);
        let same = self.b.ins().icmp(IntCC::Equal, fw, orig);
        let ok = self.b.ins().band(is_str, is_obj);
        let ok = self.b.ins().band(ok, same);
        let direct = self.b.create_block();
        self.b.ins().brif(ok, direct, &[], slow, &[]);
        self.b.switch_to_block(direct);
        let undef = self.konst(0, TAG_UNDEFINED).any();
        let mut vals: Vec<Value> = held.iter().map(|&v| self.boxed(v)).collect();
        vals.resize(2, undef);
        let (_, argv) = self.values_slot_raw(&vals);
        let ctx = self.ctx;
        let which = self.b.ins().iconst(I32, k as i64);
        let argc = self.b.ins().iconst(I32, held.len() as i64);
        let r = self.helper("JS_CallStringFn", &[ctx, sw, which, argc, argv]);
        self.check_value_method(r[0], f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(slow);
        let mut vals = vec![fw, sw];
        for &v in &held {
            let b = self.boxed(v);
            vals.push(b);
        }
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, held.len() as i64);
        let js_args: Vec<Value> = vals[2..].to_vec();
        // (A borrowed method is held while it runs.)
        let borrowed = f_owned.is_some();
        if borrowed {
            self.dup(fw);
        }
        let gw =
            self.call_linked(fw, sw, js_args.len(), argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
        if borrowed {
            self.free(fw);
        }
        self.check_value_method(gw, f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.drop_method(f, f_owned);
        V::Any(rw)
    }

    /// `JSON.stringify(v)` with `f` (held) the method read: while it is the
    /// original builtin, the engine's fast path for plain data without a
    /// call frame (`JS_JSONStringifyData`); else, or when `v` is not such
    /// data, called. Frees `f`.
    fn json_stringify(&mut self, f: V, f_owned: Option<Value>, this: V, arg: &Expr) -> V {
        let held = self.expr(arg);
        self.hold(held);
        let vw = self.boxed(held);
        let fw = f.any();
        let sw = this.any();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let orig = self.b.ins().load(I64, M_CONST, table, PIN_JSON_STRINGIFY * 8);
        let orig = self.mk_ref(TAG_OBJECT, orig);
        let same = self.b.ins().icmp(IntCC::Equal, fw, orig);
        let direct = self.b.create_block();
        self.b.ins().brif(same, direct, &[], slow, &[]);
        self.b.switch_to_block(direct);
        let ctx = self.ctx;
        // (An object of an `aot type`, or `undefined`, also as a JS value
        // for the call: written from its fields.)
        let typed = |t: Ty| matches!(t, Ty::Struct(_) | Ty::StructOrUndef(_));
        let is_struct = typed(arg.ty)
            || match &arg.kind {
                ExprKind::StructArrGet(..)
                | ExprKind::StructNew { .. }
                | ExprKind::NonNull(..)
                | ExprKind::NonNullSet(..) => true,
                ExprKind::Local(l) => typed(self.locals[*l]),
                _ => false,
            };
        let entry = if is_struct { "lum_struct_stringify" } else { "JS_JSONStringifyData" };
        let r = self.helper(entry, &[ctx, vw]);
        let not_data = self.b.ins().icmp_imm_s(IntCC::Equal, r[0], val_const(TAG_UNINITIALIZED, 0));
        let done = self.b.create_block();
        self.b.ins().brif(not_data, slow, &[], done, &[]);
        self.b.switch_to_block(done);
        self.check_value_method(r[0], f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(slow);
        let vals = vec![fw, sw, vw];
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, 1);
        // (A borrowed method is held while it runs.)
        let borrowed = f_owned.is_some();
        if borrowed {
            self.dup(fw);
        }
        let gw = self.call_linked(fw, sw, 1, argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
        if borrowed {
            self.free(fw);
        }
        self.check_value_method(gw, f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        self.release(held);
        self.free_v(held);
        self.drop_method(f, f_owned);
        V::Any(rw)
    }

    /// `this.get(k)`, `set(k, v)`, `has(k)`, `add(k)` with `f` (held) the
    /// method read: directly (`JS_CallMapFn`) while it is the original
    /// builtin and `this` a Map (a Set for `add`, either for `has`), else
    /// called. Frees `f`.
    fn map_fn(&mut self, k: i32, f: V, f_owned: Option<Value>, this: V, args: &[Expr]) -> V {
        let held: Vec<V> = args
            .iter()
            .map(|a| {
                let v = self.expr(a);
                self.hold(v);
                v
            })
            .collect();
        let fw = f.any();
        let sw = this.any();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let is_obj = self.val_is(sw, TAG_OBJECT);
        let f_obj = self.val_is(fw, TAG_OBJECT);
        let both = self.b.ins().band(is_obj, f_obj);
        let objs = self.b.create_block();
        self.b.ins().brif(both, objs, &[], slow, &[]);
        self.b.switch_to_block(objs);
        let sp = self.val_ptr(sw);
        let class = self.b.ins().uload16(I32, M_HEADER, sp, layout::OBJ_CLASS_ID);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let fits = |l: &mut Self, which: i32, cls: i64| {
            let orig = l.b.ins().load(I64, M_CONST, table, (PIN_MAP_FNS + which) * 8);
            let orig = l.mk_ref(TAG_OBJECT, orig);
            let same = l.b.ins().icmp(IntCC::Equal, fw, orig);
            let of = l.b.ins().icmp_imm_s(IntCC::Equal, class, cls);
            l.b.ins().band(same, of)
        };
        let (ok, which) = match k {
            // (A Map's `has`, or a Set's.)
            2 => {
                let map = fits(self, 2, layout::CLASS_MAP);
                let set = fits(self, 4, layout::CLASS_SET);
                let four = self.b.ins().iconst(I32, 4);
                let two = self.b.ins().iconst(I32, 2);
                let which = self.b.ins().select(set, four, two);
                (self.b.ins().bor(map, set), which)
            }
            _ => {
                let cls = if k == 3 { layout::CLASS_SET } else { layout::CLASS_MAP };
                (fits(self, k, cls), self.b.ins().iconst(I32, k as i64))
            }
        };
        let direct = self.b.create_block();
        self.b.ins().brif(ok, direct, &[], slow, &[]);
        self.b.switch_to_block(direct);
        let undef = self.konst(0, TAG_UNDEFINED).any();
        let mut vals: Vec<Value> = held.iter().map(|&v| self.boxed(v)).collect();
        vals.resize(2, undef);
        let (_, argv) = self.values_slot_raw(&vals);
        let ctx = self.ctx;
        let argc = self.b.ins().iconst(I32, held.len() as i64);
        let r = self.helper("JS_CallMapFn", &[ctx, sw, which, argc, argv]);
        self.check_value_method(r[0], f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(slow);
        let mut vals = vec![fw, sw];
        for &v in &held {
            let b = self.boxed(v);
            vals.push(b);
        }
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, held.len() as i64);
        let n = held.len();
        // (A borrowed method is held while it runs.)
        let borrowed = f_owned.is_some();
        if borrowed {
            self.dup(fw);
        }
        let gw = self.call_linked(fw, sw, n, argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
        if borrowed {
            self.free(fw);
        }
        self.check_value_method(gw, f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.drop_method(f, f_owned);
        V::Any(rw)
    }

    /// `check_value` in a builtin's fast path whose method read `f` is
    /// released after it (`drop_method`): an exception releases it first
    /// when the read made a new reference (a held one is freed with the
    /// other held values).
    fn check_value_method(&mut self, w: Value, f: V, f_owned: Option<Value>) {
        if let Some(flag) = f_owned {
            let exc = self.b.ins().icmp_imm_s(IntCC::Equal, w, val_const(TAG_EXCEPTION, 0));
            let both = self.b.ins().band(exc, flag);
            let yes = self.b.create_block();
            let join = self.b.create_block();
            self.b.set_cold_block(yes);
            self.b.ins().brif(both, yes, &[], join, &[]);
            self.b.switch_to_block(yes);
            self.free(f.any());
            self.b.ins().jump(join, &[]);
            self.b.switch_to_block(join);
        }
        self.check_value(w);
    }

    /// Release the method read for a builtin's fast path: held (`None`), or
    /// a new reference only if `f_owned` is set.
    fn drop_method(&mut self, f: V, f_owned: Option<Value>) {
        match f_owned {
            None => {
                self.release(f);
                self.free_v(f);
            }
            Some(flag) => {
                let yes = self.b.create_block();
                let join = self.b.create_block();
                self.b.ins().brif(flag, yes, &[], join, &[]);
                self.b.switch_to_block(yes);
                self.free(f.any());
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
            }
        }
    }

    /// An argument whose evaluation runs no code and cannot throw.
    fn pure_arg(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Undefined | ExprKind::Null => true,
            ExprKind::Local(l) => !matches!(self.vars[*l], LVar::Boxed { .. }),
            ExprKind::Box(x) | ExprKind::Unary(UnOp::Not, x) => self.pure_arg(x),
            ExprKind::Binary(BinOp::Add | BinOp::Sub | BinOp::Mul, x, y) => self.pure_arg(x) && self.pure_arg(y),
            _ => false,
        }
    }

    /// `this.push(arg)` with `f` (held) the method read: through the
    /// engine's fast path (`JS_ArrayPushFast`) while it is the original
    /// builtin, else called. Frees `f`.
    fn array_push(&mut self, f: V, f_owned: Option<Value>, this: V, arg: &Expr) -> V {
        let held = self.held_args(std::slice::from_ref(arg));
        let fw = f.any();
        let sw = this.any();
        let aw = held[0].any();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let is_obj = self.val_is(fw, TAG_OBJECT);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let orig = self.b.ins().load(I64, M_CONST, table, PIN_ARRAY_PUSH * 8);
        let orig = self.mk_ref(TAG_OBJECT, orig);
        let same = self.b.ins().icmp(IntCC::Equal, fw, orig);
        let ok = self.b.ins().band(is_obj, same);
        let fast = self.b.create_block();
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let ctx = self.ctx;
        // Inline while the array prototypes have no index properties (as
        // the engine's fast path), else through that.
        let engine = self.b.create_block();
        let inline = self.b.create_block();
        let std = self.b.ins().uload8(I32, M_FLAG, ctx, layout::CTX_STD_ARRAY_PROTO);
        let std = self.b.ins().icmp_imm_s(IntCC::NotEqual, std, 0);
        self.b.ins().brif(std, inline, &[], engine, &[]);
        self.b.switch_to_block(inline);
        // (The new length.)
        let n = self.append_inline(sw, None, aw, false, engine);
        let n = self.mk_int(n);
        self.b.ins().jump(join, &[BlockArg::Value(n)]);
        self.b.switch_to_block(engine);
        let r = self.helper("JS_ArrayPushFast", &[ctx, sw, aw]);
        let declined = self.b.ins().icmp_imm_s(IntCC::Equal, r[0], val_const(TAG_UNINITIALIZED, 0));
        let done = self.b.create_block();
        self.b.ins().brif(declined, slow, &[], done, &[]);
        self.b.switch_to_block(done);
        self.check_value_method(r[0], f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(slow);
        let vals = [fw, sw, aw];
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, 1);
        let borrowed = f_owned.is_some();
        if borrowed {
            self.dup(fw);
        }
        let gw = self.call_linked(fw, sw, 1, argv, false, |l| l.helper("lum_call", &[ctx, slot, argc, argv])[0]);
        if borrowed {
            self.free(fw);
        }
        self.check_value_method(gw, f, f_owned);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.drop_method(f, f_owned);
        V::Any(rw)
    }

    /// Call `f` with `this` and `argc` arguments at `argv` (all borrowed;
    /// `f` held by the caller unless `borrowed`, then held here during the
    /// call). A native function (C function data with a fast entry) is
    /// entered directly through its entry, as linked calls in JSC; other
    /// callees, or a declining entry, take `generic`. A new reference.
    fn call_linked(
        &mut self,
        fw: Value,
        this: Value,
        argc: usize,
        argv: Value,
        borrowed: bool,
        generic: impl FnOnce(&mut Self) -> Value,
    ) -> Value {
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        let is_obj = self.val_is(fw, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let fp = self.val_ptr(fw);
        let class = self.b.ins().uload16(I32, M_HEADER, fp, layout::OBJ_CLASS_ID);
        let is_data = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_C_FUNCTION_DATA);
        let data = self.b.create_block();
        let not_data = self.b.create_block();
        self.b.ins().brif(is_data, data, &[], not_data, &[]);
        // A builtin: called directly.
        self.b.switch_to_block(not_data);
        let is_c = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_C_FUNCTION);
        let builtin = self.b.create_block();
        self.b.ins().brif(is_c, builtin, &[], slow, &[]);
        self.b.switch_to_block(builtin);
        let ctx = self.ctx;
        let n = self.b.ins().iconst(I32, argc as i64);
        if borrowed {
            self.dup(fw);
        }
        let r = self.helper("JS_CallBuiltin", &[ctx, fw, this, n, argv]);
        if borrowed {
            self.free(fw);
        }
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(data);
        let rec = self.b.ins().load(I64, M_HEADER, fp, layout::OBJ_FUNC_DATA);
        let entry = self.b.ins().load(I64, M_HEADER, rec, layout::FDATA_FAST);
        let has = self.b.create_block();
        self.b.ins().brif(entry, has, &[], slow, &[]);
        self.b.switch_to_block(has);
        // Entries skip the stack check of the engine's call path: so here.
        let limit_addr = self.b.ins().symbol_value(self.ptr, self.env.stack_limit);
        let limit = self.b.ins().load(self.ptr, M_LIMIT, limit_addr, 0);
        let sp = self.b.ins().get_stack_pointer(self.ptr);
        let deep = self.b.ins().icmp(IntCC::UnsignedLessThan, sp, limit);
        let call = self.b.create_block();
        self.b.ins().brif(deep, slow, &[], call, &[]);
        self.b.switch_to_block(call);
        if borrowed {
            self.dup(fw);
        }
        let fdata = self.b.ins().iadd_imm_s(rec, layout::FDATA_DATA as i64);
        let ctx = self.ctx;
        let n = self.b.ins().iconst(I32, argc as i64);
        let sig: SigRef = self.env.fast_sig;
        let inst = self.b.ins().call_indirect(sig, entry, &[ctx, this, n, argv, fdata]);
        let r = self.b.inst_results(inst).to_vec();
        if borrowed {
            self.free(fw);
        }
        let declined = self.b.ins().icmp_imm_s(IntCC::Equal, r[0], val_const(TAG_UNINITIALIZED, 0));
        self.b.ins().brif(declined, slow, &[], join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(slow);
        let gw = generic(self);
        self.b.ins().jump(join, &[BlockArg::Value(gw)]);
        self.b.switch_to_block(join);
        rw
    }

    /// Spill JS values into a fresh stack slot; returns its address.
    fn values_slot(&mut self, vals: &[Value]) -> Value {
        self.values_slot_raw(vals).1
    }

    /// `values_slot`, also giving the slot.
    fn values_slot_raw(&mut self, vals: &[Value]) -> (StackSlot, Value) {
        let size = (vals.len().max(1) * SIZE) as u32;
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 3));
        for (i, &w) in vals.iter().enumerate() {
            self.b.ins().stack_store(self.ptr, w, slot, (i * SIZE) as i32);
        }
        (slot, self.b.ins().stack_addr(self.ptr, slot, 0))
    }

    fn atom(&mut self, a: ir::AtomId) -> Value {
        let base = self.b.ins().symbol_value(self.ptr, self.env.atoms);
        self.b.ins().load(I32, M_CONST, base, (a * 4) as i32)
    }

    /// A cell's value, borrowed, without a TDZ check.
    fn raw_cell(&mut self, k: ir::CellId) -> Value {
        let base = self.b.ins().symbol_value(self.ptr, self.env.cells);
        self.b.ins().load(I64, M_SLOT, base, (k * SIZE) as i32)
    }

    /// A cell's value (borrowed); throws if it is uninitialized.
    fn load_cell(&mut self, k: ir::CellId) -> Value {
        let w = self.raw_cell(k);
        let uninit = self.b.ins().icmp_imm_s(IntCC::Equal, w, val_const(TAG_UNINITIALIZED, 0));
        let throw = self.b.create_block();
        let cont = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(uninit, throw, &[], cont, &[]);
        self.b.switch_to_block(throw);
        let kv = self.b.ins().iconst(I32, k as i64);
        let ctx = self.ctx;
        self.helper("lum_throw_uninitialized", &[ctx, kv]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(cont);
        w
    }

    /// Whether `w` is `null` or `undefined` (tags 2 and 3, payload 0).
    fn is_nullish(&mut self, w: Value) -> Value {
        let x = self.b.ins().bor_imm_u(w, 1i64 << 32);
        self.b.ins().icmp_imm_s(IntCC::Equal, x, val_const(TAG_UNDEFINED, 0))
    }

    fn logical(&mut self, op: LogicalOp, ty: Ty, x: &Expr, y: &Expr) -> V {
        let a = self.expr(x);
        let truth = match (op, a) {
            (LogicalOp::Nullish, V::Any(w)) => {
                // "Truthy" here means: not nullish, keep the left value.
                let nullish = self.is_nullish(w);
                self.b.ins().bxor_imm_u(nullish, 1)
            }
            (_, V::Bool(b)) => b,
            (_, V::Num(d)) => self.truthy_num(d),
            (_, V::Any(w)) => self.truthy_any(w),
            _ => unreachable!(),
        };
        let rhs = self.b.create_block();
        let join = self.b.create_block();
        for t in clif_types(ty) {
            self.b.append_block_param(join, t);
        }
        let keep: Vec<BlockArg> = self.abi(ty, a).into_iter().map(BlockArg::Value).collect();
        match op {
            LogicalOp::And => self.b.ins().brif(truth, rhs, &[], join, &keep),
            LogicalOp::Or | LogicalOp::Nullish => self.b.ins().brif(truth, join, &keep, rhs, &[]),
        };
        self.b.switch_to_block(rhs);
        self.free_v(a);
        let c = self.expr(y);
        let args: Vec<BlockArg> = self.abi(ty, c).into_iter().map(BlockArg::Value).collect();
        self.b.ins().jump(join, &args);
        self.b.switch_to_block(join);
        let params = self.b.block_params(join).to_vec();
        self.unabi(ty, &params)
    }

    // ---- dynamic values ----

    /// ToNumber of an operand (borrowed): inline for numbers, else
    /// `JS_ToFloat64`, which may throw.
    fn number_value(&mut self, v: V) -> Value {
        let V::Any(w) = v else { return v.num() };
        let join = self.b.create_block();
        let r = self.b.append_block_param(join, F64);
        // A number known as one: its shadow.
        if let (Some(&k), Some(&d)) = (self.num_known.get(&w), self.num_shadow.get(&w)) {
            let test = self.b.create_block();
            self.b.ins().brif(k, join, &[BlockArg::Value(d)], test, &[]);
            self.b.switch_to_block(test);
        }
        // A float (its bits), else an int32, else the call.
        let float_b = self.b.create_block();
        let other = self.b.create_block();
        let int_b = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let f = self.val_is_float(w);
        self.b.ins().brif(f, float_b, &[], other, &[]);
        self.b.switch_to_block(float_b);
        let d = self.val_f64(w);
        self.b.ins().jump(join, &[BlockArg::Value(d)]);
        self.b.switch_to_block(other);
        let i = self.val_is(w, TAG_INT);
        self.b.ins().brif(i, int_b, &[], slow, &[]);
        self.b.switch_to_block(int_b);
        let x = self.b.ins().ireduce(I32, w);
        let d = self.b.ins().fcvt_from_sint(F64, x);
        self.b.ins().jump(join, &[BlockArg::Value(d)]);
        self.b.switch_to_block(slow);
        let out = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let addr = self.b.ins().stack_addr(self.ptr, out, 0);
        let ctx = self.ctx;
        let s = self.helper("JS_ToFloat64", &[ctx, addr, w]);
        self.check_int(s[0]);
        let d = self.b.ins().stack_load(self.ptr, F64, out, 0);
        self.b.ins().jump(join, &[BlockArg::Value(d)]);
        self.b.switch_to_block(join);
        r
    }

    /// Whether `w` is a number: an int32 or a float.
    fn is_number(&mut self, w: Value) -> Value {
        val::is_number(&mut self.b, w)
    }

    /// The number in a value known to hold one.
    fn number_of(&mut self, w: Value) -> Value {
        if let Some(&d) = self.num_shadow.get(&w) {
            return d;
        }
        let i = self.b.ins().ireduce(I32, w);
        let fi = self.b.ins().fcvt_from_sint(F64, i);
        let ff = self.val_f64(w);
        let is_int = self.val_is(w, TAG_INT);
        self.b.ins().select(is_int, fi, ff)
    }

    /// Two values as numbers, where they are not both floats (`fa`, `fc`:
    /// whether each is one): a float's bits, an int32 converted, else to
    /// `slow`. Continues in a new block, where the numbers are.
    fn mixed_numbers(&mut self, a: Value, c: Value, fa: Value, fc: Value, slow: Block) -> (Value, Value) {
        let done = self.b.create_block();
        let x = self.b.append_block_param(done, F64);
        let y = self.b.append_block_param(done, F64);
        let a_float = self.b.create_block();
        let a_other = self.b.create_block();
        self.b.ins().brif(fa, a_float, &[], a_other, &[]);
        // A float, then (not a float) an int32.
        self.b.switch_to_block(a_float);
        let ic = self.val_is(c, TAG_INT);
        let ok = self.b.create_block();
        self.b.ins().brif(ic, ok, &[], slow, &[]);
        self.b.switch_to_block(ok);
        let xd = self.val_f64(a);
        let ci = self.b.ins().ireduce(I32, c);
        let yd = self.b.ins().fcvt_from_sint(F64, ci);
        self.b.ins().jump(done, &[BlockArg::Value(xd), BlockArg::Value(yd)]);
        // An int32, then a float or an int32.
        self.b.switch_to_block(a_other);
        let ia = self.val_is(a, TAG_INT);
        let a_int = self.b.create_block();
        self.b.ins().brif(ia, a_int, &[], slow, &[]);
        self.b.switch_to_block(a_int);
        let ai = self.b.ins().ireduce(I32, a);
        let xd = self.b.ins().fcvt_from_sint(F64, ai);
        let c_float = self.b.create_block();
        let c_other = self.b.create_block();
        self.b.ins().brif(fc, c_float, &[], c_other, &[]);
        self.b.switch_to_block(c_float);
        let yd = self.val_f64(c);
        self.b.ins().jump(done, &[BlockArg::Value(xd), BlockArg::Value(yd)]);
        self.b.switch_to_block(c_other);
        let ic = self.val_is(c, TAG_INT);
        let ok = self.b.create_block();
        self.b.ins().brif(ic, ok, &[], slow, &[]);
        self.b.switch_to_block(ok);
        let ci = self.b.ins().ireduce(I32, c);
        let yd = self.b.ins().fcvt_from_sint(F64, ci);
        self.b.ins().jump(done, &[BlockArg::Value(xd), BlockArg::Value(yd)]);
        self.b.switch_to_block(done);
        (x, y)
    }

    fn truthy_num(&mut self, d: Value) -> Value {
        let z = self.b.ins().f64const(0.0);
        // Ordered: NaN is falsy.
        self.b.ins().fcmp(FloatCC::OrderedNotEqual, d, z)
    }

    /// JS truthiness of a borrowed value.
    fn truthy_any(&mut self, w: Value) -> Value {
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I8);
        // int / bool (tags 0 and 1): payload != 0
        let small = val::tag_in(&mut self.b, w, TAG_INT, TAG_BOOL);
        let int_blk = self.b.create_block();
        let other = self.b.create_block();
        self.b.ins().brif(small, int_blk, &[], other, &[]);
        self.b.switch_to_block(int_blk);
        let i = self.b.ins().ireduce(I32, w);
        let r = self.b.ins().icmp_imm_s(IntCC::NotEqual, i, 0);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        // float
        self.b.switch_to_block(other);
        let is_f = self.val_is_float(w);
        let f_blk = self.b.create_block();
        let other2 = self.b.create_block();
        self.b.ins().brif(is_f, f_blk, &[], other2, &[]);
        self.b.switch_to_block(f_blk);
        let d = self.val_f64(w);
        let r = self.truthy_num(d);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        // null / undefined: false
        self.b.switch_to_block(other2);
        let nullish = self.is_nullish(w);
        let slow = self.b.create_block();
        let zero = self.b.ins().iconst(I8, 0);
        self.b.ins().brif(nullish, join, &[BlockArg::Value(zero)], slow, &[]);
        // everything else (strings, objects, ...)
        self.b.switch_to_block(slow);
        let ctx = self.ctx;
        let r = self.helper("JS_ToBool", &[ctx, w]);
        let r = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, r[0], 0);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    /// If all of `vals` are int32s, try `f` on them: it gives a result and an
    /// overflow flag. Without overflow the result, an int32, goes to `join`.
    /// Otherwise code continues in the (new) current block.
    fn int_fast_path(
        &mut self,
        vals: &[Value],
        join: Block,
        f: impl FnOnce(&mut FunctionBuilder, &[Value]) -> Option<(Value, Value)>,
    ) {
        // (A second parameter of `join`: the result's number; a third:
        // whether it is one.)
        let params = self.b.block_params(join).len();
        // All int32s: none has another high half than an int's.
        let base = val_const(TAG_INT, 0);
        let mut any = self.b.ins().bxor_imm_u(vals[0], base);
        for &w in &vals[1..] {
            let x = self.b.ins().bxor_imm_u(w, base);
            any = self.b.ins().bor(any, x);
        }
        let all = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, any, 1i64 << 32);
        let int_blk = self.b.create_block();
        let rest = self.b.create_block();
        self.b.ins().brif(all, int_blk, &[], rest, &[]);
        self.b.switch_to_block(int_blk);
        let xs: Vec<Value> = vals.iter().map(|&w| self.b.ins().ireduce(I32, w)).collect();
        match f(&mut self.b, &xs) {
            Some((r, overflow)) => {
                let ok = self.b.create_block();
                self.b.ins().brif(overflow, rest, &[], ok, &[]);
                self.b.switch_to_block(ok);
                let w = self.b.ins().uextend(I64, r);
                let w = self.mk_int(w);
                let mut args = vec![BlockArg::Value(w)];
                if params >= 2 {
                    args.push(BlockArg::Value(self.b.ins().fcvt_from_sint(F64, r)));
                }
                if params >= 3 {
                    args.push(BlockArg::Value(self.b.ins().iconst(I8, 1)));
                }
                self.b.ins().jump(join, &args);
            }
            None => {
                self.b.ins().jump(rest, &[]);
            }
        }
        self.b.switch_to_block(rest);
    }

    /// `op a b` on owned values (consumed). Fast paths for two int32s (the
    /// ops that have one), two floats, then two numbers.
    fn binary_any(&mut self, op: AnyBinOp, a: Value, c: Value) -> Value {
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let rd = self.b.append_block_param(join, F64);
        let known = self.b.append_block_param(join, I8);
        // Two int32s: integer ops, unless the result overflows.
        let int_op = |b: &mut FunctionBuilder, x: Value, y: Value| -> Option<(Value, Value)> {
            let no = b.ins().iconst(I8, 0);
            Some(match op {
                AnyBinOp::Add => b.ins().sadd_overflow(x, y),
                AnyBinOp::Sub => b.ins().ssub_overflow(x, y),
                // As the interpreter: an int32 product, else (out of range,
                // or -0: a zero from a negative operand) a double.
                AnyBinOp::Mul => {
                    let (xl, yl) = (b.ins().sextend(I64, x), b.ins().sextend(I64, y));
                    let p = b.ins().imul(xl, yl);
                    let r = b.ins().ireduce(I32, p);
                    let back = b.ins().sextend(I64, r);
                    let out = b.ins().icmp(IntCC::NotEqual, back, p);
                    let zero = b.ins().icmp_imm_s(IntCC::Equal, r, 0);
                    let signs = b.ins().bor(x, y);
                    let neg = b.ins().icmp_imm_s(IntCC::SignedLessThan, signs, 0);
                    let minus_zero = b.ins().band(zero, neg);
                    (r, b.ins().bor(out, minus_zero))
                }
                AnyBinOp::BitAnd => (b.ins().band(x, y), no),
                AnyBinOp::BitOr => (b.ins().bor(x, y), no),
                AnyBinOp::BitXor => (b.ins().bxor(x, y), no),
                // The shift count is taken modulo 32, as in JS.
                AnyBinOp::Shl => (b.ins().ishl(x, y), no),
                AnyBinOp::Sar => (b.ins().sshr(x, y), no),
                _ => return None,
            })
        };
        if matches!(
            op,
            AnyBinOp::Add
                | AnyBinOp::Sub
                | AnyBinOp::Mul
                | AnyBinOp::BitAnd
                | AnyBinOp::BitOr
                | AnyBinOp::BitXor
                | AnyBinOp::Shl
                | AnyBinOp::Sar
        ) {
            self.int_fast_path(&[a, c], join, |b, xs| int_op(b, xs[0], xs[1]));
        }
        let bop = match op {
            AnyBinOp::Add => BinOp::Add,
            AnyBinOp::Sub => BinOp::Sub,
            AnyBinOp::Mul => BinOp::Mul,
            AnyBinOp::Div => BinOp::Div,
            AnyBinOp::Rem => BinOp::Rem,
            AnyBinOp::Pow => BinOp::Pow,
            AnyBinOp::Shl => BinOp::Shl,
            AnyBinOp::Sar => BinOp::Sar,
            AnyBinOp::Shr => BinOp::Shr,
            AnyBinOp::BitAnd => BinOp::BitAnd,
            AnyBinOp::BitOr => BinOp::BitOr,
            AnyBinOp::BitXor => BinOp::BitXor,
        };
        // Two floats: their bits are the doubles (one compare each).
        let fa = self.val_is_float(a);
        let fc = self.val_is_float(c);
        let floats = self.b.ins().band(fa, fc);
        let float_b = self.b.create_block();
        self.b.ins().brif(floats, float_b, &[], fast, &[]);
        for (block, float) in [(float_b, true), (fast, false)] {
            self.b.switch_to_block(block);
            let (x, y) = if float {
                (self.val_f64(a), self.val_f64(c))
            } else {
                self.mixed_numbers(a, c, fa, fc, slow)
            };
            let r = self.binary(bop, x, y);
            // As the interpreter boxes them: sums, differences and products
            // of doubles stay doubles.
            let w = if matches!(op, AnyBinOp::Add | AnyBinOp::Sub | AnyBinOp::Mul) {
                self.box_float(r)
            } else {
                self.box_number(r)
            };
            let one = self.b.ins().iconst(I8, 1);
            self.b.ins().jump(join, &[BlockArg::Value(w), BlockArg::Value(r), BlockArg::Value(one)]);
        }

        self.b.switch_to_block(slow);
        self.b.set_cold_block(slow);
        let code = match op {
            AnyBinOp::Add => op::ADD,
            AnyBinOp::Sub => op::SUB,
            AnyBinOp::Mul => op::MUL,
            AnyBinOp::Div => op::DIV,
            AnyBinOp::Rem => op::MOD,
            AnyBinOp::Pow => op::POW,
            AnyBinOp::Shl => op::SHL,
            AnyBinOp::Sar => op::SAR,
            AnyBinOp::Shr => op::SHR,
            AnyBinOp::BitAnd => op::AND,
            AnyBinOp::BitOr => op::OR,
            AnyBinOp::BitXor => op::XOR,
        };
        let ctx = self.ctx;
        let r = if op == AnyBinOp::Add {
            // (Strings, and a string with an int32, directly.)
            self.helper("JS_AddValues", &[ctx, a, c])
        } else {
            let code = self.b.ins().iconst(I32, code);
            self.helper("JS_BinaryOperator", &[ctx, code, a, c])
        };
        self.check_value(r[0]);
        self.free(c);
        self.free(a);
        let d = self.number_of(r[0]);
        let n = self.is_number(r[0]);
        self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(d), BlockArg::Value(n)]);
        self.b.switch_to_block(join);
        self.num_shadow.insert(rw, rd);
        self.num_known.insert(rw, known);
        rw
    }

    /// A number from a number, or from a JS value known to be one.
    fn num_value(&mut self, v: V) -> Value {
        match v {
            V::Num(d) => d,
            other => self.number_of(other.any()),
        }
    }

    /// `x`, but a number for a boxed number.
    fn unboxed_operand(&mut self, x: &Expr) -> V {
        match &x.kind {
            ExprKind::Box(inner) if inner.ty == Ty::F64 => self.expr(inner),
            _ => self.expr(x),
        }
    }

    /// Comparison of borrowed values. Fast path for two numbers.
    fn compare_any(&mut self, op: AnyCmpOp, a: Value, c: Value) -> Value {
        self.compare_mixed(op, V::Any(a), V::Any(c))
    }

    /// A comparison of JS values or numbers (borrowed).
    fn compare_mixed(&mut self, op: AnyCmpOp, a: V, c: V) -> Value {
        if op == AnyCmpOp::InstanceOf {
            // Through an inline cache of the constructor (lum_instanceof).
            let ic = self.new_ic();
            let ctx = self.ctx;
            let aw = self.boxed(a);
            let cw = self.boxed(c);
            let r = self.helper("lum_instanceof", &[ctx, aw, cw, ic]);
            self.check_int(r[0]);
            return self.b.ins().icmp_imm_s(IntCC::NotEqual, r[0], 0);
        }
        let code = match op {
            AnyCmpOp::Lt => op::LT,
            AnyCmpOp::Le => op::LTE,
            AnyCmpOp::Gt => op::GT,
            AnyCmpOp::Ge => op::GTE,
            AnyCmpOp::Eq => op::EQ,
            AnyCmpOp::Ne => op::NEQ,
            AnyCmpOp::StrictEq => op::STRICT_EQ,
            AnyCmpOp::StrictNe => op::STRICT_NEQ,
            AnyCmpOp::In => op::IN,
            AnyCmpOp::InstanceOf => op::INSTANCEOF,
        };
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I8);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        if !matches!(op, AnyCmpOp::In | AnyCmpOp::InstanceOf) {
            let cc = match op {
                AnyCmpOp::Lt => FloatCC::LessThan,
                AnyCmpOp::Le => FloatCC::LessThanOrEqual,
                AnyCmpOp::Gt => FloatCC::GreaterThan,
                AnyCmpOp::Ge => FloatCC::GreaterThanOrEqual,
                AnyCmpOp::Eq | AnyCmpOp::StrictEq => FloatCC::Equal,
                _ => FloatCC::NotEqual,
            };
            // `fast`: the two numbers; `float_b`: the two values, floats.
            let fast = self.b.create_block();
            let fx = self.b.append_block_param(fast, F64);
            let fy = self.b.append_block_param(fast, F64);
            let float_b = self.b.create_block();
            let mut floats_used = true;
            match (a, c) {
                (V::Any(aw), V::Any(cw)) => {
                    // Two int32s: their payloads compared.
                    let base = val_const(TAG_INT, 0);
                    let xa = self.b.ins().bxor_imm_u(aw, base);
                    let xc = self.b.ins().bxor_imm_u(cw, base);
                    let any = self.b.ins().bor(xa, xc);
                    let ints = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, any, 1i64 << 32);
                    let int_b = self.b.create_block();
                    let rest = self.b.create_block();
                    self.b.ins().brif(ints, int_b, &[], rest, &[]);
                    self.b.switch_to_block(int_b);
                    let x = self.b.ins().ireduce(I32, aw);
                    let y = self.b.ins().ireduce(I32, cw);
                    let icc = match op {
                        AnyCmpOp::Lt => IntCC::SignedLessThan,
                        AnyCmpOp::Le => IntCC::SignedLessThanOrEqual,
                        AnyCmpOp::Gt => IntCC::SignedGreaterThan,
                        AnyCmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
                        AnyCmpOp::Eq | AnyCmpOp::StrictEq => IntCC::Equal,
                        _ => IntCC::NotEqual,
                    };
                    let r = self.b.ins().icmp(icc, x, y);
                    self.b.ins().jump(join, &[BlockArg::Value(r)]);
                    // Two floats, else two numbers.
                    self.b.switch_to_block(rest);
                    let fa = self.val_is_float(aw);
                    let fc = self.val_is_float(cw);
                    let floats = self.b.ins().band(fa, fc);
                    let mixed = self.b.create_block();
                    self.b.ins().brif(floats, float_b, &[], mixed, &[]);
                    self.b.switch_to_block(mixed);
                    let (x, y) = self.mixed_numbers(aw, cw, fa, fc, slow);
                    self.b.ins().jump(fast, &[BlockArg::Value(x), BlockArg::Value(y)]);
                }
                (V::Any(w), other) | (other, V::Any(w)) => {
                    // A float, else an int32.
                    let f = self.val_is_float(w);
                    let mixed = self.b.create_block();
                    self.b.ins().brif(f, float_b, &[], mixed, &[]);
                    self.b.switch_to_block(mixed);
                    let i = self.val_is(w, TAG_INT);
                    let int_b = self.b.create_block();
                    self.b.ins().brif(i, int_b, &[], slow, &[]);
                    self.b.switch_to_block(int_b);
                    let wi = self.b.ins().ireduce(I32, w);
                    let wd = self.b.ins().fcvt_from_sint(F64, wi);
                    let od = self.num_value(other);
                    let (x, y) = if matches!(a, V::Any(_)) { (wd, od) } else { (od, wd) };
                    self.b.ins().jump(fast, &[BlockArg::Value(x), BlockArg::Value(y)]);
                }
                _ => {
                    floats_used = false;
                    let x = self.num_value(a);
                    let y = self.num_value(c);
                    self.b.ins().jump(fast, &[BlockArg::Value(x), BlockArg::Value(y)]);
                }
            }
            if floats_used {
                self.b.switch_to_block(float_b);
                let f = |l: &mut Self, v: V| match v {
                    V::Any(w) => l.val_f64(w),
                    other => l.num_value(other),
                };
                let (x, y) = (f(self, a), f(self, c));
                let r = self.b.ins().fcmp(cc, x, y);
                self.b.ins().jump(join, &[BlockArg::Value(r)]);
            }
            self.b.switch_to_block(fast);
            let r = self.b.ins().fcmp(cc, fx, fy);
            self.b.ins().jump(join, &[BlockArg::Value(r)]);
        } else {
            self.b.ins().jump(slow, &[]);
        }
        self.b.switch_to_block(slow);
        let code = self.b.ins().iconst(I32, code);
        let ctx = self.ctx;
        let aw = self.boxed(a);
        let cw = self.boxed(c);
        let r = self.helper("JS_BinaryOperator", &[ctx, code, aw, cw]);
        self.check_value(r[0]);
        let r = self.b.ins().ireduce(I8, r[0]);
        let r = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    /// Unary operator on an owned value (consumed).
    fn unary_any(&mut self, op: AnyUnOp, v: Value) -> Value {
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        match op {
            AnyUnOp::ToStr => {
                let is_str = self.val_is(v, TAG_STRING);
                self.b.ins().brif(is_str, join, &[BlockArg::Value(v)], slow, &[]);
                self.b.switch_to_block(slow);
                let ctx = self.ctx;
                self.hold(V::Any(v));
                let r = self.helper("JS_ToString", &[ctx, v]);
                self.check_value(r[0]);
                self.release(V::Any(v));
                self.free(v);
                self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(join);
                return rw;
            }
            AnyUnOp::Typeof => {
                self.b.ins().jump(slow, &[]);
            }
            _ => {
                if matches!(op, AnyUnOp::Inc | AnyUnOp::Dec) {
                    // An int32 that does not overflow stays one.
                    let delta = if op == AnyUnOp::Inc { 1 } else { -1 };
                    self.int_fast_path(&[v], join, |b, xs| {
                        let one = b.ins().iconst(I32, delta);
                        Some(b.ins().sadd_overflow(xs[0], one))
                    });
                }
                let num = self.is_number(v);
                let fast = self.b.create_block();
                self.b.ins().brif(num, fast, &[], slow, &[]);
                self.b.switch_to_block(fast);
                let w = match op {
                    AnyUnOp::Plus | AnyUnOp::ToNumeric => v,
                    AnyUnOp::Neg | AnyUnOp::Inc | AnyUnOp::Dec | AnyUnOp::BitNot => {
                        let d = self.number_of(v);
                        let r = match op {
                            AnyUnOp::Neg => self.b.ins().fneg(d),
                            AnyUnOp::Inc | AnyUnOp::Dec => {
                                let one = self.b.ins().f64const(if op == AnyUnOp::Inc { 1.0 } else { -1.0 });
                                self.b.ins().fadd(d, one)
                            }
                            _ => {
                                let i = self.toint32(d);
                                let r = self.b.ins().bnot(i);
                                self.b.ins().fcvt_from_sint(F64, r)
                            }
                        };
                        self.box_number(r)
                    }
                    _ => unreachable!(),
                };
                self.b.ins().jump(join, &[BlockArg::Value(w)]);
            }
        }
        self.b.switch_to_block(slow);
        let code = match op {
            AnyUnOp::Neg => op::NEG,
            AnyUnOp::Plus => op::PLUS,
            AnyUnOp::BitNot => op::BITNOT,
            AnyUnOp::Inc => op::INC,
            AnyUnOp::Dec => op::DEC,
            AnyUnOp::ToNumeric => op::TONUMERIC,
            AnyUnOp::Typeof => op::TYPEOF,
            AnyUnOp::ToStr => unreachable!(),
        };
        let code = self.b.ins().iconst(I32, code);
        let ctx = self.ctx;
        self.hold(V::Any(v));
        let r = self.helper("JS_UnaryOperator", &[ctx, code, v]);
        self.check_value(r[0]);
        self.release(V::Any(v));
        self.free(v);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(join);
        rw
    }

    /// Box a `MaybeF64`: its reserved NaN is `undefined`.
    fn box_maybe(&mut self, d: Value) -> Value {
        let w = self.box_number(d);
        let u = self.b.ins().bitcast(I64, d);
        let undef = self.b.ins().icmp_imm_s(IntCC::Equal, u, MAYBE_F64_UNDEFINED as i64);
        let undef_w = self.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0));
        self.b.ins().select(undef, undef_w, w)
    }

    /// Boxes a number like the interpreter's `js_number()`: int32-valued
    /// doubles (except -0) become int32 values, everything else floats.
    ///
    /// The int32 test is QuickJS's `double_is_int32`, done on the bit pattern
    /// with integer ops only. The obvious test (convert to int and back, then
    /// compare) is a ~20-cycle chain of dependent float conversions that
    /// sits right on the latency path back into the interpreter; here the
    /// conversion that produces the int payload runs in parallel instead.
    pub(crate) fn box_number(&mut self, d: Value) -> Value {
        let w = self.box_number_raw(d);
        self.num_shadow.insert(w, d);
        w
    }

    fn box_number_raw(&mut self, d: Value) -> Value {
        if let Some(&i) = self.int_of.get(&d) {
            // (An exact integer: not a NaN.)
            let i32v = self.b.ins().ireduce(I32, i);
            let back = self.b.ins().sextend(I64, i32v);
            let is_int = self.b.ins().icmp(IntCC::Equal, back, i);
            let int_w = self.b.ins().uextend(I64, i32v);
            let int_w = self.mk_int(int_w);
            let float_w = self.b.ins().bitcast(I64, d);
            return self.b.ins().select(is_int, int_w, float_w);
        }
        let u = self.b.ins().bitcast(I64, d);
        // An int32 iff converting there and back gives the same number
        // (never for NaN), and not -0.
        let i = self.b.ins().fcvt_to_sint_any(I32, d);
        let back = self.b.ins().fcvt_from_sint(F64, i);
        let same = self.b.ins().fcmp(FloatCC::Equal, back, d);
        let not_neg_zero = self.b.ins().icmp_imm_s(IntCC::NotEqual, u, i64::MIN);
        let is_int = self.b.ins().band(same, not_neg_zero);
        let int_w = self.b.ins().uextend(I64, i);
        let int_w = self.mk_int(int_w);
        let float_w = self.box_float_raw(d);
        self.b.ins().select(is_int, int_w, float_w)
    }

    // ---- numbers ----

    fn call_helper(&mut self, name: &str, args: &[Value]) -> Value {
        self.helper(name, args)[0]
    }

    /// The bound on an exact integer's magnitude (`int_bits`), if known.
    fn bits_of(&self, v: Value) -> Option<u32> {
        if !self.int_of.contains_key(&v) {
            return None;
        }
        self.int_bits.get(&v).copied().or(self.i32s.contains(&v).then_some(32))
    }

    fn binary(&mut self, op: BinOp, a: Value, c: Value) -> Value {
        // Exact in integers below 2^53 (where the `f64` operation is exact
        // too); a product with a positive constant is never -0.
        if let (Some(ba), Some(bc)) = (self.bits_of(a), self.bits_of(c)) {
            let (x, y) = (self.int_of[&a], self.int_of[&c]);
            let positive = |v: Value| self.int_consts.get(&v).is_some_and(|&k| k > 0);
            let exact = match op {
                BinOp::Add | BinOp::Sub => Some(ba.max(bc) + 1),
                BinOp::Mul if positive(a) || positive(c) => Some(ba + bc),
                _ => None,
            };
            if let Some(bits) = exact.filter(|&b| b <= 53) {
                let r = match op {
                    BinOp::Add => self.b.ins().iadd(x, y),
                    BinOp::Sub => self.b.ins().isub(x, y),
                    _ => self.b.ins().imul(x, y),
                };
                let d = self.int_f64(r);
                self.int_bits.insert(d, bits);
                if bits <= 31 {
                    self.i32s.insert(d);
                }
                return d;
            }
        }
        let ints = (self.i32s.contains(&a) && self.i32s.contains(&c)).then(|| (self.int_of[&a], self.int_of[&c]));
        match (op, ints) {
            // Exact: |a + b| <= 2^32.
            (BinOp::Add, Some((x, y))) => {
                let r = self.b.ins().iadd(x, y);
                return self.int_f64(r);
            }
            (BinOp::Sub, Some((x, y))) => {
                let r = self.b.ins().isub(x, y);
                return self.int_f64(r);
            }
            // The `f64` product is the exact one rounded (past 2^53).
            (BinOp::Mul, Some((x, y))) => {
                let p = self.b.ins().imul(x, y);
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, p, 0);
                let neg = self.b.ins().bxor(x, y);
                let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, neg, 0);
                let negzero = self.b.ins().band(zero, neg);
                let d = self.b.ins().fcvt_from_sint(F64, p);
                // `-3 * 0` is -0.
                let mz = self.b.ins().f64const(-0.0);
                let d = self.b.ins().select(negzero, mz, d);
                self.wide_of.insert(d, p);
                return d;
            }
            (BinOp::Rem, Some((x, y))) => return self.int_rem(x, y, a, c),
            _ => {}
        }
        if op == BinOp::Rem
            && let (Some(ba), Some(bc)) = (self.bits_of(a), self.bits_of(c))
            && ba <= 53
            && bc <= 53
        {
            return self.int_rem64(a, c, bc);
        }
        match op {
            BinOp::Add => self.b.ins().fadd(a, c),
            BinOp::Sub => self.b.ins().fsub(a, c),
            BinOp::Mul => self.b.ins().fmul(a, c),
            BinOp::Div => self.b.ins().fdiv(a, c),
            BinOp::Rem => self.rem(a, c),
            // (Exactly `pow`'s correctly rounded square.)
            BinOp::Pow if self.b.f64_value(c) == Some(2.0) => self.b.ins().fmul(a, a),
            BinOp::Pow => self.call_helper("lum_pow", &[a, c]),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Sar | BinOp::Shr => {
                let x = self.toint32(a);
                let y = self.toint32(c);
                // IR shifts take the amount modulo 32, as JS does.
                let r = match op {
                    BinOp::BitAnd => self.b.ins().band(x, y),
                    BinOp::BitOr => self.b.ins().bor(x, y),
                    BinOp::BitXor => self.b.ins().bxor(x, y),
                    BinOp::Shl => self.b.ins().ishl(x, y),
                    BinOp::Sar => self.b.ins().sshr(x, y),
                    BinOp::Shr => {
                        let r = self.b.ins().ushr(x, y);
                        let r = self.b.ins().uextend(I64, r);
                        let d = self.int_f64(r);
                        self.int_bits.insert(d, 32);
                        self.nonneg.insert(d);
                        return d;
                    }
                    _ => unreachable!(),
                };
                let r = self.b.ins().sextend(I64, r);
                let d = self.int_f64(r);
                self.i32s.insert(d);
                d
            }
        }
    }

    /// JS `%`. When both operands are integer-valued (and the divisor is not 0
    /// or -1) an integer remainder is exact; `copysign` restores JS's rule
    /// that the result has the dividend's sign, including `-4 % 2 === -0`.
    /// Everything else uses C `fmod`, which matches JS exactly.
    /// `a % c` for exact integers below 2^53 in magnitude (`c` below
    /// 2^`bits`): an integer remainder, with the dividend's sign (`-4 % 2`
    /// is -0); NaN for `% 0`. A nonzero constant divisor needs no division,
    /// and with a dividend known not to be negative the result is an exact
    /// integer itself.
    fn int_rem64(&mut self, a: Value, c: Value, bits: u32) -> Value {
        let (x, y) = (self.int_of[&a], self.int_of[&c]);
        if self.int_consts.get(&c).is_some_and(|&k| k != 0) {
            let r = self.b.ins().srem(x, y);
            return self.exact_rem(r, a, bits);
        }
        if self.lo_of(c).is_some_and(|lo| lo >= 1) {
            let r = self.b.ins().srem_positive(x, y);
            return self.exact_rem(r, a, bits);
        }
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, F64);
        let r_param = self.b.append_block_param(join, I64);
        let nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, y, 0);
        self.b.ins().brif(nonzero, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        // No overflow: |x| < 2^53.
        let r = self.b.ins().srem(x, y);
        let rf = self.b.ins().fcvt_from_sint(F64, r);
        let rf = self.b.ins().fcopysign(rf, a);
        self.b.ins().jump(join, &[BlockArg::Value(rf), BlockArg::Value(r)]);
        self.b.switch_to_block(slow);
        let nan = self.b.ins().f64const(f64::NAN);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(join, &[BlockArg::Value(nan), BlockArg::Value(zero)]);
        self.b.switch_to_block(join);
        self.rem_of.insert(res, RemInt { int: r_param, ok: nonzero, nan: true, small: true, none: None });
        res
    }

    fn rem(&mut self, a: Value, c: Value) -> Value {
        // An operand that is an exact integer needs no test (`int_of` values
        // stay far from the `i64` limits).
        let exact = |l: &Self, v: Value| l.int_of.get(&v).copied();
        let (ai, a_int) = match exact(self, a) {
            Some(i) => (i, self.b.ins().iconst(I8, 1)),
            None => {
                let ai = self.b.ins().fcvt_to_sint_sat(I64, a);
                let af = self.b.ins().fcvt_from_sint(F64, ai);
                // Values >= 2^63 saturate to i64::MAX, which converts back
                // to 2^63 and would pass the round-trip check.
                let unsat = self.b.ins().icmp_imm_s(IntCC::NotEqual, ai, i64::MAX);
                let same = self.b.ins().fcmp(FloatCC::Equal, af, a);
                (ai, self.b.ins().band(same, unsat))
            }
        };
        let (ci, c_int) = match exact(self, c) {
            Some(i) => (i, self.b.ins().iconst(I8, 1)),
            None => {
                let ci = self.b.ins().fcvt_to_sint_sat(I64, c);
                let cf = self.b.ins().fcvt_from_sint(F64, ci);
                let unsat = self.b.ins().icmp_imm_s(IntCC::NotEqual, ci, i64::MAX);
                let same = self.b.ins().fcmp(FloatCC::Equal, cf, c);
                (ci, self.b.ins().band(same, unsat))
            }
        };
        // |c| > 1 excludes 0 (NaN result) and -1 (srem overflow on INT64_MIN).
        let positive = exact(self, c).is_some() && self.lo_of(c).is_some_and(|lo| lo >= 2);
        let c_ok = if positive {
            self.b.ins().iconst(I8, 1)
        } else {
            let c_abs = self.b.ins().iabs(ci);
            self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, c_abs, 1)
        };
        let ok = self.b.ins().band(a_int, c_int);
        let ok = self.b.ins().band(ok, c_ok);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, F64);
        let r_param = self.b.append_block_param(join, I64);
        let is_int = self.b.append_block_param(join, I8);
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let r = if positive { self.b.ins().srem_positive(ai, ci) } else { self.b.ins().srem(ai, ci) };
        let rf = self.b.ins().fcvt_from_sint(F64, r);
        let rf = self.b.ins().fcopysign(rf, a);
        let yes = self.b.ins().iconst(I8, 1);
        self.b.ins().jump(join, &[BlockArg::Value(rf), BlockArg::Value(r), BlockArg::Value(yes)]);
        self.b.switch_to_block(slow);
        self.b.set_cold_block(slow);
        let f = self.call_helper("fmod", &[a, c]);
        let zero = self.b.ins().iconst(I64, 0);
        let no = self.b.ins().iconst(I8, 0);
        self.b.ins().jump(join, &[BlockArg::Value(f), BlockArg::Value(zero), BlockArg::Value(no)]);
        self.b.switch_to_block(join);
        // On the fast path, the exact remainder (|r| < |c|).
        let small = self.bits_of(c).is_some_and(|b| b <= 53);
        self.rem_of.insert(res, RemInt { int: r_param, ok: is_int, nan: false, small, none: None });
        res
    }

    /// An object operand, read while the `later` operands are evaluated. A
    /// plain JS-valued local is borrowed instead of referenced when none of
    /// them assigns it (nothing else can: it is not captured). Returns the
    /// value and whether it is owned (then also held).
    fn operand(&mut self, e: &Expr, later: &[&Expr]) -> (V, bool) {
        if let ExprKind::This = e.kind {
            let w = self.this.expect("`this` in a method");
            return (V::Any(w), false);
        }
        if let ExprKind::Local(l) = e.kind
            && let LVar::Val(..) = self.vars[l]
            && !later.iter().any(|x| ir::assigns_local(x, l))
        {
            return (self.use_local(l), false);
        }
        // The same, checked to be an object (an element of an array of an
        // `aot type`).
        if let ExprKind::NonNull(_, x) | ExprKind::NonNullSet(_, x) = &e.kind
            && let ExprKind::Local(l) = x.kind
            && let LVar::Val(..) = self.vars[l]
            && !later.iter().any(|x| ir::assigns_local(x, l))
        {
            let v = self.use_local(l);
            match &e.kind {
                ExprKind::NonNull(atom, _) => self.non_null(*atom, v.any()),
                ExprKind::NonNullSet(atom, _) => self.non_null_set(*atom, v.any()),
                _ => unreachable!(),
            }
            return (v, false);
        }
        let v = self.expr(e);
        self.hold(v);
        (v, true)
    }

    /// Continue if `w` (an object or `undefined`) is an object, else
    /// throw what JS throws reading property `atom` of it (converting it to
    /// an object without `atom`).
    fn non_null(&mut self, atom: Option<ir::AtomId>, w: Value) {
        let ok = self.b.create_block();
        let bad = self.b.create_block();
        self.b.set_cold_block(bad);
        let is_obj = self.val_is(w, TAG_OBJECT);
        self.b.ins().brif(is_obj, ok, &[], bad, &[]);
        self.b.switch_to_block(bad);
        let ctx = self.ctx;
        let r = match atom {
            Some(a) => {
                let atom = self.atom(a);
                self.helper("JS_GetProperty", &[ctx, w, atom])
            }
            None => self.helper("JS_ToObject", &[ctx, w]),
        };
        self.check_value(r[0]);
        self.free(r[0]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(ok);
    }

    /// Continue if `w` (an object or `undefined`) is an object, else throw
    /// what JS throws setting property `atom` of it.
    fn non_null_set(&mut self, atom: ir::AtomId, w: Value) {
        let ok = self.b.create_block();
        let bad = self.b.create_block();
        self.b.set_cold_block(bad);
        let is_obj = self.val_is(w, TAG_OBJECT);
        self.b.ins().brif(is_obj, ok, &[], bad, &[]);
        self.b.switch_to_block(bad);
        let ctx = self.ctx;
        let a = self.atom(atom);
        let undef = self.b.ins().iconst(I64, val_const(TAG_UNDEFINED, 0));
        let r = self.helper("JS_SetProperty", &[ctx, w, a, undef]);
        self.check_int(r[0]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(ok);
    }

    /// Done with an `operand`.
    fn drop_operand(&mut self, v: V, owned: bool) {
        if owned {
            self.release(v);
            self.free_v(v);
        }
    }

    // ---- iteration ----

    /// `helper(ctx, x)` on an owned value, which it borrows: a new value.
    fn convert(&mut self, x: &Expr, helper: &str) -> V {
        let v = self.expr(x);
        self.hold(v);
        let w = v.any();
        let ctx = self.ctx;
        let r = self.helper(helper, &[ctx, w]);
        self.check_value(r[0]);
        self.release(v);
        self.free_v(v);
        V::Any(r[0])
    }

    /// See `ExprKind::IterOpen`.
    fn iter_open(&mut self, x: &Expr, iter: ir::LocalId, next: ir::LocalId, atom: ir::AtomId) {
        let w = self.expr(x).any();
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        // An array with the shape of a fresh one: iterated directly.
        let is_obj = self.val_is(w, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let p = self.val_ptr(w);
        let kind = self.b.ins().iconst(I32, TYPED_ARRAYS);
        let pristine = self.pristine_shape(p, kind);
        let check = self.b.create_block();
        self.b.ins().brif(pristine, check, &[], slow, &[]);
        // ...while array iteration is the original one.
        self.b.switch_to_block(check);
        let ctx = self.ctx;
        let ok = self.helper("lum_array_iter_ok", &[ctx]);
        self.b.ins().brif(ok[0], fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        self.set_local(iter, V::Any(w));
        let zero = self.konst(0, TAG_INT);
        self.set_local(next, zero);
        self.b.ins().jump(join, &[]);
        // Anything else: the iterator protocol.
        self.b.switch_to_block(slow);
        let v = V::Any(w);
        self.hold(v);
        let sync = self.b.ins().iconst(I32, 0);
        let it = self.helper("JS_GetIteratorObject", &[ctx, w, sync]);
        self.check_value(it[0]);
        self.release(v);
        self.free_v(v);
        self.set_local(iter, V::Any(it[0]));
        let a = self.atom(atom);
        let m = self.helper("JS_GetProperty", &[ctx, it[0], a]);
        self.check_value(m[0]);
        self.set_local(next, V::Any(m[0]));
        // A native generator with the original `next`: resumed directly
        // (`next` becomes `true`).
        let ok = self.helper("lum_gen_iter_ok", &[ctx, it[0], m[0]]);
        let generator = self.b.create_block();
        self.b.ins().brif(ok[0], generator, &[], join, &[]);
        self.b.switch_to_block(generator);
        let t = self.konst(1, TAG_BOOL);
        self.set_local(next, t);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    /// See `ExprKind::IterStep`.
    fn iter_step(&mut self, iter: ir::LocalId, next: Option<ir::LocalId>) -> V {
        let iw = self.use_local(iter).any();
        let ctx = self.ctx;
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let finished = self.b.create_block();
        let failed = self.b.create_block();
        self.b.set_cold_block(failed);
        // Done already: no more calls, `undefined`.
        let undef = val_const(TAG_UNDEFINED, 0);
        let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, iw, undef);
        let step = self.b.create_block();
        let u = self.konst(0, TAG_UNDEFINED).any();
        self.b.ins().brif(live, step, &[], join, &[BlockArg::Value(u)]);
        self.b.switch_to_block(step);
        let r = match next {
            Some(n) => {
                let nw = self.use_local(n).any();
                let direct = self.val_is(nw, TAG_INT);
                let array = self.b.create_block();
                let protocol = self.b.create_block();
                self.b.ins().brif(direct, array, &[], protocol, &[]);
                // An array iterated directly: index `next` until its length
                // (read every time, as the array iterator does).
                self.b.switch_to_block(array);
                let idx = self.b.ins().band_imm_u(nw, 0xffff_ffffi64);
                let ip = self.val_ptr(iw);
                let len = self.array_length(ip);
                let more = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, len);
                let get = self.b.create_block();
                self.b.ins().brif(more, get, &[], finished, &[]);
                self.b.switch_to_block(get);
                let v = self.array_element(iw, idx);
                let exc = self.b.ins().icmp_imm_s(IntCC::Equal, v, val_const(TAG_EXCEPTION, 0));
                let got = self.b.create_block();
                self.b.ins().brif(exc, failed, &[], got, &[]);
                self.b.switch_to_block(got);
                let n1 = self.b.ins().iadd_imm_s(idx, 1);
                let n1 = self.mk_int(n1);
                self.def_local(n, V::Any(n1));
                self.b.ins().jump(join, &[BlockArg::Value(v)]);
                // A native generator resumed directly, or the iterator protocol.
                self.b.switch_to_block(protocol);
                let done_addr = self.done_slot();
                let resume = self.b.create_block();
                let call = self.b.create_block();
                let after = self.b.create_block();
                let rw2 = self.b.append_block_param(after, I64);
                let is_gen = self.val_is(nw, TAG_BOOL);
                self.b.ins().brif(is_gen, resume, &[], call, &[]);
                self.b.switch_to_block(resume);
                let r = self.helper("JS_NativeGeneratorNext", &[ctx, iw, done_addr]);
                self.b.ins().jump(after, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(call);
                let zero = self.b.ins().iconst(I32, 0);
                let null = self.b.ins().iconst(self.ptr, 0);
                let r = self.helper("JS_IteratorStep", &[ctx, iw, nw, zero, null, done_addr]);
                self.b.ins().jump(after, &[BlockArg::Value(r[0])]);
                self.b.switch_to_block(after);
                (rw2, done_addr)
            }
            None => {
                let done_addr = self.done_slot();
                (self.helper("JS_ForInStep", &[ctx, iw, done_addr])[0], done_addr)
            }
        };
        let (r, done_addr) = r;
        let exc = self.b.ins().icmp_imm_s(IntCC::Equal, r, val_const(TAG_EXCEPTION, 0));
        let ok = self.b.create_block();
        self.b.ins().brif(exc, failed, &[], ok, &[]);
        self.b.switch_to_block(ok);
        let done = self.b.ins().load(I32, Self::mem(), done_addr, 0);
        self.b.ins().brif(done, finished, &[], join, &[BlockArg::Value(r)]);
        // Done, or thrown: the iterator is not closed after either.
        self.b.switch_to_block(finished);
        let u = self.konst(0, TAG_UNDEFINED);
        self.set_local(iter, u);
        self.b.ins().jump(join, &[BlockArg::Value(u.any())]);
        self.b.switch_to_block(failed);
        let u = self.konst(0, TAG_UNDEFINED);
        self.set_local(iter, u);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(join);
        V::Any(rw)
    }

    /// A stack slot for an `int` out-parameter; its address.
    fn done_slot(&mut self) -> Value {
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 4, 2));
        self.b.ins().stack_addr(self.ptr, slot, 0)
    }

    /// The length of array `p` (its first property), as a `u32` in an `I64`.
    fn array_length(&mut self, p: Value) -> Value {
        let prop = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_PROP);
        let w = self.b.ins().load(I64, M_SLOT, prop, 0);
        let is_int = self.val_is(w, TAG_INT);
        let i = self.b.ins().band_imm_u(w, 0xffff_ffffi64);
        let d = self.val_f64(w);
        let f = self.b.ins().fcvt_to_uint_sat(I64, d);
        self.b.ins().select(is_int, i, f)
    }

    /// Element `idx` (an `I64` below 2^32) of array `w`: a new value, or
    /// `JS_EXCEPTION`.
    fn array_element(&mut self, w: Value, idx: Value) -> Value {
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let p = self.val_ptr(w);
        let count = self.b.ins().uload32(M_HEADER, p, layout::OBJ_ARRAY_COUNT);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
        let hit = self.b.create_block();
        let slow = self.b.create_block();
        self.b.ins().brif(inb, hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        let at = self.array_slot(p, idx);
        let vw = self.b.ins().load(I64, M_SLOT, at, 0);
        self.dup(vw);
        self.b.ins().jump(join, &[BlockArg::Value(vw)]);
        // Holes and slow arrays: a property read.
        self.b.switch_to_block(slow);
        let d = self.b.ins().fcvt_from_uint(F64, idx);
        let kw = self.box_number(d);
        let ctx = self.ctx;
        let r = self.helper("JS_GetPropertyValue", &[ctx, w, kw]);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(join);
        rw
    }

    /// Close the iteration in `iter` unless it is done, leaving `undefined`
    /// (`pending`: with an exception pending, which closing keeps; errors of
    /// the closing itself are then ignored). Returns the closing's result.
    fn close_raw(&mut self, iter: ir::LocalId, next: ir::LocalId, pending: bool) -> Value {
        let w = self.use_local(iter).any();
        let nw = self.use_local(next).any();
        let join = self.b.create_block();
        let r = self.b.append_block_param(join, I32);
        let zero = self.b.ins().iconst(I32, 0);
        let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, w, val_const(TAG_UNDEFINED, 0));
        let close = self.b.create_block();
        self.b.ins().brif(live, close, &[], join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(close);
        let ctx = self.ctx;
        let pending = self.b.ins().iconst(I32, pending as i64);
        let direct = self.val_is(nw, TAG_INT);
        let array = self.b.create_block();
        let protocol = self.b.create_block();
        let closed = self.b.create_block();
        let res = self.b.append_block_param(closed, I32);
        self.b.ins().brif(direct, array, &[], protocol, &[]);
        self.b.switch_to_block(array);
        let idx = self.b.ins().ireduce(I32, nw);
        let a = self.helper("lum_array_iter_close", &[ctx, w, idx, pending]);
        self.b.ins().jump(closed, &[BlockArg::Value(a[0])]);
        self.b.switch_to_block(protocol);
        let c = self.helper("JS_IteratorCloseCompletion", &[ctx, w, pending]);
        self.b.ins().jump(closed, &[BlockArg::Value(c[0])]);
        self.b.switch_to_block(closed);
        let u = self.konst(0, TAG_UNDEFINED);
        self.set_local(iter, u);
        self.b.ins().jump(join, &[BlockArg::Value(res)]);
        self.b.switch_to_block(join);
        r
    }

    /// Close the iteration (normal completion) unless it is done.
    fn close_iter(&mut self, iter: ir::LocalId, next: ir::LocalId) {
        let r = self.close_raw(iter, next, false);
        self.check_int(r);
    }

    /// `IterGuard`: `body`, closing the iterator when it completes abruptly.
    fn iter_guard(&mut self, iter: ir::LocalId, next: ir::LocalId, loop_id: Option<ir::LoopId>, body: &[Stmt]) {
        let handler = self.b.create_block();
        self.b.set_cold_block(handler);
        let outer = self.handlers.last().copied().unwrap_or(self.exc_exit);
        let (handlers, scopes) = (self.handlers.len(), self.scopes.len());
        self.scopes.push(Scope::CloseIter { iter, next, loop_id, handlers, scopes });
        self.handlers.push(handler);
        self.stmts(body);
        self.handlers.pop();
        self.scopes.pop();
        let after = self.b.create_block();
        if !self.is_filled() {
            self.b.ins().jump(after, &[]);
        }
        // An exception: close with it pending, then propagate it.
        self.b.switch_to_block(handler);
        self.clear_flag();
        self.close_raw(iter, next, true);
        self.b.ins().jump(outer, &[]);
        self.b.switch_to_block(after);
    }

    // ---- inline caches ----

    /// `o.k = o.k + y` / `o.k - y` / `o.k++` / `o.k--` (`x` is the value
    /// assigned to `o.k`), with `o` a plain local or `this` and `y` a cheap
    /// number: updated in place by `prop_update`.
    fn prop_step<'e>(&self, o: &Expr, a: ir::AtomId, x: &'e Expr) -> Option<PropStep<'e>> {
        let plain = match o.kind {
            ExprKind::Local(l) => matches!(self.vars[l], LVar::Val(..)),
            ExprKind::This => true,
            _ => false,
        };
        let same = |g: &Expr| match &g.kind {
            ExprKind::GetProp(o2, a2) => {
                *a2 == a
                    && std::mem::discriminant(&o.kind) == std::mem::discriminant(&o2.kind)
                    && match (&o.kind, &o2.kind) {
                        (ExprKind::Local(l), ExprKind::Local(m)) => l == m,
                        _ => true,
                    }
            }
            _ => false,
        };
        match &x.kind {
            _ if !plain => None,
            ExprKind::AnyUnary(op @ (AnyUnOp::Inc | AnyUnOp::Dec), g) if same(g) => Some(PropStep::Unary(*op)),
            ExprKind::AnyBinary(op @ (AnyBinOp::Add | AnyBinOp::Sub), g, y) if same(g) => match &y.kind {
                ExprKind::Box(n) if n.ty == Ty::F64 && is_cheap(n) => Some(PropStep::Binary(*op, y, n)),
                _ => None,
            },
            _ => None,
        }
    }

    /// A `prop_step` update. An int or double data property of a cached
    /// shape is updated in place (an int result that fits stays an int);
    /// everything else goes through the engine. Returns the new value if
    /// `want`ed, else `V::Void`.
    fn prop_update(&mut self, o: &Expr, a: ir::AtomId, step: PropStep, want: bool) -> V {
        let w = match o.kind {
            ExprKind::Local(l) => self.use_local(l).any(),
            _ => self.this.expect("`this` in a method"),
        };
        // Filled by the slow path's set: an own writable data property.
        let ic = self.new_ic();
        let get_ic = self.new_ic();
        let join = self.b.create_block();
        let rw = if want { self.b.append_block_param(join, I64) } else { self.b.ins().iconst(I64, 0) };
        let result = |nw: Value| if want { vec![BlockArg::Value(nw)] } else { vec![] };
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let num = self.b.create_block();
        let old_d = self.b.append_block_param(num, F64);

        let at = self.ic_hit(w, ic, slow);
        let vw = self.b.ins().load(I64, M_SLOT, at, 0);
        // The step, as a double and (when it is one) as an integer.
        let (cd, ci) = match step {
            PropStep::Unary(op) => {
                let c = if op == AnyUnOp::Inc { 1 } else { -1 };
                (self.b.ins().f64const(c as f64), Some(self.b.ins().iconst(I64, c)))
            }
            PropStep::Binary(op, _, n) => {
                let d = self.expr(n).num();
                let i = self.int_of.get(&d).copied();
                if op == AnyBinOp::Sub { (self.b.ins().fneg(d), i.map(|i| self.b.ins().ineg(i))) } else { (d, i) }
            }
        };
        let is_int = self.val_is(vw, TAG_INT);
        let int_b = self.b.create_block();
        let not_int = self.b.create_block();
        self.b.ins().brif(is_int, int_b, &[], not_int, &[]);

        self.b.switch_to_block(int_b);
        let old_i = self.val_int(vw);
        // An int result that fits is stored as is; otherwise compute in double.
        if let Some(ci) = ci {
            let r = self.b.ins().iadd(old_i, ci);
            let r32 = self.b.ins().ireduce(I32, r);
            let back = self.b.ins().sextend(I64, r32);
            let fits = self.b.ins().icmp(IntCC::Equal, back, r);
            let store = self.b.create_block();
            let wide = self.b.create_block();
            self.b.ins().brif(fits, store, &[], wide, &[]);
            self.b.switch_to_block(store);
            let nw = self.b.ins().uextend(I64, r32);
            let nw = self.mk_int(nw);
            self.b.ins().store(M_SLOT, nw, at, 0);
            self.b.ins().jump(join, &result(nw));
            self.b.switch_to_block(wide);
        }
        let as_d = self.b.ins().fcvt_from_sint(F64, old_i);
        self.b.ins().jump(num, &[BlockArg::Value(as_d)]);

        self.b.switch_to_block(not_int);
        let is_f = self.val_is_float(vw);
        let fd = self.val_f64(vw);
        self.b.ins().brif(is_f, num, &[BlockArg::Value(fd)], slow, &[]);

        // Numbers: the old value is not a reference, nothing to free.
        self.b.switch_to_block(num);
        let nd = self.b.ins().fadd(old_d, cd);
        let nw = self.box_float(nd);
        self.b.ins().store(M_SLOT, nw, at, 0);
        self.b.ins().jump(join, &result(nw));

        self.b.switch_to_block(slow);
        let atom = self.atom(a);
        let ctx = self.ctx;
        let r = self.helper("lum_ic_get", &[ctx, w, atom, get_ic]);
        self.check_value(r[0]);
        let nw = match step {
            PropStep::Unary(op) => self.unary_any(op, r[0]),
            PropStep::Binary(op, y, _) => {
                let g = V::Any(r[0]);
                self.hold(g);
                let c = self.expr(y);
                self.hold(c);
                let v = self.binary_any(op, r[0], c.any());
                self.release(c);
                self.release(g);
                v
            }
        };
        let v = V::Any(nw);
        if want {
            self.hold(v);
            // One reference for the property, one for the expression.
            self.dup(nw);
        }
        let atom = self.atom(a);
        let r = self.helper("lum_ic_set", &[ctx, w, atom, nw, ic]);
        self.check_int(r[0]);
        if want {
            self.release(v);
        }
        self.b.ins().jump(join, &result(nw));

        self.b.switch_to_block(join);
        if want { V::Any(rw) } else { V::Void }
    }

    /// `obj.a` (borrowed object) through a new inline cache: an own data
    /// property, or a data property (a method) or getter of the direct
    /// prototype. A new reference.
    fn prop_get(&mut self, w: Value, a: ir::AtomId) -> Value {
        self.prop_get_from(w, None, a, Vec::new(), false)
    }

    /// `prop_get` for a method call: its cache also serves strings, whose
    /// methods it reads from String.prototype (`LUM_IC_STRING`).
    fn method_get(&mut self, w: Value, a: ir::AtomId) -> Value {
        self.prop_get_from(w, None, a, Vec::new(), true)
    }

    /// `prop_get`, or with a receiver: `super.a` (`w`: the home object;
    /// only its prototypes are searched, and getters get the receiver).
    fn prop_get_from(
        &mut self,
        w: Value,
        receiver: Option<Value>,
        a: ir::AtomId,
        pred: Vec<Pred>,
        strings: bool,
    ) -> Value {
        self.prop_get_inner(w, receiver, a, pred, strings, false).0
    }

    /// `prop_get_from`; with `borrow`, data properties are read without a
    /// new reference, and the third result says whether the value is one
    /// (an `I8`: getters and the engine return new references).
    #[allow(clippy::too_many_arguments)]
    fn prop_get_inner(
        &mut self,
        w: Value,
        receiver: Option<Value>,
        a: ir::AtomId,
        pred: Vec<Pred>,
        strings: bool,
        borrow: bool,
    ) -> (Value, Option<Value>) {
        // A polymorphic call site's method: its cache's other entries are
        // checked inline, and are no cold path.
        let poly = std::mem::take(&mut self.poly_site);
        let ic = self.new_ic();
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let owned = borrow.then(|| self.b.append_block_param(join, I8));
        // A method's (a value that is called): its JS entry, where the cache
        // keeps it (see `fn_entries`), else 0.
        let entry = strings.then(|| self.b.append_block_param(join, I64));
        // The join's arguments: a borrowed value (not dup'ed when
        // borrowing), or a new reference; and the entry, if known.
        let args_entry = |l: &mut Self, vw: Value, new_ref: bool, e: Option<Value>| -> Vec<BlockArg> {
            let mut a = vec![BlockArg::Value(vw)];
            if borrow {
                a.push(BlockArg::Value(l.b.ins().iconst(I8, new_ref as i64)));
            }
            if strings {
                a.push(BlockArg::Value(e.unwrap_or_else(|| l.b.ins().iconst(I64, 0))));
            }
            a
        };
        let args = |l: &mut Self, vw: Value, new_ref: bool| args_entry(l, vw, new_ref, None);
        let slow = self.b.create_block();
        if !poly {
            self.b.set_cold_block(slow);
        }
        let not_obj = if strings { self.b.create_block() } else { slow };
        // (A method, or a getter something predicts, is usually a
        // prototype's: that shape compared first.)
        let proto_first = strings || !pred.is_empty();
        let (p, shape, own, kind) = self.ic_hit_split(w, ic, not_obj, slow, proto_first);
        if strings {
            // A string: String.prototype's property, if it has the cached
            // shape.
            let after = self.b.current_block().unwrap();
            self.b.switch_to_block(not_obj);
            let is_str = self.val_is(w, TAG_STRING);
            let k = self.b.ins().uload32(M_IC, ic, 12);
            let is_k = self.b.ins().icmp_imm_s(IntCC::Equal, k, IC_STRING);
            let ok = self.b.ins().band(is_str, is_k);
            let str_blk = self.b.create_block();
            self.b.ins().brif(ok, str_blk, &[], slow, &[]);
            self.b.switch_to_block(str_blk);
            let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
            let proto = self.b.ins().load(I64, M_CONST, table, PIN_STRING_PROTO * 8);
            let pshape = self.b.ins().load(I64, M_HEADER, proto, layout::OBJ_SHAPE);
            let cached = self.b.ins().load(I64, M_IC, ic, 16);
            let same = self.b.ins().icmp(IntCC::Equal, pshape, cached);
            let hit = self.b.create_block();
            self.b.ins().brif(same, hit, &[], slow, &[]);
            self.b.switch_to_block(hit);
            let at = self.ic_slot_addr(proto, ic);
            let vw = self.b.ins().load(I64, M_SLOT, at, 0);
            if !borrow {
                self.dup(vw);
            }
            let ja = args(self, vw, false);
            self.b.ins().jump(join, &ja);
            self.b.switch_to_block(after);
        }
        // A prototype's method or getter (kinds 1 and 2), cached with what
        // its holder has: while js_proto_epoch is unchanged (no watched
        // prototype changed), that value, with nothing else checked.
        let fresh_blk = self.b.create_block();
        let stale = self.b.create_block();
        let proto = self.b.create_block();
        // (Rare once caches are made: out of the way.)
        self.b.set_cold_block(stale);
        self.b.set_cold_block(proto);
        let base = self.b.ins().symbol_value(self.ptr, self.env.proto_epoch);
        let now = self.b.ins().load(I64, M_TABLE, base, 0);
        let then = self.b.ins().load(I64, M_IC, ic, IC_EPOCH);
        let fresh = self.b.ins().icmp(IntCC::Equal, now, then);
        self.b.ins().brif(fresh, fresh_blk, &[], stale, &[]);
        // A watched prototype changed since the entry was made: it is
        // made again where its shapes still hold (`lum_ic_refresh`), then
        // checked once more (no loop: a back edge here would weigh on the
        // register allocation of every function with a property read); an
        // entry without an epoch goes through the shapes.
        self.b.switch_to_block(stale);
        let refresh = self.b.create_block();
        self.b.set_cold_block(refresh);
        self.b.ins().brif(then, refresh, &[], proto, &[]);
        self.b.switch_to_block(refresh);
        self.helper("lum_ic_refresh", &[ic, p]);
        let now = self.b.ins().load(I64, M_TABLE, base, 0);
        let then = self.b.ins().load(I64, M_IC, ic, IC_EPOCH);
        let fresh = self.b.ins().icmp(IntCC::Equal, now, then);
        self.b.ins().brif(fresh, fresh_blk, &[], proto, &[]);
        // A getter's call, from either path (the getter as a JS value),
        // with its JS entry if known.
        let call = self.b.create_block();
        let g = self.b.append_block_param(call, I64);
        let g_entry = self.b.append_block_param(call, I64);
        self.b.switch_to_block(fresh_blk);
        let fw = self.b.ins().load(I64, M_IC, ic, IC_VALUE);
        let fe = self.b.ins().load(I64, M_IC, ic, IC_ENTRY);
        let data_fresh = self.b.create_block();
        let is_data = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 1);
        self.b.ins().brif(is_data, data_fresh, &[], call, &[BlockArg::Value(fw), BlockArg::Value(fe)]);
        self.b.switch_to_block(data_fresh);
        if !borrow {
            self.dup(fw);
        }
        let ja = args_entry(self, fw, false, Some(fe));
        self.b.ins().jump(join, &ja);
        // An own data property.
        self.b.switch_to_block(own);
        if receiver.is_some() {
            // (Never filled for `super`.)
            self.b.ins().jump(slow, &[]);
            let dead = self.b.create_block();
            self.b.switch_to_block(dead);
        }
        let at = self.ic_slot_addr(p, ic);
        let vw = self.b.ins().load(I64, M_SLOT, at, 0);
        if !borrow {
            self.dup(vw);
        }
        let ja = args(self, vw, false);
        self.b.ins().jump(join, &ja);
        // The other kinds, and a prototype's property after a change to a
        // watched object: through the holder's shapes. Or a call of a getter
        // (own or on the prototype).
        self.b.switch_to_block(proto);
        // (Caches hold fields of `aot type` objects only in modules that
        // have such types.)
        if receiver.is_none() && !self.env.layouts.is_empty() {
            // A field of an `aot type` object.
            let low = self.b.ins().band_imm_u(kind, 0xff);
            let is_struct = self.b.ins().icmp_imm_s(IntCC::Equal, low, IC_STRUCT);
            let st = self.b.create_block();
            let rest = self.b.create_block();
            self.b.ins().brif(is_struct, st, &[], rest, &[]);
            self.b.switch_to_block(st);
            let vw = self.struct_ic_read(p, kind, ic, slow);
            let ja = args(self, vw, true);
            self.b.ins().jump(join, &ja);
            self.b.switch_to_block(rest);
        }
        let data = self.b.create_block();
        let getter = self.b.create_block();
        let is_data = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 1);
        self.b.ins().brif(is_data, data, &[], getter, &[]);
        self.b.switch_to_block(data);
        let pat = self.ic_proto_slot(shape, ic, slow);
        let vw = self.b.ins().load(I64, M_SLOT, pat, 0);
        if !borrow {
            self.dup(vw);
        }
        let ja = args(self, vw, false);
        self.b.ins().jump(join, &ja);
        self.b.switch_to_block(getter);
        let pat = self.ic_accessor_slot(p, shape, kind, ic, slow);
        let (_, gv) = self.accessor(pat, false);
        let unknown = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(call, &[BlockArg::Value(gv), BlockArg::Value(unknown)]);
        self.b.switch_to_block(call);
        // (No getter: the reference 0, an object's tag above it.)
        let gp = self.val_ptr(g);
        self.nonnull(gp, slow);
        self.fn_entries.insert(g, g_entry);
        let ctx = self.ctx;
        let this = receiver.unwrap_or(w);
        let gw = if !pred.is_empty() {
            self.call_or_inline(
                &pred,
                g,
                this,
                &[],
                |l| l.b.ins().iconst(I64, 0),
                true,
                |l, _| l.helper("lum_call_getter", &[ctx, gp, this])[0],
            )
        } else {
            // (A getter nothing predicts: called by the runtime, keeping
            // every property read small.)
            self.helper("lum_call_getter", &[ctx, gp, this])[0]
        };
        self.check_value(gw);
        let ja = args(self, gw, true);
        self.b.ins().jump(join, &ja);
        self.b.switch_to_block(slow);
        if receiver.is_none() {
            // A polymorphic site's other entries (`LumIC.poly`): the
            // property's address, without a lookup.
            let at = if poly { self.ic_poly_inline(w, ic) } else { self.ic_poly_addr(w, ic) };
            let found = self.b.create_block();
            let miss = self.b.create_block();
            self.b.ins().brif(at, found, &[], miss, &[]);
            self.b.switch_to_block(found);
            let vw = self.b.ins().load(I64, M_SLOT, at, 0);
            if !borrow {
                self.dup(vw);
            }
            let ja = args(self, vw, false);
            self.b.ins().jump(join, &ja);
            self.b.switch_to_block(miss);
        }
        let atom = self.atom(a);
        let r = match receiver {
            Some(rw_) => self.helper("lum_super_get_ic", &[ctx, w, rw_, atom, ic]),
            None => self.helper("lum_ic_get_proto", &[ctx, w, atom, ic]),
        };
        self.check_value(r[0]);
        let ja = args(self, r[0], true);
        self.b.ins().jump(join, &ja);
        self.b.switch_to_block(join);
        if let Some(e) = entry {
            self.fn_entries.insert(rw, e);
        }
        (rw, owned)
    }

    /// A field of an `aot type` object (`p`) cached in `ic`
    /// (`LUM_IC_STRUCT`), as a new JS value: if `p` is of the cached layout
    /// and has the field, else go to `slow`.
    fn struct_ic_read(&mut self, p: Value, kind: Value, ic: Value, slow: Block) -> Value {
        let class = self.b.ins().uload16(I32, M_HEADER, p, layout::OBJ_CLASS_ID);
        let sc = self.b.ins().symbol_value(self.ptr, self.env.struct_class);
        let want = self.b.ins().load(I32, M_CONST, sc, 0);
        let same = self.b.ins().icmp(IntCC::Equal, class, want);
        let ours = self.b.create_block();
        self.b.ins().brif(same, ours, &[], slow, &[]);
        self.b.switch_to_block(ours);
        let s = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_OPAQUE);
        let l = self.b.ins().load(I32, M_STRUCT, s, 0);
        let slot = self.b.ins().load(I32, M_IC, ic, 8);
        let cached = self.b.ins().band_imm_u(slot, 0xffff);
        let same = self.b.ins().icmp(IntCC::Equal, l, cached);
        let laid = self.b.create_block();
        self.b.ins().brif(same, laid, &[], slow, &[]);
        self.b.switch_to_block(laid);
        // Present (the mask's bit of the field).
        let mask = self.b.ins().load(I32, M_STRUCT, s, 4);
        let fi = self.b.ins().ushr_imm_u(kind, 8);
        let fi = self.b.ins().band_imm_u(fi, 0xff);
        let bit = self.b.ins().ushr(mask, fi);
        let bit = self.b.ins().band_imm_u(bit, 1);
        let present = self.b.create_block();
        self.b.ins().brif(bit, present, &[], slow, &[]);
        self.b.switch_to_block(present);
        let off = self.b.ins().ushr_imm_u(slot, 16);
        let off = self.b.ins().uextend(I64, off);
        let base = self.b.ins().iadd_imm_s(s, 8);
        let at = self.b.ins().iadd(base, off);
        let storage = self.b.ins().ushr_imm_u(kind, 16);
        let storage = self.b.ins().band_imm_u(storage, 3);
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let blocks: Vec<Block> = (0..4).map(|_| self.b.create_block()).collect();
        let mut table = crate::mir::Switch::new();
        for (i, &b) in blocks.iter().enumerate() {
            table.set_entry(i as u128, b);
        }
        table.emit(&mut self.b, storage, blocks[3]);
        // int32
        self.b.switch_to_block(blocks[0]);
        let i = self.b.ins().uload32(M_STRUCT, at, 0);
        let i = self.mk_int(i);
        self.b.ins().jump(join, &[BlockArg::Value(i)]);
        // f64
        self.b.switch_to_block(blocks[1]);
        let d = self.b.ins().load(F64, M_STRUCT, at, 0);
        let bw = self.box_number(d);
        self.b.ins().jump(join, &[BlockArg::Value(bw)]);
        // boolean
        self.b.switch_to_block(blocks[2]);
        let x = self.b.ins().uload8(I64, M_STRUCT, at, 0);
        let x = self.b.ins().bor_imm_u(x, val_const(TAG_BOOL, 0));
        self.b.ins().jump(join, &[BlockArg::Value(x)]);
        // a JS value
        self.b.switch_to_block(blocks[3]);
        let vw = self.b.ins().load(I64, M_STRUCT, at, 0);
        self.dup(vw);
        self.b.ins().jump(join, &[BlockArg::Value(vw)]);
        self.b.switch_to_block(join);
        rw
    }

    /// Where `w` is an object of the shape `ic` has for an own data
    /// property: to the block returned (not yet switched to); of its other
    /// shape (`alt_shape`): on in the current block, with the cache's kind;
    /// else to `slow` (`not_obj` if not an object). Also returns the
    /// object's address and shape.
    fn ic_hit_split(
        &mut self,
        w: Value,
        ic: Value,
        not_obj: Block,
        slow: Block,
        proto_first: bool,
    ) -> (Value, Value, Block, Value) {
        let is_obj = self.val_is(w, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], not_obj, &[]);
        self.b.switch_to_block(obj);
        let p = self.val_ptr(w);
        let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
        let own = self.b.create_block();
        let kinded = self.b.create_block();
        let other = self.b.create_block();
        // `shape` for an own data property, `alt_shape` for the other kinds.
        let (first, then, first_off, then_off) = if proto_first { (kinded, own, 32, 0) } else { (own, kinded, 0, 32) };
        let cached = self.b.ins().load(I64, M_IC, ic, first_off);
        let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
        self.b.ins().brif(same, first, &[], other, &[]);
        self.b.switch_to_block(other);
        let cached = self.b.ins().load(I64, M_IC, ic, then_off);
        let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
        self.b.ins().brif(same, then, &[], slow, &[]);
        self.b.switch_to_block(kinded);
        let kind = self.b.ins().uload32(M_IC, ic, 12);
        (p, shape, own, kind)
    }

    /// Continue if `w` is an object with the shape cached in `ic`, else
    /// go to `slow`. Returns the shape and the cache's kind.
    fn ic_shape_hit(&mut self, w: Value, ic: Value, slow: Block) -> (Value, Value) {
        self.ic_shape_hit_or(w, ic, slow, slow)
    }

    /// `ic_shape_hit`, going to `not_obj` if `w` is not an object.
    fn ic_shape_hit_or(&mut self, w: Value, ic: Value, not_obj: Block, slow: Block) -> (Value, Value) {
        let is_obj = self.val_is(w, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], not_obj, &[]);
        self.b.switch_to_block(obj);
        let p = self.val_ptr(w);
        let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
        // An own data property: `shape` (kind 0); any other kind: `alt_shape`.
        let cached = self.b.ins().load(I64, M_IC, ic, 0);
        let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
        let hit = self.b.create_block();
        let kind = self.b.append_block_param(hit, I64);
        let other = self.b.create_block();
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().brif(same, hit, &[BlockArg::Value(zero)], other, &[]);
        self.b.switch_to_block(other);
        let alt = self.b.ins().load(I64, M_IC, ic, 32);
        let same = self.b.ins().icmp(IntCC::Equal, shape, alt);
        let kinded = self.b.create_block();
        self.b.ins().brif(same, kinded, &[], slow, &[]);
        self.b.switch_to_block(kinded);
        let k = self.b.ins().uload32(M_IC, ic, 12);
        self.b.ins().jump(hit, &[BlockArg::Value(k)]);
        self.b.switch_to_block(hit);
        (shape, kind)
    }

    /// Where the primary entry of `ic` misses: the address of the property
    /// of `w` that one of its polymorphic entries (`LumIC.poly`) holds,
    /// else 0.
    fn ic_poly_addr(&mut self, w: Value, ic: Value) -> Value {
        let join = self.b.create_block();
        let at = self.b.append_block_param(join, I64);
        let is_obj = self.val_is(w, TAG_OBJECT);
        let poly = self.b.ins().load(I64, M_IC, ic, IC_POLY);
        let zero = self.b.ins().iconst(I64, 0);
        let has = self.b.ins().icmp_imm_s(IntCC::NotEqual, poly, 0);
        let probe = self.b.ins().band(is_obj, has);
        let call = self.b.create_block();
        self.b.ins().brif(probe, call, &[], join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(call);
        let p = self.val_ptr(w);
        let r = self.helper("lum_ic_poly_addr", &[ic, p])[0];
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        at
    }

    /// `ic_poly_addr` with the entries checked inline (an own data
    /// property, or a prototype's up to three levels up), for a predicted
    /// call's method, where they are what usually hits.
    fn ic_poly_inline(&mut self, w: Value, ic: Value) -> Value {
        let join = self.b.create_block();
        let at = self.b.append_block_param(join, I64);
        let is_obj = self.val_is(w, TAG_OBJECT);
        let poly = self.b.ins().load(I64, M_IC, ic, IC_POLY);
        let zero = self.b.ins().iconst(I64, 0);
        let has = self.b.ins().icmp_imm_s(IntCC::NotEqual, poly, 0);
        let probe = self.b.ins().band(is_obj, has);
        let entries = self.b.create_block();
        self.b.ins().brif(probe, entries, &[], join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(entries);
        let p = self.val_ptr(w);
        let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
        for k in 0..POLY_ENTRIES {
            let e = self.b.ins().iadd_imm_s(poly, (k * IC_SIZE) as i64);
            let own = self.b.create_block();
            let other = self.b.create_block();
            let next = self.b.create_block();
            let s0 = self.b.ins().load(I64, M_IC, e, 0);
            let is_own = self.b.ins().icmp(IntCC::Equal, s0, shape);
            self.b.ins().brif(is_own, own, &[], other, &[]);
            self.b.switch_to_block(own);
            let a = self.ic_slot_addr(p, e);
            self.b.ins().jump(join, &[BlockArg::Value(a)]);
            // A data property of a prototype: up to two levels up as the
            // primary entry is checked; three (the entry's `mid2_shape`,
            // where a site's entry has `poly`) through one more shape.
            self.b.switch_to_block(other);
            let alt = self.b.ins().load(I64, M_IC, e, 32);
            let kind = self.b.ins().uload32(M_IC, e, 12);
            let same = self.b.ins().icmp(IntCC::Equal, alt, shape);
            let data = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 1);
            let ok = self.b.ins().band(same, data);
            let proto = self.b.create_block();
            self.b.ins().brif(ok, proto, &[], next, &[]);
            self.b.switch_to_block(proto);
            let mid2 = self.b.ins().load(I64, M_IC, e, IC_POLY);
            let near = self.b.create_block();
            let far = self.b.create_block();
            self.b.ins().brif(mid2, far, &[], near, &[]);
            self.b.switch_to_block(near);
            let a = self.ic_proto_slot(shape, e, next);
            self.b.ins().jump(join, &[BlockArg::Value(a)]);
            // shape -> first prototype (mid_shape) -> second (mid2_shape)
            // -> the holder (proto_shape).
            self.b.switch_to_block(far);
            let mut cur = shape;
            for off in [24, IC_POLY] {
                let obj = self.b.ins().load(I64, M_HEADER, cur, layout::SHAPE_PROTO);
                let s = self.b.ins().load(I64, M_HEADER, obj, layout::OBJ_SHAPE);
                let want = self.b.ins().load(I64, M_IC, e, off);
                let same = self.b.ins().icmp(IntCC::Equal, s, want);
                let up = self.b.create_block();
                self.b.ins().brif(same, up, &[], next, &[]);
                self.b.switch_to_block(up);
                cur = s;
            }
            let holder = self.b.ins().load(I64, M_HEADER, cur, layout::SHAPE_PROTO);
            let hs = self.b.ins().load(I64, M_HEADER, holder, layout::OBJ_SHAPE);
            let want = self.b.ins().load(I64, M_IC, e, 16);
            let same = self.b.ins().icmp(IntCC::Equal, hs, want);
            let hit = self.b.create_block();
            self.b.ins().brif(same, hit, &[], next, &[]);
            self.b.switch_to_block(hit);
            let a = self.ic_slot_addr(holder, e);
            self.b.ins().jump(join, &[BlockArg::Value(a)]);
            self.b.switch_to_block(next);
        }
        let r = self.helper("lum_ic_poly_addr", &[ic, p])[0];
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        at
    }

    /// The address of the cached own property of `p`.
    fn ic_slot_addr(&mut self, p: Value, ic: Value) -> Value {
        let slot = self.b.ins().uload32(M_IC, ic, 8);
        let props = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_PROP);
        let off = self.b.ins().imul_imm_s(slot, SIZE as i64);
        self.b.ins().iadd(props, off)
    }

    /// For a cache of a prototype property: continue if the prototype (of
    /// an object of `shape`) holding it, and the one before it if it is two
    /// levels up, have the cached shapes, else go to `slow`. Returns the
    /// address of the property in the holder.
    fn ic_proto_slot(&mut self, shape: Value, ic: Value, slow: Block) -> Value {
        let first = self.b.ins().load(I64, M_HEADER, shape, layout::SHAPE_PROTO);
        let mid = self.b.ins().load(I64, M_IC, ic, 24);
        let holder = self.b.create_block();
        let proto = self.b.append_block_param(holder, I64);
        let second = self.b.create_block();
        self.b.ins().brif(mid, second, &[], holder, &[BlockArg::Value(first)]);
        self.b.switch_to_block(second);
        let first_shape = self.b.ins().load(I64, M_HEADER, first, layout::OBJ_SHAPE);
        let same = self.b.ins().icmp(IntCC::Equal, first_shape, mid);
        let up = self.b.create_block();
        self.b.ins().brif(same, up, &[], slow, &[]);
        self.b.switch_to_block(up);
        let p2 = self.b.ins().load(I64, M_HEADER, first_shape, layout::SHAPE_PROTO);
        self.b.ins().jump(holder, &[BlockArg::Value(p2)]);
        self.b.switch_to_block(holder);
        let pshape = self.b.ins().load(I64, M_HEADER, proto, layout::OBJ_SHAPE);
        let cached = self.b.ins().load(I64, M_IC, ic, 16);
        let same = self.b.ins().icmp(IntCC::Equal, pshape, cached);
        let hit = self.b.create_block();
        self.b.ins().brif(same, hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        self.ic_slot_addr(proto, ic)
    }

    /// For a cache of a getter or setter: the address of the property
    /// holding it, own (kinds 4 and 5) or on a prototype (`ic_proto_slot`).
    fn ic_accessor_slot(&mut self, p: Value, shape: Value, kind: Value, ic: Value, slow: Block) -> Value {
        let holder = self.b.create_block();
        let at = self.b.append_block_param(holder, I64);
        let own = self.b.create_block();
        let proto = self.b.create_block();
        let is_own = self.b.ins().icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, kind, 4);
        self.b.ins().brif(is_own, own, &[], proto, &[]);
        self.b.switch_to_block(own);
        let own_at = self.ic_slot_addr(p, ic);
        self.b.ins().jump(holder, &[BlockArg::Value(own_at)]);
        self.b.switch_to_block(proto);
        let proto_at = self.ic_proto_slot(shape, ic, slow);
        self.b.ins().jump(holder, &[BlockArg::Value(proto_at)]);
        self.b.switch_to_block(holder);
        at
    }

    /// Continue if `v` (a cached getter or setter) is not NULL, else go to
    /// `slow`: an accessor half can be removed without a shape change.
    fn nonnull(&mut self, v: Value, slow: Block) {
        let ok = self.b.create_block();
        self.b.ins().brif(v, ok, &[], slow, &[]);
        self.b.switch_to_block(ok);
    }

    /// The address of a new inline cache (a `LumIC`).
    fn new_ic(&mut self) -> Value {
        let i = self.env.next_ic.get();
        self.env.next_ic.set(i + 1);
        let base = self.b.ins().symbol_value(self.ptr, self.env.ics);
        self.b.ins().iadd_imm_s(base, (i * IC_SIZE) as i64)
    }

    /// Continue if `(p, t)` is an object with the shape cached in `ic`, else
    /// go to `slow`. Returns the address of the cached property's value.
    fn ic_hit(&mut self, w: Value, ic: Value, slow: Block) -> Value {
        let is_obj = self.val_is(w, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let p = self.val_ptr(w);
        let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
        let cached = self.b.ins().load(I64, M_IC, ic, 0);
        let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
        let hit = self.b.create_block();
        self.b.ins().brif(same, hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        let slot = self.b.ins().uload32(M_IC, ic, 8);
        let props = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_PROP);
        let off = self.b.ins().imul_imm_s(slot, SIZE as i64);
        self.b.ins().iadd(props, off)
    }

    // ---- arrays ----

    /// An element index from a key: whether it is an integer (`I8`) and, if
    /// so, its value (`I64`; negative ones fail unsigned bounds checks).
    fn elem_index(&mut self, key: V) -> (Value, Value) {
        match key {
            V::Num(d) if self.int_of.contains_key(&d) => (self.b.ins().iconst(I8, 1), self.int_of[&d]),
            V::Num(d) => {
                let i = self.b.ins().fcvt_to_sint_sat(I64, d);
                let back = self.b.ins().fcvt_from_sint(F64, i);
                (self.b.ins().fcmp(FloatCC::Equal, back, d), i)
            }
            V::Any(w) => {
                let ok = self.val_is(w, TAG_INT);
                (ok, self.val_int(w))
            }
            _ => {
                let zero = self.b.ins().iconst(I64, 0);
                (self.b.ins().iconst(I8, 0), zero)
            }
        }
    }

    /// Continue only if `ok` and `w` is an array, else go to `slow`.
    /// Returns its address and its fast element count (0 unless a fast
    /// array).
    fn array_part(&mut self, w: Value, ok: Value, slow: Block) -> (Value, Value) {
        let is_obj = self.val_is(w, TAG_OBJECT);
        let go = self.b.ins().band(is_obj, ok);
        let obj = self.b.create_block();
        self.b.ins().brif(go, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let p = self.val_ptr(w);
        let class = self.b.ins().uload16(I32, M_HEADER, p, layout::OBJ_CLASS_ID);
        let is_array = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_ARRAY);
        let array = self.b.create_block();
        self.b.ins().brif(is_array, array, &[], slow, &[]);
        self.b.switch_to_block(array);
        (p, self.b.ins().uload32(M_HEADER, p, layout::OBJ_ARRAY_COUNT))
    }

    /// Entry `slot` of a table of JS values (borrowed).
    fn table_value(&mut self, table: GlobalValue, slot: usize) -> Value {
        let base = self.b.ins().symbol_value(self.ptr, table);
        self.b.ins().load(I64, M_TABLE, base, (slot * SIZE) as i32)
    }

    /// Append `vw` (taken if `take`, else a new reference) to `aw`
    /// inline, as the engine's fast path does, when it is an
    /// extensible fast array with a fresh array's shape (so its `length` is
    /// writable) and room for one more element, at `idx` if that is its
    /// element count (`None`: at its count, which must be its `length`, as
    /// `push` appends): the element count and `length` grow. Returns the
    /// new count; else branches to `slow` (the value not taken).
    fn append_inline(&mut self, aw: Value, idx: Option<Value>, vw: Value, take: bool, slow: Block) -> Value {
        let is_obj = self.val_is(aw, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let ap = self.val_ptr(aw);
        let shape = self.b.ins().load(I64, M_HEADER, ap, layout::OBJ_SHAPE);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let fresh = self.b.ins().load(I64, M_CONST, table, SHAPE_ARRAY * 8);
        let same = self.b.ins().icmp(IntCC::Equal, shape, fresh);
        let word = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_FLAGS);
        let bits = self.b.ins().band_imm_u(word, layout::OBJ_APPEND_MASK);
        let fast = self.b.ins().icmp_imm_s(IntCC::Equal, bits, layout::OBJ_APPEND_FLAGS);
        let ok = self.b.ins().band(same, fast);
        let array = self.b.create_block();
        self.b.ins().brif(ok, array, &[], slow, &[]);
        self.b.switch_to_block(array);
        let count = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_ARRAY_COUNT);
        let size = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_ARRAY_SIZE);
        let prop = self.b.ins().load(I64, M_HEADER, ap, layout::OBJ_PROP);
        // (`length`: an int value, its int32 not negative here.)
        let lw = self.b.ins().load(I64, M_SLOT, prop, 0);
        let is_int = self.val_is(lw, TAG_INT);
        let len = self.b.ins().band_imm_u(lw, 0xffff_ffffi64);
        let (k, at_count) = match idx {
            Some(k) => (k, self.b.ins().icmp(IntCC::Equal, k, count)),
            None => (count, self.b.ins().icmp(IntCC::Equal, len, count)),
        };
        // (Room, and a length that stays an int32.)
        let room = self.b.ins().icmp(IntCC::UnsignedLessThan, k, size);
        let small = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, k, i32::MAX as i64);
        let ok = self.b.ins().band(is_int, at_count);
        let ok = self.b.ins().band(ok, room);
        let ok = self.b.ins().band(ok, small);
        let store = self.b.create_block();
        self.b.ins().brif(ok, store, &[], slow, &[]);
        self.b.switch_to_block(store);
        if !take {
            self.dup(vw);
        }
        let k1 = self.b.ins().iadd_imm_s(k, 1);
        let grow = self.b.ins().icmp(IntCC::UnsignedLessThan, len, k1);
        let new_len = self.b.ins().select(grow, k1, len);
        let new_len = self.mk_int(new_len);
        self.b.ins().store(M_SLOT, new_len, prop, 0);
        let at = self.array_slot(ap, k);
        self.b.ins().store(M_SLOT, vw, at, 0);
        self.b.ins().istore32(M_HEADER, k1, ap, layout::OBJ_ARRAY_COUNT);
        self.write_barrier(ap, vw);
        k1
    }

    /// Append `v` (taken) at index `k` of fast array `ap`, which no other
    /// code holds (`fresh`, else this goes on), when it has room: its
    /// element count grows, and its `length` too if `length` (else that
    /// stays). Then jumps to `next` with `args`; when there is no room,
    /// carries on in a new block.
    #[allow(clippy::too_many_arguments)]
    fn append_fresh(
        &mut self,
        fresh: Value,
        aw: Value,
        k: Value,
        vw: Value,
        length: bool,
        next: Block,
        args: &[Value],
    ) {
        let ap = self.val_ptr(aw);
        let private = self.b.create_block();
        let store = self.b.create_block();
        let other = self.b.create_block();
        self.b.ins().brif(fresh, private, &[], other, &[]);
        self.b.switch_to_block(private);
        let size = self.b.ins().uload32(M_HEADER, ap, layout::OBJ_ARRAY_SIZE);
        let room = self.b.ins().icmp(IntCC::UnsignedLessThan, k, size);
        self.b.ins().brif(room, store, &[], other, &[]);
        self.b.switch_to_block(store);
        let at = self.array_slot(ap, k);
        self.b.ins().store(M_SLOT, vw, at, 0);
        let k1 = self.b.ins().iadd_imm_s(k, 1);
        self.b.ins().istore32(M_HEADER, k1, ap, layout::OBJ_ARRAY_COUNT);
        self.write_barrier(ap, vw);
        if length {
            let prop = self.b.ins().load(I64, M_HEADER, ap, layout::OBJ_PROP);
            let len = self.mk_int(k1);
            self.b.ins().store(M_SLOT, len, prop, 0);
        }
        let args: Vec<BlockArg> = args.iter().map(|&v| BlockArg::Value(v)).collect();
        self.b.ins().jump(next, &args);
        self.b.switch_to_block(other);
    }

    /// Address of fast array element `idx`.
    fn array_slot(&mut self, p: Value, idx: Value) -> Value {
        let values = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_ARRAY_VALUES);
        let off = self.b.ins().imul_imm_s(idx, SIZE as i64);
        self.b.ins().iadd(values, off)
    }

    /// Whether typed array `p` (of `JSTypedArrayEnum` `kind`, an `I32`) has
    /// the shape of a fresh one, so its `length` is its element count.
    fn pristine_shape(&mut self, p: Value, kind: Value) -> Value {
        let shape = self.b.ins().load(I64, M_HEADER, p, layout::OBJ_SHAPE);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let k = self.b.ins().uextend(I64, kind);
        let off = self.b.ins().imul_imm_s(k, 8);
        let at = self.b.ins().iadd(table, off);
        let want = self.b.ins().load(I64, M_CONST, at, 0);
        self.b.ins().icmp(IntCC::Equal, shape, want)
    }

    /// Read the element at `at` as a number.
    fn load_element(&mut self, kind: ArrKind, at: Value) -> Value {
        let m = M_ELEM;
        match kind {
            ArrKind::Uint8Clamped | ArrKind::Uint8 => {
                let x = self.b.ins().uload8(I32, m, at, 0);
                self.b.ins().fcvt_from_uint(F64, x)
            }
            ArrKind::Int8 => {
                let x = self.b.ins().sload8(I32, m, at, 0);
                self.b.ins().fcvt_from_sint(F64, x)
            }
            ArrKind::Uint16 => {
                let x = self.b.ins().uload16(I32, m, at, 0);
                self.b.ins().fcvt_from_uint(F64, x)
            }
            ArrKind::Int16 => {
                let x = self.b.ins().sload16(I32, m, at, 0);
                self.b.ins().fcvt_from_sint(F64, x)
            }
            ArrKind::Uint32 => {
                let x = self.b.ins().load(I32, m, at, 0);
                self.b.ins().fcvt_from_uint(F64, x)
            }
            ArrKind::Int32 => {
                let x = self.b.ins().load(I32, m, at, 0);
                self.b.ins().fcvt_from_sint(F64, x)
            }
            // Any NaN may be stored: made the canonical one, the one NaN
            // native code reads from memory with bits of its own (see
            // `val::float`; also never the reserved `undefined` NaN).
            ArrKind::Float32 => {
                let x = self.b.ins().load(F32, m, at, 0);
                let d = self.b.ins().fpromote(F64, x);
                self.canonical_nan(d)
            }
            ArrKind::Float64 => {
                let d = self.b.ins().load(F64, m, at, 0);
                self.canonical_nan(d)
            }
        }
    }

    /// `d`, or the canonical NaN if it is a NaN.
    fn canonical_nan(&mut self, d: Value) -> Value {
        let nan = self.b.ins().fcmp(FloatCC::Unordered, d, d);
        let canonical = self.b.ins().f64const(f64::from_bits(CANONICAL_NAN_BITS as u64));
        self.b.ins().select(nan, canonical, d)
    }

    /// Store number `d` at `at`, converted as the engine does.
    fn store_element(&mut self, kind: ArrKind, at: Value, d: Value) {
        let m = M_ELEM;
        match kind {
            ArrKind::Float64 => {
                self.b.ins().store(m, d, at, 0);
            }
            ArrKind::Float32 => {
                let x = self.b.ins().fdemote(F32, d);
                self.b.ins().store(m, x, at, 0);
            }
            ArrKind::Uint8Clamped => {
                // NaN -> 0, clamped to [0, 255], rounded half to even.
                let lo = self.b.ins().f64const(0.0);
                let hi = self.b.ins().f64const(255.0);
                let x = self.b.ins().fmax(d, lo);
                let x = self.b.ins().fmin(x, hi);
                let x = self.b.ins().nearest(x);
                let is_nan = self.b.ins().fcmp(FloatCC::Unordered, d, d);
                let x = self.b.ins().select(is_nan, lo, x);
                let x = self.b.ins().fcvt_to_uint_sat(I32, x);
                self.b.ins().istore8(m, x, at, 0);
            }
            _ => {
                let x = self.toint32(d);
                match kind.size() {
                    1 => self.b.ins().istore8(m, x, at, 0),
                    2 => self.b.ins().istore16(m, x, at, 0),
                    _ => self.b.ins().store(m, x, at, 0),
                };
            }
        }
    }

    /// `v` as a JS value (a new reference only if it is a number or boolean).
    fn boxed(&mut self, v: V) -> Value {
        match v {
            V::Num(d) => self.box_number(d),
            V::Int(i) => {
                let d = self.int_f64(i);
                self.box_number(d)
            }
            V::Bool(x) => self.box_bool(x),
            V::Any(w) => w,
            V::Void => self.konst(0, TAG_UNDEFINED).any(),
        }
    }

    /// JS ToInt32. Exact for |x| < 2^63 via a saturating i64 conversion and
    /// truncation; everything else (huge values, infinities, NaN) goes to the
    /// runtime.
    fn toint32(&mut self, x: Value) -> Value {
        if let Some(&i) = self.int_of.get(&x) {
            return self.b.ins().ireduce(I32, i);
        }
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
        let r = self.call_helper("lum_toint32_slow", &[x]);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    fn math(&mut self, f: MathFn, a: &[Value]) -> Value {
        if let Some(sym) = f.symbol() {
            return self.call_helper(sym, a);
        }
        fn ins(l: &mut Lower) -> Ins<'_> {
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
            // IR fmin/fmax propagate NaN and order -0 < +0, like JS.
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

impl V {
    /// Math results that are booleans (`Number.isNaN` & co.).
    fn retype(self, t: Ty, _l: &mut Lower) -> V {
        match (self, t) {
            (V::Num(v), Ty::Bool) => V::Bool(v),
            (v, _) => v,
        }
    }
}

/// Shapes per record in `lum_record_shapes` (`LUM_RECORD_SHAPES`),
/// then a count of failed learning (stopping at `LUM_RECORD_TRIES`).
const RECORD_SHAPES: usize = 4;
const RECORD_TRIES: i64 = 64;
pub(crate) const RECORD_STRIDE: usize = (RECORD_SHAPES + 1) * 8;
/// Size of a `LumIC` (an inline cache entry).
pub(crate) const IC_SIZE: usize = 80;
/// `LumFnCache`: its size, its closures' cell class and size (0: not made
/// inline), and their template (`JSFunctionTemplate`).
const FN_CACHE_SIZE: i64 = 80;
const FN_CACHE_INFO: i32 = 12;
const FN_CACHE_TMPL: i32 = 16;
/// `LumIC.epoch`, `LumIC.value` and `LumIC.entry`.
const IC_EPOCH: i32 = 48;
const IC_VALUE: i32 = 56;
const IC_ENTRY: i32 = 64;
/// `LumIC.poly`.
const IC_POLY: i32 = 40;
/// `LUM_POLY`: the entries `LumIC.poly` points to.
const POLY_ENTRIES: usize = 4;
/// `LUM_IC_STRUCT`: a cached field of an `aot type` object.
const IC_STRUCT: i64 = 7;

/// Frame layout of an async function: per local, its first slot (boxed
/// locals use two: the box object and the address of its value), then the
/// slot of `this` (methods), and the total number of slots.
/// See `Lower::prop_step`.
#[derive(Clone, Copy)]
enum PropStep<'e> {
    Unary(AnyUnOp),
    /// The operator, its boxed right operand and that operand's number.
    Binary(AnyBinOp, &'e Expr, &'e Expr),
}

/// The comparison of doubles for `op`.
fn float_cc(op: CmpOp) -> FloatCC {
    match op {
        CmpOp::Lt => FloatCC::LessThan,
        CmpOp::Le => FloatCC::LessThanOrEqual,
        CmpOp::Gt => FloatCC::GreaterThan,
        CmpOp::Ge => FloatCC::GreaterThanOrEqual,
        CmpOp::Eq => FloatCC::Equal,
        CmpOp::Ne => FloatCC::NotEqual,
    }
}

/// An argument evaluated in part only for a call (see `ExprKind::Lazy`):
/// what is evaluated first, and the lazy value.
fn lazy_part(a: &Expr) -> Option<(&[Expr], &Expr)> {
    match &a.kind {
        ExprKind::Lazy(x) => Some((&[], x)),
        ExprKind::Seq(xs) => match &xs.last()?.kind {
            ExprKind::Lazy(x) => Some((&xs[..xs.len() - 1], x)),
            _ => None,
        },
        _ => None,
    }
}

/// The lower bounds of loop counters: for a loop `for (i = a; ...; i += d)`
/// (`i++`, `i += d`, `i = i + d` with `d > 0`; `a` an integer literal, the
/// last assignment to `i` before the loop in its statement list) whose test
/// and body do not assign `i`, `i >= a` throughout the loop's test, body
/// and update.
fn loop_bounds(body: &[Stmt]) -> HashMap<ir::LoopId, Vec<(ir::LocalId, i64)>> {
    fn walk(stmts: &[Stmt], out: &mut HashMap<ir::LoopId, Vec<(ir::LocalId, i64)>>) {
        for (k, s) in stmts.iter().enumerate() {
            match s {
                Stmt::Loop { id, test, body, update, .. } => {
                    if let Some((i, lo)) = loop_counter(&stmts[..k], test.as_ref(), body, update.as_ref()) {
                        out.entry(*id).or_default().push((i, lo));
                    }
                    walk(body, out);
                }
                Stmt::If(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Stmt::Switch { cases, .. } => cases.iter().for_each(|(_, b)| walk(b, out)),
                Stmt::Block(b) | Stmt::IterGuard { body: b, .. } => walk(b, out),
                Stmt::Try { body, catch, finally, .. } => {
                    walk(body, out);
                    catch.iter().for_each(|(_, c)| walk(c, out));
                    finally.iter().for_each(|f| walk(f, out));
                }
                _ => {}
            }
        }
    }
    let mut out = HashMap::new();
    walk(body, &mut out);
    out
}

/// See `loop_bounds`.
fn loop_counter(
    before: &[Stmt],
    test: Option<&Expr>,
    body: &[Stmt],
    update: Option<&Expr>,
) -> Option<(ir::LocalId, i64)> {
    let i = match &update?.kind {
        ExprKind::Update { local, delta, .. } if *delta > 0.0 => *local,
        ExprKind::SetLocal(l, x) if int_step(*l, x).is_some_and(|c| c > 0) => *l,
        _ => return None,
    };
    let assigns = |s: &Stmt| {
        let mut found = false;
        ir::visit_stmts(std::slice::from_ref(s), &mut |e| found |= ir::assigns_local(e, i));
        found
    };
    if body.iter().any(assigns) || test.is_some_and(|t| ir::assigns_local(t, i)) {
        return None;
    }
    match before.iter().rev().find(|s| assigns(s))? {
        Stmt::Expr(Expr { kind: ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v), .. }) if *l == i => {
            int_literal(v, INT_EXACT as f64).map(|a| (i, a))
        }
        _ => None,
    }
}

/// A small expression without calls or side effects, cheap to lower twice.
fn is_cheap(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Local(_) => true,
        ExprKind::Compare(_, x, y) | ExprKind::Binary(BinOp::Add | BinOp::Sub, x, y) => is_cheap(x) && is_cheap(y),
        ExprKind::Unary(UnOp::Not, x) | ExprKind::Truthy(x) => is_cheap(x),
        _ => false,
    }
}

/// Largest integer step `int_step` takes on an int local: smaller than half
/// the spacing of doubles near 2^62, so rounding stops an int local from
/// growing well before it could overflow an `i64`.
const INT_STEP_MAX: f64 = 256.0;
/// 2^53: integers up to this magnitude are exact doubles.
const INT_EXACT: i64 = 1 << 53;

/// An integer literal that `int_locals` accepts (an exact double; not -0).
fn int_literal(e: &Expr, limit: f64) -> Option<i64> {
    match e.kind {
        ExprKind::Num(c) if c.fract() == 0.0 && c.abs() <= limit && !(c == 0.0 && c.is_sign_negative()) => {
            Some(c as i64)
        }
        _ => None,
    }
}

/// `x + c` / `x - c` / `c + x` on local `l`, with `c` a small integer: the
/// step `c` (negated for `-`).
fn int_step(l: ir::LocalId, e: &Expr) -> Option<i64> {
    int_offset(e).filter(|&(m, _)| m == l).map(|(_, c)| c)
}

/// `m + c` / `m - c` / `c + m` for a local `m`, with `c` a small integer:
/// `m`, and `c` (negated for `-`).
fn int_offset(e: &Expr) -> Option<(ir::LocalId, i64)> {
    let ExprKind::Binary(op, x, y) = &e.kind else { return None };
    match (op, &x.kind, &y.kind) {
        (BinOp::Add, &ExprKind::Local(m), _) => Some((m, int_literal(y, INT_STEP_MAX)?)),
        (BinOp::Add, _, &ExprKind::Local(m)) => Some((m, int_literal(x, INT_STEP_MAX)?)),
        (BinOp::Sub, &ExprKind::Local(m), _) => Some((m, -int_literal(y, INT_STEP_MAX)?)),
        _ => None,
    }
}

/// The int locals that provably stay below 2^52 in magnitude, so their
/// steps need no range check: assigned only integer literals, and stepped
/// only in code that runs a statically bounded number of times (loops
/// `for (i = a; i < K; i++)` with literal `a` and `K`, and their nests).
fn small_ints(f: &ir::Func, ints: &[bool]) -> Vec<bool> {
    let n = f.locals.len();
    let mut lit = vec![0.0f64; n];
    let mut steps = vec![0.0f64; n];
    bound_stmts(&f.body, 1.0, &mut lit, &mut steps);
    (0..n).map(|l| ints[l] && lit[l] + steps[l] <= (1u64 << 52) as f64).collect()
}

/// For `small_ints`: the largest literal assigned to each local, and how far
/// the steps executed `times` times (at most) can take it.
fn bound_stmts(stmts: &[Stmt], times: f64, lit: &mut [f64], steps: &mut [f64]) {
    let count = |e: &Expr, times: f64, lit: &mut [f64], steps: &mut [f64]| {
        ir::visit(e, &mut |x| match &x.kind {
            ExprKind::Update { local, delta, .. } => steps[*local] += delta.abs() * times,
            ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v) => {
                if let Some(c) = int_literal(v, INT_EXACT as f64) {
                    lit[*l] = lit[*l].max(c.unsigned_abs() as f64);
                } else if v.ty == Ty::I32 {
                    lit[*l] = lit[*l].max(2_147_483_648.0);
                } else if let Some(c) = int_step(*l, v) {
                    steps[*l] += c.unsigned_abs() as f64 * times;
                } else {
                    steps[*l] = f64::INFINITY;
                }
            }
            _ => {}
        })
    };
    for (k, s) in stmts.iter().enumerate() {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Return(Some(e)) => count(e, times, lit, steps),
            Stmt::Await { value, .. } | Stmt::Yield { value, .. } => count(value, times, lit, steps),
            Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::NewBoxes(_) => {}
            Stmt::If(c, a, b) => {
                count(c, times, lit, steps);
                bound_stmts(a, times, lit, steps);
                bound_stmts(b, times, lit, steps);
            }
            Stmt::Loop { test, post_test, body, update, .. } => {
                let test_pre = test.as_ref().filter(|_| !post_test);
                let trips = static_trips(&stmts[..k], test_pre, body, update.as_ref());
                // The test runs once more than the body.
                let inner = times * trips.map_or(f64::INFINITY, |t| t + 1.0);
                test.iter().chain(update).for_each(|e| count(e, inner, lit, steps));
                bound_stmts(body, inner, lit, steps);
            }
            Stmt::Switch { cases, .. } => {
                for (c, body) in cases {
                    c.iter().for_each(|e| count(e, times, lit, steps));
                    bound_stmts(body, times, lit, steps);
                }
            }
            Stmt::Block(body) | Stmt::IterGuard { body, .. } => bound_stmts(body, times, lit, steps),
            Stmt::Try { body, catch, finally, .. } => {
                bound_stmts(body, times, lit, steps);
                catch.iter().for_each(|(_, c)| bound_stmts(c, times, lit, steps));
                finally.iter().for_each(|c| bound_stmts(c, times, lit, steps));
            }
        }
    }
}

/// The number of iterations of a loop `for (i = a; i < K; i += d)` (or
/// `<=`, or counting down with `>` / `>=`) whose body does not assign `i`,
/// with `i = a` (`a` an integer literal) the last assignment to `i` among
/// the statements `before` the loop.
fn static_trips(before: &[Stmt], test: Option<&Expr>, body: &[Stmt], update: Option<&Expr>) -> Option<f64> {
    let ExprKind::Compare(op, x, y) = &test?.kind else { return None };
    let ExprKind::Local(i) = x.kind else { return None };
    let ExprKind::Update { local, delta, .. } = update?.kind else { return None };
    let k = int_literal(y, (INT_EXACT >> 1) as f64)? as f64;
    if local != i || delta == 0.0 {
        return None;
    }
    let assigns = |s: &Stmt| {
        let mut found = false;
        ir::visit_stmts(std::slice::from_ref(s), &mut |e| found |= ir::assigns_local(e, i));
        found
    };
    if body.iter().any(assigns) {
        return None;
    }
    let a = match before.iter().rev().find(|s| assigns(s))? {
        Stmt::Expr(Expr { kind: ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v), .. }) if *l == i => {
            int_literal(v, (INT_EXACT >> 1) as f64)? as f64
        }
        _ => return None,
    };
    let d = delta.abs();
    let span = match op {
        CmpOp::Lt if delta > 0.0 => k - a,
        CmpOp::Le if delta > 0.0 => k - a + 1.0,
        CmpOp::Gt if delta < 0.0 => a - k,
        CmpOp::Ge if delta < 0.0 => a - k + 1.0,
        _ => return None,
    };
    Some((span / d).ceil().max(0.0))
}

/// The `F64` locals that only ever hold integers, which are kept in `i64`s:
/// locals (not parameters, not boxed, not `bound_elsewhere`) only assigned
/// integer literals, `int`
/// values (`I32`: bitwise results, `int` locals and calls), other
/// such locals, and themselves or such locals plus or minus small integers
/// (`x++`, `x += 2`, `j = i + 1`). A step past 2^53 rounds the result as `f64` arithmetic would,
/// so the value is always exactly the double JS computes; with small steps
/// rounding keeps it far from the `i64` limits.
fn int_locals(f: &ir::Func, bound_elsewhere: &[ir::LocalId]) -> Vec<bool> {
    let n = f.locals.len();
    let mut ints: Vec<bool> = (0..n).map(|i| i >= f.params.len() && !f.boxed[i] && f.locals[i] == Ty::F64).collect();
    for &l in bound_elsewhere {
        ints[l] = false;
    }
    loop {
        let mut changed = false;
        ir::visit_stmts(&f.body, &mut |e| {
            let (l, ok) = match &e.kind {
                ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) => (
                    *l,
                    x.ty == Ty::I32
                        || matches!(x.kind, ExprKind::StrLen { .. })
                        || int_literal(x, INT_EXACT as f64).is_some()
                        || int_offset(x).is_some_and(|(m, _)| m == *l || ints[m])
                        || matches!(x.kind, ExprKind::Local(m) if ints[m]),
                ),
                ExprKind::Update { local, delta, .. } => (*local, delta.fract() == 0.0 && delta.abs() <= INT_STEP_MAX),
                _ => return,
            };
            if ints[l] && !ok {
                ints[l] = false;
                changed = true;
            }
        });
        if !changed {
            return ints;
        }
    }
}

/// `F64` locals (not parameters, nor bound elsewhere: see
/// `predicted_params`) that are only assigned characters' codes
/// (`StrCharCode`, where the region's guard checked the string) and integer
/// constants, never updated: they keep the code as an integer too, so that
/// comparing them with integers compares integers (see `Lower::code_vars`).
fn char_code_locals(f: &ir::Func, bound_elsewhere: &[ir::LocalId]) -> Vec<bool> {
    let n = f.locals.len();
    let mut codes: Vec<bool> = (0..n).map(|i| i >= f.params.len() && !f.boxed[i] && f.locals[i] == Ty::F64).collect();
    let mut assigned = vec![false; n];
    for &l in bound_elsewhere {
        codes[l] = false;
    }
    ir::visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) => {
            assigned[*l] = true;
            let ok = matches!(x.kind, ExprKind::StrCharCode { .. }) || int_literal(x, (1i64 << 31) as f64).is_some();
            if !ok {
                codes[*l] = false;
            }
        }
        ExprKind::Update { local, .. } => codes[*local] = false,
        _ => {}
    });
    // (Only those that hold a code somewhere: not plain integer locals.)
    let mut any_code = vec![false; n];
    ir::visit_stmts(&f.body, &mut |e| {
        if let ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) = &e.kind
            && matches!(x.kind, ExprKind::StrCharCode { .. })
        {
            any_code[*l] = true;
        }
    });
    (0..n).map(|l| codes[l] && assigned[l] && any_code[l]).collect()
}

/// The parameters of the bodies predicted calls inline (`Pred`), which
/// `inline_method` binds to the arguments, of native loops' callbacks
/// (`array_loop`), and the locals guards of speculative regions load: not
/// assignments of the IR.
fn predicted_params(f: &ir::Func, params: &[Vec<Ty>]) -> Vec<ir::LocalId> {
    let mut out = Vec::new();
    ir::visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::Predicted { func, locals, .. } | ExprKind::ArrayLoop { func, locals, .. } => {
            out.extend(locals.iter().take(params[*func].len()));
        }
        ExprKind::SpecGuard { nums, .. } => out.extend(nums.iter().map(|n| n.1)),
        ExprKind::ElemGuard { nums, .. } => out.extend(nums.iter().map(|n| n.0)),
        _ => {}
    });
    out
}

/// Of the int locals, those always in the int32 range: assigned only int32
/// literals and int32 values (bitwise results, `int`s), never stepped.
fn i32_locals(f: &ir::Func, ints: &[bool]) -> Vec<bool> {
    let mut i32s = ints.to_vec();
    ir::visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) => {
            let lit = int_literal(x, INT_EXACT as f64).is_some_and(|c| i32::try_from(c).is_ok());
            if !(x.ty == Ty::I32 || lit || matches!(x.kind, ExprKind::StrLen { .. })) {
                i32s[*l] = false;
            }
        }
        ExprKind::Update { local, .. } => i32s[*local] = false,
        _ => {}
    });
    i32s
}

pub(crate) fn frame_layout(f: &ir::Func) -> (Vec<usize>, Option<usize>, usize) {
    let mut at = Vec::with_capacity(f.locals.len());
    let mut n = 0;
    for i in 0..f.locals.len() {
        at.push(n);
        n += if f.boxed[i] { 2 } else { 1 };
    }
    let this = f.has_this.then(|| {
        n += 1;
        n - 1
    });
    (at, this, n)
}

/// State of an async step function being lowered.
pub(crate) struct AsyncCx {
    /// The frame's slots (`JSValue *`).
    pub slots: Value,
    pub state: Value,
    /// The value delivered on resumption (`JSValue *`, borrowed).
    pub value: Value,
    pub is_throw: Value,
    pub layout: Vec<usize>,
    /// Resume points, by state - 1.
    resumes: Vec<Block>,
    dispatch: Block,
}

impl Lower {
    /// Start lowering an async step function: move the locals out of the
    /// frame, then dispatch on the state (0: start of the body).
    pub(crate) fn enter_async(&mut self, slots: Value, state: Value, value: Value, is_throw: Value, f: &ir::Func) {
        let (layout, this_slot, _) = frame_layout(f);
        // (A value capture's value is in its first slot, which its address
        // then points at: the closure's data may be gone while the frame is
        // suspended.)
        let mut by_value = vec![false; layout.len()];
        for (k, &cl) in f.captures.iter().enumerate() {
            by_value[cl] = f.value_captures[k];
        }
        for (l, &s) in layout.iter().enumerate() {
            let at = (s * SIZE) as i32;
            let w = self.b.ins().load(I64, M_SLOT, slots, at);
            match self.vars[l] {
                LVar::Boxed { obj, slot, .. } => {
                    let sp = if by_value[l] {
                        self.b.ins().iadd_imm_s(slots, at as i64)
                    } else {
                        self.b.ins().load(I64, M_SLOT, slots, at + SIZE as i32)
                    };
                    self.b.def_var(obj, w);
                    self.b.def_var(slot, sp);
                }
                LVar::Val(var) => {
                    self.b.def_var(var, w);
                    if let Some(sv) = self.shadows[l] {
                        let d = self.number_of(w);
                        self.b.def_var(sv, d);
                    }
                }
                // (Exact as a double: see `int_locals`.)
                LVar::Int(v) => {
                    let d = self.val_f64(w);
                    let i = self.b.ins().fcvt_to_sint_sat(I64, d);
                    self.b.def_var(v, i)
                }
                LVar::One(v) => {
                    let x = match self.locals[l] {
                        Ty::F64 | Ty::MaybeF64 => self.val_f64(w),
                        _ => self.b.ins().ireduce(I8, w),
                    };
                    self.b.def_var(v, x);
                }
            }
            // (The slots keep their values while the step runs: the
            // collector traces what they hold, which the next suspension
            // or the frame's end replaces or drops.)
        }
        if let Some(s) = this_slot {
            // `this` stays owned by the frame; borrowed here.
            let w = self.b.ins().load(I64, M_SLOT, slots, (s * SIZE) as i32);
            self.this = Some(w);
        }
        let dispatch = self.b.create_block();
        self.b.ins().jump(dispatch, &[]);
        let start = self.b.create_block();
        self.b.switch_to_block(start);
        self.async_cx = Some(AsyncCx { slots, state, value, is_throw, layout, resumes: vec![start], dispatch });
        self.async_gen = f.is_async && f.is_generator;
    }

    /// Move every local into the frame (suspending): as JS values (the
    /// collector traces the frame), numbers as floats.
    fn spill_locals(&mut self) {
        let cx = self.async_cx.as_ref().unwrap();
        let (slots, layout) = (cx.slots, cx.layout.clone());
        for (l, &s) in layout.iter().enumerate() {
            let at = (s * SIZE) as i32;
            let w = match self.vars[l] {
                LVar::Boxed { obj, slot, .. } => {
                    // (The value's address: below the cage's top, an int to
                    // the collector.)
                    let sp = self.b.use_var(slot);
                    self.b.ins().store(M_SLOT, sp, slots, at + SIZE as i32);
                    self.b.use_var(obj)
                }
                LVar::Int(v) => {
                    let i = self.b.use_var(v);
                    let d = self.b.ins().fcvt_from_sint(F64, i);
                    self.box_float_raw(d)
                }
                LVar::Val(var) => self.b.use_var(var),
                LVar::One(v) => {
                    let x = self.b.use_var(v);
                    match self.locals[l] {
                        Ty::F64 | Ty::MaybeF64 => self.box_float_raw(x),
                        _ => self.box_bool(x),
                    }
                }
            };
            self.b.ins().store(M_SLOT, w, slots, at);
        }
    }

    /// The step function's result when it suspends (`payload`: the resume
    /// state and what it yields; see the runtime).
    fn suspended(&mut self, payload: i64) {
        let r = self.b.ins().iconst(I64, val_const(TAG_UNINITIALIZED, payload));
        self.b.ins().return_(&[r]);
    }

    /// `await value`: register the continuation, spill, return "suspended";
    /// lowering continues at the resume point.
    fn await_stmt(&mut self, value: &Expr, target: ir::AwaitTarget) {
        let w = if self.async_gen {
            let v = self.expr(value).any();
            self.await_value(v)
        } else {
            self.await_promise(value)
        };
        self.awaited(w, target);
    }

    /// `await` in an async function: register the continuation, spill,
    /// return "suspended". The awaited value (a new reference).
    fn await_promise(&mut self, value: &Expr) -> Value {
        let resume = self.b.create_block();
        let state = {
            let cx = self.async_cx.as_mut().expect("await in an async function");
            cx.resumes.push(resume);
            cx.resumes.len() - 1
        };
        let w = self.awaited_operand(value);
        let slots = self.async_cx.as_ref().unwrap().slots;
        let st = self.b.ins().iconst(I32, state as i64);
        let ctx = self.ctx;
        // Only locals survive a suspension.
        debug_assert!(self.temps.is_empty(), "values held across an await");
        // Takes ownership of the value. The value to continue with at once
        // (the locals still in place) when suspending could not be told
        // apart, else "suspended".
        let r = self.helper("lum_await", &[ctx, slots, w, st]);
        self.check_value(r[0]);
        let join = self.b.create_block();
        let vw = self.b.append_block_param(join, I64);
        let suspend = self.b.create_block();
        let is_suspended = self.b.ins().icmp_imm_s(IntCC::Equal, r[0], val_const(TAG_UNINITIALIZED, 0));
        self.b.ins().brif(is_suspended, suspend, &[], join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(suspend);
        self.spill_locals();
        self.suspended(0);

        // Resumed: the locals come back from the frame (see enter_async).
        self.b.switch_to_block(resume);
        let (vptr, is_throw) = {
            let cx = self.async_cx.as_ref().unwrap();
            (cx.value, cx.is_throw)
        };
        let rw = self.b.ins().load(I64, M_SLOT, vptr, 0);
        self.dup(rw);
        let throw = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(is_throw, throw, &[], join, &[BlockArg::Value(rw)]);
        self.b.switch_to_block(throw);
        // A rejection is thrown at the await.
        self.helper("JS_Throw", &[ctx, rw]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(join);
        vw
    }

    /// The value to await (a new reference). `await Promise.resolve(x)`
    /// awaits `x` itself when `Promise` and its `resolve` are the intrinsic
    /// ones: PromiseResolve of `x` happens either way, and no promise is
    /// made for nothing. `Promise` and `resolve` are read first, as in JS.
    fn awaited_operand(&mut self, value: &Expr) -> Value {
        let ExprKind::CallMethod(o, Key::Atom(a), args) = &value.kind else {
            return self.expr(value).any();
        };
        let ExprKind::Global(g) = o.kind else {
            return self.expr(value).any();
        };
        if self.env.promise_resolve != Some((g, *a)) || args.len() != 1 {
            return self.expr(value).any();
        }
        let ctx = self.ctx;
        let atom = self.atom(g);
        let ic = self.new_ic();
        let pr = self.helper("lum_global_get", &[ctx, atom, ic]);
        self.check_value(pr[0]);
        let promise = V::Any(pr[0]);
        self.hold(promise);
        let mw = self.prop_get(pr[0], *a);
        let method = V::Any(mw);
        self.hold(method);
        let x = self.expr(&args[0]);
        self.hold(x);
        let xw = x.any();
        let same = self.helper("lum_is_promise_resolve", &[pr[0], mw]);
        // Both paths consume `x`.
        self.release(x);
        let join = self.b.create_block();
        let rw = self.b.append_block_param(join, I64);
        let call = self.b.create_block();
        self.b.set_cold_block(call);
        self.b.ins().brif(same[0], join, &[BlockArg::Value(xw)], call, &[]);
        self.b.switch_to_block(call);
        let slot = self.values_slot(&[mw, pr[0], xw]);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, 1);
        let r = self.helper("lum_call", &[ctx, slot, argc, argv]);
        self.free(xw);
        self.check_value(r[0]);
        self.b.ins().jump(join, &[BlockArg::Value(r[0])]);
        self.b.switch_to_block(join);
        self.release(method);
        self.release(promise);
        self.free(mw);
        self.free(pr[0]);
        rw
    }

    /// Deliver an awaited value (a new reference) to `target`.
    fn awaited(&mut self, w: Value, target: ir::AwaitTarget) {
        let v = V::Any(w);
        match target {
            ir::AwaitTarget::Discard => self.free(w),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => {
                let v = if self.ret == Ty::Void {
                    self.free(w);
                    V::Void
                } else {
                    v
                };
                self.return_value(v);
            }
        }
    }

    /// `await` in an async generator: suspend for the engine to await `v`
    /// (taken). The settled value (a new reference); a rejection is thrown.
    fn await_value(&mut self, v: Value) -> Value {
        self.suspend(Some(v), 3);
        let (vw, mode) = self.resumption();
        let throw = self.b.create_block();
        let ok = self.b.create_block();
        self.b.set_cold_block(throw);
        let is_throw = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 2);
        self.b.ins().brif(is_throw, throw, &[], ok, &[]);
        self.b.switch_to_block(throw);
        let ctx = self.ctx;
        self.helper("JS_Throw", &[ctx, vw]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(ok);
        vw
    }

    /// Suspend the generator: spill the locals and return to the runtime
    /// what it yields (`kind` 0: a value, 1: an iterator result as is, 3: a
    /// value to await, in an async generator). It
    /// continues at the returned block, with the resumption in the step
    /// function's parameters.
    fn suspend(&mut self, value: Option<Value>, kind: i64) -> Block {
        let resume = self.b.create_block();
        let (state, vptr) = {
            let cx = self.async_cx.as_mut().expect("yield in a generator");
            cx.resumes.push(resume);
            (cx.resumes.len() - 1, cx.value)
        };
        debug_assert!(self.temps.is_empty(), "values held across a yield");
        self.spill_locals();
        if let Some(w) = value {
            self.b.ins().store(M_SLOT, w, vptr, 0);
        }
        self.suspended(4 * state as i64 + kind);
        self.b.switch_to_block(resume);
        resume
    }

    /// The value the generator was resumed with (a new reference), and how
    /// (next / return / throw: 0 / 1 / 2).
    fn resumption(&mut self) -> (Value, Value) {
        let (vptr, mode) = {
            let cx = self.async_cx.as_ref().unwrap();
            (cx.value, cx.is_throw)
        };
        let vw = self.b.ins().load(I64, M_SLOT, vptr, 0);
        self.dup(vw);
        (vw, mode)
    }

    fn yield_stmt(&mut self, value: &Expr, kind: ir::YieldKind, target: ir::AwaitTarget) {
        match kind {
            ir::YieldKind::Initial => {
                self.suspend(None, 0);
            }
            ir::YieldKind::Value => {
                let v = self.expr(value).any();
                // An async generator yields the awaited value.
                let v = if self.async_gen { self.await_value(v) } else { v };
                self.suspend(Some(v), 0);
                let (v, mode) = self.resumption();
                self.resumed(v, mode, target);
            }
            ir::YieldKind::Delegate { iter, next, next_atom } if self.async_gen => {
                self.async_yield_star(value, iter, next, next_atom, target)
            }
            ir::YieldKind::Delegate { iter, next, next_atom } => self.yield_star(value, iter, next, next_atom, target),
        }
    }

    /// After a suspension: throw or return the value delivered, or deliver
    /// it to `target`.
    fn resumed(&mut self, vw: Value, mode: Value, target: ir::AwaitTarget) {
        let throw = self.b.create_block();
        let not_throw = self.b.create_block();
        let ret = self.b.create_block();
        let next = self.b.create_block();
        self.b.set_cold_block(throw);
        let is_throw = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 2);
        self.b.ins().brif(is_throw, throw, &[], not_throw, &[]);
        self.b.switch_to_block(throw);
        let ctx = self.ctx;
        self.helper("JS_Throw", &[ctx, vw]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(not_throw);
        let is_return = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 1);
        self.b.ins().brif(is_return, ret, &[], next, &[]);
        // `return(v)`: as a `return v` here (running `finally` blocks; an
        // async generator awaits the value first).
        self.b.switch_to_block(ret);
        let rw = if self.async_gen { self.await_value(vw) } else { vw };
        self.return_value(V::Any(rw));
        self.b.switch_to_block(next);
        let v = V::Any(vw);
        match target {
            ir::AwaitTarget::Discard => self.free(vw),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => unreachable!("`return yield` is hoisted"),
        }
    }

    /// `yield* value`: resume the inner iterator as the generator is
    /// resumed, yielding its results, until it is done.
    fn yield_star(
        &mut self,
        value: &Expr,
        iter: ir::LocalId,
        next: ir::LocalId,
        next_atom: ir::AtomId,
        target: ir::AwaitTarget,
    ) {
        let w = self.expr(value).any();
        let v = V::Any(w);
        self.hold(v);
        let ctx = self.ctx;
        let sync = self.b.ins().iconst(I32, 0);
        let it = self.helper("JS_GetIteratorObject", &[ctx, w, sync]);
        self.check_value(it[0]);
        self.release(v);
        self.free_v(v);
        self.set_local(iter, V::Any(it[0]));
        let a = self.atom(next_atom);
        let m = self.helper("JS_GetProperty", &[ctx, it[0], a]);
        self.check_value(m[0]);
        self.set_local(next, V::Any(m[0]));
        // The loop: (mode, received value).
        let head = self.b.create_block();
        let mode = self.b.append_block_param(head, I32);
        let rw = self.b.append_block_param(head, I64);
        let zero = self.b.ins().iconst(I32, 0);
        let u = self.konst(0, TAG_UNDEFINED).any();
        self.b.ins().jump(head, &[BlockArg::Value(zero), BlockArg::Value(u)]);
        self.b.switch_to_block(head);
        let iw = self.use_local(iter).any();
        let nw = self.use_local(next).any();
        let slot = self.values_slot(&[iw, nw, rw]);
        let kind_addr = self.done_slot();
        let r = self.helper("lum_yield_star", &[ctx, slot, mode, kind_addr]);
        self.check_value(r[0]);
        let kind = self.b.ins().load(I32, Self::mem(), kind_addr, 0);
        let yield_blk = self.b.create_block();
        let finished = self.b.create_block();
        let ret = self.b.create_block();
        let deliver = self.b.create_block();
        let not_yield = self.b.ins().icmp_imm_s(IntCC::NotEqual, kind, 0);
        self.b.ins().brif(not_yield, finished, &[], yield_blk, &[]);
        // An inner result: yielded as is, then resumed with the next mode.
        self.b.switch_to_block(yield_blk);
        self.suspend(Some(r[0]), 1);
        let (vw, mode) = self.resumption();
        self.b.ins().jump(head, &[BlockArg::Value(mode), BlockArg::Value(vw)]);
        self.b.switch_to_block(finished);
        let is_return = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 2);
        self.b.ins().brif(is_return, ret, &[], deliver, &[]);
        self.b.switch_to_block(ret);
        self.return_value(V::Any(r[0]));
        // Done: the value of the `yield*`.
        self.b.switch_to_block(deliver);
        let v = V::Any(r[0]);
        match target {
            ir::AwaitTarget::Discard => self.free(r[0]),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => unreachable!("`return yield*` is hoisted"),
        }
    }
    /// `yield*` in an async generator, as the interpreter's bytecode does it:
    /// the inner (async) iterator is resumed as the generator is, each
    /// result is awaited, and the values of the results not done are yielded
    /// (without awaiting them again). A `return` awaits the value it gets
    /// before passing it on, and again before returning.
    fn async_yield_star(
        &mut self,
        value: &Expr,
        iter: ir::LocalId,
        next: ir::LocalId,
        next_atom: ir::AtomId,
        target: ir::AwaitTarget,
    ) {
        let w = self.expr(value).any();
        let v = V::Any(w);
        self.hold(v);
        let ctx = self.ctx;
        let is_async = self.b.ins().iconst(I32, 1);
        let it = self.helper("JS_GetIteratorObject", &[ctx, w, is_async]);
        self.check_value(it[0]);
        self.release(v);
        self.free_v(v);
        self.set_local(iter, V::Any(it[0]));
        let a = self.atom(next_atom);
        let m = self.helper("JS_GetProperty", &[ctx, it[0], a]);
        self.check_value(m[0]);
        self.set_local(next, V::Any(m[0]));

        // The loop: (mode, received value).
        let head = self.b.create_block();
        let mode = self.b.append_block_param(head, I32);
        let rw = self.b.append_block_param(head, I64);
        // A value to yield; the value of the `yield*`.
        let yield_part = self.b.create_block();
        let yw = self.b.append_block_param(yield_part, I64);
        let finish = self.b.create_block();
        let fw = self.b.append_block_param(finish, I64);
        let zero = self.b.ins().iconst(I32, 0);
        let u = self.konst(0, TAG_UNDEFINED).any();
        self.b.ins().jump(head, &[BlockArg::Value(zero), BlockArg::Value(u)]);

        self.b.switch_to_block(head);
        let on_next = self.b.create_block();
        let not_next = self.b.create_block();
        let on_return = self.b.create_block();
        let on_throw = self.b.create_block();
        let is_next = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 0);
        self.b.ins().brif(is_next, on_next, &[], not_next, &[]);
        self.b.switch_to_block(not_next);
        let is_return = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 1);
        self.b.ins().brif(is_return, on_return, &[], on_throw, &[]);

        // Each mode separately: a mode does not survive the awaits.
        for (blk, m) in [(on_next, 0), (on_return, 1), (on_throw, 2)] {
            self.b.switch_to_block(blk);
            let received = if m == 1 { self.await_value(rw) } else { rw };
            let iw = self.use_local(iter).any();
            let nw = self.use_local(next).any();
            let slot = self.values_slot(&[iw, nw, received]);
            let kind_addr = self.done_slot();
            let mv = self.b.ins().iconst(I32, m);
            let r = self.helper("lum_async_yield_star", &[ctx, slot, mv, kind_addr]);
            self.check_value(r[0]);
            let kind = self.b.ins().load(I32, Self::mem(), kind_addr, 0);
            let call_result = self.b.create_block();
            let special = self.b.create_block();
            let is_call = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 0);
            self.b.ins().brif(is_call, call_result, &[], special, &[]);

            self.b.switch_to_block(special);
            if m == 1 {
                // No `return` method: return the (awaited) value.
                let vw = self.await_value(r[0]);
                self.return_value(V::Any(vw));
            } else if m == 2 {
                // No `throw` method: the iterator was closed; await that,
                // then throw.
                let cw = self.await_value(r[0]);
                self.free(cw);
                let e = self.helper("lum_throw_iterator_throw", &[ctx]);
                self.check_int(e[0]);
                self.b.ins().trap();
            } else {
                self.b.ins().trap();
            }

            self.b.switch_to_block(call_result);
            let aw = self.await_value(r[0]);
            let done_addr = self.done_slot();
            let val = self.helper("lum_async_yield_star_result", &[ctx, aw, done_addr]);
            self.check_value(val[0]);
            let done = self.b.ins().load(I32, Self::mem(), done_addr, 0);
            let finished = self.b.create_block();
            let args = [BlockArg::Value(val[0])];
            self.b.ins().brif(done, finished, &[], yield_part, &args);
            self.b.switch_to_block(finished);
            if m == 1 {
                let vw = self.await_value(val[0]);
                self.return_value(V::Any(vw));
            } else {
                self.b.ins().jump(finish, &args);
            }
        }

        // A result not done: its value is yielded, then the generator's
        // resumption goes to the inner iterator.
        self.b.switch_to_block(yield_part);
        self.suspend(Some(yw), 0);
        let (vw, mode) = self.resumption();
        self.b.ins().jump(head, &[BlockArg::Value(mode), BlockArg::Value(vw)]);

        self.b.switch_to_block(finish);
        let v = V::Any(fw);
        match target {
            ir::AwaitTarget::Discard => self.free(fw),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => unreachable!("`return yield*` is hoisted"),
        }
    }

    /// The dispatch on the resume state.
    fn emit_dispatch(&mut self) {
        let Some(cx) = self.async_cx.take() else { return };
        self.b.switch_to_block(cx.dispatch);
        for (k, &blk) in cx.resumes.iter().enumerate().skip(1) {
            let is_k = self.b.ins().icmp_imm_s(IntCC::Equal, cx.state, k as i64);
            let next = self.b.create_block();
            self.b.ins().brif(is_k, blk, &[], next, &[]);
            self.b.switch_to_block(next);
        }
        self.b.ins().jump(cx.resumes[0], &[]);
    }
}

/// A prediction for a call, getter or setter (see `ExprKind::Predicted`).
pub(crate) struct Pred {
    func: usize,
    body: Vec<Stmt>,
    locals: Vec<ir::LocalId>,
}

/// The parts of a speculative region's guard (`ExprKind::SpecGuard`).
struct SpecGuard<'a> {
    arrays: &'a [ir::LocalId],
    protos: &'a [(usize, usize)],
    objs: &'a [(ir::LocalId, usize)],
    distinct: bool,
    fields: &'a [(ir::LocalId, u32)],
    nums: &'a [(ir::LocalId, ir::LocalId)],
    strs: &'a [ir::LocalId],
    char_code_at: bool,
    derived: &'a [(ir::LocalId, ir::LocalId, u32, usize)],
    methods: &'a [(Option<ir::LocalId>, usize, ir::AtomId, usize)],
    cells: &'a [ir::CellId],
    appends: &'a [ir::LocalId],
    typed_writes: &'a [ir::LocalId],
}

/// The parts of a template literal's concatenation (`a${x}b` is checked as
/// "a" + ToString(x) + "b"): strings and string conversions, added left to
/// right. `+` of other values converts them otherwise.
fn concat_parts(e: &Expr) -> Option<Vec<&Expr>> {
    let mut parts = Vec::new();
    let mut e = e;
    while let ExprKind::AnyBinary(AnyBinOp::Add, l, r) = &e.kind {
        parts.push(&**r);
        e = l;
    }
    parts.push(e);
    parts.reverse();
    // Once the first or second part is a string, every `+` concatenates,
    // converting a primitive with ToString (an object's ToPrimitive could
    // differ, so only strings and primitives).
    let string = |x: &&Expr| matches!(x.kind, ExprKind::Str(_) | ExprKind::AnyUnary(AnyUnOp::ToStr, _));
    let primitive = |x: &&Expr| {
        string(x) || matches!(&x.kind, ExprKind::Box(i) if matches!(i.ty, Ty::F64 | Ty::I32 | Ty::MaybeF64 | Ty::Bool))
    };
    (parts.len() >= 2 && parts[..2].iter().any(string) && parts.iter().all(primitive)).then_some(parts)
}

/// A part of a concatenation as `JS_ConcatStrings` takes it: a number's
/// ToString is left to it (an int32 is written as its digits).
fn concat_operand(x: &Expr) -> &Expr {
    match &x.kind {
        ExprKind::AnyUnary(AnyUnOp::ToStr, inner) if matches!(&inner.kind, ExprKind::Box(i) if matches!(i.ty, Ty::F64 | Ty::I32 | Ty::MaybeF64 | Ty::Bool)) => {
            inner
        }
        _ => x,
    }
}
