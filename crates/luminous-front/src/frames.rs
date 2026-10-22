//! Frame objects: objects that must exist as objects (written by the
//! functions they are passed to, given to `JSON.stringify` and the like)
//! but never outlive the call that makes them are made in a slot of that
//! call's frame rather than in the heap (`ExprKind::FrameObject`).
//!
//! A sync function's slots are in the runtime's frame region, pushed on
//! entry and popped on exit; an async function's objects that live across
//! awaits are in its frame's cell, which lives as long as the function
//! runs. The
//! collector traces the live slots' objects as roots (or as part of the
//! frame cell), and they are real objects in every other way: identity,
//! shapes, property order, `JSON.stringify`. See the engine's
//! `JS_GCFramePin`.
//!
//! An object goes in a slot when every value of its local is made by an
//! object literal (or a `new` of a class whose constructor is inlined:
//! `NewInstance`) at a statement of its own, the local is not captured,
//! and every other use of it is one that cannot keep it:
//!
//! - reading or writing one of its own properties (every literal of the
//!   local has the key: no prototype getter or setter runs);
//! - `typeof`, truthiness, `===` / `!==` and nullish comparisons;
//! - passing it to a function whose escape summary says it only reads or
//!   writes own properties of it, and does not hand it to an async call
//!   that may run later (`Use::Reads`, `Use::Writes`, not `outlives`); an
//!   async callee that may suspend only from an async function that
//!   awaits it at once (its frame lives until the callee is done);
//! - for an object in the frame region, builtins that read it without
//!   running code that could keep it: `JSON.stringify(o)`,
//!   `Object.keys/values/entries(o)`, `for (k in o)`, `{...o}`, rest
//!   destructuring. Where such a call may reach other code instead (the
//!   global was replaced, `toJSON`, a getter of `Object.prototype` read by
//!   `o[k]` in `for`-`in`), native code pins the frame region first: the
//!   objects made so far stay.
//!
//! In an async function, an object that does not live across an await is
//! in the frame region too, in slots pushed for each step (each run of the
//! step function until it suspends); one that does is in the frame's cell.
//!
//! A slot is reused when its literal runs again (a loop): by then the
//! local holds the previous object, and nothing else does.

use crate::escape::{Field, Summaries, THIS_PARAM, Use, awaited};
use crate::ir::*;
use std::collections::HashSet;

/// Most slots in a function's frame, and their bytes (a deep recursion
/// fills the frame region less: past it, objects go to the heap).
const SLOTS: usize = 32;
const FRAME_BYTES: usize = 4096;

/// A slot's bytes: its word, the cell header, the object, room for `n`
/// properties (as `JSValue`s, 8 bytes with compressed pointers).
pub fn slot_bytes(n: u32) -> usize {
    8 + 8 + 24 + 8 * n as usize
}

/// The properties an object made by defining `n` properties in turn on
/// an empty one has room for (the engine's `resize_properties`, from
/// `JS_PROP_INITIAL_SIZE`): its shape's `prop_size`.
pub fn room(n: usize) -> u32 {
    let mut s = 2usize;
    while s < n {
        s = (s + 1).max(s * 3 / 2);
    }
    s as u32
}

/// Places the objects of every function that can have them in its frame.
/// `table`: the module's escape summaries (`escape::summaries`), from
/// before its number fields were speculated on (`spec`: guarded loads and
/// stores of the same fields, which summaries do not follow).
pub fn frame_objects(m: &mut Module, table: &Summaries) {
    let nf = m.funcs.len();
    let suspends: Vec<bool> = m.funcs.iter().chain(&m.closures).map(|f| f.suspends()).collect();
    let atom = |name: &str| m.atoms.iter().position(|a| a == name).map(|i| i as AtomId);
    let builtins = Builtins {
        json: atom("JSON"),
        stringify: atom("stringify"),
        object: atom("Object"),
        reads: ["keys", "values", "entries"].iter().filter_map(|k| atom(k)).collect(),
    };
    let debug = std::env::var_os("LUMINOUS_DEBUG_FRAME").is_some();
    for g in 0..nf + m.closures.len() {
        let f = if g < nf { &mut m.funcs[g] } else { &mut m.closures[g - nf] };
        place(f, table, &suspends, &builtins, debug);
    }
}

/// The atoms of the builtins that frame objects may be given.
struct Builtins {
    json: Option<AtomId>,
    stringify: Option<AtomId>,
    object: Option<AtomId>,
    /// `keys`, `values`, `entries` (of `Object`).
    reads: Vec<AtomId>,
}

impl Builtins {
    /// `JSON.stringify(x)` or `Object.keys/values/entries(x)` (the globals,
    /// as read when called: native code checks them, see `frames`).
    fn is(&self, recv: &Expr, m: AtomId) -> bool {
        let ExprKind::Global(g) = recv.kind else { return false };
        (Some(g) == self.json && Some(m) == self.stringify) || (Some(g) == self.object && self.reads.contains(&m))
    }
}

/// Every statement list of `stmts`, also those in inlined, predicted and
/// callback bodies.
fn all_lists(stmts: &[Stmt], f: &mut impl FnMut(&[Stmt])) {
    stmt_lists(stmts, &mut |list| {
        f(list);
    });
    visit_stmts(stmts, &mut |e| {
        if let ExprKind::Inline { body, .. } | ExprKind::Predicted { body, .. } | ExprKind::ArrayLoop { body, .. } =
            &e.kind
        {
            // (visit_stmts goes into the body too: only its lists here.)
            stmt_lists(body, &mut |list| f(list));
        }
    });
}

/// The value of a sequence: its last expression's.
fn value_of(x: &Expr) -> &Expr {
    match &x.kind {
        ExprKind::Seq(xs) if !xs.is_empty() => value_of(&xs[xs.len() - 1]),
        _ => x,
    }
}

fn value_of_mut(x: &mut Expr) -> &mut Expr {
    if matches!(&x.kind, ExprKind::Seq(xs) if !xs.is_empty()) {
        let ExprKind::Seq(xs) = &mut x.kind else { unreachable!() };
        let last = xs.len() - 1;
        return value_of_mut(&mut xs[last]);
    }
    x
}

/// The keys of the literal a site makes (its value, after what a
/// sequence evaluates first).
fn literal_keys(x: &Expr) -> Option<Vec<AtomId>> {
    match &value_of(x).kind {
        ExprKind::Object(props) | ExprKind::NewInstance { fields: props, .. } => {
            Some(props.iter().map(|(a, _)| *a).collect())
        }
        _ => None,
    }
}

fn place(f: &mut Func, table: &Summaries, suspends: &[bool], builtins: &Builtins, debug: bool) {
    if f.is_generator {
        return;
    }
    let n = f.locals.len();
    let mut ok: Vec<bool> = (0..n)
        .map(|l| {
            l >= f.params.len() && l != THIS_LOCAL && !f.boxed[l] && f.locals[l] == Ty::Any && f.ret_slot != Some(l)
        })
        .collect();
    deep_stmt_locals(&f.body, &mut |l| ok[l] = false);
    let mut total = vec![0usize; n];
    each_name(&f.body, &mut |l| total[l] += 1);

    // The sites: `l = literal` (or its declaration) where its value is
    // not used (a statement, or before the end of a sequence), and the
    // keys every one of them has; `l = undefined` (or `null`) there too.
    let mut discarded: HashSet<*const Expr> = HashSet::new();
    all_lists(&f.body, &mut |list| {
        for s in list {
            match s {
                Stmt::Expr(e) => {
                    discarded.insert(e as *const Expr);
                }
                Stmt::Loop { update: Some(e), .. } => {
                    discarded.insert(e as *const Expr);
                }
                _ => {}
            }
        }
    });
    visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::Seq(xs) => {
            let all = discarded.contains(&(e as *const Expr));
            for (i, x) in xs.iter().enumerate() {
                if all || i + 1 < xs.len() {
                    discarded.insert(x as *const Expr);
                }
            }
        }
        ExprKind::Void(x) => {
            discarded.insert(&**x as *const Expr);
        }
        _ => {}
    });
    let mut sites = vec![0usize; n];
    let mut empty = vec![0usize; n];
    let mut keys: Vec<Option<Vec<AtomId>>> = vec![None; n];
    let mut site_exprs: HashSet<*const Expr> = HashSet::new();
    visit_stmts(&f.body, &mut |e| {
        if let ExprKind::InitLocal(l, x) | ExprKind::SetLocal(l, x) = &e.kind
            && discarded.contains(&(e as *const Expr))
        {
            if let Some(ks) = literal_keys(x) {
                sites[*l] += 1;
                site_exprs.insert(value_of(x) as *const Expr);
                keys[*l] = Some(match keys[*l].take() {
                    None => ks,
                    Some(prev) => prev.into_iter().filter(|a| ks.contains(a)).collect(),
                });
            } else if matches!(value_of(x).kind, ExprKind::Undefined | ExprKind::Null) {
                empty[*l] += 1;
            }
        }
    });
    if sites.iter().all(|&c| c == 0) {
        return;
    }

    // In an async function with a frame, an object that may live across an
    // await is in that frame's cell (no builtins: the frame region's
    // pinning does not cover them). One that does not is in the frame
    // region, in slots pushed for each step: the statements that name its
    // local, in one list, the first of them making it (so that a value of
    // an earlier step is never read), with no await among them.
    let mut in_cell = vec![false; n];
    if f.suspends() {
        let mut step = vec![false; n];
        all_lists(&f.body, &mut |list| {
            let cands: Vec<LocalId> = (0..n).filter(|&l| sites[l] > 0 && !step[l]).collect();
            if cands.is_empty() {
                return;
            }
            let mut names = vec![vec![0usize; n]; list.len()];
            let mut waits = vec![false; list.len()];
            for (i, st) in list.iter().enumerate() {
                each_name(std::slice::from_ref(st), &mut |l| names[i][l] += 1);
                stmt_lists(std::slice::from_ref(st), &mut |ss| {
                    waits[i] |= ss.iter().any(|x| matches!(x, Stmt::Await { .. } | Stmt::Yield { .. }));
                });
            }
            for l in cands {
                if names.iter().map(|c| c[l]).sum::<usize>() != total[l] {
                    continue;
                }
                let first = names.iter().position(|c| c[l] > 0).unwrap();
                let last = names.iter().rposition(|c| c[l] > 0).unwrap();
                if waits[first..=last].iter().any(|&w| w) {
                    continue;
                }
                if let Stmt::Expr(e) = &list[first]
                    && defines_first(e, l)
                {
                    step[l] = true;
                }
            }
        });
        for l in 0..n {
            in_cell[l] = !step[l];
        }
    }

    // `for`-`in` over a local copies it into a local of the loop's own
    // (named by `ForInIterator` and `ForInStep` only): the copy is a use.
    let mut alias: Vec<Option<LocalId>> = vec![None; n];
    {
        let mut copies: Vec<Option<LocalId>> = vec![None; n];
        let mut copy_count = vec![0usize; n];
        let mut loop_names = vec![0usize; n];
        visit_stmts(&f.body, &mut |e| match &e.kind {
            ExprKind::SetLocal(q, x) | ExprKind::InitLocal(q, x) => {
                copy_count[*q] += 1;
                if let ExprKind::Local(l) = x.kind {
                    copies[*q] = Some(l);
                }
            }
            ExprKind::ForInIterator(x, _) => {
                if let ExprKind::Local(q) = x.kind {
                    loop_names[q] += 1;
                }
            }
            ExprKind::ForInStep(_, _, q) => loop_names[*q] += 1,
            _ => {}
        });
        for q in 0..n {
            if let Some(l) = copies[q]
                && copy_count[q] == 1
                && loop_names[q] > 0
                && total[q] == 1 + loop_names[q]
                && !f.boxed[q]
            {
                alias[q] = Some(l);
            }
        }
    }

    // The uses that cannot keep the object, and the keys they need.
    let mut fine = vec![0usize; n];
    let mut need: Vec<Vec<AtomId>> = vec![Vec::new(); n];
    let mut bad = vec![false; n];
    let awaits = awaited(&f.body);
    let local = |x: &Expr| match value_of(x).kind {
        ExprKind::Local(l) if l < n && sites[l] > 0 => Some(l),
        _ => None,
    };
    visit_stmts(&f.body, &mut |e| {
        let mut use_of = |x: &Expr, u: Option<&Use>, fine: &mut Vec<usize>, need: &mut Vec<Vec<AtomId>>| {
            let Some(l) = local(x) else { return };
            if let Some(Use::Reads { fields, outlives: false } | Use::Writes { fields, outlives: false }) = u {
                for fd in fields {
                    match fd {
                        Field::Prop(a) => need[l].push(*a),
                        Field::Slot(_) => bad[l] = true,
                    }
                }
                fine[l] += 1;
            }
        };
        if let Some((l, _)) = nullish_compare(e) {
            if l < n && sites[l] > 0 {
                fine[l] += 1;
            }
            return;
        }
        match &e.kind {
            ExprKind::GetProp(o, a)
            | ExprKind::SetProp(o, a, _)
            | ExprKind::ShapeGet { obj: o, atom: a, .. }
            | ExprKind::ShapeSet { obj: o, atom: a, .. } => {
                if let Some(l) = local(o) {
                    need[l].push(*a);
                    fine[l] += 1;
                }
            }
            ExprKind::Truthy(x) | ExprKind::AnyUnary(AnyUnOp::Typeof, x) => {
                if let Some(l) = local(x) {
                    fine[l] += 1;
                }
            }
            ExprKind::AnyCompare(AnyCmpOp::StrictEq | AnyCmpOp::StrictNe, a, b) => {
                for x in [a, b] {
                    if let Some(l) = local(x) {
                        fine[l] += 1;
                    }
                }
            }
            ExprKind::Call(k, args)
            | ExprKind::CallDirectThis(k, _, args)
            | ExprKind::CallEntry { func: k, args, .. } => {
                let (declines, entry) = match &e.kind {
                    ExprKind::CallEntry { callee, .. } => (callee.is_some(), true),
                    _ => (false, false),
                };
                if declines {
                    return;
                }
                // A callee that may suspend reads its arguments later: only
                // while an async caller's frame waits for it.
                let later = suspends.get(*k).copied().unwrap_or(true);
                let waits = entry && awaits.contains(&(e as *const Expr));
                let fits = |x: &Expr| local(x).is_some_and(|l| !later || (in_cell[l] && waits));
                let np = table.params.get(*k).map_or(0, Vec::len);
                for (q, a) in args.iter().enumerate() {
                    if q < np && fits(a) {
                        use_of(a, table.of(*k, q), &mut fine, &mut need);
                    }
                }
                if let ExprKind::CallDirectThis(k, this, _) = &e.kind
                    && fits(this)
                {
                    use_of(this, table.of(*k, THIS_PARAM), &mut fine, &mut need);
                }
            }
            // Builtins (sync functions: see `frames`).
            ExprKind::CallMethod(recv, Key::Atom(m), args) if args.len() == 1 => {
                if builtins.is(recv, *m)
                    && let Some(l) = local(&args[0])
                    && !in_cell[l]
                {
                    fine[l] += 1;
                }
            }
            ExprKind::KeysLength(recv, _, x) => {
                if matches!(recv.kind, ExprKind::Global(g) if Some(g) == builtins.object)
                    && let Some(l) = local(x)
                    && !in_cell[l]
                {
                    fine[l] += 1;
                }
            }
            ExprKind::ForInGet(_, _, o, _) => {
                if let Some(l) = local(o)
                    && !in_cell[l]
                {
                    fine[l] += 1;
                }
            }
            ExprKind::ObjectParts(parts) => {
                for p in parts {
                    if let ObjPart::Spread(x) = p
                        && let Some(l) = local(x)
                    {
                        fine[l] += 1;
                    }
                }
            }
            ExprKind::ObjectRest(x, _) => {
                if let Some(l) = local(x) {
                    fine[l] += 1;
                }
            }
            ExprKind::SetLocal(q, x) | ExprKind::InitLocal(q, x) if alias[*q].is_some() => {
                if let Some(l) = local(x)
                    && !in_cell[l]
                {
                    fine[l] += 1;
                }
            }
            _ => {}
        }
    });

    // The locals whose every naming is a site or such a use.
    let mut chosen = Vec::new();
    for l in 0..n {
        if sites[l] == 0 || !ok[l] || bad[l] || total[l] != sites[l] + empty[l] + fine[l] {
            continue;
        }
        let ks = keys[l].as_deref().unwrap_or(&[]);
        if !need[l].iter().all(|a| ks.contains(a)) {
            continue;
        }
        chosen.push(l);
    }
    if debug {
        for l in 0..n {
            if sites[l] > 0 {
                eprintln!(
                    "frame {} local {l}: {}{} (sites {}, uses {}/{})",
                    f.name,
                    if chosen.contains(&l) { "yes" } else { "no" },
                    if in_cell[l] { " (in the frame's cell)" } else { "" },
                    sites[l],
                    fine[l],
                    total[l] - sites[l] - empty[l]
                );
            }
        }
    }
    if chosen.is_empty() {
        return;
    }

    // Each site of a chosen local gets a slot (while the frame has room).
    let mut frame: Vec<(u32, bool)> = Vec::new();
    let mut bytes = 0usize;
    let mut placed: HashSet<LocalId> = HashSet::new();
    visit_stmts_mut(&mut f.body, &mut |e| {
        if let ExprKind::InitLocal(l, x) | ExprKind::SetLocal(l, x) = &mut e.kind
            && chosen.contains(l)
            && site_exprs.contains(&(value_of(x) as *const Expr))
            && let Some(ks) = literal_keys(x)
        {
            let r = room(ks.len());
            if frame.len() >= SLOTS || bytes + slot_bytes(r) > FRAME_BYTES {
                return;
            }
            bytes += slot_bytes(r);
            let slot = frame.len() as u32;
            frame.push((r, in_cell[*l]));
            placed.insert(*l);
            let lit = value_of_mut(x);
            let obj = std::mem::replace(lit, Expr::new(Ty::Any, ExprKind::Undefined));
            *lit = Expr::new(Ty::Any, ExprKind::FrameObject { slot, obj: Box::new(obj) });
        }
    });
    f.frame = frame;
    let mut locals: Vec<LocalId> = placed.into_iter().collect();
    locals.sort_unstable();
    f.frame_locals = locals;
}

/// Whether `e` (a statement's) makes local `l` (as a site, or `undefined`)
/// before anything else in it names `l`.
fn defines_first(e: &Expr, l: LocalId) -> bool {
    let names = |x: &Expr| {
        let mut c = 0;
        visit(x, &mut |y| expr_locals(y, &mut |m| c += (m == l) as usize));
        c
    };
    match &e.kind {
        ExprKind::InitLocal(m, x) | ExprKind::SetLocal(m, x) if *m == l => names(x) == 0,
        ExprKind::Seq(xs) => {
            for x in xs {
                if defines_first(x, l) {
                    return true;
                }
                if names(x) > 0 {
                    return false;
                }
            }
            false
        }
        _ => false,
    }
}
