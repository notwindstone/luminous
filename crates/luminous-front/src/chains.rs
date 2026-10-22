//! Array method chains as one loop: `xs.filter(f).map(g)`, and such chains
//! of `filter` and `map` ending with `.length` or `.reduce(h, init)`
//! (`ExprKind::ArrayChain`), where the callbacks (arrow functions inline,
//! see `inline_callbacks`) are made of what can run no code: their
//! parameters, property reads of them (`Peek`), arithmetic and
//! comparisons of numbers, constants.
//!
//! Run as one loop, each element goes through every callback before the
//! next element does, where the chain as written runs each callback over
//! all elements in turn, and makes the arrays in between. Neither can be
//! told apart while the callbacks run no code and throw nothing, and
//! making the arrays runs no code (the original builtins, of an array
//! whose species is `Array`): so the loop checks that the receiver is such
//! an array (`lum_chain_ok`), and that each value a callback works on is
//! as it needs (`Sure`: a data property that a `Peek` found, a number);
//! where one is not, the loop stops and the chain as written runs from the
//! start, which is as if the loop had not run.

use crate::ir::*;

/// Fuses the array method chains of every function of `m`.
pub fn fuse_chains(m: &mut Module) {
    let nfuncs = m.funcs.len();
    let reduce = m.atoms.iter().position(|a| a == "reduce");
    let proto = m.atoms.iter().position(|a| a == "__proto__");
    let closures: Vec<Func> = m.closures.clone();
    let debug = std::env::var_os("LUMINOUS_DEBUG_CHAINS").is_some();
    for g in 0..nfuncs + closures.len() {
        let f = if g < nfuncs { &mut m.funcs[g] } else { &mut m.closures[g - nfuncs] };
        let mut cx = Cx { nfuncs, closures: &closures, reduce, proto, f, fused: 0 };
        let mut body = std::mem::take(&mut cx.f.body);
        walk_stmts(&mut body, &mut cx);
        if debug && cx.fused > 0 {
            eprintln!("chains: {}: {} fused", cx.f.name, cx.fused);
        }
        cx.f.body = body;
    }
}

struct Cx<'a> {
    nfuncs: usize,
    closures: &'a [Func],
    reduce: Option<AtomId>,
    proto: Option<AtomId>,
    f: &'a mut Func,
    fused: usize,
}

impl Cx<'_> {
    fn new_local(&mut self) -> LocalId {
        self.f.locals.push(Ty::Any);
        self.f.boxed.push(false);
        self.f.local_names.push(None);
        self.f.locals.len() - 1
    }

    /// `e` as a stage: an inline `filter` or `map` whose callback takes at
    /// most the element, and whose value runs no code (`pure`).
    fn stage(&self, e: &Expr) -> Option<ChainStage> {
        let ExprKind::ArrayLoop { kind: kind @ (LoopKind::Filter | LoopKind::Map), func, body, locals, .. } = &e.kind
        else {
            return None;
        };
        let t = self.closures.get(func.checked_sub(self.nfuncs)?)?;
        if t.params.len() > 1 || t.params.iter().any(|&p| p != Ty::Any) {
            return None;
        }
        let [Stmt::Return(Some(x))] = body.as_slice() else { return None };
        let param = (t.params.len() == 1).then(|| locals[0]);
        Some(ChainStage { kind: *kind, param, value: self.pure(x)? })
    }

    /// `e` without anything that could run code or throw: its JS values
    /// read where they are as needed (`Sure`), else `None`.
    fn pure(&self, e: &Expr) -> Option<Expr> {
        use ExprKind as K;
        let kind = match &e.kind {
            K::Num(_) | K::Bool(_) | K::Undefined | K::Null | K::Str(_) => e.kind.clone(),
            K::Local(l) if *l != THIS_LOCAL && !self.f.boxed[*l] => K::Local(*l),
            K::GetProp(o, a) | K::ShapeGet { obj: o, atom: a, .. } => {
                let K::Local(l) = o.kind else { return None };
                if l == THIS_LOCAL || self.f.boxed[l] || Some(*a) == self.proto {
                    return None;
                }
                let peek = Expr::new(Ty::Any, K::Peek { base: PeekBase::Local(l), atom: *a });
                K::Sure(Box::new(peek), Sure::Peeked)
            }
            K::Box(x) => K::Box(Box::new(self.pure(x)?)),
            K::Binary(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem), a, b)
            | K::NumArith(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem), a, b) => {
                return Some(Expr::new(Ty::F64, K::Binary(*op, self.num(a)?, self.num(b)?)));
            }
            K::AnyBinary(op, a, b) => {
                let op = match op {
                    AnyBinOp::Add => BinOp::Add,
                    AnyBinOp::Sub => BinOp::Sub,
                    AnyBinOp::Mul => BinOp::Mul,
                    AnyBinOp::Div => BinOp::Div,
                    AnyBinOp::Rem => BinOp::Rem,
                    _ => return None,
                };
                let x = Expr::new(Ty::F64, K::Binary(op, self.num(a)?, self.num(b)?));
                K::Box(Box::new(x))
            }
            K::Compare(op, a, b) if a.ty == Ty::F64 && b.ty == Ty::F64 => K::Compare(*op, self.num(a)?, self.num(b)?),
            K::AnyCompare(op, a, b) => {
                // (Of two numbers, `==` is `===`.)
                let op = match op {
                    AnyCmpOp::Lt => CmpOp::Lt,
                    AnyCmpOp::Le => CmpOp::Le,
                    AnyCmpOp::Gt => CmpOp::Gt,
                    AnyCmpOp::Ge => CmpOp::Ge,
                    AnyCmpOp::Eq | AnyCmpOp::StrictEq => CmpOp::Eq,
                    AnyCmpOp::Ne | AnyCmpOp::StrictNe => CmpOp::Ne,
                    _ => return None,
                };
                K::Compare(op, self.num(a)?, self.num(b)?)
            }
            K::Unary(op @ (UnOp::Neg | UnOp::Not), x) => K::Unary(*op, Box::new(self.pure(x)?)),
            K::Logical(op @ (LogicalOp::And | LogicalOp::Or), a, b) if e.ty == Ty::Bool => {
                K::Logical(*op, Box::new(self.pure(a)?), Box::new(self.pure(b)?))
            }
            K::Cond(c, a, b) if a.ty == e.ty && b.ty == e.ty => {
                K::Cond(Box::new(self.pure(c)?), Box::new(self.pure(a)?), Box::new(self.pure(b)?))
            }
            K::Truthy(x) => K::Truthy(Box::new(self.pure(x)?)),
            _ => return None,
        };
        Some(Expr::new(e.ty, kind))
    }

    /// `e` as a number (`F64`) that runs no code: a JS value only if it is
    /// one (`Sure`).
    fn num(&self, e: &Expr) -> Option<Box<Expr>> {
        match (&e.kind, e.ty) {
            (_, Ty::F64) => Some(Box::new(self.pure(e)?)),
            (ExprKind::Box(x), Ty::Any) if x.ty == Ty::F64 => Some(Box::new(self.pure(x)?)),
            (_, Ty::Any) => Some(Box::new(Expr::new(Ty::F64, ExprKind::Sure(Box::new(self.pure(e)?), Sure::Number)))),
            _ => None,
        }
    }

    /// `e` as a chain, if it is one: made a `Seq` of the receiver's
    /// evaluation into a local and the `ArrayChain` over it.
    fn fuse(&mut self, e: &mut Expr) -> bool {
        // The end, and the chain's last stage.
        let (end, last): (ChainEnd, &Expr) = match &e.kind {
            ExprKind::Length(c, _) if self.stage(c).is_some() => (ChainEnd::Length, c),
            ExprKind::CallMethod(c, Key::Atom(a), args) if Some(*a) == self.reduce && self.stage(c).is_some() => {
                let [cb, init] = args.as_slice() else { return false };
                let ExprKind::Closure(cid, caps) = &cb.kind else { return false };
                let Some(end) = self.reduce_end(*cid, caps, init) else { return false };
                (end, c)
            }
            ExprKind::ArrayLoop { recv, .. } if self.stage(e).is_some() && self.stage(recv).is_some() => {
                (ChainEnd::Array, e)
            }
            _ => return false,
        };
        // The stages, last first, down to the receiver.
        let mut stages = Vec::new();
        let mut at = last;
        while let Some(s) = self.stage(at) {
            stages.push(s);
            let ExprKind::ArrayLoop { recv, .. } = &at.kind else { unreachable!() };
            at = recv;
        }
        stages.reverse();
        // The chain as written, over the receiver's value in a local.
        let t = self.new_local();
        let ty = e.ty;
        let mut whole = std::mem::replace(e, Expr::new(Ty::Any, ExprKind::Undefined));
        let mut slot = match &mut whole.kind {
            ExprKind::Length(c, _) | ExprKind::CallMethod(c, ..) => &mut **c,
            _ => &mut whole,
        };
        for _ in 0..stages.len() {
            let ExprKind::ArrayLoop { recv, .. } = &mut slot.kind else { unreachable!() };
            slot = recv;
        }
        let recv = std::mem::replace(slot, Expr::new(Ty::Any, ExprKind::Local(t)));
        let init = Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(recv)));
        let chain = Expr::new(ty, ExprKind::ArrayChain { recv: t, stages, end, whole: Box::new(whole) });
        *e = Expr::new(ty, ExprKind::Seq(vec![init, chain]));
        self.fused += 1;
        true
    }

    /// `reduce`'s end, for arrow function `cid` (capturing the caller's
    /// `caps`) taking at most the accumulator and the element, whose value
    /// runs no code, and an `init` that runs none (no `Sure` either).
    fn reduce_end(&mut self, cid: usize, caps: &[LocalId], init: &Expr) -> Option<ChainEnd> {
        let t = self.closures.get(cid)?;
        if t.params.len() > 2
            || t.params.iter().any(|&p| p != Ty::Any)
            || t.is_async
            || t.is_generator
            || t.has_this
            || t.rest
            || t.arguments
        {
            return None;
        }
        let [Stmt::Return(Some(x))] = t.body.as_slice() else { return None };
        let init = self.pure(init)?;
        let mut sure = false;
        visit(&init, &mut |e| sure |= matches!(e.kind, ExprKind::Sure(..)));
        if sure {
            return None;
        }
        // Its parameters as new locals of the caller; its captured
        // variables the caller's own; no other locals.
        const OTHER: LocalId = usize::MAX - 1;
        let params: Vec<LocalId> = (0..t.params.len()).map(|_| self.new_local()).collect();
        let mut other = false;
        let map = |l: LocalId| {
            if let Some(j) = t.captures.iter().position(|&c| c == l) {
                caps[j]
            } else if l < params.len() {
                params[l]
            } else {
                OTHER
            }
        };
        let mut x = x.clone();
        visit_mut(&mut x, &mut |e| remap_locals(e, &map));
        visit(&x, &mut |e| other |= matches!(e.kind, ExprKind::Local(OTHER)));
        if other {
            return None;
        }
        let value = self.pure(&x)?;
        Some(ChainEnd::Reduce {
            acc: params.first().copied(),
            elem: params.get(1).copied(),
            value: Box::new(value),
            init: Box::new(init),
        })
    }
}

/// Fuses the chains in `e` and below it (not in a chain made already).
fn walk(e: &mut Expr, cx: &mut Cx) {
    if matches!(e.kind, ExprKind::ArrayChain { .. }) || cx.fuse(e) {
        return;
    }
    if let Some((body, _)) = body_of(e) {
        walk_stmts(body, cx);
    }
    for c in children_mut(e) {
        walk(c, cx);
    }
}

fn walk_stmts(stmts: &mut [Stmt], cx: &mut Cx) {
    for s in stmts {
        match s {
            Stmt::Expr(x) | Stmt::Throw(x) => walk(x, cx),
            Stmt::Return(x) => x.iter_mut().for_each(|x| walk(x, cx)),
            Stmt::Await { value, .. } | Stmt::Yield { value, .. } => walk(value, cx),
            Stmt::If(c, a, b) => {
                walk(c, cx);
                walk_stmts(a, cx);
                walk_stmts(b, cx);
            }
            Stmt::Loop { test, body, update, .. } => {
                test.iter_mut().for_each(|x| walk(x, cx));
                walk_stmts(body, cx);
                update.iter_mut().for_each(|x| walk(x, cx));
            }
            Stmt::Block(body) | Stmt::IterGuard { body, .. } => walk_stmts(body, cx),
            Stmt::Switch { cases, .. } => {
                for (c, b) in cases.iter_mut() {
                    c.iter_mut().for_each(|x| walk(x, cx));
                    walk_stmts(b, cx);
                }
            }
            Stmt::Try { body, catch, finally, .. } => {
                walk_stmts(body, cx);
                catch.iter_mut().for_each(|(_, c)| walk_stmts(c, cx));
                finally.iter_mut().for_each(|c| walk_stmts(c, cx));
            }
            Stmt::NewBoxes(_) | Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }
}
