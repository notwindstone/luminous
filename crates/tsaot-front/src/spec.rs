//! Speculative numeric regions, as JIT compilers speculate on what fields
//! hold: a run of statements that only computes numbers from the number
//! fields of objects of record types (`ShapeGet` / `ShapeSet` of fields
//! annotated `number`), number locals, JS-valued locals and constants (with
//! comparisons, `if`, `?:`, `&&`, `||` and `return` among them) gets a copy
//! where those fields are read as numbers (`SlotLoad`) and kept in `F64`
//! locals once written, as JS-valued locals are. A guard before it
//! (`SpecGuard`) checks the objects' shapes, that they are distinct objects
//! if fields are written, and that the fields and the JS-valued locals it
//! reads hold numbers; the copy then runs as plain float code and stores the
//! fields and locals it changed where it ends or returns (`SlotStore`).
//! When the guard fails, the statements run as they are. Nothing in the
//! copy can run code, throw or allocate, so what the guard checked holds
//! throughout, and when the stores happen cannot be observed.

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
    for f in m.funcs.iter_mut().chain(m.closures.iter_mut()) {
        if f.is_async || f.is_generator {
            continue;
        }
        let refs = mentions(&f.body, f.locals.len());
        let mut body = std::mem::take(&mut f.body);
        let ret = f.ret;
        let mut cx = Cx { f, nums: &nums, refs };
        cx.list(&mut body, Some(ret));
        // The statement lists inside expressions (inlined bodies, whose
        // `return`s give the expression's value).
        visit_stmts_mut(&mut body, &mut |e| match &mut e.kind {
            ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } => cx.list(body, Some(e.ty)),
            ExprKind::ArrayLoop { body, func, .. } => cx.list(body, rets.get(*func).copied()),
            _ => {}
        });
        cx.f.body = body;
    }
}

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

struct Cx<'a> {
    f: &'a mut Func,
    nums: &'a [Vec<bool>],
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
    /// Whether every path so far has returned.
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
        if r.accesses < MIN_ACCESSES || r.accesses + r.work < MIN_WORK || (!stores && r.work < MIN_PURE_WORK) {
            self.truncate_locals(base);
            list.extend(run);
            return;
        }
        if !r.returned {
            fast.extend(r.leave(self, &run));
        }
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
            distinct: written && r.objs.len() > 1,
            objs: r.objs,
            fields: r.fields.iter().map(|&(o, s, _)| (o, s)).collect(),
            nums: r.nums,
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
                let c = self.valid(l);
                let v = Expr::new(Ty::Any, ExprKind::Box(bx(f64(ExprKind::Local(c)))));
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
            Stmt::Expr(e) => Some(Stmt::Expr(if e.ty == Ty::Bool { self.cond(e, cx)? } else { self.num(e, cx)? })),
            Stmt::Block(b) => Some(Stmt::Block(self.stmts(b, cx)?)),
            Stmt::If(c, a, b) => {
                let c = self.cond(c, cx)?;
                let before = (self.defined.clone(), self.returned);
                let a = self.stmts(a, cx)?;
                let after_a = (std::mem::replace(&mut self.defined, before.0.clone()), self.returned);
                self.returned = before.1;
                let b = self.stmts(b, cx)?;
                match (after_a.1, self.returned) {
                    (true, true) => {}
                    (true, false) => {}
                    (false, true) => {
                        self.defined = after_a.0;
                        self.returned = false;
                    }
                    (false, false) => self.defined.retain(|l| after_a.0.contains(l)),
                }
                Some(Stmt::If(c, a, b))
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

    fn stmts(&mut self, list: &[Stmt], cx: &mut Cx) -> Option<Vec<Stmt>> {
        list.iter().map(|s| if self.returned { Some(s.clone()) } else { self.stmt(s, cx) }).collect()
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
    fn obj(&mut self, obj: &Expr, record: usize, cx: &Cx) -> Option<LocalId> {
        let ExprKind::Local(o) = obj.kind else { return None };
        let ok = cx.f.locals[o] == Ty::Any
            && !cx.f.boxed[o]
            && !self.copies.contains_key(&o)
            && self.objs.iter().all(|&(p, r)| p != o || r == record);
        if ok && !self.objs.iter().any(|&(p, _)| p == o) {
            self.objs.push((o, record));
        }
        ok.then_some(o)
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
        let r = f(self);
        self.defined = defined;
        r
    }

    /// The copy of a `Bool` expression made only of comparisons of
    /// numbers, if there is one.
    fn cond(&mut self, e: &Expr, cx: &mut Cx) -> Option<Expr> {
        use ExprKind::*;
        if e.ty != Ty::Bool {
            return None;
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
            Truthy(x) if x.ty == Ty::Bool => self.cond(x, cx)?,
            Truthy(x) => bool(Truthy(bx(self.num(x, cx)?))),
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
                let v = bx(self.cond(x, cx)?);
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
                Ty::Any if !self.objs.iter().any(|&(o, _)| o == *l) => f64(Local(self.copy(*l, true, cx))),
                _ => return None,
            },
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
                    Ty::Any if !self.objs.iter().any(|&(o, _)| o == *l) => {
                        let v = self.num(x, cx)?;
                        let c = self.copy(*l, false, cx);
                        if !self.assigned.contains(l) {
                            self.assigned.push(*l);
                        }
                        self.defined.insert(*l);
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
