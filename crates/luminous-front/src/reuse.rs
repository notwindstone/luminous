//! Reads that survive calls: a property read in a loop whose object the
//! loop does not change, and whose property the loop's code (the
//! functions it calls included, by their effect summaries) writes nowhere,
//! is made once before the loop, where it can be without running code or
//! throwing (`ExprKind::Peek`: a data property of ordinary objects). In
//! the loop the read is then that value (`ExprKind::Reuse`) while the
//! engine has run no code native code does not call directly since (its
//! call count, `js_call_epoch`: a getter, a setter, a coercion, a call
//! through a value, any of which could write it); else the read is made as
//! before, which also throws, or runs a getter, exactly where it did.

use crate::effects::{Summaries, of_stmts};
use crate::ir::*;

/// Hoists the loop-invariant property reads of every function of `m`.
pub fn hoist_reads(m: &mut Module, table: &Summaries) {
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
    // (`length` is mostly of arrays and strings, which are not ordinary
    // objects: their own reads are quick.)
    let skip = |a: AtomId| matches!(m.atoms[a].as_str(), "__proto__" | "length");
    let ok = |b: PeekBase, a: AtomId| {
        !skip(a)
            && !effects.writes.field(a)
            && match b {
                // (A parameter `arguments` may also assign is left out.)
                PeekBase::Local(l) => {
                    l < f.locals.len()
                        && f.locals[l] == Ty::Any
                        && !written[l]
                        && !f.boxed[l]
                        && !(f.arguments && l < f.params.len())
                }
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
            *e = Expr::new(Ty::Any, ExprKind::Reuse { value: values[k], epoch, read: Box::new(read) });
            return false;
        }
        true
    };
    test.iter_mut().for_each(|x| walk(x, &mut rewrite));
    update.iter_mut().for_each(|x| walk(x, &mut rewrite));
    walk_stmts(body, &mut rewrite);
    prelude
}
