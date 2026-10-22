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
    /// An array of objects of layout `k` (`LumStructArray`; like `Any`).
    StructArr(u32),
    /// An element of such an array, or a field or parameter that may be
    /// absent or null: an object of layout `k`, or `undefined` / `null`.
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
    /// Record types: object types with a name, then the instances of
    /// classes made natively (see `class_records`).
    pub records: Vec<Record>,
    /// Per class: the record of its instances, if made natively.
    pub class_records: Vec<Option<usize>>,
}

/// An object type with known fields in a known order (see
/// `specialize_fields`): an object whose shape has these fields first, as
/// plain writable data properties, has field `i` in slot `i`.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub fields: Vec<AtomId>,
    /// What the fields' annotations say they hold.
    pub hints: Vec<Option<Hint>>,
    /// Which fields are annotated as numbers.
    pub nums: Vec<bool>,
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
    /// An array of objects of this layout (`LumStructArray`).
    Array(u32),
    /// This type, or `null`.
    Nullable(Box<FieldType>),
}

impl FieldType {
    /// The type without `null`.
    pub fn base(&self) -> &FieldType {
        match self {
            FieldType::Nullable(t) => t,
            t => t,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LayoutField {
    pub name: String,
    pub storage: Storage,
    /// Byte offset in the object's data.
    pub offset: u32,
    /// Present in every member, and optional in none (so in every object).
    pub universal: bool,
    /// The type a read gives: of the value if `universal`, else also
    /// `undefined` (`MaybeF64` for numbers, `StructOrUndef` for objects of
    /// an `aot type`, else `Any`).
    pub ty: Ty,
    /// Of the same type in every member: a write cannot change which member
    /// the object matches.
    pub simple: bool,
}

#[derive(Clone, Debug)]
pub struct LayoutMember {
    /// The member's fields (bit `i`: field `i`).
    pub mask: u32,
    /// Those that may be absent (`x?: T`, `x: T | undefined`).
    pub optional: u32,
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
    /// Its instance fields annotated as numbers.
    pub nums: Vec<String>,
    /// Classes whose instances native constructors make: the fields they
    /// define, in order (its superclasses' first), else empty.
    pub instance_fields: Vec<String>,
    /// Its instances (or its superclasses') have private fields or brands,
    /// which only their constructors make: they are never virtual.
    pub private_state: bool,
}

/// What a type annotation says a value is, for predicting the methods it
/// reaches (`predict_calls`). Only a hint: predictions are checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    /// An instance of top-level class `k`.
    Class(usize),
    /// An array of them.
    ArrayOf(usize),
    /// An object of record type `r` (`Module::records`).
    Record(usize),
    /// An array of them.
    ArrayOfRecord(usize),
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
    /// `async`: compiled as a resumable step function over a heap frame,
    /// unless `frameless`.
    pub is_async: bool,
    /// An `async` function that never awaits (no `Stmt::Await`): compiled
    /// as other functions are, its JS entry making the promise of its
    /// result (no frame).
    pub frameless: bool,
    /// A closure's text in the bundle's source (method or accessor of an
    /// object literal: with its key): what its `toString` gives, without
    /// types (the others' is their interpreted fallback's).
    pub source: Option<(u32, u32)>,
    /// The last parameter collects the remaining arguments in an array
    /// (before the arguments object, if any).
    pub rest: bool,
    /// The function uses its `arguments` (or arrow functions inside it
    /// do): its last parameter is the arguments object, which its JS entry
    /// makes (native code calls it only through its JS entry).
    pub arguments: bool,
    /// The function reads its `arguments` only as `arguments.length`: that
    /// parameter is the number of arguments (an int), not the object.
    pub arguments_count: bool,
    /// The constructor of a class inside a function: a closure that `new`
    /// calls with `this` new.target (`has_this`), and that makes and
    /// returns the instance.
    pub class_ctor: bool,
    /// `function*`: compiled as a resumable step function over a heap
    /// frame, as async functions are.
    pub is_generator: bool,
    /// An ordinary function (a `function` declaration or expression, not a
    /// method): a constructor in JS, with a `prototype`.
    pub constructor: bool,
    /// In functions that suspend: holds the return value while `finally`
    /// blocks run on the way out (`Any`).
    pub ret_slot: Option<LocalId>,
    /// What the locals hold, as far as type annotations (and values made
    /// from annotated ones) say: see `infer_hints`.
    pub hints: HashMap<LocalId, Hint>,
    /// What the function returns, by its annotation.
    pub ret_hint: Option<Hint>,
    /// Frame objects (`ExprKind::FrameObject`): per slot, the properties
    /// it has room for, and whether it is in the frame's cell (an async
    /// function's object that may live across an await) rather than in
    /// the frame region; and the locals that hold them.
    pub frame: Vec<(u32, bool)>,
    pub frame_locals: Vec<LocalId>,
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
    /// The same of a native method (a private one), with its `this`.
    CallDirectThis(FuncId, Box<Expr>, Vec<Expr>),
    /// A call of native function `func` (module functions, then closures)
    /// through its JS entry, with `this` undefined and these arguments (JS
    /// values): an `async` function, which returns its promise. The entry
    /// declines arguments that do not suit its unboxed parameters only where
    /// `callee` (its function object) is given: that is then called instead,
    /// with `slow` if given (evaluated only then), else the same arguments.
    /// `Any`.
    CallEntry {
        func: FuncId,
        args: Vec<Expr>,
        callee: Option<Box<Expr>>,
        slow: Option<Vec<Expr>>,
    },
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
    /// A regular expression literal (its pattern and flags): a new RegExp
    /// object each time, of a pattern compiled once.
    RegExp(StrId, StrId),
    /// A BigInt literal below 2^64.
    BigInt(u64),
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
    /// Define own property `obj.atom` (enumerable, writable, configurable)
    /// as `value`, as a class field is: no setter runs. `Void`.
    DefineField(Box<Expr>, AtomId, Box<Expr>),
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
    /// `super(args)` in the native constructor of class `class` whose
    /// superclass has no native constructor: the superclass (the class's
    /// prototype, as it is then) constructed with `new.target` `target`.
    SuperNew { class: Box<Expr>, target: Box<Expr>, args: Vec<Expr> },
    /// The same, the arguments in an array.
    SuperNewSpread { class: Box<Expr>, target: Box<Expr>, args: Box<Expr> },
    /// Home object slot `k` (`Module::homes`): a top-level class's
    /// prototype (even `k`) or the class (odd `k`).
    HomeValue(usize),
    /// The function object of the closure running (a class constructor's:
    /// the class).
    CurrentFunction,
    /// A class inside a function, made: its constructor (a closure with
    /// `Func::class_ctor`) given its superclass, if it has one; evaluates
    /// to its prototype.
    ClassSetup { ctor: Box<Expr>, parent: Option<Box<Expr>> },
    /// Method (`kind` 0), getter (1) or setter (2) `func` of a class inside
    /// a function, defined on `obj` (its prototype, or the class) as a
    /// class definition does (not enumerable).
    /// (A computed key, `Key::Value`, is a property key; the function is
    /// named for it.)
    DefineMethod { obj: Box<Expr>, key: Key, func: Box<Expr>, kind: u8 },
    /// The instance a base class's constructor makes for `new.target`.
    NewFromTarget(Box<Expr>),
    /// Object literal with plain data properties, defined in order.
    Object(Vec<(AtomId, Expr)>),
    /// `obj` (an `Object` or a `NewInstance`) made in slot `slot` of the
    /// function's frame (`Func::frame`) while there is room, else in the
    /// heap: an object that never outlives the call (see `frames`).
    FrameObject {
        slot: u32,
        obj: Box<Expr>,
    },
    /// A new instance of top-level class `class` (whose prototype is home
    /// object `2 * class`) with these own fields, in order (enumerable,
    /// writable, configurable), as its constructor makes it. `Any`.
    NewInstance {
        class: usize,
        fields: Vec<(AtomId, Expr)>,
    },
    /// `NewInstance` of a class whose instances also have private state:
    /// after its public fields, the brand of the class whose home object
    /// is `brand` (its prototype's slot), then its private fields
    /// (`Module::privates` slots, values). The allocation site caches the
    /// final shape, and later instances are made with all of it at once.
    NewInstanceFull {
        class: usize,
        fields: Vec<(AtomId, Expr)>,
        brand: Option<usize>,
        privates: Vec<(usize, Expr)>,
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
    /// The `for`-`in` iterator of a value; local `idx` becomes 0 when it
    /// is the shared one of the value's shape (stepped by `ForInStep` with
    /// that index), else `undefined`.
    ForInIterator(Box<Expr>, LocalId),
    /// The next key of the `for`-`in` iterator in local `iter` (index `idx`,
    /// see `ForInIterator`) over the object in local `obj`. When it is
    /// done, or throws, `iter` becomes `undefined`; the value is then
    /// `undefined`.
    ForInStep(LocalId, LocalId, LocalId),
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
    /// (index `idx`) whose current key `key` may be (a direct read when it
    /// is, and obj has the shape being enumerated).
    ForInGet(LocalId, LocalId, Box<Expr>, Box<Expr>),
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
    /// A native constructor's: a private field defined on the instance
    /// (`Void`), and the brand of a class (`Module::homes` slot of its
    /// prototype) given to it, for that class's private methods and
    /// accessors (`Void`).
    PrivateDefine(Box<Expr>, usize, Box<Expr>),
    AddBrand(Box<Expr>, usize),
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
    /// An argument of a predicted method call that is evaluated only when
    /// the call is made, not when the prediction holds and the body runs
    /// inline (which does not use its parameter): see
    /// `virtualize_instances`. Elsewhere, the value itself.
    Lazy(Box<Expr>),
    /// `obj.atom`, with `obj` expected to be of record type `record` (by a
    /// type annotation), where `atom` is field `slot`: read from that slot
    /// when the object has the record's shape (`specialize_fields`), else
    /// as `GetProp`. `Any`.
    ShapeGet {
        obj: Box<Expr>,
        atom: AtomId,
        record: usize,
        slot: u32,
    },
    /// `obj.atom = value` likewise (as `SetProp` otherwise); evaluates to
    /// the value.
    ShapeSet {
        obj: Box<Expr>,
        atom: AtomId,
        record: usize,
        slot: u32,
        value: Box<Expr>,
    },
    /// The guard of a speculative region (see `spec::speculate_numbers`),
    /// only as the condition of its `Stmt::If`: whether each local of
    /// `objs` holds an object with the shape of its record (all distinct
    /// objects if `distinct`), and its fields `(obj, slot)` in `fields` and
    /// the JS-valued locals `(from, to)` in `nums` hold numbers. If so, the
    /// numbers of `nums` are now in their `F64` locals `to`. `Bool`.
    SpecGuard {
        objs: Vec<(LocalId, usize)>,
        distinct: bool,
        fields: Vec<(LocalId, u32)>,
        nums: Vec<(LocalId, LocalId)>,
        /// Locals that must hold flat strings, and whether
        /// `String.prototype.charCodeAt` must be the builtin.
        strs: Vec<LocalId>,
        char_code_at: bool,
        /// Objects held by fields of guarded objects, in order: `(obj, of,
        /// slot, record)`, the object in field `slot` of `of`, of `record`
        /// (`obj` names it in the region; it is guarded as `objs` are).
        derived: Vec<(LocalId, LocalId, u32, usize)>,
        /// Method calls the region runs inline: `(obj, class, method,
        /// func)`, the prototype of `obj`'s shape (if any) is `class`'s,
        /// whose own data property `method` is native function `func`.
        methods: Vec<(Option<LocalId>, usize, AtomId, usize)>,
        /// Cells (class bindings) that must be initialized.
        cells: Vec<CellId>,
        /// Locals that must hold dense fast arrays (`Array`s whose
        /// `length` is their element count), whose elements the region's
        /// loop reads (`ElemGuard`); those of typed array types, typed
        /// arrays with a pristine shape (`length` the element count).
        arrays: Vec<LocalId>,
        /// `(record, class)`: once its shape is known, `record`'s objects
        /// have `class`'s prototype (elements whose methods run inline).
        protos: Vec<(usize, usize)>,
        /// Typed arrays the region writes (`RegionTypedSet`): their buffers
        /// must not be immutable.
        typed_writes: Vec<LocalId>,
        /// Locals that must hold `Array`s `push` appends to (`RegionPush`):
        /// fast, extensible, with a fresh array's shape and `length` their
        /// element count, `Array.prototype.push` the builtin, and no index
        /// properties on the array prototypes.
        appends: Vec<LocalId>,
    },
    /// In a speculative region's loop, around each iteration: whether the
    /// objects the iteration reads from arrays hold what the region needs
    /// (only as the condition of its `Stmt::If`, whose `then` branch is the
    /// iteration and whose other branch leaves the region: see `spec`). `elems`: `(obj, arr, idx, record)`, element
    /// `idx` (an `F64` or `I32` local) of the array in local `arr` (one of
    /// the guard's `arrays`) is an object of `record`'s shape, named `obj`
    /// in the region (the array holds it); `derived`: objects their fields
    /// hold, as `SpecGuard`'s; `fields`: their fields that hold numbers
    /// (int32s or floats), read and written in place (`SlotLoad`,
    /// `SlotStore`); `distinct`: the objects each of them must not be (the
    /// region's, whose fields it keeps in locals, or reads as floats
    /// where elements are written); `nums`: `(to, arr, idx)`, element
    /// `idx` of the array in local `arr` is a number (an int32 or a
    /// float), loaded into `F64` local `to`; `bounds`: `(arr, idx)`,
    /// index `idx` is inside the typed array in local `arr` (whose
    /// elements the iteration reads, `RegionTypedGet`). `Bool`.
    ElemGuard {
        elems: Vec<(LocalId, LocalId, LocalId, usize)>,
        nums: Vec<(LocalId, LocalId, LocalId)>,
        bounds: Vec<(LocalId, LocalId)>,
        derived: Vec<(LocalId, LocalId, u32, usize)>,
        fields: Vec<(LocalId, u32)>,
        distinct: Vec<LocalId>,
    },
    /// While speculative regions are made (`spec`), where one leaves by a
    /// jump from inside its loops: replaced by the statements that leave
    /// (once the region is known). `Void`.
    RegionLeave(usize),
    /// In a speculative region: the length of the dense fast array in local
    /// `arr` (a guard's `arrays`). `F64`.
    ArrLen {
        arr: LocalId,
    },
    /// In a speculative region's loop: element `idx` (an index local) of
    /// the typed array in local `arr`, inside it (the iteration's guard
    /// checked: `bounds`). `F64`.
    RegionTypedGet {
        arr: LocalId,
        idx: LocalId,
    },
    /// In a speculative region: `arr[idx] = value` for the typed array in
    /// local `arr` (a guard's `typed_writes`), `idx` and `value` numbers:
    /// nothing where `idx` is not an index inside it. Evaluates to the
    /// value. `F64`.
    RegionTypedSet {
        arr: LocalId,
        idx: Box<Expr>,
        value: Box<Expr>,
    },
    /// In a speculative region: `s.charCodeAt(idx)` for the flat string in
    /// local `s` (the builtin, checked by the guard), `idx` a number. `F64`.
    StrCharCode {
        s: LocalId,
        idx: Box<Expr>,
    },
    /// In a speculative region: the length of the flat string in local `s`.
    /// `F64`.
    StrLen {
        s: LocalId,
    },
    /// In a speculative region: `arr.push(value)` for the array in local
    /// `arr` (a guard's `appends`), `value` a JS value made of numbers (an
    /// object literal, or a number). `F64` (0).
    RegionPush {
        arr: LocalId,
        value: Box<Expr>,
    },
    /// In a speculative region: the number field `slot` of the object in
    /// local `obj` held when the region's guard checked it. `F64`.
    SlotLoad {
        obj: LocalId,
        slot: u32,
    },
    /// In a speculative region: store number `value` (`F64`) in field
    /// `slot` of the object in local `obj`, which the region's guard
    /// checked (the slot holds a number: nothing to free); as a float when
    /// only floats were read and `arith` (the value is the result of
    /// arithmetic, which makes floats of floats). Evaluates to the value.
    SlotStore {
        obj: LocalId,
        slot: u32,
        value: Box<Expr>,
        arith: bool,
    },
    /// Whether class `class` (home object `2 * class + 1`) still has class
    /// `parent` as its prototype: its superclass. `Bool`.
    ProtoIs {
        class: usize,
        parent: usize,
    },
    /// Whether the prototype of class `class` still has native function
    /// `func` as its own data property `method` (no getter runs). `Bool`.
    /// (See `virtualize_instances`.)
    ProtoMethodIs {
        class: usize,
        method: AtomId,
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
        /// The arguments of the call of `callee`, if not `args` (evaluated
        /// only for it).
        slow: Option<Vec<Expr>>,
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
    /// The same for a store of property `atom`: else what JS throws setting
    /// it on `undefined`.
    NonNullSet(AtomId, Box<Expr>),
    /// A value as an object of `aot type` layout `desc >> 1` (an array of
    /// them if `desc` is odd): itself if one, else converted
    /// (`lum_struct_convert`). Of `JSON.parse(text)`, which then parses to
    /// the type directly while it is the builtin.
    StructConvert(i32, Box<Expr>),
    /// `r.json()` whose result is converted to `desc` (`lum_json_method_as`:
    /// the host may parse the body to the type directly).
    JsonMethodAs(i32, Box<Expr>),
    /// `o.keys(x).length` (`Object.keys`): counted without the array while
    /// the method is the builtin (`lum_keys_length`).
    KeysLength(Box<Expr>, AtomId, Box<Expr>),
    /// `o.create(x)` (`Object.create` with one argument): while the method
    /// is the builtin, an object of prototype x made with the empty shape
    /// the site caches (`lum_object_create`), else a call.
    ObjectCreate(Box<Expr>, AtomId, Box<Expr>),
    /// Whether `o.k = v`, on an object made by an object literal (whose
    /// prototype is Object.prototype) that does not have `k`, adds it as an
    /// own data property: Object.prototype has no setter nor read-only `k`
    /// (`lum_plain_add_ok`). `Bool`. (See `virtualize_objects`.)
    PlainAddOk(AtomId),
    /// Whether a value is the builtin `Object.create`. `Bool`.
    IsObjectCreate(Box<Expr>),
    /// A new object without a prototype (as `Object.create(null)`). `Any`.
    NullObject,
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
    /// `__proto__: value`: the prototype, if `value` is an object or null.
    Proto(Expr),
    /// `[key]() {}` (0), `get [key]() {}` (1) or `set [key](v) {}` (2): the
    /// key, then the function (named after the key when it is defined).
    ComputedMethod(Expr, Expr, u8),
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
    /// One, or `null` or `undefined`.
    StructOrNullish(u32),
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
            MathFn::Round => "lum_round",
            MathFn::Pow => "lum_pow",
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
                | RegExp(..)
                | BigInt(_)
                | Cell(_)
                | Global(_)
                | GlobalOrUndefined(_)
                | This
                | IterStep(..)
                | ForInStep(..)
                | IterClose(..)
                | IterCloseQuiet(_)
                | SuperGetAtom(..)
                | ProtoMethodIs { .. }
                | ProtoIs { .. }
                | PlainAddOk(_)
                | NullObject
                | SpecGuard { .. }
                | ElemGuard { .. }
                | ArrLen { .. }
                | RegionTypedGet { .. }
                | RegionLeave(_)
                | SlotLoad { .. }
                | StrLen { .. }
                | HomeValue(_)
                | CurrentFunction
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
                ShapeGet { obj, .. } | FrameObject { obj, .. } => $name(obj, f),
                SlotStore { value, .. } | StrCharCode { idx: value, .. } | RegionPush { value, .. } => $name(value, f),
                RegionTypedSet { idx, value, .. } => {
                    $name(idx, f);
                    $name(value, f);
                }
                ShapeSet { obj, value, .. } => {
                    $name(obj, f);
                    $name(value, f);
                }
                CallChecked { callee, args, slow, .. } => {
                    for x in args {
                        $name(x, f);
                    }
                    $name(callee, f);
                    if let Some(xs) = slow {
                        for x in xs {
                            $name(x, f);
                        }
                    }
                }
                CallEntry { args, callee, slow, .. } => {
                    for x in args {
                        $name(x, f);
                    }
                    if let Some(c) = callee {
                        $name(c, f);
                    }
                    if let Some(xs) = slow {
                        for x in xs {
                            $name(x, f);
                        }
                    }
                }
                StructNew { fields, .. } => {
                    for (_, x) in fields {
                        $name(x, f);
                    }
                }
                StructGet(_, a, _)
                | StructCoerce(_, _, a)
                | StructArrFrom(_, a)
                | StructArrLen(a)
                | NonNull(_, a)
                | NonNullSet(_, a)
                | StructConvert(_, a)
                | JsonMethodAs(_, a)
                | IsObjectCreate(a) => {
                    $name(a, f)
                }
                StructArrGet(_, a, b) | StructArrPush(_, a, b) | KeysLength(a, _, b) | ObjectCreate(a, _, b) => {
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
                | ForInIterator(a, _)
                | ToObject(a)
                | ToPropKey(a)
                | PrivateField(a, _)
                | PrivateMethod(a, _)
                | PrivateGetter(a, _)
                | PrivateIn(a, _)
                | AddBrand(a, _)
                | SuperGet(_, a)
                | ArrayPush(_, a)
                | IntOf(a)
                | AsyncIterator(a)
                | AsyncIterResult(a, _)
                | CheckObject(a)
                | Lazy(a)
                | NewFromTarget(a)
                | Void(a) => $name(a, f),
                SuperNewSpread { class, target, args } => {
                    $name(class, f);
                    $name(target, f);
                    $name(args, f);
                }
                SuperNew { class, target, args } => {
                    $name(class, f);
                    $name(target, f);
                    for x in args {
                        $name(x, f);
                    }
                }
                ClassSetup { ctor, parent } => {
                    $name(ctor, f);
                    if let Some(p) = parent {
                        $name(p, f);
                    }
                }
                DefineMethod { obj, key, func, .. } => {
                    $name(obj, f);
                    if let Key::Value(k) = key {
                        $name(k, f);
                    }
                    $name(func, f);
                }
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
                            ObjPart::Field(_, x) | ObjPart::Spread(x) | ObjPart::Accessor(_, x, _) | ObjPart::Proto(x) => {
                                $name(x, f)
                            }
                            ObjPart::Computed(k, x) | ObjPart::ComputedMethod(k, x, _) => {
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
                ForInGet(_, _, a, b) => {
                    $name(a, f);
                    $name(b, f);
                }
                PrivateSetField(a, _, b) | PrivateSetter(a, _, b) | SuperSet(_, a, b) | PrivateDefine(a, _, b) => {
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
                | DefineField(a, _, b)
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
                CallValue(a, xs) | New(a, xs) | CallDirectThis(_, a, xs) => {
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
                NewInstanceFull { fields, privates, .. } => {
                    for (_, x) in fields {
                        $name(x, f);
                    }
                    for (_, x) in privates {
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

/// The direct subexpressions of `x`, in the order they are evaluated (not
/// the statements of the bodies it holds).
pub fn children_mut(x: &mut Expr) -> Vec<&mut Expr> {
    let mut out: Vec<&mut Expr> = Vec::new();
    macro_rules! push {
        ($e:expr) => {
            out.push($e)
        };
    }
    use ExprKind::*;
    match &mut x.kind {
        Num(_)
        | Bool(_)
        | Local(_)
        | Update { .. }
        | Undefined
        | Null
        | Str(_)
        | RegExp(..)
        | BigInt(_)
        | Cell(_)
        | Global(_)
        | GlobalOrUndefined(_)
        | This
        | IterStep(..)
        | ForInStep(..)
        | IterClose(..)
        | IterCloseQuiet(_)
        | SuperGetAtom(..)
        | ProtoMethodIs { .. }
        | ProtoIs { .. }
        | PlainAddOk(_)
        | NullObject
        | SpecGuard { .. }
        | ElemGuard { .. }
        | ArrLen { .. }
        | RegionTypedGet { .. }
        | RegionLeave(_)
        | SlotLoad { .. }
        | StrLen { .. }
        | HomeValue(_)
        | CurrentFunction
        | Closure(..) => {}
        Inline { .. } => {}
        Predicted { expr, .. } => push!(expr),
        ArrayLoop { recv, callback, .. } => {
            push!(recv);
            push!(callback);
        }
        ArraySort { recv, callback, .. } => {
            push!(recv);
            push!(callback);
        }
        ShapeGet { obj, .. } | FrameObject { obj, .. } => push!(obj),
        SlotStore { value, .. } | StrCharCode { idx: value, .. } | RegionPush { value, .. } => push!(value),
        RegionTypedSet { idx, value, .. } => {
            push!(idx);
            push!(value);
        }
        ShapeSet { obj, value, .. } => {
            push!(obj);
            push!(value);
        }
        CallChecked { callee, args, slow, .. } => {
            for x in args {
                push!(x);
            }
            push!(callee);
            if let Some(xs) = slow {
                for x in xs {
                    push!(x);
                }
            }
        }
        CallEntry { args, callee, slow, .. } => {
            for x in args {
                push!(x);
            }
            if let Some(c) = callee {
                push!(c);
            }
            if let Some(xs) = slow {
                for x in xs {
                    push!(x);
                }
            }
        }
        StructNew { fields, .. } => {
            for (_, x) in fields {
                push!(x);
            }
        }
        StructGet(_, a, _)
        | StructCoerce(_, _, a)
        | StructArrFrom(_, a)
        | StructArrLen(a)
        | NonNull(_, a)
        | NonNullSet(_, a)
        | StructConvert(_, a)
        | JsonMethodAs(_, a)
        | IsObjectCreate(a) => {
            push!(a)
        }
        StructArrGet(_, a, b) | StructArrPush(_, a, b) | KeysLength(a, _, b) | ObjectCreate(a, _, b) => {
            push!(a);
            push!(b);
        }
        StructArrSet(_, a, b, c) => {
            push!(a);
            push!(b);
            push!(c);
        }
        StructSet(_, a, _, b) => {
            push!(a);
            push!(b);
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
        | ForInIterator(a, _)
        | ToObject(a)
        | ToPropKey(a)
        | PrivateField(a, _)
        | PrivateMethod(a, _)
        | PrivateGetter(a, _)
        | PrivateIn(a, _)
        | AddBrand(a, _)
        | SuperGet(_, a)
        | ArrayPush(_, a)
        | IntOf(a)
        | AsyncIterator(a)
        | AsyncIterResult(a, _)
        | CheckObject(a)
        | Lazy(a)
        | NewFromTarget(a)
        | Void(a) => push!(a),
        SuperNewSpread { class, target, args } => {
            push!(class);
            push!(target);
            push!(args);
        }
        SuperNew { class, target, args } => {
            push!(class);
            push!(target);
            for x in args {
                push!(x);
            }
        }
        ClassSetup { ctor, parent } => {
            push!(ctor);
            if let Some(p) = parent {
                push!(p);
            }
        }
        DefineMethod { obj, key, func, .. } => {
            push!(obj);
            if let Key::Value(k) = key {
                push!(k);
            }
            push!(func);
        }
        ArrayParts(parts) => {
            for p in parts {
                if let ArrPart::Value(x) | ArrPart::Spread(x) = p {
                    push!(x);
                }
            }
        }
        ObjectParts(parts) => {
            for p in parts {
                match p {
                    ObjPart::Field(_, x) | ObjPart::Spread(x) | ObjPart::Accessor(_, x, _) | ObjPart::Proto(x) => {
                        push!(x)
                    }
                    ObjPart::Computed(k, x) | ObjPart::ComputedMethod(k, x, _) => {
                        push!(k);
                        push!(x);
                    }
                }
            }
        }
        ObjectRest(a, xs) => {
            push!(a);
            for x in xs {
                push!(x);
            }
        }
        ForInGet(_, _, a, b) => {
            push!(a);
            push!(b);
        }
        PrivateSetField(a, _, b) | PrivateSetter(a, _, b) | SuperSet(_, a, b) | PrivateDefine(a, _, b) => {
            push!(a);
            push!(b);
        }
        NewSpread(a, b) => {
            push!(a);
            push!(b);
        }
        CallSpread(a, b, c) => {
            push!(a);
            push!(b);
            push!(c);
        }
        TypedGet(_, a, b) => {
            push!(a);
            push!(b);
        }
        TypedSet(_, a, b, c) => {
            push!(a);
            push!(b);
            push!(c);
        }
        Binary(_, a, b)
        | NumArith(_, a, b)
        | Compare(_, a, b)
        | Logical(_, a, b)
        | AnyBinary(_, a, b)
        | AnyCompare(_, a, b)
        | SetProp(a, _, b)
        | DefineField(a, _, b)
        | GetElem(a, b)
        | DeleteElem(a, b) => {
            push!(a);
            push!(b);
        }
        Cond(a, b, c) | SetElem(a, b, c) => {
            push!(a);
            push!(b);
            push!(c);
        }
        Call(_, xs) | Math(_, xs) | Seq(xs) | Array(xs) => {
            for x in xs {
                push!(x);
            }
        }
        CallValue(a, xs) | New(a, xs) | CallDirectThis(_, a, xs) => {
            push!(a);
            for x in xs {
                push!(x);
            }
        }
        CallThis(a, b, xs) => {
            push!(a);
            push!(b);
            for x in xs {
                push!(x);
            }
        }
        CallMethod(a, k, xs) => {
            push!(a);
            if let Key::Value(k) = k {
                push!(k);
            }
            for x in xs {
                push!(x);
            }
        }
        Object(props) | NewInstance { fields: props, .. } => {
            for (_, x) in props {
                push!(x);
            }
        }
        NewInstanceFull { fields, privates, .. } => {
            for (_, x) in fields {
                push!(x);
            }
            for (_, x) in privates {
                push!(x);
            }
        }
    }
    out
}

/// A function that can neither throw nor call: only unboxed numbers and
/// booleans, arithmetic, comparisons and control flow. Callers skip the
/// unwinding check after calling it, and it skips the stack check.
pub fn nothrow_leaf(f: &Func) -> bool {
    use ExprKind as K;
    fn expr_ok(e: &Expr) -> bool {
        e.ty != Ty::Any
            && match &e.kind {
                K::Num(_) | K::Bool(_) | K::Local(_) | K::Update { .. } => true,
                K::SetLocal(_, x) | K::InitLocal(_, x) | K::Unary(_, x) | K::Truthy(x) => expr_ok(x),
                K::Binary(_, x, y) | K::Compare(_, x, y) | K::Logical(_, x, y) => expr_ok(x) && expr_ok(y),
                K::Cond(c, x, y) => expr_ok(c) && expr_ok(x) && expr_ok(y),
                K::Math(_, xs) | K::Seq(xs) => xs.iter().all(expr_ok),
                _ => false,
            }
    }
    fn stmts_ok(ss: &[Stmt]) -> bool {
        ss.iter().all(|s| match s {
            Stmt::Expr(e) => expr_ok(e),
            Stmt::If(c, a, b) => expr_ok(c) && stmts_ok(a) && stmts_ok(b),
            Stmt::Loop { test, body, update, per_iteration, .. } => {
                per_iteration.is_empty()
                    && test.as_ref().is_none_or(expr_ok)
                    && stmts_ok(body)
                    && update.as_ref().is_none_or(expr_ok)
            }
            Stmt::Break(_) | Stmt::Continue(_) => true,
            Stmt::Return(e) => e.as_ref().is_none_or(expr_ok),
            Stmt::Block(b) => stmts_ok(b),
            _ => false,
        })
    }
    !f.is_async
        && !f.is_generator
        && !f.is_closure
        && !f.has_this
        && f.ret_slot.is_none()
        && f.boxed.iter().all(|b| !b)
        && f.locals.iter().all(|t| matches!(t, Ty::F64 | Ty::I32 | Ty::Bool))
        && matches!(f.ret, Ty::F64 | Ty::I32 | Ty::Bool | Ty::Void)
        && stmts_ok(&f.body)
}

impl Func {
    /// Whether it is compiled as a step function over a heap frame: it may
    /// suspend (`await`, `yield`).
    pub fn suspends(&self) -> bool {
        (self.is_async && !self.frameless) || self.is_generator
    }

    /// The rest parameter's index.
    pub fn rest_param(&self) -> Option<usize> {
        self.rest.then(|| self.params.len() - 1 - self.arguments as usize)
    }

    /// The arguments object's parameter index.
    pub fn arguments_param(&self) -> Option<usize> {
        self.arguments.then(|| self.params.len() - 1)
    }
}

/// Whether `stmts` await (`Stmt::Await`, `for await` included).
pub fn has_await(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        Stmt::Await { .. } => true,
        Stmt::If(_, a, b) => has_await(a) || has_await(b),
        Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => has_await(body),
        Stmt::Switch { cases, .. } => cases.iter().any(|(_, b)| has_await(b)),
        Stmt::Try { body, catch, finally, .. } => {
            has_await(body)
                || catch.as_ref().is_some_and(|(_, b)| has_await(b))
                || finally.as_ref().is_some_and(|b| has_await(b))
        }
        _ => false,
    })
}

/// Whether evaluating `x` may assign local `l` (only the function's own code
/// assigns a local that is not captured).
pub fn assigns_local(x: &Expr, l: LocalId) -> bool {
    let mut found = false;
    visit(x, &mut |e| {
        found |= match e.kind {
            ExprKind::SetLocal(m, _) | ExprKind::InitLocal(m, _) | ExprKind::Update { local: m, .. } => m == l,
            ExprKind::IterStep(m, n) => m == l || n == Some(l),
            ExprKind::ForInIterator(_, n) => n == l,
            ExprKind::ForInStep(m, n, _) => m == l || n == l,
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
pub(crate) fn stmt_locals(stmts: &[Stmt], f: &mut impl FnMut(LocalId)) {
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

/// What `SpecGuard`, `SlotLoad` and `SlotStore` name for the function's
/// `this` (a method's receiver, or a predicted call's in its body).
pub const THIS_LOCAL: LocalId = usize::MAX;

/// Calls `f` on the locals expression `e` names itself (not through its
/// subexpressions), and on those the statements of a body it holds name.
pub(crate) fn expr_locals(e: &Expr, f: &mut impl FnMut(LocalId)) {
    let f = &mut |l: LocalId| {
        if l != THIS_LOCAL {
            f(l)
        }
    };
    use ExprKind::*;
    match &e.kind {
        Local(l)
        | SetLocal(l, _)
        | InitLocal(l, _)
        | ArrayPush(l, _)
        | AsyncIterResult(_, l)
        | IterCloseQuiet(l)
        | ForInIterator(_, l)
        | Update { local: l, .. }
        | StrCharCode { s: l, .. }
        | StrLen { s: l }
        | ArrLen { arr: l }
        | RegionPush { arr: l, .. }
        | RegionTypedSet { arr: l, .. }
        | SlotLoad { obj: l, .. }
        | SlotStore { obj: l, .. } => f(*l),
        IterOpen(_, a, b, _) | IterClose(a, b) => {
            f(*a);
            f(*b);
        }
        IterStep(a, b) => {
            f(*a);
            b.iter().for_each(|&b| f(b));
        }
        ForInGet(a, b, ..) => {
            f(*a);
            f(*b);
        }
        ForInStep(a, b, c) => {
            f(*a);
            f(*b);
            f(*c);
        }
        Closure(_, ls) => ls.iter().for_each(|&l| f(l)),
        Inline { locals, body } | Predicted { locals, body, .. } | ArrayLoop { locals, body, .. } => {
            locals.iter().for_each(|&l| f(l));
            stmt_locals(body, f);
        }
        ElemGuard { elems, nums, bounds, derived, fields, distinct } => {
            bounds.iter().for_each(|&(a, i)| {
                f(a);
                f(i);
            });
            elems.iter().for_each(|&(o, a, i, _)| {
                f(o);
                f(a);
                f(i);
            });
            nums.iter().for_each(|&(to, a, i)| {
                f(to);
                f(a);
                f(i);
            });
            derived.iter().for_each(|&(l, of, _, _)| {
                f(l);
                f(of);
            });
            fields.iter().for_each(|&(l, _)| f(l));
            distinct.iter().for_each(|&l| f(l));
        }
        RegionTypedGet { arr, idx } => {
            f(*arr);
            f(*idx);
        }
        SpecGuard { objs, fields, nums, strs, derived, methods, arrays, appends, typed_writes, .. } => {
            arrays.iter().for_each(|&l| f(l));
            typed_writes.iter().for_each(|&l| f(l));
            appends.iter().for_each(|&l| f(l));
            objs.iter().for_each(|&(l, _)| f(l));
            derived.iter().for_each(|&(l, of, _, _)| {
                f(l);
                f(of);
            });
            methods.iter().for_each(|&(l, ..)| l.iter().for_each(|&l| f(l)));
            fields.iter().for_each(|&(l, _)| f(l));
            nums.iter().for_each(|&(a, b)| {
                f(a);
                f(b);
            });
            strs.iter().for_each(|&l| f(l));
        }
        _ => {}
    }
}

/// Calls `f` on every naming of a local in `stmts` (by statements, or
/// expressions at any depth).
pub(crate) fn each_local(stmts: &[Stmt], f: &mut impl FnMut(LocalId)) {
    stmt_locals(stmts, f);
    visit_stmts(stmts, &mut |e| expr_locals(e, f));
}

/// How many times each of the `n` locals is named in `stmts`.
pub fn local_uses(stmts: &[Stmt], n: usize) -> Vec<usize> {
    let mut uses = vec![0; n];
    each_local(stmts, &mut |l| uses[l] += 1);
    uses
}

/// The locals a loop body declares for each iteration: among those `cand`
/// accepts, the ones a statement of the body itself (or of its blocks)
/// declares (`InitLocal`, as `let` and `const` do) before anything else in
/// the body names them, and that nothing outside the body names (`uses`:
/// the function's counts). Their value is dead once an iteration ends: the
/// next one declares them again.
pub fn iteration_locals(body: &[Stmt], uses: &[usize], cand: impl Fn(LocalId) -> bool) -> Vec<LocalId> {
    fn flat<'a>(stmts: &'a [Stmt], out: &mut Vec<&'a Stmt>) {
        for s in stmts {
            match s {
                Stmt::Block(b) => flat(b, out),
                _ => out.push(s),
            }
        }
    }
    let mut seq = Vec::new();
    flat(body, &mut seq);
    let inside = local_uses(body, uses.len());
    let mut seen = vec![false; uses.len()];
    let mut out = Vec::new();
    for s in seq {
        if let Stmt::Expr(Expr { kind: ExprKind::InitLocal(l, x), .. }) = s {
            let mut own = false;
            visit(x, &mut |e| expr_locals(e, &mut |m| own |= m == *l));
            if !seen[*l] && !own && inside[*l] == uses[*l] && cand(*l) {
                out.push(*l);
            }
        }
        each_local(std::slice::from_ref(s), &mut |l| seen[l] = true);
    }
    out
}

/// Calls `f` on the expressions that are statements.
pub(crate) fn top_exprs(stmts: &[Stmt], f: &mut impl FnMut(&Expr)) {
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
///
/// With escape summaries (`spec`), the object may also be an argument of a
/// call of a function that only reads it: it calls a clone of the
/// function that takes the fields it reads instead (see
/// `escape::Spec`), so that the caller reads them. (An async callee may
/// read them after the call: an object written anywhere is then kept.)
pub fn scalar_replace(f: &mut Func, mut spec: Option<&mut crate::escape::Spec>) {
    use crate::escape::Field;
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
    // Property writes, and whether it is passed to a call that may read it
    // later.
    let mut writes = vec![0; n];
    let mut settles = vec![false; n];
    let awaits = crate::escape::awaited(&f.body);
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
        ExprKind::ForInGet(a, b, ..) => {
            ok[*a] = false;
            ok[*b] = false;
        }
        ExprKind::ForInIterator(_, a) => ok[*a] = false,
        ExprKind::ForInStep(a, b, c) => {
            ok[*a] = false;
            ok[*b] = false;
            ok[*c] = false;
        }
        ExprKind::AsyncIterResult(_, a) | ExprKind::IterCloseQuiet(a) => ok[*a] = false,
        ExprKind::Closure(_, ls) => ls.iter().for_each(|&l| ok[l] = false),
        ExprKind::GetProp(o, k)
        | ExprKind::SetProp(o, k, _)
        | ExprKind::ShapeGet { obj: o, atom: k, .. }
        | ExprKind::ShapeSet { obj: o, atom: k, .. } => {
            if let ExprKind::Local(l) = o.kind {
                as_object[l] += 1;
                used[l].push(*k);
                writes[l] += matches!(e.kind, ExprKind::SetProp(..) | ExprKind::ShapeSet { .. }) as usize;
            }
        }
        ExprKind::Call(k, args)
        | ExprKind::CallEntry { func: k, args, callee: None, .. }
        | ExprKind::CallDirectThis(k, _, args) => {
            let Some(spec) = spec.as_deref() else { return };
            let entry = matches!(e.kind, ExprKind::CallEntry { .. });
            for (q, a) in args.iter().enumerate() {
                let ExprKind::Local(l) = a.kind else { continue };
                let Some((fs, outlives)) = spec.reads(*k, q) else { continue };
                if !fs.iter().all(|fd| matches!(fd, Field::Prop(_))) {
                    continue;
                }
                as_object[l] += 1;
                used[l].extend(fs.iter().map(|fd| match fd {
                    Field::Prop(a) => *a,
                    Field::Slot(_) => unreachable!(),
                }));
                settles[l] |= outlives || (entry && spec.suspends(*k) && !awaits.contains(&(e as *const Expr)));
                // (The fields are read before the arguments after it are
                // evaluated: those must not write them.)
                for later in &args[q + 1..] {
                    visit(later, &mut |x| match &x.kind {
                        ExprKind::SetProp(o, ..) | ExprKind::ShapeSet { obj: o, .. } if matches!(o.kind, ExprKind::Local(m) if m == l) => {
                            ok[l] = false
                        }
                        _ => {}
                    });
                }
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
            && !(settles[l] && (writes[l] > 0 || sets[l] > 0))
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
    // The clones the calls need (else the objects they pass are kept).
    if let Some(spec) = spec.as_deref_mut() {
        loop {
            let mut dropped = false;
            visit_stmts(&f.body, &mut |e| {
                let (ExprKind::Call(k, args)
                | ExprKind::CallEntry { func: k, args, callee: None, .. }
                | ExprKind::CallDirectThis(k, _, args)) = &e.kind
                else {
                    return;
                };
                let objs = replaced_args(spec, *k, args, &|l| fields.get(l).is_some_and(Option::is_some));
                if !objs.is_empty() && spec.clone_of(*k, &objs).is_none() {
                    for a in args {
                        if let ExprKind::Local(l) = a.kind {
                            dropped |= fields[l].take().is_some();
                        }
                    }
                }
            });
            if !dropped {
                break;
            }
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
        if let ExprKind::Call(k, args)
        | ExprKind::CallEntry { func: k, args, callee: None, .. }
        | ExprKind::CallDirectThis(k, _, args) = &mut e.kind
            && let Some(spec) = spec.as_deref_mut()
        {
            let objs = replaced_args(spec, *k, args, &|l| fields.get(l).is_some_and(Option::is_some));
            if objs.is_empty() {
                return;
            }
            let clone = spec.clone_of(*k, &objs).expect("made above");
            let mut out = Vec::new();
            for (q, a) in std::mem::take(args).into_iter().enumerate() {
                match a.kind {
                    ExprKind::Local(l) if objs.iter().any(|(p, _)| *p == q) => {
                        let (fs, _) = spec.reads(*k, q).unwrap();
                        for fd in fs {
                            let Field::Prop(key) = fd else { unreachable!() };
                            out.push(Expr::new(Ty::Any, ExprKind::Local(field(l, *key).unwrap())));
                        }
                    }
                    _ => out.push(a),
                }
            }
            *args = out;
            *k = clone;
            return;
        }
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
            ExprKind::GetProp(o, k) | ExprKind::ShapeGet { obj: o, atom: k, .. } => match o.kind {
                ExprKind::Local(l) if fields[l].is_some() => ExprKind::Local(field(l, *k).unwrap()),
                _ => return,
            },
            ExprKind::SetProp(o, k, v) | ExprKind::ShapeSet { obj: o, atom: k, value: v, .. } => match o.kind {
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

/// The arguments of a call of function `k` that are objects `replaced`
/// accepts and that `k` only reads: (index, the types of the fields it
/// reads), as `Spec::clone_of` takes them.
fn replaced_args(
    spec: &crate::escape::Spec,
    k: FuncId,
    args: &[Expr],
    replaced: &dyn Fn(LocalId) -> bool,
) -> Vec<(usize, Vec<Ty>)> {
    let mut out = Vec::new();
    for (q, a) in args.iter().enumerate() {
        if let ExprKind::Local(l) = a.kind
            && replaced(l)
            && let Some((fs, _)) = spec.reads(k, q)
        {
            out.push((q, vec![Ty::Any; fs.len()]));
        }
    }
    out
}

/// The arguments of call `e` that are object `l` passed to a function
/// that only reads its fields, all properties among `keys`: (index, the
/// keys it reads, in the order the clone takes them). A call that may read
/// them after it returns only if nothing that follows it writes the object
/// or needs it as an object: `later`, the uses of the object from its
/// statement on that `non_reads` counts (its own aside); `awaits`: the
/// values of the `await` statements around.
pub(crate) fn object_args(
    spec: &crate::escape::Spec,
    e: &Expr,
    l: LocalId,
    keys: &[AtomId],
    later: usize,
    awaits: &HashSet<*const Expr>,
) -> Vec<(usize, Vec<AtomId>)> {
    use crate::escape::Field;
    let (ExprKind::Call(k, args)
    | ExprKind::CallEntry { func: k, args, .. }
    | ExprKind::CallDirectThis(k, _, args)
    | ExprKind::CallChecked { func: k, args, .. }) = &e.kind
    else {
        return Vec::new();
    };
    let entry = matches!(e.kind, ExprKind::CallEntry { .. });
    let mut out = Vec::new();
    for (q, a) in args.iter().enumerate() {
        if !matches!(a.kind, ExprKind::Local(m) if m == l) {
            continue;
        }
        let Some((fs, outlives)) = spec.reads(*k, q) else { continue };
        let props: Option<Vec<AtomId>> = fs
            .iter()
            .map(|fd| match fd {
                Field::Prop(a) if keys.contains(a) => Some(*a),
                _ => None,
            })
            .collect();
        let Some(props) = props else { continue };
        let settles = outlives || (entry && spec.suspends(*k) && !awaits.contains(&(e as *const Expr)));
        if settles && later > own_non_reads(e, l) {
            continue;
        }
        out.push((q, props));
    }
    out
}

/// How many uses of the object in local `l` in `stmts` do more than read
/// its properties (or compare it with `null`, or pass it to functions that
/// only read it): writes, and those that need it as an object. Where there
/// are none after a call that may still read fields it was given, they are
/// what the object holds.
fn non_reads(spec: &crate::escape::Spec, stmts: &[Stmt], l: LocalId) -> usize {
    let mut uses = 0;
    each_name(stmts, &mut |m| uses += (m == l) as usize);
    let is_l = |x: &Expr| matches!(x.kind, ExprKind::Local(m) if m == l);
    let mut reads = 0;
    visit_stmts(stmts, &mut |e| match &e.kind {
        _ if nullish_compare(e).is_some_and(|(m, _)| m == l) => reads += 1,
        // (Its declaration.)
        ExprKind::InitLocal(m, _) if *m == l => reads += 1,
        ExprKind::GetProp(o, _) | ExprKind::ShapeGet { obj: o, .. } if is_l(o) => reads += 1,
        // (Not calls that may need the object itself.)
        ExprKind::Call(k, args)
        | ExprKind::CallEntry { func: k, args, callee: None, .. }
        | ExprKind::CallDirectThis(k, _, args) => {
            reads += args.iter().enumerate().filter(|(q, a)| is_l(a) && spec.reads(*k, *q).is_some()).count();
        }
        _ => {}
    });
    uses - reads
}

/// The uses of the object in local `l` by call `e` that `non_reads` counts.
fn own_non_reads(e: &Expr, l: LocalId) -> usize {
    match &e.kind {
        ExprKind::CallChecked { args, .. } | ExprKind::CallEntry { args, callee: Some(_), .. } => {
            args.iter().filter(|a| matches!(a.kind, ExprKind::Local(m) if m == l)).count()
        }
        _ => 0,
    }
}

/// The key of `Spec::clone_of` for `object_args`: the fields are JS values.
pub(crate) fn clone_key(objs: &[(usize, Vec<AtomId>)]) -> Vec<(usize, Vec<Ty>)> {
    objs.iter().map(|(q, ks)| (*q, vec![Ty::Any; ks.len()])).collect()
}

/// The function a direct call calls.
pub(crate) fn callee_of(e: &Expr) -> FuncId {
    match &e.kind {
        ExprKind::Call(k, _)
        | ExprKind::CallEntry { func: k, .. }
        | ExprKind::CallDirectThis(k, ..)
        | ExprKind::CallChecked { func: k, .. } => *k,
        _ => unreachable!("not a direct call"),
    }
}

/// A direct call taken apart (see `take_call`).
pub(crate) struct CallParts {
    pub func: FuncId,
    pub this: Option<Expr>,
    pub args: Vec<Expr>,
    /// `CallEntry`, `CallChecked`: the function object called otherwise.
    pub callee: Option<Expr>,
    /// `CallChecked`: the checks (by argument).
    pub checks: Option<Vec<(usize, ArgCheck)>>,
    /// A `CallEntry`.
    pub entry: bool,
}

/// `e`, a direct call, taken apart (`e` itself is then a husk).
pub(crate) fn take_call(e: &mut Expr) -> CallParts {
    let kind = std::mem::replace(&mut e.kind, ExprKind::Undefined);
    match kind {
        ExprKind::Call(func, args) => CallParts { func, this: None, args, callee: None, checks: None, entry: false },
        ExprKind::CallEntry { func, args, callee, .. } => {
            CallParts { func, this: None, args, callee: callee.map(|c| *c), checks: None, entry: true }
        }
        ExprKind::CallDirectThis(func, this, args) => {
            CallParts { func, this: Some(*this), args, callee: None, checks: None, entry: false }
        }
        ExprKind::CallChecked { func, callee, args, checks, .. } => {
            CallParts { func, this: None, args, callee: Some(*callee), checks: Some(checks), entry: false }
        }
        _ => unreachable!("not a direct call"),
    }
}

impl CallParts {
    /// The call, of `func` with `args`; `slow`: the arguments `callee` takes
    /// instead, if it is called. A `CallChecked`'s checks move with the
    /// arguments: `index` gives each one's new index.
    pub fn call(
        &self,
        func: FuncId,
        args: Vec<Expr>,
        slow: Option<Vec<Expr>>,
        index: &dyn Fn(usize) -> usize,
    ) -> ExprKind {
        match (&self.checks, &self.callee, &self.this) {
            (Some(checks), Some(callee), _) => ExprKind::CallChecked {
                func,
                callee: Box::new(callee.clone()),
                args,
                checks: checks.iter().map(|&(i, c)| (index(i), c)).collect(),
                slow,
            },
            (None, callee, None) if self.entry => {
                ExprKind::CallEntry { func, args, callee: callee.clone().map(Box::new), slow }
            }
            (None, _, Some(this)) => ExprKind::CallDirectThis(func, Box::new(this.clone()), args),
            _ => ExprKind::Call(func, args),
        }
    }
}

/// Whether statement `s` is `InitLocal(l, _)`.
fn is_init_of(s: &Stmt, l: LocalId) -> bool {
    matches!(s, Stmt::Expr(Expr { kind: ExprKind::InitLocal(m, _), .. }) if *m == l)
}

/// Put the statements of the block of the declaration of `l` (`let` or
/// `const`: expressions) in the list that has the block, with the
/// statements after it that use `l`.
fn splice_declaration(stmts: &mut Vec<Stmt>, l: LocalId) -> bool {
    let decl = |s: &Stmt| match s {
        Stmt::Block(b) => b.iter().any(|s| is_init_of(s, l)) && b.iter().all(|s| matches!(s, Stmt::Expr(_))),
        _ => false,
    };
    if let Some(i) = stmts.iter().position(decl) {
        let Stmt::Block(b) = stmts.remove(i) else { unreachable!() };
        stmts.splice(i..i, b);
        return true;
    }
    stmts.iter_mut().any(|s| match s {
        Stmt::If(_, a, b) => splice_declaration(a, l) || splice_declaration(b, l),
        Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => splice_declaration(body, l),
        Stmt::Switch { cases, .. } => cases.iter_mut().any(|(_, b)| splice_declaration(b, l)),
        Stmt::Try { body, catch, finally, .. } => {
            splice_declaration(body, l)
                || catch.as_mut().is_some_and(|(_, c)| splice_declaration(c, l))
                || finally.as_mut().is_some_and(|c| splice_declaration(c, l))
        }
        _ => false,
    })
}

/// `x == null`, `x !== undefined` and the like (`Bool`), where `x` is a
/// local: the local, and whether it is the right operand.
pub(crate) fn nullish_compare(e: &Expr) -> Option<(LocalId, bool)> {
    let ExprKind::AnyCompare(AnyCmpOp::Eq | AnyCmpOp::Ne | AnyCmpOp::StrictEq | AnyCmpOp::StrictNe, a, b) = &e.kind
    else {
        return None;
    };
    let nullish = |x: &Expr| matches!(x.kind, ExprKind::Null | ExprKind::Undefined);
    match (&a.kind, &b.kind) {
        _ if e.ty != Ty::Bool => None,
        (ExprKind::Local(l), _) if nullish(b) => Some((*l, false)),
        (_, ExprKind::Local(l)) if nullish(a) => Some((*l, true)),
        _ => None,
    }
}

/// A value that can be evaluated twice, in either branch of a check.
pub(crate) fn plain_value(x: &Expr) -> bool {
    match &x.kind {
        ExprKind::Local(_) | ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Str(_) => true,
        ExprKind::Null | ExprKind::Undefined => true,
        ExprKind::Box(y) => matches!(y.kind, ExprKind::Local(_) | ExprKind::Num(_) | ExprKind::Bool(_)),
        _ => false,
    }
}

/// Scalar replacement of plain objects that get their properties after
/// they are made (`const o: any = {}; o.a = x; ...`), have no prototype
/// (`Object.create(null)`), or are needed as objects somewhere: what
/// `scalar_replace` leaves. A local initialized with an object literal or
/// `Object.create(null)`, named only by the statements after its
/// declaration in the same block, keeps its properties in locals of their
/// own while it is virtual (a flag local). A key is added by a statement
/// `o.k = v` of that block (the first one with it); the statements after
/// it read and write it. An addition to an object of Object.prototype goes
/// through it: while it has no setter nor read-only property of the key
/// (`PlainAddOk`), the key is the object's own. Anywhere else the object is
/// needed (passed, returned, a key it does not have, an addition in a
/// branch), and where Object.prototype does not allow an addition, it is
/// made from the properties it has ("materialized", once) and that use
/// and every later one work on it. `Object.create(null)` is virtual while
/// the method is the builtin (else its result is used); a key not added
/// reads `undefined`.
pub fn virtualize_objects(f: &mut Func, mut spec: Option<&mut crate::escape::Spec>) {
    let n = f.locals.len();
    let mut ok: Vec<bool> =
        (0..n).map(|l| l >= f.params.len() && !f.boxed[l] && f.locals[l] == Ty::Any && f.ret_slot != Some(l)).collect();
    deep_stmt_locals(&f.body, &mut |l| ok[l] = false);
    let mut named = vec![0usize; n];
    each_name(&f.body, &mut |l| named[l] += 1);
    // Per local: how it is made (`Some(true)`: without a prototype), and
    // its namings as a value or a declaration (what can be replaced).
    let mut made: Vec<Option<bool>> = vec![None; n];
    let mut values = vec![0usize; n];
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::InitLocal(l, x) => {
            let how = match &x.kind {
                ExprKind::Object(_) => Some(false),
                ExprKind::ObjectCreate(_, _, a) if matches!(a.kind, ExprKind::Null) => Some(true),
                _ => None,
            };
            if how.is_none() || made[*l].is_some() {
                ok[*l] = false;
            }
            made[*l] = how;
            values[*l] += 1;
        }
        ExprKind::Local(l) => values[*l] += 1,
        _ => {}
    });
    let debug = std::env::var_os("LUMINOUS_DEBUG_VIRT").is_some();
    for l in 0..n {
        if let Some(no_proto) = made[l]
            && ok[l]
            && named[l] == values[l]
        {
            let done = virtualize_object(f, l, no_proto, spec.as_deref_mut());
            if debug {
                eprintln!("virtualize object {} local {l}: {}", f.name, if done { "yes" } else { "no" });
            }
        }
    }
}

/// How many places may materialize an object (each a copy of the code
/// making it).
const MATERIALIZE_SITES: usize = 3;

/// `stmt_locals` of `stmts` and of the bodies of the inlined, predicted and
/// callback calls in them.
pub(crate) fn deep_stmt_locals(stmts: &[Stmt], f: &mut impl FnMut(LocalId)) {
    stmt_locals(stmts, f);
    visit_stmts(stmts, &mut |e| {
        if let ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } | ExprKind::ArrayLoop { body, .. } =
            &e.kind
        {
            stmt_locals(body, f);
        }
    });
}

/// `each_local`, but for the locals lists of inlined, predicted and
/// callback bodies (which only scope their locals).
pub(crate) fn each_name(stmts: &[Stmt], f: &mut impl FnMut(LocalId)) {
    stmt_locals(stmts, f);
    visit_stmts(stmts, &mut |e| match &e.kind {
        ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } | ExprKind::ArrayLoop { body, .. } => {
            stmt_locals(body, f)
        }
        _ => expr_locals(e, f),
    });
}

/// How many times `each_name` names `l` in `stmts`.
pub(crate) fn names_of(stmts: &[Stmt], l: LocalId) -> usize {
    let mut n = 0;
    each_name(stmts, &mut |m| n += (m == l) as usize);
    n
}

/// The bodies of inlined, predicted and callback calls, with their locals.
pub(crate) fn body_of(e: &mut Expr) -> Option<(&mut Vec<Stmt>, &mut Vec<LocalId>)> {
    match &mut e.kind {
        ExprKind::Inline { body, locals } | ExprKind::Predicted { body, locals, .. } => Some((body, locals)),
        ExprKind::ArrayLoop { body, locals, .. } => Some((body, locals)),
        _ => None,
    }
}

/// Whether `pred` accepts `stmts` or a statement list nested in them (also
/// in the bodies of inlined, predicted and callback calls).
fn has_list(stmts: &[Stmt], pred: &dyn Fn(&[Stmt]) -> bool) -> bool {
    let mut found = false;
    stmt_lists(stmts, &mut |list| found |= pred(list));
    if !found {
        visit_stmts(stmts, &mut |e| {
            if let ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } | ExprKind::ArrayLoop { body, .. } =
                &e.kind
            {
                found = found || has_list(body, pred);
            }
        });
    }
    found
}

/// The expressions of statement `s` itself (not of the statements in it).
fn own_exprs_mut(s: &mut Stmt) -> Vec<&mut Expr> {
    match s {
        Stmt::Expr(x) | Stmt::Throw(x) | Stmt::If(x, ..) => vec![x],
        Stmt::Await { value, .. } | Stmt::Yield { value, .. } => vec![value],
        Stmt::Return(x) => x.iter_mut().collect(),
        Stmt::Loop { test, update, .. } => test.iter_mut().chain(update.iter_mut()).collect(),
        Stmt::Switch { cases, .. } => cases.iter_mut().filter_map(|(c, _)| c.as_mut()).collect(),
        _ => Vec::new(),
    }
}

/// The statement lists of statement `s` itself.
fn own_lists_mut(s: &mut Stmt) -> Vec<&mut Vec<Stmt>> {
    match s {
        Stmt::If(_, a, b) => vec![a, b],
        Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => vec![body],
        Stmt::Switch { cases, .. } => cases.iter_mut().map(|(_, b)| b).collect(),
        Stmt::Try { body, catch, finally, .. } => {
            let mut v = vec![body];
            v.extend(catch.as_mut().map(|(_, c)| c));
            v.extend(finally.as_mut());
            v
        }
        _ => Vec::new(),
    }
}

/// Calls `k` on the first statement list that `pred` accepts, among
/// `stmts` and the lists nested in them (also in the bodies of inlined,
/// predicted and callback calls), with the locals list of the body it is
/// in (`scope`; `None` at the function's own level).
pub(crate) fn find_list_mut<R>(
    stmts: &mut Vec<Stmt>,
    scope: Option<&mut Vec<LocalId>>,
    pred: &dyn Fn(&[Stmt]) -> bool,
    k: &mut dyn FnMut(&mut Vec<Stmt>, Option<&mut Vec<LocalId>>) -> R,
) -> Option<R> {
    if pred(stmts) {
        return Some(k(stmts, scope));
    }
    let mut scope = scope;
    for s in stmts.iter_mut() {
        if !has_list(std::slice::from_ref(s), pred) {
            continue;
        }
        for list in own_lists_mut(s) {
            if let Some(r) = find_list_mut(list, scope.as_deref_mut(), pred, k) {
                return Some(r);
            }
        }
        for e in own_exprs_mut(s) {
            let mut out = None;
            visit_mut(e, &mut |x| {
                if out.is_some() {
                    return;
                }
                if let Some((body, locals)) = body_of(x)
                    && has_list(body, pred)
                {
                    out = find_list_mut(body, Some(locals), pred, k);
                }
            });
            if out.is_some() {
                return out;
            }
        }
    }
    None
}

/// `virtualize_objects` for local `l` (made by `Object.create(null)` if
/// `no_proto`), if its uses are where it allows them and some are virtual.
/// (Its declaration may be in an inlined body: what it adds is scoped
/// there.)
fn virtualize_object(f: &mut Func, l: LocalId, no_proto: bool, mut spec: Option<&mut crate::escape::Spec>) -> bool {
    let named = names_of(&f.body, l);
    let base = f.locals.len();
    // A `let` / `const` block of the declaration is spliced into its list.
    let decl_block = |list: &[Stmt]| {
        list.iter().any(|s| match s {
            Stmt::Block(b) => b.iter().any(|s| is_init_of(s, l)) && b.iter().all(|s| matches!(s, Stmt::Expr(_))),
            _ => false,
        })
    };
    // (Again while that block is one of expressions itself: an inlined
    // body's declaration, in the statements a flattened call left.)
    while find_list_mut(&mut f.body, None, &decl_block, &mut |list, _| splice_declaration(list, l)) == Some(true) {}
    let has_decl = |list: &[Stmt]| list.iter().any(|s| is_init_of(s, l));
    let added = find_list_mut(&mut f.body, None, &has_decl, &mut |list, scope| {
        let added = virtualize_in(list, l, no_proto, named, base, spec.as_deref_mut())?;
        if let Some(scope) = scope {
            scope.extend(base..base + added.len());
        }
        Some(added)
    })
    .flatten();
    let Some(added) = added else { return false };
    f.boxed.extend(std::iter::repeat_n(false, added.len()));
    f.local_names.extend(std::iter::repeat_n(None, added.len()));
    f.locals.extend(added);
    // (The flag is local `base`.)
    crate::escape::fold_flag(f, base);
    true
}

/// `virtualize_object` in `list`, which has the declaration of `l` (named
/// `named` times in the function); new locals from `base`. Their types, if
/// it is done.
fn virtualize_in(
    list: &mut [Stmt],
    l: LocalId,
    no_proto: bool,
    named: usize,
    base: LocalId,
    mut spec: Option<&mut crate::escape::Spec>,
) -> Option<Vec<Ty>> {
    let i0 = list.iter().position(|s| is_init_of(s, l)).unwrap();
    // Every use after the declaration, in the same block.
    let mut later = 0;
    each_name(&list[i0 + 1..], &mut |m| later += (m == l) as usize);
    if later + 1 != named {
        return None;
    }
    let is_l = |x: &Expr| matches!(x.kind, ExprKind::Local(m) if m == l);
    let awaits = crate::escape::awaited(list);
    // Per statement after the declaration: the uses from it on that need
    // more than reads (see `object_args`).
    let mut later: Vec<usize> = match spec.as_deref() {
        Some(spec) => list[i0 + 1..].iter().map(|s| non_reads(spec, std::slice::from_ref(s), l)).collect(),
        None => vec![usize::MAX; list.len() - i0 - 1],
    };
    for j in (0..later.len().saturating_sub(1)).rev() {
        later[j] = later[j].saturating_add(later[j + 1]);
    }
    // The keys: the literal's, then those added, in order.
    let mut keys: Vec<AtomId> = Vec::new();
    if let Stmt::Expr(Expr { kind: ExprKind::InitLocal(_, x), .. }) = &list[i0]
        && let ExprKind::Object(props) = &x.kind
    {
        for (k, _) in props {
            if !keys.contains(k) {
                keys.push(*k);
            }
        }
    }
    // An addition: statement `l.k = v` of a key it does not have yet.
    let addition = |s: &Stmt, keys: &[AtomId]| match s {
        Stmt::Expr(Expr { kind: ExprKind::SetProp(o, k, _), .. }) if is_l(o) && !keys.contains(k) => Some(*k),
        _ => None,
    };
    let k0 = keys.len();
    // Per statement after the declaration: the key it adds, if any. Uses
    // that keep the object virtual, and the others (where it is made).
    let mut adds: Vec<Option<AtomId>> = Vec::new();
    let (mut virtual_uses, mut others) = (0, 0);
    for (j, s) in list[i0 + 1..].iter().enumerate() {
        let add = addition(s, &keys);
        let mut uses = 0;
        each_name(std::slice::from_ref(s), &mut |m| uses += (m == l) as usize);
        let mut kept = add.is_some() as usize;
        // (The statement's own expression comes first.)
        let mut top = add.is_some();
        visit_stmts(std::slice::from_ref(s), &mut |e| {
            if nullish_compare(e).is_some_and(|(m, _)| m == l) {
                kept += 1;
                return;
            }
            // Passed to a function that only reads it: the fields.
            if let Some(spec) = spec.as_deref() {
                kept += object_args(spec, e, l, &keys, later[j], &awaits).len();
            }
            let (ExprKind::GetProp(o, k)
            | ExprKind::SetProp(o, k, _)
            | ExprKind::ShapeGet { obj: o, atom: k, .. }
            | ExprKind::ShapeSet { obj: o, atom: k, .. }) = &e.kind
            else {
                return;
            };
            if !is_l(o) || std::mem::take(&mut top) {
                return;
            }
            // A key it has; or, without a prototype, one it does not (read).
            if keys.contains(k) || (no_proto && matches!(e.kind, ExprKind::GetProp(..) | ExprKind::ShapeGet { .. })) {
                kept += 1;
            }
        });
        virtual_uses += kept;
        others += uses - kept;
        keys.extend(add);
        adds.push(add);
    }
    if virtual_uses == 0 || others > MATERIALIZE_SITES {
        return None;
    }
    // The flag, the properties, the object itself while being rewritten
    // (renamed `l` at the end); then temporaries.
    let flag = base;
    let slots: Vec<LocalId> = (base + 1..base + 1 + keys.len()).collect();
    let ph = base + 1 + keys.len();
    let slot = |k: AtomId| slots[keys.iter().position(|&x| x == k).unwrap()];
    let local = |x: LocalId| Expr::new(Ty::Any, ExprKind::Local(x));
    let flag_e = || Expr::new(Ty::Bool, ExprKind::Local(flag));
    let bool_ = |b: bool| Expr::new(Ty::Bool, ExprKind::Bool(b));
    let any = |kind: ExprKind| Expr::new(Ty::Any, kind);
    let cond = |ty: Ty, c: Expr, a: Expr, b: Expr| Expr::new(ty, ExprKind::Cond(Box::new(c), Box::new(a), Box::new(b)));
    let temps = std::cell::Cell::new(0);
    let temp = || {
        temps.set(temps.get() + 1);
        ph + temps.get()
    };
    // `v` once, as a value usable in both branches of a check: itself, or
    // a temporary it is stored in first (`Seq[init, ...]`; evaluated before
    // the check, also if it makes the object).
    let once = |v: Expr| -> (Option<Expr>, Expr) {
        let mut names = false;
        visit(&v, &mut |x| names |= is_l(x));
        if plain_value(&v) && !names {
            return (None, v);
        }
        let t = temp();
        (Some(any(ExprKind::InitLocal(t, Box::new(v)))), local(t))
    };
    let seq = |first: Option<Expr>, then: Expr| match first {
        Some(x) => Expr::new(then.ty, ExprKind::Seq(vec![x, then])),
        None => then,
    };
    // The declaration.
    let Stmt::Expr(init) = &mut list[i0] else { unreachable!() };
    let ExprKind::InitLocal(_, x) = std::mem::replace(&mut init.kind, ExprKind::Undefined) else { unreachable!() };
    let mut parts = Vec::new();
    match x.kind {
        ExprKind::ObjectCreate(o, create, arg) => {
            // The method read (from `Object`, once), then called unless it is
            // the builtin.
            let (to, tf) = (temp(), temp());
            parts.push(any(ExprKind::InitLocal(to, o)));
            parts.push(any(ExprKind::InitLocal(tf, Box::new(any(ExprKind::GetProp(Box::new(local(to)), create))))));
            let is = Expr::new(Ty::Bool, ExprKind::IsObjectCreate(Box::new(local(tf))));
            parts.push(Expr::new(Ty::Bool, ExprKind::InitLocal(flag, Box::new(is))));
            let call = any(ExprKind::CallThis(Box::new(local(tf)), Box::new(local(to)), vec![*arg]));
            parts.push(any(ExprKind::InitLocal(ph, Box::new(cond(Ty::Any, flag_e(), any(ExprKind::Undefined), call)))));
        }
        ExprKind::Object(props) => {
            parts.push(Expr::new(Ty::Bool, ExprKind::InitLocal(flag, Box::new(bool_(true)))));
            for (k, v) in props {
                parts.push(any(ExprKind::InitLocal(slot(k), Box::new(v))));
            }
            parts.push(any(ExprKind::InitLocal(ph, Box::new(any(ExprKind::Undefined)))));
        }
        _ => unreachable!(),
    }
    // (Assigned before they are read, by their additions: a number to
    // begin with, so that those of numbers can be numbers.)
    for &s in &slots[k0..] {
        parts.push(any(ExprKind::InitLocal(s, Box::new(any(ExprKind::Box(Box::new(Expr::num(0.0))))))));
    }
    parts.push(any(ExprKind::Undefined));
    *init = any(ExprKind::Seq(parts));
    // The object, made from the properties it has while it is virtual (an
    // object literal's: defined; one without a prototype: set, as no
    // setter can run).
    let materialize = |have: &[AtomId]| {
        let mut make = Vec::new();
        if no_proto {
            make.push(any(ExprKind::SetLocal(ph, Box::new(any(ExprKind::NullObject)))));
            for &k in have {
                make.push(any(ExprKind::SetProp(Box::new(local(ph)), k, Box::new(local(slot(k))))));
            }
        } else {
            let lit = any(ExprKind::Object(have.iter().map(|&k| (k, local(slot(k)))).collect()));
            make.push(any(ExprKind::SetLocal(ph, Box::new(lit))));
        }
        make.push(Expr::new(Ty::Bool, ExprKind::SetLocal(flag, Box::new(bool_(false)))));
        make.push(local(ph));
        cond(Ty::Any, flag_e(), any(ExprKind::Seq(make)), local(ph))
    };
    let mut have: Vec<AtomId> = keys[..k0].to_vec();
    for (j, (s, add)) in list[i0 + 1..].iter_mut().zip(adds).enumerate() {
        // An addition: the value, then the key (while Object.prototype lets
        // it be the object's own), else the object made and the key added.
        if let Some(k) = add {
            let Stmt::Expr(e) = s else { unreachable!() };
            let ExprKind::SetProp(_, _, v) = std::mem::replace(&mut e.kind, ExprKind::Undefined) else {
                unreachable!()
            };
            let (first, v) = once(*v);
            let ok = if no_proto {
                flag_e()
            } else {
                cond(Ty::Bool, flag_e(), Expr::new(Ty::Bool, ExprKind::PlainAddOk(k)), bool_(false))
            };
            let real = any(ExprKind::SetProp(Box::new(materialize(&have)), k, Box::new(v.clone())));
            let set = cond(Ty::Any, ok, any(ExprKind::SetLocal(slot(k), Box::new(v))), real);
            *e = seq(first, set);
        }
        visit_stmts_mut(std::slice::from_mut(s), &mut |e| {
            // A call of a function that only reads it: of its clone with the
            // fields while it is virtual, else with the object. (The other
            // operands first, once.)
            if let Some(spec) = spec.as_deref_mut()
                && let objs = object_args(spec, e, l, &have, later[j], &awaits)
                && !objs.is_empty()
                && let Some(clone) = spec.clone_of(callee_of(e), &clone_key(&objs))
            {
                let ty = e.ty;
                let mut parts = take_call(e);
                let mut first = Vec::new();
                if let Some(t) = parts.this.take() {
                    let (f0, v) = once(t);
                    first.extend(f0);
                    parts.this = Some(v);
                }
                let (mut fast, mut slow, mut index) = (Vec::new(), Vec::new(), Vec::new());
                for (q, a) in std::mem::take(&mut parts.args).into_iter().enumerate() {
                    index.push(fast.len());
                    match objs.iter().find(|(p, _)| *p == q) {
                        Some((_, ks)) => {
                            fast.extend(ks.iter().map(|&key| local(slot(key))));
                            slow.push(materialize(&have));
                        }
                        None => {
                            let (f0, v) = once(a);
                            first.extend(f0);
                            fast.push(v.clone());
                            slow.push(v);
                        }
                    }
                }
                // (Where the clone's call may go to the function object instead, the
                // object is made for it.)
                let made = parts.callee.is_some().then(|| slow.clone());
                let fast = Expr::new(ty, parts.call(clone, fast, made, &|i| index[i]));
                let slow = Expr::new(ty, parts.call(parts.func, slow, None, &|i| i));
                let c = cond(ty, flag_e(), fast, slow);
                *e = if first.is_empty() {
                    c
                } else {
                    first.push(c);
                    Expr::new(ty, ExprKind::Seq(first))
                };
                return;
            }
            if let Some((_, right)) = nullish_compare(e) {
                let ExprKind::AnyCompare(op, a, b) = &mut e.kind else { unreachable!() };
                let ne = matches!(op, AnyCmpOp::Ne | AnyCmpOp::StrictNe);
                let x = if right { b } else { a };
                if is_l(x) {
                    **x = local(ph);
                    let real = std::mem::replace(e, bool_(false));
                    *e = cond(Ty::Bool, flag_e(), bool_(ne), real);
                }
                return;
            }
            let ty = e.ty;
            match &mut e.kind {
                ExprKind::GetProp(o, k) | ExprKind::ShapeGet { obj: o, atom: k, .. }
                    if is_l(o) && (have.contains(k) || no_proto) =>
                {
                    let fast = if have.contains(k) { local(slot(*k)) } else { any(ExprKind::Undefined) };
                    **o = local(ph);
                    let real = std::mem::replace(e, any(ExprKind::Undefined));
                    *e = cond(ty, flag_e(), fast, real);
                }
                ExprKind::SetProp(o, k, v) | ExprKind::ShapeSet { obj: o, atom: k, value: v, .. }
                    if is_l(o) && have.contains(k) =>
                {
                    let k = *k;
                    let v = std::mem::replace(&mut **v, any(ExprKind::Undefined));
                    let (first, v) = once(v);
                    // (The store, on the object.)
                    let mut real = std::mem::replace(e, any(ExprKind::Undefined));
                    let (ExprKind::SetProp(o, _, rv) | ExprKind::ShapeSet { obj: o, value: rv, .. }) = &mut real.kind
                    else {
                        unreachable!()
                    };
                    **o = local(ph);
                    **rv = v.clone();
                    let set = cond(ty, flag_e(), any(ExprKind::SetLocal(slot(k), Box::new(v))), real);
                    *e = seq(first, set);
                }
                // The object itself.
                ExprKind::Local(m) if *m == l => *e = materialize(&have),
                _ => {}
            }
        });
        have.extend(add);
    }
    // The object local is `l` again.
    visit_stmts_mut(&mut list[i0..], &mut |e| match &mut e.kind {
        ExprKind::Local(m) | ExprKind::SetLocal(m, _) | ExprKind::InitLocal(m, _) if *m == ph => *m = l,
        _ => {}
    });
    let added = keys.len() + 2 + temps.get();
    let mut types = vec![Ty::Bool];
    types.extend(std::iter::repeat_n(Ty::Any, added - 1));
    Some(types)
}

/// A number written to an `Any` local `l` that `retype_numbers` can turn
/// into an `F64` one: a boxed number, `l` plus or minus one, or `+ - * /`
/// on `l` and boxed numbers.
fn number_write(l: LocalId, v: &Expr) -> bool {
    fn operand(l: LocalId, x: &Expr) -> bool {
        match &x.kind {
            ExprKind::Local(m) => *m == l,
            ExprKind::Box(e) => e.ty.is_number(),
            // (`c ? 1 : x`)
            ExprKind::Cond(_, a, b) => operand(l, a) && operand(l, b),
            _ => false,
        }
    }
    let operand = |x: &Expr| operand(l, x);
    match &v.kind {
        ExprKind::Box(e) => e.ty.is_number(),
        ExprKind::Cond(..) => operand(v),
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
        ExprKind::Cond(c, a, b) => {
            let (a, b) = (number_operand(l, *a), number_operand(l, *b));
            // (Both as doubles: an int32 arm is widened where it is used.)
            let ty = if a.ty == b.ty { a.ty } else { Ty::F64 };
            Expr::new(ty, ExprKind::Cond(c, Box::new(a), Box::new(b)))
        }
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
            // A number and a boxed number.
            if let ExprKind::NumArith(op, x, y) = &mut e.kind {
                for z in [&mut *x, &mut *y] {
                    if let ExprKind::Box(inner) = &mut z.kind
                        && inner.ty.is_number()
                    {
                        let n = std::mem::replace(&mut **inner, Expr::num(0.0));
                        **z = n;
                    }
                }
                if x.ty.is_number() && y.ty.is_number() {
                    let op = *op;
                    let (a, b) =
                        (std::mem::replace(&mut **x, Expr::num(0.0)), std::mem::replace(&mut **y, Expr::num(0.0)));
                    e.kind = ExprKind::Binary(op, Box::new(a), Box::new(b));
                    again = true;
                }
                return;
            }
            // The int32 of a boxed int32: that int32.
            if let ExprKind::IntOf(x) = &mut e.kind
                && let ExprKind::Box(inner) = &mut x.kind
                && inner.ty == Ty::I32
            {
                let n = std::mem::replace(&mut **inner, Expr::num(0.0));
                *e = n;
                again = true;
                return;
            }
            // A math function of a boxed number: of the number.
            if let ExprKind::Math(_, args) = &mut e.kind {
                for a in args.iter_mut() {
                    if let ExprKind::Box(inner) = &mut a.kind
                        && inner.ty.is_number()
                    {
                        let n = std::mem::replace(&mut **inner, Expr::num(0.0));
                        *a = n;
                        again = true;
                    }
                }
                return;
            }
            // A comparison of two boxed numbers: a number comparison (the
            // same result, NaN included; equality of numbers is strict).
            if let ExprKind::AnyCompare(op, x, y) = &mut e.kind
                && let (ExprKind::Box(a), ExprKind::Box(b)) = (&x.kind, &y.kind)
                && a.ty.is_number()
                && b.ty.is_number()
                && e.ty == Ty::Bool
            {
                let cmp = match op {
                    AnyCmpOp::Lt => CmpOp::Lt,
                    AnyCmpOp::Le => CmpOp::Le,
                    AnyCmpOp::Gt => CmpOp::Gt,
                    AnyCmpOp::Ge => CmpOp::Ge,
                    AnyCmpOp::Eq | AnyCmpOp::StrictEq => CmpOp::Eq,
                    AnyCmpOp::Ne | AnyCmpOp::StrictNe => CmpOp::Ne,
                    AnyCmpOp::In | AnyCmpOp::InstanceOf => return,
                };
                let take = |x: &mut Box<Expr>| match std::mem::replace(&mut x.kind, ExprKind::Undefined) {
                    ExprKind::Box(inner) => *inner,
                    _ => unreachable!(),
                };
                let (a, b) = (take(x), take(y));
                e.kind = ExprKind::Compare(cmp, Box::new(a), Box::new(b));
                again = true;
                return;
            }
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
        ExprKind::ForInGet(a, b, ..) => {
            special[*a] = true;
            special[*b] = true;
        }
        ExprKind::ForInStep(a, b, c) => {
            special[*a] = true;
            special[*b] = true;
            special[*c] = true;
        }
        ExprKind::ForInIterator(_, a)
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
        | ExprKind::ArrayPush(m, _)
        | ExprKind::AsyncIterResult(_, m)
        | ExprKind::ForInIterator(_, m)
        | ExprKind::IterCloseQuiet(m) => (0, (*m == l) as usize),
        ExprKind::IterOpen(_, m, n, _) | ExprKind::IterClose(m, n) | ExprKind::ForInGet(m, n, ..) => {
            (0, (*m == l || *n == l) as usize)
        }
        ExprKind::IterStep(m, n) => (0, (*m == l || *n == Some(l)) as usize),
        ExprKind::ForInStep(m, n, o) => ((*o == l) as usize, (*m == l || *n == l) as usize),
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
pub(crate) fn stmt_lists<'a>(stmts: &'a [Stmt], f: &mut impl FnMut(&'a [Stmt])) {
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
pub(crate) fn stmt_lists_mut(stmts: &mut Vec<Stmt>, f: &mut impl FnMut(&mut Vec<Stmt>)) {
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
/// the closure never assigns it, nor do the closures it passes it to. (A
/// closure that suspends keeps the value in its frame: see `enter_async`.)
fn value_capture_ok(m: &Module, k: ClosureId, j: usize) -> bool {
    let f = &m.closures[k];
    let c = f.captures[j];
    stmts_refs(&f.body, c).1 == 0 && capturing(&f.body, c).into_iter().all(|(k2, j2)| value_capture_ok(m, k2, j2))
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
/// statements after it (in the same list), or a parameter never assigned
/// (set before any closure is made; no `arguments` object of the function
/// is its alias), and captured only by closures that never assign it, is
/// no longer boxed, and its closures hold its value in their data slots
/// instead of the box.
pub fn capture_values(m: &mut Module) {
    let nfuncs = m.funcs.len();
    for g in 0..nfuncs + m.closures.len() {
        let f = if g < nfuncs { &m.funcs[g] } else { &m.closures[g - nfuncs] };
        let mut found = Vec::new();
        for l in 0..f.locals.len() {
            if !f.boxed[l] || f.captures.contains(&l) {
                continue;
            }
            if l < f.params.len() {
                if f.arguments || stmts_refs(&f.body, l).1 != 0 {
                    continue;
                }
                let caps = capturing(&f.body, l);
                if !caps.is_empty() && caps.iter().all(|&(k, j)| value_capture_ok(m, k, j)) {
                    found.push((l, caps));
                }
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
    if f.is_async || f.is_generator || other || f.rest || f.arguments || f.ret_slot.is_some() {
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
        | ExprKind::ArrayPush(l, _)
        | ExprKind::AsyncIterResult(_, l)
        | ExprKind::ForInIterator(_, l)
        | ExprKind::IterCloseQuiet(l) => *l += base,
        ExprKind::IterOpen(_, l, m, _) | ExprKind::IterClose(l, m) | ExprKind::ForInGet(l, m, ..) => {
            *l += base;
            *m += base;
        }
        ExprKind::ForInStep(l, m, n) => {
            *l += base;
            *m += base;
            *n += base;
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
pub(crate) fn remap_locals(e: &mut Expr, map: &impl Fn(LocalId) -> LocalId) {
    let m = |l: &mut LocalId| *l = map(*l);
    match &mut e.kind {
        ExprKind::Local(l)
        | ExprKind::SetLocal(l, _)
        | ExprKind::InitLocal(l, _)
        | ExprKind::Update { local: l, .. }
        | ExprKind::ArrayPush(l, _)
        | ExprKind::AsyncIterResult(_, l)
        | ExprKind::ForInIterator(_, l)
        | ExprKind::IterCloseQuiet(l) => m(l),
        ExprKind::IterOpen(_, l, n, _) | ExprKind::IterClose(l, n) | ExprKind::ForInGet(l, n, ..) => {
            m(l);
            m(n);
        }
        ExprKind::ForInStep(l, n, o) => {
            m(l);
            m(n);
            m(o);
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

/// Calls `f` on the locals that `e` itself writes (or names otherwise than
/// by reading them: what `local_refs` counts as other references).
pub(crate) fn written_locals(e: &Expr, f: &mut impl FnMut(LocalId)) {
    match &e.kind {
        ExprKind::SetLocal(m, _)
        | ExprKind::InitLocal(m, _)
        | ExprKind::Update { local: m, .. }
        | ExprKind::ArrayPush(m, _)
        | ExprKind::AsyncIterResult(_, m)
        | ExprKind::ForInIterator(_, m)
        | ExprKind::IterCloseQuiet(m) => f(*m),
        ExprKind::IterOpen(_, m, n, _) | ExprKind::IterClose(m, n) | ExprKind::ForInGet(m, n, ..) => {
            f(*m);
            f(*n);
        }
        ExprKind::IterStep(m, n) => {
            f(*m);
            n.iter().for_each(|&n| f(n));
        }
        ExprKind::ForInStep(m, n, _) => {
            f(*m);
            f(*n);
        }
        _ => {}
    }
}

/// An inlined call binds the arguments to the callee's parameters
/// (`InitLocal(p, x)`, the first statements of its body). Where an argument
/// is a local `a` of the caller that nothing in the inlined call assigns,
/// and the parameter is assigned nowhere else, the body reads `a` itself:
/// an object passed to an inlined function is then only used for its
/// properties there (`a.key`), which scalar replacement needs.
pub fn forward_inline_args(f: &mut Func) {
    let n = f.locals.len();
    let mut writes = vec![0usize; n];
    deep_stmt_locals(&f.body, &mut |l| writes[l] += 1);
    visit_stmts(&f.body, &mut |e| written_locals(e, &mut |l| writes[l] += 1));
    let boxed = f.boxed.clone();
    let types = f.locals.clone();
    // (Only parameters named by reads and their binding.)
    let mut names = vec![0usize; n];
    each_name(&f.body, &mut |l| names[l] += 1);
    let mut plain = vec![0usize; n];
    visit_stmts(&f.body, &mut |e| match e.kind {
        ExprKind::Local(l) => plain[l] += 1,
        _ => written_locals(e, &mut |l| plain[l] += 1),
    });
    visit_stmts_mut(&mut f.body, &mut |e| {
        let ExprKind::Inline { body, locals } = &mut e.kind else { return };
        let mut map: Vec<(LocalId, LocalId)> = Vec::new();
        for s in body.iter() {
            let Stmt::Expr(Expr { kind: ExprKind::InitLocal(p, x), .. }) = s else { break };
            let ExprKind::Local(a) = x.kind else { continue };
            if locals.contains(p)
                && writes[*p] == 1
                && names[*p] == plain[*p]
                && !boxed[*p]
                && !boxed[a]
                && types[*p] == types[a]
                && x.ty == types[a]
            {
                map.push((*p, a));
            }
        }
        if map.is_empty() {
            return;
        }
        let mut assigned = HashSet::new();
        deep_stmt_locals(body, &mut |l| {
            assigned.insert(l);
        });
        visit_stmts(body, &mut |x| {
            written_locals(x, &mut |l| {
                assigned.insert(l);
            })
        });
        map.retain(|(_, a)| !assigned.contains(a));
        if map.is_empty() {
            return;
        }
        body.retain(|s| !matches!(s, Stmt::Expr(Expr { kind: ExprKind::InitLocal(p, _), .. }) if map.iter().any(|(q, _)| q == p)));
        visit_stmts_mut(body, &mut |x| {
            if let ExprKind::Local(q) = &mut x.kind
                && let Some(&(_, a)) = map.iter().find(|(p, _)| p == q)
            {
                *q = a;
            }
        });
    });
}

/// A local declared as a copy of another (`let q = o`: what a flattened
/// inlined call that returns a local leaves), where neither is ever
/// assigned otherwise, is that other local: its uses name it instead.
pub fn forward_copies(f: &mut Func) {
    let n = f.locals.len();
    let mut writes = vec![0usize; n];
    deep_stmt_locals(&f.body, &mut |l| writes[l] += 1);
    visit_stmts(&f.body, &mut |e| written_locals(e, &mut |l| writes[l] += 1));
    // Locals named otherwise than read (`Local`) or written are kept (a
    // closure capturing one).
    let mut names = vec![0usize; n];
    each_name(&f.body, &mut |l| names[l] += 1);
    let mut plain = vec![0usize; n];
    visit_stmts(&f.body, &mut |e| match e.kind {
        ExprKind::Local(l) => plain[l] += 1,
        _ => written_locals(e, &mut |l| plain[l] += 1),
    });
    let mut map: Vec<Option<LocalId>> = vec![None; n];
    let mut found = false;
    // The locals a statement declares, itself or in blocks it is (which
    // run once, in order).
    fn declares(s: &Stmt, out: &mut Vec<LocalId>) {
        match s {
            Stmt::Expr(Expr { kind: ExprKind::InitLocal(l, _), .. }) => out.push(*l),
            Stmt::Block(b) => b.iter().for_each(|s| declares(s, out)),
            _ => {}
        }
    }
    lists_deep_mut(&mut f.body, &mut |list| {
        // (The other local is declared before, in the same list: the copy
        // runs again only after it does.)
        let mut before: Vec<LocalId> = Vec::new();
        list.retain(|s| {
            declares(s, &mut before);
            let Stmt::Expr(Expr { kind: ExprKind::InitLocal(q, x), .. }) = s else { return true };
            let ExprKind::Local(o) = x.kind else { return true };
            let ok = *q >= f.params.len()
                && (o < f.params.len() || before.contains(&o))
                && names[*q] == plain[*q]
                && names[o] == plain[o]
                && writes[*q] == 1
                // (A parameter has no declaration of its own.)
                && writes[o] == (o >= f.params.len()) as usize
                && !f.boxed[*q]
                && !f.boxed[o]
                && f.locals[*q] == f.locals[o]
                && x.ty == f.locals[o]
                && f.ret_slot != Some(*q)
                && map[o].is_none();
            if ok {
                map[*q] = Some(o);
                found = true;
            }
            !ok
        });
    });
    if !found {
        return;
    }
    // (A copy of a copy names the first.)
    let root = |mut l: LocalId| {
        while let Some(o) = map[l] {
            l = o;
        }
        l
    };
    visit_stmts_mut(&mut f.body, &mut |e| {
        if let ExprKind::Local(q) = &mut e.kind {
            *q = root(*q);
        }
    });
}

/// Calls `f` on every statement list of `stmts`, also in the bodies of
/// inlined, predicted and callback calls (before the lists in them).
pub(crate) fn lists_deep_mut(stmts: &mut Vec<Stmt>, f: &mut dyn FnMut(&mut Vec<Stmt>)) {
    stmt_lists_mut(stmts, &mut |l| f(l));
    visit_stmts_mut(stmts, &mut |e| {
        if let Some((body, _)) = body_of(e) {
            stmt_lists_mut(body, &mut |l| f(l));
        }
    });
}

/// `s` without its inlined call: when `s` initializes, assigns, returns,
/// awaits or discards the value of an inlined call whose body returns only
/// at its end, the body's statements, then `s` with the value returned.
fn flatten_stmt(s: &mut Stmt) -> Option<Vec<Stmt>> {
    let slot: &mut Expr = match s {
        Stmt::Expr(e) => match &mut e.kind {
            ExprKind::InitLocal(_, x) | ExprKind::SetLocal(_, x) | ExprKind::Void(x) => x,
            ExprKind::Inline { .. } => e,
            _ => return None,
        },
        Stmt::Return(Some(x)) | Stmt::Await { value: x, .. } => x,
        _ => return None,
    };
    let ExprKind::Inline { body, .. } = &mut slot.kind else { return None };
    let Some(Stmt::Return(Some(_))) = body.last() else { return None };
    let mut early = false;
    stmt_lists(&body[..body.len() - 1], &mut |l| early |= l.iter().any(|s| matches!(s, Stmt::Return(_))));
    if early {
        return None;
    }
    let mut stmts = std::mem::take(body);
    let Some(Stmt::Return(Some(x))) = stmts.pop() else { unreachable!() };
    *slot = x;
    let rest = std::mem::replace(s, Stmt::Block(Vec::new()));
    stmts.push(rest);
    Some(stmts)
}

/// Inlined calls whose values statements use directly (see `flatten_stmt`)
/// become their bodies' statements in the caller: an object an inlined
/// call returns is then made by a statement of the caller, as scalar
/// replacement needs. Elsewhere, a body of expressions and a `return`
/// becomes a sequence of them (whose value number arithmetic can use).
pub fn flatten_inlines(f: &mut Func) {
    visit_stmts_mut(&mut f.body, &mut |e| {
        let ExprKind::Inline { body, .. } = &mut e.kind else { return };
        let Some((Stmt::Return(Some(_)), rest)) = body.split_last() else { return };
        if !rest.iter().all(|s| matches!(s, Stmt::Expr(_))) {
            return;
        }
        let Some(Stmt::Return(Some(x))) = body.pop() else { unreachable!() };
        let mut xs: Vec<Expr> = std::mem::take(body)
            .into_iter()
            .map(|s| match s {
                Stmt::Expr(x) => x,
                _ => unreachable!(),
            })
            .collect();
        let ty = x.ty;
        if xs.is_empty() {
            *e = x;
        } else {
            xs.push(x);
            *e = Expr::new(ty, ExprKind::Seq(xs));
        }
    });
    loop {
        let mut changed = false;
        lists_deep_mut(&mut f.body, &mut |list| {
            let mut i = 0;
            while i < list.len() {
                match flatten_stmt(&mut list[i]) {
                    Some(new) => {
                        // (The statement with the value is looked at again.)
                        let n = new.len();
                        list.splice(i..i + 1, new);
                        i += n - 1;
                        changed = true;
                    }
                    None => i += 1,
                }
            }
        });
        if !changed {
            return;
        }
    }
}

/// The passes that remove objects, again after inlining (`inline_calls`,
/// `predict_calls`, `inline_callbacks`): an object passed to an inlined
/// function is used for its properties there (`forward_inline_args`), and
/// objects made in inlined bodies are the caller's own. Scalar
/// replacement, allocation sinking (`virtualize_objects`, which runs only
/// here, so that it sees through calls inlined later) and those of `aot
/// type` objects, then `number_locals` for the locals they make.
pub fn replace_objects(m: &mut Module) {
    let layouts = std::mem::take(&mut m.layouts);
    for f in m.funcs.iter_mut().chain(m.closures.iter_mut()) {
        forward_inline_args(f);
        flatten_inlines(f);
        forward_copies(f);
    }
    m.layouts = layouts;
    let mut spec = crate::escape::Spec::new(m);
    let cx = VirtCx::new(m);
    let layouts = std::mem::take(&mut m.layouts);
    let objects = |f: &mut Func, spec: &mut crate::escape::Spec| {
        scalar_replace(f, Some(&mut *spec));
        virtualize_objects(f, Some(&mut *spec));
        scalar_replace_structs(f, &layouts, Some(&mut *spec));
        promote_struct_fields(f, &layouts);
        number_locals(f);
        // (Again for the fields of instances kept in locals.)
        if virtualize_instances(f, &cx, Some(spec)) {
            number_locals(f);
        }
    };
    for f in m.funcs.iter_mut().chain(m.closures.iter_mut()) {
        objects(f, &mut spec);
    }
    // The clones, as they are made (they may need others).
    let mut j = 0;
    while j < spec.clones.len() {
        let mut f = spec.clones[j].clone();
        objects(&mut f, &mut spec);
        spec.clones[j] = f;
        j += 1;
    }
    let (base, fields) = (spec.base, spec.field_params());
    m.closures.extend(spec.clones);
    m.layouts = layouts;
    crate::escape::number_params(m, base, &fields);
}

/// Inlining, as JIT compilers inline small functions: a direct call of a
/// small native function (`inlinable`) becomes its body, with its locals
/// appended to the caller's (its parameters first, set from the
/// arguments). A few rounds inline calls in the inlined bodies too.
/// A small function that calls itself inlines its own body once (in the
/// first round): each call then does two levels of the recursion, which
/// halves the calls' cost (`fib`, a tree walk).
pub fn inline_calls(m: &mut Module) {
    let nfuncs = m.funcs.len();
    // Callees qualify by their own size; what they inlined comes along.
    let small: Vec<bool> = m.funcs.iter().map(|f| inlinable(f, INLINE_SIZE)).collect();
    for round in 0..3 {
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
                if (g < nfuncs && *k == g && round > 0) || args.len() != callee.params.len() {
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
        _ => None,
    };
    let field_of_record = |o: &Expr, a: AtomId| match expr_hint(m, f, hints, o)? {
        Hint::Record(r) => {
            let rec = m.records.get(r)?;
            rec.hints[rec.fields.iter().position(|&b| b == a)?]
        }
        _ => None,
    };
    match &x.kind {
        ExprKind::This => f.class.filter(|&(_, s)| f.has_this && !s).map(|(c, _)| Hint::Class(c)),
        ExprKind::Local(l) => hints.get(l).copied(),
        ExprKind::Box(e) | ExprKind::NonNull(_, e) | ExprKind::NonNullSet(_, e) | ExprKind::Predicted { expr: e, .. } => {
            expr_hint(m, f, hints, e)
        }
        ExprKind::Seq(xs) => expr_hint(m, f, hints, xs.last()?),
        ExprKind::NewInstance { class, .. } | ExprKind::NewInstanceFull { class, .. } => Some(Hint::Class(*class)),
        ExprKind::Call(g, _) | ExprKind::CallChecked { func: g, .. } | ExprKind::CallDirectThis(g, ..) => {
            func_at(m, *g).ret_hint
        }
        ExprKind::CallMethod(o, Key::Atom(a), _) => match expr_hint(m, f, hints, o)? {
            Hint::Class(k) => {
                let g = class_member(m, Some(k), false, &m.atoms[*a], 0)?;
                func_at(m, g).ret_hint
            }
            // Array methods that give the same elements, or one of them.
            h @ (Hint::ArrayOf(_) | Hint::ArrayOfRecord(_)) => match m.atoms[*a].as_str() {
                "slice" | "filter" | "sort" | "reverse" | "toSorted" | "toReversed" => Some(h),
                "find" | "findLast" | "pop" | "shift" | "at" => element_hint(h),
                _ => None,
            },
            _ => None,
        },
        ExprKind::ShapeGet { obj, atom, .. } => field_of_record(obj, *atom).or_else(|| {
            let k = class_of(obj)?;
            m.classes.get(k)?.fields.iter().find(|(n, _)| *n == m.atoms[*atom]).map(|(_, h)| *h)
        }),
        ExprKind::GetProp(o, a) if field_of_record(o, *a).is_some() => field_of_record(o, *a),
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
        ExprKind::GetElem(o, _) => element_hint(expr_hint(m, f, hints, o)?),
        ExprKind::Cond(_, a, b) => {
            let h = expr_hint(m, f, hints, a)?;
            (expr_hint(m, f, hints, b)? == h).then_some(h)
        }
        _ => None,
    }
}

/// What an element of an array with hint `h` is.
fn element_hint(h: Hint) -> Option<Hint> {
    match h {
        Hint::ArrayOf(k) => Some(Hint::Class(k)),
        Hint::ArrayOfRecord(r) => Some(Hint::Record(r)),
        Hint::Class(_) | Hint::Record(_) => None,
    }
}

/// Extends `Func::hints` (from annotations) to the locals whose every
/// value has the same hint (`null` and `undefined` aside), including the
/// variables of `for`-`of` loops over arrays, and to the element
/// parameters of arrow functions passed to array methods (`xs.map((x) =>
/// ...)`, `xs.sort((a, b) => ...)`).
pub fn infer_hints(m: &mut Module) {
    let total = m.funcs.len() + m.closures.len();
    let nfuncs = m.funcs.len();
    let mut seeds: Vec<HashMap<LocalId, Hint>> = (0..total).map(|g| func_at(m, g).hints.clone()).collect();
    loop {
        let all: Vec<HashMap<LocalId, Hint>> = (0..total).map(|g| infer_in(m, g, &seeds[g])).collect();
        let mut changed = false;
        for g in 0..total {
            let f = func_at(m, g);
            visit_stmts(&f.body, &mut |e| {
                let ExprKind::CallMethod(recv, Key::Atom(a), args) = &e.kind else { return };
                let Some(ExprKind::Closure(k, _)) = args.first().map(|x| &x.kind) else { return };
                let Some(elem) = expr_hint(m, f, &all[g], recv).and_then(element_hint) else { return };
                let params: &[LocalId] = match m.atoms[*a].as_str() {
                    "forEach" | "map" | "filter" | "some" | "every" | "find" | "findIndex" | "findLast"
                    | "findLastIndex" | "flatMap" => &[0],
                    "sort" | "toSorted" => &[0, 1],
                    "reduce" | "reduceRight" => &[1],
                    _ => return,
                };
                let c = nfuncs + *k;
                let n = func_at(m, c).params.len();
                for &p in params {
                    if p < n && !seeds[c].contains_key(&p) {
                        seeds[c].insert(p, elem);
                        changed = true;
                    }
                }
            });
        }
        if !changed {
            for (g, hints) in all.into_iter().enumerate() {
                let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
                f.hints = hints;
            }
            return;
        }
    }
}

/// `infer_hints` in function `g`, from hints `seeds`.
fn infer_in(m: &Module, g: usize, seeds: &HashMap<LocalId, Hint>) -> HashMap<LocalId, Hint> {
    let f = func_at(m, g);
    let n = f.locals.len();
    let mut other = f.boxed.clone();
    visit_stmts(&f.body, &mut |e| {
        if let ExprKind::Update { local, .. } = &e.kind {
            other[*local] = true;
        }
    });
    stmt_locals(&f.body, &mut |l| other[l] = true);
    let mut hints = seeds.clone();
    loop {
        // The elements of what iterators iterate.
        let mut iters: HashMap<LocalId, Hint> = HashMap::new();
        visit_stmts(&f.body, &mut |e| {
            if let ExprKind::IterOpen(x, iter, _, _) = &e.kind
                && let Some(h) = expr_hint(m, f, &hints, x).and_then(element_hint)
            {
                iters.insert(*iter, h);
            }
        });
        // Per local: the hint all its values have so far, or a conflict.
        let mut seen: Vec<Option<Option<Hint>>> = vec![None; n];
        visit_stmts(&f.body, &mut |e| {
            let (ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x)) = &e.kind else { return };
            if hints.contains_key(l) || other[*l] || matches!(x.kind, ExprKind::Null | ExprKind::Undefined) {
                return;
            }
            let h = match &x.kind {
                ExprKind::IterStep(iter, _) => iters.get(iter).copied(),
                _ => expr_hint(m, f, &hints, x),
            };
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
            return hints;
        }
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
                    ObjPart::Spread(_) | ObjPart::Computed(..) | ObjPart::ComputedMethod(..) | ObjPart::Proto(_) => break,
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
    let orig: Vec<usize> = (0..total).map(|g| get(m, g).locals.len()).collect();
    // Per class: its subclasses (all the way down).
    let subclasses: Vec<Vec<usize>> = (0..m.classes.len())
        .map(|c| {
            (0..m.classes.len())
                .filter(|&k| {
                    let mut up = m.classes[k].superclass;
                    while let Some(u) = up {
                        if u == c {
                            return true;
                        }
                        up = m.classes[u].superclass;
                    }
                    false
                })
                .collect()
        })
        .collect();
    for _ in 0..2 {
        let templates: Vec<Option<Func>> = (0..total)
            .map(|g| (small[g] && inline_ok(get(m, g), usize::MAX, true)).then(|| get(m, g).clone()))
            .collect();
        for g in 0..total {
            let (this, recv, supers) = receivers(get(m, g));
            let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
            let mut body = std::mem::take(&mut f.body);
            // The predictions of the round before are made again, from this
            // round's templates, which carry the callees' own predictions.
            visit_stmts_mut(&mut body, &mut |e| {
                while let ExprKind::Predicted { expr, .. } = &mut e.kind {
                    let x = std::mem::replace(&mut **expr, Expr::new(Ty::Void, ExprKind::Undefined));
                    *e = x;
                }
            });
            f.locals.truncate(orig[g]);
            f.boxed.truncate(orig[g]);
            f.local_names.truncate(orig[g]);
            let base0 = f.locals.len();
            let m_ref: &Module = m;
            let fg = get(m_ref, g);
            let resolve = |e: &Expr| -> Option<usize> {
                // Else what type annotations say it is.
                let hinted = |o: &Expr| match expr_hint(m_ref, fg, &fg.hints, o)? {
                    Hint::Class(k) => Some(Recv::Class(k, false)),
                    _ => None,
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
            // Polymorphic call sites: a method call whose receiver may be an
            // instance of a class or of any of its subclasses (`this` in an
            // instance method; what annotations say), where they have
            // different methods: each of them predicted, in turn (nested
            // `Predicted`s), up to POLY_TARGETS. (Not the locals that only
            // ever hold new instances: their class is known.)
            let mut fresh_writes: HashMap<LocalId, bool> = HashMap::new();
            visit_stmts(&body, &mut |e| {
                if let ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) = &e.kind {
                    let fresh = fresh_instance(x).is_some();
                    fresh_writes.entry(*l).and_modify(|f| *f &= fresh).or_insert(fresh);
                }
            });
            let poly = |e: &Expr| -> Option<Vec<usize>> {
                let ExprKind::CallMethod(o, Key::Atom(a), _) = &e.kind else { return None };
                let hinted = || match expr_hint(m_ref, fg, &fg.hints, o)? {
                    Hint::Class(k) => Some(k),
                    _ => None,
                };
                let c = match &o.kind {
                    ExprKind::This => match &this {
                        Some(Recv::Class(c, false)) => *c,
                        _ => return None,
                    },
                    ExprKind::Local(l) if fresh_writes.get(l) == Some(&true) => return None,
                    ExprKind::Local(l) => match recv.get(l) {
                        Some(Recv::Class(c, false)) => *c,
                        Some(_) => return None,
                        None => hinted()?,
                    },
                    _ => hinted()?,
                };
                let name = &m_ref.atoms[*a];
                let mut out: Vec<usize> = Vec::new();
                for k in std::iter::once(c).chain(subclasses[c].iter().copied()) {
                    if let Some(f) = class_member(m_ref, Some(k), false, name, 0)
                        && !out.contains(&f)
                    {
                        out.push(f);
                    }
                }
                (out.len() > 1).then_some(out)
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
                let targets: Vec<usize> = match poly(e) {
                    Some(ks) => ks.into_iter().filter(|&k| templates[k].is_some()).take(POLY_TARGETS).collect(),
                    None => resolve(e).into_iter().filter(|&k| templates[k].is_some()).collect(),
                };
                let size: usize = targets.iter().map(|&k| ir_size(&templates[k].as_ref().unwrap().body)).sum();
                if targets.is_empty() || size > budget {
                    return;
                }
                budget -= size;
                let ty = e.ty;
                // (The first target outermost: its guard is tried first.)
                for &k in targets.iter().rev() {
                    let t = templates[k].as_ref().unwrap();
                    let base = base0 + added.len();
                    added.extend(&t.locals);
                    if t.locals.is_empty() {
                        added.push(Ty::Bool);
                    }
                    pending.insert(base, k);
                    let locals = (base..base + t.locals.len().max(1)).collect();
                    let expr = Box::new(std::mem::replace(e, Expr::new(Ty::Void, ExprKind::Undefined)));
                    wrapped.insert(&*expr as *const Expr);
                    *e = Expr { ty, kind: ExprKind::Predicted { expr, func: k, body: Vec::new(), locals } };
                }
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

/// The records of classes made natively: a class's `NewInstance` gives its
/// instances' fields in order (`Module::class_records`).
pub fn class_records(m: &mut Module) {
    m.class_records = vec![None; m.classes.len()];
    let mut found: Vec<(usize, Vec<AtomId>)> = Vec::new();
    for f in m.funcs.iter().chain(&m.closures) {
        visit_stmts(&f.body, &mut |e| {
            if let ExprKind::NewInstance { class, fields } = &e.kind {
                found.push((*class, fields.iter().map(|(a, _)| *a).collect()));
            }
        });
    }
    // (A derived class's instances get its superclasses' fields from their
    // constructors, then its own: the whole list, which no `NewInstance`
    // has before `fold_new_instances`.)
    let atom_of = |m: &Module, n: &String| m.atoms.iter().position(|a| a == n);
    for k in 0..m.classes.len() {
        let names = &m.classes[k].instance_fields;
        let atoms: Option<Vec<AtomId>> = names.iter().map(|n| atom_of(m, n)).collect();
        if let Some(atoms) = atoms.filter(|a| !a.is_empty()) {
            found.insert(0, (k, atoms));
        }
    }
    for (k, fields) in found {
        if k >= m.classes.len() || m.class_records[k].is_some() || fields.is_empty() {
            continue;
        }
        let hints = fields
            .iter()
            .map(|&a| m.classes[k].fields.iter().find(|(n, _)| *n == m.atoms[a]).map(|(_, h)| *h))
            .collect();
        let nums = fields.iter().map(|&a| m.classes[k].nums.contains(&m.atoms[a])).collect();
        m.records.push(Record { fields, hints, nums });
        m.class_records[k] = Some(m.records.len() - 1);
    }
}

/// Fast field access, as JIT compilers access the fields of objects of a
/// known shape at fixed offsets: a property read or write of an object
/// that type annotations say is of a record type (`Module::records`: a
/// named object type, or a class made natively) becomes `ShapeGet` /
/// `ShapeSet` with the field's slot. At run time the record's shape is the
/// first one seen with its fields first, in order, as writable data
/// properties; an object with that shape is read and written at the slot,
/// any other one through the inline cache. Annotations are never trusted:
/// a wrong one only costs the shape check.
pub fn specialize_fields(m: &mut Module) {
    let nfuncs = m.funcs.len();
    for g in 0..nfuncs + m.closures.len() {
        let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
        let mut body = std::mem::take(&mut f.body);
        let meta = f.clone();
        let m_ref: &Module = m;
        let record_of = |o: &Expr| -> Option<usize> {
            match expr_hint(m_ref, &meta, &meta.hints, o)? {
                Hint::Record(r) => Some(r),
                Hint::Class(k) => m_ref.class_records.get(k).copied().flatten(),
                _ => None,
            }
        };
        visit_stmts_mut(&mut body, &mut |e| {
            let (ExprKind::GetProp(o, a) | ExprKind::SetProp(o, a, _)) = &e.kind else { return };
            let Some(r) = record_of(o) else { return };
            let Some(slot) = m_ref.records[r].fields.iter().position(|b| b == a) else { return };
            let slot = slot as u32;
            e.kind = match std::mem::replace(&mut e.kind, ExprKind::Undefined) {
                ExprKind::GetProp(obj, atom) => ExprKind::ShapeGet { obj, atom, record: r, slot },
                ExprKind::SetProp(obj, atom, value) => ExprKind::ShapeSet { obj, atom, record: r, slot, value },
                _ => unreachable!(),
            };
        });
        let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
        f.body = body;
    }
}

/// A value whose evaluation cannot run code, throw or observe an object
/// being built: constants and unboxed locals, and numbers made of them.
fn pure_value(e: &Expr, boxed: &[bool]) -> bool {
    match &e.kind {
        ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Undefined | ExprKind::Null => true,
        ExprKind::Local(l) => !boxed[*l],
        ExprKind::Box(x) | ExprKind::Unary(_, x) => pure_value(x, boxed),
        ExprKind::Binary(_, x, y) => pure_value(x, boxed) && pure_value(y, boxed),
        _ => false,
    }
}

/// Constructors that only assign their fields: `this.f = v` statements
/// right after the instance is made (`NewInstance`, its fields
/// `undefined` until then) put `v` in the instance as it is made, when `v`
/// cannot run code; then `return this` right after returns the instance
/// expression itself. So `new Vec(x, y)` inlines to an object expression.
/// After inlining (`inline_calls`) the instance can be an inlined
/// constructor's (a derived class's from its superclass's), whose fields
/// that follow (`DefineField`) are then made with it too.
pub fn fold_new_instances(m: &mut Module) {
    let nfuncs = m.funcs.len();
    for g in 0..nfuncs + m.closures.len() {
        let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
        let boxed = f.boxed.clone();
        let mut uses = vec![0usize; f.locals.len()];
        visit_stmts(&f.body, &mut |e| {
            if let ExprKind::Local(l) = e.kind {
                uses[l] += 1;
            }
        });
        // (Inner instances fold first: a pass folds what the previous one
        // made foldable.)
        loop {
            let mut changed = false;
            stmt_lists_mut(&mut f.body, &mut |list| changed |= fold_list(list, &mut uses, &boxed));
            visit_stmts_mut(&mut f.body, &mut |e| {
                if let ExprKind::Inline { body, .. } = &mut e.kind {
                    stmt_lists_mut(body, &mut |list| changed |= fold_list(list, &mut uses, &boxed));
                }
            });
            if !changed {
                break;
            }
        }
    }
}

/// The fields of the instance that `e` makes, if it is a `NewInstance`, or
/// an inlined body that returns one only at its end.
fn instance_fields(e: &mut Expr) -> Option<&mut Vec<(AtomId, Expr)>> {
    match &mut e.kind {
        ExprKind::NewInstance { fields, .. } => Some(fields),
        ExprKind::Inline { body, .. } => {
            let (last, rest) = body.split_last_mut()?;
            let mut returns = false;
            stmt_lists(rest, &mut |l| returns |= l.iter().any(|s| matches!(s, Stmt::Return(_))));
            match last {
                Stmt::Return(Some(x)) if !returns => instance_fields(x),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `fold_new_instances` in one statement list; whether it folded anything.
fn fold_list(list: &mut Vec<Stmt>, uses: &mut [usize], boxed: &[bool]) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < list.len() {
        let Stmt::Expr(Expr { kind: ExprKind::InitLocal(n, inst), .. }) = &mut list[i] else {
            i += 1;
            continue;
        };
        let n = *n;
        if boxed[n] || instance_fields(inst).is_none() {
            i += 1;
            continue;
        }
        // Fold the assignments (and field definitions) that follow.
        let mut j = i + 1;
        let mut folded = 0;
        while j < list.len() {
            let (o, a, v, define) = match &list[j] {
                Stmt::Expr(Expr { kind: ExprKind::SetProp(o, a, v), .. }) => (o, a, v, false),
                Stmt::Expr(Expr { kind: ExprKind::ShapeSet { obj, atom, value, .. }, .. }) => (obj, atom, value, false),
                Stmt::Expr(Expr { kind: ExprKind::DefineField(o, a, v), .. }) => (o, a, v, true),
                _ => break,
            };
            let (a, v) = (*a, (**v).clone());
            if !matches!(o.kind, ExprKind::Local(l) if l == n) || !pure_value(&v, boxed) {
                break;
            }
            let Stmt::Expr(Expr { kind: ExprKind::InitLocal(_, inst), .. }) = &mut list[i] else { unreachable!() };
            let fields = instance_fields(inst).unwrap();
            if define {
                // A new last field (else it is redefined in place).
                if fields.iter().any(|(b, _)| *b == a) {
                    break;
                }
                fields.push((a, v));
            } else {
                let Some((_, slot)) = fields.iter_mut().find(|(b, x)| *b == a && matches!(x.kind, ExprKind::Undefined))
                else {
                    break;
                };
                *slot = v;
            }
            folded += 1;
            j += 1;
        }
        if folded > 0 {
            // (The `Local(n)` of each assignment is gone.)
            uses[n] -= folded;
            list.drain(i + 1..j);
            changed = true;
        }
        // `return this` right after, its only other use.
        if let Some(Stmt::Return(Some(Expr { kind: ExprKind::Local(r), .. }))) = list.get(i + 1)
            && *r == n
            && uses[n] == 1
        {
            let Stmt::Expr(Expr { kind: ExprKind::InitLocal(_, inst), .. }) = list.remove(i) else { unreachable!() };
            list[i] = Stmt::Return(Some(*inst));
            uses[n] = 0;
            changed = true;
        }
        i += 1;
    }
    changed
}

/// Most methods a polymorphic call site predicts (`predict_calls`).
const POLY_TARGETS: usize = 4;

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
    f.is_closure && !f.has_this && !f.is_async && !f.is_generator && !f.rest && !f.arguments && f.ret_slot.is_none()
}

/// Whether a closure (its parameter types, return type, and whether it is
/// `callback_plain` without captures) can be `JS_SortNative`'s comparator.
fn comparator_ok((params, ret, plain): &(Vec<Ty>, Ty, bool)) -> bool {
    *plain && matches!(params.as_slice(), [Ty::Any, Ty::Any] | [Ty::F64, Ty::F64]) && matches!(ret, Ty::F64 | Ty::Any)
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
            let locals: Vec<LocalId> =
                (0..t.locals.len()).filter(|l| !t.captures.contains(l)).map(|l| base + l).collect();
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
///
/// As `scalar_replace` does, with escape summaries (`spec`) the object may
/// be passed to functions that only read it: their clones take the fields.
pub fn scalar_replace_structs(f: &mut Func, layouts: &[Layout], mut spec: Option<&mut crate::escape::Spec>) {
    use crate::escape::Field;
    let n = f.locals.len();
    let mut ok: Vec<bool> = (0..n)
        .map(|l| {
            l >= f.params.len()
                && !f.boxed[l]
                && f.ret_slot != Some(l)
                && matches!(f.locals[l], Ty::Struct(k) if layouts[k as usize].members.len() == 1
                    && layouts[k as usize].fields.iter().all(|f| f.universal))
        })
        .collect();
    let mut inits = vec![0; n];
    let mut fields_used = vec![0; n];
    let mut sets = vec![0; n];
    let mut top_sets = vec![0; n];
    let mut total = vec![0; n];
    // Passed to a call that may read it later.
    let mut settles = vec![false; n];
    let awaits = crate::escape::awaited(&f.body);
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
        ExprKind::Call(k, args)
        | ExprKind::CallEntry { func: k, args, callee: None, .. }
        | ExprKind::CallDirectThis(k, _, args)
            if spec.is_some() =>
        {
            let spec = spec.as_deref().unwrap();
            let entry = matches!(e.kind, ExprKind::CallEntry { .. });
            for (q, a) in args.iter().enumerate() {
                let ExprKind::Local(l) = a.kind else { continue };
                let Some((fs, outlives)) = spec.reads(*k, q) else { continue };
                if !ok[l] || !fs.iter().all(|fd| matches!(fd, Field::Slot(_))) {
                    continue;
                }
                fields_used[l] += 1;
                settles[l] |= outlives || (entry && spec.suspends(*k) && !awaits.contains(&(e as *const Expr)));
                for later in &args[q + 1..] {
                    visit(later, &mut |x| {
                        if let ExprKind::StructSet(_, o, ..) = &x.kind
                            && matches!(o.kind, ExprKind::Local(m) if m == l)
                        {
                            ok[l] = false;
                        }
                    });
                }
            }
        }
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
        if ok[l]
            && inits[l] == 1
            && total[l] == fields_used[l]
            && sets[l] == top_sets[l]
            && !(settles[l] && sets[l] > 0)
        {
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
    // The clones the calls need (else the objects they pass are kept).
    let slot_args = |spec: &crate::escape::Spec, k: FuncId, args: &[Expr], fields: &[Option<Vec<LocalId>>]| {
        let mut out = Vec::new();
        for (q, a) in args.iter().enumerate() {
            if let ExprKind::Local(l) = a.kind
                && let Some(Some(_)) = fields.get(l)
                && let Some((fs, _)) = spec.reads(k, q)
            {
                let Ty::Struct(lay) = f.locals[l] else { unreachable!() };
                let tys = fs
                    .iter()
                    .map(|fd| match fd {
                        Field::Slot(fi) => field_local_ty(&layouts[lay as usize].fields[*fi as usize]),
                        Field::Prop(_) => unreachable!(),
                    })
                    .collect();
                out.push((q, tys));
            }
        }
        out
    };
    if let Some(spec) = spec.as_deref_mut() {
        loop {
            let mut dropped = false;
            visit_stmts(&f.body, &mut |e| {
                let (ExprKind::Call(k, args)
                | ExprKind::CallEntry { func: k, args, callee: None, .. }
                | ExprKind::CallDirectThis(k, _, args)) = &e.kind
                else {
                    return;
                };
                let objs = slot_args(spec, *k, args, &fields);
                if !objs.is_empty() && spec.clone_of(*k, &objs).is_none() {
                    for a in args {
                        if let ExprKind::Local(l) = a.kind {
                            dropped |= fields[l].take().is_some();
                        }
                    }
                }
            });
            if !dropped {
                break;
            }
        }
        if fields.iter().all(Option::is_none) {
            return;
        }
    }
    let field_of = |l: LocalId| fields.get(l).and_then(Option::as_ref);
    visit_stmts_mut(&mut f.body, &mut |e| {
        if let ExprKind::Call(k, args)
        | ExprKind::CallEntry { func: k, args, callee: None, .. }
        | ExprKind::CallDirectThis(k, _, args) = &mut e.kind
            && let Some(spec) = spec.as_deref_mut()
        {
            let objs = slot_args(spec, *k, args, &fields);
            if objs.is_empty() {
                return;
            }
            let clone = spec.clone_of(*k, &objs).expect("made above");
            let mut out = Vec::new();
            for (q, a) in std::mem::take(args).into_iter().enumerate() {
                match (a.kind, objs.iter().find(|(p, _)| *p == q)) {
                    (ExprKind::Local(l), Some((_, tys))) => {
                        let (fs, _) = spec.reads(*k, q).unwrap();
                        for (fd, &t) in fs.iter().zip(tys) {
                            let Field::Slot(fi) = fd else { unreachable!() };
                            out.push(Expr::new(t, ExprKind::Local(field_of(l).unwrap()[*fi as usize])));
                        }
                    }
                    (kind, _) => out.push(Expr { ty: a.ty, kind }),
                }
            }
            *args = out;
            *k = clone;
            return;
        }
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
pub(crate) fn field_local_ty(f: &LayoutField) -> Ty {
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

/// The class and fields of the new instance `e` evaluates to, where it
/// makes one: a `NewInstance`, or a sequence, inlined body, predicted call
/// (inline) or conditional that does on some path. Other paths may give
/// other values (see `virtual_value`).
fn fresh_instance(e: &Expr) -> Option<(usize, Vec<AtomId>)> {
    fn returns(stmts: &[Stmt], out: &mut Vec<(usize, Vec<AtomId>)>) {
        for s in stmts {
            match s {
                Stmt::Return(Some(x)) => out.extend(fresh_instance(x)),
                Stmt::If(_, a, b) => {
                    returns(a, out);
                    returns(b, out);
                }
                Stmt::Block(b) => returns(b, out),
                _ => {}
            }
        }
    }
    match &e.kind {
        ExprKind::NewInstance { class, fields } => Some((*class, fields.iter().map(|(a, _)| *a).collect())),
        ExprKind::Seq(xs) => fresh_instance(xs.last()?),
        ExprKind::Cond(_, a, b) => {
            let (x, y) = (fresh_instance(a), fresh_instance(b));
            match (x, y) {
                (Some(x), Some(y)) => (x == y).then_some(x),
                (x, y) => x.or(y),
            }
        }
        ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } => {
            let mut out = Vec::new();
            returns(body, &mut out);
            let first = out.first()?.clone();
            out.iter().all(|x| *x == first).then_some(first)
        }
        _ => None,
    }
}

/// For `virtualize_instances`: `e` (see `fresh_instance`) with its new
/// instances of class `class` put in the field locals `slots` instead,
/// setting `flag` (the value is then `undefined`).
fn virtual_value(e: Expr, class: usize, slots: &[LocalId], flag: LocalId) -> Expr {
    fn returns(stmts: &mut [Stmt], class: usize, slots: &[LocalId], flag: LocalId) {
        for s in stmts {
            match s {
                Stmt::Return(Some(x)) => {
                    let v = std::mem::replace(x, Expr::new(Ty::Any, ExprKind::Undefined));
                    *x = virtual_value(v, class, slots, flag);
                }
                Stmt::If(_, a, b) => {
                    returns(a, class, slots, flag);
                    returns(b, class, slots, flag);
                }
                Stmt::Block(b) => returns(b, class, slots, flag),
                _ => {}
            }
        }
    }
    let ty = e.ty;
    match e.kind {
        ExprKind::NewInstance { class: k, fields } if k == class => {
            let mut seq: Vec<Expr> = fields
                .into_iter()
                .zip(slots)
                .map(|((_, v), &s)| Expr::new(Ty::Any, ExprKind::SetLocal(s, Box::new(v))))
                .collect();
            seq.push(Expr::new(
                Ty::Bool,
                ExprKind::SetLocal(flag, Box::new(Expr::new(Ty::Bool, ExprKind::Bool(true)))),
            ));
            seq.push(Expr::new(Ty::Any, ExprKind::Undefined));
            Expr::new(Ty::Any, ExprKind::Seq(seq))
        }
        ExprKind::Seq(mut xs) => {
            let last = xs.pop().unwrap();
            xs.push(virtual_value(last, class, slots, flag));
            Expr::new(ty, ExprKind::Seq(xs))
        }
        ExprKind::Cond(c, a, b) => Expr::new(
            ty,
            ExprKind::Cond(
                c,
                Box::new(virtual_value(*a, class, slots, flag)),
                Box::new(virtual_value(*b, class, slots, flag)),
            ),
        ),
        ExprKind::Inline { mut body, locals } => {
            returns(&mut body, class, slots, flag);
            Expr::new(ty, ExprKind::Inline { body, locals })
        }
        ExprKind::Predicted { expr, func, mut body, locals } => {
            returns(&mut body, class, slots, flag);
            Expr::new(ty, ExprKind::Predicted { expr, func, body, locals })
        }
        kind => Expr::new(ty, kind),
    }
}

/// `visit_stmts_mut` over `stmts`, but not in the bodies of predicted calls
/// (whose `this` is their receiver), nor on those calls themselves.
pub(crate) fn visit_outside_predicted(stmts: &mut Vec<Stmt>, f: &mut dyn FnMut(&mut Expr)) {
    let mut stash: Vec<Vec<Stmt>> = Vec::new();
    // (Set aside behind a marker, as `this_to_local` does.)
    let marker = |i: usize| vec![Stmt::Expr(Expr::new(Ty::Void, ExprKind::Num(i as f64)))];
    visit_stmts_mut(stmts, &mut |e| match &mut e.kind {
        ExprKind::Predicted { body, .. } => {
            let b = std::mem::replace(body, marker(stash.len()));
            stash.push(b);
        }
        _ => f(e),
    });
    let mut stash: Vec<Option<Vec<Stmt>>> = stash.into_iter().map(Some).collect();
    visit_stmts_mut(stmts, &mut |e| {
        if let ExprKind::Predicted { body, .. } = &mut e.kind
            && let [Stmt::Expr(Expr { ty: Ty::Void, kind: ExprKind::Num(i) })] = body.as_slice()
        {
            *body = stash[*i as usize].take().unwrap();
        }
    });
}

/// Replace `This` in `body` with `Local(l)`, except inside the bodies of
/// predicted calls (their own `this`).
fn this_to_local(body: &mut Vec<Stmt>, l: LocalId) {
    let mut stash: Vec<Vec<Stmt>> = Vec::new();
    // A predicted body is set aside behind a marker (a `Void`-typed number,
    // which the IR has nowhere else) while `this` is replaced.
    let marker = |i: usize| vec![Stmt::Expr(Expr::new(Ty::Void, ExprKind::Num(i as f64)))];
    visit_stmts_mut(body, &mut |e| match &mut e.kind {
        ExprKind::Predicted { body, .. } => {
            let b = std::mem::replace(body, marker(stash.len()));
            stash.push(b);
        }
        ExprKind::This => e.kind = ExprKind::Local(l),
        _ => {}
    });
    let mut stash: Vec<Option<Vec<Stmt>>> = stash.into_iter().map(Some).collect();
    visit_stmts_mut(body, &mut |e| {
        if let ExprKind::Predicted { body, .. } = &mut e.kind
            && let [Stmt::Expr(Expr { ty: Ty::Void, kind: ExprKind::Num(i) })] = body.as_slice()
        {
            *body = stash[*i as usize].take().unwrap();
        }
    });
}

/// `x` without the boxing of a number or boolean for a JS value.
fn unboxed(x: &Expr) -> &Expr {
    match &x.kind {
        ExprKind::Box(y) => y,
        _ => x,
    }
}

/// What `virtualize_instances` needs of the module.
struct VirtCx {
    params: Vec<Vec<Ty>>,
    rets: Vec<Ty>,
    /// Per class: its instance methods (own), by atom.
    methods: Vec<HashMap<AtomId, usize>>,
    /// Per class: whether its instances have private state (never virtual).
    private_state: Vec<bool>,
}

impl VirtCx {
    /// Of the module's functions and classes.
    fn new(m: &Module) -> VirtCx {
        let all = || m.funcs.iter().chain(&m.closures);
        let atom_of: HashMap<&str, AtomId> = m.atoms.iter().enumerate().map(|(i, a)| (a.as_str(), i)).collect();
        VirtCx {
            params: all().map(|f| f.params.clone()).collect(),
            rets: all().map(|f| f.ret).collect(),
            private_state: m.classes.iter().map(|c| c.private_state).collect(),
            methods: m
                .classes
                .iter()
                .map(|c| {
                    c.members
                        .iter()
                        .filter(|(_, kind, is_static, _)| *kind == 0 && !is_static)
                        .filter_map(|(n, _, _, f)| Some((*atom_of.get(n.as_str())?, *f)))
                        .collect()
                })
                .collect(),
        }
    }
}

/// Scalar replacement of class instances, as JIT compilers' escape analysis
/// does: a local holding a new instance of a class (a `NewInstance`, also
/// as the value of an inlined or predicted call) that is only used for its
/// fields and for predicted calls of its class's own methods keeps its
/// fields in locals of their own while it is virtual (a flag local): reads
/// and writes of the fields use them, and a method runs inline over them
/// behind a check that the class's prototype still has it. Where the
/// object itself is needed (a method that is not the predicted one, a call
/// that a prediction declines), it is made first ("materialized", as
/// deoptimization does), and the uses then work on it.
///
/// (In `replace_objects`, function by function: with escape summaries, an
/// instance may be passed to functions that only read it, see
/// `try_virtualize`.)
fn virtualize_instances(f: &mut Func, cx: &VirtCx, mut spec: Option<&mut crate::escape::Spec>) -> bool {
    {
        if f.is_generator {
            return false;
        }
        hoist_fresh_receivers(f);
        // Locals initialized once (a statement, or a value discarded in a
        // sequence) by a value that may be a new instance, and never
        // assigned otherwise.
        let mut inits = vec![0usize; f.locals.len()];
        let mut assigns = vec![0usize; f.locals.len()];
        let mut fresh = vec![false; f.locals.len()];
        let mut discarded: Vec<LocalId> = Vec::new();
        let mut statements = |stmts: &[Stmt]| {
            top_exprs(stmts, &mut |e| {
                if let ExprKind::InitLocal(l, _) = e.kind {
                    discarded.push(l);
                }
            })
        };
        statements(&f.body);
        visit_stmts(&f.body, &mut |e| {
            if let ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } | ExprKind::ArrayLoop { body, .. } =
                &e.kind
            {
                statements(body);
            }
        });
        visit_stmts(&f.body, &mut |e| match &e.kind {
            ExprKind::InitLocal(l, x) => {
                inits[*l] += 1;
                fresh[*l] |= fresh_instance(x).is_some();
            }
            ExprKind::SetLocal(l, _) | ExprKind::Update { local: l, .. } | ExprKind::ArrayPush(l, _) => {
                assigns[*l] += 1
            }
            ExprKind::Seq(xs) => {
                for x in &xs[..xs.len().saturating_sub(1)] {
                    if let ExprKind::InitLocal(l, _) = x.kind {
                        discarded.push(l);
                    }
                }
            }
            _ => {}
        });
        let mut owned = vec![false; f.locals.len()];
        stmt_locals(&f.body, &mut |l| owned[l] = true);
        let candidates: Vec<LocalId> = (f.params.len()..f.locals.len())
            .filter(|&l| {
                fresh[l]
                    && inits[l] == 1
                    && assigns[l] == 0
                    && !owned[l]
                    && !f.boxed[l]
                    && f.locals[l] == Ty::Any
                    && f.ret_slot != Some(l)
                    && discarded.contains(&l)
            })
            .collect();
        let mut any = false;
        for l in candidates {
            // (The parameters of the methods it ran inline read the
            // arguments' locals: the next instances' uses.)
            if try_virtualize(f, l, cx, spec.as_deref_mut()) {
                forward_inline_args(f);
                any = true;
            }
        }
        any
    }
}

/// Give a new instance that receives a predicted call (`(new V(...)).m()`,
/// `a.add(b).scale(k)`), or is an argument of one (`a.add(b.scale(k))`), a
/// local of its own, for `virtualize_instances`: the receiver before the
/// call (it is evaluated first), an argument in place.
fn hoist_fresh_receivers(f: &mut Func) {
    let base = f.locals.len();
    let mut added = 0;
    visit_stmts_mut(&mut f.body, &mut |e| {
        let ExprKind::Predicted { expr, .. } = &mut e.kind else { return };
        let ExprKind::CallMethod(o, _, args) = &mut expr.kind else { return };
        for x in args.iter_mut() {
            if !matches!(x.kind, ExprKind::Local(_)) && fresh_instance(x).is_some() {
                let t = base + added;
                added += 1;
                let r = std::mem::replace(x, Expr::new(Ty::Any, ExprKind::Local(t)));
                *x = Expr::new(
                    Ty::Any,
                    ExprKind::Seq(vec![Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(r))), local_expr(t)]),
                );
            }
        }
        if matches!(o.kind, ExprKind::Local(_) | ExprKind::This) || fresh_instance(o).is_none() {
            return;
        }
        let t = base + added;
        added += 1;
        let r = std::mem::replace(&mut **o, local_expr(t));
        let call = std::mem::replace(e, Expr::new(Ty::Void, ExprKind::Undefined));
        let ty = call.ty;
        *e = Expr::new(ty, ExprKind::Seq(vec![Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(r))), call]));
    });
    f.locals.extend(std::iter::repeat_n(Ty::Any, added));
    f.boxed.extend(std::iter::repeat_n(false, added));
    f.local_names.extend(std::iter::repeat_n(None, added));
}

fn local_expr(l: LocalId) -> Expr {
    Expr::new(Ty::Any, ExprKind::Local(l))
}

/// `virtualize_instances` for local `l`, if all its uses allow it.
///
/// With escape summaries (`spec`), a call of a function that only reads the
/// object calls a clone of it with the fields while it is virtual (its
/// other operands evaluated first), else the function with the object.
fn try_virtualize(f: &mut Func, l: LocalId, cx: &VirtCx, mut spec: Option<&mut crate::escape::Spec>) -> bool {
    // The producer's class and fields.
    let mut found = None;
    visit_stmts(&f.body, &mut |e| {
        if let ExprKind::InitLocal(m, x) = &e.kind
            && *m == l
        {
            found = fresh_instance(x);
        }
    });
    let Some((class, fields)) = found else { return false };
    let Some(methods) = cx.methods.get(class) else { return false };
    // (Made from its fields alone, it would lack them.)
    if cx.private_state[class] {
        return false;
    }
    let base = f.locals.len();
    let flag = base;
    let slots: Vec<LocalId> = (base + 1..base + 1 + fields.len()).collect();
    // The object itself while being rewritten (renamed `l` at the end).
    let ph = base + 1 + fields.len();
    let mut types = f.locals.clone();
    types.push(Ty::Bool);
    types.extend(std::iter::repeat_n(Ty::Any, fields.len() + 1));
    let local = |x: LocalId| Expr::new(Ty::Any, ExprKind::Local(x));
    let bool_ = |b: bool| Expr::new(Ty::Bool, ExprKind::Bool(b));
    let flag_e = || Expr::new(Ty::Bool, ExprKind::Local(flag));
    // The object, made from the fields if it is virtual.
    let materialize = || {
        let make = Expr::new(
            Ty::Any,
            ExprKind::NewInstance { class, fields: fields.iter().zip(&slots).map(|(&a, &s)| (a, local(s))).collect() },
        );
        Expr::new(
            Ty::Any,
            ExprKind::Cond(
                Box::new(flag_e()),
                Box::new(Expr::new(
                    Ty::Any,
                    ExprKind::Seq(vec![
                        Expr::new(Ty::Any, ExprKind::SetLocal(ph, Box::new(make))),
                        Expr::new(Ty::Bool, ExprKind::SetLocal(flag, Box::new(bool_(false)))),
                        local(ph),
                    ]),
                )),
                Box::new(local(ph)),
            ),
        )
    };
    let field = |a: AtomId| fields.iter().position(|&b| b == a);
    let mut ok = true;
    let mut virtual_uses = 0;
    // Where the object itself is used (and made, if virtual).
    let mut others = 0;
    // The calls predicted calls make where they do not run inline.
    let mut wrapped: HashSet<*const Expr> = HashSet::new();
    let mut reason = String::new();
    let debug = std::env::var_os("LUMINOUS_DEBUG_VIRT").is_some();
    let mut body = f.body.clone();
    let awaits = crate::escape::awaited(&body);
    // (Anywhere in the function: see `object_args`.)
    let later = spec.as_deref().map_or(usize::MAX, |spec| non_reads(spec, &f.body, l));
    // Temporaries, after the object's locals.
    let temps = std::cell::Cell::new(0);
    let once = |v: Expr| -> (Option<Expr>, Expr) {
        let mut names = false;
        visit(&v, &mut |x| names |= matches!(x.kind, ExprKind::Local(m) if m == l));
        if plain_value(&v) && !names {
            return (None, v);
        }
        let t = ph + 1 + temps.get();
        temps.set(temps.get() + 1);
        (Some(Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(v)))), local(t))
    };
    visit_stmts_mut(&mut body, &mut |e| {
        if !ok {
            return;
        }
        if debug {
            reason = format!("{:?}", e.kind);
        }
        let here = e as *const Expr;
        let is_l = |x: &Expr| matches!(x.kind, ExprKind::Local(m) if m == l);
        let ty = e.ty;
        // A call of a function that only reads it.
        if let Some(spec) = spec.as_deref_mut()
            && let objs = object_args(spec, e, l, &fields, later, &awaits)
            && !objs.is_empty()
            && let Some(clone) = spec.clone_of(callee_of(e), &clone_key(&objs))
        {
            let mut parts = take_call(e);
            let mut first = Vec::new();
            if let Some(t) = parts.this.take() {
                let (f0, v) = once(t);
                first.extend(f0);
                parts.this = Some(v);
            }
            let (mut fast, mut slow, mut index) = (Vec::new(), Vec::new(), Vec::new());
            for (q, a) in std::mem::take(&mut parts.args).into_iter().enumerate() {
                index.push(fast.len());
                match objs.iter().find(|(p, _)| *p == q) {
                    Some((_, ks)) => {
                        fast.extend(ks.iter().map(|&key| local(slots[field(key).unwrap()])));
                        slow.push(materialize());
                    }
                    None => {
                        let (f0, v) = once(a);
                        first.extend(f0);
                        fast.push(v.clone());
                        slow.push(v);
                    }
                }
            }
            // (Where the clone's call may go to the function object instead, the
            // object is made for it.)
            let made = parts.callee.is_some().then(|| slow.clone());
            let fast = Expr::new(ty, parts.call(clone, fast, made, &|i| index[i]));
            let slow = Expr::new(ty, parts.call(parts.func, slow, None, &|i| i));
            let c = Expr::new(ty, ExprKind::Cond(Box::new(flag_e()), Box::new(fast), Box::new(slow)));
            *e = if first.is_empty() {
                c
            } else {
                first.push(c);
                Expr::new(ty, ExprKind::Seq(first))
            };
            virtual_uses += 1;
            return;
        }
        match &mut e.kind {
            ExprKind::SetLocal(m, _) | ExprKind::Update { local: m, .. } if *m == l => ok = false,
            // The object itself: made here (if it is still virtual).
            ExprKind::Local(m) if *m == l => {
                others += 1;
                *e = materialize();
            }
            ExprKind::InitLocal(m, x) if *m == l => {
                let x = std::mem::replace(&mut **x, Expr::new(Ty::Any, ExprKind::Undefined));
                let mut seq = vec![Expr::new(Ty::Bool, ExprKind::InitLocal(flag, Box::new(bool_(false))))];
                // (A slot is read only once the instance's field is in it: a
                // number to begin with, so that number fields' can be numbers.)
                for &s in &slots {
                    seq.push(Expr::new(
                        Ty::Any,
                        ExprKind::InitLocal(s, Box::new(Expr::new(Ty::Any, ExprKind::Box(Box::new(Expr::num(0.0)))))),
                    ));
                }
                seq.push(Expr::new(Ty::Any, ExprKind::InitLocal(ph, Box::new(virtual_value(x, class, &slots, flag)))));
                *e = Expr::new(Ty::Any, ExprKind::Seq(seq));
            }
            ExprKind::GetProp(o, a) | ExprKind::ShapeGet { obj: o, atom: a, .. } if is_l(o) => {
                // (Not a field: read from the object, made.)
                let Some(i) = field(*a) else { return };
                virtual_uses += 1;
                // (The object's read, on the object: `ph`.)
                let mut real = std::mem::replace(e, Expr::new(Ty::Void, ExprKind::Undefined));
                let (ExprKind::GetProp(o, _) | ExprKind::ShapeGet { obj: o, .. }) = &mut real.kind else {
                    unreachable!()
                };
                **o = local(ph);
                *e = Expr::new(ty, ExprKind::Cond(Box::new(flag_e()), Box::new(local(slots[i])), Box::new(real)));
            }
            ExprKind::SetProp(o, a, _) | ExprKind::ShapeSet { obj: o, atom: a, .. } if is_l(o) => {
                let Some(i) = field(*a) else { return };
                let mut real = std::mem::replace(e, Expr::new(Ty::Void, ExprKind::Undefined));
                let (ExprKind::SetProp(o, _, v) | ExprKind::ShapeSet { obj: o, value: v, .. }) = &mut real.kind else {
                    unreachable!()
                };
                **o = local(ph);
                let v = (**v).clone();
                *e = Expr::new(
                    ty,
                    ExprKind::Cond(
                        Box::new(flag_e()),
                        Box::new(Expr::new(Ty::Any, ExprKind::SetLocal(slots[i], Box::new(v)))),
                        Box::new(real),
                    ),
                );
            }
            // A method of its class not run inline, that only reads `this`:
            // its clone with the fields while the object is virtual, behind
            // the check that its class's prototype still has it (the method
            // read, as a call reads it before the arguments).
            ExprKind::CallMethod(o, Key::Atom(a), args) if is_l(o) && !wrapped.contains(&here) => {
                let a = *a;
                let Some(spec) = spec.as_deref_mut() else { return };
                let Some(&func) = methods.get(&a) else { return };
                let Some((fs, false)) = spec.reads(func, crate::escape::THIS_PARAM) else { return };
                let Some(keys) = fs
                    .iter()
                    .map(|fd| match fd {
                        crate::escape::Field::Prop(k) if field(*k).is_some() => Some(*k),
                        _ => None,
                    })
                    .collect::<Option<Vec<AtomId>>>()
                else {
                    return;
                };
                let params = &cx.params[func];
                let ret = cx.rets[func];
                // (A number or boolean parameter takes the value of a plain
                // boxed one: a constant, a local.)
                let typed_ok = |i: usize, x: &Expr| match params.get(i) {
                    Some(Ty::Any) => true,
                    Some(&t @ (Ty::F64 | Ty::Bool)) => {
                        matches!(&x.kind, ExprKind::Box(y) if y.ty == t && plain_value(y))
                    }
                    _ => false,
                };
                if args.len() > params.len()
                    || !args.iter().enumerate().all(|(i, x)| typed_ok(i, x))
                    || params[args.len()..].iter().any(|&t| t != Ty::Any)
                {
                    return;
                }
                let wrap = |x: Expr| match ret {
                    Ty::Any => Some(x),
                    Ty::F64 | Ty::Bool => Some(Expr::new(Ty::Any, ExprKind::Box(Box::new(x)))),
                    Ty::Void => {
                        Some(Expr::new(Ty::Any, ExprKind::Seq(vec![x, Expr::new(Ty::Any, ExprKind::Undefined)])))
                    }
                    _ => None,
                };
                let key = [(crate::escape::THIS_PARAM, vec![Ty::Any; keys.len()])];
                let Some(clone) = spec.clone_of(func, &key) else { return };
                let mut first = Vec::new();
                let mut vals = Vec::new();
                for (i, x) in args.clone().into_iter().enumerate() {
                    if params[i] != Ty::Any {
                        vals.push(unboxed(&x).clone());
                        continue;
                    }
                    let (f0, v) = once(x);
                    first.extend(f0);
                    vals.push(v);
                }
                vals.resize(params.len(), Expr::new(Ty::Any, ExprKind::Undefined));
                let mut clone_args: Vec<Expr> = keys.iter().map(|&k| local(slots[field(k).unwrap()])).collect();
                clone_args.extend(vals.iter().cloned());
                let (Some(fast), Some(direct)) = (
                    wrap(Expr::new(ret, ExprKind::Call(clone, clone_args))),
                    wrap(Expr::new(ret, ExprKind::CallDirectThis(func, Box::new(materialize()), vals))),
                ) else {
                    return;
                };
                // (The arguments may have made it: then the method, checked,
                // on the object.)
                first.push(Expr::new(Ty::Any, ExprKind::Cond(Box::new(flag_e()), Box::new(fast), Box::new(direct))));
                let fast = Expr::new(Ty::Any, ExprKind::Seq(first));
                let slow = Expr::new(ty, ExprKind::CallMethod(Box::new(materialize()), Key::Atom(a), args.clone()));
                let check = Expr::new(
                    Ty::Bool,
                    ExprKind::Cond(
                        Box::new(flag_e()),
                        Box::new(Expr::new(Ty::Bool, ExprKind::ProtoMethodIs { class, method: a, func })),
                        Box::new(bool_(false)),
                    ),
                );
                virtual_uses += 1;
                *e = Expr::new(ty, ExprKind::Cond(Box::new(check), Box::new(fast), Box::new(slow)));
            }
            ExprKind::Predicted { expr, func, body: pbody, locals } => {
                // (Its call is not made a clone's: see above.)
                wrapped.insert(&**expr as *const Expr);
                // One of several predictions of a call (polymorphic): they
                // all bind its arguments, so the object is made for them.
                if wrapped.contains(&here) {
                    return;
                }
                let ExprKind::CallMethod(o, Key::Atom(a), args) = &mut expr.kind else { return };
                if !is_l(o) {
                    // The object passed to a method of another receiver: its
                    // parameter is the object inline; the object is made only
                    // for the call (a lazy argument).
                    let params = &cx.params[*func];
                    for i in 0..args.len() {
                        // (The object itself, or made in place: `Seq[init, it]`.)
                        let direct = is_l(&args[i]);
                        let in_seq = matches!(&args[i].kind, ExprKind::Seq(xs) if xs.last().is_some_and(is_l));
                        if !direct && !in_seq {
                            continue;
                        }
                        let p = locals[i];
                        let mut assigned = false;
                        visit_stmts(pbody, &mut |x| assigned |= assigns_local(x, p));
                        // (Else made for the call.)
                        if params.get(i) != Some(&Ty::Any) || assigned {
                            continue;
                        }
                        visit_stmts_mut(pbody, &mut |y| {
                            if matches!(y.kind, ExprKind::Local(q) if q == p) {
                                y.kind = ExprKind::Local(l);
                            }
                        });
                        let lazy = Expr::new(Ty::Any, ExprKind::Lazy(Box::new(materialize())));
                        match &mut args[i].kind {
                            ExprKind::Seq(xs) if in_seq => *xs.last_mut().unwrap() = lazy,
                            _ => args[i] = lazy,
                        }
                        virtual_uses += 1;
                    }
                    return;
                }
                let (a, func) = (*a, *func);
                let params = &cx.params[func];
                // (Else the method called on the object, made.)
                if methods.get(&a) != Some(&func) || args.len() > params.len() {
                    return;
                }
                // The method inline over the fields: parameters bound to the
                // arguments (the object itself: an alias), `this` the object.
                let mut inner = pbody.clone();
                let mut binds = Vec::new();
                for (i, &pt) in params.iter().enumerate() {
                    let p = locals[i];
                    let arg = args.get(i);
                    let mut assigned = false;
                    visit_stmts(&inner, &mut |x| assigned |= assigns_local(x, p));
                    let v = match (arg, pt) {
                        (Some(x), Ty::Any) if is_l(x) && !assigned => {
                            visit_stmts_mut(&mut inner, &mut |y| {
                                if matches!(y.kind, ExprKind::Local(q) if q == p) {
                                    y.kind = ExprKind::Local(l);
                                }
                            });
                            Expr::new(Ty::Any, ExprKind::Undefined)
                        }
                        (Some(x), Ty::Any) if types[p] == Ty::Any => match x.ty {
                            Ty::F64 | Ty::Bool => Expr::new(Ty::Any, ExprKind::Box(Box::new(x.clone()))),
                            Ty::Any => x.clone(),
                            _ => return,
                        },
                        // (Arguments are JS values: a number boxed for the call.)
                        (Some(x), Ty::F64 | Ty::Bool) if types[p] == pt && unboxed(x).ty == pt => unboxed(x).clone(),
                        (None, Ty::Any) if types[p] == Ty::Any => Expr::new(Ty::Any, ExprKind::Undefined),
                        _ => return,
                    };
                    binds.push(Stmt::Expr(Expr::new(types[p], ExprKind::InitLocal(p, Box::new(v)))));
                }
                this_to_local(&mut inner, l);
                binds.extend(inner);
                let ret = cx.rets[func];
                let mut inline = Expr::new(ret, ExprKind::Inline { body: binds, locals: locals.clone() });
                match ret {
                    Ty::Any => {}
                    Ty::F64 | Ty::Bool => inline = Expr::new(Ty::Any, ExprKind::Box(Box::new(inline))),
                    _ => return,
                }
                // Otherwise the call, on the object (made if virtual).
                let mut args2 = args.clone();
                for x in &mut args2 {
                    visit_mut(x, &mut |y| {
                        if matches!(y.kind, ExprKind::Local(q) if q == l) {
                            y.kind = ExprKind::Local(ph);
                        }
                    });
                }
                // (Not predicted: the method changed, or the object came from
                // a call, both rare; the body is not copied again.)
                let call = Expr::new(ty, ExprKind::CallMethod(Box::new(materialize()), Key::Atom(a), args2));
                let check = Expr::new(
                    Ty::Bool,
                    ExprKind::Cond(
                        Box::new(flag_e()),
                        Box::new(Expr::new(Ty::Bool, ExprKind::ProtoMethodIs { class, method: a, func })),
                        Box::new(bool_(false)),
                    ),
                );
                virtual_uses += 1;
                *e = Expr::new(ty, ExprKind::Cond(Box::new(check), Box::new(inline), Box::new(call)));
            }
            _ => {}
        }
    });
    if debug {
        eprintln!("virtualize {} local {l}: {}", f.name, if ok { "yes" } else { "no" });
        if !ok {
            eprintln!("  at {}", reason.chars().take(300).collect::<String>());
        }
    }
    if !ok || virtual_uses == 0 || others > MATERIALIZE_SITES {
        return false;
    }
    // The object local is `l` again.
    visit_stmts_mut(&mut body, &mut |e| match &mut e.kind {
        ExprKind::Local(m) | ExprKind::SetLocal(m, _) | ExprKind::InitLocal(m, _) if *m == ph => *m = l,
        _ => {}
    });
    f.body = body;
    f.locals.push(Ty::Bool);
    f.locals.extend(std::iter::repeat_n(Ty::Any, fields.len() + 1 + temps.get()));
    let n = fields.len() + 2 + temps.get();
    f.boxed.extend(std::iter::repeat_n(false, n));
    f.local_names.extend(std::iter::repeat_n(None, n));
    crate::escape::fold_flag(f, flag);
    true
}
