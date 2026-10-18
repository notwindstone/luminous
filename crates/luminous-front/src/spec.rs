//! Speculative numeric regions, as JIT compilers speculate on what fields
//! hold: a run of statements that only computes numbers from the number
//! fields of objects of record types (`ShapeGet` / `ShapeSet` of fields
//! annotated `number`), number locals, JS-valued locals and constants (with
//! comparisons, `if`, `?:`, `&&`, `||`, `return`, loops, the characters and
//! lengths of strings, and calls of leaf functions on numbers among them)
//! gets a copy
//! where those fields are read as numbers (`SlotLoad`) and kept in `F64`
//! locals once written, as JS-valued locals are. A guard before it
//! (`SpecGuard`) checks the objects' shapes, that they are distinct objects
//! if fields are written, that the fields and the JS-valued locals it reads
//! hold numbers, and that the strings it reads are flat strings (with
//! `charCodeAt` the builtin); the copy then runs as plain float code and stores the
//! fields and locals it changed where it ends or returns (`SlotStore`).
//! When the guard fails, the statements run as they are. Nothing in the
//! copy can run code, so what the guard checked holds throughout, and when
//! the stores happen cannot be observed (one that appends to arrays,
//! `RegionPush`, may allocate, and so throw out of memory: it defers no
//! store, and is not in `try`).
//!
//! Methods take part: `this` is an object as locals are, a predicted call
//! of a class's method (`Predicted`) runs its body in the copy (the guard
//! checks that the receiver's prototype is the class's, still with that
//! method), objects held by guarded objects' fields are guarded too, and
//! the scalar-replaced objects of `virtualize_instances` (`undefined`, a
//! flag, and a local per field) keep their fields in `F64` locals.

use crate::ir::*;
use std::collections::{HashMap, HashSet};

/// A run needs this many field accesses, and with the JS-valued
/// arithmetic it does, this much work, to be worth its guard and copy;
/// one that stores no field needs this much arithmetic (a few reads of
/// int32 fields cost about as much through the guard, and converting
/// them to doubles lengthens the dependency chains of short code such as
/// comparators).
const MIN_ACCESSES: usize = 2;
const MIN_WORK: usize = 4;
const MIN_PURE_WORK: usize = 6;

pub fn speculate_numbers(m: &mut Module) {
    let nums: Vec<Vec<bool>> = m.records.iter().map(|r| r.nums.clone()).collect();
    let rets: Vec<Ty> = m.funcs.iter().chain(&m.closures).map(|f| f.ret).collect();
    let methods = Methods {
        class: m.funcs.iter().chain(&m.closures).map(|f| f.class).collect(),
        params: m.funcs.iter().chain(&m.closures).map(|f| f.params.len()).collect(),
        records: m.class_records.clone(),
        fields: m.records.iter().map(|r| r.fields.clone()).collect(),
    };
    // Native functions a region may call: they can neither throw nor call.
    let leaves: Vec<Option<(Vec<Ty>, Ty)>> =
        m.funcs.iter().map(|f| nothrow_leaf(f).then(|| (f.params.clone(), f.ret))).collect();
    let atoms = std::mem::take(&mut m.atoms);
    for f in m.funcs.iter_mut().chain(m.closures.iter_mut()) {
        if f.is_async || f.is_generator {
            continue;
        }
        let refs = mentions(&f.body, f.locals.len());
        let uses = local_uses(&f.body, f.locals.len());
        let mut texts = HashSet::new();
        visit_stmts(&f.body, &mut |e| {
            if let ExprKind::CallMethod(s, Key::Atom(a), _) = &e.kind
                && let ExprKind::Local(l) = s.kind
                && atoms[*a] == "charCodeAt"
            {
                texts.insert(l);
            }
        });
        let mut body = std::mem::take(&mut f.body);
        let ret = f.ret;
        let this = f.has_this;
        let exits = !has_try(&body);
        let mut cx = Cx {
            f,
            nums: &nums,
            refs,
            uses,
            atoms: &atoms,
            leaves: &leaves,
            methods: &methods,
            this,
            next_loop: FIRST_COPY_LOOP,
            allocs: true,
            texts,
            exits,
        };
        cx.list(&mut body, Some(ret));
        cx.allocs = false;
        cx.exits = false;
        // The statement lists inside expressions (inlined bodies, whose
        // `return`s give the expression's value; a predicted call's `this`
        // is its receiver).
        visit_stmts_mut(&mut body, &mut |e| match &mut e.kind {
            ExprKind::Inline { body, .. } => {
                cx.this = false;
                cx.list(body, Some(e.ty));
            }
            // (Its `return`s give the callee's value, boxed after it.)
            ExprKind::Predicted { body, func, .. } => {
                cx.this = true;
                cx.list(body, rets.get(*func).copied());
            }
            ExprKind::ArrayLoop { body, func, .. } => {
                cx.this = false;
                cx.list(body, rets.get(*func).copied())
            }
            _ => {}
        });
        cx.f.body = body;
    }
    m.atoms = atoms;
}

/// The ids of the loops in regions' copies: above any the front end makes.
const FIRST_COPY_LOOP: LoopId = 1 << 40;

/// How many times each local is named in `stmts`.
fn mentions(stmts: &[Stmt], n: usize) -> Vec<usize> {
    let mut refs = vec![0; n];
    let mut add = |l: LocalId| {
        if l < refs.len() {
            refs[l] += 1;
        }
    };
    visit_stmts(stmts, &mut |e| match &e.kind {
        ExprKind::Local(l)
        | ExprKind::SetLocal(l, _)
        | ExprKind::InitLocal(l, _)
        | ExprKind::Update { local: l, .. }
        | ExprKind::ArrayPush(l, _)
        | ExprKind::AsyncIterResult(_, l)
        | ExprKind::ForInIterator(_, l)
        | ExprKind::IterCloseQuiet(l) => add(*l),
        ExprKind::IterOpen(_, a, b, _) | ExprKind::IterClose(a, b) | ExprKind::ForInGet(a, b, ..) => {
            add(*a);
            add(*b);
        }
        ExprKind::ForInStep(a, b, c) => {
            add(*a);
            add(*b);
            add(*c);
        }
        ExprKind::IterStep(a, b) => {
            add(*a);
            b.iter().for_each(|&b| add(b));
        }
        // Not the locals of inlined bodies, which are only released after
        // them (a value no one reads may be stale).
        _ => {}
    });
    stmt_mentions(stmts, &mut add);
    refs
}

/// The locals statements name outside their expressions.
fn stmt_mentions(stmts: &[Stmt], add: &mut impl FnMut(LocalId)) {
    for s in stmts {
        match s {
            Stmt::If(_, a, b) => {
                stmt_mentions(a, add);
                stmt_mentions(b, add);
            }
            Stmt::Loop { body, per_iteration, .. } => {
                per_iteration.iter().for_each(|&l| add(l));
                stmt_mentions(body, add);
            }
            Stmt::Block(body) => stmt_mentions(body, add),
            Stmt::IterGuard { iter, next, body, .. } => {
                add(*iter);
                add(*next);
                stmt_mentions(body, add);
            }
            Stmt::Switch { disc, cases, .. } => {
                add(*disc);
                cases.iter().for_each(|(_, b)| stmt_mentions(b, add));
            }
            Stmt::Try { body, catch, finally, exc } => {
                exc.iter().for_each(|&l| add(l));
                stmt_mentions(body, add);
                if let Some((param, c)) = catch {
                    param.iter().for_each(|&l| add(l));
                    stmt_mentions(c, add);
                }
                finally.iter().for_each(|c| stmt_mentions(c, add));
            }
            Stmt::NewBoxes(ls) => ls.iter().for_each(|&l| add(l)),
            _ => {}
        }
    }
}

/// What predicted method calls need to know of the module.
struct Methods {
    /// Per function (module functions, then closures): its class, and
    /// whether it is static; its number of parameters.
    class: Vec<Option<(usize, bool)>>,
    params: Vec<usize>,
    /// Per class, its instances' record; per record, its fields.
    records: Vec<Option<usize>>,
    fields: Vec<Vec<AtomId>>,
}

struct Cx<'a> {
    f: &'a mut Func,
    nums: &'a [Vec<bool>],
    atoms: &'a [String],
    methods: &'a Methods,
    /// Whether the statements being made into regions may read `this`
    /// (a method's, or a predicted call's receiver in its body).
    this: bool,
    /// Per module function: its parameter and return types if it is a
    /// leaf (see `nothrow_leaf`).
    leaves: &'a [Option<(Vec<Ty>, Ty)>],
    next_loop: LoopId,
    /// Whether regions made now may allocate (`RegionPush`): in the
    /// function's own statements, outside `try` (an allocation may throw,
    /// out of memory, where nothing may observe what the region defers).
    allocs: bool,
    /// The locals whose characters the function reads (`charCodeAt`): their
    /// `length` is a string's.
    texts: HashSet<LocalId>,
    /// How many times each local is named in the function (before any
    /// region is made), by `mentions` and by `local_uses`.
    refs: Vec<usize>,
    uses: Vec<usize>,
    /// Whether a `return` in the statements being made into regions leaves
    /// the function with nothing reading its locals after it (no `finally`;
    /// not an inlined body, whose caller goes on).
    exits: bool,
}

/// Whether `stmts` have a `try` statement.
fn has_try(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        Stmt::Try { .. } => true,
        Stmt::If(_, a, b) => has_try(a) || has_try(b),
        Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => has_try(body),
        Stmt::Switch { cases, .. } => cases.iter().any(|(_, b)| has_try(b)),
        _ => false,
    })
}

/// What a run of statements needs and changes.
#[derive(Clone, Default)]
struct Region {
    /// What a `return` gives in these statements (`None`: no `return`).
    ret: Option<Ty>,
    /// The objects whose fields it uses, with their records.
    objs: Vec<(LocalId, usize)>,
    /// Fields used: object, slot, and once written, their `F64` local and
    /// whether the last value written is the result of arithmetic.
    fields: Vec<(LocalId, u32, Option<(LocalId, bool)>)>,
    /// JS-valued locals whose values the guard loads into their copies
    /// (read before they are surely assigned).
    nums: Vec<(LocalId, LocalId)>,
    /// Every JS-valued local used, and its `F64` copy.
    copies: HashMap<LocalId, LocalId>,
    /// The JS-valued locals assigned, in order.
    assigned: Vec<LocalId>,
    /// The JS-valued locals surely assigned here.
    defined: HashSet<LocalId>,
    /// Locals that must hold flat strings, and whether `charCodeAt` is used.
    strs: Vec<LocalId>,
    char_code_at: bool,
    /// The loops in it, and their ids in the copy.
    loops: HashMap<LoopId, LoopId>,
    /// Objects held by fields of guarded objects: `(name, of, slot,
    /// record)` (see `SpecGuard`); method calls run inline; class bindings
    /// read.
    derived: Vec<(LocalId, LocalId, u32, usize)>,
    methods: Vec<(Option<LocalId>, usize, AtomId, usize)>,
    cells: Vec<CellId>,
    /// `Bool` locals with a value known here (the flags of scalar-replaced
    /// objects).
    consts: HashMap<LocalId, bool>,
    /// JS-valued locals holding `undefined` here (a scalar-replaced
    /// object's own local, and its fields' before they are set).
    undef: HashSet<LocalId>,
    /// The receivers of the predicted calls running inline (innermost
    /// last): what `this` is in their bodies.
    receivers: Vec<LocalId>,
    /// Parameters of those calls (and locals of inlined bodies) used as
    /// objects: their arguments (initializers), and the receivers where
    /// those are read.
    aliases: HashMap<LocalId, (Expr, Vec<LocalId>)>,
    /// Whether every path so far has left (returned, or jumped).
    returned: bool,
    accesses: usize,
    work: usize,
    /// How many of the region's loops the statements being copied are in.
    depth: usize,
    /// The loop (the region's only statement) whose iterations read
    /// objects from arrays, while its body is copied: see `ElemLoop`.
    elem_loop: Option<ElemLoop>,
    /// Arrays whose elements its iterations read, the element objects
    /// `(obj, arr, idx, record)`, the objects their fields hold, and the
    /// fields of those read or written (see `ExprKind::ElemGuard`).
    arrays: Vec<LocalId>,
    elems: Vec<(LocalId, LocalId, LocalId, usize)>,
    /// Elements its iterations read as numbers: `(local, arr, idx)`; of
    /// typed arrays, `(arr, idx)` (read in place).
    elem_nums: Vec<(LocalId, LocalId, LocalId)>,
    elem_bounds: Vec<(LocalId, LocalId)>,
    /// Typed arrays it writes.
    typed_writes: Vec<LocalId>,
    elem_derived: Vec<(LocalId, LocalId, u32, usize)>,
    elem_fields: Vec<(LocalId, u32)>,
    /// Whether an element's field is written.
    elem_writes: bool,
    /// The `Bool` local set when an iteration's objects are not what the
    /// region needs (it then leaves, and the original loop goes on from
    /// that iteration), once the loop reads elements.
    bailed: Option<LocalId>,
    /// Whether the statement being copied is the region's first (a loop
    /// that reads elements must be its only one).
    may_elems: bool,
    /// Records whose shape (once known) must have a class's prototype:
    /// `(record, class)`, for methods run inline on elements.
    protos: Vec<(usize, usize)>,
    /// How many method bodies with statements run inline (`Inline`) the
    /// statements being copied are in: their `return`s give the call's
    /// value.
    inline: usize,
    /// How many branches of `if`s the statements being copied are in (a
    /// loop that reads elements is in none), and whether such a loop has
    /// been copied (nothing may follow it).
    branches: usize,
    closed: bool,
    /// Arrays it appends to (`RegionPush`), and whether it does.
    appends: Vec<LocalId>,
    allocates: bool,
    /// While a loop in the body of the loop that reads elements reads
    /// elements too (see `Nest`): the outer loop's.
    nest: Option<Box<Nest>>,
    /// The original statements after each statement being copied, in the
    /// body of the loop that reads elements (see `Nest`).
    rest: Vec<Vec<Stmt>>,
    /// Per loop of the region being copied, the locals it assigns; the
    /// jumps out of the region from inside them (see `JumpOut`).
    loop_assigned: Vec<HashSet<LocalId>>,
    jumps: Vec<JumpOut>,
}

/// A jump out of a region from inside its loops (`ExprKind::RegionLeave`):
/// what leaves there is known once the whole region is (a later statement
/// of the loop's body may have assigned locals and fields in an earlier
/// iteration). The locals surely assigned there, those undefined there,
/// and those the enclosing loops assign.
#[derive(Clone)]
struct JumpOut {
    defined: HashSet<LocalId>,
    undef: HashSet<LocalId>,
    looped: HashSet<LocalId>,
    /// The jump.
    jump: Stmt,
}

/// What a loop that reads elements has: the elements its iterations read
/// (see `Region::elems`).
#[derive(Clone, Default)]
struct ElemSet {
    elems: Vec<(LocalId, LocalId, LocalId, usize)>,
    nums: Vec<(LocalId, LocalId, LocalId)>,
    bounds: Vec<(LocalId, LocalId)>,
    derived: Vec<(LocalId, LocalId, u32, usize)>,
    fields: Vec<(LocalId, u32)>,
    writes: bool,
}

impl ElemSet {
    fn has(&self, o: LocalId) -> bool {
        self.elems.iter().any(|e| e.0 == o) || self.derived.iter().any(|d| d.0 == o)
    }
}

/// A loop in the body of the region's loop that reads elements (its only
/// such loop, outside branches), reading elements in its own iterations
/// too, each checked by its own guard: where one fails, the region leaves,
/// and the original code goes on from that inner iteration (the rest of
/// the inner loop, the rest of the outer iteration, the outer loop from the
/// next iteration). The outer loop's state, while the inner one is copied.
#[derive(Clone)]
struct Nest {
    outer_loop: ElemLoop,
    outer: ElemSet,
}

/// The loop whose iterations read objects from arrays.
#[derive(Clone)]
struct ElemLoop {
    /// The locals its body assigns, and whether its body (not its test or
    /// update) is being copied, outside any inner loop.
    assigned: HashSet<LocalId>,
    in_body: bool,
    /// The locals each iteration declares (`ir::iteration_locals`): dead
    /// where an iteration starts.
    iteration: HashSet<LocalId>,
    /// How many of the region's loops its body is in (1, or 2 for a loop
    /// in the body of another: see `Nest`).
    level: usize,
    /// Its id, its id in the copy and its update (the original); where its
    /// body's statements start in `Region::rest`.
    id: LoopId,
    copy: LoopId,
    update: Option<Expr>,
    rest_base: usize,
}

impl Cx<'_> {
    fn new_bool_local(&mut self) -> LocalId {
        let l = self.new_local();
        self.f.locals[l] = Ty::Bool;
        l
    }

    fn new_local(&mut self) -> LocalId {
        self.f.locals.push(Ty::F64);
        self.f.boxed.push(false);
        self.f.local_names.push(None);
        self.f.locals.len() - 1
    }

    fn truncate_locals(&mut self, n: usize) {
        self.f.locals.truncate(n);
        self.f.boxed.truncate(n);
        self.f.local_names.truncate(n);
    }

    /// Make the regions of `list` and of the statement lists in it (not of
    /// those in its expressions), where `return` gives a `ret`.
    fn list(&mut self, list: &mut Vec<Stmt>, ret: Option<Ty>) {
        let old = std::mem::take(list);
        let fresh = || Region { ret, ..Region::default() };
        let mut run: Vec<Stmt> = Vec::new();
        let mut fast: Vec<Stmt> = Vec::new();
        let mut r = fresh();
        let mut base = self.f.locals.len();
        for mut s in old {
            let saved = r.clone();
            let n = self.f.locals.len();
            r.may_elems = run.is_empty();
            if !r.returned
                && let Some(x) = r.stmt(&s, self)
            {
                run.push(s);
                fast.push(x);
                // (A loop that reads elements is a region of its own.)
                if r.bailed.is_some() {
                    self.flush(list, &mut run, &mut fast, std::mem::replace(&mut r, fresh()), base);
                    base = self.f.locals.len();
                }
                continue;
            }
            self.truncate_locals(n);
            r = saved;
            let alone = run.is_empty();
            self.flush(list, &mut run, &mut fast, std::mem::replace(&mut r, fresh()), base);
            base = self.f.locals.len();
            // It may start a run (a loop that reads elements, a region on
            // its own).
            if !alone {
                r.may_elems = true;
                if let Some(x) = r.stmt(&s, self) {
                    run.push(s);
                    fast.push(x);
                    if r.bailed.is_some() {
                        self.flush(list, &mut run, &mut fast, std::mem::replace(&mut r, fresh()), base);
                        base = self.f.locals.len();
                    }
                    continue;
                }
                self.truncate_locals(base);
                r = fresh();
            }
            self.nested(&mut s, ret);
            list.push(s);
            base = self.f.locals.len();
        }
        self.flush(list, &mut run, &mut fast, r, base);
    }

    /// The statement lists in `s`.
    fn nested(&mut self, s: &mut Stmt, ret: Option<Ty>) {
        match s {
            Stmt::If(c, a, b) => {
                if !matches!(c.kind, ExprKind::SpecGuard { .. }) {
                    self.list(a, ret);
                    self.list(b, ret);
                }
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => self.list(body, ret),
            Stmt::Switch { cases, .. } => cases.iter_mut().for_each(|(_, b)| self.list(b, ret)),
            Stmt::Try { body, catch, finally, .. } => {
                let allocs = std::mem::replace(&mut self.allocs, false);
                self.list(body, ret);
                catch.iter_mut().for_each(|(_, c)| self.list(c, ret));
                finally.iter_mut().for_each(|c| self.list(c, ret));
                self.allocs = allocs;
            }
            _ => {}
        }
    }

    /// Emit a run: as a region if it is worth one, else as it is.
    fn flush(&mut self, list: &mut Vec<Stmt>, run: &mut Vec<Stmt>, fast: &mut Vec<Stmt>, mut r: Region, base: usize) {
        let run = std::mem::take(run);
        let mut fast = std::mem::take(fast);
        if run.is_empty() {
            return;
        }
        let stores = r.fields.iter().any(|f| f.2.is_some());
        let looped = !r.loops.is_empty() && r.accesses > 0;
        if !looped
            && (r.accesses < MIN_ACCESSES || r.accesses + r.work < MIN_WORK || (!stores && r.work < MIN_PURE_WORK))
        {
            self.truncate_locals(base);
            list.extend(run);
            return;
        }
        if !r.returned {
            let leave = r.leave(self, &run);
            match r.bailed {
                // (After the loop that reads elements, the last statement:
                // where it did not leave midway.)
                Some(b) => resume_else(&mut fast, b, leave),
                None => fast.extend(leave),
            }
        }
        // The jumps out of it from inside its loops, now that what it
        // assigns is known (see `JumpOut`).
        if !r.jumps.is_empty() {
            r.fix_jumps(&mut fast, self);
        }
        // A field the region writes is read from its local everywhere (in a
        // loop, a read before the write sees the previous iteration's).
        let locals: HashMap<(LocalId, u32), LocalId> =
            r.fields.iter().filter_map(|&(o, s, c)| Some(((o, s), c?.0))).collect();
        visit_stmts_mut(&mut fast, &mut |e| {
            if let ExprKind::SlotLoad { obj, slot } = e.kind
                && let Some(&c) = locals.get(&(obj, slot))
            {
                e.kind = ExprKind::Local(c);
            }
        });
        // The fields written start as they are (a write may be skipped).
        let mut init: Vec<Stmt> = r
            .fields
            .iter()
            .filter_map(|&(obj, slot, c)| {
                let (c, _) = c?;
                let v = Box::new(Expr::new(Ty::F64, ExprKind::SlotLoad { obj, slot }));
                Some(Stmt::Expr(Expr::new(Ty::F64, ExprKind::SetLocal(c, v))))
            })
            .collect();
        init.extend(fast);
        let written = r.fields.iter().any(|f| f.2.is_some());
        let guard = ExprKind::SpecGuard {
            distinct: written && r.objs.len() + r.derived.len() > 1,
            objs: r.objs,
            fields: r.fields.iter().map(|&(o, s, _)| (o, s)).collect(),
            nums: r.nums,
            strs: r.strs,
            char_code_at: r.char_code_at,
            derived: r.derived,
            methods: r.methods,
            cells: r.cells,
            arrays: r.arrays,
            typed_writes: r.typed_writes,
            protos: r.protos,
            appends: r.appends,
        };
        list.push(Stmt::If(Expr::new(Ty::Bool, guard), init, run));
    }
}

/// Whether `stmts` break out of, or continue, loop `id`.
fn jumps_to(stmts: &[Stmt], id: LoopId) -> bool {
    let mut found = false;
    fn walk(stmts: &[Stmt], id: LoopId, found: &mut bool) {
        for s in stmts {
            match s {
                Stmt::Break(l) | Stmt::Continue(l) if *l == id => *found = true,
                Stmt::If(_, a, b) => {
                    walk(a, id, found);
                    walk(b, id, found);
                }
                Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => walk(body, id, found),
                Stmt::Switch { cases, .. } => cases.iter().for_each(|(_, b)| walk(b, id, found)),
                Stmt::Try { body, catch, finally, .. } => {
                    walk(body, id, found);
                    catch.iter().for_each(|(_, c)| walk(c, id, found));
                    finally.iter().for_each(|c| walk(c, id, found));
                }
                _ => {}
            }
        }
    }
    walk(stmts, id, &mut found);
    found
}

/// Put `leave` in the `else` branch of the `if (bailed)` after a loop that
/// reads elements (see `Region::stmt`).
fn resume_else(stmts: &mut [Stmt], bailed: LocalId, leave: Vec<Stmt>) {
    let mut leave = Some(leave);
    fn find(stmts: &mut [Stmt], b: LocalId, leave: &mut Option<Vec<Stmt>>) {
        for s in stmts.iter_mut().rev() {
            match s {
                Stmt::If(Expr { kind: ExprKind::Local(l), .. }, _, els) if *l == b => {
                    *els = leave.take().expect("one resume");
                    return;
                }
                Stmt::Block(x) => find(x, b, leave),
                _ => {}
            }
            if leave.is_none() {
                return;
            }
        }
    }
    find(stmts, bailed, &mut leave);
    assert!(leave.is_none(), "the resume of a loop that reads elements");
}

fn f64(kind: ExprKind) -> Expr {
    Expr::new(Ty::F64, kind)
}

fn bool(kind: ExprKind) -> Expr {
    Expr::new(Ty::Bool, kind)
}

fn bx(e: Expr) -> Box<Expr> {
    Box::new(e)
}

/// The arithmetic `BinOp` of an `AnyBinOp` on two numbers.
fn arith(op: AnyBinOp) -> Option<BinOp> {
    Some(match op {
        AnyBinOp::Add => BinOp::Add,
        AnyBinOp::Sub => BinOp::Sub,
        AnyBinOp::Mul => BinOp::Mul,
        AnyBinOp::Div => BinOp::Div,
        AnyBinOp::Rem => BinOp::Rem,
        AnyBinOp::Pow => BinOp::Pow,
        _ => return None,
    })
}

/// The comparison of two numbers an `AnyCmpOp` makes.
fn compare(op: AnyCmpOp) -> Option<CmpOp> {
    Some(match op {
        AnyCmpOp::Lt => CmpOp::Lt,
        AnyCmpOp::Le => CmpOp::Le,
        AnyCmpOp::Gt => CmpOp::Gt,
        AnyCmpOp::Ge => CmpOp::Ge,
        AnyCmpOp::Eq | AnyCmpOp::StrictEq => CmpOp::Eq,
        AnyCmpOp::Ne | AnyCmpOp::StrictNe => CmpOp::Ne,
        _ => return None,
    })
}

impl Region {
    /// The statements that leave the region here: the JS-valued locals the
    /// rest of the function may see, and the fields written so far, get
    /// their values.
    fn leave(&mut self, cx: &Cx, run: &[Stmt]) -> Vec<Stmt> {
        let mut out = Vec::new();
        let inside = mentions(run, cx.f.locals.len());
        // (After a `return` that leaves the function, only its value reads
        // them.)
        let returning = cx.exits && matches!(run.first(), Some(Stmt::Return(_)));
        for l in self.assigned.clone() {
            let loaded = self.nums.iter().any(|&(m, _)| m == l);
            let needed = if returning { inside[l] > 0 } else { loaded || cx.refs[l] > inside[l] };
            if needed {
                // (No copy: a local an iteration declared, set to
                // `undefined`, and the loop is done.)
                let v = if self.undef.contains(&l) || !self.copies.contains_key(&l) {
                    Expr::new(Ty::Any, ExprKind::Undefined)
                } else {
                    let c = self.valid(l);
                    Expr::new(Ty::Any, ExprKind::Box(bx(f64(ExprKind::Local(c)))))
                };
                out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::SetLocal(l, bx(v)))));
            }
        }
        for &(obj, slot, c) in &self.fields {
            if let Some((c, arith)) = c {
                let value = bx(f64(ExprKind::Local(c)));
                out.push(Stmt::Expr(f64(ExprKind::SlotStore { obj, slot, value, arith })));
            }
        }
        out
    }

    /// `leave` where a jump (`run`) leaves the region: from inside its
    /// loops, a `RegionLeave` to be replaced once the region is known (see
    /// `JumpOut`, `fix_jumps`).
    fn leave_here(&mut self, cx: &Cx, run: &[Stmt]) -> Vec<Stmt> {
        if self.loop_assigned.is_empty() {
            return self.leave(cx, run);
        }
        let looped = self.loop_assigned.iter().flatten().copied().collect();
        let k = self.jumps.len();
        self.jumps.push(JumpOut {
            defined: self.defined.clone(),
            undef: self.undef.clone(),
            looped,
            jump: run[0].clone(),
        });
        vec![Stmt::Expr(Expr::new(Ty::Void, ExprKind::RegionLeave(k)))]
    }

    /// The `leave` of jump `k` out of the region (see `JumpOut`): every
    /// JS-valued local it may have assigned there, and every field it
    /// keeps in a local, get their values; a local it may have assigned in
    /// an earlier iteration (but not surely there) is valid in its copy
    /// once the guard loads it.
    fn leave_jump(&mut self, k: usize, cx: &Cx) -> Vec<Stmt> {
        let j = self.jumps[k].clone();
        let mut out = Vec::new();
        let inside = mentions(std::slice::from_ref(&j.jump), cx.f.locals.len());
        let returning = cx.exits && matches!(j.jump, Stmt::Return(_));
        for l in self.assigned.clone() {
            let loaded = self.nums.iter().any(|&(m, _)| m == l);
            let needed = if returning { inside[l] > 0 } else { loaded || cx.refs[l] > inside[l] };
            if !needed {
                continue;
            }
            let sure = j.defined.contains(&l);
            if !sure && !j.looped.contains(&l) && !loaded {
                continue;
            }
            let v = if j.undef.contains(&l) || !self.copies.contains_key(&l) {
                Expr::new(Ty::Any, ExprKind::Undefined)
            } else {
                let c = self.copies[&l];
                if !sure && !loaded {
                    self.nums.push((l, c));
                }
                Expr::new(Ty::Any, ExprKind::Box(bx(f64(ExprKind::Local(c)))))
            };
            out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::SetLocal(l, bx(v)))));
        }
        for &(obj, slot, c) in &self.fields {
            if let Some((c, arith)) = c {
                let value = bx(f64(ExprKind::Local(c)));
                out.push(Stmt::Expr(f64(ExprKind::SlotStore { obj, slot, value, arith })));
            }
        }
        out
    }

    /// Replace the `RegionLeave`s in `stmts` (see `leave_here`).
    fn fix_jumps(&mut self, stmts: &mut Vec<Stmt>, cx: &Cx) {
        let old = std::mem::take(stmts);
        for mut s in old {
            match &mut s {
                Stmt::Expr(Expr { kind: ExprKind::RegionLeave(k), .. }) => {
                    let k = *k;
                    stmts.extend(self.leave_jump(k, cx));
                    continue;
                }
                Stmt::If(_, a, b) => {
                    self.fix_jumps(a, cx);
                    self.fix_jumps(b, cx);
                }
                Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => self.fix_jumps(body, cx),
                Stmt::Switch { cases, .. } => {
                    for (_, b) in cases.iter_mut() {
                        self.fix_jumps(b, cx);
                    }
                }
                Stmt::Try { body, catch, finally, .. } => {
                    self.fix_jumps(body, cx);
                    for (_, c) in catch.iter_mut() {
                        self.fix_jumps(c, cx);
                    }
                    for c in finally.iter_mut() {
                        self.fix_jumps(c, cx);
                    }
                }
                _ => {}
            }
            stmts.push(s);
        }
    }

    /// The statements that leave the region in the middle of its loop, for
    /// the original loop to go on: every JS-valued local assigned, and the
    /// fields written so far, get their values.
    fn leave_all(&mut self) -> Vec<Stmt> {
        let mut out = Vec::new();
        let dead = self.elem_loop.as_ref().map(|el| el.iteration.clone()).unwrap_or_default();
        for l in self.assigned.clone() {
            // (The loop's iteration locals are declared again first.)
            if dead.contains(&l) {
                continue;
            }
            let v = if self.undef.contains(&l) || !self.copies.contains_key(&l) {
                Expr::new(Ty::Any, ExprKind::Undefined)
            } else {
                let c = self.valid(l);
                Expr::new(Ty::Any, ExprKind::Box(bx(f64(ExprKind::Local(c)))))
            };
            out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::SetLocal(l, bx(v)))));
        }
        for &(obj, slot, c) in &self.fields {
            if let Some((c, arith)) = c {
                let value = bx(f64(ExprKind::Local(c)));
                out.push(Stmt::Expr(f64(ExprKind::SlotStore { obj, slot, value, arith })));
            }
        }
        out
    }

    /// Whether `o` names an object read from an array in the region's loop
    /// (or one such an object's field holds): guarded in each iteration.
    fn is_elem(&self, o: LocalId) -> bool {
        self.is_own_elem(o) || self.nest.as_ref().is_some_and(|n| n.outer.has(o))
    }

    /// Whether `o` names an object read from an array by the loop being
    /// copied (not by the outer loop of a `Nest`).
    fn is_own_elem(&self, o: LocalId) -> bool {
        self.elems.iter().any(|e| e.0 == o) || self.elem_derived.iter().any(|d| d.0 == o)
    }

    /// The elements of the loop being copied, taken (see `Nest`).
    fn take_elems(&mut self) -> ElemSet {
        ElemSet {
            elems: std::mem::take(&mut self.elems),
            nums: std::mem::take(&mut self.elem_nums),
            bounds: std::mem::take(&mut self.elem_bounds),
            derived: std::mem::take(&mut self.elem_derived),
            fields: std::mem::take(&mut self.elem_fields),
            writes: std::mem::replace(&mut self.elem_writes, false),
        }
    }

    fn put_elems(&mut self, e: ElemSet) {
        self.elems = e.elems;
        self.elem_nums = e.nums;
        self.elem_bounds = e.bounds;
        self.elem_derived = e.derived;
        self.elem_fields = e.fields;
        self.elem_writes = e.writes;
    }

    /// Whether `l` can be the array of the region's element reads: a
    /// JS-valued local the loop does not assign, used for nothing else.
    fn array_local(&self, l: LocalId, cx: &Cx) -> bool {
        cx.f.locals[l] == Ty::Any
            && !cx.f.boxed[l]
            && !self.copies.contains_key(&l)
            && !self.strs.contains(&l)
            && !self.is_obj(l)
            && !self.undef.contains(&l)
            && !self.assigned.contains(&l)
            && !self.appends.contains(&l)
            && self.elem_loop.as_ref().is_none_or(|el| !el.assigned.contains(&l))
            && self.nest.as_ref().is_none_or(|n| !n.outer_loop.assigned.contains(&l))
    }

    /// Whether `l` can be a typed array the region reads (its `length`, the
    /// elements its loop reads): a local of a typed array type it does not
    /// assign (it assigns none: see `num`).
    fn typed_local(&self, l: LocalId, cx: &Cx) -> bool {
        matches!(cx.f.locals[l], Ty::Arr(_)) && !cx.f.boxed[l]
    }

    /// The local naming element `arr[idx]` of an iteration of the region's
    /// loop (in its body, outside inner loops), as an object of `record`
    /// (`None`: only checks that the read can be one).
    fn elem(&mut self, arr: &Expr, idx: &Expr, record: Option<usize>, cx: &mut Cx) -> Option<Option<LocalId>> {
        let (&ExprKind::Local(a), &ExprKind::Local(i)) = (&arr.kind, &idx.kind) else { return None };
        let el = self.elem_loop.as_ref()?;
        if !el.in_body || self.depth != el.level {
            return None;
        }
        let index = matches!(cx.f.locals[i], Ty::F64 | Ty::I32) && !cx.f.boxed[i] && !el.assigned.contains(&i);
        let typed = self.typed_local(a, cx);
        if !index || !(self.arrays.contains(&a) || self.array_local(a, cx) || typed) || (typed && record.is_some()) {
            return None;
        }
        if !self.arrays.contains(&a) {
            self.arrays.push(a);
        }
        let Some(record) = record else { return Some(None) };
        if let Some(e) = self.elems.iter().find(|e| e.1 == a && e.2 == i) {
            return (e.3 == record).then_some(Some(e.0));
        }
        let o = cx.new_local();
        self.elems.push((o, a, i, record));
        Some(Some(o))
    }

    /// The local naming element `arr[idx]` of an iteration of a `Nest`'s
    /// outer loop, as an object of `record`, where its inner loop uses it.
    fn outer_elem(&mut self, arr: &Expr, idx: &Expr, record: usize, cx: &mut Cx) -> Option<LocalId> {
        let (&ExprKind::Local(a), &ExprKind::Local(i)) = (&arr.kind, &idx.kind) else { return None };
        let n = self.nest.as_ref()?;
        let index =
            matches!(cx.f.locals[i], Ty::F64 | Ty::I32) && !cx.f.boxed[i] && !n.outer_loop.assigned.contains(&i);
        if !index || self.typed_local(a, cx) || !(self.arrays.contains(&a) || self.array_local(a, cx)) {
            return None;
        }
        if !self.arrays.contains(&a) {
            self.arrays.push(a);
        }
        let n = self.nest.as_mut()?;
        if let Some(e) = n.outer.elems.iter().find(|e| e.1 == a && e.2 == i) {
            return (e.3 == record).then_some(e.0);
        }
        let o = cx.new_local();
        n.outer.elems.push((o, a, i, record));
        Some(o)
    }

    /// The `F64` local holding element `arr[idx]` of an iteration of the
    /// region's loop, read as a number (the iteration's guard checks it
    /// is one, and loads it).
    fn elem_num(&mut self, arr: &Expr, idx: &Expr, cx: &mut Cx) -> Option<LocalId> {
        if matches!(arr.kind, ExprKind::Local(a) if self.typed_local(a, cx)) {
            return None;
        }
        self.elem(arr, idx, None, cx)?;
        let (&ExprKind::Local(a), &ExprKind::Local(i)) = (&arr.kind, &idx.kind) else { unreachable!() };
        if let Some(e) = self.elem_nums.iter().find(|e| e.1 == a && e.2 == i) {
            return Some(e.0);
        }
        let c = cx.new_local();
        self.elem_nums.push((c, a, i));
        Some(c)
    }

    /// Whether `e` can run after the region is left here, reading locals:
    /// those it reads are as the region found them, or have copies that
    /// leaving writes back (no objects, arrays or strings the region keeps
    /// to itself).
    fn readable_after(&self, e: &Expr, cx: &Cx) -> bool {
        let named = mentions(&[Stmt::Expr(e.clone())], cx.f.locals.len());
        (0..named.len()).filter(|&l| named[l] > 0).all(|l| {
            !self.is_obj(l)
                && !self.arrays.contains(&l)
                && !self.appends.contains(&l)
                && !self.undef.contains(&l)
                && (!self.assigned.contains(&l) || self.copies.contains_key(&l))
        })
    }

    /// The copy of JS-valued local `l`, holding its value here: if it may
    /// not be assigned yet, the guard loads it.
    fn valid(&mut self, l: LocalId) -> LocalId {
        let c = self.copies[&l];
        if !self.defined.contains(&l) && !self.nums.iter().any(|&(m, _)| m == l) {
            self.nums.push((l, c));
        }
        c
    }

    /// The copy of a statement, if it can be in a region.
    fn stmt(&mut self, s: &Stmt, cx: &mut Cx) -> Option<Stmt> {
        match s {
            Stmt::Expr(e) => Some(Stmt::Expr(self.effect(e, cx)?)),
            Stmt::Block(b) => Some(Stmt::Block(self.stmts(b, cx)?)),
            Stmt::If(c, a, b) => {
                if let Some(v) = self.known(c, cx) {
                    // (A test the guard or the region decides.)
                    return Some(Stmt::Block(self.stmts(if v { a } else { b }, cx)?));
                }
                let c = self.cond(c, cx)?;
                let before = (self.defined.clone(), self.returned, self.consts.clone(), self.undef.clone());
                self.branches += 1;
                let a = self.stmts(a, cx);
                self.branches -= 1;
                let a = a?;
                let after_a = (
                    std::mem::replace(&mut self.defined, before.0.clone()),
                    self.returned,
                    std::mem::replace(&mut self.consts, before.2.clone()),
                    std::mem::replace(&mut self.undef, before.3.clone()),
                );
                self.returned = before.1;
                self.branches += 1;
                let b = self.stmts(b, cx);
                self.branches -= 1;
                let b = b?;
                match (after_a.1, self.returned) {
                    (true, true) => {}
                    (true, false) => {}
                    (false, true) => {
                        self.defined = after_a.0;
                        self.consts = after_a.2;
                        self.undef = after_a.3;
                        self.returned = false;
                    }
                    (false, false) => {
                        self.defined.retain(|l| after_a.0.contains(l));
                        self.consts.retain(|l, v| after_a.2.get(l) == Some(v));
                        if self.undef != after_a.3 {
                            return None;
                        }
                    }
                }
                Some(Stmt::If(c, a, b))
            }
            Stmt::Loop { id, test, post_test, body, update, per_iteration } if per_iteration.is_empty() => {
                let body_src: &[Stmt] = body;
                let fresh = cx.next_loop;
                cx.next_loop += 1;
                self.loops.insert(*id, fresh);
                // The region's only statement may read objects from arrays
                // in its iterations.
                let top = self.depth == 0
                    && self.branches == 0
                    && self.inline == 0
                    && self.may_elems
                    && self.elem_loop.is_none()
                    && self.bailed.is_none();
                // A loop in the body of that loop may read elements too
                // (see `Nest`), its continuation the original code.
                let nested = !top
                    && self.depth == 1
                    && self.branches == 0
                    && self.inline == 0
                    && !*post_test
                    && self.nest.is_none()
                    && self.elem_loop.as_ref().is_some_and(|el| el.in_body && el.level == 1);
                let new_loop = |body: &[Stmt], level: usize, rest_base: usize| {
                    let mut assigned = HashSet::new();
                    visit_stmts(body, &mut |e| {
                        if let ExprKind::SetLocal(l, _)
                        | ExprKind::InitLocal(l, _)
                        | ExprKind::Update { local: l, .. } = e.kind
                        {
                            assigned.insert(l);
                        }
                    });
                    let iteration = scoped_locals(body, &cx.uses);
                    ElemLoop {
                        assigned,
                        in_body: false,
                        iteration,
                        level,
                        id: *id,
                        copy: fresh,
                        update: update.clone(),
                        rest_base,
                    }
                };
                if top {
                    self.elem_loop = Some(new_loop(body, 1, 0));
                }
                if nested {
                    let inner = new_loop(body, 2, self.rest.len());
                    let outer_loop = self.elem_loop.replace(inner).expect("the outer loop");
                    let outer = self.take_elems();
                    self.nest = Some(Box::new(Nest { outer_loop, outer }));
                }
                let mine = top || nested;
                let mut looped = HashSet::new();
                let mut note = |e: &Expr| {
                    if let ExprKind::SetLocal(l, _) | ExprKind::InitLocal(l, _) | ExprKind::Update { local: l, .. } =
                        e.kind
                    {
                        looped.insert(l);
                    }
                };
                visit_stmts(body, &mut note);
                for x in test.iter().chain(update) {
                    visit(x, &mut note);
                }
                self.loop_assigned.push(looped);
                self.depth += 1;
                // (What the loop assigns is not sure after it.)
                let defined = self.defined.clone();
                let undef = self.undef.clone();
                self.consts.clear();
                let test = match test {
                    Some(t) => Some(self.cond(t, cx)?),
                    None => None,
                };
                if let Some(el) = self.elem_loop.as_mut().filter(|_| mine) {
                    el.in_body = true;
                    el.rest_base = self.rest.len();
                }
                let mut body = self.stmts(body, cx)?;
                if let Some(el) = self.elem_loop.as_mut().filter(|_| mine) {
                    el.in_body = false;
                }
                self.returned = false;
                let update = match update {
                    Some(u) => Some(self.effect(u, cx)?),
                    None => None,
                };
                self.loop_assigned.pop();
                self.depth -= 1;
                self.defined = defined;
                self.consts.clear();
                // (Locals each iteration declares, its own or those of the
                // bodies it runs inline, are set before they are read.)
                let fresh_locals = scoped_locals(body_src, &cx.uses);
                self.undef.retain(|l| !fresh_locals.contains(l));
                if self.undef.iter().any(|l| !undef.contains(l))
                    || undef.iter().any(|l| !fresh_locals.contains(l) && !self.undef.contains(l))
                {
                    return None;
                }
                let reads = !(self.elems.is_empty() && self.elem_nums.is_empty() && self.elem_bounds.is_empty());
                if nested {
                    let n = self.nest.take().expect("the nest");
                    let inner = self.elem_loop.replace(n.outer_loop).expect("the inner loop");
                    if !reads {
                        // (Reading no elements: a loop as any other.)
                        let own = self.take_elems();
                        self.put_elems(n.outer);
                        self.elem_writes |= own.writes;
                    } else {
                        let own = std::mem::replace(&mut self.elem_writes, false);
                        let set = ElemSet {
                            elems: std::mem::take(&mut self.elems),
                            nums: std::mem::take(&mut self.elem_nums),
                            bounds: std::mem::take(&mut self.elem_bounds),
                            derived: std::mem::take(&mut self.elem_derived),
                            fields: std::mem::take(&mut self.elem_fields),
                            writes: own,
                        };
                        self.put_elems(n.outer);
                        let inner_bailed = cx.new_bool_local();
                        let rest = std::mem::take(&mut body);
                        body.push(self.inner_guard(&inner, &set, inner_bailed, rest));
                        let copy = Stmt::Loop {
                            id: fresh,
                            test,
                            post_test: *post_test,
                            body,
                            update,
                            per_iteration: Vec::new(),
                        };
                        let resume = self.inner_resume(&inner, s, cx)?;
                        return Some(Stmt::Block(vec![
                            Stmt::Expr(bool(ExprKind::InitLocal(inner_bailed, bx(bool(ExprKind::Bool(false)))))),
                            copy,
                            Stmt::If(bool(ExprKind::Local(inner_bailed)), resume, Vec::new()),
                        ]));
                    }
                }
                if top && reads {
                    // (The iteration runs in the guard's `then` branch.)
                    let rest = std::mem::take(&mut body);
                    body.push(self.elem_guard(fresh, rest, cx));
                }
                if top {
                    self.elem_loop = None;
                }
                let copy =
                    Stmt::Loop { id: fresh, test, post_test: *post_test, body, update, per_iteration: Vec::new() };
                match self.bailed.filter(|_| top) {
                    // Where an iteration's objects are not what the region
                    // needs, the original loop goes on from it (`leave` is
                    // added to the other branch: see `flush`).
                    Some(b) => {
                        self.closed = true;
                        Some(Stmt::Block(vec![
                            Stmt::Expr(bool(ExprKind::InitLocal(b, bx(bool(ExprKind::Bool(false)))))),
                            copy,
                            Stmt::If(bool(ExprKind::Local(b)), vec![s.clone()], Vec::new()),
                        ]))
                    }
                    None => Some(copy),
                }
            }
            Stmt::Break(id) | Stmt::Continue(id) => {
                let out = match self.loops.get(id) {
                    Some(&f) => match s {
                        Stmt::Break(_) => Stmt::Break(f),
                        _ => Stmt::Continue(f),
                    },
                    // Leaving the region.
                    None => {
                        let mut out = self.leave_here(cx, std::slice::from_ref(s));
                        out.push(s.clone());
                        Stmt::Block(out)
                    }
                };
                self.returned = true;
                Some(out)
            }
            // A `return` of a method body running inline: the call's value.
            Stmt::Return(Some(e)) if self.inline > 0 => {
                let v = self.num(e, cx).filter(|v| v.ty == Ty::F64)?;
                self.returned = true;
                Some(Stmt::Return(Some(v)))
            }
            Stmt::Return(_) if self.inline > 0 => None,
            Stmt::Return(e) => {
                let ret = self.ret?;
                let v = match (e, ret) {
                    (None, _) => None,
                    (Some(e), Ty::F64) => Some(self.num(e, cx)?),
                    (Some(e), Ty::Bool) => Some(self.cond(e, cx)?),
                    (Some(e), Ty::Any) => {
                        let mut probe = self.clone();
                        match probe.num(e, cx) {
                            Some(v) => {
                                *self = probe;
                                Some(Expr::new(Ty::Any, ExprKind::Box(bx(v))))
                            }
                            // Any other value: computed once the region is
                            // left (what it reads written back, or as it
                            // was), when it leaves the function.
                            None if cx.exits && self.readable_after(e, cx) => Some(e.clone()),
                            None => return None,
                        }
                    }
                    _ => return None,
                };
                // (The value reads no field this stores: those it read
                // are kept in locals.)
                let mut out = self.leave_here(cx, std::slice::from_ref(s));
                out.push(Stmt::Return(v));
                self.returned = true;
                Some(Stmt::Block(out))
            }
            _ => None,
        }
    }

    /// The check that starts each iteration of loop `copy` (in the copy),
    /// then the `iteration`: its elements are what the region needs, else
    /// it leaves (the original loop goes on from this iteration).
    fn elem_guard(&mut self, copy: LoopId, iteration: Vec<Stmt>, cx: &mut Cx) -> Stmt {
        // (An inner loop's leaving may have made it: see `inner_resume`.)
        let bailed = *self.bailed.get_or_insert_with(|| cx.new_bool_local());
        // Elements are read and written in place: they must not be objects
        // whose fields the region keeps in locals, nor (if they are
        // written) read as floats.
        let writes = self.elem_writes;
        let mut distinct: Vec<LocalId> = Vec::new();
        let entry = self.objs.iter().map(|o| o.0).chain(self.derived.iter().map(|d| d.0));
        for o in entry {
            let cached = self.fields.iter().any(|f| f.0 == o && f.2.is_some());
            if (writes || cached) && !distinct.contains(&o) {
                distinct.push(o);
            }
        }
        let mut leave = self.leave_all();
        leave.push(Stmt::Expr(bool(ExprKind::SetLocal(bailed, bx(bool(ExprKind::Bool(true)))))));
        leave.push(Stmt::Break(copy));
        let guard = bool(ExprKind::ElemGuard {
            elems: self.elems.clone(),
            nums: self.elem_nums.clone(),
            bounds: self.elem_bounds.clone(),
            derived: self.elem_derived.clone(),
            fields: self.elem_fields.clone(),
            distinct,
        });
        Stmt::If(guard, iteration, leave)
    }

    /// The check that starts each iteration of a `Nest`'s inner loop (its
    /// elements `set`), then the `iteration`; where it fails, the region
    /// leaves (`bailed`, and see `inner_resume`). Its elements are read and
    /// written in place: not objects whose fields the region keeps in
    /// locals, nor (where the iteration writes elements' fields) the outer
    /// loop's elements.
    fn inner_guard(&mut self, inner: &ElemLoop, set: &ElemSet, bailed: LocalId, iteration: Vec<Stmt>) -> Stmt {
        let mut distinct: Vec<LocalId> = Vec::new();
        let entry = self.objs.iter().map(|o| o.0).chain(self.derived.iter().map(|d| d.0));
        for o in entry {
            let cached = self.fields.iter().any(|f| f.0 == o && f.2.is_some());
            if (set.writes || cached) && !distinct.contains(&o) {
                distinct.push(o);
            }
        }
        if set.writes {
            for o in self.elems.iter().map(|e| e.0).chain(self.elem_derived.iter().map(|d| d.0)) {
                if !distinct.contains(&o) {
                    distinct.push(o);
                }
            }
        }
        // (The inner iteration's own locals are dead where it starts; the
        // outer iteration's are not.)
        let outer = self.elem_loop.replace(inner.clone());
        let mut leave = self.leave_all();
        self.elem_loop = outer;
        leave.push(Stmt::Expr(bool(ExprKind::SetLocal(bailed, bx(bool(ExprKind::Bool(true)))))));
        leave.push(Stmt::Break(inner.copy));
        let guard = bool(ExprKind::ElemGuard {
            elems: set.elems.clone(),
            nums: set.nums.clone(),
            bounds: set.bounds.clone(),
            derived: set.derived.clone(),
            fields: set.fields.clone(),
            distinct,
        });
        Stmt::If(guard, iteration, leave)
    }

    /// Where a `Nest`'s inner iteration was not what the region needs (the
    /// region left, see `inner_guard`): the original code from that
    /// iteration (the outer iteration's elements named again, the inner
    /// loop `s`, the rest of the outer iteration, its update), then the
    /// outer loop from its next iteration (`bailed`, see `elem_guard`).
    fn inner_resume(&mut self, inner: &ElemLoop, s: &Stmt, cx: &mut Cx) -> Option<Vec<Stmt>> {
        let outer = self.elem_loop.as_ref().expect("the outer loop").clone();
        let mut out = Vec::new();
        // The outer iteration's locals naming elements, which the copy
        // did not set.
        for (&l, (x, _)) in &self.aliases {
            if outer.iteration.contains(&l) && !inner.iteration.contains(&l) {
                if !matches!(x.kind, ExprKind::GetElem(..)) {
                    return None;
                }
                out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(l, bx(x.clone())))));
            }
        }
        out.push(s.clone());
        for rest in self.rest[outer.rest_base.min(self.rest.len())..].iter().rev() {
            out.extend(rest.iter().cloned());
        }
        // (The original code may not jump to the outer loop: it runs
        // outside it.)
        if jumps_to(&out, outer.id) {
            return None;
        }
        if let Some(u) = &outer.update {
            out.push(Stmt::Expr(u.clone()));
        }
        let bailed = *self.bailed.get_or_insert_with(|| cx.new_bool_local());
        out.push(Stmt::Expr(bool(ExprKind::SetLocal(bailed, bx(bool(ExprKind::Bool(true)))))));
        out.push(Stmt::Break(outer.copy));
        Some(out)
    }

    /// The copies of statements (those after a jump, unreachable, left out).
    fn stmts(&mut self, list: &[Stmt], cx: &mut Cx) -> Option<Vec<Stmt>> {
        let mut out = Vec::new();
        // (In the body of the loop that reads elements: what follows each
        // statement, for a `Nest`'s continuation.)
        let track = self.elem_loop.as_ref().is_some_and(|el| el.in_body && el.level == 1) && self.nest.is_none();
        for (k, s) in list.iter().enumerate() {
            if self.returned {
                break;
            }
            // (A loop that reads elements is the region's last statement.)
            if self.closed {
                return None;
            }
            if track {
                self.rest.push(list[k + 1..].to_vec());
            }
            let r = self.stmt(s, cx);
            if track {
                self.rest.pop();
            }
            out.push(r?);
        }
        Some(out)
    }

    /// The copy of an expression evaluated for its effects.
    fn effect(&mut self, e: &Expr, cx: &mut Cx) -> Option<Expr> {
        if e.ty == Ty::Bool {
            return self.cond(e, cx);
        }
        if let ExprKind::Seq(xs) = &e.kind {
            let out = xs.iter().map(|x| self.effect(x, cx)).collect::<Option<Vec<_>>>()?;
            let ty = out.last().map_or(Ty::F64, |x| x.ty);
            return Some(Expr::new(ty, ExprKind::Seq(out)));
        }
        // `const o = arr[i]` in the loop: `o` names the element.
        if let ExprKind::InitLocal(l, x) = &e.kind
            && let ExprKind::GetElem(a, i) = &x.kind
            && cx.f.locals[*l] == Ty::Any
            && !cx.f.boxed[*l]
            && !self.is_obj(*l)
            && !self.copies.contains_key(l)
            && !self.strs.contains(l)
            && !self.arrays.contains(l)
            && !self.appends.contains(l)
        {
            self.elem(a, i, None, cx)?;
            self.aliases.insert(*l, ((**x).clone(), self.receivers.clone()));
            return Some(Expr::num(0.0));
        }
        let saved = self.clone();
        let n = cx.f.locals.len();
        if let Some(x) = self.num(e, cx) {
            return Some(x);
        }
        *self = saved;
        cx.truncate_locals(n);
        use ExprKind::*;
        match &e.kind {
            // `undefined` in a JS-valued local (a scalar-replaced object's).
            SetLocal(l, x) | InitLocal(l, x)
                if !cx.f.boxed[*l]
                    && cx.f.locals[*l] == Ty::Any
                    && !self.is_obj(*l)
                    && !self.strs.contains(l)
                    && !self.arrays.contains(l)
                    && !self.appends.contains(l) =>
            {
                let v = self.void(x, cx)?;
                if !self.assigned.contains(l) {
                    self.assigned.push(*l);
                }
                self.defined.insert(*l);
                self.undef.insert(*l);
                Some(v)
            }
            CallMethod(..) => self.push(e, cx),
            _ => self.void(e, cx),
        }
    }

    /// The copy of `arr.push(v)` for an array in a JS-valued local (one the
    /// region only appends to, guarded as one `push` appends to), `v` a
    /// number or an object literal of numbers: where it may allocate, and
    /// defers no field's store (the allocation may throw).
    fn push(&mut self, e: &Expr, cx: &mut Cx) -> Option<Expr> {
        let ExprKind::CallMethod(recv, Key::Atom(m), args) = &e.kind else { return None };
        let (&ExprKind::Local(a), [v]) = (&recv.kind, args.as_slice()) else { return None };
        if !cx.allocs || cx.atoms[*m] != "push" || self.fields.iter().any(|f| f.2.is_some()) {
            return None;
        }
        let ok = cx.f.locals[a] == Ty::Any
            && !cx.f.boxed[a]
            && !self.copies.contains_key(&a)
            && !self.strs.contains(&a)
            && !self.is_obj(a)
            && !self.is_elem(a)
            && !self.undef.contains(&a)
            && !self.arrays.contains(&a)
            && !self.assigned.contains(&a);
        if !ok {
            return None;
        }
        let value = match &v.kind {
            ExprKind::Object(props) => {
                let mut out = Vec::new();
                for (k, x) in props {
                    if cx.atoms[*k] == "__proto__" {
                        return None;
                    }
                    let n = self.num(x, cx)?;
                    out.push((*k, Expr::new(Ty::Any, ExprKind::Box(bx(n)))));
                }
                Expr::new(Ty::Any, ExprKind::Object(out))
            }
            _ => Expr::new(Ty::Any, ExprKind::Box(bx(self.num(v, cx)?))),
        };
        if !self.appends.contains(&a) {
            self.appends.push(a);
        }
        self.allocates = true;
        self.accesses += 1;
        Some(f64(ExprKind::RegionPush { arr: a, value: bx(value) }))
    }

    /// The expressions of an inlined body but the declarations of its
    /// locals used as objects, which name their object instead (their
    /// initializer: `this`, a local or a field, read where used).
    fn bind_objects<'e>(&mut self, body: &[Stmt], exprs: Vec<&'e Expr>, cx: &Cx) -> Option<Vec<&'e Expr>> {
        let mut out = Vec::new();
        for x in exprs {
            if let ExprKind::InitLocal(l, init) = &x.kind
                && !cx.f.boxed[*l]
                && cx.f.locals[*l] == Ty::Any
                && matches!(init.kind, ExprKind::This | ExprKind::Local(_) | ExprKind::ShapeGet { .. })
                && object_param(body, *l)
                && !stmts_assign(body, *l)
            {
                self.aliases.insert(*l, ((**init).clone(), self.receivers.clone()));
                continue;
            }
            out.push(x);
        }
        Some(out)
    }

    /// Whether `l` names an object of the region.
    fn is_obj(&self, l: LocalId) -> bool {
        self.objs.iter().any(|&(o, _)| o == l) || self.aliases.contains_key(&l)
    }

    /// The copy (for its effects) of an expression whose value is
    /// `undefined`.
    fn void(&mut self, e: &Expr, cx: &mut Cx) -> Option<Expr> {
        use ExprKind::*;
        match &e.kind {
            Undefined => Some(Expr::num(0.0)),
            // A class binding's TDZ check: the guard's.
            Void(x) if matches!(x.kind, Cell(_)) => {
                let Cell(k) = x.kind else { unreachable!() };
                if !self.cells.contains(&k) {
                    self.cells.push(k);
                }
                Some(Expr::num(0.0))
            }
            Void(x) => self.effect(x, cx),
            Seq(xs) => {
                let (last, rest) = xs.split_last()?;
                let mut out = rest.iter().map(|x| self.effect(x, cx)).collect::<Option<Vec<_>>>()?;
                out.push(self.void(last, cx)?);
                Some(f64(Seq(out)))
            }
            Cond(c, a, b) => {
                let v = self.known(c, cx)?;
                self.void(if v { a } else { b }, cx)
            }
            Inline { body, .. } => {
                let (exprs, v) = inline_parts(body)?;
                let exprs = self.bind_objects(body, exprs, cx)?;
                let mut out = exprs.iter().map(|x| self.effect(x, cx)).collect::<Option<Vec<_>>>()?;
                out.push(self.void(v, cx)?);
                Some(f64(Seq(out)))
            }
            Predicted { .. } => self.predicted(e, false, cx),
            _ => None,
        }
    }

    /// Whether `c` is decided here: by constants, the flags of
    /// scalar-replaced objects, or checks the guard makes (a prototype
    /// still with a method).
    fn known(&mut self, c: &Expr, cx: &Cx) -> Option<bool> {
        use ExprKind::*;
        let n = self.methods.len();
        let r = match &c.kind {
            Bool(b) => Some(*b),
            Local(l) if !cx.f.boxed[*l] => self.consts.get(l).copied(),
            ProtoMethodIs { class, method, func } => {
                let k = (None, *class, *method, *func);
                if !self.methods.contains(&k) {
                    self.methods.push(k);
                }
                Some(true)
            }
            Cond(x, a, b) => match self.known(x, cx) {
                Some(v) => self.known(if v { a } else { b }, cx),
                None => None,
            },
            Unary(UnOp::Not, x) => self.known(x, cx).map(|v| !v),
            _ => None,
        };
        if r.is_none() {
            self.methods.truncate(n);
        }
        r
    }

    /// The copy of a predicted call of a class's method, its body inline
    /// with `this` the receiver: a number (or, unless `num`, `undefined`).
    fn predicted(&mut self, e: &Expr, num: bool, cx: &mut Cx) -> Option<Expr> {
        use ExprKind::*;
        let Predicted { expr, func, body, locals } = &e.kind else { return None };
        let CallMethod(recv, Key::Atom(method), args) = &expr.kind else { return None };
        let ms = cx.methods;
        let (class, false) = (*ms.class.get(*func)?)? else { return None };
        let record = (*ms.records.get(class)?)?;
        let nparams = ms.params[*func];
        if ms.fields[record].contains(method) || args.len() != nparams || locals.len() < nparams {
            return None;
        }
        let o = self.obj(recv, record, cx)?;
        // (An element's prototype is its shape's: the guard checks that
        // shape's.)
        let k = if self.is_elem(o) {
            if !self.protos.contains(&(record, class)) {
                self.protos.push((record, class));
            }
            (None, class, *method, *func)
        } else {
            (Some(o), class, *method, *func)
        };
        if !self.methods.contains(&k) {
            self.methods.push(k);
        }
        let parts = inline_parts(body);
        if parts.is_none() && !num {
            return None;
        }
        let mut out = Vec::new();
        // Parameters: those used as objects name their argument; the
        // others get its value.
        for (a, &p) in args.iter().zip(locals) {
            if cx.f.boxed[p] || stmts_assign(body, p) {
                return None;
            }
            // A scalar-replaced object passed on: what makes it, then
            // (`Lazy`) the object, which an inline body does not take (its
            // parameter is `undefined`; it reads the fields' locals).
            let lazy = match &a.kind {
                Lazy(_) => Some(&[][..]),
                Seq(xs) if matches!(xs.last().map(|x| &x.kind), Some(Lazy(_))) => Some(&xs[..xs.len() - 1]),
                _ => None,
            };
            if let Some(pre) = lazy {
                if object_param(body, p) || cx.f.locals[p] != Ty::Any {
                    return None;
                }
                for x in pre {
                    out.push(self.effect(x, cx)?);
                }
                let init = Expr::new(Ty::Any, InitLocal(p, bx(Expr::new(Ty::Any, Undefined))));
                out.push(self.effect(&init, cx)?);
                continue;
            }
            if object_param(body, p) {
                if !matches!(a.kind, This | Local(_) | ShapeGet { .. }) {
                    return None;
                }
                self.aliases.insert(p, (a.clone(), self.receivers.clone()));
            } else {
                let init = Expr::new(cx.f.locals[p], InitLocal(p, bx(a.clone())));
                out.push(self.num(&init, cx)?);
            }
        }
        self.receivers.push(o);
        let Some((exprs, v)) = parts else {
            // A body with statements (an early `return`): its copy runs
            // inline, its `return`s giving the value.
            let stmts = self.inline_stmts(body, cx);
            self.receivers.pop();
            let mut stmts = stmts?;
            let mut all: Vec<Stmt> = out.into_iter().map(Stmt::Expr).collect();
            all.append(&mut stmts);
            return Some(Expr::new(Ty::F64, Inline { body: all, locals: Vec::new() }));
        };
        let body = (|| {
            let exprs = self.bind_objects(body, exprs, cx)?;
            for x in exprs {
                out.push(self.effect(x, cx)?);
            }
            out.push(if num { self.num(v, cx)? } else { self.void(v, cx)? });
            Some(())
        })();
        self.receivers.pop();
        body?;
        let ty = if num { out.last().map_or(Ty::F64, |x| x.ty) } else { Ty::F64 };
        Some(Expr::new(ty, Seq(out)))
    }

    /// The copy of a method body with statements, run inline: the locals
    /// it declares as objects name their initializers (as `bind_objects`
    /// does), and its `return`s give numbers.
    fn inline_stmts(&mut self, body: &[Stmt], cx: &mut Cx) -> Option<Vec<Stmt>> {
        fn flat<'a>(b: &'a [Stmt], out: &mut Vec<&'a Stmt>) {
            for s in b {
                match s {
                    Stmt::Block(x) => flat(x, out),
                    _ => out.push(s),
                }
            }
        }
        let mut all = Vec::new();
        flat(body, &mut all);
        let saved = (self.ret, self.returned);
        self.inline += 1;
        let copy = (|| {
            let mut out = Vec::new();
            for s in all {
                if self.returned {
                    break;
                }
                if let Stmt::Expr(Expr { kind: ExprKind::InitLocal(l, init), .. }) = s
                    && !cx.f.boxed[*l]
                    && cx.f.locals[*l] == Ty::Any
                    && matches!(init.kind, ExprKind::This | ExprKind::Local(_) | ExprKind::ShapeGet { .. })
                    && object_param(body, *l)
                    && !stmts_assign(body, *l)
                {
                    self.aliases.insert(*l, ((**init).clone(), self.receivers.clone()));
                    continue;
                }
                out.push(self.stmt(s, cx)?);
            }
            Some(out)
        })();
        self.inline -= 1;
        (self.ret, self.returned) = saved;
        copy
    }

    /// The copy of a call of a leaf function, if its arguments have copies
    /// of its parameters' types.
    fn leaf_call(&mut self, e: &Expr, cx: &mut Cx) -> Option<Expr> {
        let (ExprKind::Call(g, args) | ExprKind::CallChecked { func: g, args, .. }) = &e.kind else { return None };
        let (params, ret) = cx.leaves.get(*g)?.clone()?;
        if args.len() != params.len() || !matches!(ret, Ty::F64 | Ty::Bool) {
            return None;
        }
        let mut copies = Vec::new();
        for (a, t) in args.iter().zip(&params) {
            let c = match t {
                Ty::F64 => self.num(a, cx).filter(|c| c.ty == Ty::F64)?,
                Ty::Bool => self.cond(a, cx)?,
                _ => return None,
            };
            copies.push(c);
        }
        self.work += 1;
        Some(Expr::new(ret, ExprKind::Call(*g, copies)))
    }

    /// The `Bool` copy of the truthiness of a value made only of numbers
    /// and booleans.
    fn truth(&mut self, x: &Expr, cx: &mut Cx) -> Option<Expr> {
        use ExprKind::*;
        if x.ty == Ty::Bool {
            return self.cond(x, cx);
        }
        match &x.kind {
            Box(b) if b.ty == Ty::Bool => self.cond(b, cx),
            // The value of `a && b` or `a || b` is one of them.
            Logical(op @ (LogicalOp::And | LogicalOp::Or), a, b) => {
                let a = self.truth(a, cx)?;
                let b = self.maybe(|r| r.truth(b, cx))?;
                Some(bool(Logical(*op, bx(a), bx(b))))
            }
            Call(..) | CallChecked { .. } => {
                let c = self.leaf_call(x, cx)?;
                Some(if c.ty == Ty::Bool { c } else { bool(Truthy(bx(c))) })
            }
            _ => Some(bool(Truthy(bx(self.num(x, cx)?)))),
        }
    }

    /// The local holding a string read in the region, if it can be guarded.
    fn string(&mut self, s: &Expr, cx: &Cx) -> Option<LocalId> {
        let ExprKind::Local(l) = s.kind else { return None };
        let ok = cx.f.locals[l] == Ty::Any
            && !cx.f.boxed[l]
            && !self.copies.contains_key(&l)
            && !self.arrays.contains(&l)
            && !self.appends.contains(&l)
            && !self.objs.iter().any(|&(o, _)| o == l);
        if ok && !self.strs.contains(&l) {
            self.strs.push(l);
        }
        ok.then_some(l)
    }

    /// The `F64` copy of a JS-valued local, created as assigned or (if
    /// `read`) as loaded by the guard.
    fn copy(&mut self, l: LocalId, read: bool, cx: &mut Cx) -> LocalId {
        if let Some(&c) = self.copies.get(&l) {
            return if read { self.valid(l) } else { c };
        }
        let c = cx.new_local();
        self.copies.insert(l, c);
        if read {
            self.nums.push((l, c));
        }
        c
    }

    /// The local holding the object of a field access, if it can be
    /// guarded (as an object of `record`).
    fn obj(&mut self, obj: &Expr, record: usize, cx: &mut Cx) -> Option<LocalId> {
        let o = match &obj.kind {
            ExprKind::This => match self.receivers.last() {
                Some(&o) => o,
                None if cx.this => THIS_LOCAL,
                None => return None,
            },
            ExprKind::Local(l) if self.aliases.contains_key(l) => {
                let (a, receivers) = self.aliases[l].clone();
                // An outer iteration's element, named in a `Nest`'s inner
                // loop: the outer loop's.
                if let Some(n) = &self.nest
                    && n.outer_loop.iteration.contains(l)
                    && self.elem_loop.as_ref().is_some_and(|el| !el.iteration.contains(l))
                    && let ExprKind::GetElem(arr, idx) = &a.kind
                {
                    return self.outer_elem(arr, idx, record, cx);
                }
                let inner = std::mem::replace(&mut self.receivers, receivers);
                let o = self.obj(&a, record, cx);
                self.receivers = inner;
                return o;
            }
            &ExprKind::Local(o) => {
                let ok = cx.f.locals[o] == Ty::Any
                    && !cx.f.boxed[o]
                    && !self.copies.contains_key(&o)
                    && !self.strs.contains(&o)
                    && !self.arrays.contains(&o)
                    && !self.appends.contains(&o)
                    && !self.undef.contains(&o);
                if !ok {
                    return None;
                }
                o
            }
            // An element of an array, in an iteration of the region's loop.
            ExprKind::GetElem(a, i) => return self.elem(a, i, Some(record), cx)?,
            // An object a guarded object's field holds (the region writes
            // no such field); an element's, guarded in each iteration.
            ExprKind::ShapeGet { obj: of, record: r, slot, .. } if !cx.nums[*r][*slot as usize] => {
                let of = self.obj(of, *r, cx)?;
                let own = self.is_own_elem(of);
                let outer = !own && self.nest.as_ref().is_some_and(|n| n.outer.has(of));
                let list = match (own, outer) {
                    (true, _) => &mut self.elem_derived,
                    (_, true) => &mut self.nest.as_mut().unwrap().outer.derived,
                    _ => &mut self.derived,
                };
                return match list.iter().find(|d| d.1 == of && d.2 == *slot) {
                    Some(d) => (d.3 == record).then_some(d.0),
                    None => {
                        let d = cx.new_local();
                        let list = match (own, outer) {
                            (true, _) => &mut self.elem_derived,
                            (_, true) => &mut self.nest.as_mut().unwrap().outer.derived,
                            _ => &mut self.derived,
                        };
                        list.push((d, of, *slot, record));
                        Some(d)
                    }
                };
            }
            _ => return None,
        };
        let outer = self.nest.as_ref().map(|n| &n.outer);
        let outer_derived = outer.into_iter().flat_map(|e| &e.derived);
        if let Some(d) = self.derived.iter().chain(&self.elem_derived).chain(outer_derived).find(|d| d.0 == o) {
            return (d.3 == record).then_some(o);
        }
        // (`this` of a method run inline on an element.)
        let outer_elems = outer.into_iter().flat_map(|e| &e.elems);
        if let Some(e) = self.elems.iter().chain(outer_elems).find(|e| e.0 == o) {
            return (e.3 == record).then_some(o);
        }
        if !self.objs.iter().all(|&(p, r)| p != o || r == record) {
            return None;
        }
        if !self.objs.iter().any(|&(p, _)| p == o) {
            self.objs.push((o, record));
        }
        Some(o)
    }

    /// A field of a guarded object: its `F64` local once written (by
    /// arithmetic or not, if `write`).
    fn field(&mut self, o: LocalId, slot: u32, write: Option<bool>, cx: &mut Cx) -> Option<LocalId> {
        self.accesses += 1;
        // (An element's fields are read and written in place; the outer
        // loop's elements' (see `Nest`) are checked by its guard.)
        if self.is_own_elem(o) {
            if !self.elem_fields.contains(&(o, slot)) {
                self.elem_fields.push((o, slot));
            }
            self.elem_writes |= write.is_some();
            return None;
        }
        if let Some(n) = self.nest.as_mut().filter(|n| n.outer.has(o)) {
            if !n.outer.fields.contains(&(o, slot)) {
                n.outer.fields.push((o, slot));
            }
            n.outer.writes |= write.is_some();
            self.elem_writes |= write.is_some();
            return None;
        }
        let i = match self.fields.iter().position(|f| f.0 == o && f.1 == slot) {
            Some(i) => i,
            None => {
                self.fields.push((o, slot, None));
                self.fields.len() - 1
            }
        };
        if let Some(arith) = write {
            let c = match self.fields[i].2 {
                Some((c, _)) => c,
                None => cx.new_local(),
            };
            self.fields[i].2 = Some((c, arith));
        }
        self.fields[i].2.map(|f| f.0)
    }

    /// `f` on a part evaluated only sometimes: what it surely assigns is
    /// not sure after it.
    fn maybe<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let defined = self.defined.clone();
        let consts = self.consts.clone();
        let undef = self.undef.clone();
        let r = f(self);
        self.defined = defined;
        self.consts.retain(|l, v| consts.get(l) == Some(v));
        if self.undef != undef {
            return None;
        }
        r
    }

    /// The copy of a `Bool` expression made only of comparisons of
    /// numbers, if there is one.
    fn cond(&mut self, e: &Expr, cx: &mut Cx) -> Option<Expr> {
        use ExprKind::*;
        if e.ty != Ty::Bool {
            return None;
        }
        if let Some(v) = self.known(e, cx) {
            return Some(bool(Bool(v)));
        }
        Some(match &e.kind {
            Bool(_) => e.clone(),
            Local(l) if !cx.f.boxed[*l] => e.clone(),
            Compare(op, a, b) if a.ty == Ty::Bool => bool(Compare(*op, bx(self.cond(a, cx)?), bx(self.cond(b, cx)?))),
            Compare(op, a, b) => bool(Compare(*op, bx(self.num(a, cx)?), bx(self.num(b, cx)?))),
            AnyCompare(op, a, b) => {
                let op = compare(*op)?;
                let a = self.num(a, cx)?;
                let b = self.num(b, cx)?;
                self.work += 1;
                bool(Compare(op, bx(a), bx(b)))
            }
            Truthy(x) => self.truth(x, cx)?,
            Call(..) => self.leaf_call(e, cx)?,
            Unary(UnOp::Not, x) => bool(Unary(UnOp::Not, bx(self.cond(x, cx)?))),
            Logical(op @ (LogicalOp::And | LogicalOp::Or), a, b) => {
                let a = self.cond(a, cx)?;
                let b = self.maybe(|r| r.cond(b, cx))?;
                bool(Logical(*op, bx(a), bx(b)))
            }
            Cond(c, a, b) => {
                let c = self.cond(c, cx)?;
                let a = self.maybe(|r| r.cond(a, cx))?;
                let b = self.maybe(|r| r.cond(b, cx))?;
                bool(Cond(bx(c), bx(a), bx(b)))
            }
            SetLocal(l, x) | InitLocal(l, x) if !cx.f.boxed[*l] && cx.f.locals[*l] == Ty::Bool => {
                let v = self.cond(x, cx)?;
                match v.kind {
                    Bool(b) => self.consts.insert(*l, b),
                    _ => self.consts.remove(l),
                };
                let v = bx(v);
                bool(if matches!(e.kind, InitLocal(..)) { InitLocal(*l, v) } else { SetLocal(*l, v) })
            }
            _ => return None,
        })
    }

    /// The copy of an expression whose value is a number made only of
    /// numbers (`F64`, or `I32` where it is unchanged), if there is one.
    fn num(&mut self, e: &Expr, cx: &mut Cx) -> Option<Expr> {
        use ExprKind::*;
        Some(match &e.kind {
            Num(_) => e.clone(),
            Local(l) if !cx.f.boxed[*l] => match cx.f.locals[*l] {
                Ty::F64 | Ty::I32 => e.clone(),
                Ty::Any
                    if !self.is_obj(*l)
                        && !self.strs.contains(l)
                        && !self.undef.contains(l)
                        && !self.arrays.contains(l)
                        && !self.appends.contains(l) =>
                {
                    f64(Local(self.copy(*l, true, cx)))
                }
                _ => return None,
            },
            Seq(xs) => {
                let (last, rest) = xs.split_last()?;
                let mut out = rest.iter().map(|x| self.effect(x, cx)).collect::<Option<Vec<_>>>()?;
                let v = self.num(last, cx)?;
                let ty = v.ty;
                out.push(v);
                Expr::new(ty, Seq(out))
            }
            Inline { body, .. } => {
                let (exprs, v) = inline_parts(body)?;
                let exprs = self.bind_objects(body, exprs, cx)?;
                let mut out = exprs.iter().map(|x| self.effect(x, cx)).collect::<Option<Vec<_>>>()?;
                let v = self.num(v, cx)?;
                let ty = v.ty;
                out.push(v);
                Expr::new(ty, Seq(out))
            }
            Predicted { .. } => self.predicted(e, true, cx)?,
            Cond(c, a, b) if e.ty != Ty::Bool && self.known(c, cx).is_some() => {
                let v = self.known(c, cx).expect("decided");
                self.num(if v { a } else { b }, cx)?
            }
            Update { local, .. } if cx.f.locals[*local] == Ty::F64 && !cx.f.boxed[*local] => e.clone(),
            Box(x) if matches!(x.ty, Ty::F64 | Ty::I32) => self.num(x, cx)?,
            Unary(UnOp::Neg, x) if e.ty == Ty::F64 => f64(Unary(UnOp::Neg, bx(self.num(x, cx)?))),
            AnyUnary(AnyUnOp::Neg, x) => {
                self.work += 1;
                f64(Unary(UnOp::Neg, bx(self.num(x, cx)?)))
            }
            AnyUnary(AnyUnOp::Plus | AnyUnOp::ToNumeric, x) => {
                self.work += 1;
                self.num(x, cx)?
            }
            Binary(op, a, b) if e.ty == Ty::F64 && arith_op(*op) => {
                let a = self.num(a, cx)?;
                let b = self.num(b, cx)?;
                f64(Binary(*op, bx(a), bx(b)))
            }
            NumArith(op, a, b) if arith_op(*op) => {
                let a = self.num(a, cx)?;
                let b = self.num(b, cx)?;
                self.work += 1;
                f64(Binary(*op, bx(a), bx(b)))
            }
            AnyBinary(op, a, b) => {
                let op = arith(*op)?;
                let a = self.num(a, cx)?;
                let b = self.num(b, cx)?;
                self.work += 1;
                f64(Binary(op, bx(a), bx(b)))
            }
            Math(fun, args) if e.ty == Ty::F64 => {
                let args = args.iter().map(|a| self.num(a, cx)).collect::<Option<Vec<_>>>()?;
                f64(Math(*fun, args))
            }
            Logical(op @ (LogicalOp::And | LogicalOp::Or), a, b) if e.ty != Ty::Bool => {
                let a = self.num(a, cx)?;
                let b = self.maybe(|r| r.num(b, cx))?;
                self.work += 1;
                f64(Logical(*op, bx(a), bx(b)))
            }
            Cond(c, a, b) if e.ty != Ty::Bool => {
                let c = self.cond(c, cx)?;
                let a = self.maybe(|r| r.num(a, cx))?;
                let b = self.maybe(|r| r.num(b, cx))?;
                f64(Cond(bx(c), bx(a), bx(b)))
            }
            Call(..) | CallChecked { .. } => self.leaf_call(e, cx).filter(|c| c.ty == Ty::F64)?,
            CallMethod(s, Key::Atom(a), args) if cx.atoms[*a] == "charCodeAt" && args.len() <= 1 => {
                let idx = match args.first() {
                    Some(x) => self.num(x, cx)?,
                    None => Expr::num(0.0),
                };
                let s = self.string(s, cx)?;
                self.char_code_at = true;
                self.accesses += 1;
                f64(StrCharCode { s, idx: bx(idx) })
            }
            Length(s, _) if matches!(s.kind, Local(l) if self.strs.contains(&l) || cx.texts.contains(&l)) => {
                let s = self.string(s, cx)?;
                f64(StrLen { s })
            }
            // An element the loop reads, a number (see `elem_num`).
            GetElem(a, i) => {
                let c = self.elem_num(a, i, cx)?;
                self.accesses += 1;
                f64(Local(c))
            }
            // An element of a typed array the loop reads, inside it (the
            // iteration's guard checks: read in place, as it may be written).
            TypedGet(_, a, i) => {
                let (&Local(arr), &Local(idx)) = (&a.kind, &i.kind) else { return None };
                if !self.typed_local(arr, cx) {
                    return None;
                }
                self.elem(a, i, None, cx)?;
                if !self.elem_bounds.contains(&(arr, idx)) {
                    self.elem_bounds.push((arr, idx));
                }
                self.accesses += 1;
                f64(RegionTypedGet { arr, idx })
            }
            // A typed array's element written (nothing outside it).
            TypedSet(_, a, i, x) if matches!(a.kind, Local(l) if self.typed_local(l, cx)) => {
                let Local(arr) = a.kind else { unreachable!() };
                let idx = self.num(i, cx)?;
                let value = self.num(x, cx)?;
                if !self.typed_writes.contains(&arr) {
                    self.typed_writes.push(arr);
                }
                self.accesses += 1;
                f64(RegionTypedSet { arr, idx: bx(idx), value: bx(value) })
            }
            // A typed array's (guarded: with a pristine shape, its `length`
            // is its element count).
            &Length(ref a, _) if matches!(a.kind, Local(l) if self.typed_local(l, cx)) => {
                let Local(arr) = a.kind else { unreachable!() };
                if !self.arrays.contains(&arr) {
                    self.arrays.push(arr);
                }
                self.accesses += 1;
                f64(ArrLen { arr })
            }
            // An array's (guarded as a dense fast array).
            &Length(ref a, _) if matches!(a.kind, Local(l) if self.arrays.contains(&l) || self.array_local(l, cx)) => {
                let Local(arr) = a.kind else { unreachable!() };
                if !self.arrays.contains(&arr) {
                    self.arrays.push(arr);
                }
                self.accesses += 1;
                f64(ArrLen { arr })
            }
            ShapeGet { obj, record, slot, .. } if cx.nums[*record][*slot as usize] => {
                let o = self.obj(obj, *record, cx)?;
                match self.field(o, *slot, None, cx) {
                    Some(c) => f64(Local(c)),
                    None => f64(SlotLoad { obj: o, slot: *slot }),
                }
            }
            ShapeSet { obj, record, slot, value, .. } if cx.nums[*record][*slot as usize] => {
                let o = self.obj(obj, *record, cx)?;
                let v = self.num(value, cx)?;
                let arith = matches!(v.kind, Binary(..) | Unary(..));
                match self.field(o, *slot, Some(arith), cx) {
                    // (Not where it may allocate: see `push`.)
                    Some(_) if self.allocates => return None,
                    Some(c) => f64(SetLocal(c, bx(v))),
                    // An element's: in place.
                    None => f64(SlotStore { obj: o, slot: *slot, value: bx(v), arith }),
                }
            }
            SetLocal(l, x) | InitLocal(l, x) if !cx.f.boxed[*l] => {
                let init = matches!(e.kind, InitLocal(..));
                match cx.f.locals[*l] {
                    Ty::F64 => {
                        let v = bx(self.num(x, cx)?);
                        f64(if init { InitLocal(*l, v) } else { SetLocal(*l, v) })
                    }
                    Ty::Any
                        if !self.is_obj(*l)
                            && !self.strs.contains(l)
                            && !self.arrays.contains(l)
                            && !self.appends.contains(l) =>
                    {
                        let v = self.num(x, cx)?;
                        let c = self.copy(*l, false, cx);
                        if !self.assigned.contains(l) {
                            self.assigned.push(*l);
                        }
                        self.defined.insert(*l);
                        self.undef.remove(l);
                        f64(SetLocal(c, bx(v)))
                    }
                    _ => return None,
                }
            }
            _ => return None,
        })
    }
}

/// Arithmetic that stays a number (not the bitwise operators, which make
/// `I32`s).
fn arith_op(op: BinOp) -> bool {
    matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Pow)
}

/// An inlined body that only evaluates expressions, then returns a value:
/// those expressions and the value.
fn inline_parts(body: &[Stmt]) -> Option<(Vec<&Expr>, &Expr)> {
    fn flat<'a>(b: &'a [Stmt], out: &mut Vec<&'a Stmt>) {
        for s in b {
            match s {
                Stmt::Block(x) => flat(x, out),
                _ => out.push(s),
            }
        }
    }
    let mut all = Vec::new();
    flat(body, &mut all);
    let (last, rest) = all.split_last()?;
    let Stmt::Return(Some(v)) = last else { return None };
    let exprs = rest
        .iter()
        .map(|s| match s {
            Stmt::Expr(e) => Some(e),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((exprs, v))
}

/// The locals a loop body declares afresh in each iteration: those its
/// body names first by declaring them (`InitLocal`, as `let`, `const`, the
/// fields of scalar-replaced objects and inlined bodies' locals are), that
/// nothing outside the body names (`uses`: the function's counts). Their
/// values are dead where an iteration starts.
fn scoped_locals(body: &[Stmt], uses: &[usize]) -> HashSet<LocalId> {
    let inside = local_uses(body, uses.len());
    let mut seen: HashSet<LocalId> = HashSet::new();
    let mut out = HashSet::new();
    visit_stmts(body, &mut |e| match &e.kind {
        ExprKind::InitLocal(l, x) => {
            let mut own = false;
            visit(x, &mut |y| expr_locals(y, &mut |m| own |= m == *l));
            if !seen.contains(l) && !own && inside[*l] == uses[*l] {
                out.insert(*l);
            }
            seen.insert(*l);
        }
        // (The locals an inlined body declares, not named yet.)
        ExprKind::Inline { .. } | ExprKind::Predicted { .. } => {}
        _ => expr_locals(e, &mut |m| {
            seen.insert(m);
        }),
    });
    out
}

/// Whether `stmts` assign local `l`.
fn stmts_assign(stmts: &[Stmt], l: LocalId) -> bool {
    let mut found = false;
    visit_stmts(stmts, &mut |e| {
        found |= matches!(e.kind, ExprKind::SetLocal(m, _) | ExprKind::Update { local: m, .. } if m == l)
    });
    found
}

/// Whether parameter `p` is used as an object in `body` (whose fields are
/// read, or whose methods are called).
fn object_param(body: &[Stmt], p: LocalId) -> bool {
    object_use(body, p, 4)
}

/// `object_param` through up to `depth` locals that `p` initializes (the
/// parameters of the methods run inline that it is passed to).
fn object_use(body: &[Stmt], p: LocalId, depth: usize) -> bool {
    let is_p = |x: &Expr| matches!(x.kind, ExprKind::Local(l) if l == p);
    let mut found = false;
    let mut passed = Vec::new();
    visit_stmts(body, &mut |e| {
        found |= match &e.kind {
            ExprKind::ShapeGet { obj, .. } | ExprKind::ShapeSet { obj, .. } => is_p(obj),
            ExprKind::CallMethod(o, ..) | ExprKind::GetProp(o, _) => is_p(o),
            ExprKind::InitLocal(m, x) if is_p(x) && *m != p => {
                passed.push(*m);
                false
            }
            _ => false,
        }
    });
    found || (depth > 0 && passed.into_iter().any(|m| object_use(body, m, depth - 1)))
}
