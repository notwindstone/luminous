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
            // (The function object, and the arguments it is given, only if
            // the direct call declines.)
            ExprKind::CallChecked { args, callee, slow, .. }
            | ExprKind::CallEntry { args, callee: Some(callee), slow, .. } => {
                let mut st = st;
                for a in args {
                    st = self.expr(a, st);
                }
                let mut other = self.expr(callee, st);
                for x in slow.iter_mut().flatten() {
                    other = self.expr(x, other);
                }
                join(st, other)
            }
            // (Each branch of a test of the flag knows its answer.)
            ExprKind::Cond(c, a, b) if matches!(c.kind, ExprKind::Local(l) if l == self.flag) => {
                let x = self.expr(a, St::Yes);
                let y = self.expr(b, St::No);
                join(x, y)
            }
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

// ---- Escape summaries ----

/// A field of the object a parameter receives: a property by name, or a
/// field of an `aot type` by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Prop(AtomId),
    Slot(u32),
}

/// What a function does with the object one of its parameters receives.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Use {
    /// The object does not escape the call, which only reads these fields
    /// (sorted). `outlives`: they may be read after the call is over (by
    /// an async function it passes the object to, without awaiting it).
    Reads {
        fields: Vec<Field>,
        outlives: bool,
    },
    /// The object does not escape, but fields are written (`fields`: the
    /// fields read or written).
    Writes {
        fields: Vec<Field>,
        outlives: bool,
    },
    Escapes,
}

impl Use {
    fn none() -> Use {
        Use::Reads { fields: Vec::new(), outlives: false }
    }

    pub fn join(&self, other: &Use) -> Use {
        let merge = |a: &[Field], b: &[Field]| {
            let mut v: Vec<Field> = a.iter().chain(b).copied().collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        match (self, other) {
            (Use::Escapes, _) | (_, Use::Escapes) => Use::Escapes,
            (Use::Reads { fields: a, outlives: x }, Use::Reads { fields: b, outlives: y }) => {
                Use::Reads { fields: merge(a, b), outlives: *x || *y }
            }
            (
                Use::Reads { fields: a, outlives: x } | Use::Writes { fields: a, outlives: x },
                Use::Reads { fields: b, outlives: y } | Use::Writes { fields: b, outlives: y },
            ) => Use::Writes { fields: merge(a, b), outlives: *x || *y },
        }
    }

    /// The same, for a call that may still run after the caller is over.
    fn outliving(self) -> Use {
        match self {
            Use::Reads { fields, .. } => Use::Reads { fields, outlives: true },
            Use::Writes { fields, .. } => Use::Writes { fields, outlives: true },
            Use::Escapes => Use::Escapes,
        }
    }

    /// The fields only read, if that is all the call does.
    pub fn reads(&self) -> Option<(&[Field], bool)> {
        match self {
            Use::Reads { fields, outlives } => Some((fields, *outlives)),
            _ => None,
        }
    }
}

fn func_at(m: &Module, g: usize) -> &Func {
    if g < m.funcs.len() { &m.funcs[g] } else { &m.closures[g - m.funcs.len()] }
}

/// The `aot type` layouts whose objects `scalar_replace_structs` can keep
/// in locals (one member, every field in every object).
pub(crate) fn plain_layout(layouts: &[Layout], k: u32) -> bool {
    let l = &layouts[k as usize];
    l.members.len() == 1 && l.fields.iter().all(|f| f.universal)
}

/// Whether parameter `i` of `f` can be summarized as anything but
/// `Escapes`: a JS value (or an `aot type` object kept in locals) in a
/// local never assigned, not boxed, and no `arguments` object aliasing it.
fn candidate(f: &Func, i: usize, layouts: &[Layout], written: &[bool]) -> bool {
    let ty_ok = match f.params[i] {
        Ty::Any => f.locals[i] == Ty::Any,
        Ty::Struct(k) => f.locals[i] == Ty::Struct(k) && plain_layout(layouts, k),
        _ => false,
    };
    ty_ok && !f.is_generator && !f.is_closure && !f.arguments && f.rest_param() != Some(i) && !f.boxed[i] && !written[i]
}

/// The locals of `f` that are assigned anywhere (or named by statements).
fn written(f: &Func) -> Vec<bool> {
    let mut w = vec![false; f.locals.len()];
    deep_stmt_locals(&f.body, &mut |l| w[l] = true);
    visit_stmts(&f.body, &mut |e| written_locals(e, &mut |l| w[l] = true));
    w
}

/// The values of `await` statements in `stmts` (by address).
pub(crate) fn awaited(stmts: &[Stmt]) -> std::collections::HashSet<*const Expr> {
    let mut out = std::collections::HashSet::new();
    stmt_lists(stmts, &mut |list| {
        for s in list {
            if let Stmt::Await { value, .. } = s {
                out.insert(value as *const Expr);
            }
        }
    });
    out
}

/// The functions `f` calls directly.
fn callees(f: &Func) -> Vec<usize> {
    let mut out = Vec::new();
    visit_stmts(&f.body, &mut |e| {
        if let ExprKind::Call(k, _) | ExprKind::CallEntry { func: k, .. } | ExprKind::CallDirectThis(k, ..) = &e.kind
            && !out.contains(k)
        {
            out.push(*k);
        }
    });
    out
}

/// What function `g` does with its parameters, given the summaries of the
/// functions it calls (`table`).
fn summarize(m: &Module, g: usize, table: &[Vec<Use>]) -> Vec<Use> {
    let f = func_at(m, g);
    let n = f.params.len();
    let w = written(f);
    let mut out: Vec<Use> =
        (0..n).map(|i| if candidate(f, i, &m.layouts, &w) { Use::none() } else { Use::Escapes }).collect();
    if out.iter().all(|u| *u == Use::Escapes) {
        return out;
    }
    let mut total = vec![0usize; n];
    each_name(&f.body, &mut |l| {
        if l < n {
            total[l] += 1;
        }
    });
    let mut seen = vec![0usize; n];
    let awaits = awaited(&f.body);
    let param = |x: &Expr| match x.kind {
        ExprKind::Local(l) if l < n => Some(l),
        _ => None,
    };
    visit_stmts(&f.body, &mut |e| {
        if let Some((l, _)) = nullish_compare(e)
            && l < n
        {
            seen[l] += 1;
            return;
        }
        let mut add = |i: usize, u: Use, seen: &mut Vec<usize>| {
            seen[i] += 1;
            out[i] = out[i].join(&u);
        };
        let read = |fd: Field| Use::Reads { fields: vec![fd], outlives: false };
        let write = |fd: Field| Use::Writes { fields: vec![fd], outlives: false };
        match &e.kind {
            ExprKind::GetProp(o, a) | ExprKind::ShapeGet { obj: o, atom: a, .. } => {
                if let Some(i) = param(o) {
                    add(i, read(Field::Prop(*a)), &mut seen);
                }
            }
            ExprKind::SetProp(o, a, _) | ExprKind::ShapeSet { obj: o, atom: a, .. } => {
                if let Some(i) = param(o) {
                    add(i, write(Field::Prop(*a)), &mut seen);
                }
            }
            ExprKind::StructGet(_, o, fi) => {
                if let Some(i) = param(o) {
                    add(i, read(Field::Slot(*fi)), &mut seen);
                }
            }
            ExprKind::StructSet(_, o, fi, _) => {
                if let Some(i) = param(o) {
                    add(i, write(Field::Slot(*fi)), &mut seen);
                }
            }
            ExprKind::Truthy(x) => {
                if let Some(i) = param(x) {
                    add(i, Use::none(), &mut seen);
                }
            }
            ExprKind::Call(k, args)
            | ExprKind::CallEntry { func: k, args, .. }
            | ExprKind::CallDirectThis(k, _, args) => {
                let entry = matches!(e.kind, ExprKind::CallEntry { .. });
                // (An entry that may decline calls the function object with
                // the object: the function as interpreted.)
                let declines = matches!(e.kind, ExprKind::CallEntry { callee: Some(_), .. });
                for (q, a) in args.iter().enumerate() {
                    let Some(i) = param(a) else { continue };
                    let u = match table[*k].get(q) {
                        Some(u) if !declines => u.clone(),
                        _ => Use::Escapes,
                    };
                    // (An async callee not awaited may still run after this
                    // function is over.)
                    let u = if entry && !awaits.contains(&(e as *const Expr)) { u.outliving() } else { u };
                    add(i, u, &mut seen);
                }
            }
            _ => {}
        }
    });
    for i in 0..n {
        if seen[i] != total[i] {
            out[i] = Use::Escapes;
        }
    }
    out
}

/// The strongly connected components of the call graph, callees first
/// (Tarjan's algorithm).
fn components(succ: &[Vec<usize>]) -> Vec<Vec<usize>> {
    struct T<'a> {
        succ: &'a [Vec<usize>],
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        out: Vec<Vec<usize>>,
    }
    fn visit(t: &mut T, v: usize) {
        t.index[v] = Some(t.next);
        t.low[v] = t.next;
        t.next += 1;
        t.stack.push(v);
        t.on[v] = true;
        for &w in &t.succ[v] {
            match t.index[w] {
                None => {
                    visit(t, w);
                    t.low[v] = t.low[v].min(t.low[w]);
                }
                Some(iw) if t.on[w] => t.low[v] = t.low[v].min(iw),
                _ => {}
            }
        }
        if Some(t.low[v]) == t.index[v] {
            let mut c = Vec::new();
            loop {
                let w = t.stack.pop().unwrap();
                t.on[w] = false;
                c.push(w);
                if w == v {
                    break;
                }
            }
            t.out.push(c);
        }
    }
    let n = succ.len();
    let mut t = T {
        succ,
        index: vec![None; n],
        low: vec![0; n],
        on: vec![false; n],
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };
    for v in 0..n {
        if t.index[v].is_none() {
            visit(&mut t, v);
        }
    }
    t.out
}

/// What every function (module functions, then closures) does with the
/// objects its parameters receive: bottom-up over the call graph, each
/// strongly connected component (recursion) to a fixpoint, from "reads
/// nothing" up.
pub fn summaries(m: &Module) -> Vec<Vec<Use>> {
    let n = m.funcs.len() + m.closures.len();
    let succ: Vec<Vec<usize>> =
        (0..n).map(|g| callees(func_at(m, g)).into_iter().filter(|&k| k < n).collect()).collect();
    let mut table: Vec<Vec<Use>> = (0..n).map(|g| vec![Use::none(); func_at(m, g).params.len()]).collect();
    for comp in components(&succ) {
        loop {
            let mut changed = false;
            for &g in &comp {
                let s = summarize(m, g, &table);
                if s != table[g] {
                    table[g] = s;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
    table
}

// ---- Specialized clones ----

/// Most clones made of one function, and in all.
const CLONES_PER_FUNCTION: usize = 8;
const CLONES: usize = 256;

/// Specialized clones (see `replace_objects`): copies of functions whose
/// parameters that receive objects they only read are replaced by the
/// fields they read, one parameter each, so that the caller passes the
/// fields and no object needs to exist. They are added after the closures
/// (`Module::closures`; not closures themselves), and only called directly.
pub struct Spec {
    /// Per function, per parameter: what it does with an object (see
    /// `summaries`).
    pub uses: Vec<Vec<Use>>,
    /// The functions as they are cloned (module functions only).
    templates: Vec<Option<Func>>,
    memo: HashMap<(usize, Vec<(usize, Vec<Ty>)>), Option<usize>>,
    per_function: Vec<usize>,
    /// The clones made, from `base` on; and per clone, what it is: its
    /// function, its `clone_of` key, and per parameter the function's own
    /// parameter it is, if it is one (not a field).
    pub clones: Vec<Func>,
    pub base: usize,
    origins: Vec<(usize, Vec<(usize, Vec<Ty>)>, Vec<Option<usize>>)>,
}

impl Spec {
    pub fn new(m: &Module) -> Spec {
        let uses = summaries(m);
        let templates =
            m.funcs.iter().map(|f| (!f.is_generator && !f.is_closure && !f.arguments).then(|| f.clone())).collect();
        let n = m.funcs.len() + m.closures.len();
        if std::env::var_os("LUMINOUS_DEBUG_ESCAPE").is_some() {
            for (g, u) in uses.iter().enumerate().take(m.funcs.len()) {
                eprintln!("escape {} {}: {u:?}", g, func_at(m, g).name);
            }
        }
        Spec {
            uses,
            templates,
            memo: HashMap::new(),
            per_function: vec![0; n],
            clones: Vec::new(),
            base: n,
            origins: Vec::new(),
        }
    }

    /// Function `k` as a function cloned, and its parameter `q` as that
    /// function's (a clone's parameter that is one of its function's).
    fn origin(&self, k: usize, q: usize) -> Option<(usize, usize)> {
        if k < self.base {
            return Some((k, q));
        }
        let (t, _, params) = self.origins.get(k - self.base)?;
        Some((*t, (*params.get(q)?)?))
    }

    /// What function `k` (any, clones too) does with the object parameter
    /// `q` receives, if `k` can be cloned for it: the fields it reads, and
    /// whether they may be read after the call is over.
    pub fn reads(&self, k: usize, q: usize) -> Option<(&[Field], bool)> {
        let (t, q) = self.origin(k, q)?;
        self.templates.get(t)?.as_ref()?;
        self.uses.get(t)?.get(q)?.reads()
    }

    /// Whether function `k` suspends (an async callee whose reads may
    /// happen after the call returns its promise).
    pub fn suspends(&self, k: usize) -> bool {
        let t = match k.checked_sub(self.base) {
            None => k,
            Some(c) => match self.origins.get(c) {
                Some(o) => o.0,
                None => return false,
            },
        };
        self.templates.get(t).and_then(Option::as_ref).is_some_and(|f| f.suspends())
    }

    /// The clone of function `k` whose parameters `objs` (index, and the
    /// types of the fields it reads: see `reads`) take those fields instead
    /// of the object, in their place, in the order of `reads`.
    pub fn clone_of(&mut self, k: usize, objs: &[(usize, Vec<Ty>)]) -> Option<usize> {
        // (Of a clone: of its function, with both its objects and these.)
        if k >= self.base {
            let (t, mut key, _) = self.origins.get(k - self.base)?.clone();
            for (q, tys) in objs {
                let (_, q) = self.origin(k, *q)?;
                key.push((q, tys.clone()));
            }
            return self.clone_of(t, &key);
        }
        let mut key: Vec<(usize, Vec<Ty>)> = objs.to_vec();
        key.sort_by_key(|(q, _)| *q);
        if let Some(&id) = self.memo.get(&(k, key.clone())) {
            return id;
        }
        if self.clones.len() >= CLONES || self.per_function[k] >= CLONES_PER_FUNCTION {
            return None;
        }
        let id = self.base + self.clones.len();
        // (Recursive calls of the same clone find it.)
        self.memo.insert((k, key.clone()), Some(id));
        self.clones.push(Func { name: String::new(), ..self.templates[k].clone()? });
        // (Its parameters: each kept one in place, the fields of the others.)
        let np = self.templates[k].as_ref()?.params.len();
        let mut params = Vec::new();
        for q in 0..np {
            match key.iter().find(|(p, _)| *p == q) {
                Some((_, tys)) => params.extend(std::iter::repeat_n(None, tys.len())),
                None => params.push(Some(q)),
            }
        }
        self.origins.push((k, key.clone(), params));
        self.per_function.push(0);
        self.per_function[k] += 1;
        match self.build(k, &key) {
            Some(f) => {
                self.clones[id - self.base] = f;
                Some(id)
            }
            None => {
                // (A placeholder that nothing calls.)
                self.memo.insert((k, key), None);
                let f = &mut self.clones[id - self.base];
                f.body = vec![Stmt::Throw(Expr::new(Ty::Any, ExprKind::Undefined))];
                None
            }
        }
    }

    fn build(&mut self, k: usize, objs: &[(usize, Vec<Ty>)]) -> Option<Func> {
        let t = self.templates[k].clone()?;
        let nl = t.locals.len();
        let np = t.params.len();
        // The new parameters: the fields of each object parameter in its
        // place (named `nl + j` while the body is rewritten).
        let mut params = Vec::new();
        let mut local_tys = Vec::new();
        let mut boxed = Vec::new();
        let mut names = Vec::new();
        let mut kept: Vec<Option<usize>> = vec![None; np];
        let mut fields: HashMap<(usize, Field), (LocalId, Ty)> = HashMap::new();
        let mut temp_index: Vec<usize> = Vec::new();
        for q in 0..np {
            match objs.iter().find(|(p, _)| *p == q) {
                Some((_, tys)) => {
                    let (fs, _) = self.uses[k][q].reads()?;
                    for (&fd, &ty) in fs.iter().zip(tys) {
                        fields.insert((q, fd), (nl + temp_index.len(), ty));
                        temp_index.push(params.len());
                        params.push(ty);
                        local_tys.push(ty);
                        boxed.push(false);
                        names.push(None);
                    }
                }
                None => {
                    kept[q] = Some(params.len());
                    params.push(t.params[q]);
                    local_tys.push(t.locals[q]);
                    boxed.push(t.boxed[q]);
                    names.push(t.local_names[q]);
                }
            }
        }
        let n_new = params.len();
        let spec = |l: LocalId| l < np && kept[l].is_none();
        let mut body = t.body.clone();
        let mut ok = true;
        let field_of = |o: &Expr, fd: Field| match o.kind {
            ExprKind::Local(q) if spec(q) => fields.get(&(q, fd)).copied(),
            _ => None,
        };
        visit_stmts_mut(&mut body, &mut |e| {
            if !ok {
                return;
            }
            if let Some((l, right)) = nullish_compare(e)
                && spec(l)
            {
                let _ = right;
                let ExprKind::AnyCompare(op, ..) = e.kind else { unreachable!() };
                // An object is neither `null` nor `undefined`.
                let ne = matches!(op, AnyCmpOp::Ne | AnyCmpOp::StrictNe);
                *e = Expr::new(Ty::Bool, ExprKind::Bool(ne));
                return;
            }
            let ty = e.ty;
            let new = match &mut e.kind {
                ExprKind::GetProp(o, a) | ExprKind::ShapeGet { obj: o, atom: a, .. } => {
                    let Some((l, t)) = field_of(o, Field::Prop(*a)) else { return };
                    value(l, t, ty)
                }
                ExprKind::StructGet(_, o, fi) => {
                    let Some((l, t)) = field_of(o, Field::Slot(*fi)) else { return };
                    value(l, t, ty)
                }
                ExprKind::Truthy(x) if matches!(x.kind, ExprKind::Local(q) if spec(q)) => {
                    Expr::new(Ty::Bool, ExprKind::Bool(true))
                }
                ExprKind::Call(k2, args)
                | ExprKind::CallEntry { func: k2, args, callee: None, .. }
                | ExprKind::CallDirectThis(k2, _, args) => {
                    if !args.iter().any(|a| matches!(a.kind, ExprKind::Local(q) if spec(q))) {
                        return;
                    }
                    let mut objs2 = Vec::new();
                    let mut new_args = Vec::new();
                    for (r, a) in std::mem::take(args).into_iter().enumerate() {
                        let ExprKind::Local(q) = a.kind else {
                            new_args.push(a);
                            continue;
                        };
                        if !spec(q) {
                            new_args.push(a);
                            continue;
                        }
                        let Some((fs, _)) = self.uses.get(*k2).and_then(|u| u.get(r)).and_then(Use::reads) else {
                            ok = false;
                            return;
                        };
                        let mut tys = Vec::new();
                        for &fd in fs {
                            let Some(&(l, t)) = fields.get(&(q, fd)) else {
                                ok = false;
                                return;
                            };
                            tys.push(t);
                            new_args.push(Expr::new(t, ExprKind::Local(l)));
                        }
                        objs2.push((r, tys));
                    }
                    *args = new_args;
                    let Some(c) = self.clone_of(*k2, &objs2) else {
                        ok = false;
                        return;
                    };
                    *k2 = c;
                    return;
                }
                _ => return,
            };
            *e = new;
        });
        if !ok {
            return None;
        }
        // The locals renumbered: the new parameters, then the template's.
        let map = |l: LocalId| -> LocalId {
            if l == THIS_LOCAL {
                l
            } else if l >= nl {
                temp_index[l - nl]
            } else if l < np {
                kept[l].unwrap_or(n_new + l)
            } else {
                n_new + l
            }
        };
        remap_all(&mut body, &map);
        local_tys.extend(t.locals.iter().copied());
        boxed.extend(t.boxed.iter().enumerate().map(|(l, &b)| b && !(l < np && kept[l].is_some())));
        names.extend(t.local_names.iter().copied());
        // (The template's parameters' own locals are unused.)
        for q in 0..np {
            boxed[n_new + q] = false;
        }
        Some(Func {
            name: t.name.clone(),
            js_length: params.len(),
            params,
            ret: t.ret,
            locals: local_tys,
            boxed,
            local_names: names,
            captures: Vec::new(),
            value_captures: Vec::new(),
            is_closure: false,
            has_this: t.has_this,
            class: t.class,
            is_async: t.is_async,
            frameless: t.frameless,
            source: None,
            rest: t.rest,
            arguments: false,
            arguments_count: false,
            class_ctor: false,
            is_generator: false,
            constructor: false,
            ret_slot: t.ret_slot.map(map),
            hints: t.hints.iter().filter(|(l, _)| !spec(**l)).map(|(&l, &h)| (map(l), h)).collect(),
            ret_hint: t.ret_hint,
            body,
        })
    }
}

/// A field passed as parameter `l` of type `t`, where an expression of type
/// `want` read it.
fn value(l: LocalId, t: Ty, want: Ty) -> Expr {
    let x = Expr::new(t, ExprKind::Local(l));
    if t == want || want != Ty::Any || !matches!(t, Ty::F64 | Ty::Bool | Ty::I32) {
        x
    } else {
        Expr::new(Ty::Any, ExprKind::Box(Box::new(x)))
    }
}

/// Renumber every local named in `stmts` (by statements and expressions,
/// also in the bodies of inlined calls) by `map`.
pub(crate) fn remap_all(stmts: &mut Vec<Stmt>, map: &impl Fn(LocalId) -> LocalId) {
    fn target(t: &mut AwaitTarget, map: &impl Fn(LocalId) -> LocalId) {
        if let AwaitTarget::Set(l) | AwaitTarget::Init(l) = t {
            *l = map(*l);
        }
    }
    lists_deep_mut(stmts, &mut |list| {
        for s in list.iter_mut() {
            match s {
                Stmt::Await { target: t, .. } => target(t, map),
                Stmt::Yield { kind, target: t, .. } => {
                    target(t, map);
                    if let YieldKind::Delegate { iter, next, .. } = kind {
                        *iter = map(*iter);
                        *next = map(*next);
                    }
                }
                Stmt::Loop { per_iteration, .. } => per_iteration.iter_mut().for_each(|l| *l = map(*l)),
                Stmt::Switch { disc, .. } => *disc = map(*disc),
                Stmt::NewBoxes(ls) => ls.iter_mut().for_each(|l| *l = map(*l)),
                Stmt::IterGuard { iter, next, .. } => {
                    *iter = map(*iter);
                    *next = map(*next);
                }
                Stmt::Try { catch, exc, .. } => {
                    if let Some((Some(p), _)) = catch {
                        *p = map(*p);
                    }
                    if let Some(x) = exc {
                        *x = map(*x);
                    }
                }
                _ => {}
            }
        }
    });
    visit_stmts_mut(stmts, &mut |e| remap_locals(e, map));
}

impl Spec {
    /// Per clone made: which of its parameters are fields (not one of its
    /// function's own).
    pub fn field_params(&self) -> Vec<Vec<bool>> {
        self.origins.iter().map(|(_, _, ps)| ps.iter().map(Option::is_none).collect()).collect()
    }
}

/// The parameters of clones that take fields (`fields`, per clone from
/// `base` on) and are given numbers by every call (`box(x)`, `x` a number)
/// become numbers: the calls pass them unboxed (but through a JS entry,
/// which unboxes them itself), and the clones' bodies read them boxed,
/// which `number_locals` then folds into their arithmetic.
pub fn number_params(m: &mut Module, base: usize, fields: &[Vec<bool>]) {
    let nfuncs = m.funcs.len();
    let boxed_number = |a: &Expr| matches!(&a.kind, ExprKind::Box(x) if x.ty.is_number());
    loop {
        // Per clone parameter: whether every call gives it a number.
        let mut num: Vec<Vec<Option<bool>>> = fields.iter().map(|ps| vec![None; ps.len()]).collect();
        for f in m.funcs.iter().chain(&m.closures) {
            visit_stmts(&f.body, &mut |e| {
                if let ExprKind::Call(k, args)
                | ExprKind::CallEntry { func: k, args, .. }
                | ExprKind::CallDirectThis(k, _, args)
                | ExprKind::CallChecked { func: k, args, .. } = &e.kind
                    && *k >= base
                    && let Some(ps) = num.get_mut(*k - base)
                {
                    for (j, a) in args.iter().enumerate() {
                        if let Some(p) = ps.get_mut(j) {
                            *p = Some(p.unwrap_or(true) && boxed_number(a));
                        }
                    }
                }
            });
        }
        let mut retyped: Vec<(usize, usize)> = Vec::new();
        for (c, ps) in num.iter().enumerate() {
            let f = &mut m.closures[base - nfuncs + c];
            for (j, &p) in ps.iter().enumerate() {
                if p == Some(true) && fields[c][j] && f.params[j] == Ty::Any && f.locals[j] == Ty::Any {
                    f.params[j] = Ty::F64;
                    f.locals[j] = Ty::F64;
                    retyped.push((base + c, j));
                }
            }
        }
        if retyped.is_empty() {
            return;
        }
        // The clones' reads of them, boxed.
        let mut changed: Vec<usize> = retyped.iter().map(|&(c, _)| c).collect();
        changed.dedup();
        for &c in &changed {
            let f = &mut m.closures[c - nfuncs];
            let params: Vec<usize> = retyped.iter().filter(|&&(k, _)| k == c).map(|&(_, j)| j).collect();
            visit_stmts_mut(&mut f.body, &mut |e| {
                if let ExprKind::Local(l) = e.kind
                    && e.ty == Ty::Any
                    && params.contains(&l)
                {
                    *e = Expr::new(Ty::Any, ExprKind::Box(Box::new(Expr::new(Ty::F64, ExprKind::Local(l)))));
                }
            });
        }
        // The calls, unboxed (not through a JS entry; a checked call's
        // function object takes its arguments of its own).
        for f in m.funcs.iter_mut().chain(m.closures.iter_mut()) {
            visit_stmts_mut(&mut f.body, &mut |e| {
                if let ExprKind::Call(k, args)
                | ExprKind::CallDirectThis(k, _, args)
                | ExprKind::CallChecked { func: k, args, .. } = &mut e.kind
                {
                    for (j, a) in args.iter_mut().enumerate() {
                        if retyped.contains(&(*k, j))
                            && let ExprKind::Box(x) = &mut a.kind
                        {
                            let x = std::mem::replace(&mut **x, Expr::num(0.0));
                            *a = x;
                        }
                    }
                }
            });
        }
        for &c in &changed {
            number_locals(&mut m.closures[c - nfuncs]);
        }
    }
}
