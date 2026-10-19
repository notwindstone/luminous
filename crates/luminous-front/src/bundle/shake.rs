//! Tree shaking of a library bundle (`Embedding::library`), as Bun's and
//! esbuild's: the top-level statements that run code are kept (with their
//! module), and the declarations that what is kept uses, across modules
//! through imports; a declaration nothing uses is left out, and so is a
//! module left with nothing. A module whose package says it has no side
//! effects runs only if something of it is used.
//!
//! Purity is decided conservatively: declarations of functions, of classes
//! whose definition runs no code, of variables whose initializers are
//! literals, functions, identifiers, object and array literals of those,
//! and calls marked `/* @__PURE__ */` (or of a few builtins that have no
//! effects: `Symbol()`, `new Map()`, `Object.freeze({...})`).

use std::collections::HashMap;

use oxc::ast::ast::*;
use oxc::semantic::SymbolId;
use oxc::span::GetSpan;

use super::{Binding, DynTo, Target, Unit, export_names, member_of_namespace, resolve_export};

/// What tree shaking keeps: the modules, and per module the top-level
/// statements left out.
pub(super) struct Shaken {
    pub keep: Vec<bool>,
    pub removed: Vec<Vec<bool>>,
}

/// Globals that reading cannot throw or run code.
const KNOWN_GLOBALS: &[&str] = &[
    "undefined", "NaN", "Infinity", "globalThis", "Object", "Function", "Array", "Number", "String", "Boolean",
    "Symbol", "BigInt", "Math", "JSON", "Reflect", "Proxy", "Promise", "Map", "Set", "WeakMap", "WeakSet",
    "WeakRef", "Date", "RegExp", "Error", "TypeError", "RangeError", "SyntaxError", "ReferenceError", "EvalError",
    "URIError", "AggregateError", "ArrayBuffer", "SharedArrayBuffer", "DataView", "Uint8Array", "Int8Array",
    "Uint8ClampedArray", "Uint16Array", "Int16Array", "Uint32Array", "Int32Array", "Float32Array", "Float64Array",
    "BigInt64Array", "BigUint64Array", "Intl", "Atomics", "parseInt", "parseFloat", "isNaN", "isFinite",
    "encodeURIComponent", "decodeURIComponent", "encodeURI", "decodeURI", "TextEncoder", "TextDecoder", "URL",
    "URLSearchParams", "console",
];

struct Purity<'u, 'a> {
    u: &'u Unit<'a>,
}

impl Purity<'_, '_> {
    /// Whether `id` names a global (no binding of the module declares it).
    fn global(&self, id: &IdentifierReference) -> bool {
        let scoping = self.u.sem.scoping();
        id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id()).is_none()
    }

    fn ident(&self, id: &IdentifierReference) -> bool {
        !self.global(id) || KNOWN_GLOBALS.contains(&id.name.as_str())
    }

    fn literal(e: &Expression) -> bool {
        matches!(
            e.without_parentheses(),
            Expression::BooleanLiteral(_)
                | Expression::NullLiteral(_)
                | Expression::NumericLiteral(_)
                | Expression::BigIntLiteral(_)
                | Expression::StringLiteral(_)
        ) || matches!(e.without_parentheses(), Expression::TemplateLiteral(t) if t.expressions.is_empty())
    }

    fn key(&self, k: &PropertyKey, computed: bool) -> bool {
        if !computed {
            return true;
        }
        match k.as_expression() {
            Some(e) => Self::literal(e),
            None => true,
        }
    }

    /// `Symbol(...)`, `Symbol.for(...)`, `Object.freeze(...)`,
    /// `Object.create(...)`: calls of builtins that run no code of the
    /// program (with arguments that run none).
    fn known_call(&self, callee: &Expression) -> bool {
        match callee.without_parentheses() {
            Expression::Identifier(id) => self.global(id) && id.name == "Symbol",
            Expression::StaticMemberExpression(m) => match &m.object {
                Expression::Identifier(id) if self.global(id) => {
                    matches!((id.name.as_str(), m.property.name.as_str()), ("Symbol", "for") | ("Object", "freeze") | ("Object", "create"))
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn known_new(&self, callee: &Expression) -> bool {
        match callee.without_parentheses() {
            Expression::Identifier(id) => {
                self.global(id) && matches!(id.name.as_str(), "Map" | "Set" | "WeakMap" | "WeakSet" | "WeakRef" | "Object" | "Array")
            }
            _ => false,
        }
    }

    fn args(&self, args: &[Argument]) -> bool {
        args.iter().all(|a| match a {
            Argument::SpreadElement(_) => false,
            a => a.as_expression().is_some_and(|e| self.expr(e)),
        })
    }

    fn expr(&self, e: &Expression) -> bool {
        match e {
            Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::RegExpLiteral(_)
            | Expression::FunctionExpression(_)
            | Expression::ArrowFunctionExpression(_) => true,
            Expression::TemplateLiteral(t) => t.expressions.iter().all(Self::literal),
            Expression::Identifier(id) => self.ident(id),
            Expression::ClassExpression(c) => self.class(c),
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            Expression::ObjectExpression(o) => o.properties.iter().all(|p| match p {
                ObjectPropertyKind::ObjectProperty(p) => self.key(&p.key, p.computed) && self.expr(&p.value),
                ObjectPropertyKind::SpreadProperty(_) => false,
            }),
            Expression::ArrayExpression(a) => a.elements.iter().all(|el| match el {
                ArrayExpressionElement::SpreadElement(_) => false,
                ArrayExpressionElement::Elision(_) => true,
                el => el.as_expression().is_some_and(|e| self.expr(e)),
            }),
            Expression::UnaryExpression(u) => match u.operator {
                UnaryOperator::Void | UnaryOperator::LogicalNot => self.expr(&u.argument),
                UnaryOperator::Typeof => match &u.argument {
                    Expression::Identifier(_) => true,
                    a => self.expr(a),
                },
                UnaryOperator::UnaryNegation | UnaryOperator::UnaryPlus | UnaryOperator::BitwiseNot => Self::literal(&u.argument),
                UnaryOperator::Delete => false,
            },
            Expression::BinaryExpression(b) => match b.operator {
                BinaryOperator::StrictEquality | BinaryOperator::StrictInequality => self.expr(&b.left) && self.expr(&b.right),
                BinaryOperator::In | BinaryOperator::Instanceof => false,
                _ => Self::literal(&b.left) && Self::literal(&b.right),
            },
            Expression::LogicalExpression(l) => self.expr(&l.left) && self.expr(&l.right),
            Expression::ConditionalExpression(c) => self.expr(&c.test) && self.expr(&c.consequent) && self.expr(&c.alternate),
            Expression::SequenceExpression(s) => s.expressions.iter().all(|e| self.expr(e)),
            Expression::TSAsExpression(t) => self.expr(&t.expression),
            Expression::TSSatisfiesExpression(t) => self.expr(&t.expression),
            Expression::TSNonNullExpression(t) => self.expr(&t.expression),
            Expression::TSTypeAssertion(t) => self.expr(&t.expression),
            Expression::TSInstantiationExpression(t) => self.expr(&t.expression),
            Expression::CallExpression(c) => (c.pure || self.known_call(&c.callee)) && self.args(&c.arguments),
            Expression::NewExpression(n) => (n.pure || self.known_new(&n.callee)) && self.args(&n.arguments),
            _ => false,
        }
    }

    /// Whether defining class `c` runs no code: its superclass, computed
    /// keys and static initializers are pure, and it has no static blocks
    /// or decorators.
    fn class(&self, c: &Class) -> bool {
        if !c.decorators.is_empty() {
            return false;
        }
        if let Some(h) = &c.heritage
            && !self.expr(&h.expression)
        {
            return false;
        }
        c.body.body.iter().all(|el| match el {
            ClassElement::StaticBlock(_) => false,
            ClassElement::MethodDefinition(m) => m.decorators.is_empty() && self.key(&m.key, m.computed),
            ClassElement::PropertyDefinition(p) => {
                p.decorators.is_empty()
                    && self.key(&p.key, p.computed)
                    && (!p.r#static || p.value.as_ref().is_none_or(|v| self.expr(v)))
            }
            ClassElement::AccessorProperty(p) => {
                p.decorators.is_empty()
                    && self.key(&p.key, p.computed)
                    && (!p.r#static || p.value.as_ref().is_none_or(|v| self.expr(v)))
            }
            ClassElement::TSIndexSignature(_) => true,
        })
    }

    fn variables(&self, v: &VariableDeclaration) -> bool {
        !matches!(v.kind, VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing)
            && v.declarations.iter().all(|d| {
                matches!(d.id, BindingPattern::BindingIdentifier(_)) && d.init.as_ref().is_none_or(|e| self.expr(e))
            })
    }

    fn declaration(&self, d: &Declaration) -> bool {
        match d {
            Declaration::FunctionDeclaration(_) => true,
            Declaration::ClassDeclaration(c) => self.class(c),
            Declaration::VariableDeclaration(v) => self.variables(v),
            Declaration::TSTypeAliasDeclaration(_) | Declaration::TSInterfaceDeclaration(_) => true,
            Declaration::TSEnumDeclaration(e) => !e.declare && e.body.members.iter().all(|m| m.initializer.as_ref().is_none_or(Self::literal)),
            // (A namespace's body runs.)
            Declaration::TSNamespaceDeclaration(m) => m.declare,
            Declaration::TSExternalModuleDeclaration(m) => m.declare,
            Declaration::TSGlobalDeclaration(_) => true,
            Declaration::TSImportEqualsDeclaration(_) => false,
        }
    }

    /// How a top-level statement takes part: `Some(true)` a declaration
    /// that runs no code (removable), `Some(false)` code that runs,
    /// `None` module syntax (imports and re-exports, kept as they are).
    fn statement(&self, s: &Statement) -> Option<bool> {
        Some(match s {
            Statement::ImportDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_) => return None,
            Statement::ExportDeclaration(e) => self.declaration(&e.declaration),
            Statement::ExportDefaultDeclaration(e) => match &e.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(_) => true,
                ExportDefaultDeclarationKind::ClassDeclaration(c) => self.class(c),
                ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => true,
                d => d.as_expression().is_some_and(|e| self.expr(e)),
            },
            Statement::EmptyStatement(_) => true,
            Statement::FunctionDeclaration(_) => true,
            Statement::ClassDeclaration(c) => self.class(c),
            Statement::VariableDeclaration(v) => self.variables(v),
            Statement::TSTypeAliasDeclaration(_) | Statement::TSInterfaceDeclaration(_) => true,
            Statement::TSEnumDeclaration(e) => !e.declare && e.body.members.iter().all(|m| m.initializer.as_ref().is_none_or(Self::literal)),
            Statement::TSNamespaceDeclaration(m) => m.declare,
            Statement::TSExternalModuleDeclaration(m) => m.declare,
            Statement::TSGlobalDeclaration(_) => true,
            _ => false,
        })
    }
}

/// The index of the top-level statement of `u` that span position `pos`
/// is in.
fn stmt_at(u: &Unit, pos: u32) -> Option<usize> {
    let body = &u.program.body;
    let i = body.partition_point(|s| s.span().end <= pos);
    (i < body.len() && body[i].span().start <= pos).then_some(i)
}

/// Shake the modules running in `order` (and `lazy`, which are kept whole),
/// with what the library exports (`exported`).
pub(super) fn shake(
    units: &[Unit],
    order: &[usize],
    lazy: &[usize],
    resolved: &HashMap<(usize, SymbolId), Binding>,
    exported: &[(String, Binding)],
) -> Shaken {
    let n = units.len();
    // Per module: how each statement takes part, the statement declaring
    // each top-level binding, and the references in each statement (to
    // top-level bindings, imports among them).
    let mut kinds: Vec<Vec<Option<bool>>> = vec![Vec::new(); n];
    let mut decl: Vec<HashMap<SymbolId, usize>> = vec![HashMap::new(); n];
    let mut refs: Vec<Vec<Vec<(SymbolId, oxc::semantic::NodeId)>>> = vec![Vec::new(); n];
    let mut default_stmt: Vec<Option<usize>> = vec![None; n];
    for &m in order.iter().chain(lazy) {
        let u = &units[m];
        let p = Purity { u };
        kinds[m] = u.program.body.iter().map(|s| p.statement(s)).collect();
        refs[m] = vec![Vec::new(); u.program.body.len()];
        let scoping = u.sem.scoping();
        for (_, &sym) in scoping.get_bindings(u.root()).iter() {
            if let Some(i) = stmt_at(u, scoping.symbol_span(sym).start) {
                decl[m].insert(sym, i);
            }
            for r in scoping.get_resolved_references(sym) {
                if !r.flags().is_value() && !r.flags().is_write() {
                    continue;
                }
                let at = u.sem.nodes().get_node(r.node_id()).kind().span().start;
                if let Some(i) = stmt_at(u, at) {
                    refs[m][i].push((sym, r.node_id()));
                }
            }
        }
        default_stmt[m] = u.default_expr.and_then(|d| stmt_at(u, d.prefix.start));
    }

    struct State<'s, 'a> {
        kinds: &'s [Vec<Option<bool>>],
        units: &'s [Unit<'a>],
        keep: Vec<bool>,
        live: Vec<Vec<bool>>,
        work: Vec<(usize, usize)>,
        materialized: Vec<bool>,
    }
    impl State<'_, '_> {
        fn stmt(&mut self, m: usize, i: usize) {
            if !self.live[m][i] {
                self.live[m][i] = true;
                self.work.push((m, i));
            }
        }
        /// Module `m` runs: its code that runs is kept, and so are the
        /// modules it imports that may have side effects (as esbuild's: a
        /// module only modules left out import is left out).
        fn include(&mut self, m: usize) {
            if self.keep[m] {
                return;
            }
            self.keep[m] = true;
            for i in 0..self.kinds[m].len() {
                if self.kinds[m][i] == Some(false) {
                    self.stmt(m, i);
                }
            }
            let units = self.units;
            for (t, runtime) in &units[m].deps {
                if let (Target::Module(d), true) = (t, runtime)
                    && !units[*d].pure
                {
                    self.include(*d);
                }
            }
        }
    }
    let mut st = State {
        kinds: &kinds,
        units,
        keep: vec![false; n],
        live: kinds.iter().map(|k| vec![false; k.len()]).collect(),
        work: Vec::new(),
        materialized: vec![false; n],
    };

    fn binding(st: &mut State<'_, '_>, units: &[Unit], decl: &[HashMap<SymbolId, usize>], default_stmt: &[Option<usize>], b: Binding) {
        match b {
            Binding::Symbol(k, sym) => {
                st.include(k);
                if let Some(&i) = decl[k].get(&sym) {
                    st.stmt(k, i);
                }
            }
            Binding::Default(k) => {
                st.include(k);
                if let Some(i) = default_stmt[k] {
                    st.stmt(k, i);
                }
            }
            Binding::Namespace(t) => {
                st.include(t);
                if st.materialized[t] {
                    return;
                }
                st.materialized[t] = true;
                for name in export_names(units, t) {
                    if let Some(b) = resolve_export(units, t, &name, &mut Vec::new()) {
                        binding(st, units, decl, default_stmt, b);
                    }
                }
            }
        }
    }

    st.include(0);
    for &m in lazy {
        st.include(m);
        for i in 0..kinds[m].len() {
            st.stmt(m, i);
        }
    }
    for (_, b) in exported {
        binding(&mut st, units, &decl, &default_stmt, *b);
    }
    while let Some((m, i)) = st.work.pop() {
        let u = &units[m];
        // (`import()`s of modules of the bundle: their exports.)
        let span = u.program.body[i].span();
        for d in &u.dynamic {
            if let DynTo::Module(t) = d.to
                && span.start <= d.span.start
                && d.span.end <= span.end
            {
                binding(&mut st, units, &decl, &default_stmt, Binding::Namespace(t));
            }
        }
        for &(sym, node) in &refs[m][i] {
            match resolved.get(&(m, sym)) {
                Some(&Binding::Namespace(t)) => match member_of_namespace(units, u.sem.nodes(), node, t) {
                    Some((_, b)) => binding(&mut st, units, &decl, &default_stmt, b),
                    None => binding(&mut st, units, &decl, &default_stmt, Binding::Namespace(t)),
                },
                Some(&b) => binding(&mut st, units, &decl, &default_stmt, b),
                None => {
                    if let Some(&d) = decl[m].get(&sym) {
                        st.stmt(m, d);
                    }
                }
            }
        }
    }
    let removed = (0..n)
        .map(|m| (0..kinds[m].len()).map(|i| st.keep[m] && kinds[m][i] == Some(true) && !st.live[m][i]).collect())
        .collect();
    Shaken { keep: st.keep, removed }
}
