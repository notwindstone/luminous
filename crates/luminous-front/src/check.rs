//! Checker: decides which module-level functions can be compiled natively and
//! lowers them to the typed IR.
//!
//! TypeScript types are unsound at runtime, so they are only trusted where
//! they are re-checked: `number` / `boolean` parameters become unboxed
//! values, guarded by the function's JS entry (which falls back to the
//! interpreter on a mismatch). Everything else is an `Any` value with exact JS
//! semantics. A function stays interpreted only if it uses a construct this
//! checker does not lower yet.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use oxc::ast::ast::*;
use oxc::semantic::{Scoping, SymbolId};
use oxc::span::{GetSpan, Span};

use crate::ir::{
    self, AnyBinOp, AnyCmpOp, AnyUnOp, ArrKind, BinOp, CmpOp, Expr, ExprKind, Key, LogicalOp, MathFn, Stmt, Ty, UnOp,
};

/// Why a function stays interpreted.
#[derive(Debug, Clone)]
pub struct Reject {
    pub span: Span,
    pub msg: String,
    /// Internal: a local was widened to `Any`; check the function again.
    retry: bool,
    /// It calls a function that turned out not to be native (once that one
    /// is interpreted, the call goes through its binding instead).
    callee: bool,
}

type R<T> = Result<T, Reject>;

fn reject<T>(span: Span, msg: impl Into<String>) -> R<T> {
    Err(Reject { span, msg: msg.into(), retry: false, callee: false })
}

const LONE_SURROGATES: &str = "string with a lone surrogate (the interpreter keeps it exact)";

/// Native code keeps literal strings as UTF-8, which cannot hold a lone
/// surrogate (`"\uD800"`; oxc marks such a literal's value as escaped).
fn lone_surrogates(s: &StringLiteral) -> R<()> {
    if s.lone_surrogates { reject(s.span, LONE_SURROGATES) } else { Ok(()) }
}

fn callee_failed<T>(span: Span, name: &str) -> R<T> {
    let msg = format!("calls `{name}`, which is not compiled natively");
    Err(Reject { span, msg, retry: false, callee: true })
}

/// The syntax of a candidate function.
#[derive(Clone, Copy)]
pub enum FnNode<'a> {
    Function(&'a Function<'a>),
    Arrow(&'a ArrowFunctionExpression<'a>),
    /// Module-level statements, compiled as a function without parameters
    /// (spanning them).
    Segment(Span, &'a [Statement<'a>]),
}

impl FnNode<'_> {
    pub fn is_async(&self) -> bool {
        match self {
            FnNode::Function(f) => f.r#async,
            FnNode::Arrow(a) => a.r#async,
            FnNode::Segment(..) => false,
        }
    }

    pub fn span(&self) -> Span {
        match self {
            FnNode::Function(f) => f.span,
            FnNode::Arrow(a) => a.span,
            FnNode::Segment(span, _) => *span,
        }
    }

    pub fn is_generator(&self) -> bool {
        matches!(self, FnNode::Function(f) if f.generator)
    }
}

/// Where a candidate is bound, which decides how the source is rewritten.
#[derive(Clone)]
pub enum Site {
    /// `function f() {}`; `export_prefix` spans `export ` if present.
    Declaration { id: Span, export_prefix: Option<Span> },
    /// `const f = <init>` where `init` is an arrow or function expression.
    ConstInit { init: Span },
    /// A method of top-level class `class`, installed over the original after
    /// the class declaration (which ends at `stmt_end`).
    /// `accessor`: 0 for a method, 1 for a getter, 2 for a setter.
    Method { class: String, is_static: bool, key: String, stmt_end: u32, accessor: u8 },
    /// Module-level statements, replaced by a call of the native code.
    Segment { span: Span },
    /// The constructor of top-level class `class`, which native `new`
    /// expressions call directly (interpreted code keeps the class's own).
    Ctor { class: String },
    /// A private instance method of top-level class `class`, which native
    /// `this.#m(...)` calls directly after the brand check (interpreted
    /// code keeps the class's own: a private method cannot be replaced).
    Private { class: String },
}

#[derive(Clone)]
struct Param<'a> {
    /// The binding (an identifier, or a pattern destructuring the argument).
    pattern: &'a BindingPattern<'a>,
    /// ABI type: unboxed (and guarded by the JS entry) only for `number` and
    /// `boolean` annotations.
    ty: Ty,
    default: Option<&'a Expression<'a>>,
}

/// A module-level function binding that might be compiled natively.
pub struct Candidate<'a> {
    pub name: String,
    pub node: FnNode<'a>,
    /// The binding (none for methods).
    pub symbol: Option<SymbolId>,
    /// Methods: `this` is the receiver.
    pub has_this: bool,
    pub site: Site,
    /// Whether other native functions may call this one directly. False for
    /// `const` bindings that may still be in their TDZ when native code runs.
    direct_callable: bool,
    params: Vec<Param<'a>>,
    /// The last parameter is a rest parameter.
    rest: bool,
    /// Annotated return type, if any (only a hint for callers).
    ret: Option<Ty>,
    /// Methods: their class (index in `Outcome::classes`), and whether
    /// they are static.
    class: Option<(usize, bool)>,
    /// Segments: the module bindings they declare that other code uses,
    /// which live in cells (the others are locals).
    cells: HashSet<SymbolId>,
    /// Native constructors: what the instance is made with.
    ctor: Option<CtorInfo<'a>>,
}

/// A class constructor compiled natively (see `class_ctor`): a base
/// class's makes the instance, with the class's fields defined in order to
/// their initializers' values (its parameter properties first, then
/// assigned from the parameters), then runs the body with `this` the
/// instance; a derived class's gets the instance from its superclass's at
/// its `super(...)`, then defines its own fields.
#[derive(Clone)]
pub struct CtorInfo<'a> {
    /// The class's binding (top-level classes).
    pub symbol: Option<SymbolId>,
    /// The class (its declaration).
    pub span: Span,
    /// Index in `Outcome::classes`.
    pub class: usize,
    /// The class of the instance made (`new.target`): `class`, or a
    /// subclass whose constructor reaches this one through `super(...)`
    /// (each such pair is a function of its own).
    pub home: usize,
    pub fields: Vec<(String, Option<&'a Expression<'a>>)>,
    /// Parameter properties (`constructor(public x: number)`): the field,
    /// the parameter.
    pub param_props: Vec<(String, SymbolId)>,
    /// A derived class: its superclass.
    pub parent: Option<usize>,
    /// A derived class whose superclass (not one of these classes, as
    /// `Error`) has no native constructor: `super(...)` constructs it as
    /// interpreted code would (`ir::ExprKind::SuperNew`).
    pub dynamic: bool,
    /// A derived class without a constructor: its parameters are its
    /// superclass's (without their defaults), passed on (`dynamic`: all
    /// its arguments, its only parameter an array of them).
    pub forward: bool,
    /// The constructor `new` calls (`home == class`) of a derived class:
    /// the classes whose superclass must still be the one their
    /// declaration names (each `(class, superclass)`), else the class is
    /// constructed as interpreted code would.
    pub checks: Vec<(usize, usize)>,
    /// (`home == class`) the fields of the instances, in the order they
    /// are defined: its superclasses' first (public ones).
    pub instance_fields: Vec<String>,
    /// Its instances get its brand (it has private methods or accessors).
    pub brand: bool,
    /// Its parameters' defaults may run code (before `super(...)`, after
    /// the checks of `checks`): the classes it is made by, from it up, to
    /// check again at `super(...)` (else the superclass is constructed as
    /// interpreted code would).
    pub recheck: Vec<(usize, usize)>,
    /// A class inside a function (`FnCx::class_value`): its constructor is
    /// a closure that `new` calls (`ir::Func::class_ctor`), the instance
    /// made from new.target's prototype (`class`, `home` unused), its
    /// superclass, if it has one, constructed as interpreted code does
    /// (`dynamic`).
    pub local: bool,
}

impl Candidate<'_> {
    /// Native constructors: the span of their class.
    pub fn ctor_class(&self) -> Option<Span> {
        self.ctor.as_ref().map(|c| c.span)
    }
}

#[derive(Clone, Copy)]
enum ConstVal {
    Num(f64),
    Bool(bool),
}

/// How a module-level binding is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Function,
    Const,
    Let,
    Var,
    Class,
    Import,
}

/// A module-level binding that native code may access through a cell.
#[derive(Clone, Debug)]
pub struct Binding {
    pub kind: BindingKind,
    pub name: String,
    /// Assigned anywhere after its declaration.
    pub written: bool,
    pub exported: bool,
    /// Span of the initializer of a single-identifier declarator.
    pub init: Option<Span>,
    /// End of the declaring statement.
    pub stmt_end: u32,
}

/// When a cell gets its value, for source rewriting.
#[derive(Clone, Copy, Debug)]
pub enum CellInit {
    /// In the module header (hoisted functions, imports, native functions).
    Header,
    /// By wrapping the declarator's initializer: `x = __lum_init(k, init)`.
    Wrap(Span),
    /// After the declaring statement.
    After(u32),
    /// By native code: the binding is declared by a native segment.
    Native,
}

#[derive(Clone, Debug)]
pub struct CellInfo {
    pub symbol: SymbolId,
    pub name: String,
    /// Written after initialization: interpreted code must access the cell
    /// itself instead of its own binding.
    pub mutable: bool,
    pub init: CellInit,
    /// `var`: `undefined` from the module header until its declaration runs.
    pub hoisted_undefined: bool,
    /// `const` (or an import): assigning to it throws a TypeError.
    pub constant: bool,
}

pub struct Outcome<'a> {
    /// Native functions, in source order; index == `ir::FuncId`.
    pub native: Vec<Candidate<'a>>,
    pub module: ir::Module,
    pub cells: Vec<CellInfo>,
    /// Spans of the nested functions compiled as closures.
    pub closure_spans: Vec<Span>,
    /// Rejected candidates with the reason they stay interpreted.
    pub rejected: Vec<(String, Span, Reject)>,
    /// Top-level classes, and those that native code needs data from.
    pub classes: Vec<ClassInfo>,
    pub class_uses: Vec<ClassUse>,
}

/// `interpreted`: start offsets of the functions marked `interpreted`.
pub fn check<'a>(
    program: &'a Program<'a>,
    scoping: &'a Scoping,
    int_syms: &HashSet<SymbolId>,
    interpreted: &HashSet<u32>,
    structs: &crate::structs::Structs,
) -> Outcome<'a> {
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    let mut consts = HashMap::new();
    let mut bindings = HashMap::new();
    let mut classes = Vec::new();
    let mut aliases = HashMap::new();
    collect_toplevel(
        program,
        scoping,
        interpreted,
        &mut classes,
        &mut candidates,
        &mut rejected,
        &mut consts,
        &mut bindings,
        &mut aliases,
    );
    let records = collect_records(program);
    RECORD_NAMES.with(|r| *r.borrow_mut() = records.iter().map(|r| r.name.clone()).collect());
    // Module code, in runs of statements.
    let uses = module_uses(program, scoping);
    for run in segment_runs(&program.body, &bindings, &consts) {
        candidates.push(segment(run, scoping, &uses));
    }

    if scoping.root_unresolved_references().contains_key("eval") {
        // Direct eval can read and rebind module-level bindings, so nothing
        // about this module can be assumed statically.
        for c in candidates.drain(..) {
            let span = c.node.span();
            rejected.push((
                c.name,
                span,
                Reject { span, msg: "module uses `eval`".into(), retry: false, callee: false },
            ));
        }
    }

    // Keyed by the function's position.
    let mut widened: HashMap<u32, HashSet<SymbolId>> = HashMap::new();
    let mut ret_override: HashMap<u32, Ty> = HashMap::new();
    // Functions without a return type that call themselves: they return any
    // value (see `ModuleCx::signature`).
    let mut any_ret: HashSet<u32> = HashSet::new();
    // Fixpoint: a function is native only if all its native callees are, and
    // callers must agree with the callees' actual return types.
    loop {
        let mut cx = ModuleCx {
            scoping,
            int_syms,
            interpreted,
            consts: &consts,
            bindings: &bindings,
            aliases: &aliases,
            cands: &candidates,
            state: vec![State::Todo; candidates.len()],
            widened: &mut widened,
            ret_override: &mut ret_override,
            any_ret: &mut any_ret,
            arguments: HashMap::new(),
            ret_changed: false,
            strings: Vec::new(),
            string_ids: HashMap::new(),
            atoms: Vec::new(),
            atom_ids: HashMap::new(),
            cells: Vec::new(),
            cell_ids: HashMap::new(),
            closures: Vec::new(),
            closure_spans: Vec::new(),
            classes: &classes,
            privates: Vec::new(),
            private_ids: HashMap::new(),
            homes_used: Vec::new(),
            layouts: &structs.layouts,
            struct_sites: &structs.sites,
            array_sites: &structs.array_sites,
            parse_sites: &structs.parse_sites,
            json_sites: &structs.json_sites,
            sure_arrays: &structs.sure_arrays,
        };
        for i in 0..candidates.len() {
            cx.ensure(i);
        }
        if cx.ret_changed {
            continue;
        }
        let failed: Vec<usize> = (0..candidates.len()).filter(|&i| matches!(cx.state[i], State::Failed(_))).collect();
        if failed.is_empty() {
            let funcs = std::mem::take(&mut cx.state)
                .into_iter()
                .map(|s| match s {
                    State::Done(f) => *f,
                    _ => unreachable!(),
                })
                .collect();
            let cells = cx.cell_infos();
            let class_uses = cx.class_uses();
            let records = records
                .iter()
                .map(|r| ir::Record {
                    fields: r.fields.iter().map(|n| cx.atom(n)).collect(),
                    hints: r.hints.iter().map(|h| h.as_ref().and_then(|h| resolve_hint(h, &classes))).collect(),
                    nums: r.nums.clone(),
                })
                .collect();
            let mut module = ir::Module {
                funcs,
                closures: std::mem::take(&mut cx.closures),
                strings: cx.strings,
                atoms: cx.atoms,
                cells: cells.iter().map(|c| c.name.clone()).collect(),
                privates: cx.privates.len(),
                homes: 2 * classes.len(),
                classes: class_tables(&classes, &candidates),
                layouts: structs.layouts.clone(),
                int_wrapping: true,
                records,
                class_records: Vec::new(),
            };
            ir::capture_values(&mut module);
            ir::infer_hints(&mut module);
            ir::class_records(&mut module);
            ir::specialize_fields(&mut module);
            ir::fold_new_instances(&mut module);
            ir::inline_calls(&mut module);
            ir::fold_new_instances(&mut module);
            ir::predict_calls(&mut module);
            ir::inline_callbacks(&mut module);
            ir::virtualize_instances(&mut module);
            crate::spec::speculate_numbers(&mut module);
            let closure_spans = std::mem::take(&mut cx.closure_spans);
            return Outcome { native: candidates, module, cells, closure_spans, rejected, classes, class_uses };
        }
        let mut reasons: HashMap<usize, Reject> = HashMap::new();
        for &i in &failed {
            if let State::Failed(r) = &cx.state[i] {
                reasons.insert(i, r.clone());
            }
        }
        // Functions that failed only because a callee did are checked again
        // once their failing callees are interpreted (and called through
        // their bindings).
        if reasons.values().any(|r| !r.callee) {
            reasons.retain(|_, r| !r.callee);
        }
        let mut keep = Vec::new();
        for (i, c) in candidates.into_iter().enumerate() {
            match reasons.remove(&i) {
                Some(r) => {
                    // Module code: only the statement at fault stays
                    // interpreted; the statements around it are tried again.
                    let at = match c.node {
                        FnNode::Segment(_, stmts) => stmts
                            .iter()
                            .position(|s| s.span().start <= r.span.start && r.span.end <= s.span().end)
                            .map(|k| (stmts, k)),
                        _ => None,
                    };
                    match at {
                        Some((stmts, k)) => {
                            rejected.push((c.name, stmts[k].span(), r));
                            for piece in [&stmts[..k], &stmts[k + 1..]] {
                                if piece.iter().any(|s| !matches!(s, Statement::EmptyStatement(_))) {
                                    keep.push(segment(piece, scoping, &uses));
                                }
                            }
                        }
                        None => rejected.push((c.name, c.node.span(), r)),
                    }
                }
                None => keep.push(c),
            }
        }
        candidates = keep;
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_toplevel<'a>(
    program: &'a Program<'a>,
    scoping: &'a Scoping,
    interpreted: &HashSet<u32>,
    classes: &mut Vec<ClassInfo>,
    out: &mut Vec<Candidate<'a>>,
    rejected: &mut Vec<(String, Span, Reject)>,
    consts: &mut HashMap<SymbolId, ConstVal>,
    bindings: &mut HashMap<SymbolId, Binding>,
    aliases: &mut HashMap<SymbolId, SymbolId>,
) {
    use oxc::ast_visit::Visit;
    let written = |sym: SymbolId| scoping.get_resolved_references(sym).any(|r| r.is_write());
    // Names exported through `export { a, b as c }`.
    let mut export_names = HashSet::new();
    for stmt in &program.body {
        if let Statement::ExportNamedDeclaration(e) = stmt {
            for s in &e.specifiers {
                export_names.insert(s.local.name().to_string());
            }
        }
    }
    let mut bind = |id: &BindingIdentifier, kind: BindingKind, init: Option<Span>, stmt_end: u32, exported: bool| {
        let sym = id.symbol_id.get().unwrap();
        let name = id.name.to_string();
        let exported = exported || export_names.contains(&name);
        bindings.insert(sym, Binding { kind, name, written: written(sym), exported, init, stmt_end });
    };

    // Module-level consts can be folded, and const-bound functions called
    // directly from native code, when nothing can observe them in their
    // TDZ: no code that runs before their declaration can reach them, by
    // naming them or naming (transitively) functions, classes or values
    // whose code names them.
    let deferred = module_deferred_refs(program, scoping);
    let mut reached: HashSet<SymbolId> = HashSet::new();
    let run = |reached: &mut HashSet<SymbolId>, refs: Vec<SymbolId>| {
        let mut work = refs;
        while let Some(s) = work.pop() {
            if reached.insert(s) {
                work.extend(deferred.get(&s).into_iter().flatten().copied());
            }
        }
    };
    let mut methods = Vec::new();
    let mut method_rejects = Vec::new();
    let mut ctor_defs = Vec::new();
    // Bindings of class expressions compiled as classes (see below).
    let mut bound_classes: HashSet<SymbolId> = HashSet::new();
    // (An earlier top-level class, before the one starting at `start`, is
    // initialized while the prefix is pure.)
    let earlier_class = |bound: &HashSet<SymbolId>, start: u32, id: &IdentifierReference| {
        id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id()).is_some_and(|s| {
            (scoping.symbol_flags(s).is_class() || bound.contains(&s))
                && scoping.symbol_scope_id(s) == scoping.root_scope_id()
                && scoping.symbol_span(s).end <= start
        })
    };
    let global =
        |id: &IdentifierReference| id.reference_id.get().is_some_and(|r| scoping.get_reference(r).symbol_id().is_none());
    let mut add = |name: String, node: FnNode<'a>, symbol: SymbolId, site: Site, direct_callable: bool| {
        if written(symbol) {
            let r = Reject { span: node.span(), msg: format!("`{name}` is reassigned"), retry: false, callee: false };
            rejected.push((name, node.span(), r));
            return;
        }
        let body = match node {
            FnNode::Function(f) => f.body.as_deref(),
            FnNode::Arrow(a) => match &a.body {
                ArrowFunctionBody::FunctionBody(b) => Some(&**b),
                _ => None,
            },
            FnNode::Segment(..) => None,
        };
        if interpreted.contains(&node.span().start) {
            let r = Reject { span: node.span(), msg: "marked `interpreted`".into(), retry: false, callee: false };
            rejected.push((name, node.span(), r));
            return;
        }
        if body.is_some_and(|b| b.directives.iter().any(|d| d.directive.as_str() == "use no aot")) {
            let r = Reject { span: node.span(), msg: "marked \"use no aot\"".into(), retry: false, callee: false };
            rejected.push((name, node.span(), r));
            return;
        }
        match header(node) {
            Ok((params, rest, ret)) => out.push(Candidate {
                name,
                node,
                symbol: Some(symbol),
                // (An ordinary function's `this`: the receiver of a call, or
                // the instance `new` makes.)
                has_this: matches!(node, FnNode::Function(f) if function_uses_this(f)),
                site,
                direct_callable,
                params,
                rest,
                ret,
                class: None,
                cells: HashSet::new(),
                ctor: None,
            }),
            Err(r) => rejected.push((name, node.span(), r)),
        }
    };
    for stmt in &program.body {
        let end = stmt.span().end;
        if let Statement::ImportDeclaration(i) = stmt {
            for s in i.specifiers.iter().flatten() {
                bind(s.local(), BindingKind::Import, None, end, false);
            }
            continue;
        }
        let (decl, export_prefix) = match stmt {
            Statement::ExportDeclaration(e) => {
                (&e.declaration, Some(Span::new(e.span.start, e.declaration.span().start)))
            }
            _ => match stmt.as_declaration() {
                Some(d) => (d, None),
                None => {
                    if !matches!(stmt, Statement::EmptyStatement(_) | Statement::ExportNamedDeclaration(_)) {
                        run(&mut reached, module_refs(scoping, |v| v.visit_statement(stmt)));
                    }
                    continue;
                }
            },
        };
        let exported = export_prefix.is_some();
        match decl {
            Declaration::FunctionDeclaration(f) if f.body.is_some() => {
                let Some(id) = &f.id else { continue };
                bind(id, BindingKind::Function, None, end, exported);
                let symbol = id.symbol_id.get().expect("semantic analysis ran");
                let site = Site::Declaration { id: id.span, export_prefix };
                // Function declarations are hoisted: always initialized.
                add(id.name.to_string(), FnNode::Function(f), symbol, site, true);
            }
            Declaration::FunctionDeclaration(_) => {} // overload signature or `declare`
            Declaration::VariableDeclaration(v) if !v.declare => {
                let kind = match v.kind {
                    VariableDeclarationKind::Const => BindingKind::Const,
                    VariableDeclarationKind::Let => BindingKind::Let,
                    VariableDeclarationKind::Var => BindingKind::Var,
                    _ => {
                        run(&mut reached, module_refs(scoping, |vis| vis.visit_variable_declaration(v)));
                        continue;
                    }
                };
                for d in &v.declarations {
                    let BindingPattern::BindingIdentifier(id) = &d.id else {
                        // Destructuring: bind every name; initialized after the statement.
                        for id in d.id.get_binding_identifiers() {
                            bind(id, kind, None, end, exported);
                        }
                        run(&mut reached, module_refs(scoping, |vis| vis.visit_variable_declarator(d)));
                        continue;
                    };
                    bind(id, kind, d.init.as_ref().map(|i| i.span()), end, exported);
                    let symbol = id.symbol_id.get().unwrap();
                    let Some(init) = &d.init else {
                        continue;
                    };
                    // (Whether code that ran before can reach this binding.)
                    let unsafe_tdz = reached.contains(&symbol);
                    // A class expression bound to a name (`var X = class
                    // {...}`, as bundlers and compilers write class
                    // declarations): compiled as the class declaration
                    // `class X {...}` would be, when nothing can tell the
                    // binding from that class's: it is never reassigned, and
                    // no code that runs before it can reach it (to see it
                    // undefined, or in its TDZ).
                    if let Expression::ClassExpression(c) = init.without_parentheses()
                        && c.id.as_ref().is_none_or(|cid| cid.name == id.name)
                        && c.decorators.is_empty()
                        && !written(symbol)
                        && !unsafe_tdz
                    {
                        class_methods(c, &id.name, classes.len(), end, &mut methods, &mut method_rejects);
                        class_ctor(
                            c,
                            id,
                            classes.len(),
                            interpreted,
                            scoping,
                            &mut ctor_defs,
                            &mut methods,
                            &mut method_rejects,
                        );
                        let info = class_info(c, &id.name, classes);
                        classes.push(info);
                        let start = c.span.start;
                        if !pure_class(c, &|r| earlier_class(&bound_classes, start, r), &global) {
                            run(&mut reached, module_refs(scoping, |vis| vis.visit_class(c)));
                        }
                        bound_classes.insert(symbol);
                        // (Its own name, `class X`, is the binding's value
                        // once it is defined: the binding.)
                        if let Some(cid) = &c.id {
                            aliases.insert(cid.symbol_id(), symbol);
                        }
                        continue;
                    }
                    // (A `var` or `let` bound to a function and never assigned
                    // again or declared again is as a `const` would be: the
                    // functions of `var a = function a() {...}, b = ...`.)
                    let function_init = matches!(
                        init.without_parentheses(),
                        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
                    ) && !written(symbol)
                        && scoping.symbol_redeclarations(symbol).is_empty();
                    if kind != BindingKind::Const && !function_init {
                        if !pure_init(init) {
                            run(&mut reached, module_refs(scoping, |vis| vis.visit_expression(init)));
                        }
                        continue;
                    }
                    let site = Site::ConstInit { init: init.span() };
                    // Creating a function runs no code.
                    match init.without_parentheses() {
                        Expression::ArrowFunctionExpression(a) => {
                            add(id.name.to_string(), FnNode::Arrow(a), symbol, site, !unsafe_tdz);
                        }
                        Expression::FunctionExpression(f) => {
                            add(id.name.to_string(), FnNode::Function(f), symbol, site, !unsafe_tdz);
                        }
                        _ => match literal(init) {
                            Some(val) if !unsafe_tdz => {
                                consts.insert(symbol, val);
                            }
                            _ if pure_init(init) => {}
                            _ => run(&mut reached, module_refs(scoping, |vis| vis.visit_expression(init))),
                        },
                    }
                }
            }
            Declaration::VariableDeclaration(_) => {}
            Declaration::ClassDeclaration(c) => {
                if let Some(id) = &c.id {
                    bind(id, BindingKind::Class, None, end, exported);
                    if c.decorators.is_empty() && !c.declare {
                        class_methods(c, &id.name, classes.len(), end, &mut methods, &mut method_rejects);
                        if !written(id.symbol_id.get().unwrap()) {
                            class_ctor(
                                c,
                                id,
                                classes.len(),
                                interpreted,
                                scoping,
                                &mut ctor_defs,
                                &mut methods,
                                &mut method_rejects,
                            );
                        }
                    }
                    let info = class_info(c, &id.name, classes);
                    classes.push(info);
                }
                let start = c.span.start;
                if !pure_class(c, &|r| earlier_class(&bound_classes, start, r), &global) {
                    run(&mut reached, module_refs(scoping, |vis| vis.visit_class(c)));
                }
            }
            // Pure type-level declarations; enums and namespaces emit code but
            // nothing that can call into this module.
            _ => {}
        }
    }
    out.extend(methods);
    rejected.extend(method_rejects);
}

/// What a private name of a class is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateKind {
    Field,
    Method,
    Getter,
    Setter,
    /// A getter and a setter.
    Accessor,
}

/// A top-level class, for the native code of its methods.
#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub name: String,
    /// Just inside the `{` of the class body.
    pub body_start: u32,
    /// Private names (with their `#`).
    pub privates: Vec<(String, PrivateKind)>,
    /// `extends C` of an earlier top-level class `C`: its index.
    pub superclass: Option<usize>,
    /// The classes the type annotations of its instance fields name.
    pub fields: Vec<(String, HintName)>,
    /// Its instance fields annotated as numbers.
    pub nums: Vec<String>,
}

/// A class a type annotation names (resolved to an `ir::Hint` once all
/// top-level classes are known).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HintName {
    Class(String),
    ArrayOf(String),
}

/// The class (or array of a class) annotation `t` names, if it does:
/// `C`, `C[]`, `Array<C>`, and those or `null` / `undefined`.
fn hint_name(t: &TSType) -> Option<HintName> {
    let array = |h: HintName| match h {
        HintName::Class(c) => Some(HintName::ArrayOf(c)),
        HintName::ArrayOf(_) => None,
    };
    match t {
        TSType::TSParenthesizedType(p) => hint_name(&p.type_annotation),
        TSType::TSArrayType(a) => array(hint_name(&a.element_type)?),
        TSType::TSTypeReference(r) => {
            let TSTypeName::IdentifierReference(id) = &r.type_name else { return None };
            match r.type_arguments.as_ref().map(|a| &a.params) {
                None => Some(HintName::Class(id.name.to_string())),
                Some(ps) if id.name == "Array" && ps.len() == 1 => array(hint_name(&ps[0])?),
                Some(_) => None,
            }
        }
        TSType::TSUnionType(u) => {
            let mut rest =
                u.types.iter().filter(|t| !matches!(t, TSType::TSNullKeyword(_) | TSType::TSUndefinedKeyword(_)));
            let one = rest.next()?;
            if rest.next().is_some() {
                return None;
            }
            hint_name(one)
        }
        _ => None,
    }
}

fn resolve_hint(h: &HintName, classes: &[ClassInfo]) -> Option<ir::Hint> {
    let find = |n: &str| classes.iter().rposition(|c| c.name == n);
    let record = |n: &str| RECORD_NAMES.with(|r| r.borrow().iter().position(|x| x == n));
    Some(match h {
        HintName::Class(n) => match find(n) {
            Some(k) => ir::Hint::Class(k),
            None => ir::Hint::Record(record(n)?),
        },
        HintName::ArrayOf(n) => match find(n) {
            Some(k) => ir::Hint::ArrayOf(k),
            None => ir::Hint::ArrayOfRecord(record(n)?),
        },
    })
}

thread_local! {
    /// The names of the module's record types (`collect_records`), for
    /// `resolve_hint`.
    static RECORD_NAMES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// An object type with a name (`type T = { ... }`, `interface T { ... }`
/// at the top level): its fields in declaration order, with the classes
/// their annotations name. Objects made with the fields in this order get
/// fast field access (see `ir::specialize_fields`).
#[derive(Clone, Debug)]
pub struct RecordInfo {
    pub name: String,
    pub fields: Vec<String>,
    pub hints: Vec<Option<HintName>>,
    /// Which fields are annotated as numbers.
    pub nums: Vec<bool>,
}

/// The record types of the module: object type literals of plain fields
/// (no optional, computed or method members), not `aot type`s, generics or
/// interfaces that extend others.
fn collect_records(program: &Program) -> Vec<RecordInfo> {
    type Members = (Vec<String>, Vec<Option<HintName>>, Vec<bool>);
    fn members(ms: &[TSSignature]) -> Option<Members> {
        let mut fields: Vec<String> = Vec::new();
        let mut hints = Vec::new();
        let mut nums = Vec::new();
        for m in ms {
            let TSSignature::TSPropertySignature(p) = m else { return None };
            if p.optional || p.computed {
                return None;
            }
            let name = p.key.static_name()?.to_string();
            if fields.contains(&name) {
                return None;
            }
            fields.push(name);
            hints.push(p.type_annotation.as_ref().and_then(|t| hint_name(&t.type_annotation)));
            nums.push(p.type_annotation.as_ref().is_some_and(|t| is_number_type(&t.type_annotation)));
        }
        (!fields.is_empty()).then_some((fields, hints, nums))
    }
    let mut out = Vec::new();
    for s in &program.body {
        let decl = match s {
            Statement::ExportDeclaration(e) => &e.declaration,
            _ => match s.as_declaration() {
                Some(d) => d,
                None => continue,
            },
        };
        let found = match decl {
            Declaration::TSTypeAliasDeclaration(d)
                if d.type_parameters.is_none() && crate::structs::struct_named(&d.id.name).is_none() =>
            {
                match &d.type_annotation {
                    TSType::TSTypeLiteral(lit) => members(&lit.members).map(|m| (d.id.name.to_string(), m)),
                    _ => None,
                }
            }
            Declaration::TSInterfaceDeclaration(i) if i.type_parameters.is_none() && i.extends.is_empty() => {
                members(&i.body.body).map(|m| (i.id.name.to_string(), m))
            }
            _ => None,
        };
        if let Some((name, (fields, hints, nums))) = found
            && !out.iter().any(|r: &RecordInfo| r.name == name)
        {
            out.push(RecordInfo { name, fields, hints, nums });
        }
    }
    out
}

/// A class whose private names or home object native code uses: it gets
/// a static block that hands them over (see `erase`).
#[derive(Clone, Debug)]
pub struct ClassUse {
    /// Index in `Outcome::classes`, which is also its home slots' (`2k`).
    pub class: usize,
    /// The private name variables (`#x`, or `#x<set>` for a setter) it
    /// hands over, and their slots.
    pub vars: Vec<(String, usize)>,
}

fn class_info(c: &Class, name: &str, earlier: &[ClassInfo]) -> ClassInfo {
    let mut privates: Vec<(String, PrivateKind)> = Vec::new();
    for el in &c.body.body {
        let (key, kind) = match el {
            ClassElement::PropertyDefinition(p) => (&p.key, PrivateKind::Field),
            ClassElement::MethodDefinition(m) => (
                &m.key,
                match m.kind {
                    MethodDefinitionKind::Get => PrivateKind::Getter,
                    MethodDefinitionKind::Set => PrivateKind::Setter,
                    _ => PrivateKind::Method,
                },
            ),
            _ => continue,
        };
        let PropertyKey::PrivateIdentifier(id) = key else { continue };
        let name = format!("#{}", id.name);
        match privates.iter_mut().find(|(n, _)| *n == name) {
            Some((_, k)) => *k = PrivateKind::Accessor,
            None => privates.push((name, kind)),
        }
    }
    let superclass = match c.heritage.as_ref().map(|h| h.expression.without_parentheses()) {
        Some(Expression::Identifier(id)) => earlier.iter().rposition(|k| k.name == id.name.as_str()),
        _ => None,
    };
    let mut fields = Vec::new();
    let mut nums = Vec::new();
    for el in &c.body.body {
        if let ClassElement::PropertyDefinition(p) = el
            && !p.r#static
            && let PropertyKey::StaticIdentifier(k) = &p.key
            && let Some(a) = &p.type_annotation
        {
            if let Some(h) = hint_name(&a.type_annotation) {
                fields.push((k.name.to_string(), h));
            }
            if is_number_type(&a.type_annotation) {
                nums.push(k.name.to_string());
            }
        }
    }
    ClassInfo { name: name.to_string(), body_start: c.body.span.start + 1, privates, superclass, fields, nums }
}

/// Ordinary methods (static or not) of a top-level class.
fn class_methods<'a>(
    c: &'a Class<'a>,
    class: &str,
    index: usize,
    stmt_end: u32,
    out: &mut Vec<Candidate<'a>>,
    rejected: &mut Vec<(String, Span, Reject)>,
) {
    for el in &c.body.body {
        let ClassElement::MethodDefinition(m) = el else { continue };
        let accessor = match m.kind {
            MethodDefinitionKind::Method => 0,
            MethodDefinitionKind::Get => 1,
            MethodDefinitionKind::Set => 2,
            _ => continue,
        };
        if m.computed
            || !m.decorators.is_empty()
            || m.r#type != MethodDefinitionType::MethodDefinition
            || m.value.body.is_none()
        {
            continue;
        }
        let node = FnNode::Function(&m.value);
        // A private instance method: called directly by native code.
        if let PropertyKey::PrivateIdentifier(id) = &m.key {
            if accessor != 0 || m.r#static {
                continue;
            }
            let name = format!("#{}", id.name);
            match header(node) {
                Ok((params, rest, ret)) => out.push(Candidate {
                    name,
                    node,
                    symbol: None,
                    has_this: true,
                    site: Site::Private { class: class.to_string() },
                    direct_callable: true,
                    params,
                    rest,
                    // (Called directly: a JS value unless annotated, so that
                    // recursive calls need no annotation.)
                    ret: ret.or(Some(Ty::Any)),
                    class: Some((index, false)),
                    cells: HashSet::new(),
                    ctor: None,
                }),
                Err(r) => rejected.push((format!("{class}.{name}"), node.span(), r)),
            }
            continue;
        }
        let key = match &m.key {
            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
            PropertyKey::StringLiteral(s) => s.value.to_string(),
            _ => continue,
        };
        let site =
            Site::Method { class: class.to_string(), is_static: m.r#static, key: key.clone(), stmt_end, accessor };
        // As JS names accessor functions.
        let name = match accessor {
            1 => format!("get {key}"),
            2 => format!("set {key}"),
            _ => key.clone(),
        };
        match header(node) {
            Ok((params, rest, ret)) => out.push(Candidate {
                name,
                node,
                symbol: None,
                has_this: true,
                site,
                direct_callable: false,
                params,
                rest,
                ret,
                class: Some((index, m.r#static)),
                cells: HashSet::new(),
                ctor: None,
            }),
            Err(r) => rejected.push((format!("{class}.{name}"), node.span(), r)),
        }
    }
}

/// A class whose constructor `class_ctor` compiles natively: what the
/// constructors of its subclasses need of it.
struct CtorDef<'a> {
    symbol: SymbolId,
    name: String,
    span: Span,
    index: usize,
    node: FnNode<'a>,
    params: Vec<Param<'a>>,
    rest: bool,
    fields: Vec<(String, Option<&'a Expression<'a>>)>,
    param_props: Vec<(String, SymbolId)>,
    /// Its superclass (index in the `CtorDef`s).
    parent: Option<usize>,
    dynamic: bool,
    forward: bool,
    /// It has private methods or accessors: its instances get its brand.
    brand: bool,
    /// Its parameters' defaults may run code (`CtorInfo::recheck`).
    impure: bool,
}

/// The constructor of a top-level class, as a native function that makes
/// the instance itself: for classes whose instances are plain objects with
/// their fields, public or private (no accessors or computed keys), whose
/// initializers cannot see the instance but in arrow functions that are
/// the field's value (a derived class's may: its fields are defined in
/// order), and whose constructor returns nothing but the instance; its
/// private methods and accessors give the instance its brand. A derived class qualifies when its superclass
/// (named by its binding) does, and its constructor starts with its only
/// `super(...)` call (whose arguments, and the parameters' defaults, which
/// run before it, are plain values) or is the default one. Its
/// superclasses' constructors are then also compiled to make its instances
/// (`CtorInfo::home`).
#[allow(clippy::too_many_arguments)]
fn class_ctor<'a>(
    c: &'a Class<'a>,
    id: &BindingIdentifier<'a>,
    index: usize,
    interpreted: &HashSet<u32>,
    scoping: &Scoping,
    defs: &mut Vec<CtorDef<'a>>,
    out: &mut Vec<Candidate<'a>>,
    rejected: &mut Vec<(String, Span, Reject)>,
) {
    let name = id.name.to_string();
    // Why the constructor stays interpreted (reported at the class's name).
    let mut give_up = |span: Span, msg: String| {
        rejected.push((name.clone(), id.span, Reject { span, msg, retry: false, callee: false }));
    };
    // (A superclass without a native constructor: `dynamic`.)
    let (parent, dynamic) = match &c.heritage {
        None => (None, false),
        Some(h) => {
            let found = match h.expression.without_parentheses() {
                Expression::Identifier(r) => scoping
                    .get_reference(r.reference_id())
                    .symbol_id()
                    .and_then(|sym| defs.iter().position(|d| d.symbol == sym)),
                _ => None,
            };
            (found, found.is_none())
        }
    };
    let derived = parent.is_some() || dynamic;
    let mut fields = Vec::new();
    let mut ctor = None;
    let mut brand = false;
    for el in &c.body.body {
        match el {
            ClassElement::MethodDefinition(m) if m.kind == MethodDefinitionKind::Constructor => ctor = Some(m),
            ClassElement::MethodDefinition(m)
                if !m.r#static && matches!(m.key, PropertyKey::PrivateIdentifier(_)) =>
            {
                brand = true;
            }
            ClassElement::PropertyDefinition(p) if p.r#static || p.declare => {}
            ClassElement::PropertyDefinition(p) if p.r#type == PropertyDefinitionType::TSAbstractPropertyDefinition => {
            }
            ClassElement::PropertyDefinition(p) => {
                // (A private field's name keeps its `#`.)
                let key = match &p.key {
                    PropertyKey::StaticIdentifier(k) if !p.computed => k.name.to_string(),
                    PropertyKey::PrivateIdentifier(k) => format!("#{}", k.name),
                    _ => {
                        give_up(p.span, "field with a computed name".into());
                        return;
                    }
                };
                if !p.decorators.is_empty() {
                    give_up(p.span, "decorated field".into());
                    return;
                }
                if !derived && p.value.as_ref().is_some_and(|v| mentions_this(v) && !this_arrow(v)) {
                    give_up(p.span, "field initializer uses `this` (but in an arrow function that is its value)".into());
                    return;
                }
                fields.push((key, p.value.as_ref()));
            }
            ClassElement::AccessorProperty(a) if !a.r#static => {
                give_up(a.span, "`accessor` field".into());
                return;
            }
            _ => {}
        }
    }
    let mut param_props = Vec::new();
    let mut forward = false;
    let mut impure = false;
    let (node, params, rest) = match ctor {
        Some(m) => {
            let f = &m.value;
            if !m.decorators.is_empty() || f.body.is_none() || interpreted.contains(&f.span.start) {
                return;
            }
            let body = f.body.as_ref().unwrap();
            if body.directives.iter().any(|d| d.directive.as_str() == "use no aot") {
                return;
            }
            // TS parameter properties assign the instance from the
            // parameters (an identifier each).
            for p in &f.params.items {
                if p.accessibility.is_some() || p.readonly || p.r#override {
                    let BindingPattern::BindingIdentifier(b) = &p.pattern else { return };
                    param_props.push((b.name.to_string(), b.symbol_id.get().unwrap()));
                }
            }
            if derived {
                let Some(i) = super_first(f) else {
                    give_up(
                        f.span,
                        "the constructor does not start with `super(...)` (with arguments that cannot see `this`)"
                            .into(),
                    );
                    return;
                };
                impure = i;
            }
            // Their fields come first, `undefined` until assigned.
            fields.splice(0..0, param_props.iter().map(|(n, _)| (n.clone(), None)));
            let node = FnNode::Function(f);
            match header(node) {
                Ok((params, rest, _)) => (node, params, rest),
                Err(r) => {
                    rejected.push((name, node.span(), r));
                    return;
                }
            }
        }
        // The default constructor: no parameters, nothing to run; a derived
        // class's passes its arguments on.
        None => match parent {
            None if dynamic => {
                forward = true;
                (FnNode::Segment(c.span, &[]), Vec::new(), false)
            }
            None => (FnNode::Segment(c.span, &[]), Vec::new(), false),
            Some(p) if !defs[p].rest && !(defs[p].dynamic && defs[p].forward) => {
                forward = true;
                let params = defs[p].params.iter().map(|q| Param { default: None, ..q.clone() }).collect();
                (FnNode::Segment(c.span, &[]), params, false)
            }
            Some(_) => return,
        },
    };
    let def = CtorDef {
        symbol: id.symbol_id.get().unwrap(),
        name: name.clone(),
        span: c.span,
        index,
        node,
        params,
        rest,
        fields,
        param_props,
        parent,
        dynamic,
        forward,
        brand,
        impure,
    };
    // Its own (called by `new`), checking its superclasses, then its
    // superclasses' (each called by `super(...)`).
    let mut checks = Vec::new();
    let mut chain = vec![&def];
    let mut at = &def;
    while let Some(p) = at.parent {
        checks.push((at.index, defs[p].index));
        at = &defs[p];
        chain.push(at);
    }
    // (Not known when a superclass's interpreted constructor makes the
    // instance.)
    let instance_fields: Vec<String> = if at.dynamic {
        Vec::new()
    } else {
        chain
            .iter()
            .rev()
            .flat_map(|d| d.fields.iter().filter(|(n, _)| !n.starts_with('#')).map(|(n, _)| n.clone()))
            .collect()
    };
    let mut make = |d: &CtorDef<'a>, checks: Vec<(usize, usize)>, instance_fields: Vec<String>| {
        let name = if d.index == index { name.clone() } else { format!("{} (constructing {name})", d.name) };
        // Called only directly by native code, with JS values.
        let params = d.params.iter().map(|p| Param { ty: Ty::Any, ..p.clone() }).collect();
        out.push(Candidate {
            name: name.clone(),
            node: d.node,
            symbol: None,
            has_this: false,
            site: Site::Ctor { class: name },
            direct_callable: true,
            params,
            rest: d.rest,
            // (So that recursive constructors need no annotation.)
            ret: Some(Ty::Any),
            // (Its private names, for the fields and the body.)
            class: Some((d.index, false)),
            cells: HashSet::new(),
            ctor: Some(CtorInfo {
                symbol: Some(d.symbol),
                span: d.span,
                class: d.index,
                home: index,
                fields: d.fields.clone(),
                param_props: d.param_props.clone(),
                parent: d.parent.map(|p| defs[p].index),
                dynamic: d.dynamic,
                forward: d.forward,
                checks,
                instance_fields,
                brand: d.brand,
                local: false,
                recheck: if d.impure {
                    let mut links = Vec::new();
                    let mut at = d;
                    while let Some(p) = at.parent {
                        links.push((at.index, defs[p].index));
                        at = &defs[p];
                    }
                    links
                } else {
                    Vec::new()
                },
            }),
        });
    };
    make(&def, checks, instance_fields);
    let mut up = def.parent;
    while let Some(p) = up {
        make(&defs[p], Vec::new(), Vec::new());
        up = defs[p].parent;
    }
    defs.push(def);
}

/// Whether derived class constructor `f` starts with `super(args)`, its
/// only `super(...)` call, with arguments and parameter defaults that
/// cannot see `this`: then whether a default may run code (reconfigure the
/// classes it is made by; else they are literals).
fn super_first(f: &Function) -> Option<bool> {
    use oxc::ast_visit::{Visit, walk};
    struct Supers(usize);
    impl<'a> Visit<'a> for Supers {
        fn visit_call_expression(&mut self, c: &CallExpression<'a>) {
            if matches!(c.callee, Expression::Super(_)) {
                self.0 += 1;
            }
            walk::walk_call_expression(self, c);
        }
    }
    if f.params.items.iter().any(|p| p.initializer.as_deref().is_some_and(mentions_this)) {
        return None;
    }
    let impure = f.params.items.iter().any(|p| p.initializer.as_deref().is_some_and(|d| !plain_value(d)));
    let body = f.body.as_ref().unwrap();
    let Some(Statement::ExpressionStatement(s)) = body.statements.first() else { return None };
    let Expression::CallExpression(call) = &s.expression else { return None };
    if !matches!(call.callee, Expression::Super(_))
        || call.arguments.iter().any(|a| a.as_expression().is_none_or(mentions_this))
    {
        return None;
    }
    let mut v = Supers(0);
    v.visit_function_body(body);
    (v.0 == 1).then_some(impure)
}

/// A value that evaluating cannot see anything: a literal, `undefined`,
/// or an object or array literal of them (`options = {}`).
fn plain_value(e: &Expression) -> bool {
    match e.without_parentheses() {
        e if literal(e).is_some() => true,
        Expression::StringLiteral(_) | Expression::NullLiteral(_) => true,
        Expression::Identifier(i) => i.name == "undefined",
        Expression::TemplateLiteral(t) => t.expressions.is_empty(),
        Expression::ArrayExpression(a) => a.elements.iter().all(|el| el.as_expression().is_some_and(plain_value)),
        Expression::ObjectExpression(o) => o.properties.iter().all(|p| match p {
            ObjectPropertyKind::ObjectProperty(p) => {
                !p.computed && !p.method && p.kind == PropertyKind::Init && plain_value(&p.value)
            }
            ObjectPropertyKind::SpreadProperty(_) => false,
        }),
        _ => false,
    }
}

/// Whether `e` is an arrow function (whose `this` is the instance, in a
/// field initializer): making it runs no code.
fn this_arrow(e: &Expression) -> bool {
    matches!(e.without_parentheses(), Expression::ArrowFunctionExpression(_))
}

/// Whether an ordinary function uses its own `this`: in its parameters or
/// body, outside nested non-arrow functions (conservatively: in a class).
fn function_uses_this(f: &Function) -> bool {
    use oxc::ast_visit::Visit;
    use oxc::syntax::scope::ScopeFlags;
    struct Finder(bool);
    impl<'a> Visit<'a> for Finder {
        fn visit_this_expression(&mut self, _: &ThisExpression) {
            self.0 = true;
        }
        fn visit_function(&mut self, _: &Function<'a>, _: ScopeFlags) {}
        fn visit_class(&mut self, _: &Class<'a>) {
            self.0 = true;
        }
    }
    let mut finder = Finder(false);
    finder.visit_formal_parameters(&f.params);
    if let Some(body) = &f.body {
        finder.visit_function_body(body);
    }
    finder.0
}

/// Whether `this` appears in `e` outside nested non-arrow functions.
fn mentions_this(e: &Expression) -> bool {
    use oxc::ast_visit::Visit;
    use oxc::syntax::scope::ScopeFlags;
    struct Finder(bool);
    impl<'a> Visit<'a> for Finder {
        fn visit_this_expression(&mut self, _: &ThisExpression) {
            self.0 = true;
        }
        fn visit_function(&mut self, _: &Function<'a>, _: ScopeFlags) {}
        fn visit_class(&mut self, _: &Class<'a>) {
            // (Its computed keys and field initializers could see it.)
            self.0 = true;
        }
    }
    let mut f = Finder(false);
    f.visit_expression(e);
    f.0
}

/// Whether evaluating `e` runs no code of the program and reads no binding
/// (which could be uninitialized): literals, functions, and arrays and
/// objects of those. A declaration initialized so keeps a module's prefix
/// pure (see `collect_toplevel`); the bundler also evaluates such
/// declarations of lazily evaluated modules early.
pub(crate) fn pure_init(e: &Expression) -> bool {
    let primitive = |e: &Expression| {
        matches!(
            e.without_parentheses(),
            Expression::NumericLiteral(_)
                | Expression::StringLiteral(_)
                | Expression::BooleanLiteral(_)
                | Expression::NullLiteral(_)
                | Expression::BigIntLiteral(_)
        )
    };
    match e.without_parentheses() {
        e if primitive(e) => true,
        Expression::RegExpLiteral(_) | Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => true,
        Expression::TemplateLiteral(t) => t.expressions.is_empty(),
        Expression::UnaryExpression(u) => u.operator != UnaryOperator::Delete && primitive(&u.argument),
        Expression::ArrayExpression(a) => a.elements.iter().all(|el| match el {
            ArrayExpressionElement::SpreadElement(_) => false,
            ArrayExpressionElement::Elision(_) => true,
            el => el.as_expression().is_some_and(pure_init),
        }),
        Expression::ObjectExpression(o) => o.properties.iter().all(|p| match p {
            ObjectPropertyKind::ObjectProperty(p) => !p.computed && !p.shorthand && pure_init(&p.value),
            ObjectPropertyKind::SpreadProperty(_) => false,
        }),
        Expression::TSAsExpression(e) => pure_init(&e.expression),
        Expression::TSSatisfiesExpression(e) => pure_init(&e.expression),
        Expression::TSTypeAssertion(e) => pure_init(&e.expression),
        Expression::TSNonNullExpression(e) => pure_init(&e.expression),
        _ => false,
    }
}

/// Whether evaluating class `c` runs no code of the program: no
/// decorators, static blocks or static initializers but pure ones, keys
/// that are names, literals or `Symbol.<name>` (of the global `Symbol`),
/// and no superclass but one `earlier_class` accepts.
/// Whether `e` is an anonymous function, which takes the name of the
/// binding it is assigned to.
fn anonymous_function(e: &Expression) -> bool {
    match e.without_parentheses() {
        Expression::ArrowFunctionExpression(_) => true,
        Expression::FunctionExpression(f) => f.id.is_none(),
        _ => false,
    }
}

pub(crate) fn pure_class(
    c: &Class,
    earlier_class: &dyn Fn(&IdentifierReference) -> bool,
    global: &dyn Fn(&IdentifierReference) -> bool,
) -> bool {
    let key = |k: &PropertyKey, computed: bool| {
        !computed
            || match k {
                PropertyKey::StringLiteral(_) | PropertyKey::NumericLiteral(_) => true,
                PropertyKey::StaticMemberExpression(m) => {
                    matches!(&m.object, Expression::Identifier(id) if id.name == "Symbol" && global(id))
                }
                _ => false,
            }
    };
    c.decorators.is_empty()
        && match c.heritage.as_ref().map(|h| h.expression.without_parentheses()) {
            None => true,
            Some(Expression::Identifier(id)) => earlier_class(id),
            Some(_) => false,
        }
        && c.body.body.iter().all(|el| match el {
            ClassElement::StaticBlock(_) => false,
            ClassElement::MethodDefinition(m) => m.decorators.is_empty() && key(&m.key, m.computed),
            ClassElement::PropertyDefinition(p) => {
                p.decorators.is_empty() && key(&p.key, p.computed) && (!p.r#static || p.value.as_ref().is_none_or(pure_init))
            }
            ClassElement::AccessorProperty(p) => {
                p.decorators.is_empty() && key(&p.key, p.computed) && (!p.r#static || p.value.as_ref().is_none_or(pure_init))
            }
            ClassElement::TSIndexSignature(_) => true,
        })
}

fn literal(e: &Expression) -> Option<ConstVal> {
    match e.without_parentheses() {
        Expression::NumericLiteral(n) => Some(ConstVal::Num(n.value)),
        Expression::BooleanLiteral(b) => Some(ConstVal::Bool(b.value)),
        Expression::UnaryExpression(u) if u.operator == UnaryOperator::UnaryNegation => match literal(&u.argument)? {
            ConstVal::Num(v) => Some(ConstVal::Num(-v)),
            ConstVal::Bool(_) => None,
        },
        _ => None,
    }
}

/// Whether a TS annotation says `number` (or `int`, `double`).
fn is_number_type(t: &TSType) -> bool {
    matches!(ts_type(t), Some(Ty::F64 | Ty::I32))
}

/// The unboxed type a TS annotation allows, if any.
fn ts_type(t: &TSType) -> Option<Ty> {
    match t {
        TSType::TSNumberKeyword(_) => Some(Ty::F64),
        TSType::TSBooleanKeyword(_) => Some(Ty::Bool),
        TSType::TSVoidKeyword(_) => Some(Ty::Void),
        TSType::TSParenthesizedType(p) => ts_type(&p.type_annotation),
        // An array of an `aot type`, as `structs::array_sites` makes them.
        TSType::TSArrayType(a) => match ts_type(&a.element_type)? {
            Ty::Struct(k) => Some(Ty::StructArr(k)),
            _ => None,
        },
        TSType::TSTypeReference(r)
            if matches!(&r.type_name, TSTypeName::IdentifierReference(id) if id.name == "Array")
                && r.type_arguments.as_ref().is_some_and(|a| a.params.len() == 1) =>
        {
            match ts_type(&r.type_arguments.as_ref().unwrap().params[0])? {
                Ty::Struct(k) => Some(Ty::StructArr(k)),
                _ => None,
            }
        }
        // Typed arrays (or a user type of the same name: either way, the
        // entry checks the class).
        TSType::TSTypeReference(r) => match &r.type_name {
            TSTypeName::IdentifierReference(id) => match id.name.as_str() {
                // `type int = number` / `type double = number`: `int` is
                // checked to hold int32 values.
                "int" => Some(Ty::I32),
                "double" => Some(Ty::F64),
                name if r.type_arguments.is_some() => ArrKind::from_name(name).map(Ty::Arr),
                name => match (crate::structs::struct_named(name), crate::structs::array_named(name)) {
                    (Some(k), _) if crate::structs::struct_nullable(name) => Some(Ty::StructOrUndef(k)),
                    (Some(k), _) => Some(Ty::Struct(k)),
                    (None, Some(k)) => Some(Ty::StructArr(k)),
                    (None, None) => ArrKind::from_name(name).map(Ty::Arr),
                },
            },
            _ => None,
        },
        _ => None,
    }
}

/// A parameter as `FnCx::function` takes it: its binding, ABI type and
/// default value.
type ParamSpec<'a> = (&'a BindingPattern<'a>, Ty, Option<&'a Expression<'a>>);

/// Parameters, whether the last one is a rest parameter, and the annotated
/// return type if any.
type Header<'a> = (Vec<Param<'a>>, bool, Option<Ty>);

fn header<'a>(node: FnNode<'a>) -> R<Header<'a>> {
    let (span, this_param, params, return_type) = match node {
        FnNode::Segment(..) => return Ok((Vec::new(), false, None)),
        FnNode::Function(f) => (f.span, f.this_param.is_some(), &f.params, &f.return_type),
        FnNode::Arrow(a) => (a.span, false, &a.params, &a.return_type),
    };
    if this_param {
        return reject(span, "`this` parameters are not supported");
    }
    let mut out = Vec::new();
    for p in &params.items {
        let annotated = p.type_annotation.as_ref().and_then(|a| ts_type(&a.type_annotation));
        let ty = match annotated {
            Some(
                t @ (Ty::F64 | Ty::I32 | Ty::Bool | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_)),
            ) if p.initializer.is_none()
                    && !p.optional
                    && matches!(p.pattern, BindingPattern::BindingIdentifier(_)) =>
            {
                t
            }
            _ => Ty::Any,
        };
        out.push(Param { pattern: &p.pattern, ty, default: p.initializer.as_deref() });
    }
    if let Some(rest) = &params.rest {
        out.push(Param { pattern: &rest.rest.argument, ty: Ty::Any, default: None });
    }
    let ret = return_type.as_ref().map(|a| ts_type(&a.type_annotation).unwrap_or(Ty::Any));
    Ok((out, params.rest.is_some(), ret))
}

#[derive(Clone)]
enum State {
    Todo,
    InProgress,
    Done(Box<ir::Func>),
    Failed(Reject),
}

struct ModuleCx<'m, 'a> {
    scoping: &'a Scoping,
    /// Bindings declared `int` (stores into them wrap; see `ints`).
    int_syms: &'m HashSet<SymbolId>,
    /// Functions marked `interpreted` (start offsets).
    interpreted: &'m HashSet<u32>,
    consts: &'m HashMap<SymbolId, ConstVal>,
    bindings: &'m HashMap<SymbolId, Binding>,
    /// The names class expressions bound to module bindings give
    /// themselves: those bindings (see `collect_toplevel`).
    aliases: &'m HashMap<SymbolId, SymbolId>,
    cands: &'m [Candidate<'a>],
    state: Vec<State>,
    /// Per function: locals that must be `Any`.
    widened: &'m mut HashMap<u32, HashSet<SymbolId>>,
    /// Actual return types that differ from the annotation.
    ret_override: &'m mut HashMap<u32, Ty>,
    /// Functions (their span start) whose return type is `Any` whatever they
    /// return: called by themselves (recursion) before their return type
    /// was known.
    any_ret: &'m mut HashSet<u32>,
    /// Per candidate: whether it uses its `arguments` (then native code
    /// calls it through its JS entry, which makes them).
    arguments: HashMap<usize, bool>,
    ret_changed: bool,
    strings: Vec<String>,
    string_ids: HashMap<String, ir::StrId>,
    atoms: Vec<String>,
    atom_ids: HashMap<String, ir::AtomId>,
    cells: Vec<SymbolId>,
    cell_ids: HashMap<SymbolId, ir::CellId>,
    closures: Vec<ir::Func>,
    closure_spans: Vec<Span>,
    classes: &'m [ClassInfo],
    /// Private name slots: (class, variable).
    privates: Vec<(usize, String)>,
    private_ids: HashMap<(usize, String), usize>,
    /// Classes whose home object native code uses.
    homes_used: Vec<usize>,
    /// `aot type` layouts, and the object literals that make their objects
    /// (span start: span end, layout).
    layouts: &'m [ir::Layout],
    struct_sites: &'m HashMap<u32, (u32, u32)>,
    array_sites: &'m HashMap<u32, (u32, u32)>,
    /// `JSON.parse(text)` and `await r.json()` converted to an `aot type`
    /// (span start: span end, desc).
    parse_sites: &'m HashMap<u32, (u32, i32)>,
    json_sites: &'m HashMap<u32, (u32, i32)>,
    sure_arrays: &'m HashMap<SymbolId, u32>,
}

impl<'m, 'a> ModuleCx<'m, 'a> {
    fn uses_arguments(&mut self, i: usize) -> bool {
        let (node, scoping) = (self.cands[i].node, self.scoping);
        *self.arguments.entry(i).or_insert_with(|| uses_arguments(node, scoping))
    }

    fn ensure(&mut self, i: usize) {
        if !matches!(self.state[i], State::Todo) {
            return;
        }
        self.state[i] = State::InProgress;
        let c = &self.cands[i];
        let (sym, node, name, has_this) = (c.node.span().start, c.node, c.name.clone(), c.has_this);
        let ret_annot = c.ret;
        let params: Vec<ParamSpec<'a>> = c.params.iter().map(|p| (p.pattern, p.ty, p.default)).collect();
        let rest = c.rest;
        let class = c.class;
        let ctor = c.ctor.clone();
        let cells = c.cells.clone();
        // (A constructor's field initializers run in it, with its `this`.)
        let fields: Vec<&'a Expression<'a>> =
            ctor.as_ref().map(|k| k.fields.iter().filter_map(|(_, e)| *e).collect()).unwrap_or_default();
        let caps = Rc::new(capture_info(node, self.scoping, &cells, &fields));
        let result = loop {
            let widened = self.widened.get(&sym).cloned().unwrap_or_default();
            let mut fx = FnCx::new(self, caps.clone(), node.span().start, false, widened);
            fx.rest = rest;
            fx.class = class;
            fx.has_this = has_this;
            fx.ordinary_this = has_this && class.is_none() && ctor.is_none();
            fx.ctor = ctor.clone();
            fx.ret_int = ret_annot == Some(Ty::I32);
            fx.seg_cells = cells.clone();
            let r = fx.function(node, name.clone(), params.clone());
            match (r, fx.widen_request) {
                (Err(e), Some(s)) if e.retry => {
                    self.widened.entry(sym).or_default().insert(s);
                }
                (r, _) => break r,
            }
        };
        self.state[i] = match result {
            Ok(f) => {
                // Callers assumed the annotated (or previously overridden)
                // return type; check that it held.
                let c = &self.cands[i];
                let assumed = self.ret_override.get(&c.node.span().start).copied().or(c.ret);
                if let Some(assumed) = assumed
                    && assumed != f.ret
                {
                    self.ret_override.insert(c.node.span().start, f.ret);
                    self.ret_changed = true;
                }
                State::Done(Box::new(f))
            }
            Err(r) => State::Failed(r),
        };
    }

    /// Signature of candidate `i` as seen by a direct caller.
    fn signature(&mut self, i: usize, call_span: Span) -> R<(Vec<Ty>, Ty)> {
        let params = self.cands[i].params.iter().map(|p| p.ty).collect();
        let c = &self.cands[i];
        if let Some(ret) = self.ret_override.get(&c.node.span().start).copied().or(c.ret) {
            // Callers may proceed optimistically, even while the callee is
            // still being checked (recursion). The fixpoint undoes this if
            // the callee turns out not to be native or to return otherwise.
            if let State::Failed(_) = self.state[i] {
                return callee_failed(call_span, &c.name);
            }
            return Ok((params, ret));
        }
        self.ensure(i);
        match &self.state[i] {
            State::Done(f) => Ok((params, f.ret)),
            State::InProgress => {
                // Recursion before the return type is known: any value (the
                // function's returns are then boxed, whatever they are).
                let key = self.cands[i].node.span().start;
                self.any_ret.insert(key);
                self.ret_override.insert(key, Ty::Any);
                Ok((params, Ty::Any))
            }
            State::Failed(_) => callee_failed(call_span, &self.cands[i].name),
            State::Todo => unreachable!(),
        }
    }

    fn string(&mut self, s: &str) -> ir::StrId {
        if let Some(&id) = self.string_ids.get(s) {
            return id;
        }
        self.strings.push(s.to_string());
        self.string_ids.insert(s.to_string(), self.strings.len() - 1);
        self.strings.len() - 1
    }

    fn atom(&mut self, s: &str) -> ir::AtomId {
        if let Some(&id) = self.atom_ids.get(s) {
            return id;
        }
        self.atoms.push(s.to_string());
        self.atom_ids.insert(s.to_string(), self.atoms.len() - 1);
        self.atoms.len() - 1
    }

    /// The cell giving native code access to module binding `sym`.
    fn cell(&mut self, sym: SymbolId, span: Span) -> R<ir::CellId> {
        if let Some(&id) = self.cell_ids.get(&sym) {
            return Ok(id);
        }
        let Some(b) = self.bindings.get(&sym) else {
            return reject(span, "unsupported reference to a module binding");
        };
        if b.written && b.exported {
            return reject(span, format!("`{}` is an exported binding that is reassigned", b.name));
        }
        self.cells.push(sym);
        self.cell_ids.insert(sym, self.cells.len() - 1);
        Ok(self.cells.len() - 1)
    }

    /// The slot of private name variable `var` of class `class`.
    fn private(&mut self, class: usize, var: &str) -> usize {
        let key = (class, var.to_string());
        if let Some(&id) = self.private_ids.get(&key) {
            return id;
        }
        self.privates.push(key.clone());
        self.private_ids.insert(key, self.privates.len() - 1);
        self.privates.len() - 1
    }

    fn class_uses(&self) -> Vec<ClassUse> {
        fn index(uses: &mut Vec<ClassUse>, class: usize) -> usize {
            match uses.iter().position(|u| u.class == class) {
                Some(i) => i,
                None => {
                    uses.push(ClassUse { class, vars: Vec::new() });
                    uses.len() - 1
                }
            }
        }
        let mut uses: Vec<ClassUse> = Vec::new();
        for &c in &self.homes_used {
            index(&mut uses, c);
        }
        for (slot, (c, var)) in self.privates.iter().enumerate() {
            let i = index(&mut uses, *c);
            uses[i].vars.push((var.clone(), slot));
        }
        uses
    }

    fn cell_infos(&self) -> Vec<CellInfo> {
        self.cells
            .iter()
            .map(|&sym| {
                let b = &self.bindings[&sym];
                let native = self.cands.iter().find(|c| c.symbol == Some(sym));
                // Declared by a native segment: interpreted code reaches it
                // through the cells object only (its declaration is gone).
                let segment = self.cands.iter().any(|c| c.cells.contains(&sym));
                let init = match (b.kind, native) {
                    _ if segment => CellInit::Native,
                    (BindingKind::Function | BindingKind::Import, _) => CellInit::Header,
                    (_, Some(c)) => match &c.site {
                        Site::ConstInit { init } => CellInit::Wrap(*init),
                        Site::Declaration { .. }
                        | Site::Method { .. }
                        | Site::Segment { .. }
                        | Site::Ctor { .. }
                        | Site::Private { .. } => {
                            CellInit::Header
                        }
                    },
                    (_, None) => match b.init {
                        Some(span) => CellInit::Wrap(span),
                        None => CellInit::After(b.stmt_end),
                    },
                };
                CellInfo {
                    symbol: sym,
                    name: b.name.clone(),
                    mutable: b.written || segment,
                    init,
                    hoisted_undefined: b.kind == BindingKind::Var,
                    constant: matches!(b.kind, BindingKind::Const | BindingKind::Import),
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum JumpKind {
    Loop,
    Switch,
}

struct FnCx<'x, 'm, 'a> {
    m: &'x mut ModuleCx<'m, 'a>,
    /// Which variables closures capture, for the whole top-level function.
    caps: Rc<CaptureInfo>,
    /// This function (its span start, as in `CaptureInfo`).
    key: u32,
    is_closure: bool,
    /// Methods: `this` is the receiver.
    has_this: bool,
    /// An ordinary function (not a method) with a `this`: still a
    /// constructor.
    ordinary_this: bool,
    is_async: bool,
    is_generator: bool,
    /// Methods (and the closures in them): their class, and whether the
    /// method is static.
    class: Option<(usize, bool)>,
    /// Native constructors: the instance to make, then the local holding
    /// it (`this`).
    ctor: Option<CtorInfo<'a>>,
    this_local: Option<ir::LocalId>,
    /// A base class constructor's fields whose values are arrow functions
    /// that see the instance (name, initializer, class): made once it and
    /// the box of `this` exist (see `new_instance`).
    deferred_fields: Vec<(String, &'a Expression<'a>, usize)>,
    /// The box holding `this` for the arrow functions inside that use it
    /// (in those arrows: the captured box).
    this_box: Option<ir::LocalId>,
    /// A derived class's constructor, until its `super(...)` call.
    derived: Option<CtorInfo<'a>>,
    /// The constructor of a class inside a function (`ir::Func::class_ctor`):
    /// `ExprKind::This` is new.target.
    class_ctor: bool,
    /// What the type annotations of parameters and variables say they hold.
    sym_hints: HashMap<SymbolId, ir::Hint>,
    /// Enclosing `for`-`in` loops: their key variable and iterator.
    for_in_keys: Vec<(SymbolId, ir::LocalId, ir::LocalId)>,
    /// The last parameter is a rest parameter.
    rest: bool,
    /// The function's `arguments` (its last parameter, see
    /// `ir::Func::arguments`), and its box for the arrows inside that use
    /// it (in those arrows: the captured box).
    args_local: Option<ir::LocalId>,
    args_box: Option<ir::LocalId>,
    /// The function reads its `arguments` only as `arguments.length`: its
    /// parameter is the count (`ir::Func::arguments_count`).
    args_count: bool,
    /// Functions that suspend: where a return value waits for `finally`s.
    ret_slot: Option<ir::LocalId>,
    locals: Vec<Ty>,
    mutable: Vec<bool>,
    boxed: Vec<bool>,
    local_names: Vec<Option<ir::AtomId>>,
    /// Closures: locals bound to the captured boxes, in data order.
    captures: Vec<ir::LocalId>,
    /// Locals that have been declared so far in program order. A reference to
    /// an unboxed local symbol not in here would hit its TDZ.
    sym_local: HashMap<SymbolId, ir::LocalId>,
    /// Locals known to need type `Any` (from previous attempts).
    widened: HashSet<SymbolId>,
    widen_request: Option<SymbolId>,
    /// Enclosing `break` / `continue` targets.
    jumps: Vec<(Option<String>, ir::LoopId, JumpKind)>,
    next_loop: ir::LoopId,
    /// Label attached to the loop statement about to be checked.
    label: Option<String>,
    returns: Vec<Ty>,
    /// The function is annotated to return `int`.
    ret_int: bool,
    /// Segments: module bindings they declare that live in cells.
    seg_cells: HashSet<SymbolId>,
    /// While checking a `for` update: the `int` loop counter it may step
    /// (`true`: up), proven to stay in range by the loop test.
    int_counter: Option<(ir::LocalId, bool)>,
    /// Depth of `finally` blocks being checked (no jumps out of them).
    in_finally: usize,
    /// Statements to run before the current statement: `await`s hoisted out
    /// of its expressions, and the operands that must be evaluated first.
    pre: Vec<Stmt>,
    /// Name for the next anonymous closure (`const f = () => ...`).
    name_hint: Option<String>,
}

/// Variables captured by nested functions within one top-level function.
#[derive(Default)]
struct CaptureInfo {
    /// Declared in one function, referenced from a nested one.
    captured: HashSet<SymbolId>,
    /// Per function (span start): captured variables of enclosing functions
    /// it uses, directly or through its own nested functions, in order.
    free: HashMap<u32, Vec<SymbolId>>,
    /// Functions whose `this` arrow functions inside them use.
    this_owners: HashSet<u32>,
    /// Arrow functions that use the `this` of an enclosing function
    /// (directly or through nested arrows).
    this_free: HashSet<u32>,
    /// Functions whose `arguments` they, or arrow functions inside them,
    /// use; of those, the ones whose arrows do; the arrows that use an
    /// enclosing function's (directly or through nested arrows).
    args_users: HashSet<u32>,
    args_owners: HashSet<u32>,
    args_free: HashSet<u32>,
}

/// `cells`: module bindings a segment declares that live in cells (not
/// captured: closures access the cell).
fn capture_info<'a>(
    node: FnNode<'a>,
    scoping: &Scoping,
    cells: &HashSet<SymbolId>,
    fields: &[&'a Expression<'a>],
) -> CaptureInfo {
    use oxc::ast_visit::{Visit, walk};
    use oxc::syntax::scope::ScopeFlags;
    struct V<'s> {
        scoping: &'s Scoping,
        cells: &'s HashSet<SymbolId>,
        collect_refs: bool,
        /// The enclosing functions, and whether each is an arrow (or the
        /// module code: neither has a `this` of its own).
        stack: Vec<u32>,
        arrows: Vec<bool>,
        owner: HashMap<SymbolId, u32>,
        info: CaptureInfo,
    }
    // (The owner of a top-level function's own name: none of the functions.)
    const OUTSIDE: u32 = u32::MAX;
    impl<'a> Visit<'a> for V<'_> {
        fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
            // A declaration's name belongs to the enclosing function.
            if it.is_declaration()
                && let Some(id) = &it.id
            {
                let cur = self.stack.last().copied().unwrap_or(OUTSIDE);
                self.owner.entry(id.symbol_id.get().unwrap()).or_insert(cur);
            }
            self.stack.push(it.span.start);
            self.arrows.push(false);
            walk::walk_function(self, it, flags);
            self.stack.pop();
            self.arrows.pop();
        }
        fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
            self.stack.push(it.span.start);
            self.arrows.push(true);
            walk::walk_arrow_function_expression(self, it);
            self.stack.pop();
            self.arrows.pop();
        }
        fn visit_class(&mut self, c: &Class<'a>) {
            // A class inside the function: its instance fields are
            // initialized in its constructor (its own, or its default one,
            // at the class's position), with its `this`.
            let ctor = c
                .body
                .body
                .iter()
                .find_map(|el| match el {
                    ClassElement::MethodDefinition(m)
                        if m.kind == MethodDefinitionKind::Constructor && m.value.body.is_some() =>
                    {
                        Some(m.value.span.start)
                    }
                    _ => None,
                })
                .unwrap_or(c.span.start);
            for d in &c.decorators {
                self.visit_decorator(d);
            }
            if let Some(id) = &c.id {
                self.visit_binding_identifier(id);
            }
            if let Some(h) = &c.heritage {
                self.visit_expression(&h.expression);
            }
            for el in &c.body.body {
                match el {
                    ClassElement::PropertyDefinition(p) if !p.r#static => {
                        if p.computed {
                            self.visit_property_key(&p.key);
                        }
                        if let Some(v) = &p.value {
                            self.stack.push(ctor);
                            self.arrows.push(false);
                            self.visit_expression(v);
                            self.stack.pop();
                            self.arrows.pop();
                        }
                    }
                    el => self.visit_class_element(el),
                }
            }
        }
        fn visit_this_expression(&mut self, _: &ThisExpression) {
            if !self.collect_refs {
                return;
            }
            // The arrows between `this` and the function it belongs to.
            let Some(k) = self.arrows.iter().rposition(|&a| !a) else { return };
            if k + 1 < self.stack.len() {
                self.info.this_owners.insert(self.stack[k]);
                self.info.this_free.extend(&self.stack[k + 1..]);
            }
        }
        fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
            if !self.collect_refs
                && let (Some(sym), Some(&cur)) = (it.symbol_id.get(), self.stack.last())
                && !self.cells.contains(&sym)
            {
                self.owner.entry(sym).or_insert(cur);
            }
        }
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            if !self.collect_refs {
                return;
            }
            // `arguments`: as `this`.
            if is_arguments(it, self.scoping) {
                if let Some(k) = self.arrows.iter().rposition(|&a| !a) {
                    self.info.args_users.insert(self.stack[k]);
                    if k + 1 < self.stack.len() {
                        self.info.args_owners.insert(self.stack[k]);
                        self.info.args_free.extend(&self.stack[k + 1..]);
                    }
                }
                return;
            }
            // (Types are erased: a type parameter in an annotation is not
            // captured.)
            let Some(sym) = it
                .reference_id
                .get()
                .map(|r| self.scoping.get_reference(r))
                .filter(|r| r.is_value())
                .and_then(|r| r.symbol_id())
            else {
                return;
            };
            let Some(&owner) = self.owner.get(&sym) else { return };
            if owner == OUTSIDE || self.stack.last() == Some(&owner) {
                return;
            }
            self.info.captured.insert(sym);
            for &f in self.stack.iter().rev() {
                if f == owner {
                    break;
                }
                let free = self.info.free.entry(f).or_default();
                if !free.contains(&sym) {
                    free.push(sym);
                }
            }
        }
    }
    let mut v = V {
        scoping,
        cells,
        collect_refs: false,
        stack: Vec::new(),
        arrows: Vec::new(),
        owner: HashMap::new(),
        info: Default::default(),
    };
    for pass in [false, true] {
        v.collect_refs = pass;
        match node {
            FnNode::Function(f) => v.visit_function(f, ScopeFlags::Function),
            FnNode::Arrow(a) => v.visit_arrow_function_expression(a),
            FnNode::Segment(span, stmts) => {
                v.stack.push(span.start);
                v.arrows.push(true);
                stmts.iter().for_each(|s| v.visit_statement(s));
                v.stack.pop();
                v.arrows.pop();
            }
        }
        // A constructor's field initializers: in it, with its `this` (a
        // default constructor's too).
        if !fields.is_empty() {
            v.stack.push(node.span().start);
            v.arrows.push(false);
            fields.iter().for_each(|e| v.visit_expression(e));
            v.stack.pop();
            v.arrows.pop();
        }
    }
    v.info
}

/// Whether `id` is a function's `arguments` (no binding of that name).
fn is_arguments(id: &IdentifierReference, scoping: &Scoping) -> bool {
    id.name == "arguments" && id.reference_id.get().is_some_and(|r| scoping.get_reference(r).symbol_id().is_none())
}

/// Whether function `node` uses its own `arguments` (or arrow functions
/// inside it do).
fn uses_arguments(node: FnNode, scoping: &Scoping) -> bool {
    use oxc::ast_visit::{Visit, walk};
    use oxc::syntax::scope::ScopeFlags;
    struct V<'s> {
        scoping: &'s Scoping,
        found: bool,
        nested: bool,
    }
    impl<'a> Visit<'a> for V<'_> {
        fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
            // (Another function's own.)
            if !self.nested {
                self.nested = true;
                walk::walk_function(self, it, flags);
            }
        }
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            self.found |= is_arguments(it, self.scoping);
        }
    }
    let FnNode::Function(f) = node else { return false };
    let mut v = V { scoping, found: false, nested: false };
    v.visit_function(f, ScopeFlags::Function);
    v.found
}

/// Whether function `node` reads its own `arguments` only as
/// `arguments.length` (not written, deleted, or used by its arrow functions).
fn arguments_length_only(node: FnNode, scoping: &Scoping) -> bool {
    use oxc::ast_visit::{Visit, walk};
    use oxc::syntax::scope::ScopeFlags;
    struct V<'s> {
        scoping: &'s Scoping,
        other: bool,
        nested: bool,
        arrows: usize,
        writing: usize,
    }
    impl<'a> Visit<'a> for V<'_> {
        fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
            // (Another function's own.)
            if !self.nested {
                self.nested = true;
                walk::walk_function(self, it, flags);
            }
        }
        fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
            self.arrows += 1;
            walk::walk_arrow_function_expression(self, it);
            self.arrows -= 1;
        }
        fn visit_assignment_target(&mut self, it: &AssignmentTarget<'a>) {
            self.writing += 1;
            walk::walk_assignment_target(self, it);
            self.writing -= 1;
        }
        fn visit_simple_assignment_target(&mut self, it: &SimpleAssignmentTarget<'a>) {
            self.writing += 1;
            walk::walk_simple_assignment_target(self, it);
            self.writing -= 1;
        }
        fn visit_unary_expression(&mut self, it: &UnaryExpression<'a>) {
            let delete = it.operator == UnaryOperator::Delete;
            self.writing += delete as usize;
            walk::walk_unary_expression(self, it);
            self.writing -= delete as usize;
        }
        fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
            if let Expression::Identifier(id) = it.object.without_parentheses()
                && is_arguments(id, self.scoping)
                && it.property.name == "length"
                && self.arrows == 0
                && self.writing == 0
                && !it.optional
            {
                return;
            }
            // (The object of a member written is read.)
            let writing = std::mem::take(&mut self.writing);
            walk::walk_static_member_expression(self, it);
            self.writing = writing;
        }
        fn visit_computed_member_expression(&mut self, it: &ComputedMemberExpression<'a>) {
            let writing = std::mem::take(&mut self.writing);
            walk::walk_computed_member_expression(self, it);
            self.writing = writing;
        }
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            self.other |= is_arguments(it, self.scoping);
        }
    }
    let FnNode::Function(f) = node else { return false };
    let mut v = V { scoping, other: false, nested: false, arrows: 0, writing: 0 };
    v.visit_function(f, ScopeFlags::Function);
    !v.other
}

/// A class member's key that is a name known statically.
fn class_key(key: &PropertyKey, computed: bool) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) if !computed => Some(id.name.to_string()),
        PropertyKey::StringLiteral(s) if !s.lone_surrogates => Some(s.value.to_string()),
        PropertyKey::NumericLiteral(n) if n.value.fract() == 0.0 && n.value >= 0.0 && n.value < 4294967295.0 => {
            Some(format!("{}", n.value as u64))
        }
        _ => None,
    }
}

/// Expressions evaluated in order, the last one's value the result.
fn seq_expr(xs: Vec<Expr>) -> Expr {
    seq(xs)
}

/// `e` as a JS value.
fn any(e: Expr) -> Expr {
    match e.ty {
        Ty::Any => e,
        Ty::F64 | Ty::I32 | Ty::Bool | Ty::MaybeF64 => Expr::new(Ty::Any, ExprKind::Box(Box::new(e))),
        // Already a JSValue.
        Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_) | Ty::StructOrUndef(_) => Expr { ty: Ty::Any, ..e },
        Ty::Void => Expr::new(Ty::Any, ExprKind::Void(Box::new(e))),
    }
}

fn undefined() -> Expr {
    Expr::new(Ty::Any, ExprKind::Undefined)
}

fn join(a: Ty, b: Ty) -> Ty {
    match (a, b) {
        _ if a == b => a,
        (Ty::I32, Ty::F64) | (Ty::F64, Ty::I32) => Ty::F64,
        _ => Ty::Any,
    }
}

fn coerce(e: Expr, ty: Ty) -> Expr {
    if e.ty == ty || (ty == Ty::F64 && e.ty == Ty::I32) {
        e
    } else if ty == Ty::I32 {
        debug_assert!(int_literal(&e).is_some(), "an int local assigned a non-int");
        Expr { ty: Ty::I32, ..e }
    } else {
        debug_assert_eq!(ty, Ty::Any);
        any(e)
    }
}

fn seq(mut xs: Vec<Expr>) -> Expr {
    if xs.len() == 1 {
        return xs.pop().unwrap();
    }
    let ty = xs.last().unwrap().ty;
    Expr::new(ty, ExprKind::Seq(xs))
}

/// How a binding pattern declares its names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BindKind {
    Let,
    Const,
    /// Hoisted: the names exist already.
    Var,
}

/// What an assignment writes to.
enum Place {
    Local(ir::LocalId),
    Cell(ir::CellId),
    Global(ir::AtomId),
    /// Object and key in locals: temporaries, or (the object) a plain local
    /// used in place, which `assign` copies when the value may reassign it.
    Prop(ir::LocalId, ir::AtomId),
    Elem(ir::LocalId, ir::LocalId),
    /// `obj.#name` (with the `#`).
    Private(ir::LocalId, String),
    /// `super[key]`: the home slot, and the key in a local.
    Super(usize, ir::LocalId),
}

impl<'x, 'm, 'a> FnCx<'x, 'm, 'a> {
    fn new(
        m: &'x mut ModuleCx<'m, 'a>,
        caps: Rc<CaptureInfo>,
        key: u32,
        is_closure: bool,
        widened: HashSet<SymbolId>,
    ) -> Self {
        FnCx {
            m,
            caps,
            key,
            is_closure,
            has_this: false,
            ordinary_this: false,
            ctor: None,
            this_local: None,
            deferred_fields: Vec::new(),
            this_box: None,
            derived: None,
            class_ctor: false,
            sym_hints: HashMap::new(),
            is_async: false,
            is_generator: false,
            ret_slot: None,
            rest: false,
            args_local: None,
            args_count: false,
            args_box: None,
            for_in_keys: Vec::new(),
            class: None,
            locals: Vec::new(),
            mutable: Vec::new(),
            boxed: Vec::new(),
            local_names: Vec::new(),
            captures: Vec::new(),
            sym_local: HashMap::new(),
            widened,
            widen_request: None,
            jumps: Vec::new(),
            next_loop: 0,
            label: None,
            returns: Vec::new(),
            ret_int: false,
            seg_cells: HashSet::new(),
            int_counter: None,
            in_finally: 0,
            name_hint: None,
            pre: Vec::new(),
        }
    }

    fn function(&mut self, node: FnNode<'a>, name: String, params: Vec<ParamSpec<'a>>) -> R<ir::Func> {
        self.collect_hints(node);
        let annotated = match node {
            FnNode::Function(f) => f.return_type.as_ref(),
            FnNode::Arrow(a) => a.return_type.as_ref(),
            FnNode::Segment(..) => None,
        };
        let ret_hint = match &self.ctor {
            Some(c) => Some(ir::Hint::Class(c.home)),
            None => {
                annotated.and_then(|a| hint_name(&a.type_annotation)).and_then(|h| resolve_hint(&h, self.m.classes))
            }
        };
        self.is_async = node.is_async();
        self.is_generator = matches!(node, FnNode::Function(f) if f.generator);
        let mut abi = Vec::new();
        let mut prologue = Vec::new();
        let mut boxed_params = Vec::new();
        // (A default derived constructor's parameters are only passed on.)
        let forward = self.ctor.as_ref().is_some_and(|c| c.forward);
        for &(pat, ty, _) in &params {
            // A destructured parameter is a hidden local, bound below.
            let sym = match pat {
                BindingPattern::BindingIdentifier(id) if !forward => id.symbol_id.get(),
                _ => None,
            };
            let local_ty = if sym.is_none_or(|s| self.widened.contains(&s)) { Ty::Any } else { ty };
            let l = self.temp(local_ty);
            abi.push(ty);
            if let Some(sym) = sym {
                self.sym_local.insert(sym, l);
                if self.caps.captured.contains(&sym) {
                    boxed_params.push((sym, l));
                }
            }
        }
        // A default constructor passing all its arguments on: them, in an
        // array.
        let forward_all = forward && self.ctor.as_ref().is_some_and(|c| c.dynamic);
        if forward_all {
            self.temp(Ty::Any);
            abi.push(Ty::Any);
            // (A class inside a function: its JS entry makes the array.)
            if self.ctor.as_ref().is_some_and(|c| c.local) {
                self.rest = true;
            }
        }
        // The arguments object, last (made by the JS entry).
        if matches!(node, FnNode::Function(_)) && self.caps.args_users.contains(&self.key) {
            if self.ctor.is_some() {
                return reject(node.span(), "constructors using `arguments` are not supported");
            }
            let l = self.temp(Ty::Any);
            abi.push(Ty::Any);
            self.args_local = Some(l);
            self.args_count = !self.caps.args_owners.contains(&self.key) && arguments_length_only(node, self.m.scoping);
        }
        if self.suspends() {
            self.ret_slot = Some(self.temp(Ty::Any));
        }
        let mut param_props = Vec::new();
        // A base class inside a function: its instance is made once the
        // captured variables its fields may use are bound (below).
        let mut local_base = false;
        if let Some(ctor) = self.ctor.take() {
            prologue.extend(self.ctor_checks(&ctor, params.len())?);
            if ctor.local && !ctor.dynamic {
                self.this_local = Some(self.temp(Ty::Any));
                // (Its parameter properties are assigned after the
                // parameters' defaults.)
                param_props = ctor.param_props.clone();
                self.derived = Some(CtorInfo { param_props: Vec::new(), ..ctor });
                local_base = true;
            } else if ctor.parent.is_some() || ctor.dynamic {
                // The instance comes from `super(...)`.
                self.this_local = Some(self.temp(Ty::Any));
                self.derived = Some(ctor);
            } else {
                // The instance, first: its fields' initializers run before
                // the parameters' defaults.
                prologue.extend(self.new_instance(&ctor)?);
                param_props = ctor.param_props;
            }
        }
        // Closures: locals bound to the captured boxes.
        if self.is_closure {
            let free = self.caps.free.get(&self.key).cloned().unwrap_or_default();
            for sym in free {
                let l = self.declare_boxed(sym);
                self.captures.push(l);
            }
            // (An arrow's `this`, then its `arguments`, last.)
            if self.caps.this_free.contains(&self.key) {
                let l = self.this_box_local();
                self.captures.push(l);
                self.this_box = Some(l);
            }
            if self.caps.args_free.contains(&self.key) {
                let l = self.args_box_local();
                self.captures.push(l);
                self.args_box = Some(l);
            }
        }
        // Captured parameters move into boxes.
        let mut new_boxes = Vec::new();
        let mut inits = Vec::new();
        // `this`, when arrows inside use it (a derived class's: once
        // `super(...)` has made it).
        if self.caps.this_owners.contains(&self.key) && self.derived.is_none() {
            let this = if self.has_this {
                Some(Expr::new(Ty::Any, ExprKind::This))
            } else {
                self.this_local.map(|l| self.local(l))
            };
            if let Some(this) = this {
                let b = self.this_box_local();
                new_boxes.push(b);
                inits.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(b, Box::new(this)))));
                self.this_box = Some(b);
            }
        }
        if let Some(a) = self.args_local
            && self.caps.args_owners.contains(&self.key)
        {
            let b = self.args_box_local();
            new_boxes.push(b);
            inits.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(b, Box::new(self.local(a))))));
            self.args_box = Some(b);
        }
        for (sym, p) in boxed_params {
            let b = self.declare_boxed(sym);
            new_boxes.push(b);
            inits.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(b, Box::new(any(self.local(p)))))));
        }
        if !new_boxes.is_empty() {
            prologue.push(Stmt::NewBoxes(new_boxes));
            prologue.extend(inits);
        }
        if local_base {
            let target = Expr::new(Ty::Any, ExprKind::This);
            let inst = Expr::new(Ty::Any, ExprKind::NewFromTarget(Box::new(target)));
            self.super_init(inst)?;
            prologue.append(&mut self.pre);
        }
        let deferred = self.deferred_fields()?;
        prologue.extend(deferred);
        // Default values and destructuring, in order (each parameter may see
        // the previous ones).
        for (i, &(pat, _, default)) in params.iter().enumerate().filter(|_| !forward) {
            let l = match pat {
                BindingPattern::BindingIdentifier(id) => self.sym_local[&id.symbol_id.get().unwrap()],
                _ => i,
            };
            if let Some(d) = default {
                if let BindingPattern::BindingIdentifier(id) = pat {
                    self.name_hint = Some(id.name.to_string());
                }
                let v = self.expr(d)?;
                // An `int` parameter's default is a store into an int.
                let int = matches!(pat, BindingPattern::BindingIdentifier(id) if self.wraps(id.symbol_id.get()));
                let v = any(if int && v.ty != Ty::I32 { wrap(v) } else { v });
                self.name_hint = None;
                let missing = Expr::new(
                    Ty::Bool,
                    ExprKind::AnyCompare(AnyCmpOp::StrictEq, Box::new(self.local(l)), Box::new(undefined())),
                );
                let set = Stmt::Expr(Expr::new(Ty::Any, ExprKind::SetLocal(l, Box::new(v))));
                prologue.push(Stmt::If(missing, vec![set], vec![]));
            }
            if !matches!(pat, BindingPattern::BindingIdentifier(_)) {
                let v = self.local(i);
                self.bind_pattern(pat, v, BindKind::Let, &mut prologue)?;
            }
        }
        // A base class's parameter properties are assigned first thing.
        for (name, sym) in param_props {
            let v = self.local(self.sym_local[&sym]);
            prologue.push(self.param_prop(&name, v));
        }
        if forward_all {
            let call = self.super_new(None, Some(self.local(0)));
            self.super_init(call)?;
            prologue.append(&mut self.pre);
        } else if forward {
            let args = (0..params.len()).map(|l| self.local(l)).collect();
            self.super_call(args, node.span())?;
            prologue.append(&mut self.pre);
        }
        let body = match node {
            FnNode::Segment(_, stmts) => {
                prologue.extend(self.hoist_vars(stmts)?);
                self.block(stmts)?
            }
            FnNode::Function(f) if f.generator => {
                // Parameters and declarations are set up when the generator
                // is created; the body runs from the first `next`.
                let body = f.body.as_ref().unwrap();
                prologue.extend(self.hoist_vars(&body.statements)?);
                prologue.extend(self.block_prologue(&body.statements)?);
                prologue.push(Stmt::Yield {
                    value: undefined(),
                    kind: ir::YieldKind::Initial,
                    target: ir::AwaitTarget::Discard,
                });
                self.stmts(&body.statements)?
            }
            FnNode::Function(f) => {
                let body = f.body.as_ref().unwrap();
                prologue.extend(self.hoist_vars(&body.statements)?);
                self.block(&body.statements)?
            }
            FnNode::Arrow(a) => match &a.body {
                ArrowFunctionBody::FunctionBody(b) => {
                    prologue.extend(self.hoist_vars(&b.statements)?);
                    self.block(&b.statements)?
                }
                body => {
                    // Concise body: `(x) => expr` returns `expr`.
                    let e = body.as_expression().expect("concise arrow body is an expression");
                    let x = self.expr(e)?;
                    let x = if self.ret_int { self.int_return(Some(x)) } else { x };
                    self.returns.push(x.ty);
                    let mut body = std::mem::take(&mut self.pre);
                    body.push(Stmt::Return(Some(x)));
                    body
                }
            },
        };
        let mut body = body;
        // A derived class's `super(...)` statement: the instance made (and
        // its fields) in the body itself, for `ir::fold_new_instances`.
        if let Some(this) = self.this_local
            && let Some(k) = body.iter().position(|s| {
                matches!(s, Stmt::Block(b) if matches!(b.first(),
                    Some(Stmt::Expr(Expr { kind: ExprKind::InitLocal(l, _), .. })) if *l == this))
            })
        {
            let Stmt::Block(mut b) = body.remove(k) else { unreachable!() };
            if matches!(b.last(), Some(Stmt::Expr(Expr { kind: ExprKind::Local(l), .. })) if *l == this) {
                b.pop();
            }
            body.splice(k..k, b);
        }
        if let Some(l) = self.this_local
            && completes_list(&body)
        {
            body.push(Stmt::Return(Some(self.local(l))));
            self.returns.push(Ty::Any);
        }
        let mut ret = None;
        for &t in &self.returns {
            ret = Some(ret.map_or(t, |r| join(r, t)));
        }
        if completes_list(&body) {
            ret = Some(ret.map_or(Ty::Void, |r| join(r, Ty::Void)));
        }
        if !self.is_closure && self.m.any_ret.contains(&self.key) {
            ret = Some(Ty::Any);
        }
        // `undefined` is only encoded within a function; generators return
        // iterator results.
        let ret = match ret.unwrap_or(Ty::Void) {
            _ if self.is_generator => Ty::Any,
            Ty::MaybeF64 => Ty::Any,
            t => t,
        };
        // Values returned from a function of type `Any` must be boxed.
        let body = if ret == Ty::Any { box_returns(body) } else { body };
        prologue.extend(body);
        // Parameters before the first default, not counting a rest one.
        let fixed = params.len().saturating_sub(self.rest as usize);
        let js_length = params[..fixed].iter().take_while(|p| p.2.is_none()).count();
        // (A named function expression's `.name` is its own name, not that
        // of the binding it initializes.)
        let name = match node {
            FnNode::Function(f) if let Some(id) = &f.id => id.name.to_string(),
            _ => name,
        };
        let constructor = matches!(node, FnNode::Function(f) if !f.r#async && !f.generator)
            && (!self.has_this || self.ordinary_this);
        let mut func = ir::Func {
            constructor,
            name,
            js_length,
            params: abi,
            ret,
            locals: std::mem::take(&mut self.locals),
            boxed: std::mem::take(&mut self.boxed),
            local_names: std::mem::take(&mut self.local_names),
            value_captures: vec![false; self.captures.len()],
            captures: std::mem::take(&mut self.captures),
            is_closure: self.is_closure,
            has_this: self.has_this || self.class_ctor,
            class_ctor: self.class_ctor,
            is_async: self.is_async,
            frameless: false,
            source: None,
            is_generator: self.is_generator,
            rest: self.rest,
            arguments: self.args_local.is_some(),
            arguments_count: self.args_count,
            ret_slot: self.ret_slot,
            class: if self.has_this && !self.is_closure { self.class } else { None },
            hints: self.sym_local.iter().filter_map(|(sym, &l)| Some((l, *self.sym_hints.get(sym)?))).collect(),
            ret_hint,
            body: prologue,
        };
        func.frameless = func.is_async && !func.is_generator && !ir::has_await(&func.body);
        ir::scalar_replace(&mut func);
        ir::virtualize_objects(&mut func);
        ir::scalar_replace_structs(&mut func, self.m.layouts);
        ir::promote_struct_fields(&mut func, self.m.layouts);
        ir::number_locals(&mut func);
        Ok(func)
    }

    /// Whether the function can suspend (`await`, `yield`): then only its
    /// locals survive across suspension points.
    fn suspends(&self) -> bool {
        self.is_async || self.is_generator
    }

    /// A local for `sym`: boxed if a closure captures it.
    fn declare(&mut self, sym: SymbolId, ty: Ty, mutable: bool) -> ir::LocalId {
        if self.caps.captured.contains(&sym) {
            let l = self.declare_boxed(sym);
            self.mutable[l] = mutable;
            return l;
        }
        let id = self.temp(ty);
        self.mutable[id] = mutable;
        self.sym_local.insert(sym, id);
        id
    }

    fn declare_boxed(&mut self, sym: SymbolId) -> ir::LocalId {
        let l = self.temp(Ty::Any);
        self.boxed[l] = true;
        let name = self.m.scoping.symbol_name(sym).to_string();
        self.local_names[l] = Some(self.m.atom(&name));
        self.mutable[l] = !self.m.scoping.symbol_flags(sym).is_const_variable();
        self.sym_local.insert(sym, l);
        l
    }

    /// A box for `this` (arrows' lexical `this`).
    fn this_box_local(&mut self) -> ir::LocalId {
        let l = self.temp(Ty::Any);
        self.boxed[l] = true;
        self.mutable[l] = false;
        self.local_names[l] = Some(self.m.atom("this"));
        l
    }

    /// The same, for `arguments`.
    fn args_box_local(&mut self) -> ir::LocalId {
        let l = self.temp(Ty::Any);
        self.boxed[l] = true;
        self.mutable[l] = false;
        self.local_names[l] = Some(self.m.atom("arguments"));
        l
    }

    /// A hidden local.
    fn temp(&mut self, ty: Ty) -> ir::LocalId {
        self.locals.push(ty);
        self.mutable.push(true);
        self.boxed.push(false);
        self.local_names.push(None);
        self.locals.len() - 1
    }

    fn local(&self, l: ir::LocalId) -> Expr {
        Expr::new(self.locals[l], ExprKind::Local(l))
    }

    fn set_local(&mut self, sym: Option<SymbolId>, l: ir::LocalId, v: Expr, span: Span) -> R<Expr> {
        let ty = self.locals[l];
        if ty == Ty::I32 {
            let v = self.int_value(l, v);
            return Ok(Expr::new(Ty::I32, ExprKind::SetLocal(l, Box::new(v))));
        }
        if ty == Ty::F64 && v.ty == Ty::I32 {
            return Ok(Expr::new(Ty::F64, ExprKind::SetLocal(l, Box::new(v))));
        }
        if v.ty != ty && ty != Ty::Any {
            // A local of an unboxed type gets another type: make it `Any`
            // and check the function again.
            let Some(sym) = sym else { unreachable!("hidden locals are `Any`") };
            self.widen_request = Some(sym);
            return Err(Reject { span, msg: "retry".into(), retry: true, callee: false });
        }
        Ok(Expr::new(ty, ExprKind::SetLocal(l, Box::new(coerce(v, ty)))))
    }

    /// `v` as the new value of `int` local `l`: as is if it is an int32
    /// (an `int` value, a literal, the loop counter's `l + 1` / `l - 1`),
    /// else wrapped (the `ints` pass rejects values that are no integers).
    fn int_value(&self, l: ir::LocalId, v: Expr) -> Expr {
        if v.ty == Ty::I32 {
            return v;
        }
        if let Some(c) = int_literal(&v) {
            return Expr::new(Ty::I32, ExprKind::Num(c as f64));
        }
        if let ExprKind::Binary(op @ (BinOp::Add | BinOp::Sub), x, y) = &v.kind
            && matches!(x.kind, ExprKind::Local(m) if m == l)
            && int_literal(y) == Some(1)
            && self.int_counter == Some((l, *op == BinOp::Add))
        {
            return Expr { ty: Ty::I32, ..v };
        }
        wrap(v)
    }

    /// The `int` loop counter of a `for` loop with this test and body, if
    /// its update may step it: `i < n` (`n` an int) for `i++`, `i > n` for
    /// `i--`, with no other assignment to `i` in the body. The test bounds
    /// `i` whenever the update runs.
    fn int_counter_of(&self, test: &LoopTest, body: &[Stmt]) -> Option<(ir::LocalId, bool)> {
        if !test.pre.is_empty() {
            return None;
        }
        let ExprKind::Compare(op, x, y) = &test.test.as_ref()?.kind else { return None };
        let ExprKind::Local(l) = x.kind else { return None };
        if self.locals[l] != Ty::I32 {
            return None;
        }
        let bound = y.ty == Ty::I32 || int_literal(y).is_some();
        let lit = int_literal(y);
        let up = match op {
            CmpOp::Lt => bound,
            CmpOp::Le => lit.is_some_and(|k| k < i32::MAX),
            _ => false,
        };
        let down = match op {
            CmpOp::Gt => bound,
            CmpOp::Ge => lit.is_some_and(|k| k > i32::MIN),
            _ => false,
        };
        if !up && !down {
            return None;
        }
        let mut assigned = false;
        ir::visit_stmts(body, &mut |e| assigned |= ir::assigns_local(e, l));
        (!assigned).then_some((l, up))
    }

    /// Whether stores into this binding wrap like `| 0` (it is declared
    /// `int`).
    fn wraps(&self, sym: Option<SymbolId>) -> bool {
        sym.is_some_and(|s| self.m.int_syms.contains(&s))
    }

    /// A returned value in a function annotated to return `int`.
    fn int_return(&self, v: Option<Expr>) -> Expr {
        match v {
            Some(v) if v.ty == Ty::I32 => v,
            Some(v) if int_literal(&v).is_some() => Expr::new(Ty::I32, ExprKind::Num(int_literal(&v).unwrap() as f64)),
            // (`return;` is rejected by the `ints` pass.)
            v => wrap(v.unwrap_or_else(undefined)),
        }
    }

    /// `var` declarations are function-scoped: declare them up front, as
    /// `Any` locals holding `undefined` (in boxes, if captured).
    fn hoist_vars(&mut self, stmts: &'a [Statement<'a>]) -> R<Vec<Stmt>> {
        struct Vars(Vec<SymbolId>);
        impl<'a> oxc::ast_visit::Visit<'a> for Vars {
            fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
                if it.kind == VariableDeclarationKind::Var {
                    for d in &it.declarations {
                        for id in d.id.get_binding_identifiers() {
                            self.0.push(id.symbol_id.get().unwrap());
                        }
                    }
                }
                oxc::ast_visit::walk::walk_variable_declaration(self, it);
            }
            // Nested functions have their own `var` scope.
            fn visit_function(&mut self, _: &Function<'a>, _: oxc::syntax::scope::ScopeFlags) {}
            fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}
        }
        let mut v = Vars(Vec::new());
        for s in stmts {
            oxc::ast_visit::Visit::visit_statement(&mut v, s);
        }
        let mut boxes = Vec::new();
        for sym in v.0 {
            if !self.sym_local.contains_key(&sym) && !self.seg_cells.contains(&sym) {
                let l = self.declare(sym, Ty::Any, true);
                if self.boxed[l] {
                    boxes.push(l);
                }
            }
        }
        let mut out = Vec::new();
        if !boxes.is_empty() {
            out.push(Stmt::NewBoxes(boxes.clone()));
            for l in boxes {
                out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(l, Box::new(undefined())))));
            }
        }
        Ok(out)
    }

    // ---- statements ----

    /// A block: fresh boxes for its captured lexical declarations, hoisted
    /// function declarations, then its statements.
    fn block(&mut self, list: &'a [Statement<'a>]) -> R<Vec<Stmt>> {
        let mut out = self.block_prologue(list)?;
        out.extend(self.stmts(list)?);
        Ok(out)
    }

    fn block_prologue(&mut self, list: &'a [Statement<'a>]) -> R<Vec<Stmt>> {
        let mut boxes = Vec::new();
        let mut funcs = Vec::new();
        for s in list {
            let decl = match s {
                Statement::ExportDeclaration(_) => None,
                s => s.as_declaration(),
            };
            match decl {
                Some(Declaration::VariableDeclaration(v)) if v.kind != VariableDeclarationKind::Var => {
                    for d in &v.declarations {
                        for id in d.id.get_binding_identifiers() {
                            let sym = id.symbol_id.get().unwrap();
                            if self.caps.captured.contains(&sym) {
                                boxes.push(self.declare_boxed(sym));
                            }
                        }
                    }
                }
                Some(Declaration::FunctionDeclaration(f)) if f.body.is_some() => {
                    let id = f.id.as_ref().unwrap();
                    let sym = id.symbol_id.get().unwrap();
                    let l = self.declare(sym, Ty::Any, true);
                    if self.boxed[l] {
                        boxes.push(l);
                    }
                    funcs.push((l, f));
                }
                Some(Declaration::ClassDeclaration(c)) if !c.declare => {
                    // (Captured: its box exists from the block's start, in
                    // its TDZ until the class is made.)
                    let id = c.id.as_ref().unwrap();
                    let sym = id.symbol_id.get().unwrap();
                    if self.caps.captured.contains(&sym) {
                        boxes.push(self.declare_boxed(sym));
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        if !boxes.is_empty() {
            out.push(Stmt::NewBoxes(boxes));
        }
        // Function declarations are initialized when the block is entered.
        for (l, f) in funcs {
            let name = f.id.as_ref().unwrap().name.to_string();
            let c = self.closure(FnNode::Function(f), name)?;
            out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(l, Box::new(c)))));
        }
        Ok(out)
    }

    fn stmts(&mut self, list: &'a [Statement<'a>]) -> R<Vec<Stmt>> {
        let mut out = Vec::with_capacity(list.len());
        for s in list {
            if let Some(s) = self.stmt(s)? {
                out.push(s);
            }
        }
        Ok(out)
    }

    fn body(&mut self, s: &'a Statement<'a>) -> R<Vec<Stmt>> {
        Ok(match self.stmt(s)? {
            Some(Stmt::Block(b)) => b,
            Some(s) => vec![s],
            None => vec![],
        })
    }

    fn stmt(&mut self, s: &'a Statement<'a>) -> R<Option<Stmt>> {
        // `await`s hoisted out of this statement's expressions run first.
        let outer = std::mem::take(&mut self.pre);
        let r = self.stmt_inner(s);
        let pre = std::mem::replace(&mut self.pre, outer);
        let st = r?;
        if pre.is_empty() {
            return Ok(st);
        }
        let mut block = pre;
        block.extend(st);
        Ok(Some(Stmt::Block(block)))
    }

    fn stmt_inner(&mut self, s: &'a Statement<'a>) -> R<Option<Stmt>> {
        let label = self.label.take();
        if label.is_some()
            && !matches!(
                s,
                Statement::WhileStatement(_)
                    | Statement::DoWhileStatement(_)
                    | Statement::ForStatement(_)
                    | Statement::ForOfStatement(_)
                    | Statement::ForInStatement(_)
            )
        {
            return reject(s.span(), "labels are only supported on loops");
        }
        Ok(Some(match s {
            Statement::EmptyStatement(_) => return Ok(None),
            // Hoisted to the start of the block.
            Statement::FunctionDeclaration(f) if f.body.is_some() => return Ok(None),
            Statement::FunctionDeclaration(_) => return Ok(None),
            Statement::ExpressionStatement(e) => self.expr_stmt(&e.expression)?,
            Statement::BlockStatement(b) => Stmt::Block(self.block(&b.body)?),
            Statement::VariableDeclaration(v) => Stmt::Block(self.var_decl(v)?),
            Statement::ClassDeclaration(c) if c.declare => return Ok(None),
            Statement::ClassDeclaration(c) => {
                let id = c.id.as_ref().unwrap();
                let sym = id.symbol_id.get().unwrap();
                // (Its name inside it is the binding: never reassigned.)
                if self.m.scoping.get_resolved_references(sym).any(|r| r.is_write()) {
                    return reject(id.span, format!("`{}` is reassigned", id.name));
                }
                let v = self.class_value(c, id.name.to_string())?;
                let l = match self.sym_local.get(&sym) {
                    Some(&l) if self.boxed[l] => l,
                    _ => self.declare(sym, Ty::Any, false),
                };
                Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(l, Box::new(v))))
            }
            Statement::IfStatement(i) => {
                let test = self.cond(&i.test)?;
                let then = self.body(&i.consequent)?;
                let els = match &i.alternate {
                    Some(a) => self.body(a)?,
                    None => vec![],
                };
                Stmt::If(test, then, els)
            }
            Statement::WhileStatement(w) => {
                let test = self.loop_test(Some(&w.test))?;
                self.lp(label, JumpKind::Loop, |cx, id| {
                    let (test, mut body) = test.build(id);
                    body.extend(cx.body(&w.body)?);
                    Ok(Stmt::Loop { id, test, post_test: false, body, update: None, per_iteration: vec![] })
                })?
            }
            Statement::DoWhileStatement(w) => {
                let started = self.temp(Ty::Bool);
                let lp = self.lp(label, JumpKind::Loop, |cx, id| {
                    let body = cx.body(&w.body)?;
                    let test = cx.loop_test(Some(&w.test))?;
                    if test.pre.is_empty() {
                        let test = test.test;
                        return Ok(Stmt::Loop { id, test, post_test: true, body, update: None, per_iteration: vec![] });
                    }
                    // The condition awaits: it is checked at the start of
                    // every iteration but the first (where `continue` goes).
                    Ok(cx.deferred_loop(id, started, Vec::new(), test, body, vec![]))
                })?;
                Stmt::Block(vec![Stmt::Expr(set(started, Expr::new(Ty::Bool, ExprKind::Bool(false)))), lp])
            }
            Statement::ForStatement(f) => {
                // `for (let i ...)` captured by closures: each iteration gets
                // its own binding (fresh boxes, copied at each iteration).
                let mut per_iteration = Vec::new();
                let mut block = Vec::new();
                if let Some(ForStatementInit::VariableDeclaration(v)) = &f.init
                    && v.kind != VariableDeclarationKind::Var
                {
                    for d in &v.declarations {
                        for id in d.id.get_binding_identifiers() {
                            let sym = id.symbol_id.get().unwrap();
                            if self.caps.captured.contains(&sym) {
                                per_iteration.push(self.declare_boxed(sym));
                            }
                        }
                    }
                }
                if !per_iteration.is_empty() {
                    block.push(Stmt::NewBoxes(per_iteration.clone()));
                }
                match &f.init {
                    None => {}
                    Some(ForStatementInit::VariableDeclaration(v)) => block.extend(self.var_decl(v)?),
                    Some(init) => {
                        let e = self.expr(init.to_expression())?;
                        block.append(&mut self.pre);
                        block.push(Stmt::Expr(e));
                    }
                }
                let test = self.loop_test(f.test.as_ref())?;
                let started = self.temp(Ty::Bool);
                let lp = self.lp(label, JumpKind::Loop, |cx, id| {
                    let body = cx.body(&f.body)?;
                    let mark = cx.pre.len();
                    let counter = cx.int_counter_of(&test, &body);
                    let outer = std::mem::replace(&mut cx.int_counter, counter);
                    let update = f.update.as_ref().map(|u| cx.expr(u)).transpose();
                    cx.int_counter = outer;
                    let update = update?;
                    if cx.pre.len() == mark {
                        let (test, mut first) = test.build(id);
                        first.extend(body);
                        return Ok(Stmt::Loop { id, test, post_test: false, body: first, update, per_iteration });
                    }
                    // The update awaits: it runs at the start of every
                    // iteration but the first, followed by the test.
                    let mut update_stmts = cx.pre.split_off(mark);
                    update_stmts.extend(update.map(Stmt::Expr));
                    Ok(cx.deferred_loop(id, started, update_stmts, test, body, per_iteration))
                })?;
                block.push(Stmt::Expr(set(started, Expr::new(Ty::Bool, ExprKind::Bool(false)))));
                block.push(lp);
                Stmt::Block(block)
            }
            Statement::ForOfStatement(_) | Statement::ForInStatement(_) => self.for_in_of(s, label)?,
            Statement::LabeledStatement(l) => {
                self.label = Some(l.label.name.to_string());
                return self.stmt(&l.body);
            }
            Statement::BreakStatement(b) => Stmt::Break(self.jump_target(b.label.as_ref(), b.span, false)?),
            Statement::ContinueStatement(c) => Stmt::Continue(self.jump_target(c.label.as_ref(), c.span, true)?),
            Statement::ReturnStatement(r) => {
                if self.in_finally > 0 {
                    return reject(r.span, "`return` inside `finally` is not supported");
                }
                if let Some(Expression::AwaitExpression(aw)) = r.argument.as_ref().map(|a| a.without_parentheses())
                    && self.json_site(aw).is_none()
                {
                    let value = any(self.expr(&aw.argument)?);
                    self.returns.push(Ty::Any);
                    if self.is_async && self.is_generator {
                        // An async generator awaits what it returns (again).
                        let t = self.temp(Ty::Any);
                        self.pre.push(Stmt::Await { value, target: ir::AwaitTarget::Set(t) });
                        let value = self.local(t);
                        return Ok(Some(Stmt::Await { value, target: ir::AwaitTarget::Return }));
                    }
                    return Ok(Some(Stmt::Await { value, target: ir::AwaitTarget::Return }));
                }
                if self.is_async
                    && self.is_generator
                    && let Some(a) = &r.argument
                {
                    // An async generator awaits what it returns.
                    let value = any(self.expr(a)?);
                    self.returns.push(Ty::Any);
                    return Ok(Some(Stmt::Await { value, target: ir::AwaitTarget::Return }));
                }
                if let Some(l) = self.this_local {
                    // A constructor returns the instance.
                    if let Some(a) = &r.argument {
                        return reject(a.span(), "constructors returning a value are not supported");
                    }
                    self.returns.push(Ty::Any);
                    return Ok(Some(Stmt::Return(Some(self.local(l)))));
                }
                let mut arg = r.argument.as_ref().map(|a| self.expr(a)).transpose()?;
                if self.ret_int {
                    arg = Some(self.int_return(arg));
                }
                self.returns.push(arg.as_ref().map_or(Ty::Void, |a| a.ty));
                Stmt::Return(arg)
            }
            Statement::ThrowStatement(t) => Stmt::Throw(any(self.expr(&t.argument)?)),
            Statement::TryStatement(t) => self.try_stmt(t)?,
            Statement::SwitchStatement(sw) => self.switch(sw)?,
            other => return reject(other.span(), format!("unsupported statement: {}", describe_stmt(other))),
        }))
    }

    /// An expression statement; `await` is supported here, as `await e;` and
    /// `x = await e;`.
    fn expr_stmt(&mut self, e: &'a Expression<'a>) -> R<Stmt> {
        match e.without_parentheses() {
            Expression::UpdateExpression(u) => Ok(Stmt::Expr(self.update(u, false)?)),
            Expression::AwaitExpression(aw) => {
                let value = any(self.expr(&aw.argument)?);
                Ok(Stmt::Await { value, target: ir::AwaitTarget::Discard })
            }
            Expression::YieldExpression(y) => self.yield_stmt(y, ir::AwaitTarget::Discard),
            Expression::AssignmentExpression(a)
                if a.operator == AssignmentOperator::Assign
                    && matches!(a.right.without_parentheses(),
                        Expression::AwaitExpression(aw) if self.json_site(aw).is_none()) =>
            {
                let Expression::AwaitExpression(aw) = a.right.without_parentheses() else { unreachable!() };
                let local = match &a.left {
                    AssignmentTarget::AssignmentTargetIdentifier(id) => {
                        self.resolve(id).and_then(|s| self.sym_local.get(&s).copied().map(|l| (s, l)))
                    }
                    _ => None,
                };
                let Some((sym, l)) = local else {
                    // (Another target: the general assignment, its value
                    // awaited before it.)
                    return Ok(Stmt::Expr(self.expr(e)?));
                };
                if !self.mutable[l] {
                    return reject(a.span, "assignment to a constant");
                }
                if self.locals[l] != Ty::Any {
                    self.widen_request = Some(sym);
                    return Err(Reject { span: a.span, msg: "retry".into(), retry: true, callee: false });
                }
                let value = any(self.expr(&aw.argument)?);
                Ok(Stmt::Await { value, target: ir::AwaitTarget::Set(l) })
            }
            other => Ok(Stmt::Expr(self.expr(other)?)),
        }
    }

    /// `yield` / `yield*`, delivering what the generator is resumed with to
    /// `target`.
    fn yield_stmt(&mut self, y: &'a YieldExpression<'a>, target: ir::AwaitTarget) -> R<Stmt> {
        let value = match &y.argument {
            Some(a) => any(self.expr(a)?),
            None => undefined(),
        };
        let kind = if y.delegate {
            ir::YieldKind::Delegate {
                iter: self.temp(Ty::Any),
                next: self.temp(Ty::Any),
                next_atom: self.m.atom("next"),
            }
        } else {
            ir::YieldKind::Value
        };
        Ok(Stmt::Yield { value, kind, target })
    }

    /// A loop condition. One that awaits is evaluated at the start of the
    /// body instead, breaking out when false.
    fn loop_test(&mut self, test: Option<&'a Expression<'a>>) -> R<LoopTest> {
        let mark = self.pre.len();
        let test = test.map(|t| self.cond(t)).transpose()?;
        Ok(LoopTest { pre: self.pre.split_off(mark), test })
    }

    /// A loop whose condition (`do`-`while`) or update (`for`) awaits. They
    /// run at the start of the body, from the second iteration on (`started`)
    /// for `update`, and for the test too where `update` is not empty or the
    /// loop is a `do`-`while` (a `for` test runs every time).
    fn deferred_loop(
        &mut self,
        id: ir::LoopId,
        started: ir::LocalId,
        update: Vec<Stmt>,
        test: LoopTest,
        body: Vec<Stmt>,
        per_iteration: Vec<ir::LocalId>,
    ) -> Stmt {
        let is_do_while = update.is_empty();
        let mut test_stmts = test.pre;
        if let Some(t) = test.test {
            let not = Expr::new(Ty::Bool, ExprKind::Unary(UnOp::Not, Box::new(t)));
            test_stmts.push(Stmt::If(not, vec![Stmt::Break(id)], vec![]));
        }
        let mut first = Vec::new();
        if is_do_while {
            first.push(Stmt::If(self.local(started), test_stmts, vec![]));
        } else {
            first.push(Stmt::If(self.local(started), update, vec![]));
            first.extend(test_stmts);
        }
        first.push(Stmt::Expr(set(started, Expr::new(Ty::Bool, ExprKind::Bool(true)))));
        first.extend(body);
        Stmt::Loop { id, test: None, post_test: false, body: first, update: None, per_iteration }
    }

    fn lp(
        &mut self,
        label: Option<String>,
        kind: JumpKind,
        f: impl FnOnce(&mut Self, ir::LoopId) -> R<Stmt>,
    ) -> R<Stmt> {
        let id = self.next_loop;
        self.next_loop += 1;
        self.jumps.push((label, id, kind));
        let r = f(self, id);
        self.jumps.pop();
        r
    }

    fn jump_target(&self, label: Option<&LabelIdentifier>, span: Span, is_continue: bool) -> R<ir::LoopId> {
        if self.in_finally > 0 {
            return reject(span, "jumps inside `finally` are not supported");
        }
        let found = self.jumps.iter().rev().find(|(name, _, kind)| match label {
            Some(l) => name.as_deref() == Some(l.name.as_str()),
            // Unlabeled `continue` skips switches; unlabeled `break` does not.
            None => !is_continue || *kind == JumpKind::Loop,
        });
        match found {
            Some(&(_, id, kind)) if !(is_continue && kind != JumpKind::Loop) => Ok(id),
            _ => reject(span, "jump target not found"),
        }
    }

    fn switch(&mut self, sw: &'a SwitchStatement<'a>) -> R<Stmt> {
        let d = self.expr(&sw.discriminant)?;
        // The case clauses form one block.
        let all: Vec<&Statement> = sw.cases.iter().flat_map(|c| c.consequent.iter()).collect();
        let mut prologue = Vec::new();
        for s in all {
            prologue.extend(self.block_prologue(std::slice::from_ref(s))?);
        }
        let label = self.label.take();
        // Tests are checked first: if any awaits, the matching case is
        // selected by an `if` chain beforehand (in the same order).
        let mark = self.pre.len();
        let mut tests = Vec::new();
        let mut awaits = false;
        for c in &sw.cases {
            let m = self.pre.len();
            let test = c.test.as_ref().map(|t| self.expr(t)).transpose()?;
            awaits |= self.pre.len() > m;
            tests.push((self.pre.split_off(m), test));
        }
        debug_assert_eq!(self.pre.len(), mark);
        // A number against numeric constants (as `case STATE:` with a
        // module-level const) compares numbers; otherwise, values.
        let numeric = d.ty.is_number()
            && !awaits
            && tests.iter().all(|(_, t)| t.as_ref().is_none_or(|e| matches!(e.kind, ExprKind::Num(_))));
        let disc = self.temp(if numeric { Ty::F64 } else { Ty::Any });
        let init = Stmt::Expr(if numeric { set(disc, to_f64(d)) } else { set(disc, any(d)) });
        if !numeric {
            for (_, t) in &mut tests {
                *t = t.take().map(any);
            }
        }
        let mut select = Vec::new();
        let disc = if awaits {
            let sel = self.temp(Ty::Any);
            let none = any(Expr::num(-1.0));
            select.push(Stmt::Expr(set(sel, none)));
            let mut chain: Vec<Stmt> = Vec::new();
            for (i, (pre, test)) in tests.iter_mut().enumerate().rev() {
                let Some(t) = test.take() else { continue };
                let hit = Expr::new(
                    Ty::Bool,
                    ExprKind::AnyCompare(AnyCmpOp::StrictEq, Box::new(self.local(disc)), Box::new(t)),
                );
                let mut arm = std::mem::take(pre);
                arm.push(Stmt::If(hit, vec![Stmt::Expr(set(sel, any(Expr::num(i as f64))))], chain));
                chain = arm;
                *test = Some(any(Expr::num(i as f64)));
            }
            select.extend(chain);
            sel
        } else {
            disc
        };
        let sw_stmt = self.lp(label, JumpKind::Switch, |cx, id| {
            let mut cases = Vec::new();
            for (c, (_, test)) in sw.cases.iter().zip(tests) {
                cases.push((test, cx.stmts(&c.consequent)?));
            }
            Ok(Stmt::Switch { id, disc, cases })
        })?;
        // Declarations in the case block exist before the tests run.
        let mut out = vec![init];
        out.extend(prologue);
        out.extend(select);
        out.push(sw_stmt);
        Ok(Stmt::Block(out))
    }

    fn try_stmt(&mut self, t: &'a TryStatement<'a>) -> R<Stmt> {
        let body = self.block(&t.block.body)?;
        let catch = match &t.handler {
            None => None,
            Some(h) => {
                let mut handler = Vec::new();
                let param = match &h.param {
                    None => None,
                    Some(p) => {
                        let sym = match &p.pattern {
                            BindingPattern::BindingIdentifier(id) => id.symbol_id.get(),
                            _ => None,
                        };
                        if let Some(sym) = sym
                            && !self.caps.captured.contains(&sym)
                        {
                            Some(self.declare(sym, Ty::Any, true))
                        } else {
                            // The exception goes to a hidden local, then to
                            // the pattern (or the box of a captured name).
                            let raw = self.temp(Ty::Any);
                            let v = self.local(raw);
                            self.bind_pattern(&p.pattern, v, BindKind::Let, &mut handler)?;
                            Some(raw)
                        }
                    }
                };
                handler.extend(self.block(&h.body.body)?);
                Some((param, handler))
            }
        };
        let finally = match &t.finalizer {
            None => None,
            Some(f) => {
                self.in_finally += 1;
                let r = self.block(&f.body);
                self.in_finally -= 1;
                Some(r?)
            }
        };
        // The exception waits here while a finalizer that may suspend runs.
        let exc = (finally.is_some() && self.suspends()).then(|| self.temp(Ty::Any));
        Ok(Stmt::Try { body, catch, finally, exc })
    }

    fn var_decl(&mut self, v: &'a VariableDeclaration<'a>) -> R<Vec<Stmt>> {
        let (mutable, is_var) = match v.kind {
            VariableDeclarationKind::Let => (true, false),
            VariableDeclarationKind::Const => (false, false),
            VariableDeclarationKind::Var => (true, true),
            _ => return reject(v.span, "`using` declarations are not supported"),
        };
        let mut out = Vec::new();
        let base = self.pre.len();
        for d in &v.declarations {
            let BindingPattern::BindingIdentifier(id) = &d.id else {
                // Destructuring.
                let Some(init) = &d.init else {
                    return reject(d.span, "a destructuring declaration needs an initializer");
                };
                let v = self.expr(init)?;
                out.extend(self.pre.drain(base..));
                let kind = if is_var {
                    BindKind::Var
                } else if mutable {
                    BindKind::Let
                } else {
                    BindKind::Const
                };
                self.bind_pattern(&d.id, v, kind, &mut out)?;
                continue;
            };
            let sym = id.symbol_id.get().unwrap();
            if self.seg_cells.contains(&sym) {
                // A module binding of a segment that other code uses: its cell.
                self.name_hint = Some(id.name.to_string());
                let init = match &d.init {
                    Some(init) => self.expr(init)?,
                    // (A `var` is undefined until assigned: its cell, which
                    // interpreted code may be all that uses, is.)
                    None if is_var => {
                        self.m.cell(sym, d.span)?;
                        continue;
                    }
                    None => undefined(),
                };
                self.name_hint = None;
                let init = if self.wraps(Some(sym)) && init.ty != Ty::I32 { wrap(init) } else { init };
                let k = self.m.cell(sym, d.span)?;
                out.extend(self.pre.drain(base..));
                out.push(Stmt::Expr(init_cell(k, init, is_var)));
                continue;
            }
            if let Some(Expression::AwaitExpression(aw)) = d.init.as_ref().map(|i| i.without_parentheses())
                && self.json_site(aw).is_none()
            {
                let value = any(self.expr(&aw.argument)?);
                let l = match self.sym_local.get(&sym) {
                    Some(&l) if is_var || self.boxed[l] => l,
                    _ => self.declare(sym, Ty::Any, mutable),
                };
                let target = if is_var { ir::AwaitTarget::Set(l) } else { ir::AwaitTarget::Init(l) };
                out.extend(self.pre.drain(base..));
                out.push(Stmt::Await { value, target });
                continue;
            }
            if is_var {
                // Hoisted: just an assignment, if there is an initializer.
                let l = self.sym_local[&sym];
                if let Some(init) = &d.init {
                    self.name_hint = Some(id.name.to_string());
                    let v = self.expr(init)?;
                    let st = Stmt::Expr(self.set_local(Some(sym), l, v, d.span)?);
                    out.extend(self.pre.drain(base..));
                    out.push(st);
                }
                continue;
            }
            // The initializer is checked before an unboxed binding exists, so
            // a self-reference is rejected exactly like the TDZ would throw.
            // (Boxed bindings exist already and check their TDZ at runtime.)
            self.name_hint = Some(id.name.to_string());
            let init = match &d.init {
                Some(init) => self.expr(init)?,
                None => undefined(),
            };
            let declared = d.type_annotation.as_ref().and_then(|a| ts_type(&a.type_annotation));
            self.name_hint = None;
            let local = match self.sym_local.get(&sym) {
                Some(&l) if self.boxed[l] => l,
                _ => {
                    let ty = if declared == Some(Ty::I32) {
                        Ty::I32
                    } else if init.ty == Ty::Void || self.widened.contains(&sym) {
                        Ty::Any
                    } else if init.ty == Ty::I32 {
                        // Inferred: any number may be assigned later.
                        Ty::F64
                    } else {
                        init.ty
                    };
                    self.declare(sym, ty, mutable)
                }
            };
            let ty = self.locals[local];
            let init = if ty == Ty::I32 {
                self.int_value(local, init)
            } else if self.wraps(Some(sym)) && init.ty != Ty::I32 {
                // A captured `int`.
                wrap(init)
            } else {
                init
            };
            // Each declarator's hoisted `await`s run after the previous
            // declarator is initialized.
            out.extend(self.pre.drain(base..));
            out.push(Stmt::Expr(Expr::new(ty, ExprKind::InitLocal(local, Box::new(coerce(init, ty))))));
        }
        Ok(out)
    }

    /// `new C(args)` of a class with a native constructor: a direct call
    /// of it, once the binding `C` is read (it may be in its TDZ).
    fn native_new(&mut self, n: &'a NewExpression<'a>) -> R<Option<Expr>> {
        let Expression::Identifier(id) = n.callee.without_parentheses() else { return Ok(None) };
        // (Spills for `await` would run before the binding is read.)
        if self.suspends() || has_spread(&n.arguments) {
            return Ok(None);
        }
        let Some(sym) = self.resolve(id) else { return Ok(None) };
        let Some(i) = self.m.cands.iter().position(|c| c.ctor.as_ref().is_some_and(|k| k.symbol == Some(sym))) else {
            return Ok(None);
        };
        let class = any(self.ident(id)?);
        Ok(self
            .direct_call(i, &n.arguments, n.span, None)?
            .map(|call| seq(vec![Expr::new(Ty::Void, ExprKind::Void(Box::new(class))), call])))
    }

    /// The classes the type annotations of `node`'s parameters and
    /// variables name (`sym_hints`).
    fn collect_hints(&mut self, node: FnNode<'a>) {
        use oxc::ast_visit::{Visit, walk};
        struct Hints<'c>(&'c [ClassInfo], Vec<(SymbolId, ir::Hint)>);
        impl Hints<'_> {
            fn add(&mut self, id: &BindingPattern, t: Option<&TSTypeAnnotation>) {
                if let BindingPattern::BindingIdentifier(id) = id
                    && let Some(h) = t.and_then(|t| hint_name(&t.type_annotation))
                    && let Some(h) = resolve_hint(&h, self.0)
                {
                    self.1.push((id.symbol_id.get().unwrap(), h));
                }
            }
        }
        impl<'a> Visit<'a> for Hints<'_> {
            fn visit_formal_parameter(&mut self, p: &FormalParameter<'a>) {
                self.add(&p.pattern, p.type_annotation.as_deref());
                walk::walk_formal_parameter(self, p);
            }
            fn visit_variable_declarator(&mut self, d: &VariableDeclarator<'a>) {
                self.add(&d.id, d.type_annotation.as_deref());
                walk::walk_variable_declarator(self, d);
            }
        }
        let mut v = Hints(self.m.classes, Vec::new());
        match node {
            FnNode::Function(f) => v.visit_function(f, oxc::syntax::scope::ScopeFlags::Function),
            FnNode::Arrow(a) => v.visit_arrow_function_expression(a),
            FnNode::Segment(_, stmts) => stmts.iter().for_each(|st| v.visit_statement(st)),
        }
        self.sym_hints.extend(v.1);
    }

    /// A native constructor's prologue: its fields' initializers, in order,
    /// then the instance with their values in `this_local`.
    fn new_instance(&mut self, ctor: &CtorInfo<'a>) -> R<Vec<Stmt>> {
        let mut out = Vec::new();
        let mut fields = Vec::new();
        let mut privates = Vec::new();
        for &(ref name, init) in &ctor.fields {
            // An arrow function that sees the instance is made once it
            // exists (making it runs no code); a public field is
            // `undefined` until then.
            if let Some(e) = init
                && mentions_this(e)
            {
                self.deferred_fields.push((name.clone(), e, ctor.class));
                if name.starts_with('#') {
                    privates.push((name.clone(), undefined()));
                } else {
                    fields.push((self.m.atom(name), undefined()));
                }
                continue;
            }
            let v = match init {
                None => undefined(),
                Some(e) => {
                    let outer = std::mem::take(&mut self.pre);
                    self.name_hint = Some(name.clone());
                    let r = self.expr(e);
                    self.name_hint = None;
                    out.extend(std::mem::replace(&mut self.pre, outer));
                    let v = any(r?);
                    if matches!(v.kind, ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Null) {
                        v
                    } else {
                        // Evaluated in order, before the instance exists.
                        let t = self.temp(Ty::Any);
                        out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(v)))));
                        self.local(t)
                    }
                }
            };
            if name.starts_with('#') {
                privates.push((name.clone(), v));
            } else {
                fields.push((self.m.atom(name), v));
            }
        }
        if !self.m.homes_used.contains(&ctor.home) {
            self.m.homes_used.push(ctor.home);
        }
        let this = self.temp(Ty::Any);
        self.this_local = Some(this);
        let inst = if ctor.brand || !privates.is_empty() {
            // Its brand and private fields (which nothing can enumerate:
            // after its public ones), made with it.
            if ctor.brand && !self.m.homes_used.contains(&ctor.class) {
                self.m.homes_used.push(ctor.class);
            }
            let privates = privates.into_iter().map(|(n, v)| (self.m.private(ctor.class, &n), any(v))).collect();
            let brand = ctor.brand.then_some(2 * ctor.class);
            ExprKind::NewInstanceFull { class: ctor.home, fields, brand, privates }
        } else {
            ExprKind::NewInstance { class: ctor.home, fields }
        };
        out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(this, Box::new(Expr::new(Ty::Any, inst))))));
        Ok(out)
    }

    /// A derived class's construction of instance `this` (made by its
    /// superclass): the class's brand, if it has private methods or
    /// accessors.
    fn add_brand(&mut self, ctor: &CtorInfo<'a>, this: ir::LocalId) -> Option<Stmt> {
        if !ctor.brand {
            return None;
        }
        if !self.m.homes_used.contains(&ctor.class) {
            self.m.homes_used.push(ctor.class);
        }
        let brand = ExprKind::AddBrand(Box::new(self.local(this)), 2 * ctor.class);
        Some(Stmt::Expr(Expr::new(Ty::Void, brand)))
    }

    /// The same, of private field `name` (with its `#`) of the class.
    fn define_private(&mut self, ctor: &CtorInfo<'a>, this: ir::LocalId, name: &str, v: Expr) -> Stmt {
        let slot = self.m.private(ctor.class, name);
        let define = ExprKind::PrivateDefine(Box::new(self.local(this)), slot, Box::new(any(v)));
        Stmt::Expr(Expr::new(Ty::Void, define))
    }

    /// A base class constructor's deferred fields (see `deferred_fields`).
    fn deferred_fields(&mut self) -> R<Vec<Stmt>> {
        let mut out = Vec::new();
        for (name, e, class) in std::mem::take(&mut self.deferred_fields) {
            let outer = std::mem::take(&mut self.pre);
            self.name_hint = Some(name.clone());
            let r = self.expr(e);
            self.name_hint = None;
            out.extend(std::mem::replace(&mut self.pre, outer));
            let v = any(r?);
            let this = self.local(self.this_local.unwrap());
            if name.starts_with('#') {
                // (Made with the instance, `undefined` until now.)
                let slot = self.m.private(class, &name);
                out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::PrivateSetField(Box::new(this), slot, Box::new(v)))));
            } else {
                let atom = self.m.atom(&name);
                out.push(Stmt::Expr(self.set_prop(this, atom, v)));
            }
        }
        Ok(out)
    }

    /// `this.name = v` for parameter property `name`.
    fn param_prop(&mut self, name: &str, v: Expr) -> Stmt {
        let atom = self.m.atom(name);
        let this = self.local(self.this_local.unwrap());
        Stmt::Expr(self.set_prop(this, atom, v))
    }

    /// A derived class's constructor called by `new`, first: that the
    /// classes it is made by are still each other's superclasses, else
    /// the class constructed with its arguments as interpreted code would
    /// (`CtorInfo::checks`).
    fn ctor_checks(&mut self, ctor: &CtorInfo<'a>, nparams: usize) -> R<Vec<Stmt>> {
        let mut out = Vec::new();
        for &(class, parent) in &ctor.checks {
            for k in [class, parent] {
                if !self.m.homes_used.contains(&k) {
                    self.m.homes_used.push(k);
                }
            }
            let cell = self.m.cell(ctor.symbol.unwrap(), ctor.span)?;
            let callee = Box::new(Expr::new(Ty::Any, ExprKind::Cell(cell)));
            let fixed = nparams - self.rest as usize;
            let args: Vec<Expr> = (0..fixed).map(|l| self.local(l)).collect();
            let construct = if self.rest {
                let mut parts: Vec<ir::ArrPart> = args.into_iter().map(ir::ArrPart::Value).collect();
                parts.push(ir::ArrPart::Spread(self.local(fixed)));
                ExprKind::NewSpread(callee, Box::new(Expr::new(Ty::Any, ExprKind::ArrayParts(parts))))
            } else {
                ExprKind::New(callee, args)
            };
            self.returns.push(Ty::Any);
            let ok = Expr::new(Ty::Bool, ExprKind::ProtoIs { class, parent });
            out.push(Stmt::If(ok, Vec::new(), vec![Stmt::Return(Some(Expr::new(Ty::Any, construct)))]));
        }
        Ok(out)
    }

    /// A derived class constructor's `super(args)` (`args` evaluated into
    /// `self.pre`'s statements): its superclass's constructor, compiled to
    /// make an instance of the class being constructed, gives `this`;
    /// then the class's fields are defined in order and its parameter
    /// properties assigned. Evaluates to `this`.
    fn super_call(&mut self, args: Vec<Expr>, span: Span) -> R<Expr> {
        let call = self.super_ctor(|_, i| Ok(Some(Expr::new(Ty::Any, ExprKind::Call(i, args)))), span)?;
        self.super_init(call)
    }

    /// `super(args)` of a class whose superclass has no native constructor
    /// (`CtorInfo::dynamic`), the arguments in a list or an array.
    fn super_new(&mut self, args: Option<Vec<Expr>>, array: Option<Expr>) -> Expr {
        let ctor = self.derived.as_ref().unwrap();
        let (class, target) = if ctor.local {
            // (The class is the constructor running; new.target its `this`.)
            (ExprKind::CurrentFunction, ExprKind::This)
        } else {
            let (class, home) = (ctor.class, ctor.home);
            for k in [class, home] {
                if !self.m.homes_used.contains(&k) {
                    self.m.homes_used.push(k);
                }
            }
            (ExprKind::HomeValue(2 * class + 1), ExprKind::HomeValue(2 * home + 1))
        };
        let (class, target) = (Box::new(Expr::new(Ty::Any, class)), Box::new(Expr::new(Ty::Any, target)));
        let kind = match (args, array) {
            (Some(args), _) => ExprKind::SuperNew { class, target, args },
            (None, Some(a)) => ExprKind::SuperNewSpread { class, target, args: Box::new(a) },
            (None, None) => unreachable!(),
        };
        Expr::new(Ty::Any, kind)
    }

    /// The direct call of the superclass's constructor (`call` makes it,
    /// given its index).
    fn super_ctor(&mut self, call: impl FnOnce(&mut Self, usize) -> R<Option<Expr>>, span: Span) -> R<Expr> {
        let ctor = self.derived.as_ref().unwrap();
        let (parent, home) = (ctor.parent.unwrap(), ctor.home);
        let Some(i) =
            self.m.cands.iter().position(|c| c.ctor.as_ref().is_some_and(|k| k.class == parent && k.home == home))
        else {
            return reject(span, "the superclass constructor is not compiled natively");
        };
        match call(self, i)? {
            Some(c) => Ok(c),
            None => reject(span, "the superclass constructor cannot be called directly with these arguments"),
        }
    }

    fn super_init(&mut self, call: Expr) -> R<Expr> {
        let ctor = self.derived.take().unwrap();
        let this = self.this_local.unwrap();
        self.pre.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(this, Box::new(call)))));
        // The box of `this` for arrows, now that there is an instance.
        if self.caps.this_owners.contains(&self.key) {
            let b = self.this_box_local();
            self.pre.push(Stmt::NewBoxes(vec![b]));
            let v = Box::new(self.local(this));
            self.pre.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(b, v))));
            self.this_box = Some(b);
        }
        // Its brand, then its fields in order.
        let brand = self.add_brand(&ctor, this);
        self.pre.extend(brand);
        for (name, init) in &ctor.fields {
            let v = match init {
                None => undefined(),
                Some(e) => {
                    self.name_hint = Some(name.clone());
                    let r = self.expr(e);
                    self.name_hint = None;
                    any(r?)
                }
            };
            if name.starts_with('#') {
                let define = self.define_private(&ctor, this, name, v);
                self.pre.push(define);
                continue;
            }
            let atom = self.m.atom(name);
            let obj = Box::new(self.local(this));
            self.pre.push(Stmt::Expr(Expr::new(Ty::Void, ExprKind::DefineField(obj, atom, Box::new(v)))));
        }
        for (name, sym) in &ctor.param_props {
            let v = self.local(self.sym_local[sym]);
            let s = self.param_prop(name, v);
            self.pre.push(s);
        }
        Ok(self.local(this))
    }

    /// A nested function, compiled natively as a closure over the boxes of
    /// the variables it captures.
    fn closure(&mut self, node: FnNode<'a>, name: String) -> R<Expr> {
        self.closure_with(node, name, false, node.span())
    }

    /// A nested function; `method`: a method of an object literal, which
    /// gets `this` (and has no class to resolve `super` in).
    /// (`text`: its source text, see `ir::Func::source`.)
    fn closure_with(&mut self, node: FnNode<'a>, name: String, method: bool, text: Span) -> R<Expr> {
        self.closure_full(node, name, method, text, None)
    }

    /// `closure_with`, or (`ctor`) the constructor of a class inside a
    /// function (`class_value`).
    fn closure_full(
        &mut self,
        node: FnNode<'a>,
        name: String,
        method: bool,
        text: Span,
        ctor: Option<CtorInfo<'a>>,
    ) -> R<Expr> {
        let key = node.span().start;
        if self.m.interpreted.contains(&key) {
            return reject(node.span(), "contains a function marked `interpreted`");
        }
        let (params, rest, ret) = header(node)?;
        // There is no interpreted fallback to decline to: every parameter is
        // a JS value.
        let params: Vec<ParamSpec<'a>> = params.into_iter().map(|p| (p.pattern, Ty::Any, p.default)).collect();
        let caps = self.caps.clone();
        let widened = self.widened.clone();
        let mut inner = FnCx::new(&mut *self.m, caps, key, true, widened);
        inner.rest = rest;
        // (A class inside a function: no private names or `super.x` of
        // its own; not those of the class around it either.)
        inner.class = if method || ctor.is_some() { None } else { self.class };
        let ordinary_this = !method && ctor.is_none() && matches!(node, FnNode::Function(f) if function_uses_this(f));
        inner.has_this = method || ordinary_this;
        inner.ordinary_this = ordinary_this;
        inner.class_ctor = ctor.is_some();
        inner.ctor = ctor;
        inner.ret_int = ret == Some(Ty::I32);
        let r = inner.function(node, name, params);
        let request = inner.widen_request;
        let mut f = match r {
            Ok(f) => f,
            Err(e) => {
                if e.retry {
                    self.widen_request = request;
                }
                return Err(e);
            }
        };
        f.source = Some((text.start, text.end));
        let mut boxes = Vec::new();
        for sym in self.caps.free.get(&key).cloned().unwrap_or_default() {
            match self.sym_local.get(&sym) {
                Some(&l) if self.boxed[l] => boxes.push(l),
                _ => return reject(node.span(), "closure captures a variable that is not in scope here"),
            }
        }
        if self.caps.this_free.contains(&key) {
            match self.this_box {
                Some(b) => boxes.push(b),
                None => return reject(node.span(), "arrow function uses a `this` not available here"),
            }
        }
        if self.caps.args_free.contains(&key) {
            match self.args_box {
                Some(b) => boxes.push(b),
                None => return reject(node.span(), "arrow function uses an `arguments` not available here"),
            }
        }
        self.m.closures.push(f);
        self.m.closure_spans.push(node.span());
        Ok(Expr::new(Ty::Any, ExprKind::Closure(self.m.closures.len() - 1, boxes)))
    }
    // ---- classes inside functions ----

    /// A class defined inside a function (or by module code): made as the
    /// class definition makes it, its constructor a native closure that
    /// `new` calls (`ir::Func::class_ctor`), its methods, getters and
    /// setters native closures defined on its prototype (or on it, static
    /// ones), all of them over the variables of the function they see. Its
    /// fields are public instance fields; private names, static fields and
    /// blocks, computed keys and `super.x` keep the function interpreted.
    /// `name`: the class's name (`name` of its constructor).
    fn class_value(&mut self, c: &'a Class<'a>, name: String) -> R<Expr> {
        if !c.decorators.is_empty() {
            return reject(c.span, "decorated classes are not supported");
        }
        let mut ctor_method = None;
        let mut methods = Vec::new();
        let mut fields: Vec<(String, Option<&'a Expression<'a>>)> = Vec::new();
        for el in &c.body.body {
            match el {
                ClassElement::MethodDefinition(m) => {
                    if m.value.body.is_none() || m.r#type != MethodDefinitionType::MethodDefinition {
                        continue; // an overload or an abstract method
                    }
                    if !m.decorators.is_empty() {
                        return reject(m.span, "decorated methods are not supported");
                    }
                    if m.kind == MethodDefinitionKind::Constructor {
                        ctor_method = Some(m);
                        continue;
                    }
                    if matches!(m.key, PropertyKey::PrivateIdentifier(_)) {
                        return reject(m.key.span(), "private methods of classes inside functions are not supported yet");
                    }
                    methods.push((m, class_key(&m.key, m.computed)));
                }
                ClassElement::PropertyDefinition(p) => {
                    if p.declare || p.r#type == PropertyDefinitionType::TSAbstractPropertyDefinition {
                        continue;
                    }
                    if p.r#static {
                        return reject(p.span, "static fields of classes inside functions are not supported yet");
                    }
                    if !p.decorators.is_empty() {
                        return reject(p.span, "decorated fields are not supported");
                    }
                    let Some(key) = class_key(&p.key, p.computed) else {
                        return reject(p.key.span(), "private and computed field names of classes inside functions are not supported yet");
                    };
                    fields.push((key, p.value.as_ref()));
                }
                ClassElement::TSIndexSignature(_) => {}
                other => return reject(other.span(), "static blocks and `accessor` fields of classes inside functions are not supported yet"),
            }
        }
        // Its constructor.
        let derived = c.heritage.is_some();
        let mut param_props = Vec::new();
        let (node, forward) = match ctor_method {
            Some(m) => {
                let f = &m.value;
                for p in &f.params.items {
                    if p.accessibility.is_some() || p.readonly || p.r#override {
                        let BindingPattern::BindingIdentifier(b) = &p.pattern else {
                            return reject(p.span, "destructured parameter properties are not supported");
                        };
                        param_props.push((b.name.to_string(), b.symbol_id.get().unwrap()));
                    }
                }
                if derived && super_first(f).is_none() {
                    return reject(
                        f.span,
                        "the constructor does not start with `super(...)` (with arguments that cannot see `this`)",
                    );
                }
                (FnNode::Function(f), false)
            }
            // The default constructor: nothing to run; a derived class's
            // passes its arguments on.
            None => (FnNode::Segment(c.span, &[]), derived),
        };
        fields.splice(0..0, param_props.iter().map(|(n, _)| (n.clone(), None)));
        let info = CtorInfo {
            symbol: None,
            span: c.span,
            class: usize::MAX,
            home: usize::MAX,
            fields,
            param_props,
            parent: None,
            dynamic: derived,
            forward,
            checks: Vec::new(),
            instance_fields: Vec::new(),
            brand: false,
            recheck: Vec::new(),
            local: true,
        };
        // Its own name inside it (a class expression's).
        let own = match &c.id {
            Some(id) if c.r#type == ClassType::ClassExpression => {
                let l = self.declare(id.symbol_id.get().unwrap(), Ty::Any, false);
                if self.boxed[l] {
                    // (Before the functions that capture it are made.)
                    self.pre.push(Stmt::NewBoxes(vec![l]));
                }
                Some(l)
            }
            _ => None,
        };
        let mut seq = Vec::new();
        // Its superclass, first.
        let parent = match &c.heritage {
            Some(h) => {
                let mark = self.pre.len();
                let v = any(self.expr(&h.expression)?);
                if self.pre.len() != mark {
                    return reject(h.span(), "`await` in a class heritage is not supported");
                }
                let t = self.temp(Ty::Any);
                seq.push(Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(v))));
                Some(t)
            }
            None => None,
        };
        let ctor = self.closure_full(node, name, false, c.span, Some(info))?;
        let f = self.temp(Ty::Any);
        seq.push(Expr::new(Ty::Any, ExprKind::InitLocal(f, Box::new(ctor))));
        let setup = ExprKind::ClassSetup {
            ctor: Box::new(self.local(f)),
            parent: parent.map(|p| Box::new(self.local(p))),
        };
        let proto = self.temp(Ty::Any);
        seq.push(Expr::new(Ty::Any, ExprKind::InitLocal(proto, Box::new(Expr::new(Ty::Any, setup)))));
        for (m, key) in methods {
            let kind = match m.kind {
                MethodDefinitionKind::Get => 1,
                MethodDefinitionKind::Set => 2,
                _ => 0,
            };
            // A computed key: evaluated in order (the function is named for
            // it when it is defined).
            let (key, fname) = match key {
                Some(key) => {
                    let fname = match kind {
                        1 => format!("get {key}"),
                        2 => format!("set {key}"),
                        _ => key.clone(),
                    };
                    (ir::Key::Atom(self.m.atom(&key)), fname)
                }
                None => {
                    let e = m.key.as_expression().unwrap();
                    let mark = self.pre.len();
                    let k = any(self.expr(e)?);
                    if self.pre.len() != mark {
                        return reject(e.span(), "`await` in a computed key is not supported");
                    }
                    let t = self.temp(Ty::Any);
                    let k = Expr::new(Ty::Any, ExprKind::ToPropKey(Box::new(k)));
                    seq.push(Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(k))));
                    (ir::Key::Value(Box::new(self.local(t))), String::new())
                }
            };
            // (Its text without `static`, as `toString` gives it.)
            let text = if m.r#static { Span::new(m.span.start + "static".len() as u32, m.span.end) } else { m.span };
            let func = self.closure_full(FnNode::Function(&m.value), fname, true, text, None)?;
            let obj = self.local(if m.r#static { f } else { proto });
            let define = ExprKind::DefineMethod { obj: Box::new(obj), key, func: Box::new(func), kind };
            seq.push(Expr::new(Ty::Void, define));
        }
        if let Some(l) = own {
            seq.push(Expr::new(Ty::Any, ExprKind::InitLocal(l, Box::new(self.local(f)))));
        }
        seq.push(self.local(f));
        Ok(seq_expr(seq))
    }

    // ---- classes: private names and `super` ----

    /// The class and kind of private name `name` (with its `#`).
    fn private_kind(&self, name: &str, span: Span) -> R<(usize, PrivateKind)> {
        let Some((class, _)) = self.class else {
            return reject(span, "private names outside class methods are not supported");
        };
        match self.m.classes[class].privates.iter().find(|(n, _)| n == name) {
            Some(&(_, kind)) => Ok((class, kind)),
            None => reject(span, format!("`{name}` is not a private name of this class")),
        }
    }

    /// The native candidate of private method `name` (with its `#`) of
    /// this class, which native code may call directly (not async or a
    /// generator, which are called through their JS entry).
    fn private_method(&self, name: &str, span: Span) -> Option<usize> {
        let (class, kind) = self.private_kind(name, span).ok()?;
        if kind != PrivateKind::Method {
            return None;
        }
        let i = self.m.cands.iter().position(|c| {
            matches!(c.site, Site::Private { .. }) && c.class == Some((class, false)) && c.name == name
        })?;
        let node = self.m.cands[i].node;
        (!node.is_async() && !node.is_generator()).then_some(i)
    }

    fn private_read(&mut self, obj: Expr, name: &str, span: Span) -> R<Expr> {
        let (class, kind) = self.private_kind(name, span)?;
        let obj = Box::new(any(obj));
        let slot = self.m.private(class, name);
        Ok(Expr::new(
            Ty::Any,
            match kind {
                PrivateKind::Field => ExprKind::PrivateField(obj, slot),
                PrivateKind::Method => ExprKind::PrivateMethod(obj, slot),
                PrivateKind::Getter | PrivateKind::Accessor => ExprKind::PrivateGetter(obj, slot),
                PrivateKind::Setter => return reject(span, format!("`{name}` has no getter")),
            },
        ))
    }

    fn private_write(&mut self, obj: Expr, name: &str, v: Expr, span: Span) -> R<Expr> {
        let (class, kind) = self.private_kind(name, span)?;
        let (obj, v) = (Box::new(any(obj)), Box::new(any(v)));
        Ok(Expr::new(
            Ty::Any,
            match kind {
                PrivateKind::Field => ExprKind::PrivateSetField(obj, self.m.private(class, name), v),
                PrivateKind::Setter | PrivateKind::Accessor => {
                    ExprKind::PrivateSetter(obj, self.m.private(class, &format!("{name}<set>")), v)
                }
                PrivateKind::Method | PrivateKind::Getter => {
                    return reject(span, format!("`{name}` cannot be assigned"));
                }
            },
        ))
    }

    /// The home object slot of the enclosing method, for `super`.
    fn home(&mut self, span: Span) -> R<usize> {
        match self.class {
            Some((class, is_static)) if self.has_this => {
                if !self.m.homes_used.contains(&class) {
                    self.m.homes_used.push(class);
                }
                Ok(2 * class + is_static as usize)
            }
            _ => reject(span, "`super` is only supported in class methods"),
        }
    }

    /// The key of `super.name` / `super[e]` (`None`: not `super`).
    fn super_key(&mut self, e: &'a MemberExpression<'a>) -> R<Option<(usize, Expr)>> {
        let (object, key) = match e {
            MemberExpression::StaticMemberExpression(s) => {
                let id = self.m.string(s.property.name.as_str());
                (&s.object, Expr::new(Ty::Any, ExprKind::Str(id)))
            }
            MemberExpression::ComputedMemberExpression(c) if matches!(c.object, Expression::Super(_)) => {
                (&c.object, any(self.expr(&c.expression)?))
            }
            _ => return Ok(None),
        };
        if !matches!(object, Expression::Super(_)) {
            return Ok(None);
        }
        Ok(Some((self.home(e.span())?, key)))
    }

    /// Reading `super[key]`: through a cache when the key is a name.
    fn super_get(&mut self, e: &'a MemberExpression<'a>, home: usize, key: Expr) -> Expr {
        match e {
            MemberExpression::StaticMemberExpression(s) => {
                let atom = self.m.atom(s.property.name.as_str());
                Expr::new(Ty::Any, ExprKind::SuperGetAtom(home, atom))
            }
            _ => Expr::new(Ty::Any, ExprKind::SuperGet(home, Box::new(key))),
        }
    }

    /// `f.call(this, ...args)`: `this`, then `f` (from `this`, in a local),
    /// are evaluated before the arguments (into temporaries before any
    /// `await` in them).
    fn call_this(
        &mut self,
        this: Expr,
        f: impl FnOnce(&mut Self, Expr) -> R<Expr>,
        args: &'a [Argument<'a>],
    ) -> R<Expr> {
        let mark = self.pre.len();
        let (ft, tt) = (self.temp(Ty::Any), self.temp(Ty::Any));
        let f = f(self, self.local(tt))?;
        let call = if has_spread(args) {
            let arr = self.arg_array(args, &mut [])?;
            ExprKind::CallSpread(Box::new(self.local(ft)), Box::new(self.local(tt)), Box::new(arr))
        } else {
            let args = self.args(args)?;
            ExprKind::CallThis(Box::new(self.local(ft)), Box::new(self.local(tt)), args)
        };
        let setup = vec![set(tt, any(this)), set(ft, any(f))];
        let call = Expr::new(Ty::Any, call);
        if self.pre.len() == mark {
            let mut xs = setup;
            xs.push(call);
            return Ok(seq(xs));
        }
        self.pre.splice(mark..mark, setup.into_iter().map(Stmt::Expr));
        Ok(call)
    }

    // ---- destructuring ----

    /// Evaluate `e` for a statement appended to `out`: the statements
    /// hoisted out of it (`await`s) go to `out` first.
    fn eval_into(&mut self, e: &'a Expression<'a>, out: &mut Vec<Stmt>) -> R<Expr> {
        let mark = self.pre.len();
        let x = self.expr(e)?;
        out.extend(self.pre.split_off(mark));
        Ok(x)
    }

    /// `local === undefined`.
    fn is_undefined(&self, l: ir::LocalId) -> Expr {
        Expr::new(Ty::Bool, ExprKind::AnyCompare(AnyCmpOp::StrictEq, Box::new(self.local(l)), Box::new(undefined())))
    }

    /// Initialize the names in pattern `pat` from `v`, appending the
    /// statements to `out`.
    fn bind_pattern(&mut self, pat: &'a BindingPattern<'a>, v: Expr, kind: BindKind, out: &mut Vec<Stmt>) -> R<()> {
        match pat {
            BindingPattern::BindingIdentifier(id) => self.bind_ident(id, v, kind, out),
            BindingPattern::AssignmentPattern(p) => {
                let t = self.temp(Ty::Any);
                out.push(Stmt::Expr(set(t, any(v))));
                if let BindingPattern::BindingIdentifier(id) = &p.left {
                    self.name_hint = Some(id.name.to_string());
                }
                let mut arm = Vec::new();
                let d = self.eval_into(&p.right, &mut arm)?;
                self.name_hint = None;
                arm.push(Stmt::Expr(set(t, any(d))));
                out.push(Stmt::If(self.is_undefined(t), arm, vec![]));
                let v = self.local(t);
                self.bind_pattern(&p.left, v, kind, out)
            }
            BindingPattern::ObjectPattern(o) => {
                // An object of an `aot type` keeps its type.
                let (src, init) = match v.ty {
                    Ty::Struct(_) => (self.temp(v.ty), v),
                    Ty::StructOrUndef(k) => {
                        let t = Ty::Struct(k);
                        (self.temp(t), Expr::new(t, ExprKind::NonNull(None, Box::new(v))))
                    }
                    _ => (self.temp(Ty::Any), Expr::new(Ty::Any, ExprKind::ToObject(Box::new(any(v))))),
                };
                out.push(Stmt::Expr(set(src, init)));
                let mut excluded = Vec::new();
                for p in &o.properties {
                    let key = self.pattern_key(&p.key, p.computed, out)?;
                    let value = match &key.0 {
                        Key::Atom(a) => self.get_prop(self.local(src), *a),
                        k => get_elem(self.local(src), k.clone()),
                    };
                    excluded.push(key.1);
                    self.bind_pattern(&p.value, value, kind, out)?;
                }
                if let Some(r) = &o.rest {
                    let rest = Expr::new(Ty::Any, ExprKind::ObjectRest(Box::new(self.local(src)), excluded));
                    self.bind_pattern(&r.argument, rest, kind, out)?;
                }
                Ok(())
            }
            BindingPattern::ArrayPattern(a) => {
                let (iter, next) = self.open_iterator(v, out);
                let mut body = Vec::new();
                for el in &a.elements {
                    let value = Expr::new(Ty::Any, ExprKind::IterStep(iter, Some(next)));
                    match el {
                        None => body.push(Stmt::Expr(value)),
                        Some(p) => {
                            let t = self.temp(Ty::Any);
                            body.push(Stmt::Expr(set(t, value)));
                            let v = self.local(t);
                            self.bind_pattern(p, v, kind, &mut body)?;
                        }
                    }
                }
                if let Some(r) = &a.rest {
                    let rest = self.rest_of_iterator(iter, next, &mut body);
                    let v = self.local(rest);
                    self.bind_pattern(&r.argument, v, kind, &mut body)?;
                }
                body.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::IterClose(iter, next))));
                out.push(Stmt::IterGuard { iter, next, loop_id: None, body });
                Ok(())
            }
        }
    }

    /// Initialize one name.
    fn bind_ident(&mut self, id: &'a BindingIdentifier<'a>, v: Expr, kind: BindKind, out: &mut Vec<Stmt>) -> R<()> {
        let sym = id.symbol_id.get().unwrap();
        if self.seg_cells.contains(&sym) {
            let k = self.m.cell(sym, id.span)?;
            out.push(Stmt::Expr(init_cell(k, v, kind == BindKind::Var)));
            return Ok(());
        }
        if kind == BindKind::Var {
            let l = self.sym_local[&sym];
            out.push(Stmt::Expr(self.set_local(Some(sym), l, v, id.span)?));
            return Ok(());
        }
        let local = match self.sym_local.get(&sym) {
            Some(&l) if self.boxed[l] => l,
            _ if self.caps.captured.contains(&sym) => {
                // Not declared by a block (a parameter, `catch` or loop
                // binding): a fresh box.
                let l = self.declare_boxed(sym);
                self.mutable[l] = kind != BindKind::Const;
                out.push(Stmt::NewBoxes(vec![l]));
                l
            }
            _ => {
                let ty = if v.ty == Ty::Void || self.widened.contains(&sym) {
                    Ty::Any
                } else {
                    if v.ty == Ty::I32 { Ty::F64 } else { v.ty }
                };
                self.declare(sym, ty, kind != BindKind::Const)
            }
        };
        let ty = self.locals[local];
        out.push(Stmt::Expr(Expr::new(ty, ExprKind::InitLocal(local, Box::new(coerce(v, ty))))));
        Ok(())
    }

    /// A pattern's property key: for the property read, and as a value for
    /// the rest's excluded keys. Computed keys are converted once.
    fn pattern_key(&mut self, key: &'a PropertyKey<'a>, computed: bool, out: &mut Vec<Stmt>) -> R<(Key, Expr)> {
        let name = match key {
            PropertyKey::StaticIdentifier(id) if !computed => id.name.to_string(),
            PropertyKey::StringLiteral(s) => {
                lone_surrogates(s)?;
                s.value.to_string()
            }
            PropertyKey::NumericLiteral(n) if n.value.fract() == 0.0 && n.value >= 0.0 && n.value < 4294967295.0 => {
                format!("{}", n.value as u64)
            }
            PropertyKey::PrivateIdentifier(p) => return reject(p.span, "private names in patterns are not supported"),
            k => {
                let Some(e) = k.as_expression() else {
                    return reject(k.span(), "unsupported property key");
                };
                let x = self.eval_into(e, out)?;
                let t = self.temp(Ty::Any);
                out.push(Stmt::Expr(set(t, Expr::new(Ty::Any, ExprKind::ToPropKey(Box::new(any(x)))))));
                return Ok((Key::Value(Box::new(self.local(t))), self.local(t)));
            }
        };
        let id = self.m.string(&name);
        Ok((Key::Atom(self.m.atom(&name)), Expr::new(Ty::Any, ExprKind::Str(id))))
    }

    /// `GetIterator(v)` and its `next` method, in new locals.
    fn open_iterator(&mut self, v: Expr, out: &mut Vec<Stmt>) -> (ir::LocalId, ir::LocalId) {
        let iter = self.temp(Ty::Any);
        let next = self.temp(Ty::Any);
        let atom = self.m.atom("next");
        out.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::IterOpen(Box::new(any(v)), iter, next, atom))));
        (iter, next)
    }

    /// The rest of an iterator's values, in a new array (in a new local).
    fn rest_of_iterator(&mut self, iter: ir::LocalId, next: ir::LocalId, out: &mut Vec<Stmt>) -> ir::LocalId {
        let arr = self.temp(Ty::Any);
        out.push(Stmt::Expr(set(arr, Expr::new(Ty::Any, ExprKind::ArrayParts(vec![])))));
        let id = self.next_loop;
        self.next_loop += 1;
        let t = self.temp(Ty::Any);
        let body = vec![
            Stmt::Expr(set(t, Expr::new(Ty::Any, ExprKind::IterStep(iter, Some(next))))),
            Stmt::If(self.is_undefined(iter), vec![Stmt::Break(id)], vec![]),
            Stmt::Expr(Expr::new(Ty::Any, ExprKind::ArrayPush(arr, Box::new(self.local(t))))),
        ];
        out.push(Stmt::Loop { id, test: None, post_test: false, body, update: None, per_iteration: vec![] });
        arr
    }

    /// Assign `v` to an assignment target (a reference or a pattern).
    fn assign_target(&mut self, t: &'a AssignmentTarget<'a>, v: Expr, out: &mut Vec<Stmt>) -> R<()> {
        match t.as_simple_assignment_target() {
            Some(simple) => {
                let mark = self.pre.len();
                let (setup, place, sym) = self.place(simple, false)?;
                out.extend(self.pre.split_off(mark));
                out.extend(setup.into_iter().map(Stmt::Expr));
                out.push(Stmt::Expr(self.write(&place, sym, v, t.span())?));
                Ok(())
            }
            None => self.assign_pattern(t.as_assignment_target_pattern().unwrap(), v, out),
        }
    }

    fn assign_pattern(&mut self, p: &'a AssignmentTargetPattern<'a>, v: Expr, out: &mut Vec<Stmt>) -> R<()> {
        match p {
            AssignmentTargetPattern::ObjectAssignmentTarget(o) => {
                let src = self.temp(Ty::Any);
                out.push(Stmt::Expr(set(src, Expr::new(Ty::Any, ExprKind::ToObject(Box::new(any(v)))))));
                let mut excluded = Vec::new();
                for prop in &o.properties {
                    match prop {
                        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                            let name = p.binding.name.as_str();
                            let atom = self.m.atom(name);
                            let sid = self.m.string(name);
                            excluded.push(Expr::new(Ty::Any, ExprKind::Str(sid)));
                            let value = Expr::new(Ty::Any, ExprKind::GetProp(Box::new(self.local(src)), atom));
                            let (setup, place, sym) = self.ident_place(&p.binding)?;
                            out.extend(setup.into_iter().map(Stmt::Expr));
                            let value = self.with_default(value, p.init.as_ref(), Some(name), out)?;
                            out.push(Stmt::Expr(self.write(&place, sym, value, p.span)?));
                        }
                        AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            let (key, kv) = self.pattern_key(&p.name, p.computed, out)?;
                            excluded.push(kv);
                            self.assign_element(&p.binding, |cx| get_elem(cx.local(src), key.clone()), out)?;
                        }
                    }
                }
                if let Some(r) = &o.rest {
                    let rest = Expr::new(Ty::Any, ExprKind::ObjectRest(Box::new(self.local(src)), excluded));
                    self.assign_target(&r.target, rest, out)?;
                }
                Ok(())
            }
            AssignmentTargetPattern::ArrayAssignmentTarget(a) => {
                let (iter, next) = self.open_iterator(v, out);
                let mut body = Vec::new();
                for el in &a.elements {
                    let step = |_: &mut Self| Expr::new(Ty::Any, ExprKind::IterStep(iter, Some(next)));
                    match el {
                        None => body.push(Stmt::Expr(step(self))),
                        Some(t) => self.assign_element(t, step, &mut body)?,
                    }
                }
                if let Some(r) = &a.rest {
                    // The target reference is evaluated first.
                    match r.target.as_simple_assignment_target() {
                        Some(simple) => {
                            let mark = self.pre.len();
                            let (setup, place, sym) = self.place(simple, false)?;
                            body.extend(self.pre.split_off(mark));
                            body.extend(setup.into_iter().map(Stmt::Expr));
                            let rest = self.rest_of_iterator(iter, next, &mut body);
                            let v = self.local(rest);
                            body.push(Stmt::Expr(self.write(&place, sym, v, r.span)?));
                        }
                        None => {
                            let rest = self.rest_of_iterator(iter, next, &mut body);
                            let v = self.local(rest);
                            self.assign_target(&r.target, v, &mut body)?;
                        }
                    }
                }
                body.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::IterClose(iter, next))));
                out.push(Stmt::IterGuard { iter, next, loop_id: None, body });
                Ok(())
            }
        }
    }

    /// An element of an assignment pattern: a simple target's reference is
    /// evaluated before its value is read (by `read`), then the default.
    fn assign_element(
        &mut self,
        t: &'a AssignmentTargetMaybeDefault<'a>,
        read: impl FnOnce(&mut Self) -> Expr,
        out: &mut Vec<Stmt>,
    ) -> R<()> {
        let (target, init) = match t {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => (&d.binding, Some(&d.init)),
            other => (other.as_assignment_target().unwrap(), None),
        };
        let hint = match target {
            AssignmentTarget::AssignmentTargetIdentifier(id) => Some(id.name.as_str()),
            _ => None,
        };
        match target.as_simple_assignment_target() {
            Some(simple) => {
                let mark = self.pre.len();
                let (setup, place, sym) = self.place(simple, false)?;
                out.extend(self.pre.split_off(mark));
                out.extend(setup.into_iter().map(Stmt::Expr));
                let value = read(self);
                let value = self.with_default(value, init, hint, out)?;
                out.push(Stmt::Expr(self.write(&place, sym, value, target.span())?));
            }
            None => {
                let value = read(self);
                let value = self.with_default(value, init, hint, out)?;
                self.assign_pattern(target.as_assignment_target_pattern().unwrap(), value, out)?;
            }
        }
        Ok(())
    }

    /// `v`, or `init` if it is `undefined`.
    fn with_default(
        &mut self,
        v: Expr,
        init: Option<&'a Expression<'a>>,
        name: Option<&str>,
        out: &mut Vec<Stmt>,
    ) -> R<Expr> {
        let Some(init) = init else { return Ok(v) };
        let t = self.temp(Ty::Any);
        out.push(Stmt::Expr(set(t, any(v))));
        self.name_hint = name.map(str::to_string);
        let mut arm = Vec::new();
        let d = self.eval_into(init, &mut arm)?;
        self.name_hint = None;
        arm.push(Stmt::Expr(set(t, any(d))));
        out.push(Stmt::If(self.is_undefined(t), arm, vec![]));
        Ok(self.local(t))
    }

    // ---- for-of / for-in ----

    fn for_in_of(&mut self, s: &'a Statement<'a>, label: Option<String>) -> R<Stmt> {
        let (left, right, stmt_body, is_of) = match s {
            Statement::ForOfStatement(f) if f.r#await => return self.for_await(f, label),
            Statement::ForOfStatement(f) => (&f.left, &f.right, &f.body, true),
            Statement::ForInStatement(f) => (&f.left, &f.right, &f.body, false),
            _ => unreachable!(),
        };
        let mut out = Vec::new();
        // The iterated value (with `let` / `const` bindings in their TDZ).
        let v = self.eval_into(right, &mut out)?;
        // (for-in: the object, and the index in its shape's keys)
        let (iter, next, idx, obj) = if is_of {
            let (i, n) = self.open_iterator(v, &mut out);
            (i, Some(n), 0, 0)
        } else {
            let (i, idx, o) = (self.temp(Ty::Any), self.temp(Ty::Any), self.temp(Ty::Any));
            out.push(Stmt::Expr(set(o, any(v))));
            let start = ExprKind::ForInIterator(Box::new(self.local(o)), idx);
            out.push(Stmt::Expr(set(i, Expr::new(Ty::Any, start))));
            (i, None, idx, o)
        };
        let lp = self.lp(label, JumpKind::Loop, |cx, id| {
            let value = cx.temp(Ty::Any);
            let step = if is_of { ExprKind::IterStep(iter, next) } else { ExprKind::ForInStep(iter, idx, obj) };
            let mut body = vec![
                Stmt::Expr(set(value, Expr::new(Ty::Any, step))),
                Stmt::If(cx.is_undefined(iter), vec![Stmt::Break(id)], vec![]),
            ];
            // Each iteration binds the loop variable anew.
            let mut inner = Vec::new();
            let v = cx.local(value);
            match left {
                ForStatementLeft::VariableDeclaration(d) => {
                    let kind = match d.kind {
                        VariableDeclarationKind::Let => BindKind::Let,
                        VariableDeclarationKind::Const => BindKind::Const,
                        VariableDeclarationKind::Var => BindKind::Var,
                        _ => return reject(d.span, "`using` declarations are not supported"),
                    };
                    cx.bind_pattern(&d.declarations[0].id, v, kind, &mut inner)?;
                }
                target => cx.assign_target(target.to_assignment_target(), v, &mut inner)?,
            }
            // `obj[key]` with the key of a `for`-`in` may read it directly.
            let key_sym = match left {
                _ if is_of => None,
                ForStatementLeft::VariableDeclaration(d) => match &d.declarations[0].id {
                    BindingPattern::BindingIdentifier(id) => id.symbol_id.get(),
                    _ => None,
                },
                ForStatementLeft::AssignmentTargetIdentifier(id) => cx.resolve(id),
                _ => None,
            };
            if let Some(sym) = key_sym {
                cx.for_in_keys.push((sym, iter, idx));
            }
            let r = cx.body(stmt_body);
            if key_sym.is_some() {
                cx.for_in_keys.pop();
            }
            inner.extend(r?);
            if is_of {
                body.push(Stmt::IterGuard { iter, next: next.unwrap(), loop_id: Some(id), body: inner });
            } else {
                body.extend(inner);
            }
            Ok(Stmt::Loop { id, test: None, post_test: false, body, update: None, per_iteration: vec![] })
        })?;
        out.push(lp);
        Ok(Stmt::Block(out))
    }

    /// `for await (x of v)`, as the interpreter runs it: each result of the
    /// async iterator's `next` is awaited and must be an object; leaving
    /// the loop early (`break`, `return`, a jump out) calls `return` and
    /// awaits it; an exception calls `return` without awaiting it, ignoring
    /// what that does.
    fn for_await(&mut self, f: &'a ForOfStatement<'a>, label: Option<String>) -> R<Stmt> {
        if !self.is_async {
            return reject(f.span, "`for await` outside an async function");
        }
        let mut out = Vec::new();
        let v = self.eval_into(&f.right, &mut out)?;
        let iter = self.temp(Ty::Any);
        let next = self.temp(Ty::Any);
        let it = Expr::new(Ty::Any, ExprKind::AsyncIterator(Box::new(any(v))));
        out.push(Stmt::Expr(set(iter, it)));
        let next_atom = self.m.atom("next");
        out.push(Stmt::Expr(set(next, Expr::new(Ty::Any, ExprKind::GetProp(Box::new(self.local(iter)), next_atom)))));
        let lp = self.lp(label, JumpKind::Loop, |cx, id| {
            let res = cx.temp(Ty::Any);
            let done = cx.temp(Ty::Bool);
            let value = cx.temp(Ty::Any);
            let call =
                Expr::new(Ty::Any, ExprKind::CallThis(Box::new(cx.local(next)), Box::new(cx.local(iter)), Vec::new()));
            let result = Expr::new(Ty::Any, ExprKind::AsyncIterResult(Box::new(cx.local(res)), done));
            let mut body = vec![
                Stmt::Await { value: call, target: ir::AwaitTarget::Set(res) },
                Stmt::Expr(set(value, result)),
                // Done: nothing to close.
                Stmt::If(cx.local(done), vec![Stmt::Expr(set(iter, undefined())), Stmt::Break(id)], vec![]),
            ];
            let v = cx.local(value);
            match &f.left {
                ForStatementLeft::VariableDeclaration(d) => {
                    let kind = match d.kind {
                        VariableDeclarationKind::Let => BindKind::Let,
                        VariableDeclarationKind::Const => BindKind::Const,
                        VariableDeclarationKind::Var => BindKind::Var,
                        _ => return reject(d.span, "`using` declarations are not supported"),
                    };
                    cx.bind_pattern(&d.declarations[0].id, v, kind, &mut body)?;
                }
                target => cx.assign_target(target.to_assignment_target(), v, &mut body)?,
            }
            body.extend(cx.body(&f.body)?);
            Ok(Stmt::Loop { id, test: None, post_test: false, body, update: None, per_iteration: vec![] })
        })?;
        // An exception: `return` (if any) called and its result awaited,
        // what either throws ignored, then the exception rethrown (the
        // spec's AsyncIteratorClose of a throw completion).
        let exc = self.temp(Ty::Any);
        let return_atom = self.m.atom("return");
        let quiet_method = self.temp(Ty::Any);
        let quiet_r = self.temp(Ty::Any);
        let quiet_nullish = Expr::new(
            Ty::Bool,
            ExprKind::AnyCompare(
                AnyCmpOp::Eq,
                Box::new(self.local(quiet_method)),
                Box::new(Expr::new(Ty::Any, ExprKind::Null)),
            ),
        );
        let quiet_call = Expr::new(
            Ty::Any,
            ExprKind::CallThis(Box::new(self.local(quiet_method)), Box::new(self.local(iter)), Vec::new()),
        );
        let quiet_close = vec![
            Stmt::Expr(set(quiet_method, Expr::new(Ty::Any, ExprKind::GetProp(Box::new(self.local(iter)), return_atom)))),
            Stmt::If(quiet_nullish, vec![], vec![Stmt::Await { value: quiet_call, target: ir::AwaitTarget::Set(quiet_r) }]),
        ];
        let on_throw = vec![
            Stmt::Try { body: quiet_close, catch: Some((None, vec![])), finally: None, exc: None },
            Stmt::Expr(set(iter, undefined())),
            Stmt::Throw(self.local(exc)),
        ];
        // Leaving early: `return` (if any), awaited, must give an object.
        let method = self.temp(Ty::Any);
        let r = self.temp(Ty::Any);
        let get_return = Expr::new(Ty::Any, ExprKind::GetProp(Box::new(self.local(iter)), return_atom));
        let nullish = Expr::new(
            Ty::Bool,
            ExprKind::AnyCompare(
                AnyCmpOp::Eq,
                Box::new(self.local(method)),
                Box::new(Expr::new(Ty::Any, ExprKind::Null)),
            ),
        );
        let call_return = Expr::new(
            Ty::Any,
            ExprKind::CallThis(Box::new(self.local(method)), Box::new(self.local(iter)), Vec::new()),
        );
        let close = vec![
            Stmt::Expr(set(method, get_return)),
            Stmt::If(
                nullish,
                vec![],
                vec![
                    Stmt::Await { value: call_return, target: ir::AwaitTarget::Set(r) },
                    Stmt::Expr(Expr::new(Ty::Any, ExprKind::CheckObject(Box::new(self.local(r))))),
                ],
            ),
        ];
        let not_done = Expr::new(Ty::Bool, ExprKind::Unary(UnOp::Not, Box::new(self.is_undefined(iter))));
        let finally = vec![Stmt::If(not_done, close, vec![])];
        let suspend_exc = self.temp(Ty::Any);
        out.push(Stmt::Try {
            body: vec![lp],
            catch: Some((Some(exc), on_throw)),
            finally: Some(finally),
            exc: Some(suspend_exc),
        });
        Ok(Stmt::Block(out))
    }

    // ---- expressions ----

    /// An expression used as a condition: converted with JS truthiness.
    fn cond(&mut self, e: &'a Expression<'a>) -> R<Expr> {
        let x = self.expr(e)?;
        Ok(truthy(x))
    }

    fn expr(&mut self, e: &'a Expression<'a>) -> R<Expr> {
        let span = e.span();
        // An array of objects of an `aot type`.
        if matches!(e, Expression::ArrayExpression(_) | Expression::NewExpression(_) | Expression::CallExpression(_))
            && let Some(&(end, k)) = self.m.array_sites.get(&span.start)
            && end == span.end
        {
            let arr = any(self.expr_plain(e)?);
            return Ok(Expr::new(Ty::StructArr(k), ExprKind::StructArrFrom(k, Box::new(arr))));
        }
        // `JSON.parse(text)` converted to an `aot type`.
        if let Expression::CallExpression(_) = e
            && let Some(&(end, desc)) = self.m.parse_sites.get(&span.start)
            && end == span.end
        {
            let call = any(self.expr_plain(e)?);
            return Ok(Expr::new(desc_ty(desc), ExprKind::StructConvert(desc, Box::new(call))));
        }
        // `await r.json()` converted to one: awaited (as `await` is), then
        // converted.
        if let Expression::AwaitExpression(aw) = e
            && let Some(desc) = self.json_site(aw)
        {
            let Expression::CallExpression(c) = aw.argument.without_parentheses() else { unreachable!() };
            let Expression::StaticMemberExpression(m) = c.callee.without_parentheses() else { unreachable!() };
            let recv = any(self.expr(&m.object)?);
            let value = Expr::new(Ty::Any, ExprKind::JsonMethodAs(desc, Box::new(recv)));
            let t = self.temp(Ty::Any);
            self.pre.push(Stmt::Await { value, target: ir::AwaitTarget::Set(t) });
            return Ok(Expr::new(desc_ty(desc), ExprKind::StructConvert(desc, Box::new(self.local(t)))));
        }
        self.expr_plain(e)
    }

    /// The desc of `await r.json()` converted to an `aot type`, if it is.
    fn json_site(&self, aw: &AwaitExpression) -> Option<i32> {
        self.m.json_sites.get(&aw.span.start).filter(|&&(e, _)| e == aw.span.end).map(|&(_, d)| d)
    }

    fn expr_plain(&mut self, e: &'a Expression<'a>) -> R<Expr> {
        let span = e.span();
        // Only the expression right after a binding is named by it.
        let name_hint = self.name_hint.take();
        Ok(match e {
            Expression::ArrowFunctionExpression(a) => self.closure(FnNode::Arrow(a), name_hint.unwrap_or_default())?,
            Expression::FunctionExpression(f) => {
                let name = f.id.as_ref().map(|id| id.name.to_string()).or(name_hint).unwrap_or_default();
                self.closure(FnNode::Function(f), name)?
            }
            Expression::ClassExpression(c) => {
                let name = c.id.as_ref().map(|id| id.name.to_string()).or(name_hint).unwrap_or_default();
                self.class_value(c, name)?
            }
            Expression::NumericLiteral(n) => Expr::num(n.value),
            Expression::ThisExpression(_) if self.has_this => Expr::new(Ty::Any, ExprKind::This),
            Expression::ThisExpression(_) if let Some(l) = self.this_local => self.local(l),
            Expression::ThisExpression(_) if let Some(b) = self.this_box => self.local(b),
            Expression::AwaitExpression(aw) => {
                // Hoisted into a statement that runs before the current one,
                // with no expression temporaries live across the suspension.
                let value = any(self.expr(&aw.argument)?);
                let t = self.temp(Ty::Any);
                self.pre.push(Stmt::Await { value, target: ir::AwaitTarget::Set(t) });
                self.local(t)
            }
            Expression::YieldExpression(y) => {
                // Hoisted as `await` is.
                let t = self.temp(Ty::Any);
                let st = self.yield_stmt(y, ir::AwaitTarget::Set(t))?;
                self.pre.push(st);
                self.local(t)
            }
            Expression::BooleanLiteral(b) => Expr::new(Ty::Bool, ExprKind::Bool(b.value)),
            Expression::StringLiteral(s) => {
                lone_surrogates(s)?;
                let id = self.m.string(s.value.as_str());
                Expr::new(Ty::Any, ExprKind::Str(id))
            }
            Expression::NullLiteral(_) => Expr::new(Ty::Any, ExprKind::Null),
            Expression::RegExpLiteral(r) => {
                let pattern = self.m.string(r.regex.pattern.text.as_str());
                let flags: String = "dgimsuvy"
                    .chars()
                    .zip([
                        RegExpFlags::D,
                        RegExpFlags::G,
                        RegExpFlags::I,
                        RegExpFlags::M,
                        RegExpFlags::S,
                        RegExpFlags::U,
                        RegExpFlags::V,
                        RegExpFlags::Y,
                    ])
                    .filter(|(_, f)| r.regex.flags.contains(*f))
                    .map(|(c, _)| c)
                    .collect();
                let flags = self.m.string(&flags);
                Expr::new(Ty::Any, ExprKind::RegExp(pattern, flags))
            }
            Expression::TemplateLiteral(t) => self.template(t)?,
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression)?,
            Expression::Identifier(id) => self.ident(id)?,
            Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_)
                if let Some((home, key)) = self.super_key(e.as_member_expression().unwrap())? =>
            {
                self.super_get(e.as_member_expression().unwrap(), home, key)
            }
            Expression::PrivateFieldExpression(p) => {
                if p.optional {
                    return reject(span, "optional chaining is not supported yet");
                }
                let obj = self.expr(&p.object)?;
                self.private_read(obj, &format!("#{}", p.field.name), span)?
            }
            Expression::PrivateInExpression(p) => {
                let name = format!("#{}", p.left.name);
                let (class, kind) = self.private_kind(&name, span)?;
                if kind == PrivateKind::Setter {
                    return reject(span, format!("`{name} in` of a setter is not supported"));
                }
                let obj = any(self.expr(&p.right)?);
                let slot = self.m.private(class, &name);
                Expr::new(Ty::Bool, ExprKind::PrivateIn(Box::new(obj), slot))
            }
            Expression::StaticMemberExpression(m) => match builtin_const(m, self.m.scoping) {
                Some(v) => Expr::num(v),
                None => {
                    if m.optional {
                        return reject(span, "optional chaining is not supported yet");
                    }
                    // `arguments.length` of a function that reads only that:
                    // its parameter is the count.
                    if self.args_count
                        && m.property.name == "length"
                        && let Expression::Identifier(id) = m.object.without_parentheses()
                        && is_arguments(id, self.m.scoping)
                        && let Some(l) = self.args_local
                    {
                        return Ok(self.local(l));
                    }
                    // `Object.keys(x).length`: counted, without the array.
                    if m.property.name == "length"
                        && let Expression::CallExpression(c) = m.object.without_parentheses()
                        && !c.optional
                        && let Expression::StaticMemberExpression(cm) = c.callee.without_parentheses()
                        && global_member(cm, self.m.scoping) == Some(("Object", "keys"))
                        && let [arg] = &c.arguments[..]
                        && let Some(arg) = arg.as_expression()
                    {
                        let obj = Expr::new(Ty::Any, ExprKind::Global(self.m.atom("Object")));
                        let keys = self.m.atom("keys");
                        let arg = any(self.expr(arg)?);
                        return Ok(Expr::new(Ty::Any, ExprKind::KeysLength(Box::new(obj), keys, Box::new(arg))));
                    }
                    let obj = self.expr(&m.object)?;
                    let atom = self.m.atom(m.property.name.as_str());
                    if self.struct_field(obj.ty, atom).is_some() || matches!(obj.ty, Ty::StructOrUndef(_)) {
                        self.get_prop(obj, atom)
                    } else if m.property.name == "length" && matches!(obj.ty, Ty::StructArr(_)) {
                        Expr::new(Ty::F64, ExprKind::StructArrLen(Box::new(obj)))
                    } else if m.property.name == "length" {
                        Expr::new(Ty::Any, ExprKind::Length(Box::new(any(obj)), atom))
                    } else {
                        Expr::new(Ty::Any, ExprKind::GetProp(Box::new(any(obj)), atom))
                    }
                }
            },
            Expression::ComputedMemberExpression(m) => {
                if m.optional {
                    return reject(span, "optional chaining is not supported yet");
                }
                if let Expression::Identifier(id) = m.expression.without_parentheses()
                    && let Some(sym) = self.resolve(id)
                    && let Some(&(_, iter, idx)) = self.for_in_keys.iter().rev().find(|(s, ..)| *s == sym)
                {
                    let mut obj = [any(self.expr(&m.object)?)];
                    let mark = self.pre.len();
                    let key = any(self.ident(id)?);
                    self.order(&mut obj, mark);
                    let [obj] = obj;
                    return Ok(Expr::new(Ty::Any, ExprKind::ForInGet(iter, idx, Box::new(obj), Box::new(key))));
                }
                let mut obj = [self.expr(&m.object)?];
                let mark = self.pre.len();
                let key = self.elem_key(&m.expression)?;
                self.order(&mut obj, mark);
                let [obj] = obj;
                get_elem(obj, key)
            }
            Expression::UnaryExpression(u) => self.unary(u)?,
            Expression::BinaryExpression(b) => self.binary(b)?,
            Expression::LogicalExpression(l) => {
                let a = self.expr(&l.left)?;
                let mark = self.pre.len();
                let b = self.expr(&l.right)?;
                let op = match l.operator {
                    LogicalOperator::And => LogicalOp::And,
                    LogicalOperator::Or => LogicalOp::Or,
                    // A number or boolean is never nullish.
                    LogicalOperator::Coalesce if !matches!(a.ty, Ty::Any | Ty::MaybeF64) => {
                        self.pre.truncate(mark);
                        return Ok(a);
                    }
                    LogicalOperator::Coalesce => LogicalOp::Nullish,
                };
                if self.pre.len() > mark {
                    // The right operand awaits: evaluate it in an `if`.
                    let t = self.temp(Ty::Any);
                    let arm = self.conditional_arm(mark, b, t);
                    self.pre.push(Stmt::Expr(set(t, any(a))));
                    let cur = self.local(t);
                    let cond = match op {
                        LogicalOp::And => truthy(cur),
                        LogicalOp::Or => Expr::new(Ty::Bool, ExprKind::Unary(UnOp::Not, Box::new(truthy(cur)))),
                        LogicalOp::Nullish => Expr::new(
                            Ty::Bool,
                            ExprKind::AnyCompare(
                                AnyCmpOp::Eq,
                                Box::new(cur),
                                Box::new(Expr::new(Ty::Any, ExprKind::Null)),
                            ),
                        ),
                    };
                    self.pre.push(Stmt::If(cond, arm, vec![]));
                    return Ok(self.local(t));
                }
                let ty = join(a.ty, b.ty);
                let ty = if ty == Ty::Void { Ty::Any } else { ty };
                Expr::new(ty, ExprKind::Logical(op, Box::new(coerce(a, ty)), Box::new(coerce(b, ty))))
            }
            Expression::ConditionalExpression(c) => {
                let test = self.cond(&c.test)?;
                let mark = self.pre.len();
                let a = self.expr(&c.consequent)?;
                let a_end = self.pre.len();
                let b = self.expr(&c.alternate)?;
                if self.pre.len() > mark {
                    // An arm awaits: evaluate the arms in an `if`.
                    let t = self.temp(Ty::Any);
                    let b_arm = self.conditional_arm(a_end, b, t);
                    let a_arm = self.conditional_arm(mark, a, t);
                    self.pre.push(Stmt::If(test, a_arm, b_arm));
                    return Ok(self.local(t));
                }
                let ty = join(a.ty, b.ty);
                Expr::new(ty, ExprKind::Cond(Box::new(test), Box::new(coerce(a, ty)), Box::new(coerce(b, ty))))
            }
            Expression::AssignmentExpression(a) => self.assign(a)?,
            Expression::UpdateExpression(u) => self.update(u, true)?,
            Expression::SequenceExpression(s) => {
                let mut xs: Vec<Expr> = Vec::new();
                for x in &s.expressions {
                    let mark = self.pre.len();
                    let v = self.expr(x)?;
                    self.order(&mut xs, mark);
                    xs.push(v);
                }
                seq(xs)
            }
            Expression::CallExpression(c) => self.call(c)?,
            Expression::ChainExpression(c) => self.chain(c)?,
            Expression::NewExpression(n) if has_spread(&n.arguments) => {
                let mut earlier = [any(self.expr(&n.callee)?)];
                let arr = self.arg_array(&n.arguments, &mut earlier)?;
                let [callee] = earlier;
                Expr::new(Ty::Any, ExprKind::NewSpread(Box::new(callee), Box::new(arr)))
            }
            Expression::NewExpression(n) if let Some(x) = self.native_new(n)? => x,
            Expression::NewExpression(n) => {
                let mut v = vec![any(self.expr(&n.callee)?)];
                self.push_args(&mut v, &n.arguments)?;
                let callee = v.remove(0);
                // The typed array constructors are locked globals: these
                // always create one of their kind.
                let ty = match n.callee.without_parentheses() {
                    Expression::Identifier(id) if self.resolve(id).is_none() => {
                        ArrKind::from_name(id.name.as_str()).map_or(Ty::Any, Ty::Arr)
                    }
                    _ => Ty::Any,
                };
                Expr::new(ty, ExprKind::New(Box::new(callee), v))
            }
            Expression::ObjectExpression(o) => self.object(o)?,
            Expression::ArrayExpression(a) if a.elements.iter().any(|e| e.as_expression().is_none()) => {
                let mut parts = Vec::new();
                let mut values = Vec::new();
                for el in &a.elements {
                    match el {
                        ArrayExpressionElement::Elision(_) => parts.push(None),
                        ArrayExpressionElement::SpreadElement(s) => {
                            let mark = self.pre.len();
                            let v = any(self.expr(&s.argument)?);
                            self.order(&mut values, mark);
                            values.push(v);
                            parts.push(Some(true));
                        }
                        other => {
                            let mark = self.pre.len();
                            let v = any(self.expr(other.to_expression())?);
                            self.order(&mut values, mark);
                            values.push(v);
                            parts.push(Some(false));
                        }
                    }
                }
                array_parts(parts, values)
            }
            Expression::ArrayExpression(a) => {
                let mut elems = Vec::new();
                for el in &a.elements {
                    match el.as_expression() {
                        Some(x) => {
                            let mark = self.pre.len();
                            let v = any(self.expr(x)?);
                            self.order(&mut elems, mark);
                            elems.push(v);
                        }
                        None => return reject(el.span(), "array holes and spreads are not supported yet"),
                    }
                }
                Expr::new(Ty::Any, ExprKind::Array(elems))
            }
            // Type-level wrappers have no runtime effect; their types are
            // not trusted.
            // (Up to 2^64 - 1; a larger one stays interpreted.)
            Expression::BigIntLiteral(b) => match b.value.as_str().parse::<u64>() {
                Ok(v) => Expr::new(Ty::Any, ExprKind::BigInt(v)),
                Err(_) => return reject(span, "BigInt literals of 2^64 and more are not supported"),
            },
            Expression::TSNonNullExpression(n) => self.expr(&n.expression)?,
            Expression::TSAsExpression(a) => self.expr(&a.expression)?,
            Expression::TSSatisfiesExpression(a) => self.expr(&a.expression)?,
            Expression::TSTypeAssertion(a) => self.expr(&a.expression)?,
            Expression::TSInstantiationExpression(a) => self.expr(&a.expression)?,
            other => return reject(span, format!("unsupported expression: {}", describe_expr(other))),
        })
    }

    /// An optional chain. Its links are evaluated from the base outwards;
    /// at each `?.`, the value so far goes into a temporary, and if it is
    /// nullish the whole rest of the chain is skipped (`undefined`).
    fn chain(&mut self, c: &'a ChainExpression<'a>) -> R<Expr> {
        enum Link<'a> {
            Prop(&'a IdentifierName<'a>, bool),
            Elem(&'a Expression<'a>, bool),
            Call(&'a [Argument<'a>], bool),
        }
        let mut links = Vec::new();
        let mut cur: &'a Expression<'a> = match &c.expression {
            ChainElement::CallExpression(e) => {
                links.push(Link::Call(&e.arguments, e.optional));
                &e.callee
            }
            ChainElement::StaticMemberExpression(m) => {
                links.push(Link::Prop(&m.property, m.optional));
                &m.object
            }
            ChainElement::ComputedMemberExpression(m) => {
                links.push(Link::Elem(&m.expression, m.optional));
                &m.object
            }
            ChainElement::TSNonNullExpression(n) => &n.expression,
            ChainElement::PrivateFieldExpression(p) => {
                return reject(p.span, "private fields are not supported yet");
            }
        };
        // Parentheses end a chain: `(a?.b).c` reads `c` of `undefined`.
        loop {
            cur = match cur {
                Expression::StaticMemberExpression(m) => {
                    links.push(Link::Prop(&m.property, m.optional));
                    &m.object
                }
                Expression::ComputedMemberExpression(m) => {
                    links.push(Link::Elem(&m.expression, m.optional));
                    &m.object
                }
                Expression::CallExpression(e) => {
                    links.push(Link::Call(&e.arguments, e.optional));
                    &e.callee
                }
                Expression::TSNonNullExpression(n) => &n.expression,
                _ => break,
            };
        }
        links.reverse();
        if matches!(cur, Expression::Super(_)) {
            return reject(cur.span(), "`super` is not supported yet");
        }

        // Guards: (temporary, its value, what to evaluate before it).
        let mut guards: Vec<(ir::LocalId, Expr, Vec<Expr>)> = Vec::new();
        let mut val = self.expr(cur)?;
        let mut mark = None;
        let mut i = 0;
        while i < links.len() {
            let optional = match links[i] {
                Link::Prop(_, o) | Link::Elem(_, o) | Link::Call(_, o) => o,
            };
            if optional {
                let t = self.temp(Ty::Any);
                guards.push((t, any(val), vec![]));
                val = self.local(t);
                mark.get_or_insert(self.pre.len());
            }
            let key = match links[i] {
                Link::Prop(name, _) => Some(Key::Atom(self.m.atom(name.name.as_str()))),
                Link::Elem(k, _) => Some(self.elem_key(k)?),
                Link::Call(..) => None,
            };
            match (key, links.get(i + 1)) {
                // A method call: the object is `this`.
                (Some(key), Some(&Link::Call(args, call_optional))) => {
                    if call_optional {
                        let o = self.temp(Ty::Any);
                        let m = self.temp(Ty::Any);
                        let method = get_elem(self.local(o), key);
                        guards.push((m, any(method), vec![set(o, any(val))]));
                        mark.get_or_insert(self.pre.len());
                        let mut v = vec![self.local(m), self.local(o)];
                        self.push_args(&mut v, args)?;
                        let (f, this) = (v.remove(0), v.remove(0));
                        val = Expr::new(Ty::Any, ExprKind::CallThis(Box::new(f), Box::new(this), v));
                    } else {
                        val = self.call_method(any(val), key, args)?;
                    }
                    i += 2;
                    continue;
                }
                (Some(Key::Atom(a)), _) if matches!(links[i], Link::Prop(n, _) if n.name == "length") => {
                    val = Expr::new(Ty::Any, ExprKind::Length(Box::new(any(val)), a));
                }
                (Some(key), _) => val = get_elem(val, key),
                (None, _) => {
                    let Link::Call(args, _) = links[i] else { unreachable!() };
                    val = self.call_value(any(val), args)?;
                }
            }
            i += 1;
        }
        if mark.is_some_and(|m| self.pre.len() > m) {
            return reject(c.span, "`await` inside an optional chain is not supported yet");
        }
        let mut result = val;
        for (t, v, mut before) in guards.into_iter().rev() {
            let nullish = Expr::new(
                Ty::Bool,
                ExprKind::AnyCompare(
                    AnyCmpOp::Eq,
                    Box::new(self.local(t)),
                    Box::new(Expr::new(Ty::Any, ExprKind::Null)),
                ),
            );
            let cond =
                Expr::new(Ty::Any, ExprKind::Cond(Box::new(nullish), Box::new(undefined()), Box::new(any(result))));
            before.push(set(t, v));
            before.push(cond);
            result = seq(before);
        }
        Ok(result)
    }

    fn template(&mut self, t: &'a TemplateLiteral<'a>) -> R<Expr> {
        // `a${x}b` is "a" + ToString(x) + "b": strings concatenate exactly.
        let mut parts: Vec<Expr> = Vec::new();
        for (i, q) in t.quasis.iter().enumerate() {
            let Some(cooked) = &q.value.cooked else {
                return reject(q.span, "invalid escape in template literal");
            };
            if q.lone_surrogates {
                return reject(q.span, LONE_SURROGATES);
            }
            if i == 0 || !cooked.is_empty() {
                let id = self.m.string(cooked.as_str());
                parts.push(Expr::new(Ty::Any, ExprKind::Str(id)));
            }
            if let Some(e) = t.expressions.get(i) {
                let mark = self.pre.len();
                let x = any(self.expr(e)?);
                self.order(&mut parts, mark);
                parts.push(Expr::new(Ty::Any, ExprKind::AnyUnary(AnyUnOp::ToStr, Box::new(x))));
            }
        }
        let mut it = parts.into_iter();
        let first = it.next().unwrap();
        Ok(it
            .fold(first, |a, part| Expr::new(Ty::Any, ExprKind::AnyBinary(AnyBinOp::Add, Box::new(a), Box::new(part)))))
    }

    /// An object literal that makes an object of `aot type` `k` (see
    /// `structs`, which checked its fields).
    fn struct_literal(&mut self, o: &'a ObjectExpression<'a>, k: u32) -> R<Expr> {
        let mut fields: Vec<u32> = Vec::new();
        let mut values: Vec<Expr> = Vec::new();
        let mut mask = 0u32;
        for p in &o.properties {
            let ObjectPropertyKind::ObjectProperty(p) = p else { unreachable!("checked by `structs`") };
            let name = match &p.key {
                PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                PropertyKey::StringLiteral(s) => s.value.to_string(),
                _ => unreachable!("checked by `structs`"),
            };
            let i = self.m.layouts[k as usize].fields.iter().position(|f| f.name == name).unwrap() as u32;
            let mark = self.pre.len();
            let v = self.expr(&p.value)?;
            let v = self.struct_value(k, i, v);
            self.order(&mut values, mark);
            values.push(v);
            fields.push(i);
            mask |= 1 << i;
        }
        let fields = fields.into_iter().zip(values).collect();
        Ok(Expr::new(Ty::Struct(k), ExprKind::StructNew { layout: k, mask, fields }))
    }

    /// `v` as a value for field `i` of layout `k`: numbers and booleans as
    /// they are for unboxed fields, else a JS value.
    fn struct_value(&self, k: u32, i: u32, v: Expr) -> Expr {
        match self.m.layouts[k as usize].fields[i as usize].storage {
            // (`undefined` as a `MaybeF64` is not a number: the runtime checks.)
            ir::Storage::Int32 | ir::Storage::F64 if v.ty == Ty::MaybeF64 => any(v),
            ir::Storage::Int32 | ir::Storage::F64 if num(v.ty) || v.ty == Ty::Any => to_f64(v),
            ir::Storage::Bool if matches!(v.ty, Ty::Bool | Ty::Any) => v,
            _ => any(v),
        }
    }

    /// Field `atom` of an object of type `ty`, if an `aot type` with it.
    fn struct_field(&self, ty: Ty, atom: ir::AtomId) -> Option<(u32, u32)> {
        let Ty::Struct(k) = ty else { return None };
        let name = &self.m.atoms[atom];
        let i = self.m.layouts[k as usize].fields.iter().position(|f| f.name == *name)?;
        Some((k, i as u32))
    }

    /// `obj.atom`: a field of an `aot type`, else a property.
    fn get_prop(&self, obj: Expr, atom: ir::AtomId) -> Expr {
        // An element of an array of an `aot type`: an object, else what JS
        // throws reading a property of `undefined`.
        if let Ty::StructOrUndef(k) = obj.ty
            && self.struct_field(Ty::Struct(k), atom).is_some()
        {
            let obj = Expr::new(Ty::Struct(k), ExprKind::NonNull(Some(atom), Box::new(obj)));
            return self.get_prop(obj, atom);
        }
        if let Some((k, i)) = self.struct_field(obj.ty, atom) {
            let ty = self.m.layouts[k as usize].fields[i as usize].ty;
            return Expr::new(ty, ExprKind::StructGet(k, Box::new(obj), i));
        }
        Expr::new(Ty::Any, ExprKind::GetProp(Box::new(any(obj)), atom))
    }

    /// `obj.atom = v` (evaluates to the value stored).
    fn set_prop(&mut self, obj: Expr, atom: ir::AtomId, v: Expr) -> Expr {
        // An element of an array of an `aot type`: the value first, then an
        // object, else what JS throws setting a property of `undefined`.
        if let Ty::StructOrUndef(k) = obj.ty
            && self.struct_field(Ty::Struct(k), atom).is_some()
        {
            let t = self.temp(v.ty);
            let first = set(t, v);
            let obj = Expr::new(Ty::Struct(k), ExprKind::NonNullSet(atom, Box::new(obj)));
            let store = self.set_prop(obj, atom, self.local(t));
            return seq(vec![first, store]);
        }
        if let Some((k, i)) = self.struct_field(obj.ty, atom) {
            let v = self.struct_value(k, i, v);
            return Expr::new(v.ty, ExprKind::StructSet(k, Box::new(obj), i, Box::new(v)));
        }
        Expr::new(Ty::Any, ExprKind::SetProp(Box::new(any(obj)), atom, Box::new(any(v))))
    }

    fn object(&mut self, o: &'a ObjectExpression<'a>) -> R<Expr> {
        if let Some(&(_, k)) = self.m.struct_sites.get(&o.span.start) {
            return self.struct_literal(o, k);
        }
        // Parts: a field (its atom), a computed key, a spread; `values` are
        // all the operands, in evaluation order.
        enum Part {
            Field(ir::AtomId),
            Computed,
            Spread,
            /// A getter (false) or setter (true).
            Accessor(ir::AtomId, bool),
        }
        let mut parts = Vec::new();
        let mut values = Vec::new();
        let push = |cx: &mut Self, e: Expr, values: &mut Vec<Expr>, mark: usize| {
            cx.order(values, mark);
            values.push(e);
        };
        for p in &o.properties {
            let p = match p {
                ObjectPropertyKind::SpreadProperty(s) => {
                    let mark = self.pre.len();
                    let v = any(self.expr(&s.argument)?);
                    push(self, v, &mut values, mark);
                    parts.push(Part::Spread);
                    continue;
                }
                ObjectPropertyKind::ObjectProperty(p) => p,
            };
            if p.computed && (p.kind != PropertyKind::Init || p.method) {
                return reject(p.span, "computed methods and accessors in object literals are not supported yet");
            }
            if p.computed {
                let Some(k) = p.key.as_expression() else {
                    return reject(p.span, "unsupported object literal key");
                };
                let mark = self.pre.len();
                let k = any(self.expr(k)?);
                push(self, Expr::new(Ty::Any, ExprKind::ToPropKey(Box::new(k))), &mut values, mark);
                let mark = self.pre.len();
                let v = any(self.expr(&p.value)?);
                push(self, v, &mut values, mark);
                parts.push(Part::Computed);
                continue;
            }
            let name = match &p.key {
                PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                PropertyKey::StringLiteral(s) => {
                    lone_surrogates(s)?;
                    s.value.to_string()
                }
                PropertyKey::NumericLiteral(n) if n.value.fract() == 0.0 && n.value >= 0.0 => {
                    format!("{}", n.value as u64)
                }
                _ => return reject(p.span, "unsupported object literal key"),
            };
            if name == "__proto__" {
                return reject(p.span, "`__proto__` in object literals is not supported");
            }
            let atom = self.m.atom(&name);
            if p.kind != PropertyKind::Init || p.method {
                // A method or accessor: a native closure with `this`, named as
                // JS names it.
                let Expression::FunctionExpression(f) = p.value.without_parentheses() else {
                    return reject(p.span, "unsupported object literal member");
                };
                let (fname, part) = match p.kind {
                    PropertyKind::Get => (format!("get {name}"), Part::Accessor(atom, false)),
                    PropertyKind::Set => (format!("set {name}"), Part::Accessor(atom, true)),
                    _ => (name.clone(), Part::Field(atom)),
                };
                let v = self.closure_with(FnNode::Function(f), fname, true, p.span)?;
                values.push(v);
                parts.push(part);
                continue;
            }
            let mark = self.pre.len();
            let v = any(self.expr(&p.value)?);
            push(self, v, &mut values, mark);
            parts.push(Part::Field(atom));
        }
        let mut values = values.into_iter();
        if parts.iter().all(|p| matches!(p, Part::Field(_))) {
            let props = parts
                .into_iter()
                .map(|p| match p {
                    Part::Field(a) => (a, values.next().unwrap()),
                    _ => unreachable!(),
                })
                .collect();
            return Ok(Expr::new(Ty::Any, ExprKind::Object(props)));
        }
        let parts = parts
            .into_iter()
            .map(|p| match p {
                Part::Field(a) => ir::ObjPart::Field(a, values.next().unwrap()),
                Part::Computed => {
                    let k = values.next().unwrap();
                    ir::ObjPart::Computed(k, values.next().unwrap())
                }
                Part::Spread => ir::ObjPart::Spread(values.next().unwrap()),
                Part::Accessor(a, setter) => ir::ObjPart::Accessor(a, values.next().unwrap(), setter),
            })
            .collect();
        Ok(Expr::new(Ty::Any, ExprKind::ObjectParts(parts)))
    }

    /// A property key: an atom for constant names.
    fn key(&mut self, e: &'a Expression<'a>) -> R<Key> {
        Ok(match e.without_parentheses() {
            Expression::StringLiteral(s) => {
                lone_surrogates(s)?;
                Key::Atom(self.m.atom(s.value.as_str()))
            }
            other => Key::Value(Box::new(any(self.expr(other)?))),
        })
    }

    /// A key of element access: numbers stay unboxed.
    fn elem_key(&mut self, e: &'a Expression<'a>) -> R<Key> {
        Ok(match e.without_parentheses() {
            Expression::StringLiteral(s) => {
                lone_surrogates(s)?;
                Key::Atom(self.m.atom(s.value.as_str()))
            }
            other => {
                let k = self.expr(other)?;
                Key::Value(Box::new(if k.ty.is_number() { k } else { any(k) }))
            }
        })
    }

    /// Arguments with spreads, as an array; operands in `earlier` (checked
    /// before) are evaluated before any `await` hoisted out of them.
    fn arg_array(&mut self, args: &'a [Argument<'a>], earlier: &mut [Expr]) -> R<Expr> {
        let mut parts = Vec::new();
        let mut values = Vec::new();
        for a in args {
            let (e, spread) = match a {
                Argument::SpreadElement(s) => (&s.argument, true),
                other => (other.to_expression(), false),
            };
            let mark = self.pre.len();
            let v = any(self.expr(e)?);
            self.order(&mut values, mark);
            self.order(earlier, mark);
            values.push(v);
            parts.push(Some(spread));
        }
        Ok(array_parts(parts, values))
    }

    fn args(&mut self, args: &'a [Argument<'a>]) -> R<Vec<Expr>> {
        let mut out = Vec::with_capacity(args.len());
        self.push_args(&mut out, args)?;
        Ok(out)
    }

    /// Check `args` in order after the already-checked operands in `done`.
    fn push_args(&mut self, done: &mut Vec<Expr>, args: &'a [Argument<'a>]) -> R<()> {
        for a in args {
            let Some(e) = a.as_expression() else {
                return reject(a.span(), "spread arguments are not supported yet");
            };
            let mark = self.pre.len();
            let x = any(self.expr(e)?);
            self.order(done, mark);
            done.push(x);
        }
        Ok(())
    }

    /// After checking an operand that may have hoisted `await`s into `pre`
    /// (from `mark` on), make the operands checked before it run before
    /// those awaits, as in JS: compute them into temporaries first.
    fn order(&mut self, earlier: &mut [Expr], mark: usize) {
        if self.pre.len() == mark {
            return;
        }
        let mut spills = Vec::new();
        for e in earlier.iter_mut() {
            if matches!(
                e.kind,
                ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Undefined | ExprKind::Null
            ) {
                continue;
            }
            let x = std::mem::replace(e, undefined());
            if x.ty == Ty::Void {
                spills.push(Stmt::Expr(x));
            } else {
                let t = self.temp(x.ty);
                spills.push(Stmt::Expr(Expr::new(x.ty, ExprKind::SetLocal(t, Box::new(x)))));
                *e = self.local(t);
            }
        }
        self.pre.splice(mark..mark, spills);
    }

    /// When `await`s were hoisted from `mark` on, move the place `setup`
    /// before them.
    fn flush_setup(&mut self, setup: &mut Vec<Expr>, mark: usize) {
        if self.pre.len() > mark {
            self.pre.splice(mark..mark, setup.drain(..).map(Stmt::Expr));
        }
    }

    /// `e` evaluated only if `cond` holds: when it hoisted `await`s (from
    /// `mark` on), they become part of an `if` that computes `e` into `t`.
    fn conditional_arm(&mut self, mark: usize, e: Expr, t: ir::LocalId) -> Vec<Stmt> {
        let mut arm: Vec<Stmt> = self.pre.drain(mark..).collect();
        arm.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::SetLocal(t, Box::new(any(e))))));
        arm
    }

    /// The symbol `id` refers to (none: a global, `declare`d or not).
    fn resolve(&self, id: &'a IdentifierReference<'a>) -> Option<SymbolId> {
        let scoping = self.m.scoping;
        id.reference_id
            .get()
            .and_then(|r| scoping.get_reference(r).symbol_id())
            .filter(|&s| !crate::only_ambient(scoping, s))
            .map(|s| self.m.aliases.get(&s).copied().unwrap_or(s))
    }

    fn is_module_symbol(&self, sym: SymbolId) -> bool {
        let scoping = self.m.scoping;
        scoping.symbol_scope_id(sym) == scoping.root_scope_id()
    }

    fn ident(&mut self, id: &'a IdentifierReference<'a>) -> R<Expr> {
        match self.resolve(id) {
            Some(sym) => {
                if let Some(&l) = self.sym_local.get(&sym) {
                    return Ok(self.local(l));
                }
                if let Some(c) = self.m.consts.get(&sym) {
                    return Ok(match *c {
                        ConstVal::Num(v) => Expr::num(v),
                        ConstVal::Bool(b) => Expr::new(Ty::Bool, ExprKind::Bool(b)),
                    });
                }
                if self.is_module_symbol(sym) {
                    let cell = self.m.cell(sym, id.span)?;
                    // Only ever an array of an `aot type` (or in its TDZ).
                    let ty = self.m.sure_arrays.get(&sym).map_or(Ty::Any, |&k| Ty::StructArr(k));
                    return Ok(Expr::new(ty, ExprKind::Cell(cell)));
                }
                reject(id.span, format!("`{}` is not a local variable, module binding or global", id.name))
            }
            None => Ok(match id.name.as_str() {
                "NaN" => Expr::num(f64::NAN),
                "Infinity" => Expr::num(f64::INFINITY),
                "undefined" => undefined(),
                "arguments" => match self.args_local.or(self.args_box) {
                    Some(l) => return Ok(self.local(l)),
                    None => return reject(id.span, "`arguments` outside a function"),
                },
                name => {
                    let atom = self.m.atom(name);
                    Expr::new(Ty::Any, ExprKind::Global(atom))
                }
            }),
        }
    }

    // ---- assignment ----

    /// Resolve an assignment target, evaluating its object and key (in
    /// order) into temporaries. Returns the setup expressions and the place.
    /// An assignment target. `reads_first`: the target's current value is
    /// read before anything else is evaluated (compound assignments, `++`).
    fn place(
        &mut self,
        t: &'a SimpleAssignmentTarget<'a>,
        reads_first: bool,
    ) -> R<(Vec<Expr>, Place, Option<SymbolId>)> {
        Ok(match t {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => return self.ident_place(id),
            SimpleAssignmentTarget::StaticMemberExpression(_) | SimpleAssignmentTarget::ComputedMemberExpression(_)
                if let Some((home, key)) = self.super_key(t.as_member_expression().unwrap())? =>
            {
                // The key is evaluated into a local first.
                let k = self.temp(Ty::Any);
                (vec![set(k, key)], Place::Super(home, k), None)
            }
            SimpleAssignmentTarget::PrivateFieldExpression(p) => {
                let name = format!("#{}", p.field.name);
                if let Some(l) = self.plain_local(&p.object) {
                    return Ok((vec![], Place::Private(l, name), None));
                }
                let obj = any(self.expr(&p.object)?);
                let t = self.temp(Ty::Any);
                (vec![set(t, obj)], Place::Private(t, name), None)
            }
            SimpleAssignmentTarget::StaticMemberExpression(m) => {
                let atom = self.m.atom(m.property.name.as_str());
                if let Some(l) = self.plain_local(&m.object) {
                    // Used in place; `assign` copies it if the value may
                    // reassign it.
                    return Ok((vec![], Place::Prop(l, atom), None));
                }
                let obj = self.expr(&m.object)?;
                let obj = match obj.ty {
                    Ty::Struct(_) => obj,
                    // An element of an array of an `aot type`, whose field
                    // is read first: an object, else what JS throws reading
                    // the field of `undefined`.
                    Ty::StructOrUndef(k) if reads_first && self.struct_field(Ty::Struct(k), atom).is_some() => {
                        Expr::new(Ty::Struct(k), ExprKind::NonNull(Some(atom), Box::new(obj)))
                    }
                    // Only written: checked when stored (see `set_prop`).
                    Ty::StructOrUndef(k) if self.struct_field(Ty::Struct(k), atom).is_some() => obj,
                    _ => any(obj),
                };
                let t = self.temp(obj.ty);
                (vec![set(t, obj)], Place::Prop(t, atom), None)
            }
            SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                // Typed arrays keep their type.
                let direct = self.plain_local(&m.object);
                let obj = self.expr(&m.object)?;
                let obj = if matches!(obj.ty, Ty::Arr(_) | Ty::StructArr(_)) { obj } else { any(obj) };
                let mark = self.pre.len();
                let key = self.elem_key(&m.expression)?;
                let mut setup = Vec::new();
                let t = match direct {
                    Some(l) if self.pre.len() == mark && !matches!(&key, Key::Value(k) if ir::assigns_local(k, l)) => l,
                    _ => {
                        let t = self.temp(obj.ty);
                        setup.push(set(t, obj));
                        self.flush_setup(&mut setup, mark);
                        t
                    }
                };
                match key {
                    Key::Atom(a) => (setup, Place::Prop(t, a), None),
                    Key::Value(k) => {
                        let tk = self.temp(k.ty);
                        setup.push(set(tk, *k));
                        (setup, Place::Elem(t, tk), None)
                    }
                }
            }
            SimpleAssignmentTarget::TSAsExpression(a) => return self.place_expr(&a.expression),
            SimpleAssignmentTarget::TSNonNullExpression(a) => return self.place_expr(&a.expression),
            SimpleAssignmentTarget::TSSatisfiesExpression(a) => return self.place_expr(&a.expression),
            SimpleAssignmentTarget::TSTypeAssertion(a) => return self.place_expr(&a.expression),
        })
    }

    /// A local that `e` names and that only this function's own code can
    /// assign (not captured), holding a JS value.
    fn plain_local(&self, e: &'a Expression<'a>) -> Option<ir::LocalId> {
        // A constructor's `this`: the instance, in a local never reassigned.
        if let Expression::ThisExpression(_) = e.without_parentheses()
            && let Some(l) = self.this_local
            && !self.boxed[l]
        {
            return Some(l);
        }
        let Expression::Identifier(id) = e.without_parentheses() else { return None };
        let &l = self.sym_local.get(&self.resolve(id)?)?;
        (!self.boxed[l] && matches!(self.locals[l], Ty::Any | Ty::Arr(_) | Ty::Struct(_) | Ty::StructArr(_)))
            .then_some(l)
    }

    /// An identifier as an assignment target.
    fn ident_place(&mut self, id: &'a IdentifierReference<'a>) -> R<(Vec<Expr>, Place, Option<SymbolId>)> {
        Ok(match self.resolve(id) {
            Some(sym) => {
                if let Some(&l) = self.sym_local.get(&sym) {
                    if !self.mutable[l] {
                        return reject(id.span, format!("assignment to constant `{}`", id.name));
                    }
                    (vec![], Place::Local(l), Some(sym))
                } else if self.is_module_symbol(sym) {
                    let b = self.m.bindings.get(&sym);
                    if b.is_none_or(|b| b.kind == BindingKind::Const || b.kind == BindingKind::Import) {
                        return reject(id.span, format!("assignment to constant `{}`", id.name));
                    }
                    (vec![], Place::Cell(self.m.cell(sym, id.span)?), Some(sym))
                } else {
                    return reject(id.span, format!("`{}` is not a local variable or module binding", id.name));
                }
            }
            None => match id.name.as_str() {
                "undefined" | "NaN" | "Infinity" | "arguments" => {
                    return reject(id.span, format!("assignment to `{}`", id.name));
                }
                name => (vec![], Place::Global(self.m.atom(name)), None),
            },
        })
    }

    fn place_expr(&mut self, e: &'a Expression<'a>) -> R<(Vec<Expr>, Place, Option<SymbolId>)> {
        match e.without_parentheses() {
            Expression::Identifier(_)
            | Expression::StaticMemberExpression(_)
            | Expression::ComputedMemberExpression(_) => {
                // Re-dispatch through the simple target form.
                let t = match e.without_parentheses() {
                    Expression::Identifier(id) => {
                        return self.place_ident(id);
                    }
                    Expression::StaticMemberExpression(m) => {
                        let obj = self.expr(&m.object)?;
                        let obj = if matches!(obj.ty, Ty::Struct(_)) { obj } else { any(obj) };
                        let t = self.temp(obj.ty);
                        let atom = self.m.atom(m.property.name.as_str());
                        (vec![set(t, obj)], Place::Prop(t, atom), None)
                    }
                    Expression::ComputedMemberExpression(m) => {
                        let obj = any(self.expr(&m.object)?);
                        let t = self.temp(Ty::Any);
                        let tk = self.temp(Ty::Any);
                        let mut setup = vec![set(t, obj)];
                        let mark = self.pre.len();
                        let k = any(self.expr(&m.expression)?);
                        self.flush_setup(&mut setup, mark);
                        setup.push(set(tk, k));
                        (setup, Place::Elem(t, tk), None)
                    }
                    _ => unreachable!(),
                };
                Ok(t)
            }
            other => reject(other.span(), "unsupported assignment target"),
        }
    }

    fn place_ident(&mut self, id: &'a IdentifierReference<'a>) -> R<(Vec<Expr>, Place, Option<SymbolId>)> {
        match self.resolve(id) {
            Some(sym) if self.sym_local.contains_key(&sym) => {
                let l = self.sym_local[&sym];
                if !self.mutable[l] {
                    return reject(id.span, format!("assignment to constant `{}`", id.name));
                }
                Ok((vec![], Place::Local(l), Some(sym)))
            }
            _ => reject(id.span, "unsupported assignment target"),
        }
    }

    /// The current value at `place`.
    fn read(&mut self, p: &Place, span: Span) -> R<Expr> {
        Ok(match *p {
            Place::Local(l) => self.local(l),
            Place::Cell(c) => Expr::new(Ty::Any, ExprKind::Cell(c)),
            Place::Global(a) => Expr::new(Ty::Any, ExprKind::Global(a)),
            Place::Prop(o, a) => self.get_prop(self.local(o), a),
            Place::Elem(o, k) => get_elem(self.local(o), Key::Value(Box::new(self.local(k)))),
            Place::Private(o, ref name) => {
                let name = name.clone();
                return self.private_read(self.local(o), &name, span);
            }
            Place::Super(home, k) => Expr::new(Ty::Any, ExprKind::SuperGet(home, Box::new(self.local(k)))),
        })
    }

    /// Store `v` at `place`; evaluates to the stored value.
    fn write(&mut self, p: &Place, sym: Option<SymbolId>, v: Expr, span: Span) -> R<Expr> {
        Ok(match *p {
            Place::Local(l) => return self.set_local(sym, l, v, span),
            Place::Cell(c) => Expr::new(Ty::Any, ExprKind::SetCell(c, Box::new(any(v)))),
            Place::Global(a) => Expr::new(Ty::Any, ExprKind::SetGlobal(a, Box::new(any(v)))),
            Place::Prop(o, a) => self.set_prop(self.local(o), a, v),
            Place::Elem(o, k) => set_elem(self.local(o), self.local(k), v),
            Place::Private(o, ref name) => {
                let name = name.clone();
                return self.private_write(self.local(o), &name, v, span);
            }
            Place::Super(home, k) => {
                Expr::new(Ty::Any, ExprKind::SuperSet(home, Box::new(self.local(k)), Box::new(any(v))))
            }
        })
    }

    fn assign(&mut self, a: &'a AssignmentExpression<'a>) -> R<Expr> {
        let Some(target) = a.left.as_simple_assignment_target() else {
            // Destructuring: statements that run before the current one (as
            // for `await`); the expression's value is the right-hand side.
            let pattern = a.left.as_assignment_target_pattern().unwrap();
            let v = self.expr(&a.right)?;
            let t = self.temp(Ty::Any);
            let mut stmts = vec![Stmt::Expr(set(t, any(v)))];
            self.assign_pattern(pattern, self.local(t), &mut stmts)?;
            self.pre.extend(stmts);
            return Ok(self.local(t));
        };
        if a.operator.is_logical() {
            return self.logical_assign(a, target);
        }
        let (mut setup, place, sym) = self.place(target, a.operator != AssignmentOperator::Assign)?;
        let mark = self.pre.len();
        let value = if a.operator == AssignmentOperator::Assign {
            if let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = target
                && anonymous_function(&a.right)
            {
                self.name_hint = Some(id.name.to_string());
            }
            let v = self.expr(&a.right);
            self.name_hint = None;
            let v = v?;
            self.flush_setup(&mut setup, mark);
            if self.wraps(sym) && v.ty != Ty::I32 { wrap(v) } else { v }
        } else {
            let op = match a.operator {
                AssignmentOperator::Addition => AnyBinOp::Add,
                AssignmentOperator::Subtraction => AnyBinOp::Sub,
                AssignmentOperator::Multiplication => AnyBinOp::Mul,
                AssignmentOperator::Division => AnyBinOp::Div,
                AssignmentOperator::Remainder => AnyBinOp::Rem,
                AssignmentOperator::Exponential => AnyBinOp::Pow,
                AssignmentOperator::ShiftLeft => AnyBinOp::Shl,
                AssignmentOperator::ShiftRight => AnyBinOp::Sar,
                AssignmentOperator::ShiftRightZeroFill => AnyBinOp::Shr,
                AssignmentOperator::BitwiseOR => AnyBinOp::BitOr,
                AssignmentOperator::BitwiseXOR => AnyBinOp::BitXor,
                AssignmentOperator::BitwiseAnd => AnyBinOp::BitAnd,
                _ => return reject(a.span, format!("`{}` is not supported yet", a.operator.as_str())),
            };
            let mut cur = [self.read(&place, a.span)?];
            let rhs = self.expr(&a.right)?;
            if self.pre.len() > mark {
                // The target is resolved and read before the awaits.
                self.order(&mut cur, mark);
                self.flush_setup(&mut setup, mark);
            }
            let [cur] = cur;
            let v = arith(op, cur, rhs);
            if self.wraps(sym) && matches!(op, AnyBinOp::Add | AnyBinOp::Sub | AnyBinOp::Mul | AnyBinOp::Shr) {
                wrap(v)
            } else {
                v
            }
        };
        // An object held in a named local is used in place, unless the value
        // may reassign the local (or awaits run in between): then the object
        // is copied first, as JS evaluates it before the value.
        let mut place = place;
        if let Place::Prop(o, _) | Place::Elem(o, _) | Place::Private(o, _) = place
            && self.local_names[o].is_some()
            && (ir::assigns_local(&value, o) || self.pre.len() > mark)
        {
            let t = self.temp(self.locals[o]);
            setup.insert(0, set(t, self.local(o)));
            self.flush_setup(&mut setup, mark);
            place = match place {
                Place::Prop(_, a) => Place::Prop(t, a),
                Place::Elem(_, k) => Place::Elem(t, k),
                Place::Private(_, n) => Place::Private(t, n),
                other => other,
            };
        }
        setup.push(self.write(&place, sym, value, a.span)?);
        Ok(seq(setup))
    }

    /// `x ??= v`, `x ||= v`, `x &&= v`: the target resolved once and read,
    /// then assigned, with `v` evaluated, only when `??`, `||` or `&&` would
    /// evaluate their right operand; the value is the target's or `v`'s.
    fn logical_assign(&mut self, a: &'a AssignmentExpression<'a>, target: &'a SimpleAssignmentTarget<'a>) -> R<Expr> {
        let op = match a.operator {
            AssignmentOperator::LogicalAnd => LogicalOp::And,
            AssignmentOperator::LogicalOr => LogicalOp::Or,
            _ => LogicalOp::Nullish,
        };
        let (mut setup, place, sym) = self.place(target, true)?;
        let cur = self.read(&place, a.span)?;
        // (An anonymous function assigned to a name takes it.)
        if let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = target
            && anonymous_function(&a.right)
        {
            self.name_hint = Some(id.name.to_string());
        }
        let rhs_mark = self.pre.len();
        let rhs = self.expr(&a.right);
        self.name_hint = None;
        let rhs = rhs?;
        // A number or boolean is never nullish: nothing is assigned.
        if op == LogicalOp::Nullish && !matches!(cur.ty, Ty::Any | Ty::MaybeF64) {
            self.pre.truncate(rhs_mark);
            setup.push(cur);
            return Ok(seq(setup));
        }
        let value = if self.wraps(sym) && rhs.ty != Ty::I32 { wrap(rhs) } else { rhs };
        let mut place = place;
        let awaits = self.pre.len() > rhs_mark;
        // (As in `assign`: an object held in a named local that the value may
        // reassign, or with awaits in between, is copied first.)
        if let Place::Prop(o, _) | Place::Elem(o, _) | Place::Private(o, _) = place
            && self.local_names[o].is_some()
            && (ir::assigns_local(&value, o) || awaits)
        {
            let t = self.temp(self.locals[o]);
            setup.insert(0, set(t, self.local(o)));
            place = match place {
                Place::Prop(_, a) => Place::Prop(t, a),
                Place::Elem(_, k) => Place::Elem(t, k),
                Place::Private(_, n) => Place::Private(t, n),
                other => other,
            };
        }
        if awaits {
            // The value awaits: the target is resolved and read first, and
            // the value evaluated and assigned in an `if`.
            let rhs_pre: Vec<Stmt> = self.pre.drain(rhs_mark..).collect();
            self.pre.extend(setup.drain(..).map(Stmt::Expr));
            let t = self.temp(Ty::Any);
            self.pre.push(Stmt::Expr(set(t, any(cur))));
            let w = self.write(&place, sym, value, a.span)?;
            let mut arm = rhs_pre;
            arm.push(Stmt::Expr(set(t, any(w))));
            let now = self.local(t);
            let cond = match op {
                LogicalOp::And => truthy(now),
                LogicalOp::Or => Expr::new(Ty::Bool, ExprKind::Unary(UnOp::Not, Box::new(truthy(now)))),
                LogicalOp::Nullish => Expr::new(
                    Ty::Bool,
                    ExprKind::AnyCompare(AnyCmpOp::Eq, Box::new(now), Box::new(Expr::new(Ty::Any, ExprKind::Null))),
                ),
            };
            self.pre.push(Stmt::If(cond, arm, vec![]));
            return Ok(self.local(t));
        }
        let w = self.write(&place, sym, value, a.span)?;
        let ty = join(cur.ty, w.ty);
        let ty = if ty == Ty::Void { Ty::Any } else { ty };
        setup.push(Expr::new(ty, ExprKind::Logical(op, Box::new(coerce(cur, ty)), Box::new(coerce(w, ty)))));
        Ok(seq(setup))
    }

    /// `x++` and friends; `used`: whether the value of the expression is.
    fn update(&mut self, u: &'a UpdateExpression<'a>, used: bool) -> R<Expr> {
        let (mut setup, place, sym) = self.place(&u.argument, true)?;
        let inc = u.operator == UpdateOperator::Increment;
        let counter = matches!(place, Place::Local(l) if self.int_counter == Some((l, inc)));
        if self.wraps(sym) && !counter {
            // `x = (x ± 1) | 0`; the old value is `(x ∓ 1) | 0` after that, as
            // the interpreted code computes it (see `erase`).
            let d = if inc { 1.0 } else { -1.0 };
            let cur = self.read(&place, u.span)?;
            let new = wrap(arith(AnyBinOp::Add, cur, Expr::num(d)));
            let w = self.write(&place, sym, new, u.span)?;
            setup.push(if u.prefix || !used { w } else { wrap(arith(AnyBinOp::Sub, w, Expr::num(d))) });
            return Ok(seq(setup));
        }
        if let Place::Local(l) = place
            && self.locals[l] == Ty::I32
        {
            let delta = if inc { 1.0 } else { -1.0 };
            return Ok(Expr::new(Ty::I32, ExprKind::Update { local: l, delta, prefix: u.prefix }));
        }
        if let Place::Local(l) = place
            && self.locals[l] == Ty::F64
        {
            let delta = if inc { 1.0 } else { -1.0 };
            return Ok(Expr::new(Ty::F64, ExprKind::Update { local: l, delta, prefix: u.prefix }));
        }
        if let Place::Local(l) = place
            && self.locals[l] != Ty::Any
        {
            // `++` on a boolean local makes it a number.
            self.widen_request = sym;
            return Err(Reject { span: u.span, msg: "retry".into(), retry: true, callee: false });
        }
        let cur = self.read(&place, u.span)?;
        if num(cur.ty) {
            // A typed array element: number arithmetic.
            let old = self.temp(Ty::F64);
            setup.push(set(old, to_f64(cur)));
            let delta = Expr::num(if inc { 1.0 } else { -1.0 });
            let new = Expr::new(Ty::F64, ExprKind::Binary(BinOp::Add, Box::new(self.local(old)), Box::new(delta)));
            setup.push(self.write(&place, sym, new, u.span)?);
            if !u.prefix {
                setup.push(self.local(old));
            }
            return Ok(seq(setup));
        }
        let op = if inc { AnyUnOp::Inc } else { AnyUnOp::Dec };
        if u.prefix || !used {
            // The value is the new one (ToNumeric is part of the increment).
            let new = Expr::new(Ty::Any, ExprKind::AnyUnary(op, Box::new(any(cur))));
            setup.push(self.write(&place, sym, new, u.span)?);
            return Ok(seq(setup));
        }
        // old = ToNumeric(current); place = old ± 1; value: old.
        let old = self.temp(Ty::Any);
        setup.push(set(old, Expr::new(Ty::Any, ExprKind::AnyUnary(AnyUnOp::ToNumeric, Box::new(cur)))));
        let new = Expr::new(Ty::Any, ExprKind::AnyUnary(op, Box::new(self.local(old))));
        setup.push(self.write(&place, sym, new, u.span)?);
        setup.push(self.local(old));
        Ok(seq(setup))
    }

    fn unary(&mut self, u: &'a UnaryExpression<'a>) -> R<Expr> {
        if u.operator == UnaryOperator::Typeof
            && let Expression::Identifier(id) = u.argument.without_parentheses()
            && self.resolve(id).is_none()
            && !matches!(id.name.as_str(), "undefined" | "NaN" | "Infinity")
            && !(id.name == "arguments" && self.args_local.or(self.args_box).is_some())
        {
            // `typeof undeclared` is "undefined", not a ReferenceError.
            let atom = self.m.atom(id.name.as_str());
            let g = Expr::new(Ty::Any, ExprKind::GlobalOrUndefined(atom));
            for name in ["undefined", "number", "string", "boolean", "symbol", "bigint", "object", "function"] {
                self.m.string(name);
            }
            return Ok(Expr::new(Ty::Any, ExprKind::AnyUnary(AnyUnOp::Typeof, Box::new(g))));
        }
        if u.operator == UnaryOperator::Delete {
            return Ok(match u.argument.without_parentheses() {
                Expression::StaticMemberExpression(m) => {
                    let obj = any(self.expr(&m.object)?);
                    let atom = self.m.atom(m.property.name.as_str());
                    Expr::new(Ty::Bool, ExprKind::DeleteProp(Box::new(obj), atom))
                }
                Expression::ComputedMemberExpression(m) => {
                    let mut obj = [any(self.expr(&m.object)?)];
                    let mark = self.pre.len();
                    let key = self.key(&m.expression)?;
                    self.order(&mut obj, mark);
                    let [obj] = obj;
                    match key {
                        Key::Atom(a) => Expr::new(Ty::Bool, ExprKind::DeleteProp(Box::new(obj), a)),
                        Key::Value(k) => Expr::new(Ty::Bool, ExprKind::DeleteElem(Box::new(obj), k)),
                    }
                }
                _ => return reject(u.span, "`delete` of a non-property is not supported"),
            });
        }
        let x = self.expr(&u.argument)?;
        let dynamic = |op: AnyUnOp, x: Expr| Expr::new(Ty::Any, ExprKind::AnyUnary(op, Box::new(any(x))));
        Ok(match (u.operator, x.ty) {
            (UnaryOperator::UnaryNegation, Ty::F64 | Ty::I32 | Ty::MaybeF64) => {
                Expr::new(Ty::F64, ExprKind::Unary(UnOp::Neg, Box::new(to_f64(x))))
            }
            (UnaryOperator::UnaryNegation, _) => dynamic(AnyUnOp::Neg, x),
            (UnaryOperator::UnaryPlus, Ty::F64 | Ty::I32 | Ty::MaybeF64) => to_f64(x),
            (UnaryOperator::UnaryPlus, _) => dynamic(AnyUnOp::Plus, x),
            (UnaryOperator::BitwiseNot, Ty::F64 | Ty::I32 | Ty::MaybeF64) => {
                Expr::new(Ty::I32, ExprKind::Unary(UnOp::BitNot, Box::new(to_f64(x))))
            }
            (UnaryOperator::BitwiseNot, _) => dynamic(AnyUnOp::BitNot, x),
            (UnaryOperator::LogicalNot, _) => Expr::new(Ty::Bool, ExprKind::Unary(UnOp::Not, Box::new(truthy(x)))),
            (UnaryOperator::Typeof, Ty::F64 | Ty::I32 | Ty::Bool) => {
                // Statically known; still evaluate the operand.
                let name = if x.ty == Ty::Bool { "boolean" } else { "number" };
                let id = self.m.string(name);
                seq(vec![x, Expr::new(Ty::Any, ExprKind::Str(id))])
            }
            (UnaryOperator::Typeof, _) => {
                // (The names it gives, for native code to give them.)
                for name in ["undefined", "number", "string", "boolean", "symbol", "bigint", "object", "function"] {
                    self.m.string(name);
                }
                dynamic(AnyUnOp::Typeof, x)
            }
            (UnaryOperator::Void, _) => Expr::new(Ty::Any, ExprKind::Void(Box::new(x))),
            (op, _) => return reject(u.span, format!("`{}` is not supported", op.as_str())),
        })
    }

    fn binary(&mut self, b: &'a BinaryExpression<'a>) -> R<Expr> {
        use BinaryOperator as B;
        let mut x = [self.expr(&b.left)?];
        let mark = self.pre.len();
        let y = self.expr(&b.right)?;
        self.order(&mut x, mark);
        let [x] = x;
        let cmp = match b.operator {
            B::LessThan => Some((CmpOp::Lt, AnyCmpOp::Lt)),
            B::LessEqualThan => Some((CmpOp::Le, AnyCmpOp::Le)),
            B::GreaterThan => Some((CmpOp::Gt, AnyCmpOp::Gt)),
            B::GreaterEqualThan => Some((CmpOp::Ge, AnyCmpOp::Ge)),
            B::Equality => Some((CmpOp::Eq, AnyCmpOp::Eq)),
            B::StrictEquality => Some((CmpOp::Eq, AnyCmpOp::StrictEq)),
            B::Inequality => Some((CmpOp::Ne, AnyCmpOp::Ne)),
            B::StrictInequality => Some((CmpOp::Ne, AnyCmpOp::StrictNe)),
            B::In => Some((CmpOp::Eq, AnyCmpOp::In)),
            B::Instanceof => Some((CmpOp::Eq, AnyCmpOp::InstanceOf)),
            _ => None,
        };
        if let Some((op, any_op)) = cmp {
            let equality = matches!(op, CmpOp::Eq | CmpOp::Ne);
            let unboxed = !matches!(any_op, AnyCmpOp::In | AnyCmpOp::InstanceOf)
                && match (x.ty, y.ty) {
                    (Ty::Bool, Ty::Bool) => equality,
                    // `undefined` compares like NaN, except that it equals
                    // itself (and `null`, loosely): never against a number.
                    (a, b) if num(a) && num(b) => !equality || a.is_number() || b.is_number(),
                    _ => false,
                };
            if unboxed {
                let (x, y) = (to_f64(x), to_f64(y));
                return Ok(Expr::new(Ty::Bool, ExprKind::Compare(op, Box::new(x), Box::new(y))));
            }
            return Ok(Expr::new(Ty::Bool, ExprKind::AnyCompare(any_op, Box::new(any(x)), Box::new(any(y)))));
        }
        let op = match b.operator {
            B::Addition => AnyBinOp::Add,
            B::Subtraction => AnyBinOp::Sub,
            B::Multiplication => AnyBinOp::Mul,
            B::Division => AnyBinOp::Div,
            B::Remainder => AnyBinOp::Rem,
            B::Exponential => AnyBinOp::Pow,
            B::ShiftLeft => AnyBinOp::Shl,
            B::ShiftRight => AnyBinOp::Sar,
            B::ShiftRightZeroFill => AnyBinOp::Shr,
            B::BitwiseOR => AnyBinOp::BitOr,
            B::BitwiseXOR => AnyBinOp::BitXor,
            B::BitwiseAnd => AnyBinOp::BitAnd,
            op => return reject(b.span, format!("`{}` is not supported", op.as_str())),
        };
        Ok(arith(op, x, y))
    }

    fn call(&mut self, c: &'a CallExpression<'a>) -> R<Expr> {
        if c.optional {
            return reject(c.span, "optional calls are not supported yet");
        }
        // `Object.create(p)`: made inline while the method is the builtin.
        if let Expression::StaticMemberExpression(cm) = c.callee.without_parentheses()
            && global_member(cm, self.m.scoping) == Some(("Object", "create"))
            && let [arg] = &c.arguments[..]
            && let Some(arg) = arg.as_expression()
        {
            let obj = Expr::new(Ty::Any, ExprKind::Global(self.m.atom("Object")));
            let create = self.m.atom("create");
            let arg = any(self.expr(arg)?);
            return Ok(Expr::new(Ty::Any, ExprKind::ObjectCreate(Box::new(obj), create, Box::new(arg))));
        }
        match c.callee.without_parentheses() {
            callee @ (Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_))
                if let Some((home, key)) = self.super_key(callee.as_member_expression().unwrap())? =>
            {
                let this = Expr::new(Ty::Any, ExprKind::This);
                let member = callee.as_member_expression().unwrap();
                let get = |cx: &mut Self, _| Ok(cx.super_get(member, home, key));
                self.call_this(this, get, &c.arguments)
            }
            Expression::PrivateFieldExpression(p) => {
                let obj = self.expr(&p.object)?;
                let name = format!("#{}", p.field.name);
                let span = p.span;
                if let Some(i) = self.private_method(&name, span)
                    && !has_spread(&c.arguments)
                {
                    // The method is the class's own: a direct call, after the
                    // brand check (before the arguments, as reading it is).
                    let (class, _) = self.private_kind(&name, span)?;
                    let t = self.temp(Ty::Any);
                    let slot = self.m.private(class, &name);
                    let check = ExprKind::PrivateMethod(Box::new(self.local(t)), slot);
                    let mut setup =
                        vec![set(t, any(obj)), Expr::new(Ty::Void, ExprKind::Void(Box::new(Expr::new(Ty::Any, check))))];
                    let mark = self.pre.len();
                    let this = self.local(t);
                    if let Some(call) = self.direct_call_this(i, &c.arguments, c.span, None, Some(this))? {
                        self.flush_setup(&mut setup, mark);
                        setup.push(call);
                        return Ok(seq(setup));
                    }
                    setup.truncate(1);
                    let r = self.call_this(self.local(t), |cx, o| cx.private_read(o, &name, span), &c.arguments)?;
                    self.flush_setup(&mut setup, mark);
                    setup.push(r);
                    return Ok(seq(setup));
                }
                self.call_this(obj, |cx, o| cx.private_read(o, &name, span), &c.arguments)
            }
            Expression::Identifier(id) => {
                let sym = self.resolve(id);
                if let Some(i) = sym.and_then(|s| self.m.cands.iter().position(|c| c.symbol == Some(s)))
                    && !self.sym_local.contains_key(&sym.unwrap())
                    && let Some(e) = self.direct_call(i, &c.arguments, c.span, Some(id))?
                {
                    return Ok(e);
                }
                let callee = any(self.ident(id)?);
                self.call_value(callee, &c.arguments)
            }
            Expression::StaticMemberExpression(m) => {
                if let Some(f) = builtin_fn(m, self.m.scoping) {
                    let all_exprs = c.arguments.iter().all(|a| a.as_expression().is_some());
                    if all_exprs {
                        let mut args = Vec::new();
                        for a in &c.arguments {
                            let mark = self.pre.len();
                            let x = self.expr(a.as_expression().unwrap())?;
                            self.order(&mut args, mark);
                            args.push(x);
                        }
                        if args.iter().all(|a| num(a.ty)) {
                            return math_call(f, args.into_iter().map(to_f64).collect(), c.span);
                        }
                        // `Math` functions convert their arguments with
                        // ToNumber once all are evaluated, as the lowering
                        // does (`Number.is*` do not; n-ary `min` / `max`
                        // would interleave the two as a chain).
                        let coerces =
                            !matches!(f, MathFn::IsNaN | MathFn::IsFinite | MathFn::IsInteger | MathFn::IsSafeInteger);
                        let arity = match f {
                            MathFn::Min | MathFn::Max => (1..=2).contains(&args.len()),
                            _ => args.len() == f.arity(),
                        };
                        if coerces && arity && args.iter().all(|a| num(a.ty) || a.ty == Ty::Any) {
                            return math_call(f, args.into_iter().map(to_f64).collect(), c.span);
                        }
                        // Generic call with the already-checked arguments.
                        let obj = Expr::new(
                            Ty::Any,
                            ExprKind::Global(self.m.atom(global_member(m, self.m.scoping).unwrap().0)),
                        );
                        let atom = self.m.atom(m.property.name.as_str());
                        return Ok(Expr::new(
                            Ty::Any,
                            ExprKind::CallMethod(Box::new(obj), Key::Atom(atom), args.into_iter().map(any).collect()),
                        ));
                    }
                }
                if m.optional {
                    return reject(c.span, "optional chaining is not supported yet");
                }
                let obj = self.expr(&m.object)?;
                if let Ty::StructArr(k) = obj.ty
                    && m.property.name == "push"
                    && let [a] = &c.arguments[..]
                    && let Some(a) = a.as_expression()
                {
                    let mut operands = [obj];
                    let mark = self.pre.len();
                    let v = self.expr(a)?;
                    self.order(&mut operands, mark);
                    let [obj] = operands;
                    let v = if v.ty == Ty::Struct(k) { v } else { any(v) };
                    return Ok(Expr::new(Ty::F64, ExprKind::StructArrPush(k, Box::new(obj), Box::new(v))));
                }
                let obj = any(obj);
                let atom = self.m.atom(m.property.name.as_str());
                self.call_method(obj, Key::Atom(atom), &c.arguments)
            }
            Expression::ComputedMemberExpression(m) => {
                if m.optional {
                    return reject(c.span, "optional chaining is not supported yet");
                }
                let mut obj = [any(self.expr(&m.object)?)];
                let mark = self.pre.len();
                let key = self.key(&m.expression)?;
                self.order(&mut obj, mark);
                let [obj] = obj;
                self.call_method(obj, key, &c.arguments)
            }
            Expression::Super(_) if self.derived.as_ref().is_some_and(|d| d.dynamic) => {
                let mut args = Vec::new();
                self.push_args(&mut args, &c.arguments)?;
                let call = self.super_new(Some(args), None);
                self.super_init(call)
            }
            Expression::Super(_) if self.derived.as_ref().is_some_and(|d| !d.recheck.is_empty()) => {
                // The arguments; then the superclass's native constructor if
                // the classes it is made by are still each other's
                // superclasses, else the superclass constructed as
                // interpreted code would.
                let mut vals = Vec::new();
                self.push_args(&mut vals, &c.arguments)?;
                let mut args = Vec::new();
                for v in vals {
                    let t = self.temp(Ty::Any);
                    self.pre.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(t, Box::new(v)))));
                    args.push(self.local(t));
                }
                let direct = self.super_ctor(
                    |cx, i| {
                        let (params, _) = cx.m.signature(i, c.span)?;
                        let fixed = params.len() - cx.m.cands[i].rest as usize;
                        let mut out: Vec<Expr> = (0..fixed).map(|k| args.get(k).cloned().unwrap_or_else(undefined)).collect();
                        if cx.m.cands[i].rest {
                            out.push(Expr::new(Ty::Any, ExprKind::Array(args.get(fixed..).unwrap_or(&[]).to_vec())));
                        }
                        Ok(Some(Expr::new(Ty::Any, ExprKind::Call(i, out))))
                    },
                    c.span,
                )?;
                let links = self.derived.as_ref().unwrap().recheck.clone();
                let generic = self.super_new(Some(args.clone()), None);
                let mut ok = Expr::new(Ty::Bool, ExprKind::Bool(true));
                for &(class, parent) in links.iter().rev() {
                    for k in [class, parent] {
                        if !self.m.homes_used.contains(&k) {
                            self.m.homes_used.push(k);
                        }
                    }
                    let is = Expr::new(Ty::Bool, ExprKind::ProtoIs { class, parent });
                    ok = Expr::new(
                        Ty::Bool,
                        ExprKind::Cond(Box::new(is), Box::new(ok), Box::new(Expr::new(Ty::Bool, ExprKind::Bool(false)))),
                    );
                }
                let call = Expr::new(Ty::Any, ExprKind::Cond(Box::new(ok), Box::new(direct), Box::new(generic)));
                self.super_init(call)
            }
            Expression::Super(_) if self.derived.is_some() => {
                let call = self.super_ctor(|cx, i| cx.direct_call(i, &c.arguments, c.span, None), c.span)?;
                self.super_init(call)
            }
            Expression::Super(_) => reject(c.span, "`super()` calls (in constructors) are not supported"),
            other => {
                let callee = any(self.expr(other)?);
                self.call_value(callee, &c.arguments)
            }
        }
    }

    fn call_value(&mut self, callee: Expr, args: &'a [Argument<'a>]) -> R<Expr> {
        if has_spread(args) {
            let mut earlier = [callee];
            let arr = self.arg_array(args, &mut earlier)?;
            let [callee] = earlier;
            let this = undefined();
            return Ok(Expr::new(Ty::Any, ExprKind::CallSpread(Box::new(callee), Box::new(this), Box::new(arr))));
        }
        let mut v = vec![callee];
        self.push_args(&mut v, args)?;
        let callee = v.remove(0);
        Ok(Expr::new(Ty::Any, ExprKind::CallValue(Box::new(callee), v)))
    }

    /// `obj[key](args)`. The method is read before the arguments are
    /// evaluated: when they await, into a temporary before the awaits.
    fn call_method(&mut self, obj: Expr, key: Key, args: &'a [Argument<'a>]) -> R<Expr> {
        let mark = self.pre.len();
        if has_spread(args) {
            // The method is read into a temporary, then the arguments.
            let arr = self.arg_array(args, &mut [])?;
            let o = self.temp(Ty::Any);
            let f = self.temp(Ty::Any);
            let method = get_elem(self.local(o), key);
            let setup = vec![set(o, obj), set(f, any(method))];
            let call = Expr::new(
                Ty::Any,
                ExprKind::CallSpread(Box::new(self.local(f)), Box::new(self.local(o)), Box::new(arr)),
            );
            if self.pre.len() == mark {
                let mut xs = setup;
                xs.push(call);
                return Ok(seq(xs));
            }
            self.pre.splice(mark..mark, setup.into_iter().map(Stmt::Expr));
            return Ok(call);
        }
        let args = self.args(args)?;
        if self.pre.len() == mark {
            return Ok(Expr::new(Ty::Any, ExprKind::CallMethod(Box::new(obj), key, args)));
        }
        let o = self.temp(Ty::Any);
        let f = self.temp(Ty::Any);
        let method = match key {
            Key::Atom(a) => ExprKind::GetProp(Box::new(self.local(o)), a),
            Key::Value(k) => ExprKind::GetElem(Box::new(self.local(o)), k),
        };
        let spills = vec![Stmt::Expr(set(o, obj)), Stmt::Expr(set(f, Expr::new(Ty::Any, method)))];
        self.pre.splice(mark..mark, spills);
        Ok(Expr::new(Ty::Any, ExprKind::CallThis(Box::new(self.local(f)), Box::new(self.local(o)), args)))
    }

    /// A direct machine call to native function `i`, if the arguments fit
    /// its parameters exactly (otherwise the call goes through its JS entry,
    /// which guards the types and may fall back to the interpreter).
    fn direct_call(
        &mut self,
        i: usize,
        args: &'a [Argument<'a>],
        span: Span,
        callee: Option<&'a IdentifierReference<'a>>,
    ) -> R<Option<Expr>> {
        self.direct_call_this(i, args, span, callee, None)
    }

    /// `direct_call`, of a method with `this` (a local, or `this`).
    fn direct_call_this(
        &mut self,
        i: usize,
        args: &'a [Argument<'a>],
        span: Span,
        callee: Option<&'a IdentifierReference<'a>>,
        this: Option<Expr>,
    ) -> R<Option<Expr>> {
        // (An ordinary function using `this`, called plainly: undefined.)
        let this = this.or_else(|| self.m.cands[i].has_this.then(undefined));
        let call = |i: usize, out: Vec<Expr>| match &this {
            Some(t) => ExprKind::CallDirectThis(i, Box::new(t.clone()), out),
            None => ExprKind::Call(i, out),
        };
        // Async functions and generators are called through their JS entry
        // (which returns a promise or a generator object).
        let node = self.m.cands[i].node;
        if !self.m.cands[i].direct_callable || node.is_async() || node.is_generator() || self.m.uses_arguments(i) {
            return Ok(None);
        }
        let (params, ret) = self.m.signature(i, span)?;
        // A default constructor passing all its arguments on to an
        // interpreted one: them, in an array.
        if self.m.cands[i].ctor.as_ref().is_some_and(|k| k.dynamic && k.forward) {
            if args.iter().any(|a| a.as_expression().is_none()) {
                return Ok(None);
            }
            let mut all = Vec::new();
            self.push_args(&mut all, args)?;
            return Ok(Some(Expr::new(ret, call(i, vec![Expr::new(Ty::Any, ExprKind::Array(all))]))));
        }
        let rest = self.m.cands[i].rest;
        let fixed = params.len() - rest as usize;
        if (!rest && args.len() > params.len()) || args.iter().any(|a| a.as_expression().is_none()) {
            return Ok(None);
        }
        let start = self.pre.len();
        let mut out = Vec::with_capacity(params.len());
        let mut checks = Vec::new();
        if rest {
            // The extra arguments, in an array.
            let mut extra = Vec::new();
            for (k, a) in args.iter().enumerate() {
                let mark = self.pre.len();
                let x = self.expr(a.as_expression().unwrap())?;
                if k < fixed {
                    self.order(&mut out, mark);
                    out.push(x);
                } else {
                    // Spills go in at `mark`: the earliest operands last.
                    self.order(&mut extra, mark);
                    self.order(&mut out, mark);
                    extra.push(any(x));
                }
            }
            while out.len() < fixed {
                if params[out.len()] != Ty::Any {
                    self.pre.truncate(start);
                    return Ok(None);
                }
                out.push(undefined());
            }
            for (k, x) in out.iter_mut().enumerate() {
                match (params[k], x.ty) {
                    (Ty::Any, _) => *x = any(std::mem::replace(x, undefined())),
                    (p, _) if let Some(y) = fit(p, x) => *x = y,
                    _ => {
                        self.pre.truncate(start);
                        return Ok(None);
                    }
                }
            }
            out.push(Expr::new(Ty::Any, ExprKind::Array(extra)));
            return Ok(Some(Expr::new(ret, call(i, out))));
        }
        for (k, &p) in params.iter().enumerate() {
            let mark = self.pre.len();
            let x = match args.get(k) {
                Some(a) => self.expr(a.as_expression().unwrap())?,
                None if p == Ty::Any => undefined(),
                None => {
                    self.pre.truncate(start);
                    return Ok(None);
                }
            };
            self.order(&mut out, mark);
            out.push(match (p, x.ty) {
                (Ty::Any, _) => any(x),
                (p, _) if let Some(y) = fit(p, &x) => y,
                // A JS value for an `aot type` parameter: checked at the call.
                (Ty::Struct(l), Ty::Any | Ty::StructOrUndef(_)) if callee.is_some() => {
                    checks.push((k, ir::ArgCheck::Struct(l)));
                    any(x)
                }
                // An object of this one (or `null` / `undefined`) for a parameter that holds it.
                (Ty::StructOrUndef(l), Ty::Struct(l2)) if l == l2 => Expr { ty: p, ..x },
                (Ty::StructOrUndef(l), Ty::Any) if callee.is_some() => {
                    checks.push((k, ir::ArgCheck::StructOrNullish(l)));
                    any(x)
                }
                // A JS value for a `number` or `boolean` parameter: checked
                // at the call, as the function's entry would.
                (Ty::F64 | Ty::MaybeF64, Ty::Any) if callee.is_some() => {
                    checks.push((k, ir::ArgCheck::Number));
                    x
                }
                (Ty::Bool, Ty::Any) if callee.is_some() => {
                    checks.push((k, ir::ArgCheck::Bool));
                    x
                }
                // An unproven argument for an unboxed parameter: the caller
                // checks the arguments again.
                _ => {
                    self.pre.truncate(start);
                    return Ok(None);
                }
            });
        }
        if !checks.is_empty() {
            // (Checked calls pass no `this`: through the JS entry.)
            if this.is_some() {
                self.pre.truncate(start);
                return Ok(None);
            }
            let callee = any(self.ident(callee.unwrap())?);
            let call = ExprKind::CallChecked { func: i, callee: Box::new(callee), args: out, checks };
            return Ok(Some(Expr::new(Ty::Any, call)));
        }
        Ok(Some(Expr::new(ret, call(i, out))))
    }
}

struct LoopTest {
    pre: Vec<Stmt>,
    test: Option<Expr>,
}

impl LoopTest {
    /// The loop's test, and the start of its body.
    fn build(self, id: ir::LoopId) -> (Option<Expr>, Vec<Stmt>) {
        match self.test {
            Some(test) if !self.pre.is_empty() => {
                let mut body = self.pre;
                let not = Expr::new(Ty::Bool, ExprKind::Unary(UnOp::Not, Box::new(test)));
                body.push(Stmt::If(not, vec![Stmt::Break(id)], vec![]));
                (None, body)
            }
            test => (test, vec![]),
        }
    }
}

/// The declaration of the module binding in cell `k` (a `var` is only
/// assigned: it was initialized to `undefined` when the module started).
fn init_cell(k: ir::CellId, v: Expr, is_var: bool) -> Expr {
    let v = Box::new(any(v));
    Expr::new(Ty::Any, if is_var { ExprKind::SetCell(k, v) } else { ExprKind::InitCell(k, v) })
}

/// `x | 0`.
fn wrap(x: Expr) -> Expr {
    arith(AnyBinOp::BitOr, x, Expr::num(0.0))
}

/// An int32 literal (`5`, `-5`; not `-0`).
fn int_literal(e: &Expr) -> Option<i32> {
    let v = match &e.kind {
        ExprKind::Num(v) => *v,
        ExprKind::Unary(UnOp::Neg, x) => match x.kind {
            ExprKind::Num(v) => -v,
            _ => return None,
        },
        _ => return None,
    };
    let ok = v.fract() == 0.0 && v >= i32::MIN as f64 && v <= i32::MAX as f64 && !(v == 0.0 && v.is_sign_negative());
    ok.then_some(v as i32)
}

/// `x` as an argument for a parameter of ABI type `p`, if it is one without
/// a check: `int` arguments are ints, number ones any number.
fn fit(p: Ty, x: &Expr) -> Option<Expr> {
    match (p, x.ty) {
        _ if p == x.ty => Some(x.clone()),
        (Ty::F64, Ty::I32) => Some(x.clone()),
        (Ty::I32, _) => int_literal(x).map(|c| Expr::new(Ty::I32, ExprKind::Num(c as f64))),
        _ => None,
    }
}

/// `obj[key]`: on a typed array with a number key, its element.
fn get_elem(obj: Expr, key: Key) -> Expr {
    match (obj.ty, key) {
        (Ty::Arr(kind), Key::Value(k)) if k.ty.is_number() => {
            Expr::new(Ty::MaybeF64, ExprKind::TypedGet(kind, Box::new(obj), k))
        }
        (Ty::StructArr(l), Key::Value(k)) if k.ty.is_number() => {
            Expr::new(Ty::StructOrUndef(l), ExprKind::StructArrGet(l, Box::new(obj), k))
        }
        (_, Key::Atom(a)) => Expr::new(Ty::Any, ExprKind::GetProp(Box::new(any(obj)), a)),
        (_, Key::Value(k)) => Expr::new(Ty::Any, ExprKind::GetElem(Box::new(any(obj)), k)),
    }
}

/// `obj[key] = v` (evaluates to `v`).
fn set_elem(obj: Expr, key: Expr, v: Expr) -> Expr {
    match obj.ty {
        Ty::Arr(kind) if key.ty.is_number() && num(v.ty) => {
            Expr::new(v.ty, ExprKind::TypedSet(kind, Box::new(obj), Box::new(key), Box::new(v)))
        }
        Ty::StructArr(l) if key.ty.is_number() => {
            let v = if v.ty == Ty::Struct(l) { v } else { any(v) };
            Expr::new(v.ty, ExprKind::StructArrSet(l, Box::new(obj), Box::new(key), Box::new(v)))
        }
        _ => Expr::new(Ty::Any, ExprKind::SetElem(Box::new(any(obj)), Box::new(key), Box::new(any(v)))),
    }
}

fn set(l: ir::LocalId, v: Expr) -> Expr {
    Expr::new(v.ty, ExprKind::SetLocal(l, Box::new(v)))
}

/// Arithmetic: unboxed when both operands are numbers, generic otherwise.
fn arith(op: AnyBinOp, x: Expr, y: Expr) -> Expr {
    if num(x.ty) && num(y.ty) {
        let (x, y) = (to_f64(x), to_f64(y));
        let op = match op {
            AnyBinOp::Add => BinOp::Add,
            AnyBinOp::Sub => BinOp::Sub,
            AnyBinOp::Mul => BinOp::Mul,
            AnyBinOp::Div => BinOp::Div,
            AnyBinOp::Rem => BinOp::Rem,
            AnyBinOp::Pow => BinOp::Pow,
            AnyBinOp::Shl => BinOp::Shl,
            AnyBinOp::Sar => BinOp::Sar,
            AnyBinOp::Shr => BinOp::Shr,
            AnyBinOp::BitAnd => BinOp::BitAnd,
            AnyBinOp::BitOr => BinOp::BitOr,
            AnyBinOp::BitXor => BinOp::BitXor,
        };
        // ToInt32 results; `>>>` is a uint32.
        let ty = match op {
            BinOp::Shl | BinOp::Sar | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => Ty::I32,
            _ => Ty::F64,
        };
        return Expr::new(ty, ExprKind::Binary(op, Box::new(x), Box::new(y)));
    }
    // With a number operand, the result is a number (a BigInt one throws).
    let numeric = match op {
        AnyBinOp::Sub => Some(BinOp::Sub),
        AnyBinOp::Mul => Some(BinOp::Mul),
        AnyBinOp::Div => Some(BinOp::Div),
        AnyBinOp::Rem => Some(BinOp::Rem),
        AnyBinOp::Pow => Some(BinOp::Pow),
        _ => None,
    };
    if let Some(op) = numeric
        && (num(x.ty) || num(y.ty))
    {
        let side = |e: Expr| if num(e.ty) { to_f64(e) } else { any(e) };
        return Expr::new(Ty::F64, ExprKind::NumArith(op, Box::new(side(x)), Box::new(side(y))));
    }
    let int32 = matches!(op, AnyBinOp::Shl | AnyBinOp::Sar | AnyBinOp::BitAnd | AnyBinOp::BitOr | AnyBinOp::BitXor)
        && (num(x.ty) || num(y.ty));
    let v = Expr::new(Ty::Any, ExprKind::AnyBinary(op, Box::new(any(x)), Box::new(any(y))));
    // With a number operand, a BigInt one throws: the result is an int32.
    if int32 { Expr::new(Ty::I32, ExprKind::IntOf(Box::new(v))) } else { v }
}

/// JS truthiness as a `Bool`.
fn truthy(x: Expr) -> Expr {
    match x.ty {
        Ty::Bool => x,
        _ => Expr::new(Ty::Bool, ExprKind::Truthy(Box::new(any_or_num(x)))),
    }
}

fn any_or_num(x: Expr) -> Expr {
    if num(x.ty) { x } else { any(x) }
}

/// Whether values of type `t` are numbers in arithmetic (`MaybeF64`'s
/// `undefined` converts to NaN, which it is).
fn num(t: Ty) -> bool {
    matches!(t, Ty::F64 | Ty::I32 | Ty::MaybeF64)
}

/// A `num` value converted with ToNumber: the same bits.
/// What a value converted to `desc` is (see `ExprKind::StructConvert`).
fn desc_ty(desc: i32) -> Ty {
    let k = (desc >> 1) as u32;
    if desc & 1 == 1 { Ty::StructArr(k) } else { Ty::Struct(k) }
}

fn to_f64(x: Expr) -> Expr {
    if x.ty == Ty::MaybeF64 { Expr { ty: Ty::F64, ..x } } else { x }
}

/// In a function returning `Any`, box every returned value.
fn box_returns(body: Vec<Stmt>) -> Vec<Stmt> {
    fn s(st: Stmt) -> Stmt {
        match st {
            Stmt::Return(e) => Stmt::Return(Some(e.map_or_else(undefined, any))),
            Stmt::If(c, a, b) => Stmt::If(c, l(a), l(b)),
            Stmt::Loop { id, test, post_test, body, update, per_iteration } => {
                Stmt::Loop { id, test, post_test, body: l(body), update, per_iteration }
            }
            Stmt::Switch { id, disc, cases } => {
                Stmt::Switch { id, disc, cases: cases.into_iter().map(|(t, b)| (t, l(b))).collect() }
            }
            Stmt::Block(b) => Stmt::Block(l(b)),
            Stmt::IterGuard { iter, next, loop_id, body } => Stmt::IterGuard { iter, next, loop_id, body: l(body) },
            Stmt::Try { body, catch, finally, exc } => {
                Stmt::Try { body: l(body), catch: catch.map(|(p, b)| (p, l(b))), finally: finally.map(l), exc }
            }
            other => other,
        }
    }
    fn l(v: Vec<Stmt>) -> Vec<Stmt> {
        v.into_iter().map(s).collect()
    }
    l(body)
}

fn math_call(f: MathFn, mut args: Vec<Expr>, span: Span) -> R<Expr> {
    if matches!(f, MathFn::Min | MathFn::Max) {
        // Math.min()/max() are n-ary; fold into a left-to-right chain, which
        // keeps evaluation order and NaN / -0 semantics.
        if args.is_empty() {
            return Ok(Expr::num(if f == MathFn::Min { f64::INFINITY } else { f64::NEG_INFINITY }));
        }
        let mut it = args.into_iter();
        let mut acc = it.next().unwrap();
        for a in it {
            // The min / max of int32s is one of them.
            let ty = if acc.ty == Ty::I32 && a.ty == Ty::I32 { Ty::I32 } else { Ty::F64 };
            acc = Expr::new(ty, ExprKind::Math(f, vec![acc, a]));
        }
        return Ok(acc);
    }
    if args.len() != f.arity() {
        return reject(span, format!("expected {} argument(s)", f.arity()));
    }
    let ret = f.ret();
    args.truncate(f.arity());
    Ok(Expr::new(ret, ExprKind::Math(f, args)))
}

/// `Math.X` / `Number.X` where the object is the (unshadowed) global.
fn global_member<'s>(m: &'s StaticMemberExpression, scoping: &Scoping) -> Option<(&'s str, &'s str)> {
    if m.optional {
        return None;
    }
    let Expression::Identifier(obj) = &m.object else { return None };
    // A reference that resolves to a symbol means a declaration shadows the global.
    if obj.reference_id.get().is_none_or(|r| scoping.get_reference(r).symbol_id().is_some()) {
        return None;
    }
    Some((obj.name.as_str(), m.property.name.as_str()))
}

fn builtin_const(m: &StaticMemberExpression, scoping: &Scoping) -> Option<f64> {
    use std::f64::consts as c;
    Some(match global_member(m, scoping)? {
        ("Math", "PI") => c::PI,
        ("Math", "E") => c::E,
        ("Math", "LN2") => c::LN_2,
        ("Math", "LN10") => c::LN_10,
        ("Math", "LOG2E") => c::LOG2_E,
        ("Math", "LOG10E") => c::LOG10_E,
        ("Math", "SQRT2") => c::SQRT_2,
        ("Math", "SQRT1_2") => c::FRAC_1_SQRT_2,
        ("Number", "EPSILON") => f64::EPSILON,
        ("Number", "MAX_VALUE") => f64::MAX,
        ("Number", "MIN_VALUE") => 5e-324,
        ("Number", "MAX_SAFE_INTEGER") => 9007199254740991.0,
        ("Number", "MIN_SAFE_INTEGER") => -9007199254740991.0,
        ("Number", "POSITIVE_INFINITY") => f64::INFINITY,
        ("Number", "NEGATIVE_INFINITY") => f64::NEG_INFINITY,
        ("Number", "NaN") => f64::NAN,
        _ => return None,
    })
}

fn builtin_fn(m: &StaticMemberExpression, scoping: &Scoping) -> Option<MathFn> {
    use MathFn::*;
    Some(match global_member(m, scoping)? {
        ("Math", "abs") => Abs,
        ("Math", "sqrt") => Sqrt,
        ("Math", "floor") => Floor,
        ("Math", "ceil") => Ceil,
        ("Math", "trunc") => Trunc,
        ("Math", "fround") => Fround,
        ("Math", "min") => Min,
        ("Math", "max") => Max,
        ("Math", "imul") => Imul,
        ("Math", "clz32") => Clz32,
        ("Math", "sign") => Sign,
        ("Math", "round") => Round,
        ("Math", "pow") => Pow,
        ("Math", "sin") => Sin,
        ("Math", "cos") => Cos,
        ("Math", "tan") => Tan,
        ("Math", "asin") => Asin,
        ("Math", "acos") => Acos,
        ("Math", "atan") => Atan,
        ("Math", "atan2") => Atan2,
        ("Math", "sinh") => Sinh,
        ("Math", "cosh") => Cosh,
        ("Math", "tanh") => Tanh,
        ("Math", "asinh") => Asinh,
        ("Math", "acosh") => Acosh,
        ("Math", "atanh") => Atanh,
        ("Math", "exp") => Exp,
        ("Math", "expm1") => Expm1,
        ("Math", "log") => Log,
        ("Math", "log1p") => Log1p,
        ("Math", "log2") => Log2,
        ("Math", "log10") => Log10,
        ("Math", "cbrt") => Cbrt,
        ("Math", "hypot") => Hypot,
        ("Number", "isNaN") => IsNaN,
        ("Number", "isFinite") => IsFinite,
        ("Number", "isInteger") => IsInteger,
        ("Number", "isSafeInteger") => IsSafeInteger,
        _ => return None,
    })
}

// ---- reachability ----

/// Whether execution can fall off the end of `list`.
fn completes_list(list: &[Stmt]) -> bool {
    list.iter().all(completes)
}

fn completes(s: &Stmt) -> bool {
    match s {
        Stmt::Expr(_) | Stmt::NewBoxes(_) => true,
        Stmt::Await { target, .. } => !matches!(target, ir::AwaitTarget::Return),
        Stmt::Yield { .. } => true,
        Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Throw(_) => false,
        Stmt::Block(b) => completes_list(b),
        Stmt::IterGuard { body, .. } => completes_list(body),
        Stmt::If(_, a, b) => completes_list(a) || completes_list(b),
        Stmt::Loop { id, test, body, .. } => {
            let infinite = matches!(test, None | Some(Expr { kind: ExprKind::Bool(true), .. }));
            !infinite || breaks_to(body, *id)
        }
        // Conservative: a switch may complete.
        Stmt::Switch { .. } => true,
        Stmt::Try { body, catch, finally, .. } => {
            (completes_list(body) || catch.as_ref().is_some_and(|(_, c)| completes_list(c)))
                && finally.as_ref().is_none_or(|f| completes_list(f))
        }
    }
}

fn breaks_to(list: &[Stmt], id: ir::LoopId) -> bool {
    list.iter().any(|s| match s {
        Stmt::Break(t) => *t == id,
        Stmt::Block(b) => breaks_to(b, id),
        Stmt::IterGuard { body, .. } => breaks_to(body, id),
        Stmt::If(_, a, b) => breaks_to(a, id) || breaks_to(b, id),
        Stmt::Loop { body, .. } => breaks_to(body, id),
        Stmt::Switch { cases, .. } => cases.iter().any(|(_, b)| breaks_to(b, id)),
        Stmt::Try { body, catch, finally, .. } => {
            breaks_to(body, id)
                || catch.as_ref().is_some_and(|(_, c)| breaks_to(c, id))
                || finally.as_ref().is_some_and(|f| breaks_to(f, id))
        }
        _ => false,
    })
}

fn describe_stmt(s: &Statement) -> &'static str {
    match s {
        Statement::ForInStatement(_) => "for-in",
        Statement::ForOfStatement(_) => "for-of",
        Statement::FunctionDeclaration(_) => "nested function",
        Statement::DebuggerStatement(_) => "debugger",
        Statement::WithStatement(_) => "with",
        Statement::ClassDeclaration(_) => "class",
        _ => "this statement kind",
    }
}

fn describe_expr(e: &Expression) -> &'static str {
    match e {
        Expression::BigIntLiteral(_) => "bigint",
        Expression::RegExpLiteral(_) => "regular expression",
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => "closure",
        Expression::ClassExpression(_) => "class expression",
        Expression::ThisExpression(_) => "this",
        Expression::AwaitExpression(_) => "await",
        Expression::ChainExpression(_) => "optional chaining",
        Expression::ImportExpression(_) => "dynamic import",
        Expression::TaggedTemplateExpression(_) => "tagged template",
        Expression::PrivateFieldExpression(_) | Expression::PrivateInExpression(_) => "private field",
        Expression::Super(_) => "super",
        Expression::NewTarget(_) => "new.target",
        _ => "this expression kind",
    }
}

fn has_spread(args: &[Argument]) -> bool {
    args.iter().any(|a| matches!(a, Argument::SpreadElement(_)))
}

/// An array literal from its parts: holes (`None`), and values that are
/// spread (`Some(true)`) or not.
fn array_parts(parts: Vec<Option<bool>>, values: Vec<Expr>) -> Expr {
    let mut values = values.into_iter();
    let parts = parts
        .into_iter()
        .map(|p| match p {
            None => ir::ArrPart::Hole,
            Some(true) => ir::ArrPart::Spread(values.next().unwrap()),
            Some(false) => ir::ArrPart::Value(values.next().unwrap()),
        })
        .collect();
    Expr::new(Ty::Any, ExprKind::ArrayParts(parts))
}

// ---- module code ----

/// The module-level bindings code names (in all of it, nested functions
/// included), as `visit` walks it.
fn module_refs<'a>(scoping: &Scoping, visit: impl FnOnce(&mut RefsVisitor<'_>)) -> Vec<SymbolId> {
    let mut v = RefsVisitor { scoping, out: Vec::new() };
    visit(&mut v);
    v.out
}

struct RefsVisitor<'s> {
    scoping: &'s Scoping,
    out: Vec<SymbolId>,
}

impl<'a> oxc::ast_visit::Visit<'a> for RefsVisitor<'_> {
    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        if let Some(sym) = it.reference_id.get().and_then(|r| self.scoping.get_reference(r).symbol_id()) {
            if self.scoping.symbol_scope_id(sym) == self.scoping.root_scope_id() {
                self.out.push(sym);
            }
        }
    }
}

/// For each module-level binding, the module-level bindings its value's
/// code may name when the program uses it: a function's or a class's body,
/// what a variable's initializer names (functions and objects in it).
fn module_deferred_refs(program: &Program, scoping: &Scoping) -> HashMap<SymbolId, Vec<SymbolId>> {
    use oxc::ast_visit::Visit;
    let mut out: HashMap<SymbolId, Vec<SymbolId>> = HashMap::new();
    for stmt in &program.body {
        let decl = match stmt {
            Statement::ExportDeclaration(e) => &e.declaration,
            _ => match stmt.as_declaration() {
                Some(d) => d,
                None => continue,
            },
        };
        match decl {
            Declaration::FunctionDeclaration(f) => {
                if let Some(sym) = f.id.as_ref().and_then(|id| id.symbol_id.get()) {
                    out.insert(sym, module_refs(scoping, |v| v.visit_function(f, oxc::syntax::scope::ScopeFlags::empty())));
                }
            }
            Declaration::ClassDeclaration(c) => {
                if let Some(sym) = c.id.as_ref().and_then(|id| id.symbol_id.get()) {
                    out.insert(sym, module_refs(scoping, |v| v.visit_class(c)));
                }
            }
            Declaration::VariableDeclaration(v) => {
                for d in &v.declarations {
                    let Some(init) = &d.init else { continue };
                    let refs = module_refs(scoping, |vis| vis.visit_expression(init));
                    for id in d.id.get_binding_identifiers() {
                        if let Some(sym) = id.symbol_id.get() {
                            out.insert(sym, refs.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Positions of every declaration of and reference to each module-level
/// binding.
fn module_uses(program: &Program, scoping: &Scoping) -> HashMap<SymbolId, Vec<u32>> {
    use oxc::ast_visit::Visit;
    struct V<'s> {
        scoping: &'s Scoping,
        uses: HashMap<SymbolId, Vec<u32>>,
    }
    impl V<'_> {
        fn add(&mut self, sym: SymbolId, at: u32) {
            if self.scoping.symbol_scope_id(sym) == self.scoping.root_scope_id() {
                self.uses.entry(sym).or_default().push(at);
            }
        }
    }
    impl<'a> Visit<'a> for V<'_> {
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            if let Some(sym) = it.reference_id.get().and_then(|r| self.scoping.get_reference(r).symbol_id()) {
                self.add(sym, it.span.start);
            }
        }
        fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
            if let Some(sym) = it.symbol_id.get() {
                self.add(sym, it.span.start);
            }
        }
    }
    let mut v = V { scoping, uses: HashMap::new() };
    v.visit_program(program);
    v.uses
}

/// Whether a module-level statement may be part of native module code: it
/// runs in order with its neighbours (no hoisting), does not wait (no
/// top-level `await`), and declares nothing that stays a binding of the
/// interpreted module (functions, classes, exports, folded constants).
fn segment_stmt(s: &Statement, bindings: &HashMap<SymbolId, Binding>, consts: &HashMap<SymbolId, ConstVal>) -> bool {
    use oxc::ast_visit::{Visit, walk};
    struct Awaits(bool);
    impl<'a> Visit<'a> for Awaits {
        fn visit_await_expression(&mut self, _: &AwaitExpression<'a>) {
            self.0 = true;
        }
        fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
            self.0 |= it.r#await;
            walk::walk_for_of_statement(self, it);
        }
        fn visit_function(&mut self, _: &Function<'a>, _: oxc::syntax::scope::ScopeFlags) {}
        fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}
    }
    let ok = match s {
        Statement::ExpressionStatement(_)
        | Statement::IfStatement(_)
        | Statement::ForStatement(_)
        | Statement::ForInStatement(_)
        | Statement::ForOfStatement(_)
        | Statement::WhileStatement(_)
        | Statement::DoWhileStatement(_)
        | Statement::BlockStatement(_)
        | Statement::LabeledStatement(_)
        | Statement::SwitchStatement(_)
        | Statement::TryStatement(_)
        | Statement::ThrowStatement(_)
        | Statement::EmptyStatement(_) => true,
        Statement::VariableDeclaration(v) => {
            !v.declare
                && matches!(
                    v.kind,
                    VariableDeclarationKind::Let | VariableDeclarationKind::Const | VariableDeclarationKind::Var
                )
                && v.declarations.iter().all(|d| {
                    // (A function bound to the name is a function of its
                    // own; a class, made by the interpreter, its methods
                    // native: the statement between two segments.)
                    !matches!(
                        d.init.as_ref().map(|i| i.without_parentheses()),
                        Some(
                            Expression::ArrowFunctionExpression(_)
                                | Expression::FunctionExpression(_)
                                | Expression::ClassExpression(_)
                        )
                    ) && d.id.get_binding_identifiers().iter().all(|id| {
                        let sym = id.symbol_id.get().unwrap();
                        !consts.contains_key(&sym) && bindings.get(&sym).is_none_or(|b| !b.exported)
                    })
                })
        }
        _ => false,
    };
    if !ok {
        return false;
    }
    let mut a = Awaits(false);
    a.visit_statement(s);
    !a.0
}

/// The maximal runs of `segment_stmt`s, with something to run.
fn segment_runs<'a>(
    body: &'a [Statement<'a>],
    bindings: &HashMap<SymbolId, Binding>,
    consts: &HashMap<SymbolId, ConstVal>,
) -> Vec<&'a [Statement<'a>]> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, s) in body.iter().enumerate() {
        match (segment_stmt(s, bindings, consts), start) {
            (true, None) => start = Some(i),
            (false, Some(st)) => {
                runs.push(&body[st..i]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(st) = start {
        runs.push(&body[st..]);
    }
    runs.retain(|r| r.iter().any(|s| !matches!(s, Statement::EmptyStatement(_))));
    runs
}

/// Native module code for these statements. The module bindings they
/// declare are locals, unless code elsewhere uses them (then cells).
fn segment<'a>(stmts: &'a [Statement<'a>], scoping: &Scoping, uses: &HashMap<SymbolId, Vec<u32>>) -> Candidate<'a> {
    use oxc::ast_visit::Visit;
    let span = Span::new(stmts[0].span().start, stmts[stmts.len() - 1].span().end);
    // The module bindings these statements declare (a symbol's own span
    // may be another declaration of the name: `type T` and `const T`).
    struct Declared<'s>(&'s Scoping, HashSet<SymbolId>);
    impl<'a> Visit<'a> for Declared<'_> {
        fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
            if let Some(sym) = it.symbol_id.get()
                && self.0.symbol_scope_id(sym) == self.0.root_scope_id()
            {
                self.1.insert(sym);
            }
        }
    }
    let mut declared = Declared(scoping, HashSet::new());
    stmts.iter().for_each(|s| declared.visit_statement(s));
    let mut cells = HashSet::new();
    for sym in declared.1 {
        if uses.get(&sym).is_some_and(|at| at.iter().any(|&p| p < span.start || p >= span.end)) {
            cells.insert(sym);
        }
    }
    Candidate {
        name: "<module code>".into(),
        node: FnNode::Segment(span, stmts),
        symbol: None,
        has_this: false,
        site: Site::Segment { span },
        direct_callable: false,
        params: Vec::new(),
        rest: false,
        ret: None,
        class: None,
        cells,
        ctor: None,
    }
}

/// The classes' superclasses and native members, for `ir::predict_calls`.
fn class_tables(classes: &[ClassInfo], cands: &[Candidate]) -> Vec<ir::ClassTable> {
    let mut tables: Vec<ir::ClassTable> = classes
        .iter()
        .map(|c| ir::ClassTable {
            superclass: c.superclass,
            members: Vec::new(),
            fields: c.fields.iter().filter_map(|(n, h)| Some((n.clone(), resolve_hint(h, classes)?))).collect(),
            nums: c.nums.clone(),
            instance_fields: Vec::new(),
            private_state: false,
        })
        .collect();
    // (A class's private names, or a superclass's.)
    for k in 0..classes.len() {
        let mut at = Some(k);
        while let Some(c) = at {
            if !classes[c].privates.is_empty() {
                tables[k].private_state = true;
                break;
            }
            at = classes[c].superclass;
        }
    }
    for (i, c) in cands.iter().enumerate() {
        if let (Some((k, is_static)), Site::Method { key, accessor, .. }) = (c.class, &c.site) {
            tables[k].members.push((key.clone(), *accessor, is_static, i));
        }
        if let Some(k) = &c.ctor
            && k.home == k.class
        {
            tables[k.class].instance_fields = k.instance_fields.clone();
        }
    }
    tables
}
