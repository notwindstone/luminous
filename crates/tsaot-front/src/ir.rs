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

use std::collections::{HashMap, HashSet};
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
    /// A `JSValue` known to be an object of `aot type` layout `k` (see
    /// `Layout`; represented like `Any`).
    Struct(u32),
    /// An array of objects of layout `k` (`TsaotStructArray`; like `Any`).
    StructArr(u32),
    /// An element of such an array: an object of layout `k` or `undefined`.
    StructOrUndef(u32),
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
            Ty::Struct(_) => "aot type",
            Ty::StructArr(_) => "aot type array",
            Ty::StructOrUndef(_) => "aot type | undefined",
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
    /// Top-level classes (by home slot `k`).
    pub classes: Vec<ClassTable>,
    /// `aot type` layouts (`Ty::Struct`).
    pub layouts: Vec<Layout>,
    /// Stores into `int`s wrap to int32 (no `--no-int-wrapping`).
    pub int_wrapping: bool,
}

/// How an `aot type` field is stored: unboxed when all members agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    Int32,
    F64,
    Bool,
    Value,
}

/// The type of a field in one member of an `aot type`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldType {
    Absent,
    Int,
    Number,
    Bool,
    String,
    /// One of these strings.
    Literals(Vec<String>),
    /// An object of this layout.
    Struct(u32),
}

#[derive(Clone, Debug)]
pub struct LayoutField {
    pub name: String,
    pub storage: Storage,
    /// Byte offset in the object's data.
    pub offset: u32,
    /// Present in every member (so in every object).
    pub universal: bool,
    /// The type a read gives, when it is present.
    pub ty: Ty,
    /// Of the same type in every member: a write cannot change which member
    /// the object matches.
    pub simple: bool,
}

#[derive(Clone, Debug)]
pub struct LayoutMember {
    /// The member's fields (bit `i`: field `i`).
    pub mask: u32,
    /// Per field (`Absent` outside `mask`).
    pub types: Vec<FieldType>,
}

/// An `aot type`: one layout holding the fields of all its union members,
/// with a mask of those present. An object is valid when its fields
/// include all of some member's, with that member's types, and every
/// other field it has belongs to some member and has a type it allows.
#[derive(Clone, Debug)]
pub struct Layout {
    pub name: String,
    pub fields: Vec<LayoutField>,
    pub members: Vec<LayoutMember>,
    /// Bytes of field data.
    pub size: u32,
}

/// A top-level class: its superclass (if a top-level class) and its native
/// members: (key, 0 for a method / 1 a getter / 2 a setter, static, function).
#[derive(Debug, Clone, Default)]
pub struct ClassTable {
    pub superclass: Option<usize>,
    pub members: Vec<(String, u8, bool, FuncId)>,
    /// What the type annotations of its instance fields say they hold.
    pub fields: Vec<(String, Hint)>,
}

/// What a type annotation says a value is, for predicting the methods it
/// reaches (`predict_calls`). Only a hint: predictions are checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    /// An instance of top-level class `k`.
    Class(usize),
    /// An array of them.
    ArrayOf(usize),
}

/// The array methods with native loops (`ExprKind::ArrayLoop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopKind {
    ForEach,
    Map,
    Filter,
    Some,
    Every,
    Find,
    FindIndex,
}

impl LoopKind {
    pub fn named(name: &str) -> Option<LoopKind> {
        Some(match name {
            "forEach" => LoopKind::ForEach,
            "map" => LoopKind::Map,
            "filter" => LoopKind::Filter,
            "some" => LoopKind::Some,
            "every" => LoopKind::Every,
            "find" => LoopKind::Find,
            "findIndex" => LoopKind::FindIndex,
            _ => return None,
        })
    }
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
    /// Per capture: whether its data slot holds the captured value itself
    /// (never assigned; see `capture_values`) rather than its box.
    pub value_captures: Vec<bool>,
    pub is_closure: bool,
    /// Methods: `this` is passed in (borrowed).
    pub has_this: bool,
    /// Class methods: their class, and whether they are static.
    pub class: Option<(usize, bool)>,
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
    /// What the locals hold, as far as type annotations (and values made
    /// from annotated ones) say: see `infer_hints`.
    pub hints: HashMap<LocalId, Hint>,
    /// What the function returns, by its annotation.
    pub ret_hint: Option<Hint>,
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
    /// A `Math` function or `Number` predicate. The arguments are numbers,
    /// or for `Math` functions also JS values, converted with ToNumber in
    /// order once all are evaluated.
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
    /// A new instance of top-level class `class` (whose prototype is home
    /// object `2 * class`) with these own fields, in order (enumerable,
    /// writable, configurable), as its constructor makes it. `Any`.
    NewInstance {
        class: usize,
        fields: Vec<(AtomId, Expr)>,
    },
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
    /// `super.name` (cached).
    SuperGetAtom(usize, AtomId),
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
    /// `-`, `*`, `/`, `%`, `**` with a number and a JS value (`Any`): both
    /// are evaluated, then the JS value is converted with ToNumber (a BigInt
    /// throws, as mixing it with a number does). An `F64`.
    NumArith(BinOp, Box<Expr>, Box<Expr>),
    /// An inlined call (see `inline_calls`): the callee's body, whose
    /// `return`s give the value, over `locals` (the callee's, appended to
    /// the caller's; its JS values are released after it).
    Inline {
        body: Vec<Stmt>,
        locals: Vec<LocalId>,
    },
    /// A method call (`CallMethod`, `CallThis`), getter read (`GetProp`) or
    /// setter write (`SetProp`) predicted to reach native function `func`
    /// (module functions, then closures; see `predict_calls`): when it does
    /// (its entry is checked) and the arguments have its parameters' types,
    /// its `body` runs inline over `locals` (parameters first), with `this`
    /// the receiver.
    Predicted {
        expr: Box<Expr>,
        func: usize,
        body: Vec<Stmt>,
        locals: Vec<LocalId>,
    },
    /// `recv.method(callback)`, `callback` an arrow function (the `Closure`
    /// of global function `func`) whose body is attached (see
    /// `inline_callbacks`): when `recv` is an array without holes and its
    /// `method` is the original builtin (`kind`), a native loop runs `body`
    /// for each element, over `locals` (the callback's, parameters first;
    /// its captured variables are the caller's own locals); otherwise the
    /// method is called with the callback. `Any`.
    ArrayLoop {
        kind: LoopKind,
        recv: Box<Expr>,
        method: AtomId,
        callback: Box<Expr>,
        func: usize,
        body: Vec<Stmt>,
        locals: Vec<LocalId>,
    },
    /// `recv.method(callback)`, `callback` an arrow function without
    /// captures (the `Closure` of global function `func`) comparing two
    /// `Any`s or two `F64`s into an `F64` or `Any`: when `method` is the
    /// original `sort`, the engine sorts the array (`JS_SortNative`)
    /// calling the native function directly; else (or if the engine
    /// declines) the method is called with the callback. `Any`.
    ArraySort {
        recv: Box<Expr>,
        method: AtomId,
        callback: Box<Expr>,
        func: usize,
    },
    /// An object of `aot type` `layout` from a literal with the fields in
    /// `mask`: (field, value) in source order. `Ty::Struct(layout)`.
    StructNew {
        layout: u32,
        mask: u32,
        fields: Vec<(u32, Expr)>,
    },
    /// Field `field` of an object of layout `layout` (`Ty::Struct`): the
    /// field's type if it is in every member, else `Any` (`undefined` when
    /// absent).
    StructGet(u32, Box<Expr>, u32),
    /// Store a field (the value is checked, as a write from JS would be);
    /// evaluates to the value.
    StructSet(u32, Box<Expr>, u32, Box<Expr>),
    /// A direct call of native function `func` whose arguments at `checks`
    /// (JS values) must be as these say: checked here, then the native
    /// function is called directly; else a call of `callee` (its function
    /// object) with the same arguments. `Any`.
    CallChecked {
        func: FuncId,
        callee: Box<Expr>,
        args: Vec<Expr>,
        checks: Vec<(usize, ArgCheck)>,
    },
    /// An array of objects of layout `k` with the elements of an array
    /// (a literal or `new Array(n)`, see `structs::array_sites`).
    StructArrFrom(u32, Box<Expr>),
    /// `a[i]` (`StructOrUndef`), `a[i] = v` (the value), `a.length`,
    /// `a.push(v)` on an array of layout `k`.
    StructArrGet(u32, Box<Expr>, Box<Expr>),
    StructArrSet(u32, Box<Expr>, Box<Expr>, Box<Expr>),
    StructArrLen(Box<Expr>),
    StructArrPush(u32, Box<Expr>, Box<Expr>),
    /// A `StructOrUndef` value that is an object (`Ty::Struct`): else what
    /// JS throws reading property `atom` of it, or converting it to an
    /// object (`None`).
    NonNull(Option<AtomId>, Box<Expr>),
    /// A value converted for field `field` of layout `layout` as a store
    /// would convert it (an `int` wraps; TypeError if not of its type): of
    /// the field's type (see `scalar_replace_structs`).
    StructCoerce(u32, u32, Box<Expr>),
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
    /// `get name() {}` (false) / `set name(v) {}` (true): the function.
    Accessor(AtomId, Expr, bool),
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

/// What `ExprKind::CallChecked` requires of an argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgCheck {
    /// An object of this `aot type` layout.
    Struct(u32),
    /// A number (for an unboxed `number` parameter).
    Number,
    /// A boolean.
    Bool,
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
    ($(#[$doc:meta])* $name:ident $stmts:ident $($m:ident)?) => {
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
                | SuperGetAtom(..)
                | Closure(..) => {}
                Inline { body, .. } => $stmts(body, f),
                Predicted { expr, body, .. } => {
                    $name(expr, f);
                    $stmts(body, f);
                }
                ArrayLoop { recv, callback, body, .. } => {
                    $name(recv, f);
                    $name(callback, f);
                    $stmts(body, f);
                }
                ArraySort { recv, callback, .. } => {
                    $name(recv, f);
                    $name(callback, f);
                }
                CallChecked { callee, args, .. } => {
                    $name(callee, f);
                    for x in args {
                        $name(x, f);
                    }
                }
                StructNew { fields, .. } => {
                    for (_, x) in fields {
                        $name(x, f);
                    }
                }
                StructGet(_, a, _) | StructCoerce(_, _, a) | StructArrFrom(_, a) | StructArrLen(a) | NonNull(_, a) => {
                    $name(a, f)
                }
                StructArrGet(_, a, b) | StructArrPush(_, a, b) => {
                    $name(a, f);
                    $name(b, f);
                }
                StructArrSet(_, a, b, c) => {
                    $name(a, f);
                    $name(b, f);
                    $name(c, f);
                }
                StructSet(_, a, _, b) => {
                    $name(a, f);
                    $name(b, f);
                }
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
                            ObjPart::Field(_, x) | ObjPart::Spread(x) | ObjPart::Accessor(_, x, _) => $name(x, f),
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
                | NumArith(_, a, b)
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
                Object(props) | NewInstance { fields: props, .. } => {
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
    visit visit_stmts
);
visitor!(
    /// Visit `x` and every expression in it, for rewriting (`f` sees a
    /// node before its children, which it may replace).
    visit_mut visit_stmts_mut mut
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
    alias_destructuring(f);
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

/// Destructuring reads its source through a temporary: `t = ToObject(l)`,
/// then `t.a`, `t.b`... When `l` is only ever the object of its literal
/// initializer, `ToObject(l)` is that object and `t.a` reads `l.a`: such
/// temporaries (written once, only read as `t.key`) are replaced by `l`, so
/// that `scalar_replace` sees through the destructuring.
fn alias_destructuring(f: &mut Func) {
    let n = f.locals.len();
    // Locals only written by one `InitLocal(l, {...})`.
    let mut literal_inits = vec![0usize; n];
    let mut other_writes = vec![false; n];
    // Candidate temporaries: their source local, writes, and other uses.
    let mut source: Vec<Option<LocalId>> = vec![None; n];
    let mut writes = vec![0usize; n];
    let mut as_object = vec![0usize; n];
    let mut total = vec![0usize; n];
    stmt_locals(&f.body, &mut |l| other_writes[l] = true);
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::InitLocal(l, x) | ExprKind::SetLocal(l, x) => {
            writes[*l] += 1;
            match (&e.kind, &x.kind) {
                (ExprKind::InitLocal(..), ExprKind::Object(_)) => literal_inits[*l] += 1,
                (ExprKind::SetLocal(..), ExprKind::ToObject(inner)) => match inner.kind {
                    ExprKind::Local(m) => source[*l] = Some(m),
                    _ => other_writes[*l] = true,
                },
                _ => other_writes[*l] = true,
            }
        }
        ExprKind::Update { local, .. } => other_writes[*local] = true,
        ExprKind::GetProp(o, _) => {
            if let ExprKind::Local(l) = o.kind {
                as_object[l] += 1;
            }
        }
        ExprKind::Local(l) => total[*l] += 1,
        _ => {}
    });
    let alias: Vec<Option<LocalId>> = (0..n)
        .map(|t| {
            let m = source[t]?;
            let literal = literal_inits[m] == 1 && writes[m] == 1 && !other_writes[m] && !f.boxed[m];
            (literal && writes[t] == 1 && !other_writes[t] && !f.boxed[t] && total[t] == as_object[t]).then_some(m)
        })
        .collect();
    if alias.iter().all(Option::is_none) {
        return;
    }
    visit_stmts_mut(&mut f.body, &mut |e| match &mut e.kind {
        ExprKind::SetLocal(t, _) if alias[*t].is_some() => e.kind = ExprKind::Undefined,
        ExprKind::GetProp(o, _) => {
            if let ExprKind::Local(t) = o.kind
                && let Some(m) = alias[t]
            {
                o.kind = ExprKind::Local(m);
            }
        }
        _ => {}
    });
}

/// How `e` itself (not its subexpressions) refers to local `l`: reads
/// (`Local`, closure captures), and other references (writes).
fn local_refs(e: &Expr, l: LocalId) -> (usize, usize) {
    match &e.kind {
        ExprKind::Local(m) => ((*m == l) as usize, 0),
        ExprKind::Closure(_, caps) => (caps.iter().filter(|&&m| m == l).count(), 0),
        ExprKind::SetLocal(m, _)
        | ExprKind::InitLocal(m, _)
        | ExprKind::Update { local: m, .. }
        | ExprKind::ForInGet(m, _, _)
        | ExprKind::ArrayPush(m, _)
        | ExprKind::AsyncIterResult(_, m)
        | ExprKind::IterCloseQuiet(m) => (0, (*m == l) as usize),
        ExprKind::IterOpen(_, m, n, _) | ExprKind::IterClose(m, n) => (0, (*m == l || *n == l) as usize),
        ExprKind::IterStep(m, n) => (0, (*m == l || *n == Some(l)) as usize),
        _ => (0, 0),
    }
}

/// `local_refs` over statements, counting the locals that statements name
/// themselves (except in `NewBoxes`) as other references.
fn stmts_refs(stmts: &[Stmt], l: LocalId) -> (usize, usize) {
    let (mut reads, mut others) = (0, 0);
    visit_stmts(stmts, &mut |e| {
        let (r, o) = local_refs(e, l);
        reads += r;
        others += o;
    });
    stmt_locals(stmts, &mut |m| others += (m == l) as usize);
    others -= new_boxes(stmts, l);
    (reads, others)
}

/// How many `NewBoxes` in `stmts` name `l`.
fn new_boxes(stmts: &[Stmt], l: LocalId) -> usize {
    let mut n = 0;
    stmt_lists(stmts, &mut |list| {
        for s in list {
            if let Stmt::NewBoxes(ls) = s {
                n += ls.iter().filter(|&&m| m == l).count();
            }
        }
    });
    n
}

/// Calls `f` on `stmts` and on every statement list nested in them.
fn stmt_lists<'a>(stmts: &'a [Stmt], f: &mut impl FnMut(&'a [Stmt])) {
    f(stmts);
    for s in stmts {
        match s {
            Stmt::If(_, a, b) => {
                stmt_lists(a, f);
                stmt_lists(b, f);
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => stmt_lists(body, f),
            Stmt::Switch { cases, .. } => cases.iter().for_each(|(_, b)| stmt_lists(b, f)),
            Stmt::Try { body, catch, finally, .. } => {
                stmt_lists(body, f);
                catch.iter().for_each(|(_, c)| stmt_lists(c, f));
                finally.iter().for_each(|c| stmt_lists(c, f));
            }
            _ => {}
        }
    }
}

/// `stmt_lists` for rewriting.
fn stmt_lists_mut(stmts: &mut Vec<Stmt>, f: &mut impl FnMut(&mut Vec<Stmt>)) {
    f(stmts);
    for s in stmts.iter_mut() {
        match s {
            Stmt::If(_, a, b) => {
                stmt_lists_mut(a, f);
                stmt_lists_mut(b, f);
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => stmt_lists_mut(body, f),
            Stmt::Switch { cases, .. } => cases.iter_mut().for_each(|(_, b)| stmt_lists_mut(b, f)),
            Stmt::Try { body, catch, finally, .. } => {
                stmt_lists_mut(body, f);
                catch.iter_mut().for_each(|(_, c)| stmt_lists_mut(c, f));
                finally.iter_mut().for_each(|c| stmt_lists_mut(c, f));
            }
            _ => {}
        }
    }
}

/// The closures created in `stmts` that capture `l`: (closure, data index).
fn capturing(stmts: &[Stmt], l: LocalId) -> Vec<(ClosureId, usize)> {
    let mut out = Vec::new();
    visit_stmts(stmts, &mut |e| {
        if let ExprKind::Closure(k, caps) = &e.kind {
            out.extend(caps.iter().enumerate().filter(|&(_, &m)| m == l).map(|(j, _)| (*k, j)));
        }
    });
    out
}

/// The statement `InitLocal(l, x)` in `stmts`: `x`, and the statements that
/// run after it (in its list and, through blocks, the lists around it).
fn find_decl<'a>(stmts: &'a [Stmt], l: LocalId, outer: &[&'a Stmt]) -> Option<(&'a Expr, Vec<&'a Stmt>)> {
    for (p, s) in stmts.iter().enumerate() {
        let rest = || stmts[p + 1..].iter().chain(outer.iter().copied()).collect::<Vec<_>>();
        match s {
            Stmt::Expr(Expr { kind: ExprKind::InitLocal(m, x), .. }) if *m == l => return Some((x, rest())),
            Stmt::Block(body) => {
                if let Some(r) = find_decl(body, l, &rest()) {
                    return Some(r);
                }
            }
            _ => {
                let mut found = None;
                stmt_lists(std::slice::from_ref(s), &mut |list| {
                    if found.is_none() && !std::ptr::eq(list, std::slice::from_ref(s)) {
                        found = find_decl(list, l, &[]);
                    }
                });
                if found.is_some() {
                    return found;
                }
            }
        }
    }
    None
}

/// Whether capture `j` of closure `k` can hold the captured value itself:
/// the closure never assigns it and does not suspend (its frame would
/// outlive the function data), and neither do the closures it passes it to.
fn value_capture_ok(m: &Module, k: ClosureId, j: usize) -> bool {
    let f = &m.closures[k];
    let c = f.captures[j];
    !f.is_async
        && !f.is_generator
        && stmts_refs(&f.body, c).1 == 0
        && capturing(&f.body, c).into_iter().all(|(k2, j2)| value_capture_ok(m, k2, j2))
}

fn mark_value_capture(m: &mut Module, k: ClosureId, j: usize) {
    m.closures[k].value_captures[j] = true;
    let c = m.closures[k].captures[j];
    for (k2, j2) in capturing(&m.closures[k].body, c) {
        mark_value_capture(m, k2, j2);
    }
}

/// Captures by value, as engines capture variables that never change: a
/// boxed local assigned only by its declaration, used only in the
/// statements after it (in the same list), and captured only by closures
/// that never assign it, is no longer boxed, and its closures hold its value
/// in their data slots instead of the box.
pub fn capture_values(m: &mut Module) {
    let nfuncs = m.funcs.len();
    for g in 0..nfuncs + m.closures.len() {
        let f = if g < nfuncs { &m.funcs[g] } else { &m.closures[g - nfuncs] };
        let mut found = Vec::new();
        for l in 0..f.locals.len() {
            if !f.boxed[l] || f.captures.contains(&l) {
                continue;
            }
            // The declaration: `InitLocal(l, x)` as a statement.
            let Some((x, after)) = find_decl(&f.body, l, &[]) else { continue };
            let mut in_init = (0, 0);
            visit(x, &mut |e| {
                let (r, o) = local_refs(e, l);
                in_init.0 += r;
                in_init.1 += o;
            });
            let (reads, others) = stmts_refs(&f.body, l);
            let after = after.iter().fold((0, 0), |(r, o), s| {
                let (r2, o2) = stmts_refs(std::slice::from_ref(*s), l);
                (r + r2, o + o2)
            });
            if in_init != (0, 0) || others != 1 || after != (reads, 0) {
                continue;
            }
            let caps = capturing(&f.body, l);
            if !caps.is_empty() && caps.iter().all(|&(k, j)| value_capture_ok(m, k, j)) {
                found.push((l, caps));
            }
        }
        for (l, caps) in found {
            let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
            f.boxed[l] = false;
            stmt_lists_mut(&mut f.body, &mut |list| {
                for s in list.iter_mut() {
                    if let Stmt::NewBoxes(ls) = s {
                        ls.retain(|&m| m != l);
                    }
                }
                list.retain(|s| !matches!(s, Stmt::NewBoxes(ls) if ls.is_empty()));
            });
            for (k, j) in caps {
                mark_value_capture(m, k, j);
            }
        }
    }
}

/// IR nodes in a callee body (before inlining into it) that `inline_calls`
/// inlines.
const INLINE_SIZE: usize = 48;
/// Callers stop growing by inlining at this many IR nodes.
const INLINE_CALLER_LIMIT: usize = 4000;

/// Whether calls to `f` can be inlined: a plain function (no closure data,
/// `this`, rest array or suspension) without boxed locals, closures or
/// loops. Its statements then run at most once per call, which keeps the
/// caller's loop-based analyses (that count writes per iteration) sound.
fn inlinable(f: &Func, size_limit: usize) -> bool {
    inline_ok(f, size_limit, false)
}

/// `inlinable`, or for `predict_calls` (`method`): also a method (with
/// `this`) or a closure without captures.
fn inline_ok(f: &Func, size_limit: usize, method: bool) -> bool {
    fn stmts_ok(stmts: &[Stmt]) -> bool {
        stmts.iter().all(|s| match s {
            Stmt::Expr(_) | Stmt::Return(_) | Stmt::Throw(_) => true,
            Stmt::If(_, a, b) => stmts_ok(a) && stmts_ok(b),
            Stmt::Block(b) => stmts_ok(b),
            _ => false,
        })
    }
    let other = if method { !f.captures.is_empty() } else { f.is_closure || f.has_this };
    if f.is_async || f.is_generator || other || f.rest || f.ret_slot.is_some() {
        return false;
    }
    if f.boxed.iter().any(|&b| b) || !stmts_ok(&f.body) {
        return false;
    }
    let mut size = 0;
    let mut ok = true;
    visit_stmts(&f.body, &mut |e| {
        size += 1;
        match &e.kind {
            ExprKind::Closure(..) => ok = false,
            ExprKind::This => ok &= method,
            ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } => ok &= stmts_ok(body),
            _ => {}
        }
    });
    ok && size <= size_limit
}

fn ir_size(stmts: &[Stmt]) -> usize {
    let mut n = 0;
    visit_stmts(stmts, &mut |_| n += 1);
    n
}

/// Renumber the locals of `e` (not its subexpressions) by `base`.
fn shift_locals(e: &mut Expr, base: LocalId) {
    match &mut e.kind {
        ExprKind::Local(l)
        | ExprKind::SetLocal(l, _)
        | ExprKind::InitLocal(l, _)
        | ExprKind::Update { local: l, .. }
        | ExprKind::ForInGet(l, _, _)
        | ExprKind::ArrayPush(l, _)
        | ExprKind::AsyncIterResult(_, l)
        | ExprKind::IterCloseQuiet(l) => *l += base,
        ExprKind::IterOpen(_, l, m, _) | ExprKind::IterClose(l, m) => {
            *l += base;
            *m += base;
        }
        ExprKind::IterStep(l, m) => {
            *l += base;
            if let Some(m) = m {
                *m += base;
            }
        }
        ExprKind::Closure(_, caps) => caps.iter_mut().for_each(|l| *l += base),
        ExprKind::Inline { locals, .. } => locals.iter_mut().for_each(|l| *l += base),
        ExprKind::Predicted { locals, .. } => locals.iter_mut().for_each(|l| *l += base),
        ExprKind::ArrayLoop { locals, .. } => locals.iter_mut().for_each(|l| *l += base),
        _ => {}
    }
}

/// Renumber the locals of `e` (not its subexpressions) by `map`.
fn remap_locals(e: &mut Expr, map: &impl Fn(LocalId) -> LocalId) {
    let m = |l: &mut LocalId| *l = map(*l);
    match &mut e.kind {
        ExprKind::Local(l)
        | ExprKind::SetLocal(l, _)
        | ExprKind::InitLocal(l, _)
        | ExprKind::Update { local: l, .. }
        | ExprKind::ForInGet(l, _, _)
        | ExprKind::ArrayPush(l, _)
        | ExprKind::AsyncIterResult(_, l)
        | ExprKind::IterCloseQuiet(l) => m(l),
        ExprKind::IterOpen(_, l, n, _) | ExprKind::IterClose(l, n) => {
            m(l);
            m(n);
        }
        ExprKind::IterStep(l, n) => {
            m(l);
            if let Some(n) = n {
                m(n);
            }
        }
        ExprKind::Closure(_, caps) => caps.iter_mut().for_each(m),
        ExprKind::Inline { locals, .. } | ExprKind::Predicted { locals, .. } | ExprKind::ArrayLoop { locals, .. } => {
            locals.iter_mut().for_each(m)
        }
        _ => {}
    }
}

/// Inlining, as JIT compilers inline small functions: a direct call of a
/// small native function (`inlinable`) becomes its body, with its locals
/// appended to the caller's (its parameters first, set from the
/// arguments). A few rounds inline calls in the inlined bodies too.
pub fn inline_calls(m: &mut Module) {
    let nfuncs = m.funcs.len();
    // Callees qualify by their own size; what they inlined comes along.
    let small: Vec<bool> = m.funcs.iter().map(|f| inlinable(f, INLINE_SIZE)).collect();
    for _ in 0..3 {
        let callees: Vec<Option<Func>> =
            m.funcs.iter().zip(&small).map(|(f, &s)| (s && inlinable(f, usize::MAX)).then(|| f.clone())).collect();
        if callees.iter().all(Option::is_none) {
            return;
        }
        for g in 0..nfuncs + m.closures.len() {
            let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
            let mut budget = INLINE_CALLER_LIMIT.saturating_sub(ir_size(&f.body));
            let mut added: Vec<Ty> = Vec::new();
            let base0 = f.locals.len();
            // First the calls become their argument assignments (calls in
            // arguments are inlined in turn), then the bodies are added: a
            // body is never searched for calls in the round that adds it.
            let mut pending: HashMap<LocalId, FuncId> = HashMap::new();
            visit_stmts_mut(&mut f.body, &mut |e| {
                let ExprKind::Call(k, args) = &mut e.kind else { return };
                let Some(callee) = callees[*k].as_ref() else { return };
                if (g < nfuncs && *k == g) || args.len() != callee.params.len() {
                    return;
                }
                // Arguments are of the parameters' ABI types; a parameter
                // local may be `Any` (boxed on entry).
                if !args.iter().zip(&callee.locals).all(|(a, &t)| a.ty == t || t == Ty::Any) {
                    return;
                }
                let size = ir_size(&callee.body);
                if size > budget {
                    return;
                }
                budget -= size;
                let base = base0 + added.len();
                added.extend(&callee.locals);
                // A placeholder local names a callee without any.
                if callee.locals.is_empty() {
                    added.push(Ty::Bool);
                }
                pending.insert(base, *k);
                let body: Vec<Stmt> = std::mem::take(args)
                    .into_iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let t = callee.locals[i];
                        let a = if a.ty != t { Expr::new(Ty::Any, ExprKind::Box(Box::new(a))) } else { a };
                        Stmt::Expr(Expr::new(t, ExprKind::InitLocal(base + i, Box::new(a))))
                    })
                    .collect();
                let locals = (base..base + callee.locals.len().max(1)).collect();
                e.kind = ExprKind::Inline { body, locals };
            });
            let mut hints = Vec::new();
            visit_stmts_mut(&mut f.body, &mut |e| {
                let ExprKind::Inline { body, locals } = &mut e.kind else { return };
                let Some(k) = locals.first().and_then(|b| pending.remove(b)) else { return };
                let base = locals[0];
                let callee = callees[k].as_ref().unwrap();
                let mut inner = callee.body.clone();
                visit_stmts_mut(&mut inner, &mut |x| shift_locals(x, base));
                body.extend(inner);
                hints.extend(callee.hints.iter().map(|(&l, &h)| (base + l, h)));
            });
            f.hints.extend(hints);
            f.boxed.extend(added.iter().map(|_| false));
            f.local_names.extend(added.iter().map(|_| None));
            f.locals.extend(added);
        }
    }
}

/// `receivers`: `this`, the locals holding known receivers, and the locals
/// holding `super.key` (home, key).
type Receivers = (Option<Recv>, HashMap<LocalId, Recv>, HashMap<LocalId, (usize, AtomId)>);

/// What a receiver is known to be, for `predict_calls`.
#[derive(Clone)]
enum Recv {
    /// An instance of a class (or the class itself: static).
    Class(usize, bool),
    /// An object literal: its members (key, 0 method / 1 getter / 2 setter,
    /// closure), later ones first.
    Literal(Vec<(AtomId, u8, ClosureId)>),
}

/// A member `key` of `kind` (0 method, 1 getter, 2 setter) found from class
/// `c` up its superclasses.
/// What expression `x` of function `f` evaluates to, as far as type
/// annotations say (`Func::hints`): an instance made by a native
/// constructor, the annotated result of a call, field or element.
fn expr_hint(m: &Module, f: &Func, hints: &HashMap<LocalId, Hint>, x: &Expr) -> Option<Hint> {
    let class_of = |o: &Expr| match expr_hint(m, f, hints, o)? {
        Hint::Class(k) => Some(k),
        Hint::ArrayOf(_) => None,
    };
    match &x.kind {
        ExprKind::This => f.class.filter(|&(_, s)| f.has_this && !s).map(|(c, _)| Hint::Class(c)),
        ExprKind::Local(l) => hints.get(l).copied(),
        ExprKind::Box(e) | ExprKind::NonNull(_, e) | ExprKind::Predicted { expr: e, .. } => expr_hint(m, f, hints, e),
        ExprKind::Seq(xs) => expr_hint(m, f, hints, xs.last()?),
        ExprKind::NewInstance { class, .. } => Some(Hint::Class(*class)),
        ExprKind::Call(g, _) | ExprKind::CallChecked { func: g, .. } => func_at(m, *g).ret_hint,
        ExprKind::CallMethod(o, Key::Atom(a), _) => {
            let g = class_member(m, Some(class_of(o)?), false, &m.atoms[*a], 0)?;
            func_at(m, g).ret_hint
        }
        ExprKind::GetProp(o, a) => {
            let k = class_of(o)?;
            let name = &m.atoms[*a];
            let mut c = Some(k);
            while let Some(k) = c {
                let t = m.classes.get(k)?;
                if let Some((_, h)) = t.fields.iter().find(|(n, _)| n == name) {
                    return Some(*h);
                }
                c = t.superclass;
            }
            func_at(m, class_member(m, Some(k), false, name, 1)?).ret_hint
        }
        ExprKind::GetElem(o, _) => match expr_hint(m, f, hints, o)? {
            Hint::ArrayOf(k) => Some(Hint::Class(k)),
            Hint::Class(_) => None,
        },
        ExprKind::Cond(_, a, b) => {
            let h = expr_hint(m, f, hints, a)?;
            (expr_hint(m, f, hints, b)? == h).then_some(h)
        }
        _ => None,
    }
}

/// Extends `Func::hints` (from annotations) to the locals whose every
/// value has the same hint (`null` and `undefined` aside).
pub fn infer_hints(m: &mut Module) {
    let total = m.funcs.len() + m.closures.len();
    let mut all = Vec::with_capacity(total);
    for g in 0..total {
        let f = func_at(m, g);
        let n = f.locals.len();
        let mut other = f.boxed.clone();
        visit_stmts(&f.body, &mut |e| {
            if let ExprKind::Update { local, .. } = &e.kind {
                other[*local] = true;
            }
        });
        stmt_locals(&f.body, &mut |l| other[l] = true);
        let mut hints = f.hints.clone();
        loop {
            // Per local: the hint all its values have so far, or a conflict.
            let mut seen: Vec<Option<Option<Hint>>> = vec![None; n];
            visit_stmts(&f.body, &mut |e| {
                let (ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x)) = &e.kind else { return };
                if hints.contains_key(l) || other[*l] || matches!(x.kind, ExprKind::Null | ExprKind::Undefined) {
                    return;
                }
                let h = expr_hint(m, f, &hints, x);
                seen[*l] = match seen[*l] {
                    None => Some(h),
                    Some(prev) if prev == h => Some(h),
                    Some(_) => Some(None),
                };
            });
            let mut changed = false;
            for (l, s) in seen.into_iter().enumerate() {
                if let Some(Some(h)) = s {
                    hints.insert(l, h);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        all.push(hints);
    }
    let nfuncs = m.funcs.len();
    for (g, hints) in all.into_iter().enumerate() {
        let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
        f.hints = hints;
    }
}

fn class_member(m: &Module, mut c: Option<usize>, is_static: bool, key: &str, kind: u8) -> Option<usize> {
    while let Some(k) = c {
        let t = m.classes.get(k)?;
        if let Some(&(_, _, _, f)) = t.members.iter().find(|(n, a, s, _)| n == key && *a == kind && *s == is_static) {
            return Some(f);
        }
        c = t.superclass;
    }
    None
}

/// A local's write, as `receivers` sees it.
enum Write {
    This,
    Literal(Vec<(AtomId, u8, ClosureId)>),
    Super(usize, AtomId),
    Other,
}

fn write_of(x: &Expr) -> Write {
    let closure = |v: &Expr| if let ExprKind::Closure(k, _) = v.kind { Some(k) } else { None };
    match &x.kind {
        ExprKind::This => Write::This,
        ExprKind::SuperGetAtom(home, a) => Write::Super(*home, *a),
        ExprKind::Object(props) => {
            Write::Literal(props.iter().rev().filter_map(|(a, v)| Some((*a, 0, closure(v)?))).collect())
        }
        ExprKind::ObjectParts(parts) => {
            let mut members = Vec::new();
            for p in parts.iter().rev() {
                match p {
                    ObjPart::Field(a, v) => members.extend(closure(v).map(|k| (*a, 0, k))),
                    ObjPart::Accessor(a, v, setter) => {
                        members.extend(closure(v).map(|k| (*a, if *setter { 2 } else { 1 }, k)))
                    }
                    // Keys not known: nothing before it (here: after) is
                    // predicted.
                    ObjPart::Spread(_) | ObjPart::Computed(..) => break,
                }
            }
            Write::Literal(members)
        }
        _ => Write::Other,
    }
}

/// The receivers `predict_calls` knows in `f`: `this` of a class method,
/// locals only ever assigned `this`, and locals assigned once an object
/// literal; and the locals assigned only `super.key` (home, key).
fn receivers(f: &Func) -> Receivers {
    let this = f.class.filter(|_| f.has_this).map(|(c, s)| Recv::Class(c, s));
    let mut writes: HashMap<LocalId, Vec<Write>> = HashMap::new();
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) => writes.entry(*l).or_default().push(write_of(x)),
        _ => {
            for l in 0..f.locals.len() {
                if local_refs(e, l).1 > 0 {
                    writes.entry(l).or_default().push(Write::Other);
                }
            }
        }
    });
    stmt_locals(&f.body, &mut |l| writes.entry(l).or_default().push(Write::Other));
    let mut recv = HashMap::new();
    let mut supers = HashMap::new();
    for (l, ws) in writes {
        if f.boxed[l] || f.captures.contains(&l) {
            continue;
        }
        match &ws[..] {
            ws if this.is_some() && ws.iter().all(|w| matches!(w, Write::This)) => {
                recv.insert(l, this.clone().unwrap());
            }
            [Write::Literal(members)] => {
                recv.insert(l, Recv::Literal(members.clone()));
            }
            [Write::Super(home, a), rest @ ..]
                if rest.iter().all(|w| matches!(w, Write::Super(h, b) if h == home && b == a)) =>
            {
                supers.insert(l, (*home, *a));
            }
            _ => {}
        }
    }
    (this, recv, supers)
}

fn func_at(m: &Module, g: usize) -> &Func {
    if g < m.funcs.len() { &m.funcs[g] } else { &m.closures[g - m.funcs.len()] }
}

/// Speculative inlining of methods, getters and setters, as JIT compilers
/// inline a callee they expect at a call site: where the code around a
/// call says which native function it should reach (a method of `this`'s
/// class or its superclasses, `super.m()`, a member of an object literal
/// in a `const`-like local), the callee's body is attached for inlining
/// (`ExprKind::Predicted`). Code generation checks that the function
/// reached is that one, so a wrong prediction only costs the check.
pub fn predict_calls(m: &mut Module) {
    let nfuncs = m.funcs.len();
    let total = nfuncs + m.closures.len();
    let get = func_at;
    let typed = |f: &Func| f.params.iter().all(|t| matches!(t, Ty::Any | Ty::F64 | Ty::MaybeF64 | Ty::Bool));
    let small: Vec<bool> = (0..total).map(|g| inline_ok(get(m, g), INLINE_SIZE, true) && typed(get(m, g))).collect();
    for _ in 0..2 {
        let templates: Vec<Option<Func>> = (0..total)
            .map(|g| (small[g] && inline_ok(get(m, g), usize::MAX, true)).then(|| get(m, g).clone()))
            .collect();
        for g in 0..total {
            let (this, recv, supers) = receivers(get(m, g));
            let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
            let mut body = std::mem::take(&mut f.body);
            let base0 = f.locals.len();
            let m_ref: &Module = m;
            let fg = get(m_ref, g);
            let resolve = |e: &Expr| -> Option<usize> {
                // Else what type annotations say it is.
                let hinted = |o: &Expr| match expr_hint(m_ref, fg, &fg.hints, o)? {
                    Hint::Class(k) => Some(Recv::Class(k, false)),
                    Hint::ArrayOf(_) => None,
                };
                let of = |o: &Expr| match &o.kind {
                    ExprKind::This => this.clone(),
                    ExprKind::Local(l) => recv.get(l).cloned().or_else(|| hinted(o)),
                    _ => hinted(o),
                };
                let member = |r: Recv, a: AtomId, kind: u8| match r {
                    Recv::Class(c, s) => class_member(m_ref, Some(c), s, &m_ref.atoms[a], kind),
                    // The last definition of the key: a field, or this half
                    // of an accessor.
                    Recv::Literal(ms) => ms
                        .iter()
                        .find(|(b, k, _)| *b == a && (*k == kind || *k == 0))
                        .filter(|(_, k, _)| *k == kind)
                        .map(|(_, _, c)| nfuncs + c),
                };
                match &e.kind {
                    ExprKind::CallMethod(o, Key::Atom(a), _) => member(of(o)?, *a, 0),
                    ExprKind::GetProp(o, a) => member(of(o)?, *a, 1),
                    ExprKind::SetProp(o, a, _) => member(of(o)?, *a, 2),
                    ExprKind::CallThis(callee, _, _) => {
                        let ExprKind::Local(l) = callee.kind else { return None };
                        let &(home, a) = supers.get(&l)?;
                        let c = m_ref.classes.get(home / 2)?.superclass;
                        class_member(m_ref, c, home % 2 == 1, &m_ref.atoms[a], 0)
                    }
                    _ => None,
                }
            };
            let mut budget = INLINE_CALLER_LIMIT.saturating_sub(ir_size(&body));
            let mut added: Vec<Ty> = Vec::new();
            // Attached bodies are set aside while the function is searched
            // (their `this` is another receiver): by their first local.
            let mut stash: HashMap<LocalId, Vec<Stmt>> = HashMap::new();
            let mut pending: HashMap<LocalId, usize> = HashMap::new();
            let mut wrapped: HashSet<*const Expr> = HashSet::new();
            visit_stmts_mut(&mut body, &mut |e| {
                if let ExprKind::Predicted { expr, body, locals, .. } = &mut e.kind {
                    if !body.is_empty() {
                        stash.insert(locals[0], std::mem::take(body));
                    }
                    // (Its call is predicted already.)
                    wrapped.insert(&**expr as *const Expr);
                    return;
                }
                if wrapped.contains(&(e as *const Expr)) {
                    return;
                }
                let Some(k) = resolve(e) else { return };
                let Some(t) = templates[k].as_ref() else { return };
                let size = ir_size(&t.body);
                if size > budget {
                    return;
                }
                budget -= size;
                let base = base0 + added.len();
                added.extend(&t.locals);
                if t.locals.is_empty() {
                    added.push(Ty::Bool);
                }
                pending.insert(base, k);
                let locals = (base..base + t.locals.len().max(1)).collect();
                let ty = e.ty;
                let expr = Box::new(std::mem::replace(e, Expr::new(Ty::Void, ExprKind::Undefined)));
                wrapped.insert(&*expr as *const Expr);
                *e = Expr { ty, kind: ExprKind::Predicted { expr, func: k, body: Vec::new(), locals } };
            });
            visit_stmts_mut(&mut body, &mut |e| {
                let ExprKind::Predicted { body, locals, .. } = &mut e.kind else { return };
                if let Some(b) = stash.remove(&locals[0]) {
                    *body = b;
                } else if let Some(k) = pending.remove(&locals[0]) {
                    let mut inner = templates[k].as_ref().unwrap().body.clone();
                    visit_stmts_mut(&mut inner, &mut |x| shift_locals(x, locals[0]));
                    *body = inner;
                }
            });
            let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
            f.body = body;
            f.boxed.extend(added.iter().map(|_| false));
            f.local_names.extend(added.iter().map(|_| None));
            f.locals.extend(added);
        }
    }
}

/// IR nodes in a callback body that `inline_callbacks` inlines.
const CALLBACK_SIZE: usize = 160;

/// Whether closure `f` can be a native loop's body (`inline_callbacks`):
/// an arrow function without `this`, nested closures, boxes of its own,
/// loops or `try`, with at most 3 parameters typed as its loop passes them
/// (an element: `Any` or `F64`; its index: `Any` or `F64`; the array: `Any`).
fn callback_ok(f: &Func) -> bool {
    fn stmts_ok(stmts: &[Stmt]) -> bool {
        stmts.iter().all(|s| match s {
            Stmt::Expr(_) | Stmt::Return(_) | Stmt::Throw(_) => true,
            Stmt::If(_, a, b) => stmts_ok(a) && stmts_ok(b),
            Stmt::Block(b) => stmts_ok(b),
            _ => false,
        })
    }
    if !callback_plain(f) {
        return false;
    }
    let typed = [&[Ty::Any, Ty::F64][..], &[Ty::Any, Ty::F64], &[Ty::Any]];
    if f.params.len() > 3 || f.params.iter().zip(typed).any(|(t, ok)| !ok.contains(t)) {
        return false;
    }
    if (0..f.locals.len()).any(|l| f.boxed[l] && !f.captures.contains(&l)) || !stmts_ok(&f.body) {
        return false;
    }
    let mut size = 0;
    let mut ok = true;
    visit_stmts(&f.body, &mut |e| {
        size += 1;
        match &e.kind {
            ExprKind::Closure(..) | ExprKind::This => ok = false,
            ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } => ok &= stmts_ok(body),
            _ => {}
        }
    });
    ok && size <= CALLBACK_SIZE
}

/// A function the native loops can call or inline: an arrow function
/// (no `this`), neither async nor a generator, without a rest parameter.
fn callback_plain(f: &Func) -> bool {
    f.is_closure && !f.has_this && !f.is_async && !f.is_generator && !f.rest && f.ret_slot.is_none()
}

/// Whether a closure (its parameter types, return type, and whether it is
/// `callback_plain` without captures) can be `JS_SortNative`'s comparator.
fn comparator_ok((params, ret, plain): &(Vec<Ty>, Ty, bool)) -> bool {
    *plain
        && matches!(params.as_slice(), [Ty::Any, Ty::Any] | [Ty::F64, Ty::F64])
        && matches!(ret, Ty::F64 | Ty::Any)
}

/// Native loops for array callbacks, as JIT compilers inline a builtin's
/// loop and its callback: `xs.map((x) => ...)` (`forEach`, `map`, `filter`,
/// `some`, `every`, `find`, `findIndex`) with an arrow function that
/// `callback_ok` accepts gets the function's body attached
/// (`ExprKind::ArrayLoop`), its locals appended to the caller's; and
/// `xs.sort((a, b) => ...)` calls the comparator natively
/// (`ExprKind::ArraySort`).
pub fn inline_callbacks(m: &mut Module) {
    let nfuncs = m.funcs.len();
    let total = nfuncs + m.closures.len();
    let templates: Vec<Option<Func>> = m.closures.iter().map(|c| callback_ok(c).then(|| c.clone())).collect();
    let names: Vec<Option<LoopKind>> = m.atoms.iter().map(|a| LoopKind::named(a)).collect();
    let sort_atom = m.atoms.iter().position(|a| a == "sort");
    let comparators: Vec<(Vec<Ty>, Ty, bool)> =
        m.closures.iter().map(|c| (c.params.clone(), c.ret, callback_plain(c) && c.captures.is_empty())).collect();
    for g in 0..total {
        let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
        let mut body = std::mem::take(&mut f.body);
        let base0 = f.locals.len();
        let mut budget = INLINE_CALLER_LIMIT.saturating_sub(ir_size(&body));
        let mut added: Vec<Ty> = Vec::new();
        // Calls that predictions wrap stay as they are.
        let mut wrapped: HashSet<*const Expr> = HashSet::new();
        visit_stmts_mut(&mut body, &mut |e| {
            if let ExprKind::Predicted { expr, .. } = &e.kind {
                wrapped.insert(&**expr as *const Expr);
                return;
            }
            if wrapped.contains(&(e as *const Expr)) {
                return;
            }
            let ExprKind::CallMethod(_, Key::Atom(a), args) = &e.kind else { return };
            let [Expr { kind: ExprKind::Closure(cid, caps), .. }] = args.as_slice() else { return };
            if sort_atom == Some(*a) && caps.is_empty() && comparator_ok(&comparators[*cid]) {
                let func = nfuncs + *cid;
                let ty = e.ty;
                let ExprKind::CallMethod(recv, Key::Atom(method), mut args) =
                    std::mem::replace(&mut e.kind, ExprKind::Undefined)
                else {
                    unreachable!()
                };
                let callback = Box::new(args.pop().unwrap());
                *e = Expr { ty, kind: ExprKind::ArraySort { recv, method, callback, func } };
                return;
            }
            let Some(kind) = names[*a] else { return };
            let Some(t) = templates[*cid].as_ref() else { return };
            let size = ir_size(&t.body);
            if size > budget {
                return;
            }
            budget -= size;
            let base = base0 + added.len();
            added.extend(&t.locals);
            let map = |l: LocalId| match t.captures.iter().position(|&c| c == l) {
                Some(j) => caps[j],
                None => base + l,
            };
            let mut inner = t.body.clone();
            visit_stmts_mut(&mut inner, &mut |x| remap_locals(x, &map));
            let locals: Vec<LocalId> = (0..t.locals.len()).filter(|l| !t.captures.contains(l)).map(|l| base + l).collect();
            let func = nfuncs + *cid;
            let ty = e.ty;
            let ExprKind::CallMethod(recv, Key::Atom(method), mut args) =
                std::mem::replace(&mut e.kind, ExprKind::Undefined)
            else {
                unreachable!()
            };
            let callback = Box::new(args.pop().unwrap());
            *e = Expr { ty, kind: ExprKind::ArrayLoop { kind, recv, method, callback, func, body: inner, locals } };
        });
        let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
        f.body = body;
        f.boxed.extend(added.iter().map(|_| false));
        f.local_names.extend(added.iter().map(|_| None));
        f.locals.extend(added);
    }
}

/// Scalar replacement of `aot type` objects (as `scalar_replace` does for
/// object literals): a local of a single-member `aot type` initialized
/// once by a literal and only used for its fields (reads, and writes as
/// statements) becomes a local per field. Stores keep their conversion
/// (`StructCoerce`).
pub fn scalar_replace_structs(f: &mut Func, layouts: &[Layout]) {
    let n = f.locals.len();
    let mut ok: Vec<bool> = (0..n)
        .map(|l| {
            l >= f.params.len()
                && !f.boxed[l]
                && f.ret_slot != Some(l)
                && matches!(f.locals[l], Ty::Struct(k) if layouts[k as usize].members.len() == 1)
        })
        .collect();
    let mut inits = vec![0; n];
    let mut fields_used = vec![0; n];
    let mut sets = vec![0; n];
    let mut top_sets = vec![0; n];
    let mut total = vec![0; n];
    stmt_locals(&f.body, &mut |l| ok[l] = false);
    top_exprs(&f.body, &mut |e| {
        if let ExprKind::StructSet(_, o, _, _) = &e.kind
            && let ExprKind::Local(l) = o.kind
        {
            top_sets[l] += 1;
        }
    });
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::InitLocal(l, x) if matches!(x.kind, ExprKind::StructNew { .. }) => inits[*l] += 1,
        ExprKind::StructGet(_, o, _) => {
            if let ExprKind::Local(l) = o.kind {
                fields_used[l] += 1;
            }
        }
        ExprKind::StructSet(_, o, _, _) => {
            if let ExprKind::Local(l) = o.kind {
                fields_used[l] += 1;
                sets[l] += 1;
            }
        }
        ExprKind::Local(l) => total[*l] += 1,
        _ => {
            for (l, ok) in ok.iter_mut().enumerate() {
                if *ok && local_refs(e, l) != (0, 0) {
                    *ok = false;
                }
            }
        }
    });
    // Per replaced local: the locals of its fields.
    let mut fields: Vec<Option<Vec<LocalId>>> = vec![None; n];
    for l in 0..n {
        if ok[l] && inits[l] == 1 && total[l] == fields_used[l] && sets[l] == top_sets[l] {
            let Ty::Struct(k) = f.locals[l] else { unreachable!() };
            let mut map = Vec::new();
            for fd in &layouts[k as usize].fields {
                map.push(f.locals.len());
                f.locals.push(field_local_ty(fd));
                f.boxed.push(false);
                f.local_names.push(None);
            }
            fields[l] = Some(map);
        }
    }
    if fields.iter().all(Option::is_none) {
        return;
    }
    let coerce = |k: u32, fi: u32, v: Expr| -> Expr {
        let fd = &layouts[k as usize].fields[fi as usize];
        let want = field_local_ty(fd);
        let fits = match (fd.storage, v.ty) {
            // A JS value is checked unless of the `aot type` it must be.
            (Storage::Value, t) => t == want && matches!(t, Ty::Struct(_)),
            (_, t) if t == want => true,
            (Storage::F64, Ty::I32) => true,
            _ => false,
        };
        if fits {
            return v;
        }
        if fd.storage == Storage::Int32
            && let ExprKind::Num(c) = v.kind
            && c.fract() == 0.0
            && c.abs() <= i32::MAX as f64
            && !(c == 0.0 && c.is_sign_negative())
        {
            return Expr::new(Ty::I32, ExprKind::Num(c));
        }
        Expr::new(want, ExprKind::StructCoerce(k, fi, Box::new(v)))
    };
    let field_of = |l: LocalId| fields.get(l).and_then(Option::as_ref);
    visit_stmts_mut(&mut f.body, &mut |e| {
        let new = match &mut e.kind {
            ExprKind::InitLocal(l, x) if field_of(*l).is_some() => {
                let ExprKind::StructNew { layout, fields: values, .. } =
                    std::mem::replace(&mut x.kind, ExprKind::Undefined)
                else {
                    unreachable!()
                };
                let map = field_of(*l).unwrap();
                let mut seq: Vec<Expr> = values
                    .into_iter()
                    .map(|(fi, v)| {
                        let fl = map[fi as usize];
                        let v = coerce(layout, fi, v);
                        Expr::new(v.ty, ExprKind::InitLocal(fl, Box::new(v)))
                    })
                    .collect();
                seq.push(Expr::new(Ty::Any, ExprKind::Undefined));
                ExprKind::Seq(seq)
            }
            ExprKind::StructGet(_, o, fi) => match o.kind {
                ExprKind::Local(l) if field_of(l).is_some() => ExprKind::Local(field_of(l).unwrap()[*fi as usize]),
                _ => return,
            },
            ExprKind::StructSet(k, o, fi, v) => match o.kind {
                ExprKind::Local(l) if field_of(l).is_some() => {
                    let v = std::mem::replace(&mut **v, Expr::new(Ty::Any, ExprKind::Undefined));
                    let v = coerce(*k, *fi, v);
                    let ty = v.ty;
                    e.ty = ty;
                    ExprKind::SetLocal(field_of(l).unwrap()[*fi as usize], Box::new(v))
                }
                _ => return,
            },
            _ => return,
        };
        e.kind = new;
    });
}

/// The type of the local holding a field after `scalar_replace_structs`.
fn field_local_ty(f: &LayoutField) -> Ty {
    match f.storage {
        Storage::Int32 => Ty::I32,
        Storage::F64 => Ty::F64,
        Storage::Bool => Ty::Bool,
        Storage::Value => f.ty,
    }
}

/// Whether `e` (this node, not its subexpressions) can run no other code
/// and throw nothing: numbers, booleans, locals, typed array elements, and
/// fields of `aot type` objects (`promote_struct_fields`; plain stores of
/// values of the field's type).
fn pure_node(e: &Expr, layouts: &[Layout]) -> bool {
    match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Local(_)
        | ExprKind::SetLocal(..)
        | ExprKind::InitLocal(..)
        | ExprKind::Update { .. }
        | ExprKind::Unary(..)
        | ExprKind::Binary(..)
        | ExprKind::Compare(..)
        | ExprKind::Logical(..)
        | ExprKind::Cond(..)
        | ExprKind::Truthy(_)
        | ExprKind::Seq(_)
        | ExprKind::Undefined
        | ExprKind::Null
        | ExprKind::Str(_)
        | ExprKind::Box(_)
        | ExprKind::TypedGet(..)
        | ExprKind::TypedSet(..)
        | ExprKind::IntOf(_) => true,
        // Converting a JS value may run code.
        ExprKind::Math(_, args) => args.iter().all(|a| a.ty != Ty::Any),
        ExprKind::StructGet(k, o, fi) => {
            matches!(o.ty, Ty::Struct(_)) && layouts[*k as usize].fields[*fi as usize].universal
        }
        ExprKind::StructSet(k, o, fi, v) => {
            let f = &layouts[*k as usize].fields[*fi as usize];
            matches!(o.ty, Ty::Struct(_))
                && f.simple
                && match f.storage {
                    Storage::Int32 => v.ty == Ty::I32,
                    Storage::F64 => v.ty.is_number(),
                    Storage::Bool => v.ty == Ty::Bool,
                    Storage::Value => false,
                }
        }
        ExprKind::Inline { body, .. } => pure_stmts(body, layouts, None),
        _ => false,
    }
}

/// `pure_node` over statements; `loop_id`: the loop they are in (breaks
/// and continues may only target it or loops within), `None` in an
/// inlined body (whose `return`s end it).
fn pure_stmts(stmts: &[Stmt], layouts: &[Layout], loop_id: Option<LoopId>) -> bool {
    let mut inner = Vec::new();
    stmt_lists(stmts, &mut |list| {
        for s in list {
            if let Stmt::Loop { id, .. } = s {
                inner.push(*id);
            }
        }
    });
    let mut ok = true;
    stmt_lists(stmts, &mut |list| {
        for s in list {
            ok &= match s {
                Stmt::Expr(_) | Stmt::If(..) | Stmt::Block(_) | Stmt::Loop { .. } => true,
                Stmt::Break(id) | Stmt::Continue(id) => Some(*id) == loop_id || inner.contains(id),
                Stmt::Return(_) => loop_id.is_none(),
                _ => false,
            };
        }
    });
    if ok {
        visit_stmts(stmts, &mut |e| ok &= pure_node(e, layouts));
    }
    ok
}

/// Fields of `aot type` objects in registers: in a loop that runs no other
/// code and throws nothing (`pure_stmts`), nothing else can see the object
/// of a struct local, so the fields it uses live in locals for the loop,
/// loaded before it and stored back after it. Locals of the same layout
/// could be the same object: then nothing is done for that layout.
pub fn promote_struct_fields(f: &mut Func, layouts: &[Layout]) {
    let mut body = std::mem::take(&mut f.body);
    promote_in(&mut body, f, layouts);
    f.body = body;
}

fn promote_in(stmts: &mut [Stmt], f: &mut Func, layouts: &[Layout]) {
    let mut i = 0;
    while i < stmts.len() {
        if let Stmt::Loop { .. } = &stmts[i]
            && let Some(block) = promote_loop(&mut stmts[i], f, layouts)
        {
            stmts[i] = block;
            i += 1;
            continue;
        }
        match &mut stmts[i] {
            Stmt::If(_, a, b) => {
                promote_in(a, f, layouts);
                promote_in(b, f, layouts);
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => promote_in(body, f, layouts),
            Stmt::Switch { cases, .. } => cases.iter_mut().for_each(|(_, b)| promote_in(b, f, layouts)),
            Stmt::Try { body, catch, finally, .. } => {
                promote_in(body, f, layouts);
                catch.iter_mut().for_each(|(_, c)| promote_in(c, f, layouts));
                finally.iter_mut().for_each(|c| promote_in(c, f, layouts));
            }
            _ => {}
        }
        i += 1;
    }
}

/// `promote_struct_fields` for one loop: the block replacing it, if any.
fn promote_loop(s: &mut Stmt, f: &mut Func, layouts: &[Layout]) -> Option<Stmt> {
    let Stmt::Loop { id, test, body, update, .. } = s else { return None };
    let parts: Vec<Stmt> = test.iter().chain(update.iter()).map(|e| Stmt::Expr(e.clone())).collect();
    if !pure_stmts(body, layouts, Some(*id)) || !pure_stmts(&parts, layouts, Some(*id)) {
        return None;
    }
    // Struct locals used for fields: their layout, fields read and written.
    let mut fields: HashMap<LocalId, (u32, Vec<u32>, Vec<u32>)> = HashMap::new();
    let mut count = |e: &Expr| {
        if let ExprKind::StructGet(k, o, fi) | ExprKind::StructSet(k, o, fi, _) = &e.kind
            && let ExprKind::Local(l) = o.kind
        {
            let entry = fields.entry(l).or_insert((*k, Vec::new(), Vec::new()));
            let list = if matches!(e.kind, ExprKind::StructSet(..)) { &mut entry.2 } else { &mut entry.1 };
            if !list.contains(fi) {
                list.push(*fi);
            }
        }
    };
    visit_stmts(body, &mut count);
    test.iter().chain(update.iter()).for_each(|e| visit(e, &mut count));
    // A local as the object of a field access is also a `Local` node: the
    // ones used otherwise have more `Local` nodes than accesses.
    let mut uses: HashMap<LocalId, usize> = HashMap::new();
    let mut accesses: HashMap<LocalId, usize> = HashMap::new();
    let mut tally = |e: &Expr| match &e.kind {
        ExprKind::Local(l) => *uses.entry(*l).or_default() += 1,
        ExprKind::StructGet(_, o, _) | ExprKind::StructSet(_, o, _, _) => {
            if let ExprKind::Local(l) = o.kind {
                *accesses.entry(l).or_default() += 1;
            }
        }
        ExprKind::SetLocal(l, _) | ExprKind::InitLocal(l, _) | ExprKind::Update { local: l, .. } => {
            *uses.entry(*l).or_default() += 1000;
        }
        _ => {}
    };
    visit_stmts(body, &mut tally);
    test.iter().chain(update.iter()).for_each(|e| visit(e, &mut tally));
    let mut candidates: Vec<(LocalId, u32, Vec<u32>, Vec<u32>)> = fields
        .into_iter()
        .filter(|(l, _)| !f.boxed[*l] && uses.get(l) == accesses.get(l))
        .map(|(l, (k, r, w))| (l, k, r, w))
        .collect();
    // One local per layout (two could hold the same object).
    let mut per_layout: HashMap<u32, usize> = HashMap::new();
    for c in &candidates {
        *per_layout.entry(c.1).or_default() += 1;
    }
    candidates.retain(|c| per_layout[&c.1] == 1);
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|c| c.0);
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut map: HashMap<(LocalId, u32), LocalId> = HashMap::new();
    for (l, k, reads, writes) in &candidates {
        let lt = f.locals[*l];
        let mut all: Vec<u32> = reads.iter().chain(writes).copied().collect();
        all.sort_unstable();
        all.dedup();
        for fi in all {
            let fd = &layouts[*k as usize].fields[fi as usize];
            let t = field_local_ty(fd);
            let tmp = f.locals.len();
            f.locals.push(t);
            f.boxed.push(false);
            f.local_names.push(None);
            map.insert((*l, fi), tmp);
            let get = Expr::new(fd.ty, ExprKind::StructGet(*k, Box::new(Expr::new(lt, ExprKind::Local(*l))), fi));
            before.push(Stmt::Expr(Expr::new(t, ExprKind::InitLocal(tmp, Box::new(get)))));
            if writes.contains(&fi) {
                let v = Expr::new(t, ExprKind::Local(tmp));
                let obj = Box::new(Expr::new(lt, ExprKind::Local(*l)));
                after.push(Stmt::Expr(Expr::new(t, ExprKind::StructSet(*k, obj, fi, Box::new(v)))));
            }
        }
    }
    let mut rewrite = |e: &mut Expr| {
        let new = match &mut e.kind {
            ExprKind::StructGet(_, o, fi) => match o.kind {
                ExprKind::Local(l) if map.contains_key(&(l, *fi)) => ExprKind::Local(map[&(l, *fi)]),
                _ => return,
            },
            ExprKind::StructSet(_, o, fi, v) => match o.kind {
                ExprKind::Local(l) if map.contains_key(&(l, *fi)) => {
                    let v = std::mem::replace(&mut **v, Expr::new(Ty::Any, ExprKind::Undefined));
                    ExprKind::SetLocal(map[&(l, *fi)], Box::new(v))
                }
                _ => return,
            },
            _ => return,
        };
        e.kind = new;
    };
    visit_stmts_mut(body, &mut rewrite);
    test.iter_mut().chain(update.iter_mut()).for_each(|e| visit_mut(e, &mut rewrite));
    let lp = std::mem::replace(s, Stmt::Block(Vec::new()));
    before.push(lp);
    before.extend(after);
    Some(Stmt::Block(before))
}
