//! Reads that survive calls, by the effect summaries (`effects`):
//!
//! - A property read, or a call of a function that writes nothing and
//!   allocates nothing, made by a statement and again by later statements
//!   of the same list that (with the functions they call) write nothing it
//!   reads, is made once: the later ones use its value (`repeated`).
//! - A property read in a loop whose object the loop does not change, and
//!   whose property the loop's code writes nowhere, is made once before
//!   the loop, where it can be without running code or throwing
//!   (`ExprKind::Peek`: a data property of ordinary objects; `hoist`).
//!
//! The value is used (`ExprKind::Reuse`) while the engine has run no code
//! native code does not call directly since it was read (its call count,
//! `js_call_epoch`: a getter, a setter, a coercion, a call through a
//! value, any of which could write what was read); else the read or call
//! is made as before, so it also throws, or runs a getter, exactly where
//! it did.

use crate::effects::{Effects, Summaries, of_stmts, of_stmts_in};
use crate::ir::*;

/// Reuses the repeated reads and calls of every function of `m`
/// (`repeated`), then hoists the loop-invariant property reads (`hoist`).
pub fn reuse_reads(m: &mut Module, table: &Summaries) {
    let n = m.funcs.len();
    for g in 0..n + m.closures.len() {
        let before = if g < n { m.funcs[g].clone() } else { m.closures[g - n].clone() };
        let mut body = std::mem::take(if g < n { &mut m.funcs[g].body } else { &mut m.closures[g - n].body });
        let added = {
            let locals = Locals { base: before.locals.len(), added: Vec::new() };
            let cx = crate::effects::context_of(m, table, &before);
            let mut scan = Scan { m, table, f: &before, cx, locals };
            lists_repeated(&mut body, &mut scan);
            scan.locals.added
        };
        let f = if g < n { &mut m.funcs[g] } else { &mut m.closures[g - n] };
        f.body = body;
        for ty in added {
            f.locals.push(ty);
            f.boxed.push(false);
            f.local_names.push(None);
        }
    }
    hoist_reads(m, table);
}

/// What a statement reads again: a property of a base, or a call.
#[derive(Clone, PartialEq)]
enum Key {
    Read(PeekBase, AtomId),
    /// A function, and its arguments: locals, `this` or constants; and
    /// whether the call checks them (`CallChecked`: when a check fails,
    /// the function object is called, through the engine).
    Call(FuncId, Vec<Arg>, bool),
}

#[derive(Clone, PartialEq)]
enum Arg {
    Local(LocalId),
    This,
    /// (By its bits: `-0` is not `0`.)
    Num(u64),
    Bool(bool),
    Str(StrId),
    Undefined,
    Null,
}

fn arg_of(e: &Expr) -> Option<Arg> {
    Some(match e.kind {
        ExprKind::Local(l) if l == THIS_LOCAL => Arg::This,
        ExprKind::Local(l) => Arg::Local(l),
        ExprKind::This => Arg::This,
        ExprKind::Num(v) => Arg::Num(v.to_bits()),
        ExprKind::Bool(b) => Arg::Bool(b),
        ExprKind::Str(s) => Arg::Str(s),
        ExprKind::Undefined => Arg::Undefined,
        ExprKind::Null => Arg::Null,
        _ => return None,
    })
}

/// The key of `e`, if it is one: a read `read_of` accepts, or a call of a
/// function that writes nothing, allocates nothing and observes nothing
/// else (time, randomness), with plain arguments.
fn key_of(e: &Expr, m: &Module, table: &Summaries) -> Option<Key> {
    if let Some((b, a)) = read_of(e) {
        return (!skipped(m, a)).then_some(Key::Read(b, a));
    }
    let (k, args, checked) = match &e.kind {
        ExprKind::Call(k, args) => (*k, args, false),
        // (The function object: a binding never assigned.)
        ExprKind::CallChecked { func, callee, args, slow: None, .. } if matches!(callee.kind, ExprKind::Cell(_)) => {
            (*func, args, true)
        }
        _ => return None,
    };
    let x = table.funcs.get(k)?;
    if !x.writes.is_empty() || x.allocates || x.engine || x.suspends || x.nondet || e.ty == Ty::Void {
        return None;
    }
    Some(Key::Call(k, args.iter().map(arg_of).collect::<Option<_>>()?, checked))
}

/// Whether what `key` gives stays the same across code with effects `x`
/// that writes the locals `written` (but for code the engine runs).
fn holds(key: &Key, x: &Effects, written: &[LocalId], f: &Func, table: &Summaries) -> bool {
    if x.suspends {
        return false;
    }
    match key {
        Key::Read(b, a) => {
            !x.writes.field(*a)
                && match *b {
                    PeekBase::Local(l) => base_local(f, l) && !written.contains(&l),
                    PeekBase::Cell(c) => !x.writes.cell(c),
                    PeekBase::This => f.has_this,
                }
        }
        Key::Call(k, args, _) => {
            !x.writes.meets(&table.funcs[*k].reads)
                && args.iter().all(|a| match *a {
                    Arg::Local(l) => {
                        l < f.locals.len()
                            && !f.boxed[l]
                            && !(f.arguments && l < f.params.len())
                            && !written.contains(&l)
                    }
                    _ => true,
                })
        }
    }
}

/// Whether local `l` may be the base of a reused read: a JS value only the
/// function's own code assigns.
fn base_local(f: &Func, l: LocalId) -> bool {
    l < f.locals.len() && f.locals[l] == Ty::Any && !f.boxed[l] && !(f.arguments && l < f.params.len())
}

/// Properties not reused: `__proto__` (an accessor), and `length` (mostly
/// of arrays and strings, which are not ordinary objects: their own reads
/// are quick).
fn skipped(m: &Module, a: AtomId) -> bool {
    matches!(m.atoms[a].as_str(), "__proto__" | "length")
}

/// The locals statement `s` writes.
fn written_by(s: &Stmt) -> Vec<LocalId> {
    let mut out = Vec::new();
    let one = std::slice::from_ref(s);
    deep_stmt_locals(one, &mut |l| out.push(l));
    visit_stmts(one, &mut |e| written_locals(e, &mut |l| out.push(l)));
    out
}

/// Calls `f` on the subexpressions of `e` that are evaluated whenever `e`
/// is (not in a branch, a short circuit, or an inlined body), `e` first.
fn walk_always(e: &mut Expr, f: &mut dyn FnMut(&mut Expr) -> bool) {
    if !f(e) {
        return;
    }
    match e.kind {
        ExprKind::Logical(..)
        | ExprKind::Cond(..)
        | ExprKind::Lazy(_)
        | ExprKind::Inline { .. }
        | ExprKind::Predicted { .. }
        | ExprKind::ArrayLoop { .. }
        | ExprKind::ArraySort { .. }
        | ExprKind::CallChecked { .. }
        | ExprKind::ArrayChain { .. }
        | ExprKind::Reuse { .. } => return,
        _ => {}
    }
    for c in children_mut(e) {
        walk_always(c, f);
    }
}

fn lists_repeated(list: &mut [Stmt], cx: &mut Scan) {
    repeated(list, cx);
    for s in list.iter_mut() {
        match s {
            Stmt::If(_, a, b) => {
                lists_repeated(a, cx);
                lists_repeated(b, cx);
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => lists_repeated(body, cx),
            Stmt::Switch { cases, .. } => cases.iter_mut().for_each(|(_, b)| lists_repeated(b, cx)),
            Stmt::Try { body, catch, finally, .. } => {
                lists_repeated(body, cx);
                catch.iter_mut().for_each(|(_, c)| lists_repeated(c, cx));
                finally.iter_mut().for_each(|c| lists_repeated(c, cx));
            }
            _ => {}
        }
    }
}

/// The reads and calls that statements of `list` make (always, when the
/// statement runs: `walk_always`) and the code after them makes again
/// (later statements, the branches of the `if` whose test made them),
/// reused there.
fn repeated(list: &mut [Stmt], cx: &mut Scan) {
    if list.is_empty() {
        return;
    }
    let effects: Vec<Effects> = list.iter().map(|s| of_stmts_in(&cx.cx, std::slice::from_ref(s))).collect();
    for i in 0..list.len() {
        let Some(own) = own_expr(&mut list[i]) else { continue };
        let mut keys: Vec<Key> = Vec::new();
        walk_always(own, &mut |e| {
            if let Some(k) = key_of(e, cx.m, cx.table)
                && !keys.contains(&k)
            {
                keys.push(k);
            }
            true
        });
        for key in keys {
            // Where it is made again (`None`: only counted).
            let (stmt, rest) = list[i..].split_first_mut().unwrap();
            let (n, engine, through) = cx.after_own(stmt, &key, None);
            let (n2, engine2) = if through { cx.scan(rest, Some(&effects[i + 1..]), &key, None).0 } else { (0, false) };
            if n + n2 == 0 {
                continue;
            }
            // The first time: the value kept (with the call count before).
            // (A read may run a getter, a checked call the function as
            // interpreted: then the count moves.)
            let epoch = (matches!(key, Key::Read(..) | Key::Call(_, _, true)) || engine || engine2)
                .then(|| cx.locals.add(Ty::I32));
            let own = own_expr(stmt).expect("as above");
            let mut value = None;
            let locals = &mut cx.locals;
            walk_always(own, &mut |e| {
                if value.is_some() || key_of(e, cx.m, cx.table).as_ref() != Some(&key) {
                    return value.is_none();
                }
                let v = locals.add(e.ty);
                value = Some(v);
                let ty = e.ty;
                let read = std::mem::replace(e, Expr::new(Ty::Any, ExprKind::Undefined));
                let mut seq = Vec::new();
                if let Some(ep) = epoch {
                    let now = Expr::new(Ty::I32, ExprKind::CallEpoch);
                    seq.push(Expr::new(Ty::I32, ExprKind::SetLocal(ep, Box::new(now))));
                }
                seq.push(Expr::new(ty, ExprKind::SetLocal(v, Box::new(read))));
                *e = Expr::new(ty, ExprKind::Seq(seq));
                false
            });
            let value = value.expect("found before");
            if std::env::var_os("LUMINOUS_DEBUG_REUSE").is_some() {
                let what = match &key {
                    Key::Read(_, a) => format!(".{}", cx.m.atoms[*a]),
                    Key::Call(k, ..) => format!("call of {}", func_name(cx.m, *k)),
                };
                eprintln!("reuse: {}: {what} made again {} time(s) (epoch: {})", cx.f.name, n + n2, epoch.is_some());
            }
            // The next times: that value.
            let reuse = Some((value, epoch));
            cx.after_own(stmt, &key, reuse);
            if through {
                cx.scan(rest, Some(&effects[i + 1..]), &key, reuse);
            }
        }
    }
}

/// A function's code, for `repeated`.
struct Scan<'a> {
    m: &'a Module,
    table: &'a Summaries,
    /// The function as it was.
    f: &'a Func,
    cx: crate::effects::Cx<'a>,
    locals: Locals,
}

impl Scan<'_> {
    /// The code statement `s` runs after its own expression (`own_expr`),
    /// scanned for `key` (see `scan`): the number of times it makes it,
    /// whether that code may run code the engine runs, and whether `key`
    /// holds across all of `s`.
    fn after_own(&mut self, s: &mut Stmt, key: &Key, reuse: Option<(LocalId, Option<LocalId>)>) -> (usize, bool, bool) {
        match s {
            Stmt::Expr(_) => {
                let x = of_stmts_in(&self.cx, std::slice::from_ref(s));
                (0, x.engine, holds(key, &x, &written_by(s), self.f, self.table))
            }
            Stmt::If(c, a, b) => {
                let x = of_stmts_in(&self.cx, &[Stmt::Expr(c.clone())]);
                let mut w = Vec::new();
                visit(c, &mut |e| written_locals(e, &mut |l| w.push(l)));
                if !holds(key, &x, &w, self.f, self.table) {
                    return (0, x.engine, false);
                }
                let ((na, ea), ta) = self.scan(a, None, key, reuse);
                let ((nb, eb), tb) = self.scan(b, None, key, reuse);
                (na + nb, x.engine || ea || eb, ta && tb)
            }
            Stmt::Block(b) => {
                let (first, rest) = b.split_first_mut().expect("own_expr found one");
                let (n, e, t) = self.after_own(first, key, reuse);
                if !t {
                    return (n, e, false);
                }
                let ((n2, e2), t2) = self.scan(rest, None, key, reuse);
                (n + n2, e || e2, t2)
            }
            _ => unreachable!("not one own_expr takes"),
        }
    }

    /// The times `stmts` make `key` again, in order, while it holds (by
    /// their `effects`, if known): replaced by its `reuse` if given; with
    /// whether that code may run code the engine runs, and whether `key`
    /// holds across all of `stmts`.
    fn scan(
        &mut self,
        stmts: &mut [Stmt],
        effects: Option<&[Effects]>,
        key: &Key,
        reuse: Option<(LocalId, Option<LocalId>)>,
    ) -> ((usize, bool), bool) {
        let (mut n, mut engine) = (0, false);
        for (j, t) in stmts.iter_mut().enumerate() {
            let x = match effects {
                Some(e) => e[j].clone(),
                None => of_stmts_in(&self.cx, std::slice::from_ref(t)),
            };
            if !holds(key, &x, &written_by(t), self.f, self.table) {
                return ((n, engine), false);
            }
            engine |= x.engine;
            walk_stmts(std::slice::from_mut(t), &mut |e| {
                if key_of(e, self.m, self.table).as_ref() != Some(key) {
                    return true;
                }
                n += 1;
                if let Some((value, epoch)) = reuse {
                    let ty = e.ty;
                    let read = std::mem::replace(e, Expr::new(Ty::Any, ExprKind::Undefined));
                    *e = Expr::new(ty, ExprKind::Reuse { value, epoch, peeked: false, read: Box::new(read) });
                }
                false
            });
        }
        ((n, engine), true)
    }
}

/// The expression statement `s` evaluates first, whenever it runs.
fn own_expr(s: &mut Stmt) -> Option<&mut Expr> {
    match s {
        Stmt::Expr(x) | Stmt::If(x, ..) => Some(x),
        Stmt::Block(b) => own_expr(b.first_mut()?),
        _ => None,
    }
}

fn func_name(m: &Module, k: FuncId) -> &str {
    let n = m.funcs.len();
    if k < n { &m.funcs[k].name } else { m.closures.get(k - n).map_or("?", |c| c.name.as_str()) }
}

/// Hoists the loop-invariant property reads of every function of `m`.
fn hoist_reads(m: &mut Module, table: &Summaries) {
    let n = m.funcs.len();
    for g in 0..n + m.closures.len() {
        let f = if g < n { &m.funcs[g] } else { &m.closures[g - n] };
        let mut has_loop = false;
        stmt_lists(&f.body, &mut |list| has_loop |= list.iter().any(|s| matches!(s, Stmt::Loop { .. })));
        if !has_loop {
            continue;
        }
        // The function as it was, for the effects of its code.
        let before = f.clone();
        let mut body = std::mem::take(if g < n { &mut m.funcs[g].body } else { &mut m.closures[g - n].body });
        let mut locals = Locals::default();
        in_list(&mut body, &before, &mut locals, m, table);
        let f = if g < n { &mut m.funcs[g] } else { &mut m.closures[g - n] };
        f.body = body;
        for ty in locals.added {
            f.locals.push(ty);
            f.boxed.push(false);
            f.local_names.push(None);
        }
    }
}

/// The locals made for a function's reused values.
#[derive(Default)]
struct Locals {
    /// The function's own count.
    base: usize,
    added: Vec<Ty>,
}

impl Locals {
    fn add(&mut self, ty: Ty) -> LocalId {
        self.added.push(ty);
        self.base + self.added.len() - 1
    }
}

fn in_list(list: &mut Vec<Stmt>, f: &Func, locals: &mut Locals, m: &Module, table: &Summaries) {
    locals.base = f.locals.len();
    let mut i = 0;
    while i < list.len() {
        if let Stmt::Loop { .. } = &list[i] {
            let prelude = hoist(&mut list[i], f, locals, m, table);
            let k = prelude.len();
            list.splice(i..i, prelude);
            i += k;
        }
        match &mut list[i] {
            Stmt::If(_, a, b) => {
                in_list(a, f, locals, m, table);
                in_list(b, f, locals, m, table);
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::IterGuard { body, .. } => {
                in_list(body, f, locals, m, table)
            }
            Stmt::Switch { cases, .. } => cases.iter_mut().for_each(|(_, b)| in_list(b, f, locals, m, table)),
            Stmt::Try { body, catch, finally, .. } => {
                in_list(body, f, locals, m, table);
                catch.iter_mut().for_each(|(_, c)| in_list(c, f, locals, m, table));
                finally.iter_mut().for_each(|c| in_list(c, f, locals, m, table));
            }
            _ => {}
        }
        i += 1;
    }
}

/// A read of property `atom` of `base`, if the base is one a `Peek` reads.
fn read_of(e: &Expr) -> Option<(PeekBase, AtomId)> {
    // (Not a read of a record's field, `ShapeGet`: one guarded load,
    // which a reuse would not make quicker.)
    let ExprKind::GetProp(obj, atom) = &e.kind else { return None };
    let atom = *atom;
    let base = match obj.kind {
        ExprKind::Local(l) if l == THIS_LOCAL => PeekBase::This,
        ExprKind::Local(l) => PeekBase::Local(l),
        ExprKind::Cell(c) => PeekBase::Cell(c),
        ExprKind::This => PeekBase::This,
        _ => return None,
    };
    Some((base, atom))
}

/// Calls `f` on `e` and its subexpressions, those of inlined bodies
/// included, but not below an expression for which `f` returns false.
/// (Not into a read already reused, nor a predicted call: its read is one
/// that ran code.)
fn walk(e: &mut Expr, f: &mut dyn FnMut(&mut Expr) -> bool) {
    if matches!(e.kind, ExprKind::Reuse { .. } | ExprKind::Predicted { .. }) || !f(e) {
        return;
    }
    if let Some((body, _)) = body_of(e) {
        walk_stmts(body, f);
    }
    for c in children_mut(e) {
        walk(c, f);
    }
}

fn walk_stmts(stmts: &mut [Stmt], f: &mut dyn FnMut(&mut Expr) -> bool) {
    for s in stmts {
        match s {
            Stmt::Expr(x) | Stmt::Throw(x) => walk(x, f),
            Stmt::Return(x) => x.iter_mut().for_each(|x| walk(x, f)),
            Stmt::Await { value, .. } | Stmt::Yield { value, .. } => walk(value, f),
            Stmt::If(c, a, b) => {
                walk(c, f);
                walk_stmts(a, f);
                walk_stmts(b, f);
            }
            Stmt::Loop { test, body, update, .. } => {
                test.iter_mut().for_each(|x| walk(x, f));
                walk_stmts(body, f);
                update.iter_mut().for_each(|x| walk(x, f));
            }
            Stmt::Block(body) | Stmt::IterGuard { body, .. } => walk_stmts(body, f),
            Stmt::Switch { cases, .. } => {
                for (c, b) in cases.iter_mut() {
                    c.iter_mut().for_each(|x| walk(x, f));
                    walk_stmts(b, f);
                }
            }
            Stmt::Try { body, catch, finally, .. } => {
                walk_stmts(body, f);
                catch.iter_mut().for_each(|(_, c)| walk_stmts(c, f));
                finally.iter_mut().for_each(|c| walk_stmts(c, f));
            }
            Stmt::NewBoxes(_) | Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }
}

/// The reads of loop `s` that may be made before it: the statements that
/// make them (to put before the loop), with the reads rewritten.
fn hoist(s: &mut Stmt, f: &Func, locals: &mut Locals, m: &Module, table: &Summaries) -> Vec<Stmt> {
    let Stmt::Loop { test, body, update, .. } = s else { return Vec::new() };
    // The loop's code, as statements.
    let mut parts: Vec<Stmt> = test.iter().chain(update.iter()).map(|e| Stmt::Expr(e.clone())).collect();
    parts.extend(body.iter().cloned());
    let effects = of_stmts(m, table, f, &parts);
    if effects.writes.all || effects.suspends {
        return Vec::new();
    }
    let mut written = vec![false; f.locals.len()];
    deep_stmt_locals(&parts, &mut |l| {
        if l < written.len() {
            written[l] = true;
        }
    });
    visit_stmts(&parts, &mut |e| {
        written_locals(e, &mut |l| {
            if l < written.len() {
                written[l] = true;
            }
        })
    });
    let ok = |b: PeekBase, a: AtomId| {
        !skipped(m, a)
            && !effects.writes.field(a)
            && match b {
                PeekBase::Local(l) => base_local(f, l) && !written[l],
                PeekBase::Cell(c) => !effects.writes.cell(c),
                PeekBase::This => f.has_this,
            }
    };
    // The reads, in order.
    let mut reads: Vec<(PeekBase, AtomId)> = Vec::new();
    let mut find = |e: &mut Expr| {
        if let Some((b, a)) = read_of(e)
            && ok(b, a)
            && !reads.contains(&(b, a))
        {
            reads.push((b, a));
        }
        true
    };
    test.iter_mut().for_each(|x| walk(x, &mut find));
    update.iter_mut().for_each(|x| walk(x, &mut find));
    walk_stmts(body, &mut find);
    if reads.is_empty() {
        return Vec::new();
    }
    if std::env::var_os("LUMINOUS_DEBUG_REUSE").is_some() {
        let names: Vec<String> = reads
            .iter()
            .map(|&(b, a)| {
                let base = match b {
                    PeekBase::Local(l) => {
                        f.local_names.get(l).copied().flatten().map_or(format!("%{l}"), |n| m.atoms[n].clone())
                    }
                    PeekBase::Cell(c) => format!("cell {c}"),
                    PeekBase::This => "this".into(),
                };
                format!("{base}.{}", m.atoms[a])
            })
            .collect();
        eprintln!("reuse: {}: {} (epoch: {})", f.name, names.join(", "), effects.engine);
    }
    let mut prelude = Vec::new();
    let mut values = Vec::new();
    for &(b, a) in &reads {
        let v = locals.add(Ty::Any);
        values.push(v);
        let peek = Expr::new(Ty::Any, ExprKind::Peek { base: b, atom: a });
        prelude.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(v, Box::new(peek)))));
    }
    // (Code the engine runs is what could change them, if any runs.)
    let epoch = effects.engine.then(|| {
        let e = locals.add(Ty::I32);
        let now = Expr::new(Ty::I32, ExprKind::CallEpoch);
        prelude.push(Stmt::Expr(Expr::new(Ty::I32, ExprKind::InitLocal(e, Box::new(now)))));
        e
    });
    let mut rewrite = |e: &mut Expr| {
        if let Some(r) = read_of(e)
            && let Some(k) = reads.iter().position(|x| *x == r)
        {
            let read = std::mem::replace(e, Expr::new(Ty::Any, ExprKind::Undefined));
            *e = Expr::new(Ty::Any, ExprKind::Reuse { value: values[k], epoch, peeked: true, read: Box::new(read) });
            return false;
        }
        true
    };
    test.iter_mut().for_each(|x| walk(x, &mut rewrite));
    update.iter_mut().for_each(|x| walk(x, &mut rewrite));
    walk_stmts(body, &mut rewrite);
    prelude
}
