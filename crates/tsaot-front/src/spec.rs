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
//! copy can run code, throw or allocate, so what the guard checked holds
//! throughout, and when the stores happen cannot be observed.
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
        let mut body = std::mem::take(&mut f.body);
        let ret = f.ret;
        let this = f.has_this;
        let mut cx = Cx {
            f,
            nums: &nums,
            refs,
            atoms: &atoms,
            leaves: &leaves,
            methods: &methods,
            this,
            next_loop: FIRST_COPY_LOOP,
        };
        cx.list(&mut body, Some(ret));
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
        | ExprKind::ForInGet(l, ..)
        | ExprKind::AsyncIterResult(_, l)
        | ExprKind::IterCloseQuiet(l) => add(*l),
        ExprKind::IterOpen(_, a, b, _) | ExprKind::IterClose(a, b) => {
            add(*a);
            add(*b);
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
    /// How many times each local is named in the function (before any
    /// region is made).
    refs: Vec<usize>,
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
}

impl Cx<'_> {
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
            if !r.returned
                && let Some(x) = r.stmt(&s, self)
            {
                run.push(s);
                fast.push(x);
                continue;
            }
            self.truncate_locals(n);
            r = saved;
            self.flush(list, &mut run, &mut fast, std::mem::replace(&mut r, fresh()), base);
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
                self.list(body, ret);
                catch.iter_mut().for_each(|(_, c)| self.list(c, ret));
                finally.iter_mut().for_each(|c| self.list(c, ret));
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
            fast.extend(r.leave(self, &run));
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
        };
        list.push(Stmt::If(Expr::new(Ty::Bool, guard), init, run));
    }
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
        for l in self.assigned.clone() {
            let loaded = self.nums.iter().any(|&(m, _)| m == l);
            if loaded || cx.refs[l] > inside[l] {
                let v = if self.undef.contains(&l) {
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
                let a = self.stmts(a, cx)?;
                let after_a = (
                    std::mem::replace(&mut self.defined, before.0.clone()),
                    self.returned,
                    std::mem::replace(&mut self.consts, before.2.clone()),
                    std::mem::replace(&mut self.undef, before.3.clone()),
                );
                self.returned = before.1;
                let b = self.stmts(b, cx)?;
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
                let fresh = cx.next_loop;
                cx.next_loop += 1;
                self.loops.insert(*id, fresh);
                // (What the loop assigns is not sure after it.)
                let defined = self.defined.clone();
                let undef = self.undef.clone();
                self.consts.clear();
                let test = match test {
                    Some(t) => Some(self.cond(t, cx)?),
                    None => None,
                };
                let body = self.stmts(body, cx)?;
                self.returned = false;
                let update = match update {
                    Some(u) => Some(self.effect(u, cx)?),
                    None => None,
                };
                self.defined = defined;
                self.consts.clear();
                if self.undef != undef {
                    return None;
                }
                Some(Stmt::Loop { id: fresh, test, post_test: *post_test, body, update, per_iteration: Vec::new() })
            }
            Stmt::Break(id) | Stmt::Continue(id) => {
                let out = match self.loops.get(id) {
                    Some(&f) => match s {
                        Stmt::Break(_) => Stmt::Break(f),
                        _ => Stmt::Continue(f),
                    },
                    // Leaving the region.
                    None => {
                        let mut out = self.leave(cx, std::slice::from_ref(s));
                        out.push(s.clone());
                        Stmt::Block(out)
                    }
                };
                self.returned = true;
                Some(out)
            }
            Stmt::Return(e) => {
                let ret = self.ret?;
                let v = match (e, ret) {
                    (None, _) => None,
                    (Some(e), Ty::F64) => Some(self.num(e, cx)?),
                    (Some(e), Ty::Any) => {
                        let v = self.num(e, cx)?;
                        Some(Expr::new(Ty::Any, ExprKind::Box(bx(v))))
                    }
                    _ => return None,
                };
                // (The value reads no field this stores: those it read
                // are kept in locals.)
                let mut out = self.leave(cx, std::slice::from_ref(s));
                out.push(Stmt::Return(v));
                self.returned = true;
                Some(Stmt::Block(out))
            }
            _ => None,
        }
    }

    /// The copies of statements (those after a jump, unreachable, left out).
    fn stmts(&mut self, list: &[Stmt], cx: &mut Cx) -> Option<Vec<Stmt>> {
        let mut out = Vec::new();
        for s in list {
            if self.returned {
                break;
            }
            out.push(self.stmt(s, cx)?);
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
                if !cx.f.boxed[*l] && cx.f.locals[*l] == Ty::Any && !self.is_obj(*l) && !self.strs.contains(l) =>
            {
                let v = self.void(x, cx)?;
                if !self.assigned.contains(l) {
                    self.assigned.push(*l);
                }
                self.defined.insert(*l);
                self.undef.insert(*l);
                Some(v)
            }
            _ => self.void(e, cx),
        }
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
        let k = (Some(o), class, *method, *func);
        if !self.methods.contains(&k) {
            self.methods.push(k);
        }
        let (exprs, v) = inline_parts(body)?;
        let exprs = self.bind_objects(body, exprs, cx)?;
        let mut out = Vec::new();
        // Parameters: those used as objects name their argument; the
        // others get its value.
        for (a, &p) in args.iter().zip(locals) {
            if cx.f.boxed[p] || stmts_assign(body, p) {
                return None;
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
        let body = (|| {
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
                    && !self.undef.contains(&o);
                if !ok {
                    return None;
                }
                o
            }
            // An object a guarded object's field holds (the region writes
            // no such field).
            ExprKind::ShapeGet { obj: of, record: r, slot, .. } if !cx.nums[*r][*slot as usize] => {
                let of = self.obj(of, *r, cx)?;
                return match self.derived.iter().find(|d| d.1 == of && d.2 == *slot) {
                    Some(d) => (d.3 == record).then_some(d.0),
                    None => {
                        let d = cx.new_local();
                        self.derived.push((d, of, *slot, record));
                        Some(d)
                    }
                };
            }
            _ => return None,
        };
        if let Some(d) = self.derived.iter().find(|d| d.0 == o) {
            return (d.3 == record).then_some(o);
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
                Ty::Any if !self.is_obj(*l) && !self.strs.contains(l) && !self.undef.contains(l) => {
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
            Length(s, _) if matches!(s.kind, Local(l) if self.strs.contains(&l)) => {
                let s = self.string(s, cx)?;
                f64(StrLen { s })
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
                let c = self.field(o, *slot, Some(arith), cx).expect("written field");
                f64(SetLocal(c, bx(v)))
            }
            SetLocal(l, x) | InitLocal(l, x) if !cx.f.boxed[*l] => {
                let init = matches!(e.kind, InitLocal(..));
                match cx.f.locals[*l] {
                    Ty::F64 => {
                        let v = bx(self.num(x, cx)?);
                        f64(if init { InitLocal(*l, v) } else { SetLocal(*l, v) })
                    }
                    Ty::Any if !self.is_obj(*l) && !self.strs.contains(l) => {
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
    let is_p = |x: &Expr| matches!(x.kind, ExprKind::Local(l) if l == p);
    let mut found = false;
    visit_stmts(body, &mut |e| {
        found |= match &e.kind {
            ExprKind::ShapeGet { obj, .. } | ExprKind::ShapeSet { obj, .. } => is_p(obj),
            ExprKind::CallMethod(o, ..) | ExprKind::GetProp(o, _) => is_p(o),
            _ => false,
        }
    });
    found
}
