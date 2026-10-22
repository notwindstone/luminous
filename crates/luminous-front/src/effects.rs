//! Effect summaries: what each function's code reads and writes, whether it
//! allocates, may throw (and from what), may suspend, and whether it may
//! run code that is not visible here. Bottom-up over the call graph, each
//! strongly connected component (recursion) iterated to a fixpoint (the
//! graph and its components are escape analysis's: `escape::components`).
//!
//! A summary describes the function's own code and the functions it calls
//! directly (native calls of known functions), on every path, the slow
//! paths of its operations included. What the engine may run on its behalf
//! (getters, setters, proxy traps, coercions through `valueOf` / `toString`
//! / `Symbol.toPrimitive`, builtins that were replaced, calls through
//! values, the function as interpreted when its entry declines) is not in
//! `reads` and `writes`: it is `engine`. The engine counts every such call
//! (`js_call_epoch`, moved by every call of code that native code does not
//! call directly: see the engine patch), so a caller that finds the count
//! unchanged after a call knows that only what the summary says happened.
//! What may throw is counted on every path, the engine's included.
//!
//! Summaries are properties of the source: they hold for a function's
//! interpreted copy as well (which only runs through the engine, which
//! counts it).

use crate::ir::*;
use std::collections::BTreeSet;

/// Where an exception may come from (`Effects::throws`, a set of these).
pub mod throws {
    /// A property of `null` or `undefined` read or written.
    pub const NULLISH: u16 = 1;
    /// A `TypeError` of an operation: a value of the wrong type, BigInt
    /// mixed with numbers, a frozen object written, a brand check.
    pub const TYPE: u16 = 2;
    /// A `ReferenceError`: a binding read before it is initialized, a
    /// global that does not exist.
    pub const REFERENCE: u16 = 4;
    /// `throw`, and a rejected `await`.
    pub const EXPLICIT: u16 = 8;
    /// Running out of memory on an allocation.
    pub const MEMORY: u16 = 16;
    /// What the engine runs (see the module comment).
    pub const ENGINE: u16 = 32;
    /// A native call's stack check (a `RangeError`).
    pub const STACK: u16 = 64;
    pub const ALL: u16 = 127;
}

/// What code reads or writes: properties by name, `aot type` fields,
/// module bindings, globals, array and typed array elements (and their
/// lengths), `Map` and `Set` contents, variables shared with closures,
/// private fields; or anything (`all`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Access {
    pub all: bool,
    pub fields: BTreeSet<AtomId>,
    /// `(layout, field)`.
    pub slots: BTreeSet<(u32, u32)>,
    pub cells: BTreeSet<CellId>,
    pub globals: BTreeSet<AtomId>,
    pub privates: BTreeSet<usize>,
    pub elements: bool,
    pub collections: bool,
    pub boxes: bool,
}

impl Access {
    pub fn top() -> Access {
        Access { all: true, ..Access::default() }
    }

    pub fn is_empty(&self) -> bool {
        *self == Access::default()
    }

    pub fn join(&mut self, o: &Access) {
        self.all |= o.all;
        self.fields.extend(&o.fields);
        self.slots.extend(&o.slots);
        self.cells.extend(&o.cells);
        self.globals.extend(&o.globals);
        self.privates.extend(&o.privates);
        self.elements |= o.elements;
        self.collections |= o.collections;
        self.boxes |= o.boxes;
    }

    /// Whether it includes property `atom` (of any object).
    pub fn field(&self, atom: AtomId) -> bool {
        self.all || self.fields.contains(&atom)
    }

    pub fn cell(&self, c: CellId) -> bool {
        self.all || self.cells.contains(&c)
    }

    fn fields(atoms: impl IntoIterator<Item = AtomId>) -> Access {
        Access { fields: atoms.into_iter().collect(), ..Access::default() }
    }
}

/// What a function (or a piece of code) does: see the module comment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub reads: Access,
    pub writes: Access,
    pub allocates: bool,
    /// `throws::*`.
    pub throws: u16,
    /// May run code through the engine (see the module comment).
    pub engine: bool,
    /// `await`, `yield`.
    pub suspends: bool,
    /// May observe or do what does not follow from its inputs: time,
    /// randomness, I/O (all through the engine, for now).
    pub nondet: bool,
}

impl Effects {
    /// Anything: an unknown callee, an operation not analysed.
    pub fn top() -> Effects {
        Effects {
            reads: Access::top(),
            writes: Access::top(),
            allocates: true,
            throws: throws::ALL,
            engine: true,
            suspends: false,
            nondet: true,
        }
    }

    pub fn join(&mut self, o: &Effects) {
        self.reads.join(&o.reads);
        self.writes.join(&o.writes);
        self.allocates |= o.allocates;
        self.throws |= o.throws;
        self.engine |= o.engine;
        self.suspends |= o.suspends;
        self.nondet |= o.nondet;
    }

    /// Code that may run what the engine runs (a call through a value, a
    /// coercion, a getter): may throw anything.
    fn engine() -> Effects {
        Effects { engine: true, nondet: true, throws: throws::ENGINE, ..Effects::default() }
    }

    fn reads(a: Access) -> Effects {
        Effects { reads: a, ..Effects::default() }
    }

    fn allocating() -> Effects {
        Effects { allocates: true, throws: throws::MEMORY, ..Effects::default() }
    }

    /// Neither throws nor runs anything else: what it reads may be cached.
    pub fn quiet(&self) -> bool {
        self.throws == 0 && !self.engine && !self.suspends
    }
}

/// The summaries of a module's functions (module functions, then
/// closures), with their call depth.
#[derive(Clone, Debug)]
pub struct Summaries {
    pub funcs: Vec<Effects>,
    /// The most native frames a call of the function adds (itself
    /// included), when bounded: not recursive, only direct callees.
    pub depth: Vec<Option<u32>>,
    /// Throws nothing (but stack overflow) and adds at most
    /// `NOTHROW_DEPTH` frames: such a function makes no stack check of its
    /// own (its caller's check covers the frames it adds, which the native
    /// stack's margin below its limit holds), so a call of it cannot throw
    /// at all, and its caller does not test for an exception after it.
    pub nothrow: Vec<bool>,
}

/// The most frames a function that makes no stack check of its own may
/// add below its caller's (see `Summaries::nothrow`).
pub const NOTHROW_DEPTH: u32 = 8;

impl Summaries {
    pub fn of(&self, k: usize) -> &Effects {
        &self.funcs[k]
    }

    /// The effects of a call of function `k`: its own, and its stack check.
    pub fn call(&self, k: usize) -> Effects {
        let Some(e) = self.funcs.get(k) else { return Effects::top() };
        let mut e = e.clone();
        if !self.nothrow[k] {
            e.throws |= throws::STACK;
        }
        e
    }
}

fn func_at(m: &Module, g: usize) -> &Func {
    if g < m.funcs.len() { &m.funcs[g] } else { &m.closures[g - m.funcs.len()] }
}

/// The functions `f` calls directly (natively, without the engine).
pub fn direct_callees(f: &Func) -> Vec<usize> {
    let mut out = Vec::new();
    visit_stmts(&f.body, &mut |e| {
        let k = match &e.kind {
            ExprKind::Call(k, _)
            | ExprKind::CallEntry { func: k, .. }
            | ExprKind::CallDirectThis(k, ..)
            | ExprKind::CallChecked { func: k, .. }
            | ExprKind::ArraySort { func: k, .. } => *k,
            _ => return,
        };
        if !out.contains(&k) {
            out.push(k);
        }
    });
    out
}

/// What a function's code knows about its locals and records, for
/// `node`.
struct Cx<'a> {
    m: &'a Module,
    table: &'a Summaries,
    f: &'a Func,
    /// Per local: the records of the objects a speculative region's guard
    /// says it holds (for `SlotLoad` / `SlotStore`).
    records: std::collections::HashMap<LocalId, Vec<usize>>,
}

impl Cx<'_> {
    /// The properties a speculative region's slot `slot` of the object in
    /// local `l` may be.
    fn slot_atoms(&self, l: LocalId, slot: u32) -> Option<Vec<AtomId>> {
        let rs = self.records.get(&l)?;
        rs.iter().map(|&r| self.m.records.get(r).and_then(|rec| rec.fields.get(slot as usize).copied())).collect()
    }

    fn boxed(&self, l: LocalId) -> bool {
        l != THIS_LOCAL && self.f.boxed.get(l).copied().unwrap_or(false)
    }
}

/// The builtin methods native code runs inline while they are the
/// originals (see the code generator's `Method`): what they do then.
/// Otherwise they are called through the engine.
fn builtin_method(name: &str) -> Option<Effects> {
    let mut e = Effects::default();
    match name {
        "charCodeAt" => {}
        "push" => {
            e.reads.elements = true;
            e.writes.elements = true;
            e.allocates = true;
            e.throws = throws::MEMORY;
        }
        "slice" | "substring" | "indexOf" | "includes" | "startsWith" | "endsWith" | "lastIndexOf" => {
            e.allocates = true;
            e.throws = throws::MEMORY;
        }
        "get" | "has" => e.reads.collections = true,
        "set" | "add" => {
            e.reads.collections = true;
            e.writes.collections = true;
            e.allocates = true;
            e.throws = throws::MEMORY;
        }
        "stringify" => {
            e.reads = Access::top();
            e.allocates = true;
            e.throws = throws::MEMORY;
        }
        _ => return None,
    }
    Some(e)
}

/// What node `e` itself does (not its subexpressions, nor the statements
/// of a body it holds, which are visited on their own).
fn node(e: &Expr, cx: &Cx) -> Effects {
    use ExprKind as K;
    let typed = |x: &Expr| x.ty != Ty::Any;
    let boxed_number = |x: &Expr| matches!(&x.kind, K::Box(n) if n.ty.is_number());
    match &e.kind {
        K::Num(_)
        | K::Bool(_)
        | K::Undefined
        | K::Null
        | K::Str(_)
        | K::This
        | K::Void(_)
        | K::Seq(_)
        | K::Cond(..)
        | K::Logical(..)
        | K::Unary(..)
        | K::Binary(..)
        | K::Compare(..)
        | K::Truthy(_)
        | K::Box(_)
        | K::IntOf(_)
        | K::Lazy(_)
        | K::Inline { .. }
        | K::CurrentFunction
        | K::HomeValue(_)
        | K::RegionLeave(_)
        | K::PlainAddOk(_)
        | K::IsObjectCreate(_)
        | K::ProtoIs { .. }
        | K::ProtoMethodIs { .. }
        | K::StrLen { .. }
        | K::StrCharCode { .. } => Effects::default(),
        K::NumArith(_, a, b) => {
            if typed(a) && typed(b) {
                Effects::default()
            } else {
                // (ToNumber of the JS value: a BigInt throws.)
                let mut x = Effects::engine();
                x.throws |= throws::TYPE;
                x
            }
        }
        K::Local(l) | K::SetLocal(l, _) | K::InitLocal(l, _) | K::Update { local: l, .. } => {
            if cx.boxed(*l) {
                let a = Access { boxes: true, ..Access::default() };
                let mut x = Effects { throws: throws::REFERENCE, ..Effects::default() };
                if matches!(e.kind, K::Local(_)) {
                    x.reads = a;
                } else {
                    x.reads = a.clone();
                    x.writes = a;
                }
                x
            } else {
                Effects::default()
            }
        }
        K::Math(_, args) => {
            if args.iter().all(typed) {
                Effects::default()
            } else {
                // (Converting a JS value to a number.)
                let mut x = Effects::engine();
                x.throws |= throws::TYPE;
                x
            }
        }
        K::AnyUnary(op, _) => {
            if *op == AnyUnOp::Typeof {
                Effects::default()
            } else {
                let mut x = Effects::engine();
                x.throws |= throws::TYPE | throws::MEMORY;
                x.allocates = true;
                x
            }
        }
        K::AnyBinary(_, a, b) => {
            if boxed_number(a) && boxed_number(b) {
                Effects::default()
            } else {
                let mut x = Effects::engine();
                x.throws |= throws::TYPE | throws::MEMORY;
                x.allocates = true;
                x
            }
        }
        K::AnyCompare(op, a, b) => match op {
            AnyCmpOp::StrictEq | AnyCmpOp::StrictNe => Effects::default(),
            _ if boxed_number(a) && boxed_number(b) => Effects::default(),
            AnyCmpOp::In => {
                let mut x = Effects::engine();
                x.reads = Access::top();
                x.throws |= throws::TYPE;
                x
            }
            _ => {
                let mut x = Effects::engine();
                x.throws |= throws::TYPE;
                x
            }
        },
        K::Cell(c) => Effects {
            reads: Access { cells: [*c].into(), ..Access::default() },
            throws: throws::REFERENCE,
            ..Effects::default()
        },
        K::SetCell(c, _) | K::InitCell(c, _) => {
            let a = Access { cells: [*c].into(), ..Access::default() };
            Effects {
                reads: a.clone(),
                writes: a,
                throws: if matches!(e.kind, K::SetCell(..)) { throws::REFERENCE } else { 0 },
                ..Effects::default()
            }
        }
        K::Global(a) | K::GlobalOrUndefined(a) => {
            let mut x = Effects::engine();
            x.reads.globals.insert(*a);
            x.throws |= throws::REFERENCE;
            x
        }
        K::SetGlobal(a, _) => {
            let mut x = Effects::engine();
            x.writes.globals.insert(*a);
            x.reads.globals.insert(*a);
            x.allocates = true;
            x.throws |= throws::REFERENCE | throws::TYPE | throws::MEMORY;
            x
        }
        K::GetProp(_, a) | K::ShapeGet { atom: a, .. } | K::SuperGetAtom(_, a) => {
            let mut x = Effects::engine();
            x.reads = Access::fields([*a]);
            x.throws |= throws::NULLISH;
            x
        }
        K::Length(_, a) => {
            let mut x = Effects::engine();
            x.reads = Access::fields([*a]);
            x.reads.elements = true;
            x.throws |= throws::NULLISH;
            x
        }
        K::SetProp(_, a, _) | K::ShapeSet { atom: a, .. } | K::DefineField(_, a, _) => {
            let mut x = Effects::engine();
            x.reads = Access::fields([*a]);
            if cx.m.atoms.get(*a).is_some_and(|s| s == "__proto__") {
                // (A prototype changes what every property reads.)
                x.writes = Access::top();
            } else {
                x.writes = Access::fields([*a]);
            }
            x.allocates = true;
            x.throws |= throws::NULLISH | throws::TYPE | throws::MEMORY;
            x
        }
        K::DeleteProp(_, a) => {
            let mut x = Effects::engine();
            x.writes = Access::fields([*a]);
            x.throws |= throws::NULLISH | throws::TYPE;
            x
        }
        K::TypedGet(..) | K::RegionTypedGet { .. } | K::ArrLen { .. } => {
            Effects::reads(Access { elements: true, ..Access::default() })
        }
        K::TypedSet(..) | K::RegionTypedSet { .. } => {
            let a = Access { elements: true, ..Access::default() };
            Effects { reads: a.clone(), writes: a, ..Effects::default() }
        }
        K::SlotLoad { obj, slot } => match cx.slot_atoms(*obj, *slot) {
            Some(atoms) => Effects::reads(Access::fields(atoms)),
            None => Effects::reads(Access::top()),
        },
        K::SlotStore { obj, slot, .. } => {
            let a = match cx.slot_atoms(*obj, *slot) {
                Some(atoms) => Access::fields(atoms),
                None => Access::top(),
            };
            Effects { reads: a.clone(), writes: a, ..Effects::default() }
        }
        K::SpecGuard { .. } | K::ElemGuard { .. } => Effects::reads(Access { elements: true, ..Access::top() }),
        K::RegionPush { .. } => {
            let a = Access { elements: true, ..Access::default() };
            Effects { reads: a.clone(), writes: a, allocates: true, throws: throws::MEMORY, ..Effects::default() }
        }
        K::StructGet(k, o, fi) => {
            let mut x = Effects::reads(Access { slots: [(*k, *fi)].into(), ..Access::default() });
            if !matches!(o.ty, Ty::Struct(_)) {
                x.throws = throws::NULLISH;
            }
            x
        }
        K::StructSet(k, o, fi, _) => {
            let a = Access { slots: [(*k, *fi)].into(), ..Access::default() };
            let mut x = Effects { reads: a.clone(), writes: a, ..Effects::default() };
            // (A JS value stored may be converted: TypeError if not of the
            // field's type.)
            x.throws = throws::TYPE | if matches!(o.ty, Ty::Struct(_)) { 0 } else { throws::NULLISH };
            x
        }
        K::StructArrGet(..) | K::StructArrLen(_) => Effects::reads(Access { elements: true, ..Access::default() }),
        K::StructArrSet(..) | K::StructArrPush(..) => {
            let a = Access { elements: true, ..Access::default() };
            Effects {
                reads: a.clone(),
                writes: a,
                allocates: true,
                throws: throws::TYPE | throws::MEMORY,
                ..Effects::default()
            }
        }
        K::Object(_)
        | K::NewInstance { .. }
        | K::NewInstanceFull { .. }
        | K::FrameObject { .. }
        | K::Array(_)
        | K::Closure(..)
        | K::StructNew { .. }
        | K::NullObject
        | K::RegExp(..)
        | K::BigInt(_) => Effects::allocating(),
        K::Call(k, _) | K::CallDirectThis(k, ..) => cx.table.call(*k),
        K::CallEntry { func, callee, .. } => {
            let mut x = cx.table.call(*func);
            // (The promise, and the function object when the entry
            // declines.)
            x.join(&Effects::allocating());
            if callee.is_some() {
                x.join(&Effects::engine());
            }
            x
        }
        K::CallChecked { func, .. } => {
            let mut x = cx.table.call(*func);
            x.join(&Effects::engine());
            x
        }
        K::CallMethod(_, Key::Atom(a), _) => {
            // The method read, then a call: of a builtin native code runs
            // inline (while it is the original), else through the engine.
            let mut x = Effects::engine();
            x.reads = Access::fields([*a]);
            x.throws |= throws::NULLISH | throws::TYPE;
            if let Some(b) = cx.m.atoms.get(*a).and_then(|s| builtin_method(s)) {
                x.join(&b);
            }
            x
        }
        K::Predicted { .. } => Effects::default(),
        K::ArrayLoop { .. } => {
            // The loop reads elements (and may make the result array); its
            // body is visited; else the method is called.
            let mut x = Effects::engine();
            x.reads.elements = true;
            x.reads.fields.extend(cx.m.atoms.iter().position(|s| s == "length"));
            x.allocates = true;
            x.throws |= throws::NULLISH | throws::TYPE | throws::MEMORY;
            x
        }
        K::ArraySort { func, .. } => {
            let mut x = cx.table.call(*func);
            x.join(&Effects::engine());
            x.reads.elements = true;
            x.writes.elements = true;
            x.allocates = true;
            x.throws |= throws::NULLISH | throws::TYPE | throws::MEMORY;
            x
        }
        K::ArrayPush(..) => {
            let mut x = Effects::engine();
            x.reads.elements = true;
            x.writes.elements = true;
            x.allocates = true;
            x.throws |= throws::TYPE | throws::MEMORY;
            x
        }
        K::PrivateField(_, s) | K::PrivateIn(_, s) => Effects {
            reads: Access { privates: [*s].into(), ..Access::default() },
            throws: throws::TYPE,
            ..Effects::default()
        },
        K::PrivateSetField(_, s, _) | K::PrivateDefine(_, s, _) => {
            let a = Access { privates: [*s].into(), ..Access::default() };
            Effects {
                reads: a.clone(),
                writes: a,
                allocates: true,
                throws: throws::TYPE | throws::MEMORY,
                ..Effects::default()
            }
        }
        K::CheckObject(_) | K::NonNull(..) => Effects { throws: throws::TYPE, ..Effects::default() },
        // `o[k]`: any property or element (a key converted, getters).
        K::GetElem(..) | K::ForInGet(..) => {
            let mut x = Effects::engine();
            x.reads = Access { elements: true, ..Access::top() };
            x.throws |= throws::NULLISH | throws::TYPE;
            x
        }
        K::SetElem(..) | K::DeleteElem(..) => {
            let mut x = Effects::engine();
            x.reads = Access::top();
            x.writes = Access::top();
            x.allocates = true;
            x.throws |= throws::NULLISH | throws::TYPE | throws::MEMORY;
            x
        }
        // Calls of code native code does not call directly: through the
        // engine (what they read or write is the engine's, see the module
        // comment).
        K::CallValue(..)
        | K::CallThis(..)
        | K::CallMethod(..)
        | K::New(..)
        | K::CallSpread(..)
        | K::NewSpread(..)
        | K::SuperNew { .. }
        | K::SuperNewSpread { .. } => {
            let mut x = Effects::engine();
            x.allocates = true;
            x.throws = throws::ALL;
            x
        }
        // Iteration: of arrays read directly, else the iterator's methods.
        K::IterOpen(..)
        | K::IterStep(..)
        | K::IterClose(..)
        | K::IterCloseQuiet(_)
        | K::ForInIterator(..)
        | K::ForInStep(..)
        | K::AsyncIterator(_)
        | K::AsyncIterResult(..) => {
            let mut x = Effects::engine();
            x.reads = Access { elements: true, ..Access::top() };
            x.allocates = true;
            x.throws |= throws::NULLISH | throws::TYPE | throws::MEMORY;
            x
        }
        K::KeysLength(..) | K::ObjectParts(_) | K::ArrayParts(_) | K::ObjectRest(..) => {
            let mut x = Effects::engine();
            x.reads = Access { elements: true, ..Access::top() };
            x.allocates = true;
            x.throws |= throws::NULLISH | throws::TYPE | throws::MEMORY;
            x
        }
        K::ObjectCreate(..) => {
            let mut x = Effects::engine();
            x.allocates = true;
            x.throws |= throws::TYPE | throws::MEMORY;
            x
        }
        K::ToPropKey(_) => {
            let mut x = Effects::engine();
            x.throws |= throws::TYPE;
            x
        }
        K::ToObject(_) => Effects { allocates: true, throws: throws::TYPE | throws::MEMORY, ..Effects::default() },
        _ => Effects::top(),
    }
}

/// What statement `s` itself does (not its expressions, visited on their
/// own, nor the statements it holds).
fn stmt(s: &Stmt) -> Effects {
    match s {
        Stmt::Throw(_) => Effects { throws: throws::EXPLICIT, ..Effects::default() },
        // (Whatever runs before the frame resumes: everything.)
        Stmt::Await { .. } | Stmt::Yield { .. } => {
            let mut x = Effects::top();
            x.suspends = true;
            x
        }
        Stmt::NewBoxes(_) => Effects::allocating(),
        Stmt::Loop { per_iteration, .. } if !per_iteration.is_empty() => Effects::allocating(),
        // (Closing an iterator calls its `return`.)
        Stmt::IterGuard { .. } => Effects::top(),
        _ => Effects::default(),
    }
}

/// The effects of statements `stmts` of function `f` (calls by `table`).
pub fn of_stmts(m: &Module, table: &Summaries, f: &Func, stmts: &[Stmt]) -> Effects {
    let cx = context(m, table, f);
    let mut out = Effects::default();
    stmt_lists(stmts, &mut |list| {
        for s in list {
            out.join(&stmt(s));
        }
    });
    visit_stmts(stmts, &mut |e| {
        out.join(&node(e, &cx));
        // Statements in the bodies of inlined calls.
        if let ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } | ExprKind::ArrayLoop { body, .. } =
            &e.kind
        {
            stmt_lists(body, &mut |list| {
                for s in list {
                    out.join(&stmt(s));
                }
            });
        }
    });
    out
}

/// The effects of expression `e` of function `f`, its subexpressions
/// included.
pub fn of_expr(m: &Module, table: &Summaries, f: &Func, e: &Expr) -> Effects {
    of_stmts(m, table, f, std::slice::from_ref(&Stmt::Expr(e.clone())))
}

fn context<'a>(m: &'a Module, table: &'a Summaries, f: &'a Func) -> Cx<'a> {
    let mut records: std::collections::HashMap<LocalId, Vec<usize>> = Default::default();
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::SpecGuard { objs, derived, .. } => {
            for &(l, r) in objs {
                records.entry(l).or_default().push(r);
            }
            for &(l, _, _, r) in derived {
                records.entry(l).or_default().push(r);
            }
        }
        ExprKind::ElemGuard { elems, derived, .. } => {
            for &(l, _, _, r) in elems {
                records.entry(l).or_default().push(r);
            }
            for &(l, _, _, r) in derived {
                records.entry(l).or_default().push(r);
            }
        }
        _ => {}
    });
    Cx { m, table, f, records }
}

/// The function's own effects (its body; calls by `table`).
fn summarize(m: &Module, table: &Summaries, g: usize) -> Effects {
    let f = func_at(m, g);
    let mut e = of_stmts(m, table, f, &f.body);
    if f.suspends() {
        e.suspends = true;
    }
    // A generator's or async function's body runs from the engine's jobs
    // or `next` as well; its first part runs in the call.
    e
}

/// The summaries of every function of `m`.
pub fn summaries(m: &Module) -> Summaries {
    let n = m.funcs.len() + m.closures.len();
    let succ: Vec<Vec<usize>> =
        (0..n).map(|g| direct_callees(func_at(m, g)).into_iter().filter(|&k| k < n).collect()).collect();
    let comps = crate::escape::components(&succ);
    // Depth: bounded for functions outside cycles, from their callees'.
    let mut depth: Vec<Option<u32>> = vec![None; n];
    for comp in &comps {
        let recursive = comp.len() > 1 || succ[comp[0]].contains(&comp[0]);
        if recursive {
            continue;
        }
        let g = comp[0];
        let mut d = Some(1u32);
        for &k in &succ[g] {
            d = match (d, depth[k]) {
                (Some(a), Some(b)) => Some(a.max(b + 1)),
                _ => None,
            };
        }
        depth[g] = d;
    }
    let mut table = Summaries { funcs: vec![Effects::default(); n], depth, nothrow: vec![false; n] };
    for comp in &comps {
        loop {
            let mut changed = false;
            for &g in comp {
                let e = summarize(m, &table, g);
                if e != table.funcs[g] {
                    table.funcs[g] = e;
                    changed = true;
                }
                let nt = table.funcs[g].throws == 0 && table.depth[g].is_some_and(|d| d <= NOTHROW_DEPTH);
                if nt != table.nothrow[g] {
                    table.nothrow[g] = nt;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
    if std::env::var_os("LUMINOUS_DEBUG_EFFECTS").is_some() {
        for g in 0..n {
            let f = func_at(m, g);
            let e = &table.funcs[g];
            eprintln!(
                "effects {g} {}: throws {:#x} engine {} alloc {} suspends {} depth {:?} nothrow {} reads {} writes {}",
                f.name,
                e.throws,
                e.engine,
                e.allocates,
                e.suspends,
                table.depth[g],
                table.nothrow[g],
                show(m, &e.reads),
                show(m, &e.writes)
            );
        }
    }
    table
}

/// An access set, with names (for `LUMINOUS_DEBUG_EFFECTS`).
pub fn show(m: &Module, a: &Access) -> String {
    if a.all {
        return "*".into();
    }
    let mut parts: Vec<String> = a.fields.iter().map(|&f| format!(".{}", m.atoms[f])).collect();
    parts.extend(a.slots.iter().map(|(k, f)| format!("slot{k}.{f}")));
    parts.extend(a.cells.iter().map(|&c| format!("cell:{}", m.cells[c])));
    parts.extend(a.globals.iter().map(|&g| format!("global:{}", m.atoms[g])));
    parts.extend(a.privates.iter().map(|&p| format!("#{p}")));
    if a.elements {
        parts.push("[elements]".into());
    }
    if a.collections {
        parts.push("[collections]".into());
    }
    if a.boxes {
        parts.push("[boxes]".into());
    }
    format!("{{{}}}", parts.join(" "))
}
