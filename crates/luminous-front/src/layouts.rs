//! Stable layouts (reported only; nothing is compiled differently yet):
//! which object literals make objects whose own properties stay the ones
//! the literal gives them, in its order, for as long as they live.
//!
//! The whole program (native and interpreted code alike, from its source)
//! is analyzed at once, flow-insensitively: an Andersen-style points-to
//! analysis with an abstract object per literal (and per array, `Map`,
//! promise... made at a site), per function, class, its prototype and its
//! instances, whose fields are told apart by key. Values flow through
//! bindings, fields, arguments, returns, `this`, iteration, promises and
//! exceptions, and through models of the engine's builtins. A literal's
//! objects lose their layout where a write adds a key, a computed key is
//! written, a key is deleted, a property defined, the object frozen,
//! sealed or given another prototype, or where they reach code the program
//! does not contain (the engine's unmodeled builtins, native modules,
//! globals, exports, `eval`): that code could do any of these. What such
//! code makes (`Top`) holds nothing the program made unless it escaped.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use oxc::allocator::Allocator;
use oxc::ast::ast::*;
use oxc::ast_visit::{Visit, walk};
use oxc::parser::Parser;
use oxc::semantic::{ScopeId, Scoping, SemanticBuilder, SymbolId};
use oxc::span::{GetSpan, SourceType, Span};

/// An object literal and whether its objects keep their layout.
#[derive(Debug, Clone)]
pub struct Site {
    pub span: (u32, u32),
    /// Its keys, in order (`@@iterator` for `[Symbol.iterator]`).
    pub keys: Vec<String>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Stable,
    /// It makes objects of an `aot type`, whose layout is fixed anyway.
    Struct,
    /// Why not (and where: a source offset).
    Unstable { why: String, at: u32, category: &'static str },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Reads {
    /// Reads `o.k` (static keys) where `o` may be an object of a literal.
    pub literal: usize,
    /// Of those, the reads where `o` is always an object of literals with
    /// stable layouts, which all have `k` at the same index.
    pub fixed: usize,
    /// The same, for reads in natively compiled functions.
    pub native_literal: usize,
    pub native_fixed: usize,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub sites: Vec<Site>,
    pub reads: Reads,
}

/// Analyze a program (`source`, as `Analysis::source` has it). `structs`:
/// where the literals that make `aot type` objects start; `native`: the
/// spans of natively compiled functions; `place` formats a source offset
/// for the reasons given.
pub fn analyze(
    source: &str,
    structs: &HashSet<u32>,
    native: &[(u32, u32)],
    place: &dyn Fn(u32) -> String,
) -> Result<Report, String> {
    let debug = std::env::var_os("LUMINOUS_DEBUG_LAYOUTS").is_some();
    let t0 = std::time::Instant::now();
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::ts().with_module(true)).parse();
    if !parsed.diagnostics.is_empty() {
        return Err("the program does not parse".into());
    }
    let program = allocator.alloc(parsed.program);
    let sem = SemanticBuilder::new().build(program);
    let scoping = sem.semantic.into_scoping();

    let mut s = Solver::new();
    let mut pre = Prepass { s: &mut s, consts: HashMap::new(), loaded: HashSet::new() };
    pre.visit_program(program);
    let (consts, loaded) = (pre.consts, pre.loaded);
    s.undeclared = loaded.into_iter().filter(|k| !s.declared.contains(k)).collect();
    s.undeclared.sort();
    let mut g = Gen {
        s: &mut s,
        scoping: &scoping,
        source,
        structs,
        syms: HashMap::new(),
        consts,
        ctx: Vec::new(),
        reads: Vec::new(),
        globals: HashMap::new(),
        name_hint: None,
        links: Vec::new(),
    };
    if debug {
        eprintln!("layouts: parsed, prepass done in {:?}", t0.elapsed());
    }
    g.program(program);
    if debug {
        eprintln!(
            "layouts: generated in {:?}: {} nodes, {} constraints, {} work",
            t0.elapsed(),
            g.s.pts.len(),
            g.s.constraints.len(),
            g.s.work.len()
        );
    }
    for (n, class) in std::mem::take(&mut g.links) {
        g.s.constrain(n, Cons::Link { class });
    }
    let reads = std::mem::take(&mut g.reads);
    s.solve();
    if debug {
        eprintln!("layouts: solved in {:?}: {} nodes, {} constraints", t0.elapsed(), s.pts.len(), s.constraints.len());
    }
    Ok(s.report(&reads, native, place))
}

type Node = u32;
type Abs = u32;
type Key = u32;

/// Holds nothing: `null`, `undefined`, values never used as objects.
const EMPTY: Node = 0;
/// Primitives: strings, numbers... (their methods make arrays).
const PRIMS: Node = 1;
/// What code the program does not contain makes.
const TOPS: Node = 2;
/// What `throw` throws, and promises reject with.
const THROWN: Node = 3;
/// What `catch` catches: what is thrown, and the engine's errors.
const CAUGHT: Node = 4;

const TOP: Abs = 0;
const PRIM: Abs = 1;
/// The `reject` of `new Promise`.
const REJECT: Abs = 2;
/// Regular expressions.
const RE: Abs = 3;

/// The field of a computed key that is not a constant.
const ANY: Key = 0;
/// A container's elements (an array's, a set's, a promise's value...).
const ELEM: Key = 1;
/// A map's keys and values.
const MKEY: Key = 2;
const MVAL: Key = 3;

#[derive(Clone, Debug)]
enum Kind {
    Top,
    Prim,
    Reject,
    Re,
    /// An object a literal makes (the site), or another the program's code
    /// makes (an object rest, `Object.create`, an error...).
    Obj(Option<u32>),
    Arr,
    Map,
    Set,
    Promise,
    Iter,
    /// A `WeakRef` or `FinalizationRegistry`: what it holds.
    Box,
    /// The generator object of a call of generator function `f`.
    Gen(u32),
    Fn(u32),
    Bound { target: Node, this: Node, args: Vec<Node> },
    /// A promise's `resolve`: resolves the value held at the node.
    Resolve(Node),
    Class(u32),
    Proto(u32),
    Inst(u32),
    /// An engine builtin, by name (`Object.keys`, `Array.prototype.push`).
    Builtin(Rc<str>),
}

/// Why a literal's objects lose their layout.
#[derive(Clone, Debug)]
enum Why {
    Adds(Key),
    Deletes(Key),
    WritesComputed,
    Defines(&'static str),
    Accessor(Key),
    Spread,
    ComputedKey,
    SetsProto,
    /// Reaches code the program does not contain (its text).
    PassedTo(Rc<str>),
    Global(Rc<str>),
    StoredIn(Rc<str>),
    Exported,
    Returned(u32),
    HeldBy(Abs),
    Eval,
}

#[derive(Clone, Debug)]
enum Cons {
    /// Copy field `key` of what arrives into `dst` (`ANY`: every field).
    Load { dst: Node, key: Key },
    /// Write `src` into field `key` (`check`: a write the program makes,
    /// which a literal's layout must have the key for).
    Store { key: Key, src: Node, at: u32, check: bool, what: Rc<str> },
    Delete { key: Key, at: u32 },
    Break { why: Why, at: u32 },
    Call(u32),
    /// A builtin method (`class.prototype.key`) called on what arrives.
    Method { call: u32, class: Rc<str>, key: Rc<str> },
    Escape { why: Why, at: u32 },
    Await { dst: Node },
    /// What arrives is `then` of `this`, awaited into `dst`.
    Then { this: Abs, dst: Node },
    Iterate { dst: Node },
    /// What arrives inherits the fields of `dst`... no: `dst` inherits the
    /// fields of what arrives (its prototype).
    Mirror { dst: Abs },
    /// What arrives gets the prototypes held at `proto`.
    SetProto { proto: Node, at: u32 },
    GetProto { dst: Node },
    /// The class `class` extends what arrives.
    Link { class: u32 },
    /// What arrives is stored as the `prototype` of function class `class`.
    ProtoOf { class: u32 },
}

#[derive(Clone, Copy, Debug)]
enum Reader {
    Copy(Node),
    Escape,
}

#[derive(Clone, Debug)]
struct CallInfo {
    this: Node,
    args: Vec<Node>,
    /// The index of the first spread argument: arguments from there on can
    /// be any parameter's (`tail` holds them all).
    spread: Option<usize>,
    tail: Node,
    result: Node,
    new: bool,
    /// `super(...)`: the instance is `this` already.
    sup: bool,
    at: u32,
    text: Rc<str>,
    /// Arguments that are `null`, `undefined` or array literals.
    plain: u64,
    /// How many calls made it (see `derive`, `forward`).
    depth: u8,
    /// The call whose model made it (see `canon`).
    parent: Option<u32>,
    /// Its callee's node (forwarding calls': see `forward`).
    callee: Node,
}

const FWD_BOUND: u8 = 1;
const FWD_BOUND_ARGS: u8 = 2;
const FWD_SUPER: u8 = 3;

struct Func {
    name: Rc<str>,
    at: u32,
    /// The function itself.
    abs: Abs,
    params: Vec<Node>,
    rest: Option<Abs>,
    this: Node,
    ret: Node,
    /// `arguments`.
    args: Abs,
    /// What its calls return (its promise, its generator...).
    result: Node,
    /// What `yield` gives back (`next(v)`).
    yieldin: Node,
    /// What a generator yields.
    yields: Node,
    arrow: bool,
    class: Option<u32>,
}

struct ClassInfo {
    name: Rc<str>,
    /// The class (or the function, for a function's instances).
    value: Abs,
    proto: Abs,
    inst: Abs,
    ctor: Option<u32>,
    parents: Vec<u32>,
    children: Vec<u32>,
    /// The `this` of its instance methods, constructor, field initializers.
    this_inst: Vec<Node>,
    this_static: Vec<Node>,
    setters: Vec<(Key, Node)>,
    static_setters: Vec<(Key, Node)>,
    /// What `super(...)` calls, `super.x` reads.
    super_ctor: Node,
    super_proto: Node,
    super_static: Node,
    /// Builtins it extends (`?`: one not modeled).
    bases: Vec<Rc<str>>,
}

/// Where a builtin method call reads its receivers' parts (see `method`).
#[derive(Clone, Copy)]
struct MethodState {
    /// The call whose arguments and result the reads use.
    call: u32,
    elems: Node,
    keys: Node,
    vals: Node,
    recv: Node,
    /// A generator's returns.
    rets: Node,
}

struct SiteInfo {
    span: (u32, u32),
    keys: Vec<Key>,
    is_struct: bool,
    why: Option<(Why, u32)>,
}

struct Solver {
    pts: Vec<Vec<Abs>>,
    has: Vec<HashSet<Abs>>,
    succ: Vec<Vec<Node>>,
    edges: HashSet<(Node, Node)>,
    cons: Vec<Vec<u32>>,
    constraints: Vec<Cons>,
    work: Vec<(Node, Abs)>,
    kinds: Vec<Kind>,
    ats: Vec<u32>,
    fields: HashMap<(Abs, Key), Node>,
    fields_of: Vec<Vec<(Key, Node)>>,
    readers: Vec<Vec<Reader>>,
    mirrors: Vec<Vec<Abs>>,
    mirrored: HashSet<(Abs, Abs)>,
    protos: Vec<Vec<Abs>>,
    proto_readers: Vec<Vec<Node>>,
    escaped: Vec<bool>,
    keys: Vec<String>,
    key_ids: HashMap<String, Key>,
    index_key: Vec<bool>,
    funcs: Vec<Func>,
    classes: Vec<ClassInfo>,
    inherited: HashSet<(u32, u32)>,
    calls: Vec<CallInfo>,
    sites: Vec<SiteInfo>,
    made: HashMap<(u32, u8), Abs>,
    builtins: HashMap<Rc<str>, Abs>,
    resolves: HashMap<Node, Abs>,
    get_protos: HashMap<Node, u32>,
    derived: HashMap<(u32, Abs), u32>,
    forwards: HashMap<(u32, u8), u32>,
    builtin_calls: HashMap<(u32, Abs), u32>,
    wired: HashSet<(u32, u32)>,
    /// The call whose model runs (what it invokes descends from it).
    cur: Option<u32>,
    /// Per thenable, what its `then` resolves with.
    thens: HashMap<Abs, Node>,
    method_states: HashMap<(u32, String), MethodState>,
    /// Keys the program defines anywhere (class members, stores, literals).
    declared: HashSet<Key>,
    /// Keys the program reads but defines nowhere: those an instance of a
    /// class extending an unmodeled builtin gets from it.
    undeclared: Vec<Key>,
}

impl Solver {
    fn new() -> Solver {
        let mut s = Solver {
            pts: Vec::new(),
            has: Vec::new(),
            succ: Vec::new(),
            edges: HashSet::new(),
            cons: Vec::new(),
            constraints: Vec::new(),
            work: Vec::new(),
            kinds: Vec::new(),
            ats: Vec::new(),
            fields: HashMap::new(),
            fields_of: Vec::new(),
            readers: Vec::new(),
            mirrors: Vec::new(),
            mirrored: HashSet::new(),
            protos: Vec::new(),
            proto_readers: Vec::new(),
            escaped: Vec::new(),
            keys: Vec::new(),
            key_ids: HashMap::new(),
            index_key: Vec::new(),
            funcs: Vec::new(),
            classes: Vec::new(),
            inherited: HashSet::new(),
            calls: Vec::new(),
            sites: Vec::new(),
            made: HashMap::new(),
            builtins: HashMap::new(),
            resolves: HashMap::new(),
            get_protos: HashMap::new(),
            derived: HashMap::new(),
            forwards: HashMap::new(),
            builtin_calls: HashMap::new(),
            wired: HashSet::new(),
            cur: None,
            thens: HashMap::new(),
            method_states: HashMap::new(),
            declared: HashSet::new(),
            undeclared: Vec::new(),
        };
        for _ in 0..5 {
            s.node();
        }
        for (k, at) in [(Kind::Top, 0), (Kind::Prim, 0), (Kind::Reject, 0), (Kind::Re, 0)] {
            s.abs(k, at);
        }
        for k in ["*", "[]", "[k]", "[v]"] {
            s.key(k);
        }
        // (The sources: added to directly, as `add` adds to no source.)
        for (n, a) in [(PRIMS, PRIM), (TOPS, TOP), (CAUGHT, TOP)] {
            s.has[n as usize].insert(a);
            s.pts[n as usize].push(a);
            s.work.push((n, a));
        }
        s.edge(THROWN, CAUGHT);
        s
    }

    fn node(&mut self) -> Node {
        self.pts.push(Vec::new());
        self.has.push(HashSet::new());
        self.succ.push(Vec::new());
        self.cons.push(Vec::new());
        (self.pts.len() - 1) as Node
    }

    fn abs(&mut self, kind: Kind, at: u32) -> Abs {
        self.kinds.push(kind);
        self.ats.push(at);
        self.fields_of.push(Vec::new());
        self.readers.push(Vec::new());
        self.mirrors.push(Vec::new());
        self.protos.push(Vec::new());
        self.proto_readers.push(Vec::new());
        self.escaped.push(false);
        (self.kinds.len() - 1) as Abs
    }

    /// A node holding `a`.
    fn holding(&mut self, a: Abs) -> Node {
        let n = self.node();
        self.add(n, a);
        n
    }

    fn key(&mut self, s: &str) -> Key {
        if let Some(&k) = self.key_ids.get(s) {
            return k;
        }
        let k = self.keys.len() as Key;
        self.keys.push(s.to_string());
        self.key_ids.insert(s.to_string(), k);
        let index = s == "0" || (!s.starts_with('0') && s.len() < 10 && s.bytes().all(|b| b.is_ascii_digit()));
        self.index_key.push(index);
        k
    }

    fn builtin(&mut self, name: &str) -> Abs {
        if let Some(&a) = self.builtins.get(name) {
            return a;
        }
        let name: Rc<str> = name.into();
        let a = self.abs(Kind::Builtin(name.clone()), 0);
        self.builtins.insert(name, a);
        a
    }

    /// An abstract object made by `owner` (a call, or an abstract object:
    /// `tag` tells apart several, and the two), at source offset `at`.
    fn made(&mut self, owner: u32, tag: u8, kind: Kind, at: u32) -> Abs {
        if let Some(&a) = self.made.get(&(owner, tag)) {
            return a;
        }
        let a = self.abs(kind, at);
        self.made.insert((owner, tag), a);
        a
    }

    fn add(&mut self, n: Node, a: Abs) {
        if n <= TOPS {
            return;
        }
        if self.has[n as usize].insert(a) {
            self.pts[n as usize].push(a);
            self.work.push((n, a));
        }
    }

    fn edge(&mut self, from: Node, to: Node) {
        if from == to || to <= TOPS || from == EMPTY || !self.edges.insert((from, to)) {
            return;
        }
        self.succ[from as usize].push(to);
        let mut i = 0;
        while i < self.pts[from as usize].len() {
            let a = self.pts[from as usize][i];
            self.add(to, a);
            i += 1;
        }
    }

    fn constrain(&mut self, n: Node, c: Cons) {
        if n == EMPTY {
            return;
        }
        let id = self.constraints.len() as u32;
        self.constraints.push(c);
        self.cons[n as usize].push(id);
        let mut i = 0;
        while i < self.pts[n as usize].len() {
            let a = self.pts[n as usize][i];
            self.apply(id, a);
            i += 1;
        }
    }

    fn field(&mut self, a: Abs, k: Key) -> Node {
        if let Some(&n) = self.fields.get(&(a, k)) {
            return n;
        }
        let n = self.node();
        self.fields.insert((a, k), n);
        self.fields_of[a as usize].push((k, n));
        let mut i = 0;
        while i < self.readers[a as usize].len() {
            let r = self.readers[a as usize][i];
            self.read(a, n, r);
            i += 1;
        }
        let mut i = 0;
        while i < self.mirrors[a as usize].len() {
            let t = self.mirrors[a as usize][i];
            let m = self.field(t, k);
            self.edge(n, m);
            i += 1;
        }
        n
    }

    fn read(&mut self, a: Abs, field: Node, r: Reader) {
        match r {
            Reader::Copy(dst) => self.edge(field, dst),
            Reader::Escape => self.constrain(field, Cons::Escape { why: Why::HeldBy(a), at: self.ats[a as usize] }),
        }
    }

    /// Every field of `a`, now and later, to `r`.
    fn read_all(&mut self, a: Abs, r: Reader) {
        self.readers[a as usize].push(r);
        let mut i = 0;
        while i < self.fields_of[a as usize].len() {
            let n = self.fields_of[a as usize][i].1;
            self.read(a, n, r);
            i += 1;
        }
    }

    /// `to` inherits the fields of `from` (its prototype).
    fn mirror(&mut self, from: Abs, to: Abs) {
        if from == to || !self.mirrored.insert((from, to)) {
            return;
        }
        self.mirrors[from as usize].push(to);
        self.protos[to as usize].push(from);
        for i in 0..self.proto_readers[to as usize].len() {
            let n = self.proto_readers[to as usize][i];
            self.add(n, from);
        }
        let mut i = 0;
        while i < self.fields_of[from as usize].len() {
            let (k, n) = self.fields_of[from as usize][i];
            let m = self.field(to, k);
            self.edge(n, m);
            i += 1;
        }
    }

    fn solve(&mut self) {
        let debug = std::env::var_os("LUMINOUS_DEBUG_LAYOUTS").is_some();
        let mut steps = 0u64;
        while let Some((n, a)) = self.work.pop() {
            steps += 1;
            if debug && steps % 1_000_000 == 0 {
                eprintln!(
                    "layouts: {steps} steps, {} nodes, {} abstract objects, {} constraints, {} calls, {} edges",
                    self.pts.len(),
                    self.kinds.len(),
                    self.constraints.len(),
                    self.calls.len(),
                    self.edges.len()
                );
                let mut by: HashMap<&str, usize> = HashMap::new();
                for c in &self.calls {
                    *by.entry(&c.text).or_default() += 1;
                }
                let mut by: Vec<_> = by.into_iter().collect();
                by.sort_by(|a, b| b.1.cmp(&a.1));
                eprintln!("  calls by text: {:?}", &by[..by.len().min(8)]);
                let mut depth = [0usize; 8];
                for c in &self.calls {
                    depth[(c.depth as usize).min(7)] += 1;
                }
                eprintln!("  by depth: {depth:?}, method states {}, derived {}", self.method_states.len(), self.derived.len());
            }
            let mut i = 0;
            while i < self.succ[n as usize].len() {
                let to = self.succ[n as usize][i];
                self.add(to, a);
                i += 1;
            }
            let mut i = 0;
            while i < self.cons[n as usize].len() {
                let c = self.cons[n as usize][i];
                self.apply(c, a);
                i += 1;
            }
        }
    }

    fn kind(&self, a: Abs) -> &Kind {
        &self.kinds[a as usize]
    }

    fn site_of(&self, a: Abs) -> Option<u32> {
        match self.kinds[a as usize] {
            Kind::Obj(s) => s,
            _ => None,
        }
    }

    /// `site`'s objects lose their layout (the best reason is kept: one
    /// that is not `HeldBy`, then the first in the source).
    fn unstable(&mut self, site: u32, why: Why, at: u32) {
        let s = &mut self.sites[site as usize];
        let better = match &s.why {
            None => true,
            Some((old, old_at)) => {
                let (held, old_held) = (matches!(why, Why::HeldBy(_)), matches!(old, Why::HeldBy(_)));
                (old_held && !held) || (held == old_held && at < *old_at)
            }
        };
        if better {
            s.why = Some((why, at));
        }
    }

    fn escape_node(&mut self, n: Node, why: Why, at: u32) {
        if n > TOPS {
            self.constrain(n, Cons::Escape { why, at });
        }
    }

    /// `a` reaches code the program does not contain, which can read and
    /// write all it holds, call it, construct it.
    fn escape(&mut self, a: Abs, why: Why, at: u32) {
        if let Some(site) = self.site_of(a) {
            self.unstable(site, why.clone(), at);
        }
        if self.escaped[a as usize] {
            return;
        }
        match self.kinds[a as usize].clone() {
            Kind::Top | Kind::Prim | Kind::Reject | Kind::Re | Kind::Builtin(_) => return,
            Kind::Fn(f) => {
                let (params, this, args, rest, yieldin, result, class) = {
                    let f = &self.funcs[f as usize];
                    (f.params.clone(), f.this, f.args, f.rest, f.yieldin, f.result, f.class)
                };
                for p in params.into_iter().chain([this, yieldin]) {
                    self.add(p, TOP);
                }
                for x in [Some(args), rest].into_iter().flatten() {
                    let e = self.field(x, ELEM);
                    self.add(e, TOP);
                }
                self.escaped[a as usize] = true;
                self.escape_node(result, Why::Returned(f), self.funcs[f as usize].at);
                if let Some(c) = class {
                    let inst = self.classes[c as usize].inst;
                    self.escape(inst, Why::HeldBy(a), at);
                }
            }
            Kind::Class(c) => {
                self.escaped[a as usize] = true;
                let (inst, ctor, sup) = {
                    let c = &self.classes[c as usize];
                    (c.inst, c.ctor, c.super_ctor)
                };
                self.escape(inst, Why::HeldBy(a), at);
                match ctor {
                    Some(f) => {
                        for i in 0..self.funcs[f as usize].params.len() {
                            let p = self.funcs[f as usize].params[i];
                            self.add(p, TOP);
                        }
                    }
                    None => {
                        let c = self.call_info(CallInfo { args: vec![TOPS], sup: true, ..CallInfo::new(EMPTY, at) });
                        self.constrain(sup, Cons::Call(c));
                    }
                }
            }
            Kind::Gen(f) => {
                let y = self.funcs[f as usize].yieldin;
                self.add(y, TOP);
            }
            Kind::Bound { target, this, args } => {
                self.escaped[a as usize] = true;
                for n in [target, this].into_iter().chain(args) {
                    self.escape_node(n, Why::HeldBy(a), at);
                }
            }
            Kind::Resolve(n) => self.add(n, TOP),
            _ => {}
        }
        self.escaped[a as usize] = true;
        let any = self.field(a, ANY);
        self.add(any, TOP);
        self.read_all(a, Reader::Escape);
        let mut i = 0;
        while i < self.protos[a as usize].len() {
            let p = self.protos[a as usize][i];
            self.escape(p, Why::HeldBy(a), self.ats[a as usize]);
            i += 1;
        }
    }

    fn call_info(&mut self, c: CallInfo) -> u32 {
        self.calls.push(c);
        (self.calls.len() - 1) as u32
    }

    /// Call what `callee` holds; returns the node of its results.
    fn invoke(&mut self, callee: Node, this: Node, args: Vec<Node>, at: u32, text: &Rc<str>) -> Node {
        let result = self.node();
        let (parent, depth) = match self.cur {
            Some(p) => (Some(p), self.calls[p as usize].depth.saturating_add(1)),
            None => (None, 0),
        };
        let c = self.call_info(CallInfo {
            this,
            args,
            result,
            text: text.clone(),
            parent,
            depth,
            ..CallInfo::new(EMPTY, at)
        });
        self.constrain(callee, Cons::Call(c));
        result
    }

    /// The call that stands for call `c` in what the models make per call
    /// (abstract objects, a method's reads, forwarding calls): itself, or
    /// past depth 5 (calls the calls of models make, and so on: these end),
    /// its ancestor at depth 5.
    fn canon(&self, mut c: u32) -> u32 {
        while self.calls[c as usize].depth > 5
            && let Some(p) = self.calls[c as usize].parent
        {
            c = p;
        }
        c
    }

    /// Call `c` does what call `first` does, which stands for it: its
    /// arguments and `this` go to `first`'s, `first`'s results to it.
    fn wire(&mut self, c: u32, first: u32) {
        if c == first || !self.wired.insert((c, first)) {
            return;
        }
        let (ci, fi) = (self.calls[c as usize].clone(), self.calls[first as usize].clone());
        for (i, &n) in ci.args.iter().enumerate() {
            match self.arg(&fi, i) {
                EMPTY => self.escape_node(n, Why::PassedTo(ci.text.clone()), ci.at),
                m => self.edge(n, m),
            }
        }
        if fi.tail != EMPTY {
            self.edge(ci.tail, fi.tail);
        } else {
            self.escape_node(ci.tail, Why::PassedTo(ci.text.clone()), ci.at);
        }
        self.edge(ci.this, fi.this);
        self.edge(fi.result, ci.result);
    }

    fn load(&mut self, base: Node, key: Key) -> Node {
        let dst = self.node();
        self.constrain(base, Cons::Load { dst, key });
        dst
    }

    fn await_(&mut self, src: Node) -> Node {
        let dst = self.node();
        self.constrain(src, Cons::Await { dst });
        dst
    }

    fn await_into(&mut self, src: Node, dst: Node) {
        self.constrain(src, Cons::Await { dst });
    }

    /// The values iterating what `src` holds gives (`for of`, spread,
    /// destructuring): containers' elements, and the iteration protocol's
    /// for the program's iterables.
    fn iterate(&mut self, src: Node, is_async: bool, at: u32) -> Node {
        let it = self.node();
        self.edge(src, it);
        let dst = self.node();
        self.constrain(it, Cons::Iterate { dst });
        let text: Rc<str> = "an iterator".into();
        let k = self.key("@@iterator");
        let f = self.load(it, k);
        let r = self.invoke(f, it, vec![], at, &text);
        self.edge(r, it);
        if is_async {
            let k = self.key("@@asyncIterator");
            let f = self.load(it, k);
            let r = self.invoke(f, it, vec![], at, &text);
            self.edge(r, it);
        }
        let k = self.key("next");
        let f = self.load(it, k);
        let mut r = self.invoke(f, it, vec![], at, &text);
        if is_async {
            r = self.await_(r);
        }
        let k = self.key("value");
        let v = self.load(r, k);
        self.edge(v, dst);
        if is_async { self.await_(dst) } else { dst }
    }

    /// The class of function `f`'s instances (made by `new f`).
    fn class_of(&mut self, f: u32) -> u32 {
        if let Some(c) = self.funcs[f as usize].class {
            return c;
        }
        let (name, at, this) = {
            let f = &self.funcs[f as usize];
            (f.name.clone(), f.at, f.this)
        };
        let value = self.funcs[f as usize].abs;
        let c = self.new_class(name, at, Some(value));
        self.funcs[f as usize].class = Some(c);
        self.classes[c as usize].ctor = Some(f);
        self.classes[c as usize].this_inst.push(this);
        self.inherit(c, c);
        let p = self.key("prototype");
        let pf = self.field(value, p);
        self.constrain(pf, Cons::ProtoOf { class: c });
        if self.escaped[value as usize] {
            let inst = self.classes[c as usize].inst;
            self.escape(inst, Why::HeldBy(value), at);
        }
        c
    }

    fn new_class(&mut self, name: Rc<str>, at: u32, value: Option<Abs>) -> u32 {
        let c = self.classes.len() as u32;
        let value = value.unwrap_or_else(|| self.abs(Kind::Class(c), at));
        let proto = self.abs(Kind::Proto(c), at);
        let inst = self.abs(Kind::Inst(c), at);
        let (super_ctor, super_proto, super_static) = (self.node(), self.node(), self.node());
        self.classes.push(ClassInfo {
            name,
            value,
            proto,
            inst,
            ctor: None,
            parents: Vec::new(),
            children: Vec::new(),
            this_inst: Vec::new(),
            this_static: Vec::new(),
            setters: Vec::new(),
            static_setters: Vec::new(),
            super_ctor,
            super_proto,
            super_static,
            bases: Vec::new(),
        });
        let (p, k) = (self.key("prototype"), self.key("constructor"));
        let pf = self.field(value, p);
        self.add(pf, proto);
        let cf = self.field(proto, k);
        self.add(cf, value);
        c
    }

    fn lineage(&self, c: u32, up: bool) -> Vec<u32> {
        let mut out = vec![c];
        let mut i = 0;
        while i < out.len() {
            let k = &self.classes[out[i] as usize];
            for &n in if up { &k.parents } else { &k.children } {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            i += 1;
        }
        out
    }

    /// Class `c` extends class `p`.
    fn link(&mut self, c: u32, p: u32) {
        if c == p || self.classes[c as usize].parents.contains(&p) {
            return;
        }
        self.classes[c as usize].parents.push(p);
        self.classes[p as usize].children.push(c);
        let (down, up) = (self.lineage(c, false), self.lineage(p, true));
        for &d in &down {
            for &a in &up {
                self.inherit(d, a);
                for b in self.classes[a as usize].bases.clone() {
                    self.base(d, &b);
                }
            }
        }
    }

    /// Class `d`'s instances get what class `a` (`d` or an ancestor)
    /// defines.
    fn inherit(&mut self, d: u32, a: u32) {
        if !self.inherited.insert((d, a)) {
            return;
        }
        let (dv, di) = (self.classes[d as usize].value, self.classes[d as usize].inst);
        let k = &self.classes[a as usize];
        let (av, ap) = (k.value, k.proto);
        let (ti, ts) = (k.this_inst.clone(), k.this_static.clone());
        let (sets, ssets) = (k.setters.clone(), k.static_setters.clone());
        self.mirror(ap, di);
        if d != a {
            self.mirror(av, dv);
        }
        for n in ti {
            self.add(n, di);
        }
        for n in ts {
            self.add(n, dv);
        }
        for (k, p) in sets {
            let f = self.field(di, k);
            self.edge(f, p);
        }
        for (k, p) in ssets {
            let f = self.field(dv, k);
            self.edge(f, p);
        }
        // (What was stored as a function class's `prototype`.)
        let pk = self.key("prototype");
        let pf = self.field(av, pk);
        let mut i = 0;
        while i < self.pts[pf as usize].len() {
            let x = self.pts[pf as usize][i];
            if x != ap && !matches!(self.kind(x), Kind::Top | Kind::Prim | Kind::Builtin(_)) {
                self.mirror(x, di);
            }
            i += 1;
        }
    }

    /// Class `c` (and those extending it) extends builtin `name`.
    fn base(&mut self, c: u32, name: &str) {
        let name: Rc<str> = match name {
            "Map" | "WeakMap" | "Set" | "WeakSet" | "Array" | "Promise" | "Object" => name.into(),
            n if n.ends_with("Error") => "Error".into(),
            _ => "?".into(),
        };
        for d in self.lineage(c, false) {
            if self.classes[d as usize].bases.contains(&name) {
                continue;
            }
            self.classes[d as usize].bases.push(name.clone());
            let (inst, value) = (self.classes[d as usize].inst, self.classes[d as usize].value);
            match &*name {
                "?" => {
                    for i in 0..self.undeclared.len() {
                        let k = self.undeclared[i];
                        for x in [inst, value] {
                            let f = self.field(x, k);
                            self.add(f, TOP);
                        }
                    }
                }
                "Error" => {
                    for k in ["message", "stack", "name", "code"] {
                        let k = self.key(k);
                        let f = self.field(inst, k);
                        self.add(f, PRIM);
                    }
                }
                "Object" => {}
                n => {
                    let class = match n {
                        "WeakMap" => "Map",
                        "WeakSet" => "Set",
                        n => n,
                    };
                    if class == "Map" {
                        self.map_pairs(inst);
                    }
                    for m in methods(class) {
                        let k = self.key(m);
                        let f = self.field(inst, k);
                        let b = self.builtin(&format!("{class}.prototype.{m}"));
                        self.add(f, b);
                    }
                }
            }
        }
    }

    /// A map's entries: arrays of its keys and values.
    fn map_pairs(&mut self, m: Abs) {
        let at = self.ats[m as usize];
        let pair = self.made(m, 201, Kind::Arr, at);
        let (k, v, e, pe) = (self.field(m, MKEY), self.field(m, MVAL), self.field(m, ELEM), self.field(pair, ELEM));
        self.edge(k, pe);
        self.edge(v, pe);
        self.add(e, pair);
    }

    fn container(&self, a: Abs, class: &str) -> bool {
        match (self.kind(a), class) {
            (Kind::Arr, "Array") | (Kind::Map, "Map" | "WeakMap") | (Kind::Set, "Set" | "WeakSet") => true,
            (Kind::Promise, "Promise") => true,
            (Kind::Inst(c), _) => {
                let class = match class {
                    "WeakMap" => "Map",
                    "WeakSet" => "Set",
                    c => c,
                };
                self.classes[*c as usize].bases.iter().any(|b| &**b == class)
            }
            _ => false,
        }
    }

    fn apply(&mut self, c: u32, a: Abs) {
        let con = self.constraints[c as usize].clone();
        match con {
            Cons::Load { dst, key } => self.apply_load(a, dst, key),
            Cons::Store { key, src, at, check, what } => self.apply_store(a, key, src, at, check, &what),
            Cons::Delete { key, at } => {
                if let Some(site) = self.site_of(a) {
                    self.unstable(site, Why::Deletes(key), at);
                }
            }
            Cons::Break { why, at } => {
                if let Some(site) = self.site_of(a) {
                    self.unstable(site, why, at);
                }
            }
            Cons::Call(ci) => self.apply_call(ci, a),
            Cons::Method { call, class, key } => self.method(call, a, &class, &key),
            Cons::Escape { why, at } => self.escape(a, why, at),
            Cons::Await { dst } => match self.kind(a).clone() {
                Kind::Promise => {
                    let e = self.field(a, ELEM);
                    self.edge(e, dst);
                }
                Kind::Inst(_) if self.container(a, "Promise") => {
                    let e = self.field(a, ELEM);
                    self.edge(e, dst);
                }
                Kind::Obj(_) | Kind::Inst(_) | Kind::Proto(_) | Kind::Class(_) => {
                    // (A thenable: its `then`, if it gets one, is called with
                    // resolving functions.)
                    self.add(dst, a);
                    let res = match self.thens.get(&a) {
                        Some(&n) => n,
                        None => {
                            let n = self.node();
                            self.thens.insert(a, n);
                            let k = self.key("then");
                            let then = self.field(a, k);
                            self.constrain(then, Cons::Then { this: a, dst: n });
                            n
                        }
                    };
                    self.edge(res, dst);
                }
                _ => self.add(dst, a),
            },
            Cons::Then { this, dst } => {
                if !matches!(self.kind(a), Kind::Fn(_) | Kind::Bound { .. } | Kind::Top) {
                    return;
                }
                let at = self.ats[this as usize];
                let callee = self.holding(a);
                let this = self.holding(this);
                let res = self.resolve_of(dst);
                let (r, j) = (self.holding(res), self.holding(REJECT));
                self.invoke(callee, this, vec![r, j], at, &"a `then` method".into());
            }
            Cons::Iterate { dst } => match self.kind(a).clone() {
                Kind::Arr | Kind::Set | Kind::Iter | Kind::Gen(_) | Kind::Map | Kind::Box => {
                    let e = self.field(a, ELEM);
                    self.edge(e, dst);
                }
                Kind::Inst(_) if ["Array", "Map", "Set"].iter().any(|c| self.container(a, c)) => {
                    let e = self.field(a, ELEM);
                    self.edge(e, dst);
                }
                Kind::Prim => self.add(dst, PRIM),
                Kind::Top | Kind::Builtin(_) => self.add(dst, TOP),
                _ => {}
            },
            Cons::Mirror { dst } => match self.kind(a) {
                Kind::Builtin(n) if &**n == "Object.prototype" => {}
                Kind::Top | Kind::Builtin(_) => {
                    let f = self.field(dst, ANY);
                    self.add(f, TOP);
                }
                Kind::Prim | Kind::Reject => {}
                _ => self.mirror(a, dst),
            },
            Cons::SetProto { proto, at } => match self.kind(a) {
                Kind::Top | Kind::Builtin(_) => self.escape_node(proto, Why::PassedTo("Object.setPrototypeOf".into()), at),
                Kind::Prim => {}
                _ => self.constrain(proto, Cons::Mirror { dst: a }),
            },
            Cons::GetProto { dst } => {
                if self.proto_readers[a as usize].contains(&dst) {
                    return;
                }
                if matches!(self.kind(a), Kind::Obj(_) | Kind::Arr | Kind::Fn(_)) && self.protos[a as usize].is_empty() {
                    let name = match self.kind(a) {
                        Kind::Arr => "Array.prototype",
                        Kind::Fn(_) => "Function.prototype",
                        _ => "Object.prototype",
                    };
                    let b = self.builtin(name);
                    self.add(dst, b);
                }
                if matches!(self.kind(a), Kind::Top) {
                    self.add(dst, TOP);
                }
                self.proto_readers[a as usize].push(dst);
                let mut i = 0;
                while i < self.protos[a as usize].len() {
                    let p = self.protos[a as usize][i];
                    self.add(dst, p);
                    i += 1;
                }
            }
            Cons::Link { class } => match self.kind(a).clone() {
                Kind::Class(p) => {
                    self.link(class, p);
                    let k = &self.classes[p as usize];
                    let (pp, pv) = (k.proto, k.value);
                    let (sp, ss) = (self.classes[class as usize].super_proto, self.classes[class as usize].super_static);
                    self.add(sp, pp);
                    self.add(ss, pv);
                }
                Kind::Fn(f) => {
                    let p = self.class_of(f);
                    self.link(class, p);
                    let pp = self.classes[p as usize].proto;
                    let sp = self.classes[class as usize].super_proto;
                    self.add(sp, pp);
                    let ss = self.classes[class as usize].super_static;
                    self.add(ss, a);
                }
                Kind::Builtin(name) => {
                    self.base(class, &name);
                    let b = self.builtin(&format!("{name}.prototype"));
                    let k = &self.classes[class as usize];
                    let (sp, ss) = (k.super_proto, k.super_static);
                    self.add(sp, b);
                    self.add(ss, a);
                }
                Kind::Top => {
                    self.base(class, "?");
                    let k = &self.classes[class as usize];
                    let (sp, ss) = (k.super_proto, k.super_static);
                    self.add(sp, TOP);
                    self.add(ss, TOP);
                }
                _ => {}
            },
            Cons::ProtoOf { class } => {
                if a == self.classes[class as usize].proto
                    || matches!(self.kind(a), Kind::Top | Kind::Prim | Kind::Builtin(_))
                {
                    if matches!(self.kind(a), Kind::Top) {
                        for d in self.lineage(class, false) {
                            let i = self.classes[d as usize].inst;
                            let f = self.field(i, ANY);
                            self.add(f, TOP);
                        }
                    }
                    return;
                }
                for d in self.lineage(class, false) {
                    let i = self.classes[d as usize].inst;
                    self.mirror(a, i);
                }
            }
        }
    }

    fn resolve_of(&mut self, n: Node) -> Abs {
        if let Some(&a) = self.resolves.get(&n) {
            return a;
        }
        let a = self.abs(Kind::Resolve(n), 0);
        self.resolves.insert(n, a);
        a
    }

    fn fields_into(&mut self, a: Abs, k: Key, dst: Node) {
        if k == ANY {
            self.read_all(a, Reader::Copy(dst));
        } else {
            let f = self.field(a, k);
            self.edge(f, dst);
            let any = self.field(a, ANY);
            self.edge(any, dst);
        }
    }

    fn apply_load(&mut self, a: Abs, dst: Node, k: Key) {
        let name = self.keys[k as usize].clone();
        let fn_method = matches!(&*name, "call" | "apply" | "bind");
        // (What every object inherits: its constructor, its prototype. A
        // computed key is taken to name neither: see `analyze`.)
        if name == "constructor" {
            let ctor = match self.kind(a) {
                Kind::Fn(_) | Kind::Class(_) | Kind::Bound { .. } | Kind::Resolve(_) | Kind::Reject => Some("Function"),
                Kind::Obj(_) => Some("Object"),
                Kind::Arr => Some("Array"),
                Kind::Map => Some("Map"),
                Kind::Set => Some("Set"),
                Kind::Promise => Some("Promise"),
                Kind::Re => Some("RegExp"),
                Kind::Prim => Some("String"),
                Kind::Iter | Kind::Gen(_) | Kind::Box => Some("Object"),
                _ => None,
            };
            if let Some(c) = ctor {
                let b = self.builtin(c);
                self.add(dst, b);
            }
        }
        if name == "__proto__" {
            let c = self.get_proto(dst);
            self.apply(c, a);
        }
        match self.kind(a).clone() {
            Kind::Top => self.add(dst, TOP),
            Kind::Prim => {
                self.add(dst, PRIM);
                if k != ANY && name != "length" && !self.index_key[k as usize] {
                    let b = self.builtin(&format!("String.prototype.{name}"));
                    self.add(dst, b);
                }
            }
            Kind::Re => {
                self.add(dst, PRIM);
                if matches!(&*name, "exec" | "test" | "@@match" | "@@matchAll" | "@@replace" | "@@split" | "@@search") {
                    let b = self.builtin(&format!("RegExp.prototype.{name}"));
                    self.add(dst, b);
                }
            }
            Kind::Builtin(b) => {
                let v = if k == ANY {
                    TOP
                } else if fn_method && !is_namespace(&b) {
                    self.builtin(&format!("Function.prototype.{name}"))
                } else if &*b == "globalThis" {
                    if is_global(&name) { self.builtin(&name) } else { TOP }
                } else {
                    // (Only the builtins' own members and their prototypes'
                    // are told apart: deeper, what code outside the program
                    // made.)
                    let full = format!("{b}.{name}");
                    let parts: Vec<&str> = full.split('.').collect();
                    if parts.len() == 2 || (parts.len() == 3 && parts[1] == "prototype") {
                        self.builtin(&full)
                    } else {
                        TOP
                    }
                };
                self.add(dst, v);
            }
            Kind::Reject | Kind::Resolve(_) | Kind::Bound { .. } => {
                if fn_method {
                    let b = self.builtin(&format!("Function.prototype.{name}"));
                    self.add(dst, b);
                }
            }
            Kind::Fn(f) => {
                if fn_method {
                    let b = self.builtin(&format!("Function.prototype.{name}"));
                    self.add(dst, b);
                } else if name == "name" || name == "length" {
                    self.add(dst, PRIM);
                } else if name == "prototype" {
                    self.class_of(f);
                }
                self.fields_into(a, k, dst);
            }
            Kind::Arr => {
                if k == ANY || self.index_key[k as usize] {
                    let e = self.field(a, ELEM);
                    self.edge(e, dst);
                }
                if name == "length" {
                    self.add(dst, PRIM);
                } else if methods("Array").contains(&&*name) {
                    let b = self.builtin(&format!("Array.prototype.{name}"));
                    self.add(dst, b);
                }
                self.fields_into(a, k, dst);
            }
            Kind::Map | Kind::Set | Kind::Promise | Kind::Iter | Kind::Gen(_) | Kind::Box => {
                let class = match self.kind(a) {
                    Kind::Map => "Map",
                    Kind::Set => "Set",
                    Kind::Promise => "Promise",
                    Kind::Box => "Box",
                    _ => "Iterator",
                };
                if name == "size" {
                    self.add(dst, PRIM);
                } else if methods(class).contains(&&*name) {
                    let b = self.builtin(&format!("{class}.prototype.{name}"));
                    self.add(dst, b);
                }
                self.fields_into(a, k, dst);
            }
            Kind::Obj(_) => {
                if methods("Object").contains(&&*name) {
                    let b = self.builtin(&format!("Object.prototype.{name}"));
                    self.add(dst, b);
                }
                self.fields_into(a, k, dst);
            }
            Kind::Inst(_) | Kind::Proto(_) => {
                if methods("Object").contains(&&*name) {
                    let b = self.builtin(&format!("Object.prototype.{name}"));
                    self.add(dst, b);
                }
                self.fields_into(a, k, dst);
            }
            Kind::Class(_) => {
                if fn_method {
                    let b = self.builtin(&format!("Function.prototype.{name}"));
                    self.add(dst, b);
                } else if name == "name" || name == "length" {
                    self.add(dst, PRIM);
                }
                self.fields_into(a, k, dst);
            }
        }
    }

    /// A constraint that copies the prototypes of what arrives to `dst`
    /// (made once per node).
    fn get_proto(&mut self, dst: Node) -> u32 {
        if let Some(&c) = self.get_protos.get(&dst) {
            return c;
        }
        let c = self.constraints.len() as u32;
        self.constraints.push(Cons::GetProto { dst });
        self.get_protos.insert(dst, c);
        c
    }

    fn apply_store(&mut self, a: Abs, k: Key, src: Node, at: u32, check: bool, what: &Rc<str>) {
        if self.keys[k as usize] == "__proto__" {
            if let Some(site) = self.site_of(a) {
                self.unstable(site, Why::SetsProto, at);
            }
            match self.kind(a) {
                Kind::Top | Kind::Builtin(_) => self.escape_node(src, Why::StoredIn(what.clone()), at),
                Kind::Prim => {}
                _ => self.constrain(src, Cons::Mirror { dst: a }),
            }
            return;
        }
        if check && let Some(site) = self.site_of(a) {
            if k == ANY {
                self.unstable(site, Why::WritesComputed, at);
            } else if !self.sites[site as usize].keys.contains(&k) {
                self.unstable(site, Why::Adds(k), at);
            }
        }
        if src == EMPTY || src == PRIMS {
            return;
        }
        match self.kind(a).clone() {
            Kind::Prim | Kind::Reject | Kind::Resolve(_) => {}
            Kind::Builtin(b) if &*b == "globalThis" => {
                let name: Rc<str> = if k == ANY { "a computed global".into() } else { self.keys[k as usize].clone().into() };
                self.escape_node(src, Why::Global(name), at);
            }
            Kind::Top | Kind::Builtin(_) | Kind::Re => self.escape_node(src, Why::StoredIn(what.clone()), at),
            Kind::Arr if k == ANY || self.index_key[k as usize] => {
                let e = self.field(a, ELEM);
                self.edge(src, e);
            }
            Kind::Fn(f) => {
                if self.keys[k as usize] == "prototype" {
                    self.class_of(f);
                }
                let f = self.field(a, k);
                self.edge(src, f);
            }
            _ => {
                let f = self.field(a, k);
                self.edge(src, f);
            }
        }
    }

    /// The node for argument `i` of a call.
    fn arg(&self, ci: &CallInfo, i: usize) -> Node {
        match ci.spread {
            Some(s) if i >= s => ci.tail,
            _ => ci.args.get(i).copied().unwrap_or(EMPTY),
        }
    }

    fn escape_args(&mut self, ci: &CallInfo, why: Why) {
        for &n in ci.args.iter().chain([&ci.this]) {
            self.escape_node(n, why.clone(), ci.at);
        }
    }

    /// Call what `callee` holds with `d`, a call that call `parent` makes
    /// when builtin `a` (`call`, `apply`...) arrives at its callee: once
    /// per parent and `a`. (Deep chains of them, which only programs
    /// calling `call` with `call` make, are taken to reach code outside the
    /// program.)
    fn derive(&mut self, parent: u32, a: Abs, callee: Node, mut d: CallInfo) {
        if self.derived.contains_key(&(parent, a)) {
            return;
        }
        d.depth = self.calls[parent as usize].depth.saturating_add(1);
        if d.depth > 8 {
            self.escape_args(&d, Why::PassedTo(d.text.clone()));
            self.add(d.result, TOP);
            return;
        }
        let c = self.call_info(d);
        self.derived.insert((parent, a), c);
        self.constrain(callee, Cons::Call(c));
    }

    /// The call that call `c` makes for what arrives at its callee and
    /// calls another (`kind`: a bound function, without bound arguments or
    /// with, a class's implicit constructor): one per call and kind, whose
    /// callee, `this`, arguments and result are nodes of its own that what
    /// arrives adds to. Past depth 4, it is `c`'s nearest ancestor of that
    /// kind (chains of functions bound again, of classes extending
    /// themselves through mixins, end).
    fn forward(&mut self, c: u32, kind: u8) -> u32 {
        let ci = self.calls[c as usize].clone();
        let canon = self.canon(c);
        let f = match self.forwards.get(&(canon, kind)) {
            Some(&f) => f,
            None => {
                let (callee, this, result) = (self.node(), self.node(), self.node());
                let (args, spread, tail) = if kind == FWD_BOUND_ARGS {
                    let t = self.node();
                    (vec![t], Some(0), t)
                } else {
                    let args: Vec<Node> = ci.args.iter().map(|_| self.node()).collect();
                    let tail = if ci.spread.is_some() { self.node() } else { EMPTY };
                    (args, ci.spread, tail)
                };
                let sup = kind == FWD_SUPER;
                let d = CallInfo {
                    callee,
                    this,
                    args,
                    spread,
                    tail,
                    result: if sup { EMPTY } else { result },
                    new: !sup && ci.new,
                    sup: sup || ci.sup,
                    depth: ci.depth.saturating_add(1),
                    parent: Some(c),
                    ..ci.clone()
                };
                let f = self.call_info(d);
                self.forwards.insert((canon, kind), f);
                self.constrain(callee, Cons::Call(f));
                f
            }
        };
        if !self.wired.insert((c, f)) {
            return f;
        }
        // (`c`'s arguments to it, its results to `c`.)
        let fi = self.calls[f as usize].clone();
        if fi.spread == Some(0) && fi.args.len() == 1 {
            for &n in ci.args.iter().chain([&ci.tail]) {
                self.edge(n, fi.tail);
            }
        } else {
            for (i, &n) in ci.args.iter().enumerate() {
                match fi.args.get(i) {
                    Some(&m) => self.edge(n, m),
                    None => self.edge(n, fi.tail),
                }
            }
            self.edge(ci.tail, fi.tail);
        }
        if !fi.sup || kind != FWD_SUPER {
            self.edge(fi.result, ci.result);
        }
        f
    }

    /// Bind a call's arguments to `f`'s parameters.
    fn bind(&mut self, f: u32, ci: &CallInfo) {
        let (params, rest, args, this, result, arrow) = {
            let f = &self.funcs[f as usize];
            (f.params.clone(), f.rest, f.args, f.this, f.result, f.arrow)
        };
        for (i, &p) in params.iter().enumerate() {
            match ci.spread {
                Some(s) if i >= s => {
                    for j in s..ci.args.len() {
                        self.edge(ci.args[j], p);
                    }
                }
                _ => {
                    if let Some(&n) = ci.args.get(i) {
                        self.edge(n, p);
                    }
                }
            }
        }
        let ae = self.field(args, ELEM);
        for &n in &ci.args {
            self.edge(n, ae);
        }
        if let Some(r) = rest {
            let re = self.field(r, ELEM);
            let from = ci.spread.map_or(params.len(), |s| s.min(params.len()));
            for &n in ci.args.iter().skip(from) {
                self.edge(n, re);
            }
        }
        if !arrow && !ci.new && !ci.sup {
            self.edge(ci.this, this);
        }
        if !ci.sup {
            self.edge(result, ci.result);
        }
    }

    fn apply_call(&mut self, c: u32, a: Abs) {
        let ci = self.calls[c as usize].clone();
        match self.kind(a).clone() {
            Kind::Fn(f) => {
                if ci.new || ci.sup {
                    let k = self.class_of(f);
                    if ci.new {
                        let inst = self.classes[k as usize].inst;
                        self.add(ci.result, inst);
                    }
                }
                self.bind(f, &ci);
            }
            Kind::Class(k) => {
                if !ci.new && !ci.sup {
                    return;
                }
                if ci.new {
                    let inst = self.classes[k as usize].inst;
                    self.add(ci.result, inst);
                }
                match self.classes[k as usize].ctor {
                    Some(f) => self.bind(f, &CallInfo { sup: true, ..ci.clone() }),
                    None => {
                        // (The implicit constructor passes its arguments on.)
                        let sup = self.classes[k as usize].super_ctor;
                        let f = self.forward(c, FWD_SUPER);
                        let callee = self.calls[f as usize].callee;
                        self.edge(sup, callee);
                    }
                }
            }
            Kind::Bound { target, this, args } => {
                let f = self.forward(c, if args.is_empty() { FWD_BOUND } else { FWD_BOUND_ARGS });
                let fi = self.calls[f as usize].clone();
                self.edge(target, fi.callee);
                if !ci.new {
                    self.edge(this, fi.this);
                }
                for b in args {
                    self.edge(b, fi.tail);
                }
            }
            Kind::Resolve(n) => {
                let x = self.arg(&ci, 0);
                self.await_into(x, n);
            }
            Kind::Reject => {
                let x = self.arg(&ci, 0);
                self.edge(x, THROWN);
            }
            Kind::Builtin(name) => self.builtin_call(&name, c, a),
            Kind::Top => {
                self.escape_args(&ci, Why::PassedTo(ci.text.clone()));
                self.add(ci.result, TOP);
            }
            _ => {}
        }
    }

    /// What `define` adds to field `key` of what `o` holds: the
    /// descriptor `desc`'s value, or its getter's results (and its setter
    /// gets what is stored).
    fn define(&mut self, o: Node, key: Key, desc: Node, at: u32) {
        let what: Rc<str> = "a property".into();
        let v = self.load(desc, self.key_ids["value"]);
        self.constrain(o, Cons::Store { key, src: v, at, check: false, what: what.clone() });
        let gk = self.key("get");
        let g = self.load(desc, gk);
        let r = self.invoke(g, o, vec![], at, &what);
        self.constrain(o, Cons::Store { key, src: r, at, check: false, what: what.clone() });
        let sk = self.key("set");
        let s = self.load(desc, sk);
        let cur = self.load(o, key);
        self.invoke(s, o, vec![cur], at, &what);
    }

    /// A fresh object of JSON data: objects and arrays holding primitives
    /// and each other.
    fn json(&mut self, c: u32, at: u32) -> (Abs, Abs) {
        let (o, a) = (self.made(c, 10, Kind::Obj(None), at), self.made(c, 11, Kind::Arr, at));
        let (of, ae) = (self.field(o, ANY), self.field(a, ELEM));
        for n in [of, ae] {
            self.add(n, PRIM);
            self.add(n, o);
            self.add(n, a);
        }
        (o, a)
    }

    fn fresh_obj(&mut self, c: u32, tag: u8, at: u32) -> Abs {
        let o = self.made(c, tag, Kind::Obj(None), at);
        let f = self.field(o, ANY);
        self.add(f, PRIM);
        o
    }

    fn builtin_call(&mut self, name: &str, c: u32, b: Abs) {
        let key = (self.canon(c), b);
        if let Some(&first) = self.builtin_calls.get(&key) {
            self.wire(c, first);
            return;
        }
        self.builtin_calls.insert(key, c);
        let cur = self.cur.replace(c);
        self.builtin_model(name, c, b);
        self.cur = cur;
    }

    fn builtin_model(&mut self, name: &str, c: u32, b: Abs) {
        let ci = self.calls[c as usize].clone();
        let at = ci.at;
        let r = ci.result;
        let a0 = self.arg(&ci, 0);
        let a1 = self.arg(&ci, 1);
        let a2 = self.arg(&ci, 2);
        let text: Rc<str> = name.into();
        match name {
            "Function.prototype.call" => {
                let (args, spread) = shift(&ci, 1);
                self.derive(c, b, ci.this, CallInfo { this: a0, args, spread, ..ci.clone() });
                return;
            }
            "Function.prototype.apply" | "Reflect.apply" | "Reflect.construct" => {
                let (callee, this, list) = match name {
                    "Reflect.apply" => (a0, a1, a2),
                    "Reflect.construct" => (a0, EMPTY, a1),
                    _ => (ci.this, a0, a1),
                };
                let elems = self.load(list, ANY);
                let new = name == "Reflect.construct";
                let d = CallInfo { this, args: vec![elems], spread: Some(0), tail: elems, new, ..ci.clone() };
                self.derive(c, b, callee, d);
                return;
            }
            "Function.prototype.bind" => {
                let (args, _) = shift(&ci, 1);
                let b = self.made(c, 0, Kind::Bound { target: ci.this, this: a0, args }, at);
                self.add(r, b);
                return;
            }
            _ => {}
        }
        if let Some((class, key)) = name.split_once(".prototype.") {
            let (class, key) = (class.into(), key.into());
            self.constrain(ci.this, Cons::Method { call: c, class, key });
            return;
        }
        let pure = |n: &str| {
            [
                "Math.", "Number.", "String.", "Date.", "BigInt.", "Symbol.", "Atomics.", "console.", "performance.",
                "Intl.",
            ]
            .iter()
            .any(|p| n.starts_with(p))
        };
        match name {
            "Object.keys" | "Object.getOwnPropertyNames" | "Object.getOwnPropertySymbols" | "Reflect.ownKeys" => {
                let arr = self.made(c, 0, Kind::Arr, at);
                let e = self.field(arr, ELEM);
                self.add(e, PRIM);
                self.add(r, arr);
            }
            "Object.values" | "Object.entries" => {
                let arr = self.made(c, 0, Kind::Arr, at);
                let e = self.field(arr, ELEM);
                let into = if name == "Object.values" {
                    e
                } else {
                    let pair = self.made(c, 1, Kind::Arr, at);
                    self.add(e, pair);
                    let pe = self.field(pair, ELEM);
                    self.add(pe, PRIM);
                    pe
                };
                self.constrain(a0, Cons::Load { dst: into, key: ANY });
                self.add(r, arr);
            }
            "Object.fromEntries" => {
                let o = self.fresh_obj(c, 0, at);
                let f = self.field(o, ANY);
                let e = self.iterate(a0, false, at);
                let v = self.iterate(e, false, at);
                self.edge(v, f);
                self.add(r, o);
            }
            "Object.assign" => {
                self.constrain(a0, Cons::Break { why: Why::Defines("Object.assign"), at });
                let all = self.node();
                for i in 1..ci.args.len() {
                    self.constrain(ci.args[i], Cons::Load { dst: all, key: ANY });
                }
                self.constrain(a0, Cons::Store { key: ANY, src: all, at, check: false, what: text });
                self.edge(a0, r);
            }
            "Object.freeze" | "Object.seal" | "Object.preventExtensions" | "Reflect.preventExtensions" => {
                let why = Why::Defines(match name {
                    "Object.freeze" => "Object.freeze",
                    "Object.seal" => "Object.seal",
                    _ => "Object.preventExtensions",
                });
                self.constrain(a0, Cons::Break { why, at });
                if name.starts_with("Object") { self.edge(a0, r) } else { self.add(r, PRIM) }
            }
            "Object.defineProperty" | "Reflect.defineProperty" => {
                self.constrain(a0, Cons::Break { why: Why::Defines("Object.defineProperty"), at });
                self.define(a0, ANY, a2, at);
                if name.starts_with("Object") { self.edge(a0, r) } else { self.add(r, PRIM) }
            }
            "Object.defineProperties" => {
                self.constrain(a0, Cons::Break { why: Why::Defines("Object.defineProperties"), at });
                let d = self.load(a1, ANY);
                self.define(a0, ANY, d, at);
                self.edge(a0, r);
            }
            "Object.create" => {
                let o = self.made(c, 0, Kind::Obj(None), at);
                self.constrain(a0, Cons::Mirror { dst: o });
                if ci.args.len() > 1 {
                    let on = self.holding(o);
                    let d = self.load(a1, ANY);
                    self.define(on, ANY, d, at);
                }
                self.add(r, o);
            }
            "Object.setPrototypeOf" | "Reflect.setPrototypeOf" => {
                self.constrain(a0, Cons::Break { why: Why::SetsProto, at });
                self.constrain(a0, Cons::SetProto { proto: a1, at });
                if name.starts_with("Object") { self.edge(a0, r) } else { self.add(r, PRIM) }
            }
            "Object.getPrototypeOf" | "Reflect.getPrototypeOf" => self.constrain(a0, Cons::GetProto { dst: r }),
            "Object.getOwnPropertyDescriptor" | "Reflect.getOwnPropertyDescriptor" => {
                let d = self.fresh_obj(c, 0, at);
                let f = self.field(d, ANY);
                self.constrain(a0, Cons::Load { dst: f, key: ANY });
                self.add(r, d);
            }
            "Object.getOwnPropertyDescriptors" => {
                let (all, d) = (self.fresh_obj(c, 0, at), self.fresh_obj(c, 1, at));
                let f = self.field(d, ANY);
                self.constrain(a0, Cons::Load { dst: f, key: ANY });
                let af = self.field(all, ANY);
                self.add(af, d);
                self.add(r, all);
            }
            "Object.groupBy" | "Map.groupBy" => {
                let items = self.iterate(a0, false, at);
                let k = self.invoke(a1, EMPTY, vec![items, PRIMS], at, &text);
                let group = self.made(c, 1, Kind::Arr, at);
                let ge = self.field(group, ELEM);
                self.edge(items, ge);
                let out = if name == "Map.groupBy" {
                    let m = self.made(c, 0, Kind::Map, at);
                    self.map_pairs(m);
                    let (mk, mv) = (self.field(m, MKEY), self.field(m, MVAL));
                    self.edge(k, mk);
                    self.add(mv, group);
                    m
                } else {
                    let o = self.fresh_obj(c, 0, at);
                    let f = self.field(o, ANY);
                    self.add(f, group);
                    o
                };
                self.add(r, out);
            }
            "Object" => {
                self.edge(a0, r);
                let o = self.made(c, 0, Kind::Obj(None), at);
                self.add(r, o);
            }
            "Reflect.get" => self.constrain(a0, Cons::Load { dst: r, key: ANY }),
            "Reflect.set" => {
                self.constrain(a0, Cons::Store { key: ANY, src: a2, at, check: true, what: text });
                self.add(r, PRIM);
            }
            "Reflect.deleteProperty" => {
                self.constrain(a0, Cons::Delete { key: ANY, at });
                self.add(r, PRIM);
            }
            "Array.from" | "Iterator.from" => {
                let arr = self.made(c, 0, if name == "Array.from" { Kind::Arr } else { Kind::Iter }, at);
                let e = self.field(arr, ELEM);
                let items = self.iterate(a0, false, at);
                self.constrain(a0, Cons::Load { dst: items, key: ANY });
                if ci.args.len() > 1 {
                    let m = self.invoke(a1, a2, vec![items, PRIMS], at, &text);
                    self.edge(m, e);
                } else {
                    self.edge(items, e);
                }
                self.add(r, arr);
            }
            "Array" | "Array.of" => {
                let arr = self.made(c, 0, Kind::Arr, at);
                let e = self.field(arr, ELEM);
                for &n in &ci.args {
                    self.edge(n, e);
                }
                self.add(r, arr);
            }
            "Promise" => {
                let p = self.made(c, 0, Kind::Promise, at);
                let e = self.field(p, ELEM);
                let res = self.resolve_of(e);
                let (rn, jn) = (self.holding(res), self.holding(REJECT));
                self.invoke(a0, EMPTY, vec![rn, jn], at, &text);
                self.add(r, p);
            }
            "Promise.resolve" | "Promise.reject" | "Promise.try" => {
                let p = self.made(c, 0, Kind::Promise, at);
                let e = self.field(p, ELEM);
                match name {
                    "Promise.resolve" => self.await_into(a0, e),
                    "Promise.reject" => self.edge(a0, THROWN),
                    _ => {
                        let (args, spread) = shift(&ci, 1);
                        let x = self.node();
                        let c = self.call_info(CallInfo { this: EMPTY, args, spread, result: x, ..ci.clone() });
                        self.constrain(a0, Cons::Call(c));
                        self.await_into(x, e);
                    }
                }
                self.add(r, p);
            }
            "Promise.all" | "Promise.race" | "Promise.any" | "Promise.allSettled" => {
                let p = self.made(c, 0, Kind::Promise, at);
                let e = self.field(p, ELEM);
                let xs = self.iterate(a0, false, at);
                let v = self.await_(xs);
                match name {
                    "Promise.all" | "Promise.allSettled" => {
                        let arr = self.made(c, 1, Kind::Arr, at);
                        let ae = self.field(arr, ELEM);
                        if name == "Promise.all" {
                            self.edge(v, ae);
                        } else {
                            let o = self.fresh_obj(c, 2, at);
                            let (vk, rk) = (self.key("value"), self.key("reason"));
                            let (of, rf) = (self.field(o, vk), self.field(o, rk));
                            self.edge(v, of);
                            self.edge(CAUGHT, rf);
                            self.add(ae, o);
                        }
                        self.add(e, arr);
                    }
                    _ => self.edge(v, e),
                }
                self.add(r, p);
            }
            "Promise.withResolvers" => {
                let p = self.made(c, 0, Kind::Promise, at);
                let e = self.field(p, ELEM);
                let o = self.fresh_obj(c, 1, at);
                let res = self.resolve_of(e);
                for (k, v) in [("promise", p), ("resolve", res), ("reject", REJECT)] {
                    let k = self.key(k);
                    let f = self.field(o, k);
                    self.add(f, v);
                }
                self.add(r, o);
            }
            "Map" | "WeakMap" => {
                let m = self.made(c, 0, Kind::Map, at);
                self.map_pairs(m);
                if ci.args.len() > 0 {
                    let entries = self.iterate(a0, false, at);
                    let kv = self.iterate(entries, false, at);
                    let (k, v) = (self.field(m, MKEY), self.field(m, MVAL));
                    self.edge(kv, k);
                    self.edge(kv, v);
                }
                self.add(r, m);
            }
            "Set" | "WeakSet" => {
                let s = self.made(c, 0, Kind::Set, at);
                if ci.args.len() > 0 {
                    let e = self.field(s, ELEM);
                    let xs = self.iterate(a0, false, at);
                    self.edge(xs, e);
                }
                self.add(r, s);
            }
            "WeakRef" | "FinalizationRegistry" => {
                let b = self.made(c, 0, Kind::Box, at);
                let e = self.field(b, ELEM);
                if name == "WeakRef" {
                    self.edge(a0, e);
                } else {
                    self.invoke(a0, EMPTY, vec![e], at, &text);
                }
                self.add(r, b);
            }
            "JSON.parse" | "structuredClone" => {
                let (o, arr) = self.json(c, at);
                if name == "JSON.parse" && ci.args.len() > 1 {
                    let this = self.node();
                    self.add(this, o);
                    self.add(this, arr);
                    let (of, ae) = (self.field(o, ANY), self.field(arr, ELEM));
                    let v = self.invoke(a1, this, vec![PRIMS, of], at, &text);
                    self.edge(v, of);
                    self.edge(v, ae);
                }
                self.add(r, o);
                self.add(r, arr);
                self.add(r, PRIM);
            }
            "JSON.stringify" => {
                if ci.args.len() > 1 && ci.plain & 2 == 0 {
                    self.escape_args(&ci, Why::PassedTo("JSON.stringify with a replacer".into()));
                }
                self.add(r, PRIM);
            }
            "setTimeout" | "setInterval" | "setImmediate" | "queueMicrotask" | "requestAnimationFrame" => {
                let skip = if name == "setTimeout" || name == "setInterval" { 2 } else { 1 };
                let args = ci.args.iter().skip(skip).copied().collect();
                self.invoke(a0, EMPTY, args, at, &text);
                self.add(r, PRIM);
            }
            "RegExp" => self.add(r, RE),
            "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError" | "EvalError" | "URIError"
            | "AggregateError" => {
                let o = self.fresh_obj(c, 0, at);
                let ck = self.key("cause");
                let cf = self.field(o, ck);
                let opts = if name == "AggregateError" { a2 } else { a1 };
                let c = self.load(opts, ck);
                self.edge(c, cf);
                if name == "AggregateError" {
                    let arr = self.made(c, 1, Kind::Arr, at);
                    let e = self.field(arr, ELEM);
                    let xs = self.iterate(a0, false, at);
                    self.edge(xs, e);
                    let k = self.key("errors");
                    let f = self.field(o, k);
                    self.add(f, arr);
                }
                self.add(r, o);
            }
            "Error.captureStackTrace" => {
                let k = self.key("stack");
                self.constrain(a0, Cons::Store { key: k, src: PRIMS, at, check: true, what: text });
                self.add(r, PRIM);
            }
            "Date" | "ArrayBuffer" | "SharedArrayBuffer" | "DataView" | "Int8Array" | "Uint8Array"
            | "Uint8ClampedArray" | "Int16Array" | "Uint16Array" | "Int32Array" | "Uint32Array" | "Float16Array"
            | "Float32Array" | "Float64Array" | "BigInt64Array" | "BigUint64Array" | "parseInt" | "parseFloat"
            | "isNaN" | "isFinite" | "encodeURIComponent" | "decodeURIComponent" | "encodeURI" | "decodeURI"
            | "escape" | "unescape" | "Number" | "String" | "Boolean" | "BigInt" | "Symbol" | "Object.is"
            | "Object.isFrozen" | "Object.isSealed" | "Object.isExtensible" | "Object.hasOwn" | "Reflect.has"
            | "Array.isArray" | "ArrayBuffer.isView" | "clearTimeout" | "clearInterval" | "clearImmediate"
            | "print" => self.add(r, PRIM),
            n if pure(n) => self.add(r, PRIM),
            _ => {
                self.escape_args(&ci, Why::PassedTo(text));
                self.add(r, TOP);
            }
        }
    }

    /// Builtin method `class.prototype.key` called on `a`: what it writes
    /// into `a`, here; what it reads, from every receiver of the call at
    /// once (`MethodState`), with its callbacks called once.
    fn method(&mut self, c: u32, a: Abs, class: &str, key: &str) {
        let ci = self.calls[c as usize].clone();
        let (at, r) = (ci.at, ci.result);
        let text: Rc<str> = format!("{class}.prototype.{key}").into();
        let a0 = self.arg(&ci, 0);
        let a1 = self.arg(&ci, 1);
        if matches!(self.kind(a), Kind::Top) {
            self.escape_args(&ci, Why::PassedTo(text));
            self.add(r, TOP);
            return;
        }
        let class = match class {
            "WeakMap" => "Map",
            "WeakSet" => "Set",
            "Number" | "Boolean" | "Symbol" | "BigInt" | "RegExp" => "String",
            "Generator" | "AsyncGenerator" => "Iterator",
            c => c,
        };
        // The receiver's parts: its elements, a map's keys and values.
        let (e, k, v) = match class {
            "Array" => match self.kind(a) {
                Kind::Arr => (self.field(a, ELEM), EMPTY, EMPTY),
                Kind::Inst(_) if self.container(a, "Array") => (self.field(a, ELEM), EMPTY, EMPTY),
                Kind::Obj(_) | Kind::Inst(_) | Kind::Proto(_) | Kind::Class(_) | Kind::Fn(_) => {
                    (self.field(a, ANY), EMPTY, EMPTY)
                }
                Kind::Prim => (PRIMS, EMPTY, EMPTY),
                _ => return,
            },
            "Map" if self.container(a, "Map") => (self.field(a, ELEM), self.field(a, MKEY), self.field(a, MVAL)),
            "Set" if self.container(a, "Set") => (self.field(a, ELEM), EMPTY, EMPTY),
            "Promise" if self.container(a, "Promise") => (self.field(a, ELEM), EMPTY, EMPTY),
            "Iterator" if matches!(self.kind(a), Kind::Iter | Kind::Gen(_)) => (self.field(a, ELEM), EMPTY, EMPTY),
            "Box" if matches!(self.kind(a), Kind::Box) => (self.field(a, ELEM), EMPTY, EMPTY),
            "Map" | "Set" | "Promise" | "Iterator" | "Box" => return,
            _ => (EMPTY, EMPTY, EMPTY),
        };
        // Writes into the receiver, and methods that return it.
        match (class, key) {
            ("Array", "push" | "unshift") => {
                for &n in &ci.args {
                    self.edge(n, e);
                }
            }
            ("Array", "splice") => {
                for &n in ci.args.iter().skip(2) {
                    self.edge(n, e);
                }
            }
            ("Array", "fill") => self.edge(a0, e),
            ("Map", "set" | "getOrInsert" | "getOrInsertComputed") => {
                self.edge(a0, k);
                if key != "getOrInsertComputed" {
                    self.edge(a1, v);
                }
            }
            ("Set", "add") => self.edge(a0, e),
            ("Box", "register") => self.edge(a1, e),
            ("Object", "__defineGetter__" | "__defineSetter__") => {
                if let Some(site) = self.site_of(a) {
                    self.unstable(site, Why::Defines("__defineGetter__"), at);
                }
                let this = self.holding(a);
                self.define(this, ANY, a1, at);
            }
            ("Iterator", "next") => {
                if let Kind::Gen(g) = *self.kind(a) {
                    let y = self.funcs[g as usize].yieldin;
                    self.edge(a0, y);
                }
            }
            _ => {}
        }
        if matches!(
            (class, key),
            ("Array", "reverse" | "sort" | "fill" | "copyWithin")
                | ("Map", "set")
                | ("Set", "add")
                | ("Object", "valueOf")
                | ("Iterator", "@@iterator" | "@@asyncIterator")
        ) {
            self.add(r, a);
        }
        let (st, first) = self.method_state(c, class, key);
        self.wire(c, st.call);
        self.edge(e, st.elems);
        self.edge(k, st.keys);
        self.edge(v, st.vals);
        self.add(st.recv, a);
        if let Kind::Gen(g) = *self.kind(a) {
            let ret = self.funcs[g as usize].ret;
            self.edge(ret, st.rets);
        }
        if first {
            let cur = self.cur.replace(st.call);
            self.method_reads(st.call, class, key, st);
            self.cur = cur;
        }
    }

    fn method_state(&mut self, c: u32, class: &str, key: &str) -> (MethodState, bool) {
        let k = (self.canon(c), format!("{class}.{key}"));
        if let Some(&st) = self.method_states.get(&k) {
            return (st, false);
        }
        let st = MethodState {
            call: c,
            elems: self.node(),
            keys: self.node(),
            vals: self.node(),
            recv: self.node(),
            rets: self.node(),
        };
        self.method_states.insert(k, st);
        (st, true)
    }

    /// What a builtin method reads from its receivers (`st`), returns,
    /// and calls.
    fn method_reads(&mut self, c: u32, class: &str, key: &str, st: MethodState) {
        let ci = self.calls[c as usize].clone();
        let (at, r) = (ci.at, ci.result);
        let text: Rc<str> = format!("{class}.prototype.{key}").into();
        let a0 = self.arg(&ci, 0);
        let a1 = self.arg(&ci, 1);
        let (e, k, v, this) = (st.elems, st.keys, st.vals, st.recv);
        let new_arr = |s: &mut Solver, tag: u8| {
            let x = s.made(c, tag, Kind::Arr, at);
            (x, s.field(x, ELEM))
        };
        let iter_of = |s: &mut Solver, from: Node| {
            let it = s.made(c, 0, Kind::Iter, at);
            let ie = s.field(it, ELEM);
            s.edge(from, ie);
            s.add(r, it);
        };
        match class {
            "Array" => match key {
                "push" | "unshift" => self.add(r, PRIM),
                "pop" | "shift" | "at" => self.edge(e, r),
                "splice" | "toSpliced" | "with" | "slice" | "toReversed" | "concat" | "flat" => {
                    let (x, xe) = new_arr(self, 0);
                    self.edge(e, xe);
                    match key {
                        "toSpliced" | "with" => {
                            for &n in ci.args.iter().skip(if key == "with" { 1 } else { 2 }) {
                                self.edge(n, xe);
                            }
                        }
                        "concat" => {
                            for &n in &ci.args {
                                self.edge(n, xe);
                                self.constrain(n, Cons::Iterate { dst: xe });
                            }
                        }
                        "flat" => {
                            let inner = self.iterate(e, false, at);
                            self.edge(inner, xe);
                            let deeper = self.iterate(inner, false, at);
                            self.edge(deeper, xe);
                        }
                        _ => {}
                    }
                    self.add(r, x);
                }
                "reverse" | "fill" | "copyWithin" => {}
                "sort" | "toSorted" => {
                    self.invoke(a0, EMPTY, vec![e, e], at, &text);
                    if key == "toSorted" {
                        let (x, xe) = new_arr(self, 0);
                        self.edge(e, xe);
                        self.add(r, x);
                    }
                }
                "map" | "flatMap" | "forEach" | "filter" | "some" | "every" | "find" | "findIndex" | "findLast"
                | "findLastIndex" => {
                    let v = self.invoke(a0, a1, vec![e, PRIMS, this], at, &text);
                    match key {
                        "map" | "flatMap" => {
                            let (x, xe) = new_arr(self, 0);
                            self.edge(v, xe);
                            if key == "flatMap" {
                                let inner = self.iterate(v, false, at);
                                self.edge(inner, xe);
                            }
                            self.add(r, x);
                        }
                        "filter" => {
                            let (x, xe) = new_arr(self, 0);
                            self.edge(e, xe);
                            self.add(r, x);
                        }
                        "find" | "findLast" => self.edge(e, r),
                        _ => self.add(r, PRIM),
                    }
                }
                "reduce" | "reduceRight" => {
                    let acc = self.node();
                    if ci.args.len() > 1 {
                        self.edge(a1, acc);
                    }
                    self.edge(e, acc);
                    let v = self.invoke(a0, EMPTY, vec![acc, e, PRIMS, this], at, &text);
                    self.edge(v, acc);
                    self.edge(acc, r);
                }
                "entries" => {
                    let (x, xe) = new_arr(self, 1);
                    self.edge(e, xe);
                    self.add(xe, PRIM);
                    let xn = self.holding(x);
                    iter_of(self, xn);
                }
                "values" | "@@iterator" => iter_of(self, e),
                "keys" => iter_of(self, PRIMS),
                "includes" | "indexOf" | "lastIndexOf" | "join" | "toString" | "toLocaleString" => self.add(r, PRIM),
                _ => {
                    self.escape_args(&ci, Why::PassedTo(text));
                    self.escape_node(this, Why::PassedTo(format!("Array.prototype.{key}").into()), at);
                    self.add(r, TOP);
                }
            },
            "Map" => match key {
                "get" | "getOrInsert" => self.edge(v, r),
                "getOrInsertComputed" => {
                    let x = self.invoke(a1, EMPTY, vec![a0], at, &text);
                    let vals = self.node();
                    self.edge(x, vals);
                    // (Into every receiver's values.)
                    self.constrain(this, Cons::Store { key: MVAL, src: x, at, check: false, what: text.clone() });
                    self.edge(v, r);
                    self.edge(x, r);
                }
                "keys" => iter_of(self, k),
                "values" => iter_of(self, v),
                "entries" | "@@iterator" => iter_of(self, e),
                "forEach" => {
                    self.invoke(a0, a1, vec![v, k, this], at, &text);
                    self.add(r, PRIM);
                }
                "set" => {}
                _ => self.add(r, PRIM),
            },
            "Set" => match key {
                "keys" | "values" | "@@iterator" => iter_of(self, e),
                "entries" => {
                    let (x, xe) = new_arr(self, 1);
                    self.edge(e, xe);
                    let xn = self.holding(x);
                    iter_of(self, xn);
                }
                "forEach" => {
                    self.invoke(a0, a1, vec![e, e, this], at, &text);
                    self.add(r, PRIM);
                }
                "union" | "intersection" | "difference" | "symmetricDifference" => {
                    let s = self.made(c, 0, Kind::Set, at);
                    let se = self.field(s, ELEM);
                    self.edge(e, se);
                    let other = self.iterate(a0, false, at);
                    self.edge(other, se);
                    self.add(r, s);
                }
                "add" => {}
                _ => self.add(r, PRIM),
            },
            "Promise" => {
                let p = self.made(c, 0, Kind::Promise, at);
                let pe = self.field(p, ELEM);
                match key {
                    "then" => {
                        let x = self.invoke(a0, EMPTY, vec![e], at, &text);
                        let y = self.invoke(a1, EMPTY, vec![CAUGHT], at, &text);
                        self.await_into(x, pe);
                        self.await_into(y, pe);
                    }
                    "catch" => {
                        let y = self.invoke(a0, EMPTY, vec![CAUGHT], at, &text);
                        self.await_into(y, pe);
                    }
                    "finally" => {
                        self.invoke(a0, EMPTY, vec![], at, &text);
                    }
                    _ => {}
                }
                self.edge(e, pe);
                self.add(r, p);
            }
            "String" => match key {
                "split" | "match" | "exec" | "matchAll" | "@@match" | "@@matchAll" | "@@split" => {
                    let arr = self.made(c, 0, Kind::Arr, at);
                    let (ae, af) = (self.field(arr, ELEM), self.field(arr, ANY));
                    self.add(ae, PRIM);
                    self.add(af, PRIM);
                    if key.ends_with("matchAll") {
                        let it = self.made(c, 1, Kind::Iter, at);
                        let ie = self.field(it, ELEM);
                        self.add(ie, arr);
                        self.add(r, it);
                    } else {
                        self.add(r, arr);
                    }
                }
                "replace" | "replaceAll" | "@@replace" => {
                    self.invoke(a1, EMPTY, vec![PRIMS, PRIMS, PRIMS, PRIMS], at, &text);
                    self.add(r, PRIM);
                }
                "@@iterator" => iter_of(self, PRIMS),
                _ => self.add(r, PRIM),
            },
            "Iterator" => match key {
                "next" | "return" | "throw" => {
                    let o = self.made(c, 1, Kind::Obj(None), at);
                    let (vk, dk) = (self.key("value"), self.key("done"));
                    let (vf, df) = (self.field(o, vk), self.field(o, dk));
                    self.edge(e, vf);
                    self.edge(st.rets, vf);
                    self.add(df, PRIM);
                    self.add(r, o);
                }
                "@@iterator" | "@@asyncIterator" => {}
                "toArray" | "map" | "filter" | "take" | "drop" | "flatMap" => {
                    let x = self.made(c, 0, if key == "toArray" { Kind::Arr } else { Kind::Iter }, at);
                    let xe = self.field(x, ELEM);
                    match key {
                        "map" => {
                            let v = self.invoke(a0, EMPTY, vec![e, PRIMS], at, &text);
                            self.edge(v, xe);
                        }
                        "flatMap" => {
                            let v = self.invoke(a0, EMPTY, vec![e, PRIMS], at, &text);
                            let inner = self.iterate(v, false, at);
                            self.edge(inner, xe);
                        }
                        "filter" => {
                            self.invoke(a0, EMPTY, vec![e, PRIMS], at, &text);
                            self.edge(e, xe);
                        }
                        _ => self.edge(e, xe),
                    }
                    self.add(r, x);
                }
                "forEach" | "some" | "every" | "find" => {
                    self.invoke(a0, EMPTY, vec![e, PRIMS], at, &text);
                    if key == "find" { self.edge(e, r) } else { self.add(r, PRIM) }
                }
                "reduce" => {
                    let acc = self.node();
                    self.edge(a1, acc);
                    self.edge(e, acc);
                    let v = self.invoke(a0, EMPTY, vec![acc, e, PRIMS], at, &text);
                    self.edge(v, acc);
                    self.edge(acc, r);
                }
                _ => {
                    self.escape_args(&ci, Why::PassedTo(text));
                    self.escape_node(this, Why::PassedTo("an iterator method".into()), at);
                    self.add(r, TOP);
                }
            },
            "Box" => match key {
                "deref" => self.edge(e, r),
                _ => self.add(r, PRIM),
            },
            "Object" => match key {
                "valueOf" | "__defineGetter__" | "__defineSetter__" => {}
                _ => self.add(r, PRIM),
            },
            "Function" | "Error" => self.add(r, PRIM),
            _ => {
                self.escape_args(&ci, Why::PassedTo(text.clone()));
                self.escape_node(this, Why::PassedTo(text), at);
                self.add(r, TOP);
            }
        }
    }

    fn describe(&self, a: Abs, place: &dyn Fn(u32) -> String) -> String {
        let at = self.ats[a as usize];
        match self.kind(a) {
            Kind::Obj(Some(_)) => format!("the object literal at {}", place(at)),
            Kind::Obj(None) => format!("an object made at {}", place(at)),
            Kind::Arr => format!("an array made at {}", place(at)),
            Kind::Map | Kind::Set => format!("a collection made at {}", place(at)),
            Kind::Promise => format!("a promise made at {}", place(at)),
            Kind::Iter | Kind::Gen(_) => format!("an iterator made at {}", place(at)),
            Kind::Fn(f) => format!("function `{}` ({})", self.funcs[*f as usize].name, place(at)),
            Kind::Class(c) => format!("class `{}` ({})", self.classes[*c as usize].name, place(at)),
            Kind::Proto(c) => format!("`{}.prototype`", self.classes[*c as usize].name),
            Kind::Inst(c) => format!("an instance of `{}`", self.classes[*c as usize].name),
            Kind::Bound { .. } => format!("a bound function made at {}", place(at)),
            _ => "an object".into(),
        }
    }

    fn why_text(&self, why: &Why, place: &dyn Fn(u32) -> String) -> (String, &'static str) {
        let key = |k: &Key| self.keys[*k as usize].clone();
        match why {
            Why::Adds(k) => (format!("adds `{}`", key(k)), "adds a key"),
            Why::Deletes(k) if *k == ANY => ("deletes a computed key".into(), "deletes"),
            Why::Deletes(k) => (format!("deletes `{}`", key(k)), "deletes"),
            Why::WritesComputed => ("writes a computed key".into(), "writes a computed key"),
            Why::Defines(f) => (format!("given to `{f}`"), "defines, freezes or assigns"),
            Why::Accessor(k) => (format!("has an accessor `{}`", key(k)), "literal has an accessor"),
            Why::Spread => ("has a spread".into(), "literal has a spread"),
            Why::ComputedKey => ("has a computed key".into(), "literal has a computed key"),
            Why::SetsProto => ("its prototype is set".into(), "prototype set"),
            Why::PassedTo(t) => (format!("reaches `{t}`, outside the program"), "escapes"),
            Why::Global(g) => (format!("stored in global `{g}`"), "escapes"),
            Why::StoredIn(t) => (format!("stored into `{t}`, made outside the program"), "escapes"),
            Why::Exported => ("exported by the program".into(), "escapes"),
            Why::Returned(f) => {
                (format!("returned by `{}`, which code outside the program calls", self.funcs[*f as usize].name), "escapes")
            }
            Why::HeldBy(a) => (format!("held by {}, which escapes", self.describe(*a, place)), "escapes"),
            Why::Eval => ("visible to `eval`".into(), "escapes"),
        }
    }

    fn report(&self, reads: &[(u32, Node, Key)], native: &[(u32, u32)], place: &dyn Fn(u32) -> String) -> Report {
        let stable = |s: &SiteInfo| !s.is_struct && s.why.is_none();
        let sites = self
            .sites
            .iter()
            .map(|s| Site {
                span: s.span,
                keys: s.keys.iter().map(|&k| self.keys[k as usize].clone()).collect(),
                verdict: if s.is_struct {
                    Verdict::Struct
                } else {
                    match &s.why {
                        None => Verdict::Stable,
                        Some((why, at)) => {
                            let (why, category) = self.why_text(why, place);
                            Verdict::Unstable { why, at: *at, category }
                        }
                    }
                },
            })
            .collect();
        let mut r = Reads::default();
        for &(at, base, key) in reads {
            let pts = &self.pts[base as usize];
            if !pts.iter().any(|&a| self.site_of(a).is_some_and(|s| !self.sites[s as usize].is_struct)) {
                continue;
            }
            let mut slot = None;
            let fixed = pts.iter().all(|&a| {
                let Some(s) = self.site_of(a) else { return false };
                let s = &self.sites[s as usize];
                let Some(i) = s.keys.iter().position(|&k| k == key) else { return false };
                stable(s) && *slot.get_or_insert(i) == i
            });
            let in_native = native.iter().any(|&(s, e)| s <= at && at < e);
            r.literal += 1;
            r.fixed += fixed as usize;
            r.native_literal += in_native as usize;
            r.native_fixed += (in_native && fixed) as usize;
        }
        Report { sites, reads: r }
    }
}

/// A call's arguments from `n` on.
fn shift(ci: &CallInfo, n: usize) -> (Vec<Node>, Option<usize>) {
    match ci.spread {
        Some(s) if s < n => (vec![ci.tail], Some(0)),
        s => (ci.args.iter().skip(n).copied().collect(), s.map(|s| s - n)),
    }
}

impl CallInfo {
    fn new(result: Node, at: u32) -> CallInfo {
        CallInfo {
            this: EMPTY,
            args: Vec::new(),
            spread: None,
            tail: EMPTY,
            result,
            new: false,
            sup: false,
            at,
            text: "".into(),
            plain: 0,
            depth: 0,
            parent: None,
            callee: EMPTY,
        }
    }
}

/// The methods of builtin prototypes (those reads of the program's
/// containers give).
fn methods(class: &str) -> &'static [&'static str] {
    match class {
        "Array" => &[
            "at", "concat", "copyWithin", "entries", "every", "fill", "filter", "find", "findIndex", "findLast",
            "findLastIndex", "flat", "flatMap", "forEach", "includes", "indexOf", "join", "keys", "lastIndexOf", "map",
            "pop", "push", "reduce", "reduceRight", "reverse", "shift", "slice", "some", "sort", "splice",
            "toLocaleString", "toReversed", "toSorted", "toSpliced", "toString", "unshift", "values", "with",
            "@@iterator",
        ],
        "Map" => &[
            "clear", "delete", "entries", "forEach", "get", "has", "keys", "set", "values", "getOrInsert",
            "getOrInsertComputed", "@@iterator",
        ],
        "Set" => &[
            "add", "clear", "delete", "entries", "forEach", "has", "keys", "values", "union", "intersection",
            "difference", "symmetricDifference", "isSubsetOf", "isSupersetOf", "isDisjointFrom", "@@iterator",
        ],
        "Promise" => &["then", "catch", "finally"],
        "Iterator" => &[
            "next", "return", "throw", "toArray", "map", "filter", "take", "drop", "flatMap", "forEach", "some",
            "every", "find", "reduce", "@@iterator", "@@asyncIterator",
        ],
        "Box" => &["deref", "register", "unregister"],
        "Object" => &[
            "hasOwnProperty", "isPrototypeOf", "propertyIsEnumerable", "toString", "toLocaleString", "valueOf",
            "__defineGetter__", "__defineSetter__",
        ],
        _ => &[],
    }
}

/// The builtins that are objects, not functions (`Reflect.apply` is not
/// `Function.prototype.apply`).
fn is_namespace(name: &str) -> bool {
    matches!(name, "Reflect" | "Math" | "JSON" | "console" | "Atomics" | "Intl" | "globalThis" | "performance" | "WebAssembly")
        || name.ends_with(".prototype")
}

/// The engine's globals the analysis models (others are what code outside
/// the program makes).
fn is_global(name: &str) -> bool {
    matches!(
        name,
        "Object" | "Array" | "Math" | "JSON" | "Number" | "String" | "Boolean" | "Symbol" | "BigInt" | "Promise"
            | "Map" | "Set" | "WeakMap" | "WeakSet" | "WeakRef" | "FinalizationRegistry" | "Reflect" | "Proxy"
            | "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError" | "EvalError" | "URIError"
            | "AggregateError" | "Date" | "RegExp" | "console" | "print" | "parseInt" | "parseFloat" | "isNaN"
            | "isFinite" | "encodeURIComponent" | "decodeURIComponent" | "encodeURI" | "decodeURI" | "escape"
            | "unescape" | "ArrayBuffer" | "SharedArrayBuffer" | "DataView" | "Int8Array" | "Uint8Array"
            | "Uint8ClampedArray" | "Int16Array" | "Uint16Array" | "Int32Array" | "Uint32Array" | "Float16Array"
            | "Float32Array" | "Float64Array" | "BigInt64Array" | "BigUint64Array" | "Atomics" | "Intl"
            | "Iterator" | "setTimeout" | "setInterval" | "setImmediate" | "clearTimeout" | "clearInterval"
            | "clearImmediate" | "queueMicrotask" | "structuredClone" | "globalThis" | "performance" | "Function"
            | "eval" | "WebAssembly"
    )
}

/// Before the analysis: `const` bindings of constant keys, the keys the
/// program reads, and those it defines.
struct Prepass<'s> {
    s: &'s mut Solver,
    consts: HashMap<SymbolId, String>,
    loaded: HashSet<Key>,
}

impl<'a> Visit<'a> for Prepass<'_> {
    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        if it.kind == VariableDeclarationKind::Const {
            for d in &it.declarations {
                if let BindingPattern::BindingIdentifier(id) = &d.id
                    && let Some(sym) = id.symbol_id.get()
                    && let Some(init) = &d.init
                    && let Some(k) = literal_key(init)
                {
                    self.consts.insert(sym, k);
                }
            }
        }
        walk::walk_variable_declaration(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
        let k = self.s.key(&it.property.name);
        self.loaded.insert(k);
        walk::walk_static_member_expression(self, it);
    }

    fn visit_assignment_target_property_identifier(&mut self, it: &AssignmentTargetPropertyIdentifier<'a>) {
        let k = self.s.key(&it.binding.name);
        self.loaded.insert(k);
        walk::walk_assignment_target_property_identifier(self, it);
    }

    fn visit_binding_property(&mut self, it: &BindingProperty<'a>) {
        if let Some(name) = it.key.static_name() {
            let k = self.s.key(&name);
            self.loaded.insert(k);
        }
        walk::walk_binding_property(self, it);
    }

    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        if let Some(m) = it.left.as_member_expression()
            && let Some(name) = m.static_property_name()
        {
            let k = self.s.key(name);
            self.s.declared.insert(k);
        }
        walk::walk_assignment_expression(self, it);
    }

    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        if let Some(name) = it.key.static_name() {
            let k = self.s.key(&name);
            self.s.declared.insert(k);
        }
        walk::walk_object_property(self, it);
    }

    fn visit_class_element(&mut self, it: &ClassElement<'a>) {
        if let Some(name) = it.static_name() {
            let k = self.s.key(&name);
            self.s.declared.insert(k);
        }
        if let ClassElement::MethodDefinition(m) = it
            && m.kind == MethodDefinitionKind::Constructor
        {
            for p in &m.value.params.items {
                if p.accessibility.is_some() || p.readonly {
                    if let BindingPattern::BindingIdentifier(id) = &p.pattern {
                        let k = self.s.key(&id.name);
                        self.s.declared.insert(k);
                    }
                }
            }
        }
        walk::walk_class_element(self, it);
    }
}

/// A key a literal expression is (strings, numbers, `Symbol.x`).
fn literal_key(e: &Expression) -> Option<String> {
    match e.get_inner_expression() {
        Expression::StringLiteral(s) => Some(s.value.to_string()),
        Expression::NumericLiteral(n) => number_key(n.value),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() => {
            t.quasis.first().and_then(|q| q.value.cooked.as_ref()).map(|c| c.to_string())
        }
        Expression::StaticMemberExpression(m)
            if matches!(&m.object, Expression::Identifier(id) if id.name == "Symbol") =>
        {
            Some(format!("@@{}", m.property.name))
        }
        _ => None,
    }
}

fn number_key(v: f64) -> Option<String> {
    (v.fract() == 0.0 && v.abs() < 1e15).then(|| format!("{}", v as i64))
}

struct Ctx {
    func: u32,
    this: Node,
    args: Option<Abs>,
    class: Option<u32>,
    is_static: bool,
    scope: Option<ScopeId>,
}

struct Gen<'s> {
    s: &'s mut Solver,
    scoping: &'s Scoping,
    source: &'s str,
    structs: &'s HashSet<u32>,
    syms: HashMap<SymbolId, Node>,
    consts: HashMap<SymbolId, String>,
    ctx: Vec<Ctx>,
    /// Static reads `o.k`: where, `o`'s node, `k`.
    reads: Vec<(u32, Node, Key)>,
    globals: HashMap<String, Node>,
    /// The name a function or class expression takes from where it is
    /// stored (for reasons).
    name_hint: Option<Rc<str>>,
    /// Classes and what they extend: linked once all is generated (a class
    /// is complete then).
    links: Vec<(Node, u32)>,
}


impl<'s> Gen<'s> {
    fn sym(&mut self, sym: SymbolId) -> Node {
        if let Some(&n) = self.syms.get(&sym) {
            return n;
        }
        let n = self.s.node();
        self.syms.insert(sym, n);
        n
    }

    fn global(&mut self, name: &str) -> Node {
        if let Some(&n) = self.globals.get(name) {
            return n;
        }
        let n = if name == "undefined" {
            EMPTY
        } else if is_global(name) || matches!(name, "window" | "self" | "global") {
            let b = self.s.builtin(if is_global(name) { name } else { "globalThis" });
            self.s.holding(b)
        } else {
            TOPS
        };
        self.globals.insert(name.to_string(), n);
        n
    }

    fn cx(&self) -> &Ctx {
        self.ctx.last().unwrap()
    }

    fn text(&self, span: Span) -> Rc<str> {
        let t = &self.source[span.start as usize..span.end as usize];
        let t: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.chars().count() > 50 { format!("{}…", t.chars().take(50).collect::<String>()).into() } else { t.into() }
    }

    fn key(&mut self, s: &str) -> Key {
        self.s.key(s)
    }

    /// A constant key of a computed member or property.
    fn const_key(&mut self, e: &Expression) -> Option<Key> {
        let k = match e.get_inner_expression() {
            Expression::Identifier(id) => {
                let r = id.reference_id.get()?;
                let sym = self.scoping.get_reference(r).symbol_id()?;
                self.consts.get(&sym).cloned()?
            }
            e => literal_key(e)?,
        };
        Some(self.key(&k))
    }

    fn property_key(&mut self, key: &PropertyKey, computed: bool) -> Option<Key> {
        match key {
            PropertyKey::StaticIdentifier(id) => Some(self.key(&id.name)),
            PropertyKey::PrivateIdentifier(id) => Some(self.key(&format!("#{}", id.name))),
            k => {
                let e = k.as_expression()?;
                if computed {
                    self.expr(e);
                }
                self.const_key(e)
            }
        }
    }

    fn union(&mut self, ns: &[Node]) -> Node {
        let n = self.s.node();
        for &m in ns {
            self.s.edge(m, n);
        }
        n
    }

    fn program(&mut self, p: &Program) {
        let f = self.new_func("module code".into(), 0, false, None);
        let args = self.s.funcs[f as usize].args;
        self.ctx.push(Ctx { func: f, this: EMPTY, args: Some(args), class: None, is_static: false, scope: p.scope_id.get() });
        self.stmts(&p.body);
        self.ctx.pop();
    }

    fn new_func(&mut self, name: Rc<str>, at: u32, arrow: bool, this: Option<Node>) -> u32 {
        let f = self.s.funcs.len() as u32;
        let this = this.unwrap_or_else(|| self.s.node());
        let (ret, result, yieldin) = (self.s.node(), self.s.node(), self.s.node());
        let args = self.s.abs(Kind::Arr, at);
        let abs = self.s.abs(Kind::Fn(f), at);
        self.s.funcs.push(Func {
            name,
            at,
            abs,
            params: Vec::new(),
            rest: None,
            this,
            ret,
            args,
            result,
            yieldin,
            yields: EMPTY,
            arrow,
            class: None,
        });
        f
    }

    /// A function (or arrow, or method); returns its id. `class`: the class
    /// whose method it is, and whether static.
    #[allow(clippy::too_many_arguments)]
    fn function(
        &mut self,
        name: Rc<str>,
        span: Span,
        params: &FormalParameters,
        body: Option<&FunctionBody>,
        expr_body: Option<&Expression>,
        arrow: bool,
        is_async: bool,
        generator: bool,
        scope: Option<ScopeId>,
        class: Option<(u32, bool)>,
    ) -> u32 {
        let outer_this = if arrow { Some(self.cx().this) } else { None };
        let f = self.new_func(name, span.start, arrow, outer_this);
        let (this, own_args, ret, result) = {
            let x = &self.s.funcs[f as usize];
            (x.this, x.args, x.ret, x.result)
        };
        if generator {
            let g = self.s.abs(Kind::Gen(f), span.start);
            self.s.add(result, g);
            let e = self.s.field(g, ELEM);
            self.s.funcs[f as usize].yields = e;
        } else if is_async {
            let p = self.s.abs(Kind::Promise, span.start);
            let e = self.s.field(p, ELEM);
            self.s.await_into(ret, e);
            self.s.add(result, p);
        } else {
            self.s.edge(ret, result);
        }
        let (args, cls, is_static) = if arrow {
            (self.cx().args, self.cx().class, self.cx().is_static)
        } else {
            (Some(own_args), class.map(|c| c.0), class.is_some_and(|c| c.1))
        };
        self.ctx.push(Ctx { func: f, this, args, class: cls, is_static, scope });
        let mut ps = Vec::new();
        for _ in &params.items {
            ps.push(self.s.node());
        }
        self.s.funcs[f as usize].params = ps.clone();
        for (p, &n) in params.items.iter().zip(&ps) {
            let v = match &p.initializer {
                Some(init) => {
                    let d = self.expr(init);
                    self.union(&[n, d])
                }
                None => n,
            };
            self.bind(&p.pattern, v);
        }
        if let Some(rest) = &params.rest {
            let r = self.s.abs(Kind::Arr, rest.span.start);
            self.s.funcs[f as usize].rest = Some(r);
            let n = self.s.holding(r);
            self.bind(&rest.rest.argument, n);
        }
        if let Some(b) = body {
            self.stmts(&b.statements);
        }
        if let Some(e) = expr_body {
            let v = self.expr(e);
            self.s.edge(v, ret);
        }
        self.ctx.pop();
        f
    }

    fn function_expr(&mut self, f: &Function, name: Rc<str>, class: Option<(u32, bool)>) -> u32 {
        self.function(
            name,
            f.span,
            &f.params,
            f.body.as_deref(),
            None,
            false,
            f.r#async,
            f.generator,
            f.scope_id.get(),
            class,
        )
    }

    fn stmts(&mut self, ss: &[Statement]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, st: &Statement) {
        match st {
            Statement::BlockStatement(b) => self.stmts(&b.body),
            Statement::ExpressionStatement(e) => {
                self.expr(&e.expression);
            }
            Statement::IfStatement(i) => {
                self.expr(&i.test);
                self.stmt(&i.consequent);
                if let Some(a) = &i.alternate {
                    self.stmt(a);
                }
            }
            Statement::DoWhileStatement(d) => {
                self.stmt(&d.body);
                self.expr(&d.test);
            }
            Statement::WhileStatement(w) => {
                self.expr(&w.test);
                self.stmt(&w.body);
            }
            Statement::ForStatement(f) => {
                match &f.init {
                    Some(ForStatementInit::VariableDeclaration(d)) => self.var_decl(d),
                    Some(e) => {
                        if let Some(e) = e.as_expression() {
                            self.expr(e);
                        }
                    }
                    None => {}
                }
                if let Some(t) = &f.test {
                    self.expr(t);
                }
                if let Some(u) = &f.update {
                    self.expr(u);
                }
                self.stmt(&f.body);
            }
            Statement::ForInStatement(f) => {
                self.expr(&f.right);
                self.for_left(&f.left, PRIMS);
                self.stmt(&f.body);
            }
            Statement::ForOfStatement(f) => {
                let r = self.expr(&f.right);
                let v = self.s.iterate(r, f.r#await, f.span.start);
                self.for_left(&f.left, v);
                self.stmt(&f.body);
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument {
                    let v = self.expr(a);
                    let ret = self.s.funcs[self.cx().func as usize].ret;
                    self.s.edge(v, ret);
                }
            }
            Statement::ThrowStatement(t) => {
                let v = self.expr(&t.argument);
                self.s.edge(v, THROWN);
            }
            Statement::TryStatement(t) => {
                self.stmts(&t.block.body);
                if let Some(h) = &t.handler {
                    if let Some(p) = &h.param {
                        self.bind(&p.pattern, CAUGHT);
                    }
                    self.stmts(&h.body.body);
                }
                if let Some(f) = &t.finalizer {
                    self.stmts(&f.body);
                }
            }
            Statement::SwitchStatement(s) => {
                self.expr(&s.discriminant);
                for c in &s.cases {
                    if let Some(t) = &c.test {
                        self.expr(t);
                    }
                    self.stmts(&c.consequent);
                }
            }
            Statement::LabeledStatement(l) => self.stmt(&l.body),
            Statement::WithStatement(w) => {
                let o = self.expr(&w.object);
                self.s.escape_node(o, Why::Eval, w.span.start);
                self.stmt(&w.body);
            }
            Statement::VariableDeclaration(d) => self.var_decl(d),
            Statement::FunctionDeclaration(f) => self.func_decl(f),
            Statement::ClassDeclaration(c) => {
                self.class(c, None);
            }
            Statement::TSEnumDeclaration(e) => {
                if !e.declare {
                    for m in &e.body.members {
                        if let Some(i) = &m.initializer {
                            self.expr(i);
                        }
                    }
                    if let Some(sym) = e.id.symbol_id.get() {
                        let o = self.s.abs(Kind::Obj(None), e.span.start);
                        let f = self.s.field(o, ANY);
                        self.s.add(f, PRIM);
                        let n = self.sym(sym);
                        self.s.add(n, o);
                    }
                }
            }
            Statement::TSNamespaceDeclaration(n) => self.namespace(n),
            Statement::TSImportEqualsDeclaration(i) => {
                if let Some(sym) = i.id.symbol_id.get() {
                    let n = self.sym(sym);
                    self.s.edge(TOPS, n);
                }
            }
            Statement::TSExportAssignment(e) => {
                let v = self.expr(&e.expression);
                self.s.escape_node(v, Why::Exported, e.span.start);
            }
            Statement::ImportDeclaration(i) => {
                // (Imports of modules the engine provides.)
                for s in i.specifiers.iter().flatten() {
                    if let Some(sym) = s.local().symbol_id.get() {
                        let n = self.sym(sym);
                        self.s.edge(TOPS, n);
                    }
                }
            }
            Statement::ExportNamedDeclaration(e) => {
                for s in &e.specifiers {
                    if let ModuleExportName::IdentifierReference(id) = &s.local {
                        let v = self.ident(id);
                        self.s.escape_node(v, Why::Exported, s.span.start);
                    }
                }
            }
            Statement::ExportDeclaration(e) => {
                let decl = &e.declaration;
                self.stmt_decl(decl);
                let mut ids = Vec::new();
                match decl {
                    Declaration::VariableDeclaration(v) => {
                        for d in &v.declarations {
                            binding_ids(&d.id, &mut ids);
                        }
                    }
                    Declaration::FunctionDeclaration(f) => ids.extend(f.id.as_ref().and_then(|i| i.symbol_id.get())),
                    Declaration::ClassDeclaration(c) => ids.extend(c.id.as_ref().and_then(|i| i.symbol_id.get())),
                    _ => {}
                }
                for sym in ids {
                    let n = self.sym(sym);
                    self.s.escape_node(n, Why::Exported, e.span.start);
                }
            }
            Statement::ExportDefaultDeclaration(e) => {
                let v = match &e.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        let name = f.id.as_ref().map_or("default".into(), |i| i.name.as_str().into());
                        let fid = self.function_expr(f, name, None);
                        let a = self.s.funcs[fid as usize].abs;
                        if let Some(sym) = f.id.as_ref().and_then(|i| i.symbol_id.get()) {
                            let n = self.sym(sym);
                            self.s.add(n, a);
                        }
                        self.s.holding(a)
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(c) => self.class(c, Some("default".into())),
                    ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => EMPTY,
                    k => match k.as_expression() {
                        Some(x) => self.expr(x),
                        None => EMPTY,
                    },
                };
                self.s.escape_node(v, Why::Exported, e.span.start);
            }
            _ => {}
        }
    }

    fn stmt_decl(&mut self, d: &Declaration) {
        match d {
            Declaration::VariableDeclaration(v) => self.var_decl(v),
            Declaration::FunctionDeclaration(f) => self.func_decl(f),
            Declaration::ClassDeclaration(c) => {
                self.class(c, None);
            }
            Declaration::TSNamespaceDeclaration(n) => self.namespace(n),
            _ => {}
        }
    }

    /// A namespace's code; what it exports is a property of an object the
    /// analysis does not follow: it escapes.
    fn namespace(&mut self, n: &TSNamespaceDeclaration) {
        if n.declare {
            return;
        }
        if let Some(sym) = n.id.symbol_id.get() {
            let v = self.sym(sym);
            self.s.edge(TOPS, v);
        }
        match &n.body {
            TSNamespaceDeclarationBody::TSModuleBlock(b) => {
                for st in &b.body {
                    match st {
                        Statement::ExportDeclaration(e) => {
                            self.stmt_decl(&e.declaration);
                            let mut ids = Vec::new();
                            match &e.declaration {
                                Declaration::VariableDeclaration(v) => {
                                    for d in &v.declarations {
                                        binding_ids(&d.id, &mut ids);
                                    }
                                }
                                Declaration::FunctionDeclaration(f) => {
                                    ids.extend(f.id.as_ref().and_then(|i| i.symbol_id.get()))
                                }
                                Declaration::ClassDeclaration(c) => {
                                    ids.extend(c.id.as_ref().and_then(|i| i.symbol_id.get()))
                                }
                                _ => {}
                            }
                            for sym in ids {
                                let v = self.sym(sym);
                                self.s.escape_node(v, Why::Exported, e.span.start);
                            }
                        }
                        st => self.stmt(st),
                    }
                }
            }
            TSNamespaceDeclarationBody::TSNamespaceDeclaration(inner) => self.namespace(inner),
        }
    }

    fn var_decl(&mut self, d: &VariableDeclaration) {
        if d.declare {
            return;
        }
        for x in &d.declarations {
            let v = match &x.init {
                Some(e) => {
                    if let BindingPattern::BindingIdentifier(id) = &x.id {
                        self.name_hint = Some(id.name.as_str().into());
                    }
                    self.expr(e)
                }
                None => EMPTY,
            };
            self.bind(&x.id, v);
        }
    }

    fn func_decl(&mut self, f: &Function) {
        if f.body.is_none() {
            return;
        }
        let name = f.id.as_ref().map_or("an anonymous function".into(), |i| i.name.as_str().into());
        let fid = self.function_expr(f, name, None);
        if let Some(sym) = f.id.as_ref().and_then(|i| i.symbol_id.get()) {
            let a = self.s.funcs[fid as usize].abs;
            let n = self.sym(sym);
            self.s.add(n, a);
        }
    }

    fn for_left(&mut self, left: &ForStatementLeft, v: Node) {
        match left {
            ForStatementLeft::VariableDeclaration(d) => {
                for x in &d.declarations {
                    self.bind(&x.id, v);
                }
            }
            t => {
                if let Some(t) = t.as_assignment_target() {
                    self.assign(t, v);
                }
            }
        }
    }

    /// Bind a pattern's names to what `src` holds (destructured).
    fn bind(&mut self, p: &BindingPattern, src: Node) {
        match p {
            BindingPattern::BindingIdentifier(id) => {
                if let Some(sym) = id.symbol_id.get() {
                    let n = self.sym(sym);
                    self.s.edge(src, n);
                }
            }
            BindingPattern::ObjectPattern(o) => {
                for prop in &o.properties {
                    let k = self.property_key(&prop.key, prop.computed).unwrap_or(ANY);
                    if k != ANY {
                        self.reads.push((prop.span.start, src, k));
                    }
                    let v = self.s.load(src, k);
                    self.bind(&prop.value, v);
                }
                if let Some(r) = &o.rest {
                    let obj = self.s.abs(Kind::Obj(None), r.span.start);
                    let f = self.s.field(obj, ANY);
                    self.s.constrain(src, Cons::Load { dst: f, key: ANY });
                    let n = self.s.holding(obj);
                    self.bind(&r.argument, n);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                let elems = self.s.iterate(src, false, a.span.start);
                for e in a.elements.iter().flatten() {
                    self.bind(e, elems);
                }
                if let Some(r) = &a.rest {
                    let arr = self.s.abs(Kind::Arr, r.span.start);
                    let e = self.s.field(arr, ELEM);
                    self.s.edge(elems, e);
                    let n = self.s.holding(arr);
                    self.bind(&r.argument, n);
                }
            }
            BindingPattern::AssignmentPattern(p) => {
                let d = self.expr(&p.right);
                let u = self.union(&[src, d]);
                self.bind(&p.left, u);
            }
        }
    }

    fn assign_ident(&mut self, id: &IdentifierReference, src: Node) {
        match id.reference_id.get().and_then(|r| self.scoping.get_reference(r).symbol_id()) {
            Some(sym) if !crate::only_ambient(self.scoping, sym) => {
                let n = self.sym(sym);
                self.s.edge(src, n);
            }
            _ => self.s.escape_node(src, Why::Global(id.name.as_str().into()), id.span.start),
        }
    }

    /// The node of a member's object (`super`: what it reads).
    fn object_of(&mut self, e: &Expression) -> Node {
        match e {
            Expression::Super(_) => {
                let Some(c) = self.cx().class else { return EMPTY };
                let k = &self.s.classes[c as usize];
                if self.cx().is_static { k.super_static } else { k.super_proto }
            }
            e => self.expr(e),
        }
    }

    /// The node of a member's object and the member's key.
    fn member_parts(&mut self, m: &MemberExpression) -> (Node, Key) {
        match m {
            MemberExpression::StaticMemberExpression(s) => {
                let base = self.object_of(&s.object);
                (base, self.key(&s.property.name))
            }
            MemberExpression::ComputedMemberExpression(c) => {
                let base = self.object_of(&c.object);
                self.expr(&c.expression);
                (base, self.const_key(&c.expression).unwrap_or(ANY))
            }
            MemberExpression::PrivateFieldExpression(p) => {
                let base = self.object_of(&p.object);
                (base, self.key(&format!("#{}", p.field.name)))
            }
        }
    }

    fn member(&mut self, m: &MemberExpression) -> Node {
        let (base, k) = self.member_parts(m);
        if k != ANY && !matches!(m, MemberExpression::PrivateFieldExpression(_)) {
            self.reads.push((m.span().start, base, k));
        }
        self.s.load(base, k)
    }

    /// `base.k = src` (`super.k = `: `this.k`).
    fn store(&mut self, m: &MemberExpression, src: Node) -> (Node, Key) {
        let (base, k) = match m.object() {
            Expression::Super(_) => {
                let this = self.cx().this;
                let (_, k) = self.member_parts(m);
                (this, k)
            }
            _ => self.member_parts(m),
        };
        let check = !matches!(m, MemberExpression::PrivateFieldExpression(_));
        let what = self.text(m.object().span());
        self.s.constrain(base, Cons::Store { key: k, src, at: m.span().start, check, what });
        (base, k)
    }

    fn simple_target_member<'b>(&self, t: &'b SimpleAssignmentTarget<'b>) -> Option<&'b MemberExpression<'b>> {
        t.as_member_expression()
    }

    fn assign(&mut self, t: &AssignmentTarget, src: Node) {
        match t {
            AssignmentTarget::AssignmentTargetIdentifier(id) => self.assign_ident(id, src),
            AssignmentTarget::TSAsExpression(e) => self.assign_expr(&e.expression, src),
            AssignmentTarget::TSSatisfiesExpression(e) => self.assign_expr(&e.expression, src),
            AssignmentTarget::TSNonNullExpression(e) => self.assign_expr(&e.expression, src),
            AssignmentTarget::TSTypeAssertion(e) => self.assign_expr(&e.expression, src),
            AssignmentTarget::ArrayAssignmentTarget(a) => {
                let elems = self.s.iterate(src, false, a.span.start);
                for e in a.elements.iter().flatten() {
                    self.assign_default(e, elems);
                }
                if let Some(r) = &a.rest {
                    let arr = self.s.abs(Kind::Arr, r.span.start);
                    let e = self.s.field(arr, ELEM);
                    self.s.edge(elems, e);
                    let n = self.s.holding(arr);
                    self.assign(&r.target, n);
                }
            }
            AssignmentTarget::ObjectAssignmentTarget(o) => {
                for p in &o.properties {
                    match p {
                        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(pi) => {
                            let k = self.key(&pi.binding.name);
                            self.reads.push((pi.span.start, src, k));
                            let mut v = self.s.load(src, k);
                            if let Some(init) = &pi.init {
                                let d = self.expr(init);
                                v = self.union(&[v, d]);
                            }
                            self.assign_ident(&pi.binding, v);
                        }
                        AssignmentTargetProperty::AssignmentTargetPropertyProperty(pp) => {
                            let k = self.property_key(&pp.name, pp.computed).unwrap_or(ANY);
                            let v = self.s.load(src, k);
                            self.assign_default(&pp.binding, v);
                        }
                    }
                }
                if let Some(r) = &o.rest {
                    let obj = self.s.abs(Kind::Obj(None), r.span.start);
                    let f = self.s.field(obj, ANY);
                    self.s.constrain(src, Cons::Load { dst: f, key: ANY });
                    let n = self.s.holding(obj);
                    self.assign(&r.target, n);
                }
            }
            t => {
                if let Some(m) = t.as_member_expression() {
                    self.store(m, src);
                }
            }
        }
    }

    fn assign_default(&mut self, t: &AssignmentTargetMaybeDefault, src: Node) {
        match t {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
                let v = self.expr(&d.init);
                let u = self.union(&[src, v]);
                self.assign(&d.binding, u);
            }
            t => {
                if let Some(t) = t.as_assignment_target() {
                    self.assign(t, src);
                }
            }
        }
    }

    /// An assignment to an expression (inside a TypeScript wrapper).
    fn assign_expr(&mut self, e: &Expression, src: Node) {
        match e.get_inner_expression() {
            Expression::Identifier(id) => self.assign_ident(id, src),
            e => {
                if let Some(m) = e.as_member_expression() {
                    self.store(m, src);
                }
            }
        }
    }

    fn assignment(&mut self, a: &AssignmentExpression) -> Node {
        use oxc::ast::ast::AssignmentOperator as Op;
        match a.operator {
            Op::Assign => {
                if matches!(
                    a.right.get_inner_expression(),
                    Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_) | Expression::ClassExpression(_)
                ) {
                    self.name_hint = Some(self.text(a.left.span()));
                }
                let v = self.expr(&a.right);
                self.assign(&a.left, v);
                v
            }
            Op::LogicalOr | Op::LogicalAnd | Op::LogicalNullish => {
                let v = self.expr(&a.right);
                match &a.left {
                    AssignmentTarget::AssignmentTargetIdentifier(id) => {
                        let old = self.ident(id);
                        self.assign_ident(id, v);
                        self.union(&[old, v])
                    }
                    t => match t.as_member_expression() {
                        Some(m) => {
                            let (base, k) = self.store(m, v);
                            let old = self.s.load(base, k);
                            self.union(&[old, v])
                        }
                        None => v,
                    },
                }
            }
            _ => {
                self.expr(&a.right);
                if let Some(m) = a.left.as_member_expression() {
                    self.store(m, EMPTY);
                }
                PRIMS
            }
        }
    }

    fn ident(&mut self, id: &IdentifierReference) -> Node {
        match id.reference_id.get().and_then(|r| self.scoping.get_reference(r).symbol_id()) {
            Some(sym) if !crate::only_ambient(self.scoping, sym) => self.sym(sym),
            _ => {
                if id.name == "arguments"
                    && let Some(a) = self.cx().args
                {
                    return self.s.holding(a);
                }
                self.global(&id.name)
            }
        }
    }

    fn expr(&mut self, e: &Expression) -> Node {
        let hint = self.name_hint.take();
        let at = e.span().start;
        match e {
            Expression::BooleanLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::StringLiteral(_) => PRIMS,
            Expression::NullLiteral(_) => EMPTY,
            Expression::RegExpLiteral(_) => self.s.holding(RE),
            Expression::TemplateLiteral(t) => {
                for x in &t.expressions {
                    self.expr(x);
                }
                PRIMS
            }
            Expression::Identifier(id) => self.ident(id),
            Expression::Super(_) => self.cx().this,
            Expression::ArrayExpression(a) => {
                let arr = self.s.abs(Kind::Arr, at);
                let e = self.s.field(arr, ELEM);
                for el in &a.elements {
                    match el {
                        ArrayExpressionElement::SpreadElement(s) => {
                            let v = self.expr(&s.argument);
                            let it = self.s.iterate(v, false, s.span.start);
                            self.s.edge(it, e);
                        }
                        ArrayExpressionElement::Elision(_) => {}
                        x => {
                            if let Some(x) = x.as_expression() {
                                let v = self.expr(x);
                                self.s.edge(v, e);
                            }
                        }
                    }
                }
                self.s.holding(arr)
            }
            Expression::ArrowFunctionExpression(a) => {
                let name = hint.unwrap_or_else(|| "an arrow function".into());
                let (body, expr_body) = match &a.body {
                    ArrowFunctionBody::FunctionBody(b) => (Some(&**b), None),
                    b => (None, b.as_expression()),
                };
                let f = self.function(name, a.span, &a.params, body, expr_body, true, a.r#async, false, a.scope_id.get(), None);
                let fa = self.s.funcs[f as usize].abs;
                self.s.holding(fa)
            }
            Expression::FunctionExpression(f) => {
                let name = f.id.as_ref().map(|i| i.name.as_str().into()).or(hint).unwrap_or_else(|| "an anonymous function".into());
                let fid = self.function_expr(f, name, None);
                let fa = self.s.funcs[fid as usize].abs;
                if let Some(sym) = f.id.as_ref().and_then(|i| i.symbol_id.get()) {
                    let n = self.sym(sym);
                    self.s.add(n, fa);
                }
                self.s.holding(fa)
            }
            Expression::ClassExpression(c) => self.class(c, hint),
            Expression::AssignmentExpression(a) => self.assignment(a),
            Expression::AwaitExpression(a) => {
                let v = self.expr(&a.argument);
                self.s.await_(v)
            }
            Expression::BinaryExpression(b) => {
                self.expr(&b.left);
                self.expr(&b.right);
                PRIMS
            }
            Expression::PrivateInExpression(p) => {
                self.expr(&p.right);
                PRIMS
            }
            Expression::CallExpression(c) => self.call(c),
            Expression::ChainExpression(c) => match &c.expression {
                ChainElement::CallExpression(call) => self.call(call),
                ChainElement::TSNonNullExpression(t) => self.expr(&t.expression),
                m => match m.as_member_expression() {
                    Some(m) => self.member(m),
                    None => EMPTY,
                },
            },
            Expression::ConditionalExpression(c) => {
                self.expr(&c.test);
                self.name_hint = hint.clone();
                let a = self.expr(&c.consequent);
                self.name_hint = hint;
                let b = self.expr(&c.alternate);
                self.union(&[a, b])
            }
            Expression::LogicalExpression(l) => {
                let a = self.expr(&l.left);
                self.name_hint = hint;
                let b = self.expr(&l.right);
                self.union(&[a, b])
            }
            Expression::ImportExpression(i) => {
                self.expr(&i.source);
                if let Some(o) = &i.options {
                    self.expr(o);
                }
                TOPS
            }
            Expression::NewExpression(n) => {
                let callee = self.expr(&n.callee);
                let (args, spread, tail, plain) = self.args(&n.arguments);
                let result = self.s.node();
                let text = self.text(n.callee.span());
                let c = self.s.call_info(CallInfo { args, spread, tail, plain, text, new: true, ..CallInfo::new(result, at) });
                self.s.constrain(callee, Cons::Call(c));
                result
            }
            Expression::ObjectExpression(o) => self.object(o),
            Expression::ParenthesizedExpression(p) => {
                self.name_hint = hint;
                self.expr(&p.expression)
            }
            Expression::SequenceExpression(s) => {
                let mut last = EMPTY;
                for x in &s.expressions {
                    last = self.expr(x);
                }
                last
            }
            Expression::TaggedTemplateExpression(t) => {
                let tag = self.expr(&t.tag);
                let mut args = vec![TOPS];
                for x in &t.quasi.expressions {
                    args.push(self.expr(x));
                }
                let text = self.text(t.tag.span());
                self.s.invoke(tag, EMPTY, args, at, &text)
            }
            Expression::ThisExpression(_) => self.cx().this,
            Expression::UnaryExpression(u) => {
                if u.operator == UnaryOperator::Delete {
                    match u.argument.get_inner_expression().as_member_expression() {
                        Some(m) => {
                            let (base, k) = self.member_parts(m);
                            self.s.constrain(base, Cons::Delete { key: k, at });
                        }
                        None => {
                            self.expr(&u.argument);
                        }
                    }
                } else {
                    self.expr(&u.argument);
                }
                PRIMS
            }
            Expression::UpdateExpression(u) => {
                if let Some(m) = self.simple_target_member(&u.argument) {
                    self.store(m, EMPTY);
                }
                PRIMS
            }
            Expression::YieldExpression(y) => {
                let f = self.cx().func;
                let (yields, yieldin) = (self.s.funcs[f as usize].yields, self.s.funcs[f as usize].yieldin);
                if let Some(a) = &y.argument {
                    let v = self.expr(a);
                    if y.delegate {
                        let it = self.s.iterate(v, false, at);
                        self.s.edge(it, yields);
                    } else {
                        self.s.edge(v, yields);
                    }
                }
                self.s.edge(TOPS, yieldin);
                yieldin
            }
            Expression::ImportMeta(_) | Expression::NewTarget(_) => TOPS,
            Expression::JSXElement(_) | Expression::JSXFragment(_) => TOPS,
            Expression::TSAsExpression(t) => {
                self.name_hint = hint;
                self.expr(&t.expression)
            }
            Expression::TSSatisfiesExpression(t) => {
                self.name_hint = hint;
                self.expr(&t.expression)
            }
            Expression::TSTypeAssertion(t) => {
                self.name_hint = hint;
                self.expr(&t.expression)
            }
            Expression::TSNonNullExpression(t) => {
                self.name_hint = hint;
                self.expr(&t.expression)
            }
            Expression::TSInstantiationExpression(t) => self.expr(&t.expression),
            Expression::V8IntrinsicExpression(v) => {
                for a in &v.arguments {
                    if let Some(a) = a.as_expression() {
                        let n = self.expr(a);
                        self.s.escape_node(n, Why::PassedTo("an intrinsic".into()), at);
                    }
                }
                TOPS
            }
            e => match e.as_member_expression() {
                Some(m) => self.member(m),
                None => EMPTY,
            },
        }
    }

    /// A call's arguments: their nodes, the first spread's index, all from
    /// it on, and which are `null`, `undefined` or array literals.
    fn args(&mut self, args: &[Argument]) -> (Vec<Node>, Option<usize>, Node, u64) {
        let (mut nodes, mut spread, mut plain) = (Vec::new(), None, 0u64);
        for (i, a) in args.iter().enumerate() {
            match a {
                Argument::SpreadElement(s) => {
                    let v = self.expr(&s.argument);
                    let it = self.s.iterate(v, false, s.span.start);
                    spread.get_or_insert(i);
                    nodes.push(it);
                }
                a => {
                    let Some(x) = a.as_expression() else { continue };
                    if i < 64
                        && (x.is_null()
                            || matches!(x, Expression::ArrayExpression(_))
                            || matches!(x, Expression::Identifier(id) if id.name == "undefined"))
                    {
                        plain |= 1 << i;
                    }
                    let n = self.expr(x);
                    nodes.push(n);
                }
            }
        }
        let tail = match spread {
            Some(s) => {
                let t = nodes[s..].to_vec();
                self.union(&t)
            }
            None => EMPTY,
        };
        (nodes, spread, tail, plain)
    }

    fn call(&mut self, c: &CallExpression) -> Node {
        let at = c.span.start;
        let text = self.text(c.callee.span());
        let callee = c.callee.get_inner_expression();
        // (Its object before its arguments, as they run.)
        let (recv, fnode) = match callee {
            Expression::Super(_) => (EMPTY, EMPTY),
            Expression::Identifier(id)
                if id.name == "eval"
                    && id.reference_id.get().and_then(|r| self.scoping.get_reference(r).symbol_id()).is_none() =>
            {
                (EMPTY, EMPTY)
            }
            e => match e.as_member_expression() {
                Some(m) => {
                    let (base, k) = self.member_parts(m);
                    if k != ANY && !matches!(m, MemberExpression::PrivateFieldExpression(_)) {
                        self.reads.push((m.span().start, base, k));
                    }
                    let f = self.s.load(base, k);
                    let recv = if matches!(m.object(), Expression::Super(_)) { self.cx().this } else { base };
                    (recv, f)
                }
                None => (EMPTY, self.expr(e)),
            },
        };
        let (args, spread, tail, plain) = self.args(&c.arguments);
        let result = self.s.node();
        match callee {
            Expression::Super(_) => {
                if let Some(k) = self.cx().class {
                    let sup = self.s.classes[k as usize].super_ctor;
                    let this = self.cx().this;
                    let ci = CallInfo { this, args, spread, tail, plain, text, sup: true, ..CallInfo::new(EMPTY, at) };
                    let ci = self.s.call_info(ci);
                    self.s.constrain(sup, Cons::Call(ci));
                }
                return self.cx().this;
            }
            Expression::Identifier(id) if id.name == "eval" && fnode == EMPTY => {
                self.eval(at);
                for &a in &args {
                    self.s.escape_node(a, Why::Eval, at);
                }
                self.s.add(result, TOP);
                return result;
            }
            _ => {}
        }
        let ci = self.s.call_info(CallInfo { this: recv, args, spread, tail, plain, text, ..CallInfo::new(result, at) });
        self.s.constrain(fnode, Cons::Call(ci));
        result
    }

    /// A direct `eval`: the bindings it can see escape (and can be set to
    /// anything).
    fn eval(&mut self, at: u32) {
        let Some(scope) = self.cx().scope else { return };
        let mut syms = Vec::new();
        for (s, bindings) in self.scoping.iter_bindings() {
            if self.scoping.scope_is_descendant_of(scope, s) || self.scoping.scope_is_descendant_of(s, scope) {
                syms.extend(bindings.values().copied());
            }
        }
        for sym in syms {
            let n = self.sym(sym);
            self.s.escape_node(n, Why::Eval, at);
            self.s.edge(TOPS, n);
        }
    }

    fn object(&mut self, o: &ObjectExpression) -> Node {
        let at = o.span.start;
        let site = self.s.sites.len() as u32;
        let is_struct = self.structs.contains(&at);
        self.s.sites.push(SiteInfo { span: (at, o.span.end), keys: Vec::new(), is_struct, why: None });
        let obj = self.s.abs(Kind::Obj(Some(site)), at);
        let node = self.s.holding(obj);
        let mut keys = Vec::new();
        for p in &o.properties {
            match p {
                ObjectPropertyKind::SpreadProperty(s) => {
                    let v = self.expr(&s.argument);
                    let any = self.s.field(obj, ANY);
                    self.s.constrain(v, Cons::Load { dst: any, key: ANY });
                    self.s.unstable(site, Why::Spread, s.span.start);
                }
                ObjectPropertyKind::ObjectProperty(p) => {
                    let k = match self.property_key(&p.key, p.computed) {
                        Some(k) => k,
                        None => {
                            self.s.unstable(site, Why::ComputedKey, p.span.start);
                            ANY
                        }
                    };
                    let name: Rc<str> = self.s.keys[k as usize].as_str().into();
                    let proto = !p.computed && !p.shorthand && !p.method && p.kind == PropertyKind::Init && &*name == "__proto__";
                    if k != ANY && !proto && !keys.contains(&k) {
                        keys.push(k);
                    }
                    let func = match &p.value {
                        Expression::FunctionExpression(f) => Some(self.function_expr(f, name.clone(), None)),
                        _ => None,
                    };
                    if let Some(f) = func {
                        let this = self.s.funcs[f as usize].this;
                        self.s.add(this, obj);
                    }
                    match p.kind {
                        PropertyKind::Init => {
                            let v = match func {
                                Some(f) => {
                                    let fa = self.s.funcs[f as usize].abs;
                                    self.s.holding(fa)
                                }
                                None => {
                                    self.name_hint = Some(name);
                                    self.expr(&p.value)
                                }
                            };
                            if proto {
                                self.s.constrain(v, Cons::Mirror { dst: obj });
                                self.s.unstable(site, Why::SetsProto, p.span.start);
                            } else {
                                let f = self.s.field(obj, k);
                                self.s.edge(v, f);
                            }
                        }
                        PropertyKind::Get | PropertyKind::Set => {
                            self.s.unstable(site, Why::Accessor(k), p.span.start);
                            let Some(f) = func else { continue };
                            let fl = self.s.field(obj, k);
                            if p.kind == PropertyKind::Get {
                                let ret = self.s.funcs[f as usize].ret;
                                self.s.edge(ret, fl);
                            } else if let Some(&p0) = self.s.funcs[f as usize].params.first() {
                                self.s.edge(fl, p0);
                            }
                        }
                    }
                }
            }
        }
        self.s.sites[site as usize].keys = keys;
        node
    }

    fn class(&mut self, c: &Class, hint: Option<Rc<str>>) -> Node {
        let name: Rc<str> =
            c.id.as_ref().map(|i| i.name.as_str().into()).or(hint).unwrap_or_else(|| "an anonymous class".into());
        let k = self.s.new_class(name.clone(), c.span.start, None);
        let (value, proto) = (self.s.classes[k as usize].value, self.s.classes[k as usize].proto);
        let vnode = self.s.holding(value);
        if let Some(sym) = c.id.as_ref().and_then(|i| i.symbol_id.get()) {
            let n = self.sym(sym);
            self.s.edge(vnode, n);
        }
        if let Some(h) = &c.heritage {
            let hn = self.expr(&h.expression);
            let sup = self.s.classes[k as usize].super_ctor;
            self.s.edge(hn, sup);
            self.links.push((hn, k));
        }
        let init = self.new_func(format!("the field initializers of `{name}`").into(), c.span.start, false, None);
        let (finit, sinit) = (self.s.funcs[init as usize].this, self.s.node());
        self.s.classes[k as usize].this_inst.push(finit);
        self.s.classes[k as usize].this_static.push(sinit);
        for el in &c.body.body {
            match el {
                ClassElement::MethodDefinition(m) => {
                    let key = self.property_key(&m.key, m.computed).unwrap_or(ANY);
                    let mname: Rc<str> = format!("{name}.{}", self.s.keys[key as usize]).into();
                    let f = self.function_expr(&m.value, mname, Some((k, m.r#static)));
                    let (fa, this) = (self.s.funcs[f as usize].abs, self.s.funcs[f as usize].this);
                    let holder = if m.r#static { value } else { proto };
                    if m.r#static {
                        self.s.classes[k as usize].this_static.push(this);
                    } else {
                        self.s.classes[k as usize].this_inst.push(this);
                    }
                    match m.kind {
                        MethodDefinitionKind::Constructor => {
                            self.s.classes[k as usize].ctor = Some(f);
                            // (Parameter properties.)
                            for (i, p) in m.value.params.items.iter().enumerate() {
                                if (p.accessibility.is_some() || p.readonly || p.r#override)
                                    && let BindingPattern::BindingIdentifier(id) = &p.pattern
                                {
                                    let key = self.key(&id.name);
                                    let src = self.s.funcs[f as usize].params[i];
                                    let what = "this".into();
                                    self.s.constrain(this, Cons::Store { key, src, at: p.span.start, check: false, what });
                                }
                            }
                        }
                        MethodDefinitionKind::Method => {
                            let fl = self.s.field(holder, key);
                            self.s.add(fl, fa);
                        }
                        MethodDefinitionKind::Get => {
                            let ret = self.s.funcs[f as usize].ret;
                            let fl = self.s.field(holder, key);
                            self.s.edge(ret, fl);
                        }
                        MethodDefinitionKind::Set => {
                            if let Some(&p0) = self.s.funcs[f as usize].params.first() {
                                let k = &mut self.s.classes[k as usize];
                                if m.r#static { k.static_setters.push((key, p0)) } else { k.setters.push((key, p0)) }
                            }
                        }
                    }
                }
                ClassElement::PropertyDefinition(p) => {
                    let key = self.property_key(&p.key, p.computed).unwrap_or(ANY);
                    if let Some(v) = &p.value {
                        self.field_init(k, init, p.r#static, key, v, p.span.start, finit, sinit);
                    }
                }
                ClassElement::AccessorProperty(p) => {
                    let key = self.property_key(&p.key, p.computed).unwrap_or(ANY);
                    if let Some(v) = &p.value {
                        self.field_init(k, init, p.r#static, key, v, p.span.start, finit, sinit);
                    }
                }
                ClassElement::StaticBlock(b) => {
                    self.ctx.push(Ctx { func: init, this: sinit, args: None, class: Some(k), is_static: true, scope: None });
                    self.stmts(&b.body);
                    self.ctx.pop();
                }
                ClassElement::TSIndexSignature(_) => {}
            }
        }
        self.s.inherit(k, k);
        vnode
    }

    #[allow(clippy::too_many_arguments)]
    fn field_init(&mut self, k: u32, init: u32, is_static: bool, key: Key, v: &Expression, at: u32, finit: Node, sinit: Node) {
        let this = if is_static { sinit } else { finit };
        self.ctx.push(Ctx { func: init, this, args: None, class: Some(k), is_static, scope: None });
        self.name_hint = Some(self.s.keys[key as usize].as_str().into());
        let n = self.expr(v);
        self.ctx.pop();
        let what = "this".into();
        self.s.constrain(this, Cons::Store { key, src: n, at, check: false, what });
    }
}

/// The bindings a pattern declares.
fn binding_ids(p: &BindingPattern, out: &mut Vec<SymbolId>) {
    match p {
        BindingPattern::BindingIdentifier(id) => out.extend(id.symbol_id.get()),
        BindingPattern::ObjectPattern(o) => {
            for prop in &o.properties {
                binding_ids(&prop.value, out);
            }
            if let Some(r) = &o.rest {
                binding_ids(&r.argument, out);
            }
        }
        BindingPattern::ArrayPattern(a) => {
            for e in a.elements.iter().flatten() {
                binding_ids(e, out);
            }
            if let Some(r) = &a.rest {
                binding_ids(&r.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(a) => binding_ids(&a.left, out),
    }
}
