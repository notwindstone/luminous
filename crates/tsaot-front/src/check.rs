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
    Method { class: String, is_static: bool, key: String, stmt_end: u32 },
    /// Module-level statements, replaced by a call of the native code.
    Segment { span: Span },
}

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
    /// By wrapping the declarator's initializer: `x = __tsaot_init(k, init)`.
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
) -> Outcome<'a> {
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    let mut consts = HashMap::new();
    let mut bindings = HashMap::new();
    let mut classes = Vec::new();
    collect_toplevel(
        program,
        scoping,
        interpreted,
        &mut classes,
        &mut candidates,
        &mut rejected,
        &mut consts,
        &mut bindings,
    );
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
    // Fixpoint: a function is native only if all its native callees are, and
    // callers must agree with the callees' actual return types.
    loop {
        let mut cx = ModuleCx {
            scoping,
            int_syms,
            interpreted,
            consts: &consts,
            bindings: &bindings,
            cands: &candidates,
            state: vec![State::Todo; candidates.len()],
            widened: &mut widened,
            ret_override: &mut ret_override,
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
                    State::Done(f) => f,
                    _ => unreachable!(),
                })
                .collect();
            let cells = cx.cell_infos();
            let class_uses = cx.class_uses();
            let module = ir::Module {
                funcs,
                closures: std::mem::take(&mut cx.closures),
                strings: cx.strings,
                atoms: cx.atoms,
                cells: cells.iter().map(|c| c.name.clone()).collect(),
                privates: cx.privates.len(),
                homes: 2 * classes.len(),
            };
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
) {
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
    // directly from native code, only while no code has run yet: before the
    // first statement that is not a pure declaration, nothing can observe
    // them in their TDZ.
    let mut pure_prefix = true;
    let mut methods = Vec::new();
    let mut method_rejects = Vec::new();
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
                has_this: false,
                site,
                direct_callable,
                params,
                rest,
                ret,
                class: None,
                cells: HashSet::new(),
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
                        pure_prefix = false;
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
                        pure_prefix = false;
                        continue;
                    }
                };
                for d in &v.declarations {
                    let BindingPattern::BindingIdentifier(id) = &d.id else {
                        // Destructuring: bind every name; initialized after the statement.
                        for id in d.id.get_binding_identifiers() {
                            bind(id, kind, None, end, exported);
                        }
                        pure_prefix = false;
                        continue;
                    };
                    bind(id, kind, d.init.as_ref().map(|i| i.span()), end, exported);
                    let symbol = id.symbol_id.get().unwrap();
                    let Some(init) = &d.init else {
                        continue;
                    };
                    if kind != BindingKind::Const {
                        pure_prefix = false;
                        continue;
                    }
                    let site = Site::ConstInit { init: init.span() };
                    // Creating a function runs no code, so it keeps the prefix pure.
                    match init.without_parentheses() {
                        Expression::ArrowFunctionExpression(a) => {
                            add(id.name.to_string(), FnNode::Arrow(a), symbol, site, pure_prefix);
                        }
                        Expression::FunctionExpression(f) => {
                            add(id.name.to_string(), FnNode::Function(f), symbol, site, pure_prefix);
                        }
                        _ => match literal(init) {
                            Some(val) if pure_prefix => {
                                consts.insert(symbol, val);
                            }
                            _ => pure_prefix = false,
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
                    }
                    classes.push(class_info(c, &id.name));
                }
                pure_prefix = false;
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

fn class_info(c: &Class, name: &str) -> ClassInfo {
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
    ClassInfo { name: name.to_string(), body_start: c.body.span.start + 1, privates }
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
        if m.kind != MethodDefinitionKind::Method
            || m.computed
            || !m.decorators.is_empty()
            || m.r#type != MethodDefinitionType::MethodDefinition
            || m.value.body.is_none()
        {
            continue;
        }
        let key = match &m.key {
            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
            PropertyKey::StringLiteral(s) => s.value.to_string(),
            _ => continue,
        };
        let node = FnNode::Function(&m.value);
        let site = Site::Method { class: class.to_string(), is_static: m.r#static, key: key.clone(), stmt_end };
        match header(node) {
            Ok((params, rest, ret)) => out.push(Candidate {
                name: key,
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
            }),
            Err(r) => rejected.push((format!("{class}.{key}"), node.span(), r)),
        }
    }
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

/// The unboxed type a TS annotation allows, if any.
fn ts_type(t: &TSType) -> Option<Ty> {
    match t {
        TSType::TSNumberKeyword(_) => Some(Ty::F64),
        TSType::TSBooleanKeyword(_) => Some(Ty::Bool),
        TSType::TSVoidKeyword(_) => Some(Ty::Void),
        TSType::TSParenthesizedType(p) => ts_type(&p.type_annotation),
        // Typed arrays (or a user type of the same name: either way, the
        // entry checks the class).
        TSType::TSTypeReference(r) => match &r.type_name {
            TSTypeName::IdentifierReference(id) => match id.name.as_str() {
                // `type int = number` / `type double = number`: `int` is
                // checked to hold int32 values.
                "int" => Some(Ty::I32),
                "double" => Some(Ty::F64),
                name => ArrKind::from_name(name).map(Ty::Arr),
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
    let (span, is_async, generator, this_param, params, return_type) = match node {
        FnNode::Segment(..) => return Ok((Vec::new(), false, None)),
        FnNode::Function(f) => (f.span, f.r#async, f.generator, f.this_param.is_some(), &f.params, &f.return_type),
        FnNode::Arrow(a) => (a.span, a.r#async, false, false, &a.params, &a.return_type),
    };
    if generator && is_async {
        return reject(span, "async generators are not supported yet");
    }
    if this_param {
        return reject(span, "`this` parameters are not supported");
    }
    let mut out = Vec::new();
    for p in &params.items {
        let annotated = p.type_annotation.as_ref().and_then(|a| ts_type(&a.type_annotation));
        let ty = match annotated {
            Some(t @ (Ty::F64 | Ty::I32 | Ty::Bool | Ty::Arr(_)))
                if p.initializer.is_none()
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
    Done(ir::Func),
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
    cands: &'m [Candidate<'a>],
    state: Vec<State>,
    /// Per function: locals that must be `Any`.
    widened: &'m mut HashMap<u32, HashSet<SymbolId>>,
    /// Actual return types that differ from the annotation.
    ret_override: &'m mut HashMap<u32, Ty>,
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
}

impl<'m, 'a> ModuleCx<'m, 'a> {
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
        let cells = c.cells.clone();
        let caps = Rc::new(capture_info(node, self.scoping, &cells));
        let result = loop {
            let widened = self.widened.get(&sym).cloned().unwrap_or_default();
            let mut fx = FnCx::new(self, caps.clone(), node.span().start, false, widened);
            fx.rest = rest;
            fx.class = class;
            fx.has_this = has_this;
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
                State::Done(f)
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
            State::InProgress => reject(
                call_span,
                format!("recursive call to `{}`, which has no return type annotation", self.cands[i].name),
            ),
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
                        Site::Declaration { .. } | Site::Method { .. } | Site::Segment { .. } => CellInit::Header,
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
    is_async: bool,
    is_generator: bool,
    /// Methods (and the closures in them): their class, and whether the
    /// method is static.
    class: Option<(usize, bool)>,
    /// Enclosing `for`-`in` loops: their key variable and iterator.
    for_in_keys: Vec<(SymbolId, ir::LocalId)>,
    /// The last parameter is a rest parameter.
    rest: bool,
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
}

/// `cells`: module bindings a segment declares that live in cells (not
/// captured: closures access the cell).
fn capture_info(node: FnNode, scoping: &Scoping, cells: &HashSet<SymbolId>) -> CaptureInfo {
    use oxc::ast_visit::{Visit, walk};
    use oxc::syntax::scope::ScopeFlags;
    struct V<'s> {
        scoping: &'s Scoping,
        cells: &'s HashSet<SymbolId>,
        collect_refs: bool,
        stack: Vec<u32>,
        owner: HashMap<SymbolId, u32>,
        info: CaptureInfo,
    }
    impl<'a> Visit<'a> for V<'_> {
        fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
            // A declaration's name belongs to the enclosing function.
            if it.is_declaration()
                && let (Some(id), Some(&cur)) = (&it.id, self.stack.last())
            {
                self.owner.entry(id.symbol_id.get().unwrap()).or_insert(cur);
            }
            self.stack.push(it.span.start);
            walk::walk_function(self, it, flags);
            self.stack.pop();
        }
        fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
            self.stack.push(it.span.start);
            walk::walk_arrow_function_expression(self, it);
            self.stack.pop();
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
            let Some(sym) = it.reference_id.get().and_then(|r| self.scoping.get_reference(r).symbol_id()) else {
                return;
            };
            let Some(&owner) = self.owner.get(&sym) else { return };
            if self.stack.last() == Some(&owner) {
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
    let mut v =
        V { scoping, cells, collect_refs: false, stack: Vec::new(), owner: HashMap::new(), info: Default::default() };
    for pass in [false, true] {
        v.collect_refs = pass;
        match node {
            FnNode::Function(f) => v.visit_function(f, ScopeFlags::Function),
            FnNode::Arrow(a) => v.visit_arrow_function_expression(a),
            FnNode::Segment(span, stmts) => {
                v.stack.push(span.start);
                stmts.iter().for_each(|s| v.visit_statement(s));
                v.stack.pop();
            }
        }
    }
    v.info
}

/// `e` as a JS value.
fn any(e: Expr) -> Expr {
    match e.ty {
        Ty::Any => e,
        Ty::F64 | Ty::I32 | Ty::Bool | Ty::MaybeF64 => Expr::new(Ty::Any, ExprKind::Box(Box::new(e))),
        // Already a JSValue.
        Ty::Arr(_) => Expr { ty: Ty::Any, ..e },
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
            is_async: false,
            is_generator: false,
            ret_slot: None,
            rest: false,
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
        self.is_async = node.is_async();
        self.is_generator = matches!(node, FnNode::Function(f) if f.generator);
        let mut abi = Vec::new();
        let mut prologue = Vec::new();
        let mut boxed_params = Vec::new();
        for &(pat, ty, _) in &params {
            // A destructured parameter is a hidden local, bound below.
            let sym = match pat {
                BindingPattern::BindingIdentifier(id) => id.symbol_id.get(),
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
        if self.suspends() {
            self.ret_slot = Some(self.temp(Ty::Any));
        }
        // Closures: locals bound to the captured boxes.
        if self.is_closure {
            let free = self.caps.free.get(&self.key).cloned().unwrap_or_default();
            for sym in free {
                let l = self.declare_boxed(sym);
                self.captures.push(l);
            }
        }
        // Captured parameters move into boxes.
        let mut new_boxes = Vec::new();
        let mut inits = Vec::new();
        for (sym, p) in boxed_params {
            let b = self.declare_boxed(sym);
            new_boxes.push(b);
            inits.push(Stmt::Expr(Expr::new(Ty::Any, ExprKind::InitLocal(b, Box::new(any(self.local(p)))))));
        }
        if !new_boxes.is_empty() {
            prologue.push(Stmt::NewBoxes(new_boxes));
            prologue.extend(inits);
        }
        // Default values and destructuring, in order (each parameter may see
        // the previous ones).
        for (i, &(pat, _, default)) in params.iter().enumerate() {
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
        let mut ret = None;
        for &t in &self.returns {
            ret = Some(ret.map_or(t, |r| join(r, t)));
        }
        if completes_list(&body) {
            ret = Some(ret.map_or(Ty::Void, |r| join(r, Ty::Void)));
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
        let fixed = params.len() - self.rest as usize;
        let js_length = params[..fixed].iter().take_while(|p| p.2.is_none()).count();
        let mut func = ir::Func {
            name,
            js_length,
            params: abi,
            ret,
            locals: std::mem::take(&mut self.locals),
            boxed: std::mem::take(&mut self.boxed),
            local_names: std::mem::take(&mut self.local_names),
            captures: std::mem::take(&mut self.captures),
            is_closure: self.is_closure,
            has_this: self.has_this,
            is_async: self.is_async,
            is_generator: self.is_generator,
            rest: self.rest,
            ret_slot: self.ret_slot,
            body: prologue,
        };
        ir::scalar_replace(&mut func);
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
                Some(Declaration::ClassDeclaration(c)) => {
                    return reject(c.span, "classes inside functions are not supported yet");
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
                if let Some(Expression::AwaitExpression(aw)) = r.argument.as_ref().map(|a| a.without_parentheses()) {
                    let value = any(self.expr(&aw.argument)?);
                    self.returns.push(Ty::Any);
                    return Ok(Some(Stmt::Await { value, target: ir::AwaitTarget::Return }));
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
                    && matches!(a.right.without_parentheses(), Expression::AwaitExpression(_)) =>
            {
                let Expression::AwaitExpression(aw) = a.right.without_parentheses() else { unreachable!() };
                let local = match &a.left {
                    AssignmentTarget::AssignmentTargetIdentifier(id) => {
                        self.resolve(id).and_then(|s| self.sym_local.get(&s).copied().map(|l| (s, l)))
                    }
                    _ => None,
                };
                let Some((sym, l)) = local else {
                    return reject(a.span, "`await` is only supported assigned to a local variable");
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
        let d = any(self.expr(&sw.discriminant)?);
        let disc = self.temp(Ty::Any);
        let init = Stmt::Expr(Expr::new(Ty::Any, ExprKind::SetLocal(disc, Box::new(d))));
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
            let test = c.test.as_ref().map(|t| self.expr(t).map(any)).transpose()?;
            awaits |= self.pre.len() > m;
            tests.push((self.pre.split_off(m), test));
        }
        debug_assert_eq!(self.pre.len(), mark);
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
                    None if is_var => continue,
                    None => undefined(),
                };
                self.name_hint = None;
                let init = if self.wraps(Some(sym)) && init.ty != Ty::I32 { wrap(init) } else { init };
                let k = self.m.cell(sym, d.span)?;
                out.extend(self.pre.drain(base..));
                out.push(Stmt::Expr(init_cell(k, init, is_var)));
                continue;
            }
            if let Some(Expression::AwaitExpression(aw)) = d.init.as_ref().map(|i| i.without_parentheses()) {
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

    /// A nested function, compiled natively as a closure over the boxes of
    /// the variables it captures.
    fn closure(&mut self, node: FnNode<'a>, name: String) -> R<Expr> {
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
        inner.class = self.class;
        inner.ret_int = ret == Some(Ty::I32);
        let r = inner.function(node, name, params);
        let request = inner.widen_request;
        let f = match r {
            Ok(f) => f,
            Err(e) => {
                if e.retry {
                    self.widen_request = request;
                }
                return Err(e);
            }
        };
        let mut boxes = Vec::new();
        for sym in self.caps.free.get(&key).cloned().unwrap_or_default() {
            match self.sym_local.get(&sym) {
                Some(&l) if self.boxed[l] => boxes.push(l),
                _ => return reject(node.span(), "closure captures a variable that is not in scope here"),
            }
        }
        self.m.closures.push(f);
        self.m.closure_spans.push(node.span());
        Ok(Expr::new(Ty::Any, ExprKind::Closure(self.m.closures.len() - 1, boxes)))
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
                let src = self.temp(Ty::Any);
                out.push(Stmt::Expr(set(src, Expr::new(Ty::Any, ExprKind::ToObject(Box::new(any(v)))))));
                let mut excluded = Vec::new();
                for p in &o.properties {
                    let key = self.pattern_key(&p.key, p.computed, out)?;
                    let value = get_elem(self.local(src), key.0.clone());
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
            PropertyKey::StringLiteral(s) => s.value.to_string(),
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
                let (setup, place, sym) = self.place(simple)?;
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
                            let (setup, place, sym) = self.place(simple)?;
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
                let (setup, place, sym) = self.place(simple)?;
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
            Statement::ForOfStatement(f) => {
                if f.r#await {
                    return reject(f.span, "`for await` is not supported yet");
                }
                (&f.left, &f.right, &f.body, true)
            }
            Statement::ForInStatement(f) => (&f.left, &f.right, &f.body, false),
            _ => unreachable!(),
        };
        let mut out = Vec::new();
        // The iterated value (with `let` / `const` bindings in their TDZ).
        let v = self.eval_into(right, &mut out)?;
        let (iter, next) = if is_of {
            let (i, n) = self.open_iterator(v, &mut out);
            (i, Some(n))
        } else {
            let i = self.temp(Ty::Any);
            out.push(Stmt::Expr(set(i, Expr::new(Ty::Any, ExprKind::ForInIterator(Box::new(any(v)))))));
            (i, None)
        };
        let lp = self.lp(label, JumpKind::Loop, |cx, id| {
            let value = cx.temp(Ty::Any);
            let mut body = vec![
                Stmt::Expr(set(value, Expr::new(Ty::Any, ExprKind::IterStep(iter, next)))),
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
                cx.for_in_keys.push((sym, iter));
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

    // ---- expressions ----

    /// An expression used as a condition: converted with JS truthiness.
    fn cond(&mut self, e: &'a Expression<'a>) -> R<Expr> {
        let x = self.expr(e)?;
        Ok(truthy(x))
    }

    fn expr(&mut self, e: &'a Expression<'a>) -> R<Expr> {
        let span = e.span();
        // Only the expression right after a binding is named by it.
        let name_hint = self.name_hint.take();
        Ok(match e {
            Expression::ArrowFunctionExpression(a) => self.closure(FnNode::Arrow(a), name_hint.unwrap_or_default())?,
            Expression::FunctionExpression(f) => {
                let name = f.id.as_ref().map(|id| id.name.to_string()).or(name_hint).unwrap_or_default();
                self.closure(FnNode::Function(f), name)?
            }
            Expression::NumericLiteral(n) => Expr::num(n.value),
            Expression::ThisExpression(_) if self.has_this => Expr::new(Ty::Any, ExprKind::This),
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
                let id = self.m.string(s.value.as_str());
                Expr::new(Ty::Any, ExprKind::Str(id))
            }
            Expression::NullLiteral(_) => Expr::new(Ty::Any, ExprKind::Null),
            Expression::TemplateLiteral(t) => self.template(t)?,
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression)?,
            Expression::Identifier(id) => self.ident(id)?,
            Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_)
                if let Some((home, key)) = self.super_key(e.as_member_expression().unwrap())? =>
            {
                Expr::new(Ty::Any, ExprKind::SuperGet(home, Box::new(key)))
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
                    let obj = any(self.expr(&m.object)?);
                    let atom = self.m.atom(m.property.name.as_str());
                    if m.property.name == "length" {
                        Expr::new(Ty::Any, ExprKind::Length(Box::new(obj), atom))
                    } else {
                        Expr::new(Ty::Any, ExprKind::GetProp(Box::new(obj), atom))
                    }
                }
            },
            Expression::ComputedMemberExpression(m) => {
                if m.optional {
                    return reject(span, "optional chaining is not supported yet");
                }
                if let Expression::Identifier(id) = m.expression.without_parentheses()
                    && let Some(sym) = self.resolve(id)
                    && let Some(&(_, iter)) = self.for_in_keys.iter().rev().find(|(s, _)| *s == sym)
                {
                    let mut obj = [any(self.expr(&m.object)?)];
                    let mark = self.pre.len();
                    let key = any(self.ident(id)?);
                    self.order(&mut obj, mark);
                    let [obj] = obj;
                    return Ok(Expr::new(Ty::Any, ExprKind::ForInGet(iter, Box::new(obj), Box::new(key))));
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

    fn object(&mut self, o: &'a ObjectExpression<'a>) -> R<Expr> {
        // Parts: a field (its atom), a computed key, a spread; `values` are
        // all the operands, in evaluation order.
        enum Part {
            Field(ir::AtomId),
            Computed,
            Spread,
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
            if p.kind != PropertyKind::Init || p.method {
                return reject(p.span, "methods and accessors in object literals are not supported yet");
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
                PropertyKey::StringLiteral(s) => s.value.to_string(),
                PropertyKey::NumericLiteral(n) if n.value.fract() == 0.0 && n.value >= 0.0 => {
                    format!("{}", n.value as u64)
                }
                _ => return reject(p.span, "unsupported object literal key"),
            };
            if name == "__proto__" {
                return reject(p.span, "`__proto__` in object literals is not supported");
            }
            let mark = self.pre.len();
            let v = any(self.expr(&p.value)?);
            push(self, v, &mut values, mark);
            parts.push(Part::Field(self.m.atom(&name)));
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
            })
            .collect();
        Ok(Expr::new(Ty::Any, ExprKind::ObjectParts(parts)))
    }

    /// A property key: an atom for constant names.
    fn key(&mut self, e: &'a Expression<'a>) -> R<Key> {
        Ok(match e.without_parentheses() {
            Expression::StringLiteral(s) => Key::Atom(self.m.atom(s.value.as_str())),
            other => Key::Value(Box::new(any(self.expr(other)?))),
        })
    }

    /// A key of element access: numbers stay unboxed.
    fn elem_key(&mut self, e: &'a Expression<'a>) -> R<Key> {
        Ok(match e.without_parentheses() {
            Expression::StringLiteral(s) => Key::Atom(self.m.atom(s.value.as_str())),
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

    fn resolve(&self, id: &'a IdentifierReference<'a>) -> Option<SymbolId> {
        let scoping = self.m.scoping;
        id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id())
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
                    return Ok(Expr::new(Ty::Any, ExprKind::Cell(cell)));
                }
                reject(id.span, format!("`{}` is not a local variable, module binding or global", id.name))
            }
            None => Ok(match id.name.as_str() {
                "NaN" => Expr::num(f64::NAN),
                "Infinity" => Expr::num(f64::INFINITY),
                "undefined" => undefined(),
                "arguments" => return reject(id.span, "`arguments` is not supported"),
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
    fn place(&mut self, t: &'a SimpleAssignmentTarget<'a>) -> R<(Vec<Expr>, Place, Option<SymbolId>)> {
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
                let obj = any(self.expr(&m.object)?);
                let t = self.temp(Ty::Any);
                (vec![set(t, obj)], Place::Prop(t, atom), None)
            }
            SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                // Typed arrays keep their type.
                let direct = self.plain_local(&m.object);
                let obj = self.expr(&m.object)?;
                let obj = if matches!(obj.ty, Ty::Arr(_)) { obj } else { any(obj) };
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
        let Expression::Identifier(id) = e.without_parentheses() else { return None };
        let &l = self.sym_local.get(&self.resolve(id)?)?;
        (!self.boxed[l] && matches!(self.locals[l], Ty::Any | Ty::Arr(_))).then_some(l)
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
                        let obj = any(self.expr(&m.object)?);
                        let t = self.temp(Ty::Any);
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
            Place::Prop(o, a) => Expr::new(Ty::Any, ExprKind::GetProp(Box::new(any(self.local(o))), a)),
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
            Place::Prop(o, a) => {
                Expr::new(Ty::Any, ExprKind::SetProp(Box::new(any(self.local(o))), a, Box::new(any(v))))
            }
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
        let (mut setup, place, sym) = self.place(target)?;
        let mark = self.pre.len();
        let value = if a.operator == AssignmentOperator::Assign {
            let v = self.expr(&a.right)?;
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

    /// `x++` and friends; `used`: whether the value of the expression is.
    fn update(&mut self, u: &'a UpdateExpression<'a>, used: bool) -> R<Expr> {
        let (mut setup, place, sym) = self.place(&u.argument)?;
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
        {
            // `typeof undeclared` is "undefined", not a ReferenceError.
            let atom = self.m.atom(id.name.as_str());
            let g = Expr::new(Ty::Any, ExprKind::GlobalOrUndefined(atom));
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
            (UnaryOperator::Typeof, _) => dynamic(AnyUnOp::Typeof, x),
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
        match c.callee.without_parentheses() {
            callee @ (Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_))
                if let Some((home, key)) = self.super_key(callee.as_member_expression().unwrap())? =>
            {
                let this = Expr::new(Ty::Any, ExprKind::This);
                let get = |_: &mut Self, _| Ok(Expr::new(Ty::Any, ExprKind::SuperGet(home, Box::new(key))));
                self.call_this(this, get, &c.arguments)
            }
            Expression::PrivateFieldExpression(p) => {
                let obj = self.expr(&p.object)?;
                let name = format!("#{}", p.field.name);
                let span = p.span;
                self.call_this(obj, |cx, o| cx.private_read(o, &name, span), &c.arguments)
            }
            Expression::Identifier(id) => {
                let sym = self.resolve(id);
                if let Some(i) = sym.and_then(|s| self.m.cands.iter().position(|c| c.symbol == Some(s)))
                    && !self.sym_local.contains_key(&sym.unwrap())
                    && let Some(e) = self.direct_call(i, &c.arguments, c.span)?
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
                let obj = any(self.expr(&m.object)?);
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
    fn direct_call(&mut self, i: usize, args: &'a [Argument<'a>], span: Span) -> R<Option<Expr>> {
        // Async functions and generators are called through their JS entry
        // (which returns a promise or a generator object).
        let node = self.m.cands[i].node;
        if !self.m.cands[i].direct_callable || node.is_async() || node.is_generator() {
            return Ok(None);
        }
        let (params, ret) = self.m.signature(i, span)?;
        let rest = self.m.cands[i].rest;
        let fixed = params.len() - rest as usize;
        if (!rest && args.len() > params.len()) || args.iter().any(|a| a.as_expression().is_none()) {
            return Ok(None);
        }
        let start = self.pre.len();
        let mut out = Vec::with_capacity(params.len());
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
            return Ok(Some(Expr::new(ret, ExprKind::Call(i, out))));
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
                // An unproven argument for an unboxed parameter: the caller
                // checks the arguments again.
                _ => {
                    self.pre.truncate(start);
                    return Ok(None);
                }
            });
        }
        Ok(Some(Expr::new(ret, ExprKind::Call(i, out))))
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
                    !matches!(
                        d.init.as_ref().map(|i| i.without_parentheses()),
                        Some(Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_))
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
    let span = Span::new(stmts[0].span().start, stmts[stmts.len() - 1].span().end);
    let mut cells = HashSet::new();
    for (&sym, at) in uses {
        let declared_here = scoping.symbol_span(sym).start >= span.start && scoping.symbol_span(sym).end <= span.end;
        if declared_here && at.iter().any(|&p| p < span.start || p >= span.end) {
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
    }
}
