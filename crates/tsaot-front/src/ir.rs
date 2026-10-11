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
    /// A number known to be an int32 (never -0): the `int` type, and the
    /// results of bitwise operators. A subtype of `F64` wherever numbers
    /// are expected; represented as a sign-extended `i64` in locals, block
    /// parameters, arguments and returns.
    I32,
}

impl Ty {
    /// `F64` or `I32`: a number that is never `undefined`.
    pub fn is_number(self) -> bool {
        matches!(self, Ty::F64 | Ty::I32)
    }
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
            Ty::I32 => "int",
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
    /// Private name slots (see `ExprKind::PrivateField`).
    pub privates: usize,
    /// Home object slots: for class `k`, its prototype (`2k`) and itself
    /// (`2k + 1`, for static methods).
    pub homes: usize,
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
    /// The last parameter collects the remaining arguments in an array.
    pub rest: bool,
    /// `function*`: compiled as a resumable step function over a heap
    /// frame, as async functions are.
    pub is_generator: bool,
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
    /// In a generator: `yield value` / `yield* value`, then deliver what
    /// the generator is resumed with (`target`: not `Return`); or the
    /// initial suspension, after the parameters are bound.
    Yield {
        value: Expr,
        kind: YieldKind,
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
    /// `body`, after which nothing more is read from the iterator in local
    /// `iter`: when `body` completes abruptly (by an exception, or a jump or
    /// `return` leaving it; not a `continue` of loop `loop_id`), the
    /// iterator is closed unless it is done (`undefined`).
    IterGuard {
        iter: LocalId,
        next: LocalId,
        loop_id: Option<LoopId>,
        body: Vec<Stmt>,
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
    /// The declaration of a module-level binding (ends its TDZ).
    InitCell(CellId, Box<Expr>),
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
    /// Array literal with holes or spreads.
    ArrayParts(Vec<ArrPart>),
    /// Object literal with spreads or computed keys, defined in order.
    ObjectParts(Vec<ObjPart>),
    /// `f(...args)` / `obj.m(...args)`: the arguments as an array, and
    /// `this` (`Undefined` for plain calls).
    CallSpread(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `new F(...args)`, the arguments as an array.
    NewSpread(Box<Expr>, Box<Expr>),
    /// Start iterating a value (`GetIterator`, sync): the iterator and its
    /// `next` method go to locals `iter` and `next` (`next`: the atom).
    /// An array with the shape of a fresh one is iterated directly instead
    /// (while array iteration is the original one): `iter` is the array and `next` the
    /// index (an int). Evaluates to `undefined`.
    IterOpen(Box<Expr>, LocalId, LocalId, AtomId),
    /// The `for`-`in` iterator of a value.
    ForInIterator(Box<Expr>),
    /// The next value of the iterator in local `iter` (with `next` from
    /// `IterOpen`, or a `for`-`in` iterator without one). When the iterator is
    /// done, or throws, `iter` becomes `undefined`; the value is then
    /// `undefined`.
    IterStep(LocalId, Option<LocalId>),
    /// Close the iterator in `iter` unless it is `undefined` (done), and
    /// make it `undefined`. Evaluates to `undefined`.
    IterClose(LocalId, LocalId),
    /// ToObject.
    ToObject(Box<Expr>),
    /// ToPropertyKey.
    ToPropKey(Box<Expr>),
    /// `obj[key]` in a `for`-`in` loop over the iterator in local `iter`
    /// whose current key `key` may be (a direct read when it is, and obj has
    /// the shape being enumerated).
    ForInGet(LocalId, Box<Expr>, Box<Expr>),
    /// Private names of classes (`Module::privates` slots, filled when the
    /// class is defined): a field's value (`obj.#x`), its assignment (to
    /// the value), a method after the brand check, a getter's result, a
    /// setter call (to the value), `#x in obj` (`Bool`).
    PrivateField(Box<Expr>, usize),
    PrivateSetField(Box<Expr>, usize, Box<Expr>),
    PrivateMethod(Box<Expr>, usize),
    PrivateGetter(Box<Expr>, usize),
    PrivateSetter(Box<Expr>, usize, Box<Expr>),
    PrivateIn(Box<Expr>, usize),
    /// `super[key]` in a method (home object: `Module::homes` slot), and
    /// its assignment (to the value).
    SuperGet(usize, Box<Expr>),
    SuperSet(usize, Box<Expr>, Box<Expr>),
    /// Append a value to the array in a local (array rest; `undefined`).
    ArrayPush(LocalId, Box<Expr>),
    /// A new object with the own enumerable properties of the source but
    /// the given keys (object rest).
    ObjectRest(Box<Expr>, Vec<Expr>),
    /// Initialization of a local by its declaration (no TDZ check).
    InitLocal(LocalId, Box<Expr>),
    /// The receiver of a method.
    This,
    /// A closure: function `ClosureId` capturing these boxed locals.
    Closure(ClosureId, Vec<LocalId>),
    /// Discard the value (`void e`, statement-level use of a void call).
    Void(Box<Expr>),
    /// The async iterator of a value (`Symbol.asyncIterator`, else its
    /// sync iterator wrapped), for `for await`.
    AsyncIterator(Box<Expr>),
    /// An awaited iterator result of `for await`: its `value`, its `done`
    /// going to the `Bool` local (TypeError if it is not an object).
    AsyncIterResult(Box<Expr>, LocalId),
    /// TypeError unless the value is an object (`undefined` otherwise).
    CheckObject(Box<Expr>),
    /// Call the `return` method of the iterator in the local, if any,
    /// ignoring what happens (an exception is propagating).
    IterCloseQuiet(LocalId),
    /// A JS value that is an int32 (a bitwise operator applied to a number
    /// and anything else), as an `I32`.
    IntOf(Box<Expr>),
}

/// What a `Stmt::Yield` does.
#[derive(Debug, Clone, Copy)]
pub enum YieldKind {
    Value,
    /// `yield*`: the iterator of the value and its `next` method go to
    /// these locals.
    Delegate {
        iter: LocalId,
        next: LocalId,
        next_atom: AtomId,
    },
    /// Suspends when the generator is created; resumed by the first `next`.
    Initial,
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

/// An element of an array literal.
#[derive(Debug, Clone)]
pub enum ArrPart {
    Value(Expr),
    Spread(Expr),
    Hole,
}

/// A property of an object literal.
#[derive(Debug, Clone)]
pub enum ObjPart {
    Field(AtomId, Expr),
    /// `[key]: value` (the key is converted when the property is defined).
    Computed(Expr, Expr),
    Spread(Expr),
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
            MathFn::Imul | MathFn::Clz32 => Ty::I32,
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

macro_rules! visitor {
    ($(#[$doc:meta])* $name:ident $($m:ident)?) => {
        $(#[$doc])*
        pub fn $name(x: & $($m)? Expr, f: &mut impl FnMut(& $($m)? Expr)) {
            f(x);
            use ExprKind::*;
            match & $($m)? x.kind {
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
                | IterStep(..)
                | IterClose(..)
                | IterCloseQuiet(_)
                | Closure(..) => {}
                SetLocal(_, a)
                | InitLocal(_, a)
                | Unary(_, a)
                | Truthy(a)
                | Box(a)
                | AnyUnary(_, a)
                | SetCell(_, a)
                | InitCell(_, a)
                | SetGlobal(_, a)
                | GetProp(a, _)
                | Length(a, _)
                | DeleteProp(a, _)
                | IterOpen(a, ..)
                | ForInIterator(a)
                | ToObject(a)
                | ToPropKey(a)
                | PrivateField(a, _)
                | PrivateMethod(a, _)
                | PrivateGetter(a, _)
                | PrivateIn(a, _)
                | SuperGet(_, a)
                | ArrayPush(_, a)
                | IntOf(a)
                | AsyncIterator(a)
                | AsyncIterResult(a, _)
                | CheckObject(a)
                | Void(a) => $name(a, f),
                ArrayParts(parts) => {
                    for p in parts {
                        if let ArrPart::Value(x) | ArrPart::Spread(x) = p {
                            $name(x, f);
                        }
                    }
                }
                ObjectParts(parts) => {
                    for p in parts {
                        match p {
                            ObjPart::Field(_, x) | ObjPart::Spread(x) => $name(x, f),
                            ObjPart::Computed(k, x) => {
                                $name(k, f);
                                $name(x, f);
                            }
                        }
                    }
                }
                ObjectRest(a, xs) => {
                    $name(a, f);
                    for x in xs {
                        $name(x, f);
                    }
                }
                ForInGet(_, a, b) => {
                    $name(a, f);
                    $name(b, f);
                }
                PrivateSetField(a, _, b) | PrivateSetter(a, _, b) | SuperSet(_, a, b) => {
                    $name(a, f);
                    $name(b, f);
                }
                NewSpread(a, b) => {
                    $name(a, f);
                    $name(b, f);
                }
                CallSpread(a, b, c) => {
                    $name(a, f);
                    $name(b, f);
                    $name(c, f);
                }
                TypedGet(_, a, b) => {
                    $name(a, f);
                    $name(b, f);
                }
                TypedSet(_, a, b, c) => {
                    $name(a, f);
                    $name(b, f);
                    $name(c, f);
                }
                Binary(_, a, b)
                | Compare(_, a, b)
                | Logical(_, a, b)
                | AnyBinary(_, a, b)
                | AnyCompare(_, a, b)
                | SetProp(a, _, b)
                | GetElem(a, b)
                | DeleteElem(a, b) => {
                    $name(a, f);
                    $name(b, f);
                }
                Cond(a, b, c) | SetElem(a, b, c) => {
                    $name(a, f);
                    $name(b, f);
                    $name(c, f);
                }
                Call(_, xs) | Math(_, xs) | Seq(xs) | Array(xs) => {
                    for x in xs {
                        $name(x, f);
                    }
                }
                CallValue(a, xs) | New(a, xs) => {
                    $name(a, f);
                    for x in xs {
                        $name(x, f);
                    }
                }
                CallThis(a, b, xs) => {
                    $name(a, f);
                    $name(b, f);
                    for x in xs {
                        $name(x, f);
                    }
                }
                CallMethod(a, k, xs) => {
                    $name(a, f);
                    if let Key::Value(k) = k {
                        $name(k, f);
                    }
                    for x in xs {
                        $name(x, f);
                    }
                }
                Object(props) => {
                    for (_, x) in props {
                        $name(x, f);
                    }
                }
            }
        }
    };
}

visitor!(
    /// Visit `x` and every expression in it.
    visit
);
visitor!(
    /// Visit `x` and every expression in it, for rewriting (`f` sees a
    /// node before its children, which it may replace).
    visit_mut mut
);

/// Whether evaluating `x` may assign local `l` (only the function's own code
/// assigns a local that is not captured).
pub fn assigns_local(x: &Expr, l: LocalId) -> bool {
    let mut found = false;
    visit(x, &mut |e| {
        found |= match e.kind {
            ExprKind::SetLocal(m, _) | ExprKind::InitLocal(m, _) | ExprKind::Update { local: m, .. } => m == l,
            ExprKind::IterStep(m, n) => m == l || n == Some(l),
            ExprKind::AsyncIterResult(_, m) => m == l,
            ExprKind::IterOpen(_, m, n, _) | ExprKind::IterClose(m, n) => m == l || n == l,
            _ => false,
        };
    });
    found
}

macro_rules! stmt_visitor {
    ($(#[$doc:meta])* $stmts:ident $exprs:ident $iter:ident $($m:ident)?) => {
        $(#[$doc])*
        pub fn $stmts(stmts: & $($m)? [Stmt], f: &mut impl FnMut(& $($m)? Expr)) {
            for s in stmts {
                match s {
                    Stmt::Expr(x) | Stmt::Throw(x) => $exprs(x, f),
                    Stmt::Return(x) => x.$iter().for_each(|x| $exprs(x, f)),
                    Stmt::Await { value, .. } | Stmt::Yield { value, .. } => $exprs(value, f),
                    Stmt::If(c, a, b) => {
                        $exprs(c, f);
                        $stmts(a, f);
                        $stmts(b, f);
                    }
                    Stmt::Loop { test, body, update, .. } => {
                        test.$iter().for_each(|x| $exprs(x, f));
                        $stmts(body, f);
                        update.$iter().for_each(|x| $exprs(x, f));
                    }
                    Stmt::Switch { cases, .. } => {
                        for (c, body) in cases {
                            c.$iter().for_each(|x| $exprs(x, f));
                            $stmts(body, f);
                        }
                    }
                    Stmt::Block(body) | Stmt::IterGuard { body, .. } => $stmts(body, f),
                    Stmt::Try { body, catch, finally, .. } => {
                        $stmts(body, f);
                        catch.$iter().for_each(|(_, c)| $stmts(c, f));
                        finally.$iter().for_each(|c| $stmts(c, f));
                    }
                    Stmt::NewBoxes(_) | Stmt::Break(_) | Stmt::Continue(_) => {}
                }
            }
        }
    };
}

stmt_visitor!(
    /// Call `f` on every expression (and subexpression) in `stmts`.
    visit_stmts visit iter
);
stmt_visitor!(
    /// `visit_stmts` for rewriting (see `visit_mut`).
    visit_stmts_mut visit_mut iter_mut mut
);

/// Calls `f` on the locals named by statements themselves (not by their
/// expressions).
fn stmt_locals(stmts: &[Stmt], f: &mut impl FnMut(LocalId)) {
    let target = |t: &AwaitTarget, f: &mut dyn FnMut(LocalId)| {
        if let AwaitTarget::Set(l) | AwaitTarget::Init(l) = t {
            f(*l);
        }
    };
    for s in stmts {
        match s {
            Stmt::Expr(_) | Stmt::Throw(_) | Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Await { target: t, .. } => target(t, f),
            Stmt::Yield { kind, target: t, .. } => {
                target(t, f);
                if let YieldKind::Delegate { iter, next, .. } = kind {
                    f(*iter);
                    f(*next);
                }
            }
            Stmt::If(_, a, b) => {
                stmt_locals(a, f);
                stmt_locals(b, f);
            }
            Stmt::Loop { body, per_iteration, .. } => {
                per_iteration.iter().for_each(|&l| f(l));
                stmt_locals(body, f);
            }
            Stmt::Switch { disc, cases, .. } => {
                f(*disc);
                cases.iter().for_each(|(_, body)| stmt_locals(body, f));
            }
            Stmt::NewBoxes(ls) => ls.iter().for_each(|&l| f(l)),
            Stmt::Block(body) => stmt_locals(body, f),
            Stmt::IterGuard { iter, next, body, .. } => {
                f(*iter);
                f(*next);
                stmt_locals(body, f);
            }
            Stmt::Try { body, catch, finally, exc } => {
                stmt_locals(body, f);
                if let Some((param, c)) = catch {
                    param.iter().for_each(|&l| f(l));
                    stmt_locals(c, f);
                }
                finally.iter().for_each(|c| stmt_locals(c, f));
                exc.iter().for_each(|&l| f(l));
            }
        }
    }
}

/// Calls `f` on the expressions that are statements.
fn top_exprs(stmts: &[Stmt], f: &mut impl FnMut(&Expr)) {
    for s in stmts {
        match s {
            Stmt::Expr(e) => f(e),
            Stmt::If(_, a, b) => {
                top_exprs(a, f);
                top_exprs(b, f);
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => top_exprs(body, f),
            Stmt::Switch { cases, .. } => cases.iter().for_each(|(_, b)| top_exprs(b, f)),
            Stmt::Try { body, catch, finally, .. } => {
                top_exprs(body, f);
                catch.iter().for_each(|(_, c)| top_exprs(c, f));
                finally.iter().for_each(|c| top_exprs(c, f));
            }
            _ => {}
        }
    }
}

/// Scalar replacement: a local initialized with an object literal and only
/// ever used as `local.key` (reads and writes of the literal's own keys)
/// holds an object nothing else can observe, so each property becomes a
/// local of its own. (Unboxed locals are only used after their declaration,
/// which evaluates the literal.) It may be reassigned other literals with
/// the same keys, in statements.
pub fn scalar_replace(f: &mut Func) {
    let n = f.locals.len();
    let mut ok: Vec<bool> =
        (0..n).map(|l| l >= f.params.len() && !f.boxed[l] && f.locals[l] == Ty::Any && f.ret_slot != Some(l)).collect();
    let mut inits = vec![0; n];
    // Reassignments with object literals, all of them / as statements.
    let mut sets = vec![0; n];
    let mut top_sets = vec![0; n];
    top_exprs(&f.body, &mut |e| {
        if let ExprKind::SetLocal(l, x) = &e.kind
            && let ExprKind::Object(_) = x.kind
        {
            top_sets[*l] += 1;
        }
    });
    let mut keys: Vec<Vec<AtomId>> = vec![Vec::new(); n];
    let mut used: Vec<Vec<AtomId>> = vec![Vec::new(); n];
    let mut as_object = vec![0; n];
    let mut total = vec![0; n];
    stmt_locals(&f.body, &mut |l| ok[l] = false);
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::InitLocal(l, x) | ExprKind::SetLocal(l, x) => match &x.kind {
            ExprKind::Object(props) => {
                if let ExprKind::InitLocal(..) = e.kind {
                    inits[*l] += 1;
                } else {
                    sets[*l] += 1;
                    // Fields are assigned one by one: values must not read
                    // the old ones.
                    for (_, v) in props {
                        visit(v, &mut |x| {
                            if matches!(x.kind, ExprKind::Local(m) if m == *l) {
                                ok[*l] = false;
                            }
                        });
                    }
                }
                let mut ks: Vec<AtomId> = props.iter().map(|(k, _)| *k).collect();
                if keys[*l].is_empty() {
                    keys[*l] = ks.clone();
                }
                // Every literal has the same keys.
                let mut have = keys[*l].clone();
                ks.sort_unstable();
                ks.dedup();
                have.sort_unstable();
                have.dedup();
                if ks != have {
                    ok[*l] = false;
                }
            }
            _ => ok[*l] = false,
        },
        ExprKind::Update { local: l, .. } | ExprKind::ArrayPush(l, _) => ok[*l] = false,
        ExprKind::IterOpen(_, a, b, _) | ExprKind::IterClose(a, b) => {
            ok[*a] = false;
            ok[*b] = false;
        }
        ExprKind::IterStep(a, b) => {
            ok[*a] = false;
            b.iter().for_each(|&b| ok[b] = false);
        }
        ExprKind::ForInGet(a, ..) => ok[*a] = false,
        ExprKind::AsyncIterResult(_, a) | ExprKind::IterCloseQuiet(a) => ok[*a] = false,
        ExprKind::Closure(_, ls) => ls.iter().for_each(|&l| ok[l] = false),
        ExprKind::GetProp(o, k) | ExprKind::SetProp(o, k, _) => {
            if let ExprKind::Local(l) = o.kind {
                as_object[l] += 1;
                used[l].push(*k);
            }
        }
        ExprKind::Local(l) => total[*l] += 1,
        _ => {}
    });
    // Per replaced local, its keys' new locals.
    let mut fields: Vec<Option<Vec<(AtomId, LocalId)>>> = vec![None; n];
    for l in 0..n {
        if ok[l]
            && inits[l] == 1
            && sets[l] == top_sets[l]
            && total[l] == as_object[l]
            && used[l].iter().all(|k| keys[l].contains(k))
        {
            let mut map: Vec<(AtomId, LocalId)> = Vec::new();
            for &k in &keys[l] {
                if !map.iter().any(|&(m, _)| m == k) {
                    map.push((k, f.locals.len()));
                    f.locals.push(Ty::Any);
                    f.boxed.push(false);
                    f.local_names.push(None);
                }
            }
            fields[l] = Some(map);
        }
    }
    if fields.iter().all(Option::is_none) {
        return;
    }
    let field = |l: LocalId, k: AtomId| -> Option<LocalId> {
        fields[l].as_ref().map(|m| m.iter().find(|&&(m, _)| m == k).expect("a literal key").1)
    };
    visit_stmts_mut(&mut f.body, &mut |e| {
        let init = matches!(e.kind, ExprKind::InitLocal(..));
        let new = match &mut e.kind {
            // (New locals are past `fields`.)
            ExprKind::InitLocal(l, x) | ExprKind::SetLocal(l, x) if fields.get(*l).is_some_and(Option::is_some) => {
                let ExprKind::Object(props) = std::mem::replace(&mut x.kind, ExprKind::Undefined) else {
                    unreachable!()
                };
                let mut seq: Vec<Expr> = props
                    .into_iter()
                    .map(|(k, v)| {
                        let (fl, v) = (field(*l, k).unwrap(), Box::new(v));
                        Expr::new(Ty::Any, if init { ExprKind::InitLocal(fl, v) } else { ExprKind::SetLocal(fl, v) })
                    })
                    .collect();
                seq.push(Expr::new(Ty::Any, ExprKind::Undefined));
                ExprKind::Seq(seq)
            }
            ExprKind::GetProp(o, k) => match o.kind {
                ExprKind::Local(l) if fields[l].is_some() => ExprKind::Local(field(l, *k).unwrap()),
                _ => return,
            },
            ExprKind::SetProp(o, k, v) => match o.kind {
                ExprKind::Local(l) if fields[l].is_some() => {
                    let v = std::mem::replace(&mut **v, Expr::new(Ty::Any, ExprKind::Undefined));
                    ExprKind::SetLocal(field(l, *k).unwrap(), Box::new(v))
                }
                _ => return,
            },
            _ => return,
        };
        e.kind = new;
    });
}

/// A number written to an `Any` local `l` that `retype_numbers` can turn
/// into an `F64` one: a boxed number, `l` plus or minus one, or `+ - * /`
/// on `l` and boxed numbers.
fn number_write(l: LocalId, v: &Expr) -> bool {
    let operand = |x: &Expr| match &x.kind {
        ExprKind::Local(m) => *m == l,
        ExprKind::Box(e) => e.ty.is_number(),
        _ => false,
    };
    match &v.kind {
        ExprKind::Box(e) => e.ty.is_number(),
        ExprKind::AnyUnary(AnyUnOp::Inc | AnyUnOp::Dec, x) => matches!(x.kind, ExprKind::Local(m) if m == l),
        ExprKind::AnyBinary(AnyBinOp::Add | AnyBinOp::Sub | AnyBinOp::Mul | AnyBinOp::Div, a, b) => {
            operand(a) && operand(b)
        }
        _ => false,
    }
}

/// `number_write` operand `x` of local `l`, as an `F64` expression.
fn number_operand(l: LocalId, x: Expr) -> Expr {
    match x.kind {
        ExprKind::Box(e) => *e,
        ExprKind::Local(m) if m == l => Expr::new(Ty::F64, ExprKind::Local(l)),
        _ => unreachable!("not a number operand"),
    }
}

/// The `Any` locals among `cands` that only ever hold numbers (every write
/// is a `number_write`) become `F64` locals; their reads are boxed. Whether
/// any did.
fn retype_numbers(f: &mut Func, cands: &[LocalId]) -> bool {
    let mut ok: Vec<bool> = (0..f.locals.len()).map(|l| cands.contains(&l)).collect();
    visit_stmts(&f.body, &mut |e| {
        if let ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v) = &e.kind
            && !number_write(*l, v)
        {
            ok[*l] = false;
        }
    });
    if !ok.iter().any(|&b| b) {
        return false;
    }
    for (l, &ok) in ok.iter().enumerate() {
        if ok {
            f.locals[l] = Ty::F64;
        }
    }
    let undefined = || Expr::new(Ty::Any, ExprKind::Undefined);
    visit_stmts_mut(&mut f.body, &mut |e| {
        let init = matches!(e.kind, ExprKind::InitLocal(..));
        let num = match &mut e.kind {
            // (Reads inside rewritten writes are `F64` already.)
            ExprKind::Local(l) if ok[*l] && e.ty == Ty::Any => Expr::new(Ty::F64, ExprKind::Local(*l)),
            ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v) if ok[*l] && e.ty == Ty::Any => {
                let l = *l;
                let v = std::mem::replace(&mut **v, undefined());
                let kind = match v.kind {
                    ExprKind::AnyUnary(op, _) => {
                        ExprKind::Update { local: l, delta: if op == AnyUnOp::Inc { 1.0 } else { -1.0 }, prefix: true }
                    }
                    ExprKind::AnyBinary(op, a, b) => {
                        let op = match op {
                            AnyBinOp::Add => BinOp::Add,
                            AnyBinOp::Sub => BinOp::Sub,
                            AnyBinOp::Mul => BinOp::Mul,
                            _ => BinOp::Div,
                        };
                        let x = Expr::new(
                            Ty::F64,
                            ExprKind::Binary(op, Box::new(number_operand(l, *a)), Box::new(number_operand(l, *b))),
                        );
                        if init { ExprKind::InitLocal(l, Box::new(x)) } else { ExprKind::SetLocal(l, Box::new(x)) }
                    }
                    _ => {
                        let x = number_operand(l, v);
                        if init { ExprKind::InitLocal(l, Box::new(x)) } else { ExprKind::SetLocal(l, Box::new(x)) }
                    }
                };
                Expr::new(Ty::F64, kind)
            }
            _ => return,
        };
        e.kind = ExprKind::Box(Box::new(num));
    });
    true
}

/// Generic arithmetic on two boxed numbers becomes number arithmetic (the
/// same operation: both operands are numbers). Whether anything changed.
fn simplify_boxed_arith(f: &mut Func) -> bool {
    let mut changed = false;
    loop {
        let mut again = false;
        visit_stmts_mut(&mut f.body, &mut |e| {
            let ExprKind::AnyBinary(op, x, y) = &mut e.kind else { return };
            let (ExprKind::Box(a), ExprKind::Box(b)) = (&x.kind, &y.kind) else { return };
            if !a.ty.is_number() || !b.ty.is_number() {
                return;
            }
            let op = *op;
            let take = |x: &mut Box<Expr>| match std::mem::replace(&mut x.kind, ExprKind::Undefined) {
                ExprKind::Box(inner) => *inner,
                _ => unreachable!(),
            };
            let (a, b) = (take(x), take(y));
            let (bop, ty) = match op {
                AnyBinOp::Add => (BinOp::Add, Ty::F64),
                AnyBinOp::Sub => (BinOp::Sub, Ty::F64),
                AnyBinOp::Mul => (BinOp::Mul, Ty::F64),
                AnyBinOp::Div => (BinOp::Div, Ty::F64),
                AnyBinOp::Rem => (BinOp::Rem, Ty::F64),
                AnyBinOp::Pow => (BinOp::Pow, Ty::F64),
                AnyBinOp::Shr => (BinOp::Shr, Ty::F64),
                AnyBinOp::Shl => (BinOp::Shl, Ty::I32),
                AnyBinOp::Sar => (BinOp::Sar, Ty::I32),
                AnyBinOp::BitAnd => (BinOp::BitAnd, Ty::I32),
                AnyBinOp::BitOr => (BinOp::BitOr, Ty::I32),
                AnyBinOp::BitXor => (BinOp::BitXor, Ty::I32),
            };
            let num = Expr::new(ty, ExprKind::Binary(bop, Box::new(a), Box::new(b)));
            e.kind = ExprKind::Box(Box::new(num));
            again = true;
        });
        if !again {
            return changed;
        }
        changed = true;
    }
}

/// Untyped locals that only ever hold numbers become number locals (see
/// `retype_numbers`), and arithmetic on boxed numbers number arithmetic,
/// until neither changes anything. Candidates are declared (an `InitLocal`
/// precedes every read of them) and used only as plain locals.
pub fn number_locals(f: &mut Func) {
    let n = f.locals.len();
    let mut special = vec![false; n];
    stmt_locals(&f.body, &mut |l| special[l] = true);
    let mut declared = vec![false; n];
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::InitLocal(l, _) => declared[*l] = true,
        ExprKind::IterOpen(_, a, b, _) | ExprKind::IterClose(a, b) => {
            special[*a] = true;
            special[*b] = true;
        }
        ExprKind::IterStep(a, b) => {
            special[*a] = true;
            b.iter().for_each(|&b| special[b] = true);
        }
        ExprKind::ForInGet(a, ..)
        | ExprKind::AsyncIterResult(_, a)
        | ExprKind::IterCloseQuiet(a)
        | ExprKind::ArrayPush(a, _) => special[*a] = true,
        ExprKind::Closure(_, ls) => ls.iter().for_each(|&l| special[l] = true),
        _ => {}
    });
    let cands: Vec<LocalId> = (f.params.len()..n)
        .filter(|&l| !f.boxed[l] && f.locals[l] == Ty::Any && f.ret_slot != Some(l) && !special[l] && declared[l])
        .collect();
    loop {
        let simplified = simplify_boxed_arith(f);
        let cands: Vec<LocalId> = cands.iter().copied().filter(|&l| f.locals[l] == Ty::Any).collect();
        if !retype_numbers(f, &cands) && !simplified {
            return;
        }
    }
}
