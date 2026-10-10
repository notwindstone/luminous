//! Typed intermediate representation for natively compiled functions.
//!
//! Every expression carries its machine-level type. `F64` and `Bool` are
//! unboxed values whose type is *proven* (by the checker, or by the type
//! guards of a function's JS entry). `Any` is a boxed QuickJS `JSValue` whose
//! type is unknown; operations on it have exact JS semantics (fast paths for
//! common cases, generic engine helpers otherwise).
//!
//! Lowering may rely on these invariants without re-checking anything: the
//! operand types of every node are the ones its documentation states.

use std::fmt;

/// Machine-level value types. `F64` is a JS `number` known to be one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    F64,
    Bool,
    /// Only valid as a function return type (the function returns `undefined`).
    Void,
    /// Any JS value (a reference-counted `JSValue`).
    Any,
    /// A `JSValue` known to be a typed array of this kind (represented like
    /// `Any`).
    Arr(ArrKind),
    /// A number or `undefined`, as an `f64`: `undefined` is the reserved NaN
    /// `MAYBE_F64_UNDEFINED`, which no other value of this type has. Both
    /// are NaN in arithmetic, as JS converts `undefined` to.
    MaybeF64,
}

/// The bit pattern of `undefined` as a `Ty::MaybeF64`.
pub const MAYBE_F64_UNDEFINED: u64 = 0x7ff8_0000_0000_0001;

/// Typed arrays with `number` elements, numbered as `JSTypedArrayEnum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArrKind {
    Uint8Clamped = 0,
    Int8 = 1,
    Uint8 = 2,
    Int16 = 3,
    Uint16 = 4,
    Int32 = 5,
    Uint32 = 6,
    Float32 = 10,
    Float64 = 11,
}

impl ArrKind {
    pub const ALL: [ArrKind; 9] = [
        ArrKind::Uint8Clamped,
        ArrKind::Int8,
        ArrKind::Uint8,
        ArrKind::Int16,
        ArrKind::Uint16,
        ArrKind::Int32,
        ArrKind::Uint32,
        ArrKind::Float32,
        ArrKind::Float64,
    ];

    /// The global constructor's name.
    pub fn name(self) -> &'static str {
        match self {
            ArrKind::Uint8Clamped => "Uint8ClampedArray",
            ArrKind::Int8 => "Int8Array",
            ArrKind::Uint8 => "Uint8Array",
            ArrKind::Int16 => "Int16Array",
            ArrKind::Uint16 => "Uint16Array",
            ArrKind::Int32 => "Int32Array",
            ArrKind::Uint32 => "Uint32Array",
            ArrKind::Float32 => "Float32Array",
            ArrKind::Float64 => "Float64Array",
        }
    }

    /// Bytes per element.
    pub fn size(self) -> usize {
        match self {
            ArrKind::Uint8Clamped | ArrKind::Int8 | ArrKind::Uint8 => 1,
            ArrKind::Int16 | ArrKind::Uint16 => 2,
            ArrKind::Int32 | ArrKind::Uint32 | ArrKind::Float32 => 4,
            ArrKind::Float64 => 8,
        }
    }

    pub fn from_name(name: &str) -> Option<ArrKind> {
        ArrKind::ALL.into_iter().find(|k| k.name() == name)
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Ty::F64 => "number",
            Ty::Bool => "boolean",
            Ty::Void => "void",
            Ty::Any => "any",
            Ty::Arr(k) => k.name(),
            Ty::MaybeF64 => "number | undefined",
        })
    }
}
pub type FuncId = usize;
pub type LocalId = usize;
/// Target of `break` / `continue`: a loop, a `switch`, or a labeled block.
pub type LoopId = usize;
/// Index into `Module::strings`.
pub type StrId = usize;
/// Index into `Module::atoms` (property names and global variable names).
pub type AtomId = usize;
/// Index into `Module::cells` (module-level bindings used by native code).
pub type CellId = usize;
/// Index into `Module::closures`.
pub type ClosureId = usize;

#[derive(Debug, Clone, Default)]
pub struct Module {
    /// Module-level functions (`FuncId`s `0..funcs.len()`).
    pub funcs: Vec<Func>,
    /// Closures created by native code (`ClosureId`s).
    pub closures: Vec<Func>,
    /// String constants.
    pub strings: Vec<String>,
    /// Property and global variable names, interned as engine atoms.
    pub atoms: Vec<String>,
    /// Names of the module-level bindings native code reads or writes. Each
    /// cell holds the binding's current value, or is uninitialized (TDZ).
    pub cells: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Func {
    /// JS name of the function (`.name` of its function object).
    pub name: String,
    /// JS `.length`: parameters before the first one with a default.
    pub js_length: usize,
    /// ABI types of the parameters. Locals `0..params.len()` are the
    /// parameters; a local may be `Any` where its parameter is not, in
    /// which case the incoming value is boxed on entry.
    pub params: Vec<Ty>,
    pub ret: Ty,
    /// Types of all locals, including parameters and hidden temporaries.
    pub locals: Vec<Ty>,
    /// Locals captured by closures. They live in heap boxes shared with the
    /// closures, are always `Any`, and are checked for their TDZ.
    pub boxed: Vec<bool>,
    /// Names of the boxed locals (for TDZ errors).
    pub local_names: Vec<Option<AtomId>>,
    /// Closures only: the boxed locals bound to the closure's captured
    /// boxes, in the order of its data slots.
    pub captures: Vec<LocalId>,
    pub is_closure: bool,
    /// Methods: `this` is passed in (borrowed).
    pub has_this: bool,
    /// `async`: compiled as a resumable step function over a heap frame.
    pub is_async: bool,
    /// In functions that suspend: holds the return value while `finally`
    /// blocks run on the way out (`Any`).
    pub ret_slot: Option<LocalId>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    /// `test` is evaluated before each iteration (`while`/`for`) or after
    /// each iteration (`do`-`while`, `post_test == true`). `update` runs at
    /// the end of every iteration and is the target of `continue`.
    Loop {
        id: LoopId,
        test: Option<Expr>,
        post_test: bool,
        body: Vec<Stmt>,
        update: Option<Expr>,
        /// Boxed loop variables (`for (let ...)` captured by closures): each
        /// iteration gets fresh boxes, holding the previous values.
        per_iteration: Vec<LocalId>,
    },
    /// `switch (local) { case ...: }`: cases are compared with `===` in
    /// order; `None` is `default`. Bodies fall through. `break` targets `id`.
    Switch {
        id: LoopId,
        disc: LocalId,
        cases: Vec<(Option<Expr>, Vec<Stmt>)>,
    },
    /// Fresh, uninitialized boxes for these boxed locals (entering their scope).
    NewBoxes(Vec<LocalId>),
    /// `await value` (`Any`) at statement level, then deliver the result.
    Await {
        value: Expr,
        target: AwaitTarget,
    },
    Break(LoopId),
    Continue(LoopId),
    Return(Option<Expr>),
    Block(Vec<Stmt>),
    /// `throw e` (`Any`).
    Throw(Expr),
    /// `try { body } catch (param) { handler } finally { finalizer }`. The
    /// finalizer contains no `return`, `break` or `continue`. In functions
    /// that suspend, `exc` holds the exception while the finalizer runs on
    /// the exception path (it may suspend: only locals survive that).
    Try {
        body: Vec<Stmt>,
        catch: Option<(Option<LocalId>, Vec<Stmt>)>,
        finally: Option<Vec<Stmt>>,
        exc: Option<LocalId>,
    },
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub ty: Ty,
    pub kind: ExprKind,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Num(f64),
    Bool(bool),
    Local(LocalId),
    /// Assignment; evaluates to the assigned value. The value has the
    /// local's type.
    SetLocal(LocalId, Box<Expr>),
    /// `++x`, `x--`, ... on a `F64` local.
    Update {
        local: LocalId,
        delta: f64,
        prefix: bool,
    },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// Comparison of two `F64` or two `Bool` operands; result is `Bool`.
    Compare(CmpOp, Box<Expr>, Box<Expr>),
    /// `&&` / `||` / `??` with JS short-circuit semantics. Both operands and
    /// the result have the same type; `??` is only used on `Any`.
    Logical(LogicalOp, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    /// JS truthiness of any value, as a `Bool`.
    Truthy(Box<Expr>),
    /// Direct call of a native function; argument types match its params.
    Call(FuncId, Vec<Expr>),
    Math(MathFn, Vec<Expr>),
    /// Comma operator: all evaluated, value of the last.
    Seq(Vec<Expr>),

    // ---- dynamic values (all `Any` unless noted) ----
    Undefined,
    Null,
    Str(StrId),
    /// `F64` or `Bool` as a JS value.
    Box(Box<Expr>),
    AnyUnary(AnyUnOp, Box<Expr>),
    AnyBinary(AnyBinOp, Box<Expr>, Box<Expr>),
    /// Comparison of two `Any` operands; result is `Bool`.
    AnyCompare(AnyCmpOp, Box<Expr>, Box<Expr>),
    /// Read of a module-level binding (ReferenceError while uninitialized).
    Cell(CellId),
    SetCell(CellId, Box<Expr>),
    /// Global variable (ReferenceError if it does not exist).
    Global(AtomId),
    /// `typeof` operand: a global variable that may not exist (undefined).
    GlobalOrUndefined(AtomId),
    SetGlobal(AtomId, Box<Expr>),
    GetProp(Box<Expr>, AtomId),
    /// Evaluates to the assigned value.
    SetProp(Box<Expr>, AtomId, Box<Expr>),
    GetElem(Box<Expr>, Box<Expr>),
    SetElem(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `obj.length`, with inline paths for arrays, typed arrays and strings.
    Length(Box<Expr>, AtomId),
    /// `arr[index]` on an `Arr` with an `F64` index (`MaybeF64`).
    TypedGet(ArrKind, Box<Expr>, Box<Expr>),
    /// `arr[index] = value` on an `Arr`, `F64` index and value (`F64`).
    TypedSet(ArrKind, Box<Expr>, Box<Expr>, Box<Expr>),
    /// `delete obj.prop` / `delete obj[key]` (`Bool`; TypeError if it fails).
    DeleteProp(Box<Expr>, AtomId),
    DeleteElem(Box<Expr>, Box<Expr>),
    /// `callee(args)` with `this` undefined.
    CallValue(Box<Expr>, Vec<Expr>),
    /// `f(args)` with an explicit `this`.
    CallThis(Box<Expr>, Box<Expr>, Vec<Expr>),
    /// `obj.name(args)` / `obj[key](args)`, with `this` = obj.
    CallMethod(Box<Expr>, Key, Vec<Expr>),
    New(Box<Expr>, Vec<Expr>),
    /// Object literal with plain data properties, defined in order.
    Object(Vec<(AtomId, Expr)>),
    /// Array literal without holes or spreads.
    Array(Vec<Expr>),
    /// Initialization of a local by its declaration (no TDZ check).
    InitLocal(LocalId, Box<Expr>),
    /// The receiver of a method.
    This,
    /// A closure: function `ClosureId` capturing these boxed locals.
    Closure(ClosureId, Vec<LocalId>),
    /// Discard the value (`void e`, statement-level use of a void call).
    Void(Box<Expr>),
}

/// Where an awaited value goes.
#[derive(Debug, Clone, Copy)]
pub enum AwaitTarget {
    /// `await e;`
    Discard,
    /// `x = await e;` (an `Any` local)
    Set(LocalId),
    /// `const x = await e;` (an `Any` local)
    Init(LocalId),
    /// `return await e;`
    Return,
}

#[derive(Debug, Clone)]
pub enum Key {
    Atom(AtomId),
    Value(Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnyUnOp {
    Neg,
    Plus,
    BitNot,
    Inc,
    Dec,
    ToNumeric,
    Typeof,
    /// ToString, as in template literals.
    ToStr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnyBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Shl,
    Sar,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnyCmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    In,
    InstanceOf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// Numeric negation.
    Neg,
    /// Boolean not (operand is `Bool`).
    Not,
    /// `~x` (ToInt32, bitwise not).
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Sar,
    Shr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    /// `??` (only on `Any`).
    Nullish,
}

/// Pure numeric intrinsics. Builtins are assumed unmodified at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathFn {
    // Lowered inline.
    Abs,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    Fround,
    /// Two operands; n-ary `Math.min/max` are folded into chains.
    Min,
    Max,
    Imul,
    Clz32,
    Sign,
    IsNaN,
    IsFinite,
    IsInteger,
    IsSafeInteger,
    // Lowered to calls (libm or runtime helpers with exact JS semantics).
    Round,
    Pow,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    Exp,
    Expm1,
    Log,
    Log1p,
    Log2,
    Log10,
    Cbrt,
    Hypot,
}

impl MathFn {
    pub fn arity(self) -> usize {
        match self {
            MathFn::Min | MathFn::Max | MathFn::Imul | MathFn::Pow | MathFn::Atan2 | MathFn::Hypot => 2,
            _ => 1,
        }
    }

    pub fn ret(self) -> Ty {
        match self {
            MathFn::IsNaN | MathFn::IsFinite | MathFn::IsInteger | MathFn::IsSafeInteger => Ty::Bool,
            _ => Ty::F64,
        }
    }

    /// Name of the external C symbol implementing this function, if it is
    /// not lowered inline.
    pub fn symbol(self) -> Option<&'static str> {
        Some(match self {
            MathFn::Round => "tsaot_round",
            MathFn::Pow => "tsaot_pow",
            MathFn::Sin => "sin",
            MathFn::Cos => "cos",
            MathFn::Tan => "tan",
            MathFn::Asin => "asin",
            MathFn::Acos => "acos",
            MathFn::Atan => "atan",
            MathFn::Atan2 => "atan2",
            MathFn::Sinh => "sinh",
            MathFn::Cosh => "cosh",
            MathFn::Tanh => "tanh",
            MathFn::Asinh => "asinh",
            MathFn::Acosh => "acosh",
            MathFn::Atanh => "atanh",
            MathFn::Exp => "exp",
            MathFn::Expm1 => "expm1",
            MathFn::Log => "log",
            MathFn::Log1p => "log1p",
            MathFn::Log2 => "log2",
            MathFn::Log10 => "log10",
            MathFn::Cbrt => "cbrt",
            MathFn::Hypot => "hypot",
            _ => return None,
        })
    }
}

impl Expr {
    pub fn new(ty: Ty, kind: ExprKind) -> Self {
        Expr { ty, kind }
    }
    pub fn num(v: f64) -> Self {
        Expr::new(Ty::F64, ExprKind::Num(v))
    }
}

/// Visit `x` and every expression in it.
pub fn visit(x: &Expr, f: &mut impl FnMut(&Expr)) {
    f(x);
    use ExprKind::*;
    match &x.kind {
        Num(_)
        | Bool(_)
        | Local(_)
        | Update { .. }
        | Undefined
        | Null
        | Str(_)
        | Cell(_)
        | Global(_)
        | GlobalOrUndefined(_)
        | This
        | Closure(..) => {}
        SetLocal(_, a)
        | InitLocal(_, a)
        | Unary(_, a)
        | Truthy(a)
        | Box(a)
        | AnyUnary(_, a)
        | SetCell(_, a)
        | SetGlobal(_, a)
        | GetProp(a, _)
        | Length(a, _)
        | DeleteProp(a, _)
        | Void(a) => visit(a, f),
        TypedGet(_, a, b) => {
            visit(a, f);
            visit(b, f);
        }
        TypedSet(_, a, b, c) => {
            visit(a, f);
            visit(b, f);
            visit(c, f);
        }
        Binary(_, a, b)
        | Compare(_, a, b)
        | Logical(_, a, b)
        | AnyBinary(_, a, b)
        | AnyCompare(_, a, b)
        | SetProp(a, _, b)
        | GetElem(a, b)
        | DeleteElem(a, b) => {
            visit(a, f);
            visit(b, f);
        }
        Cond(a, b, c) | SetElem(a, b, c) => {
            visit(a, f);
            visit(b, f);
            visit(c, f);
        }
        Call(_, xs) | Math(_, xs) | Seq(xs) | Array(xs) => xs.iter().for_each(|x| visit(x, f)),
        CallValue(a, xs) | New(a, xs) => {
            visit(a, f);
            xs.iter().for_each(|x| visit(x, f));
        }
        CallThis(a, b, xs) => {
            visit(a, f);
            visit(b, f);
            xs.iter().for_each(|x| visit(x, f));
        }
        CallMethod(a, k, xs) => {
            visit(a, f);
            if let Key::Value(k) = k {
                visit(k, f);
            }
            xs.iter().for_each(|x| visit(x, f));
        }
        Object(props) => props.iter().for_each(|(_, x)| visit(x, f)),
    }
}

/// Whether evaluating `x` may assign local `l` (only the function's own code
/// assigns a local that is not captured).
pub fn assigns_local(x: &Expr, l: LocalId) -> bool {
    let mut found = false;
    visit(x, &mut |e| {
        if let ExprKind::SetLocal(m, _) | ExprKind::InitLocal(m, _) | ExprKind::Update { local: m, .. } = e.kind
            && m == l
        {
            found = true;
        }
    });
    found
}
