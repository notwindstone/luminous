//! Escape analysis and allocation sinking support.
//!
//! `fold_flag`: the passes that keep an object virtual (`virtualize_objects`,
//! `virtualize_instances`) test a flag local at each use, which says
//! whether the object is still virtual or has been made. Along the code
//! that follows its declaration, the flag is often known: true until the
//! first place that makes the object, false after one that surely ran. The
//! tests whose answer is known are folded away.

use std::collections::HashMap;

use crate::ir::*;

/// What a flag local holds at a point of the code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum St {
    /// The point is not reached.
    Never,
    Yes,
    No,
    Maybe,
}

fn join(a: St, b: St) -> St {
    match (a, b) {
        (St::Never, x) | (x, St::Never) => x,
        (x, y) if x == y => x,
        _ => St::Maybe,
    }
}

/// Fold the tests of flag local `flag` (`Cond(Local(flag), a, b)`) whose
/// answer is known, in function `f`. The flag is set only by
/// `InitLocal(flag, true)` (the object's declaration) and by
/// `SetLocal(flag, false)` (where it is made), or else by other writes,
/// which make it unknown.
pub fn fold_flag(f: &mut Func, flag: LocalId) {
    let mut cx = Fold { flag, breaks: HashMap::new(), rets: Vec::new() };
    let mut body = std::mem::take(&mut f.body);
    cx.stmts(&mut body, St::Maybe);
    f.body = body;
}

struct Fold {
    flag: LocalId,
    /// The states at `break`s, by target (only for precision's sake: the
    /// loops' own states cover them).
    breaks: HashMap<LoopId, St>,
    /// Per inlined body being folded: the join of the states at its
    /// `return`s.
    rets: Vec<St>,
}

/// How a piece of code writes the flag: to false only, or otherwise.
#[derive(Default)]
struct Writes {
    to_false: bool,
    other: bool,
}

impl Fold {
    fn note(&self, e: &Expr, w: &mut Writes) {
        match &e.kind {
            ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v) if *l == self.flag => {
                if matches!(v.kind, ExprKind::Bool(false)) && matches!(e.kind, ExprKind::SetLocal(..)) {
                    w.to_false = true;
                } else {
                    w.other = true;
                }
            }
            _ => {}
        }
    }

    fn writes_stmts(&self, ss: &[Stmt]) -> Writes {
        let mut w = Writes::default();
        visit_stmts(ss, &mut |e| self.note(e, &mut w));
        w
    }

    fn writes_expr(&self, x: &Expr) -> Writes {
        let mut w = Writes::default();
        visit(x, &mut |e| self.note(e, &mut w));
        w
    }

    /// The state that holds throughout code that writes the flag as `w`
    /// says, reached in state `st` (and maybe again from its end).
    fn throughout(st: St, w: &Writes) -> St {
        if st == St::Never {
            St::Never
        } else if w.other {
            St::Maybe
        } else if w.to_false {
            join(st, St::No)
        } else {
            st
        }
    }

    fn stmts(&mut self, ss: &mut [Stmt], mut st: St) -> St {
        for s in ss {
            st = self.stmt(s, st);
        }
        st
    }

    fn stmt(&mut self, s: &mut Stmt, st: St) -> St {
        if st == St::Never {
            return St::Never;
        }
        match s {
            Stmt::Expr(e) => self.expr(e, st),
            Stmt::If(c, a, b) => {
                let st = self.expr(c, st);
                let x = self.stmts(a, st);
                let y = self.stmts(b, st);
                join(x, y)
            }
            Stmt::Block(b) => self.stmts(b, st),
            Stmt::Return(x) => {
                let st = match x {
                    Some(x) => self.expr(x, st),
                    None => st,
                };
                if let Some(r) = self.rets.last_mut() {
                    *r = join(*r, st);
                }
                St::Never
            }
            Stmt::Throw(x) => {
                self.expr(x, st);
                St::Never
            }
            Stmt::Break(id) => {
                let e = self.breaks.entry(*id).or_insert(St::Never);
                *e = join(*e, st);
                St::Never
            }
            Stmt::Continue(_) => St::Never,
            Stmt::Await { value, .. } | Stmt::Yield { value, .. } => self.expr(value, st),
            Stmt::NewBoxes(_) => st,
            Stmt::Loop { .. } | Stmt::Switch { .. } | Stmt::Try { .. } | Stmt::IterGuard { .. } => {
                let w = self.writes_stmts(std::slice::from_ref(s));
                let inner = Self::throughout(st, &w);
                match s {
                    Stmt::Loop { test, body, update, .. } => {
                        if let Some(t) = test {
                            self.expr(t, inner);
                        }
                        self.stmts(body, inner);
                        if let Some(u) = update {
                            self.expr(u, inner);
                        }
                    }
                    Stmt::Switch { cases, .. } => {
                        for (c, b) in cases {
                            if let Some(c) = c {
                                self.expr(c, inner);
                            }
                            self.stmts(b, inner);
                        }
                    }
                    Stmt::Try { body, catch, finally, .. } => {
                        self.stmts(body, inner);
                        if let Some((_, c)) = catch {
                            self.stmts(c, inner);
                        }
                        if let Some(fin) = finally {
                            self.stmts(fin, inner);
                        }
                    }
                    Stmt::IterGuard { body, .. } => {
                        self.stmts(body, inner);
                    }
                    _ => unreachable!(),
                }
                inner
            }
        }
    }

    /// A body whose `return`s end it (inlined, predicted).
    fn body(&mut self, b: &mut [Stmt], st: St) -> St {
        self.rets.push(St::Never);
        let end = self.stmts(b, st);
        let r = self.rets.pop().unwrap();
        join(r, end)
    }

    fn expr(&mut self, e: &mut Expr, st: St) -> St {
        if st == St::Never {
            return St::Never;
        }
        // A known test: the branch taken, in place of the test.
        if let ExprKind::Cond(c, a, b) = &mut e.kind {
            let known = match c.kind {
                ExprKind::Local(l) if l == self.flag && st != St::Maybe => Some(st == St::Yes),
                ExprKind::Bool(v) => Some(v),
                _ => None,
            };
            if let Some(v) = known {
                let taken =
                    std::mem::replace(if v { &mut **a } else { &mut **b }, Expr::new(Ty::Void, ExprKind::Undefined));
                *e = taken;
                return self.expr(e, st);
            }
        }
        if matches!(e.kind, ExprKind::Predicted { .. } | ExprKind::ArrayLoop { .. } | ExprKind::Lazy(_)) {
            // The call, or the body instead: what holds throughout.
            let w = self.writes_expr(e);
            let inner = Self::throughout(st, &w);
            for c in children_mut(e) {
                self.expr(c, inner);
            }
            if let ExprKind::Predicted { body, .. } | ExprKind::ArrayLoop { body, .. } = &mut e.kind {
                self.body(body, inner);
            }
            return inner;
        }
        match &mut e.kind {
            ExprKind::Cond(c, a, b) => {
                let st = self.expr(c, st);
                let x = self.expr(a, st);
                let y = self.expr(b, st);
                join(x, y)
            }
            ExprKind::Logical(_, a, b) => {
                let x = self.expr(a, st);
                let y = self.expr(b, x);
                join(x, y)
            }
            ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v) if *l == self.flag => {
                let after = self.expr(v, st);
                match v.kind {
                    _ if after == St::Never => St::Never,
                    ExprKind::Bool(true) => St::Yes,
                    ExprKind::Bool(false) => St::No,
                    _ => St::Maybe,
                }
            }
            ExprKind::Inline { body, .. } => self.body(body, st),
            _ => {
                let mut st = st;
                for c in children_mut(e) {
                    st = self.expr(c, st);
                }
                st
            }
        }
    }
}
