//! Programs of several modules: the module graph of an entry, resolved and
//! merged into one module (scope hoisting, as bundlers do), which the
//! single-module compiler then sees whole: calls across modules are direct
//! calls, classes of other modules have their layouts, and so on.
//!
//! Each module's text is copied as is but for edits that keep its lines: its
//! `import` declarations and `export` syntax are removed, a top-level name
//! that another module (or a global another module uses) also has is
//! renamed, and the references to an imported binding become references to
//! the binding it resolves to. Modules come in the order ESM evaluates them
//! (dependencies first, in the order of their `import`s; a module of a cycle
//! that is being evaluated is skipped), so their code runs in that order, and
//! imported bindings are live (they are the exporting module's own).
//! Imports used only as types are elided, as TypeScript does: such a module
//! contributes its type declarations only. Imports of modules the engine
//! provides (`qjs:os`, ...) stay.
//!
//! Writes to an import binding throw a TypeError (through an accessor), as
//! `ns.x = v` does on a namespace object (a Proxy with the semantics of a
//! module namespace). Modules of JSON, text or bytes (`with { type: ... }`)
//! export their content as `default`.
//!
//! A program of one module is left untouched.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use oxc::allocator::Allocator;
use oxc::ast::AstKind;
use oxc::ast::ast::*;
use oxc::ast_visit::{Visit, walk};
use oxc::diagnostics::OxcDiagnostic;
use oxc::parser::Parser;
use oxc::semantic::{NodeId, ScopeId, Semantic, SemanticBuilder, SymbolId};
use oxc::span::{GetSpan, SourceType, Span};
use oxc::str::Ident;

use crate::marker;

/// One module's part of a bundle.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    pub path: PathBuf,
    /// Its text as written.
    pub original: String,
    /// Its bytes in the bundle.
    pub start: u32,
    pub end: u32,
    /// Its first line in the bundle (1-based).
    pub first_line: u32,
    /// (offset in the bundle, offset in `original`) where each edit ends:
    /// the text after it, to the next edit, is copied.
    map: Vec<(u32, u32)>,
}

impl ModuleSource {
    /// The offset in `original` of offset `pos` of the bundle (in an edit:
    /// where the edit ends).
    pub fn original_offset(&self, pos: u32) -> u32 {
        let i = self.map.partition_point(|&(b, _)| b <= pos);
        let (b, o) = if i == 0 { (self.start, 0) } else { self.map[i - 1] };
        (o + pos.saturating_sub(b)).min(self.original.len() as u32)
    }
}

/// A program's modules merged into one source.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub source: String,
    /// In the order of the source.
    pub modules: Vec<ModuleSource>,
}

impl Bundle {
    /// The module that offset `pos` of the bundle comes from, if any (the
    /// bundle ends with definitions of its own).
    pub fn module_at(&self, pos: u32) -> Option<&ModuleSource> {
        self.modules.iter().find(|m| m.start <= pos && pos <= m.end)
    }

    /// The file and line that line `line` (1-based) of the bundle comes from.
    pub fn line(&self, line: u32) -> (&Path, u32) {
        let m = self.modules.iter().rev().find(|m| m.first_line <= line).unwrap_or(&self.modules[0]);
        (&m.path, line - m.first_line + 1)
    }

    /// Whether this is one module's text as written.
    pub fn is_single(&self) -> bool {
        self.modules.len() == 1 && self.modules[0].map.is_empty()
    }
}

/// What an import specifier names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Name {
    Named(String),
    Namespace,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Target {
    Module(usize),
    /// A module the engine provides: its imports stay.
    External(String),
}

#[derive(Debug, Clone)]
enum Export {
    /// A top-level binding of the module (possibly an import binding).
    Local(SymbolId),
    /// `export default <expression>` (or an anonymous function or class).
    DefaultExpr,
    /// `export { a as b } from`, `export * as ns from`.
    From(Target, Name),
}

/// What a name a module exports resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Binding {
    Symbol(usize, SymbolId),
    Default(usize),
    Namespace(usize),
}

struct Import {
    span: Span,
    target: Target,
    /// Local binding and what it imports.
    bindings: Vec<(SymbolId, Name)>,
}

/// What a module's file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Script,
    Json,
    Text,
    Bytes,
}

struct Unit<'a> {
    path: PathBuf,
    original: String,
    program: &'a Program<'a>,
    sem: Semantic<'a>,
    imports: Vec<Import>,
    exports: Vec<(String, Export)>,
    stars: Vec<Target>,
    /// Dependencies, in source order, and whether each is needed at run
    /// time (not only for types).
    deps: Vec<(Target, bool)>,
    /// `export ...` syntax to remove (not the declarations it exports).
    deletions: Vec<Span>,
    /// `export default <expression>` or an anonymous function or class.
    default_expr: Option<DefaultExport>,
    /// Why the module cannot be part of a program of several modules.
    unsupported: Option<String>,
    /// `export { a } from`: where, the module and the name it re-exports
    /// (which must resolve, used or not).
    reexports: Vec<(Span, usize, String)>,
    /// Its early errors (parse and semantic).
    errors: Vec<OxcDiagnostic>,
    /// `import(...)` expressions: where, and the module of a string
    /// specifier that resolves (else it stays the engine's).
    dynamic: Vec<(Span, Option<Target>)>,
    /// Where `import.meta` is.
    metas: Vec<Span>,
    /// Whether its code awaits at the top level.
    tla: bool,
}

impl Unit<'_> {
    fn root(&self) -> ScopeId {
        self.sem.scoping().root_scope_id()
    }

    fn import_of(&self, sym: SymbolId) -> Option<(&Import, &Name)> {
        self.imports.iter().find_map(|i| i.bindings.iter().find(|b| b.0 == sym).map(|b| (i, &b.1)))
    }

    fn internal_import(&self, sym: SymbolId) -> Option<(usize, &Name)> {
        match self.import_of(sym)? {
            (Import { target: Target::Module(m), .. }, name) => Some((*m, name)),
            _ => None,
        }
    }

    /// Whether `sym` is referenced, and only in type positions (an import
    /// of it is elided, as TypeScript does).
    fn types_only(&self, sym: SymbolId) -> bool {
        let mut refs = self.sem.scoping().get_resolved_references(sym).peekable();
        refs.peek().is_some() && refs.all(|r| !r.flags().is_value())
    }

    /// Whether a binding named `name` in a scope from `scope` up to (not
    /// including) the module scope would capture a reference there.
    fn shadowed(&self, scope: ScopeId, name: &str) -> bool {
        let scoping = self.sem.scoping();
        let root = self.root();
        scoping.scope_ancestors(scope).take_while(|&s| s != root).any(|s| scoping.get_binding(s, Ident::from(name)).is_some())
    }

    fn ref_scope(&self, node: NodeId) -> ScopeId {
        self.sem.nodes().get_node(node).scope_id()
    }
}

/// `export default` of what has no name of its own, which gets one.
#[derive(Clone, Copy)]
struct DefaultExport {
    /// The `export default ` prefix.
    prefix: Span,
    /// Where the declaration or expression ends.
    end: u32,
    /// A function or class declaration: where its name goes (it stays a
    /// declaration: hoisted, compiled as one); else an expression for a
    /// `const`.
    name_at: Option<u32>,
    /// An anonymous function or class: its `name` is `default`.
    anonymous: Anon,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Anon {
    No,
    Function,
    Class,
}

enum Resolved {
    File(PathBuf),
    External(String),
}

/// Resolve a module specifier imported by `from`.
fn resolve(from: &Path, spec: &str) -> Result<Resolved, String> {
    if spec.starts_with("qjs:") || spec.starts_with("tsaot:") {
        return Ok(Resolved::External(spec.to_string()));
    }
    let dir = from.parent().unwrap_or(Path::new("."));
    let not_found = || format!("cannot find module `{spec}`");
    if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') || spec == "." || spec == ".." {
        return resolve_path(&dir.join(spec)).map(Resolved::File).ok_or_else(not_found);
    }
    // A package: `name[/sub]` or `@scope/name[/sub]`, from the nearest
    // `node_modules` that has it.
    let mut parts = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
    let pkg: Vec<&str> = parts.by_ref().take(if spec.starts_with('@') { 2 } else { 1 }).collect();
    let pkg = pkg.join("/");
    let sub = parts.next().unwrap_or("");
    for d in dir.ancestors() {
        let root = d.join("node_modules").join(&pkg);
        if root.is_dir() {
            return resolve_package(&root, sub).map(Resolved::File).ok_or_else(not_found);
        }
    }
    Err(not_found())
}

/// A file for a path, as TypeScript resolves it: as is, a `.js` path's
/// `.ts` source, with an extension added, or a directory's index.
fn resolve_path(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    let s = base.to_string_lossy();
    for (js, ts) in [(".js", ".ts"), (".mjs", ".mts")] {
        if let Some(stem) = s.strip_suffix(js) {
            let p = PathBuf::from(format!("{stem}{ts}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for ext in [".ts", ".mts", ".js", ".mjs"] {
        let p = PathBuf::from(format!("{s}{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    if base.is_dir() {
        for index in ["index.ts", "index.mts", "index.js", "index.mjs"] {
            let p = base.join(index);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// The conditions of `package.json` `exports` that apply, in no order (the
/// map's order decides).
const CONDITIONS: [&str; 3] = ["import", "module", "default"];

/// A file of the package at `root` for subpath `sub` (empty: the package):
/// through its `exports` if it has them, else its `module` or `main` field,
/// or the path itself.
fn resolve_package(root: &Path, sub: &str) -> Option<PathBuf> {
    let json: Option<serde_json::Value> =
        std::fs::read_to_string(root.join("package.json")).ok().and_then(|s| serde_json::from_str(&s).ok());
    if let Some(exports) = json.as_ref().and_then(|j| j.get("exports")) {
        let key = if sub.is_empty() { ".".to_string() } else { format!("./{sub}") };
        let target = exports_target(exports, &key)?;
        return resolve_path(&root.join(target.trim_start_matches("./")));
    }
    if !sub.is_empty() {
        return resolve_path(&root.join(sub));
    }
    for field in ["module", "main"] {
        if let Some(f) = json.as_ref().and_then(|j| j.get(field)).and_then(|f| f.as_str())
            && let Some(p) = resolve_path(&root.join(f))
        {
            return Some(p);
        }
    }
    resolve_path(&root.join("index"))
}

/// `exports` (a target, conditions, or subpaths) for subpath `key`.
fn exports_target(exports: &serde_json::Value, key: &str) -> Option<String> {
    use serde_json::Value;
    let subpaths = matches!(exports, Value::Object(m) if m.keys().all(|k| k.starts_with('.')));
    if !subpaths {
        return if key == "." { conditional(exports, None) } else { None };
    }
    let Value::Object(map) = exports else { return None };
    if let Some(v) = map.get(key) {
        return conditional(v, None);
    }
    // Patterns (`./*`, `./lib/*.js`): the longest prefix that matches.
    let mut best: Option<(&str, &Value, String)> = None;
    for (k, v) in map {
        if let Some((pre, post)) = k.split_once('*')
            && key.len() >= pre.len() + post.len()
            && key.starts_with(pre)
            && key.ends_with(post)
            && best.as_ref().is_none_or(|b| b.0.len() < pre.len())
        {
            best = Some((pre, v, key[pre.len()..key.len() - post.len()].to_string()));
        }
    }
    let (_, v, star) = best?;
    conditional(v, Some(&star))
}

/// A target of `exports`: a path, conditions (the first that applies, in
/// the map's order), or alternatives (the first that resolves).
fn conditional(v: &serde_json::Value, star: Option<&str>) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::String(s) => Some(match star {
            Some(m) => s.replace('*', m),
            None => s.clone(),
        }),
        Value::Object(m) => m.iter().filter(|(k, _)| CONDITIONS.contains(&k.as_str())).find_map(|(_, v)| conditional(v, star)),
        Value::Array(a) => a.iter().find_map(|v| conditional(v, star)),
        _ => None,
    }
}

/// The `export` (and `default`) keywords of the declaration at `start`
/// (not what follows: an `aot` marker stays).
fn keywords(text: &str, start: u32, default: bool) -> Span {
    let mut end = start as usize + "export".len();
    if default {
        let rest = &text[end..];
        end += rest.len() - rest.trim_start().len() + "default".len();
    }
    Span::new(start, end as u32)
}

fn loc(path: &Path, source: &str, pos: u32) -> String {
    let line = source[..pos as usize].matches('\n').count() + 1;
    format!("{}:{line}", path.display())
}

/// The module records of a module: its imports and exports.
/// The `type` import attribute of a `with { ... }` clause.
fn attr_type(w: &Option<oxc::allocator::Box<WithClause>>) -> Option<String> {
    w.as_ref()?
        .with_entries
        .iter()
        .find(|e| match &e.key {
            ImportAttributeKey::Identifier(i) => i.name == "type",
            ImportAttributeKey::StringLiteral(s) => s.value == "type",
        })
        .map(|e| e.value.value.to_string())
}

#[allow(clippy::type_complexity)]
fn records(
    unit: &mut Unit,
    program: &Program,
    resolve_target: &mut dyn FnMut(&str, Option<String>) -> Result<Target, String>,
) -> Result<(), String> {
    let scoping = unit.sem.scoping();
    let root = scoping.root_scope_id();
    let at = |span: Span| loc(&unit.path, &unit.original, span.start);
    let symbol_of = |id: &IdentifierReference| {
        id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id())
    };
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut stars = Vec::new();
    let mut deps = Vec::new();
    let mut deletions = Vec::new();
    let mut default_expr = None;
    let mut reexports = Vec::new();
    let mut target_of = |src: &StringLiteral, attr: Option<String>| {
        resolve_target(src.value.as_str(), attr).map_err(|e| format!("{}: {e}", at(src.span)))
    };
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(d) => {
                let target = target_of(&d.source, attr_type(&d.with_clause))?;
                let mut bindings = Vec::new();
                // (Whether each is a type only: `type`, or only used so.)
                let mut types = Vec::new();
                for s in d.specifiers.iter().flatten() {
                    let (local, name, ty) = match s {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => {
                            (&s.local, Name::Named(s.imported.name().to_string()), s.import_kind.is_type())
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => (&s.local, Name::Named("default".into()), false),
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => (&s.local, Name::Namespace, false),
                    };
                    types.push(ty || unit.types_only(local.symbol_id()));
                    bindings.push((local.symbol_id(), name));
                }
                // An import whose bindings are all types is elided (one with
                // none, or an unused binding, runs the module).
                let runtime = !d.import_kind.is_type() && (types.is_empty() || types.iter().any(|t| !t));
                deps.push((target.clone(), runtime));
                imports.push(Import { span: d.span, target, bindings });
            }
            Statement::ExportDeclaration(e) => {
                let decl = e.declaration.span();
                deletions.push(keywords(&unit.original, e.span.start, false));
                for (_, &sym) in scoping.get_bindings(root).iter() {
                    let s = scoping.symbol_span(sym);
                    if decl.start <= s.start && s.end <= decl.end {
                        exports.push((scoping.symbol_name(sym).to_string(), Export::Local(sym)));
                    }
                }
            }
            Statement::ExportNamedDeclaration(e) => {
                deletions.push(e.span);
                for s in &e.specifiers {
                    let ModuleExportName::IdentifierReference(id) = &s.local else {
                        return Err(format!("{}: unsupported export", at(s.span)));
                    };
                    let sym = symbol_of(id).ok_or_else(|| format!("{}: `{}` is not declared", at(id.span), id.name))?;
                    exports.push((s.exported.name().to_string(), Export::Local(sym)));
                }
            }
            Statement::ExportFromDeclaration(e) => {
                deletions.push(e.span);
                let target = target_of(&e.source, attr_type(&e.with_clause))?;
                if matches!(target, Target::External(_)) {
                    unit.unsupported.get_or_insert(format!("{}: re-exports of `{}` are not supported yet", at(e.span), e.source.value));
                }
                for s in &e.specifiers {
                    let name = s.local.name().to_string();
                    if let Target::Module(t) = target
                        && !e.export_kind.is_type()
                        && !s.export_kind.is_type()
                    {
                        reexports.push((s.span, t, name.clone()));
                    }
                    exports.push((s.exported.name().to_string(), Export::From(target.clone(), Name::Named(name))));
                }
                deps.push((target, !e.export_kind.is_type()));
            }
            Statement::ExportAllDeclaration(e) => {
                deletions.push(e.span);
                let target = target_of(&e.source, attr_type(&e.with_clause))?;
                if matches!(target, Target::External(_)) {
                    unit.unsupported.get_or_insert(format!("{}: re-exports of `{}` are not supported yet", at(e.span), e.source.value));
                }
                match &e.exported {
                    Some(name) => exports.push((name.name().to_string(), Export::From(target.clone(), Name::Namespace))),
                    None => stars.push(target.clone()),
                }
                deps.push((target, !e.export_kind.is_type()));
            }
            Statement::ExportDefaultDeclaration(e) => {
                let named = match &e.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => f.id.as_ref().map(|id| (f.span, id.symbol_id())),
                    ExportDefaultDeclarationKind::ClassDeclaration(c) => c.id.as_ref().map(|id| (c.span, id.symbol_id())),
                    ExportDefaultDeclarationKind::TSInterfaceDeclaration(i) => Some((i.span, i.id.symbol_id())),
                    _ => None,
                };
                match named {
                    Some((_, sym)) => {
                        deletions.push(keywords(&unit.original, e.span.start, true));
                        exports.push(("default".into(), Export::Local(sym)));
                    }
                    None => {
                        let decl = e.declaration.span();
                        let text = &unit.original;
                        // (An anonymous function or class, parenthesized or
                        // not, is named `default`.)
                        let (name_at, anonymous) = match &e.declaration {
                            ExportDefaultDeclarationKind::FunctionDeclaration(f) => (Some(f.params.span.start), Anon::Function),
                            ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                                let at = text[c.span.start as usize..].find("class").map(|i| c.span.start + i as u32 + 5);
                                (at, Anon::Class)
                            }
                            ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => (None, Anon::No),
                            other => match other.to_expression().without_parentheses() {
                                Expression::FunctionExpression(f) if f.id.is_none() => (None, Anon::Function),
                                Expression::ArrowFunctionExpression(_) => (None, Anon::Function),
                                Expression::ClassExpression(c) if c.id.is_none() => (None, Anon::Class),
                                _ => (None, Anon::No),
                            },
                        };
                        default_expr = Some(DefaultExport {
                            prefix: keywords(text, e.span.start, true),
                            end: decl.end,
                            name_at,
                            anonymous,
                        });
                        exports.push(("default".into(), Export::DefaultExpr));
                    }
                }
            }
            Statement::TSExportAssignment(e) => {
                unit.unsupported.get_or_insert(format!("{}: `export =` is not supported", at(e.span)));
            }
            Statement::TSImportEqualsDeclaration(e) => {
                unit.unsupported.get_or_insert(format!("{}: `import ... = require(...)` is not supported", at(e.span)));
            }
            _ => {}
        }
    }
    let mut dynamic = Dynamic::default();
    dynamic.visit_program(program);
    unit.dynamic = dynamic
        .imports
        .into_iter()
        .map(|(span, spec)| {
            // (One that does not resolve rejects when it runs, as the
            // engine's does.)
            let t = spec.and_then(|(s, ty)| resolve_target(&s, ty).ok()).filter(|t| matches!(t, Target::Module(_)));
            (span, t)
        })
        .collect();
    unit.metas = dynamic.metas;
    unit.tla = dynamic.tla;
    // An export name declared twice is an early error.
    let mut seen = HashSet::new();
    for (name, _) in &exports {
        if !seen.insert(name.as_str()) {
            return Err(format!("{}: duplicate export `{name}`", unit.path.display()));
        }
    }
    unit.imports = imports;
    unit.exports = exports;
    unit.stars = stars;
    unit.deps = deps;
    unit.deletions = deletions;
    unit.default_expr = default_expr;
    unit.reexports = reexports;
    Ok(())
}

/// A module's `import(...)` expressions (string specifiers, and a `type`
/// in `{ with: { type } }`), `import.meta`, and whether it awaits at the
/// top level.
#[derive(Default)]
struct Dynamic {
    imports: Vec<(Span, Option<(String, Option<String>)>)>,
    metas: Vec<Span>,
    tla: bool,
    depth: u32,
}

impl<'a> Visit<'a> for Dynamic {
    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        let spec = match &it.source {
            Expression::StringLiteral(s) => Some(s.value.to_string()),
            Expression::TemplateLiteral(t) if t.expressions.is_empty() => {
                t.quasis.first().and_then(|q| q.value.cooked.map(|c| c.to_string()))
            }
            _ => None,
        };
        let ty = it.options.as_ref().and_then(|o| {
            let Expression::ObjectExpression(o) = o.without_parentheses() else { return None };
            o.properties.iter().find_map(|p| {
                let ObjectPropertyKind::ObjectProperty(p) = p else { return None };
                if p.key.static_name().as_deref() != Some("with") {
                    return None;
                }
                let Expression::ObjectExpression(w) = p.value.without_parentheses() else { return None };
                w.properties.iter().find_map(|q| {
                    let ObjectPropertyKind::ObjectProperty(q) = q else { return None };
                    match (&q.key.static_name().as_deref(), &q.value) {
                        (Some("type"), Expression::StringLiteral(v)) => Some(v.value.to_string()),
                        _ => None,
                    }
                })
            })
        });
        self.imports.push((it.span, spec.map(|s| (s, ty))));
        walk::walk_import_expression(self, it);
    }

    fn visit_import_meta(&mut self, it: &ImportMeta) {
        self.metas.push(it.span);
    }

    fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
        if self.depth == 0 {
            self.tla = true;
        }
        walk::walk_await_expression(self, it);
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        if it.r#await && self.depth == 0 {
            self.tla = true;
        }
        walk::walk_for_of_statement(self, it);
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: oxc::syntax::scope::ScopeFlags) {
        self.depth += 1;
        walk::walk_function(self, it, flags);
        self.depth -= 1;
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.depth += 1;
        walk::walk_arrow_function_expression(self, it);
        self.depth -= 1;
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        // (Field initializers and static blocks are functions of their own.)
        self.depth += 1;
        walk::walk_class(self, it);
        self.depth -= 1;
    }
}

/// The source of a module of JSON, text or bytes: its content as a default
/// export.
fn synthesize(path: &Path, kind: Kind) -> Result<String, String> {
    let read_err = |e: std::io::Error| format!("reading {}: {e}", path.display());
    Ok(match kind {
        Kind::Script => unreachable!(),
        Kind::Json => {
            let text = std::fs::read_to_string(path).map_err(read_err)?;
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| format!("{}: invalid JSON: {e}", path.display()))?;
            format!("export default JSON.parse({});\n", serde_json::to_string(&text).unwrap())
        }
        Kind::Text => {
            let text = std::fs::read_to_string(path).map_err(read_err)?;
            format!("export default {};\n", serde_json::to_string(&text).unwrap())
        }
        Kind::Bytes => {
            let bytes = std::fs::read(path).map_err(read_err)?;
            let list: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
            format!("export default new Uint8Array([{}]);\n", list.join(","))
        }
    })
}

/// Build the bundle of the program whose entry module is `entry`.
pub fn bundle(entry: &Path) -> Result<Bundle, String> {
    let allocator = Allocator::default();
    let mut units: Vec<Unit> = Vec::new();
    let mut index: HashMap<(PathBuf, Kind), usize> = HashMap::new();
    let mut queue = vec![(entry.to_path_buf(), Kind::Script)];
    index.insert((entry.to_path_buf(), Kind::Script), 0);
    // (The entry's own path, canonical, is its index too.)
    if let Ok(c) = entry.canonicalize() {
        index.insert((c, Kind::Script), 0);
    }
    let mut i = 0;
    while i < queue.len() {
        let (path, kind) = queue[i].clone();
        let original = match kind {
            Kind::Script => std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?,
            _ => synthesize(&path, kind)?,
        };
        let stripped = allocator.alloc_str(&marker::strip(&original).source);
        let parsed = Parser::new(&allocator, stripped, SourceType::ts().with_module(true)).parse();
        let mut errors: Vec<OxcDiagnostic> = parsed.diagnostics.into_iter().collect();
        let program: &Program = allocator.alloc(parsed.program);
        let ret = SemanticBuilder::new().with_build_nodes(true).with_check_syntax_error(true).build(program);
        if errors.is_empty() {
            errors.extend(ret.diagnostics);
        }
        let mut unit = Unit {
            path: path.clone(),
            original,
            program,
            sem: ret.semantic,
            imports: Vec::new(),
            exports: Vec::new(),
            stars: Vec::new(),
            deps: Vec::new(),
            deletions: Vec::new(),
            default_expr: None,
            unsupported: None,
            reexports: Vec::new(),
            errors,
            dynamic: Vec::new(),
            metas: Vec::new(),
            tla: false,
        };
        let mut resolve_target = |spec: &str, attr: Option<String>| -> Result<Target, String> {
            Ok(match resolve(&path, spec)? {
                Resolved::External(s) => Target::External(s),
                Resolved::File(p) => {
                    let kind = match attr.as_deref() {
                        Some("json") => Kind::Json,
                        Some("text") => Kind::Text,
                        Some("bytes") => Kind::Bytes,
                        Some("javascript") | None if p.extension().is_some_and(|e| e == "json") => Kind::Json,
                        Some("javascript") | None => Kind::Script,
                        Some(t) => return Err(format!("unsupported import type `{t}`")),
                    };
                    let p = p.canonicalize().unwrap_or(p);
                    let n = queue.len();
                    let k = *index.entry((p.clone(), kind)).or_insert(n);
                    if k == n {
                        queue.push((p, kind));
                    }
                    Target::Module(k)
                }
            })
        };
        if unit.errors.is_empty() {
            records(&mut unit, program, &mut resolve_target)?;
        }
        units.push(unit);
        i += 1;
    }
    // One module: as written (the compiler reports its errors).
    if units.len() == 1
        && units[0].imports.iter().all(|i| matches!(i.target, Target::External(_)))
        && units[0].dynamic.iter().all(|d| d.1.is_none())
    {
        let u = units.pop().unwrap();
        return Ok(single(&u.path, u.original));
    }
    for u in &mut units {
        if !u.errors.is_empty() {
            return Err(crate::erase::render(std::mem::take(&mut u.errors), &u.original, &u.path));
        }
    }
    for u in &units {
        if let Some(e) = &u.unsupported {
            return Err(e.clone());
        }
    }
    link(&units)
}

fn single(path: &Path, original: String) -> Bundle {
    let end = original.len() as u32;
    Bundle {
        source: original.clone(),
        modules: vec![ModuleSource { path: path.to_path_buf(), original, start: 0, end, first_line: 1, map: Vec::new() }],
    }
}

/// Shorthand declarations: `{ a }` destructuring properties and `{ a }`
/// import specifiers (by where the name starts), which a rename must
/// expand.
#[derive(Default)]
struct Shorthands {
    properties: HashSet<u32>,
    imports: HashSet<u32>,
}

impl<'a> Visit<'a> for Shorthands {
    fn visit_binding_property(&mut self, it: &BindingProperty<'a>) {
        if it.shorthand {
            self.properties.insert(it.key.span().start);
        }
        walk::walk_binding_property(self, it);
    }

    fn visit_import_specifier(&mut self, it: &ImportSpecifier<'a>) {
        if it.imported.span() == it.local.span {
            self.imports.insert(it.local.span.start);
        }
    }
}

/// Names the merged program defines besides the modules' own.
struct Names {
    /// Each binding's name in the merged program (namespaces: the function
    /// that returns the module's namespace object).
    of: HashMap<Binding, String>,
    /// Import bindings that are assigned to: the function that returns an
    /// object whose `v` reads the binding and throws when set.
    writes: HashMap<(usize, SymbolId), String>,
}

/// The JavaScript globals the code this adds refers to: no module's
/// top-level binding may take their names.
const GLOBALS_USED: [&str; 7] = ["Object", "Proxy", "Reflect", "Symbol", "TypeError", "JSON", "Uint8Array"];

/// Order, name and merge the modules.
fn link(units: &[Unit]) -> Result<Bundle, String> {
    // Evaluation order: dependencies first (run-time ones), depth first.
    fn visit(m: usize, units: &[Unit], state: &mut [u8], order: &mut Vec<usize>) {
        state[m] = 1;
        for (t, runtime) in &units[m].deps {
            if let (Target::Module(d), true) = (t, runtime)
                && state[*d] == 0
            {
                visit(*d, units, state, order);
            }
        }
        state[m] = 2;
        order.push(m);
    }
    let mut state = vec![0u8; units.len()];
    let mut order = Vec::new();
    visit(0, units, &mut state, &mut order);
    // Modules imported for their types only: their type declarations.
    let typed_only: Vec<usize> = (0..units.len()).filter(|&m| state[m] == 0).collect();
    let all: Vec<usize> = typed_only.iter().copied().chain(order.iter().copied()).collect();

    // `export { a } from` must resolve, used or not (as ESM links them).
    for u in units {
        for (span, t, name) in &u.reexports {
            if resolve_export(units, *t, name, &mut Vec::new()).is_none() {
                return Err(format!(
                    "{}: module `{}` has no export `{name}` (or more than one through `export *`)",
                    loc(&u.path, &u.original, span.start),
                    units[*t].path.display()
                ));
            }
        }
    }

    // What each import binding resolves to.
    let mut resolved: HashMap<(usize, SymbolId), Binding> = HashMap::new();
    for (m, u) in units.iter().enumerate() {
        for imp in &u.imports {
            let Target::Module(t) = imp.target else { continue };
            for (sym, name) in &imp.bindings {
                let b = match name {
                    Name::Namespace => Binding::Namespace(t),
                    Name::Named(n) => resolve_export(units, t, n, &mut Vec::new()).ok_or_else(|| {
                        format!(
                            "{}: module `{}` has no export `{n}` (or more than one through `export *`)",
                            loc(&u.path, &u.original, imp.span.start),
                            units[t].path.display()
                        )
                    })?,
                };
                resolved.insert((m, *sym), b);
            }
        }
    }

    // Namespace objects that are needed: of namespace imports used other
    // than as `ns.x` (with their references' scopes), and those they hold.
    let mut ns_sites: HashMap<usize, Vec<(usize, ScopeId)>> = HashMap::new();
    let mut writes: Vec<(usize, SymbolId)> = Vec::new();
    for (&(m, sym), &b) in &resolved {
        let u = &units[m];
        for r in u.sem.scoping().get_resolved_references(sym) {
            if r.flags().is_write() && !writes.contains(&(m, sym)) {
                writes.push((m, sym));
            }
            let Binding::Namespace(t) = b else { continue };
            match member_of_namespace(units, u.sem.nodes(), r.node_id(), t) {
                Some((_, Binding::Namespace(k))) => ns_sites.entry(k).or_default().push((m, u.ref_scope(r.node_id()))),
                Some(_) => {}
                None => ns_sites.entry(t).or_default().push((m, u.ref_scope(r.node_id()))),
            }
        }
    }
    writes.sort();
    let mut needed: Vec<usize> = ns_sites.keys().copied().collect();
    for &(m, sym) in &writes {
        if let Binding::Namespace(t) = resolved[&(m, sym)]
            && !needed.contains(&t)
        {
            needed.push(t);
        }
    }
    let mut k = 0;
    while k < needed.len() {
        let t = needed[k];
        for n in export_names(units, t) {
            if let Some(Binding::Namespace(j)) = resolve_export(units, t, &n, &mut Vec::new())
                && !needed.contains(&j)
            {
                needed.push(j);
            }
        }
        k += 1;
    }
    needed.sort();

    // Names: each top-level binding of each module (but import bindings of
    // the program's modules), each default expression and each namespace
    // object, unique, not the name of a global the modules (or the code
    // added) use, and not that of a binding in a scope where a reference to
    // it is.
    let mut taken: HashSet<String> = GLOBALS_USED.iter().map(|s| s.to_string()).collect();
    for u in units {
        for name in u.sem.scoping().root_unresolved_references().keys() {
            taken.insert(name.to_string());
        }
    }
    let mut importers: HashMap<Binding, Vec<(usize, SymbolId)>> = HashMap::new();
    for (&(m, sym), &b) in &resolved {
        importers.entry(b).or_default().push((m, sym));
    }
    let pick = |base: &str, sites: &[(usize, ScopeId)], taken: &HashSet<String>| -> String {
        let mut k = 0;
        loop {
            let name = if k == 0 { base.to_string() } else { format!("{base}${k}") };
            k += 1;
            if taken.contains(&name) || sites.iter().any(|&(m, s)| units[m].shadowed(s, &name)) {
                continue;
            }
            return name;
        }
    };
    let sites_of = |b: Binding, own: Vec<(usize, ScopeId)>| -> Vec<(usize, ScopeId)> {
        let mut v = own;
        for &(m, sym) in importers.get(&b).map(|v| v.as_slice()).unwrap_or(&[]) {
            let u = &units[m];
            v.extend(u.sem.scoping().get_resolved_references(sym).map(|r| (m, u.ref_scope(r.node_id()))));
        }
        v
    };
    let mut names = Names { of: HashMap::new(), writes: HashMap::new() };
    // `type int = number` in several modules is one alias (the checker
    // knows `int` by its name).
    let mut int_aliases: Vec<(usize, SymbolId)> = Vec::new();
    for &m in &all {
        let u = &units[m];
        let scoping = u.sem.scoping();
        let mut syms: Vec<SymbolId> = scoping.get_bindings(u.root()).iter().map(|(_, &s)| s).collect();
        syms.sort_by_key(|&s| scoping.symbol_span(s).start);
        for sym in syms {
            if u.internal_import(sym).is_some() {
                continue;
            }
            let name = scoping.symbol_name(sym);
            let b = Binding::Symbol(m, sym);
            if name == "int" && scoping.symbol_flags(sym).is_type_alias() {
                int_aliases.push((m, sym));
                if int_aliases.len() > 1 {
                    names.of.insert(b, "int".into());
                    continue;
                }
            }
            let own = scoping.get_resolved_references(sym).map(|r| (m, u.ref_scope(r.node_id()))).collect();
            let n = pick(name, &sites_of(b, own), &taken);
            taken.insert(n.clone());
            names.of.insert(b, n);
        }
        if u.default_expr.is_some() {
            let b = Binding::Default(m);
            let n = pick(&format!("{}_default", stem(&u.path)), &sites_of(b, Vec::new()), &taken);
            taken.insert(n.clone());
            names.of.insert(b, n);
        }
    }
    for &t in &needed {
        let sites = ns_sites.get(&t).cloned().unwrap_or_default();
        let n = pick(&format!("{}_ns", stem(&units[t].path)), &sites, &taken);
        taken.insert(n.clone());
        // (and its cache, `<name>$`)
        taken.insert(format!("{n}$"));
        names.of.insert(Binding::Namespace(t), n);
    }
    for &(m, sym) in &writes {
        let u = &units[m];
        let sites: Vec<(usize, ScopeId)> =
            u.sem.scoping().get_resolved_references(sym).map(|r| (m, u.ref_scope(r.node_id()))).collect();
        let n = pick(&format!("{}_import", u.sem.scoping().symbol_name(sym)), &sites, &taken);
        taken.insert(n.clone());
        taken.insert(format!("{n}$"));
        names.writes.insert((m, sym), n);
    }
    let helper = (!needed.is_empty()).then(|| {
        let n = pick("__namespace", &[], &taken);
        taken.insert(n.clone());
        n
    });

    // The text of each module, edited, in order; what must run before any
    // of them (`.name` of anonymous default functions, which are hoisted)
    // is put first, on the first line.
    let mut source = String::new();
    let mut modules = Vec::new();
    let mut line = 1u32;
    let mut prelude = String::new();
    for &m in &all {
        let u = &units[m];
        let typed = typed_only.contains(&m);
        if let Some(d) = u.default_expr
            && !typed
            && d.anonymous == Anon::Function
            && d.name_at.is_some()
        {
            prelude.push_str(&format!(
                "Object.defineProperty({}, \"name\", {{ value: \"default\" }}); ",
                names.of[&Binding::Default(m)]
            ));
        }
    }
    source.push_str(&prelude);
    for &m in &all {
        let u = &units[m];
        let mut edits = edits(units, m, &resolved, &names, typed_only.contains(&m), &int_aliases)?;
        // (An edit inside a removed span is dropped: the longer first.)
        edits.sort_by_key(|e| (e.0.start, std::cmp::Reverse(e.0.end)));
        let start = source.len() as u32;
        let mut map = Vec::new();
        let mut pos = 0usize;
        let text = &u.original;
        for (span, repl) in edits {
            let (s, e) = (span.start as usize, span.end as usize);
            if s < pos || (s == pos && e == s && map.last().is_some_and(|l: &(u32, u32)| l.1 as usize > s)) {
                continue;
            }
            source.push_str(&text[pos..s]);
            source.push_str(&repl);
            pos = e;
            map.push((source.len() as u32, e as u32));
        }
        source.push_str(&text[pos..]);
        if !source.ends_with('\n') {
            source.push('\n');
        }
        let end = source.len() as u32;
        modules.push(ModuleSource { path: u.path.clone(), original: u.original.clone(), start, end, first_line: line, map });
        line += source[start as usize..].matches('\n').count() as u32;
    }
    // Namespace objects: made on first use (`__namespace` below).
    if let Some(helper) = &helper {
        source.push_str(&format!("function {helper}{NAMESPACE_HELPER}\n"));
    }
    for &t in &needed {
        let name = &names.of[&Binding::Namespace(t)];
        // (Not types: they are not values at run time.)
        let is_value = |b: &Binding| match *b {
            Binding::Symbol(k, sym) => units[k].sem.scoping().symbol_flags(sym).is_value(),
            _ => true,
        };
        let mut entries: Vec<(String, Binding)> = export_names(units, t)
            .into_iter()
            .filter_map(|n| resolve_export(units, t, &n, &mut Vec::new()).map(|b| (n, b)))
            .filter(|(_, b)| is_value(b))
            .collect();
        // (In the order of their UTF-16 code units, as the spec sorts them.)
        entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        let keys: Vec<String> = entries.iter().map(|(n, _)| serde_json::to_string(n).unwrap()).collect();
        let cases: Vec<String> = entries
            .iter()
            .map(|(n, b)| {
                let value = match b {
                    Binding::Namespace(k) => format!("{}()", names.of[&Binding::Namespace(*k)]),
                    b => names.of[b].clone(),
                };
                format!("case {}: return {value};", serde_json::to_string(n).unwrap())
            })
            .collect();
        source.push_str(&format!(
            "var {name}$; function {name}() {{ return {name}$ || ({name}$ = {}([{}], (k) => {{ switch (k) {{ {} }} }})); }}\n",
            helper.as_ref().unwrap(),
            keys.join(", "),
            cases.join(" "),
        ));
    }
    // Import bindings that are assigned to: read through `v`, which throws
    // when set.
    for (&(m, sym), name) in &names.writes {
        let target = match resolved[&(m, sym)] {
            Binding::Namespace(t) => format!("{}()", names.of[&Binding::Namespace(t)]),
            b => names.of[&b].clone(),
        };
        source.push_str(&format!(
            "var {name}$; function {name}() {{ return {name}$ || ({name}$ = {{ get v() {{ return {target}; }}, set v(_) {{ throw new TypeError(\"Assignment to constant variable.\"); }} }}); }}\n"
        ));
    }
    Ok(Bundle { source, modules })
}

/// A module namespace object (after `function <name>`): a Proxy over a
/// non-extensible object with the module's export names, whose traps are
/// those of module namespace exotic objects (ECMA-262 10.4.6): values of
/// live bindings (throwing while uninitialized), not writable, not
/// deletable, a null prototype, `@@toStringTag` "Module".
const NAMESPACE_HELPER: &str = "(names, get) { \
const target = Object.create(null); \
for (const n of names) Object.defineProperty(target, n, { value: undefined, writable: true, enumerable: true, configurable: false }); \
Object.defineProperty(target, Symbol.toStringTag, { value: \"Module\" }); \
Object.preventExtensions(target); \
const has = (k) => typeof k === \"string\" && names.includes(k); \
return new Proxy(target, { \
get(t, k) { return has(k) ? get(k) : Reflect.get(t, k); }, \
set() { return false; }, \
has(t, k) { return has(k) || Reflect.has(t, k); }, \
deleteProperty(t, k) { return has(k) ? false : Reflect.deleteProperty(t, k); }, \
getOwnPropertyDescriptor(t, k) { return has(k) ? { value: get(k), writable: true, enumerable: true, configurable: false } : Reflect.getOwnPropertyDescriptor(t, k); }, \
defineProperty(t, k, d) { \
if (!has(k)) return Reflect.defineProperty(t, k, d); \
const v = get(k); \
if (d.configurable === true || d.enumerable === false || \"get\" in d || \"set\" in d || d.writable === false) return false; \
if (\"value\" in d && !Object.is(d.value, v)) return false; \
Reflect.defineProperty(t, k, { value: v }); return true; }, \
ownKeys() { return [...names, Symbol.toStringTag]; }, \
getPrototypeOf() { return null; }, \
setPrototypeOf(t, p) { return p === null; }, \
isExtensible() { return false; }, \
preventExtensions() { return true; } }); }";

fn stem(path: &Path) -> String {
    let s: String = path
        .file_stem()
        .map(|s| s.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect())
        .unwrap_or_default();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) { format!("m_{s}") } else { s }
}

/// The names module `m` exports (`export *` brings all but `default`).
fn export_names(units: &[Unit], m: usize) -> Vec<String> {
    fn go(units: &[Unit], m: usize, seen: &mut Vec<usize>, out: &mut Vec<String>, star: bool) {
        if seen.contains(&m) {
            return;
        }
        seen.push(m);
        for (n, _) in &units[m].exports {
            if !(star && n == "default") && !out.contains(n) {
                out.push(n.clone());
            }
        }
        for t in &units[m].stars {
            if let Target::Module(t) = t {
                go(units, *t, seen, out, true);
            }
        }
    }
    let mut out = Vec::new();
    go(units, m, &mut Vec::new(), &mut out, false);
    out
}

/// What export `name` of module `m` resolves to (none: not found, or
/// ambiguous through `export *`).
fn resolve_export(units: &[Unit], m: usize, name: &str, seen: &mut Vec<(usize, String)>) -> Option<Binding> {
    if seen.iter().any(|s| s.0 == m && s.1 == name) {
        return None;
    }
    seen.push((m, name.to_string()));
    let u = &units[m];
    if let Some((_, e)) = u.exports.iter().find(|(n, _)| n == name) {
        return match e {
            Export::Local(sym) => match u.internal_import(*sym) {
                Some((t, Name::Named(n))) => {
                    let n = n.clone();
                    resolve_export(units, t, &n, seen)
                }
                Some((t, Name::Namespace)) => Some(Binding::Namespace(t)),
                None => Some(Binding::Symbol(m, *sym)),
            },
            Export::DefaultExpr => Some(Binding::Default(m)),
            Export::From(Target::Module(t), Name::Named(n)) => resolve_export(units, *t, n, seen),
            Export::From(Target::Module(t), Name::Namespace) => Some(Binding::Namespace(*t)),
            Export::From(Target::External(_), _) => None,
        };
    }
    if name == "default" {
        return None;
    }
    // `export *`: the binding they agree on.
    let mut found: Option<Binding> = None;
    for t in &u.stars {
        if let Target::Module(t) = t
            && let Some(b) = resolve_export(units, *t, name, &mut seen.clone())
        {
            if found.is_some_and(|f| f != b) {
                return None;
            }
            found = Some(b);
        }
    }
    found
}

/// Whether the function that binding `b` names uses `this` (a call
/// `ns.f()` passes the namespace object as `this`): a function declaration
/// or expression whose body (not that of a nested function) has `this`.
fn uses_this(units: &[Unit], b: Binding) -> bool {
    struct This(bool);
    impl<'a> Visit<'a> for This {
        fn visit_this_expression(&mut self, _: &ThisExpression) {
            self.0 = true;
        }
        fn visit_function(&mut self, _: &Function<'a>, _: oxc::syntax::scope::ScopeFlags) {}
        fn visit_class(&mut self, _: &Class<'a>) {}
    }
    let Binding::Symbol(m, sym) = b else { return false };
    let u = &units[m];
    let scoping = u.sem.scoping();
    let mut v = This(false);
    match u.sem.nodes().get_node(scoping.symbol_declaration(sym)).kind() {
        AstKind::Function(f) => {
            if let Some(body) = &f.body {
                v.visit_function_body(body);
            }
        }
        AstKind::VariableDeclarator(d) => match d.init.as_ref().map(|e| e.without_parentheses()) {
            Some(Expression::FunctionExpression(f)) => {
                if let Some(body) = &f.body {
                    v.visit_function_body(body);
                }
            }
            Some(Expression::ArrowFunctionExpression(_)) => {}
            // (Another value: its methods might.)
            _ => return true,
        },
        AstKind::Class(_) => {}
        _ => return true,
    }
    v.0
}

/// For a reference to a namespace import at `node`: `ns.x` (or the type
/// `ns.X`) as a whole, and what `x` is in module `t`, where it reads `x`
/// (not where it is assigned or deleted, or called with the namespace as
/// `this` by a function that uses it).
fn member_of_namespace(units: &[Unit], nodes: &oxc::semantic::AstNodes, node: NodeId, t: usize) -> Option<(Span, Binding)> {
    let span = nodes.get_node(node).kind().span();
    let (whole, prop, is_type) = match nodes.parent_kind(node) {
        AstKind::StaticMemberExpression(e) if e.object.span() == span => (e.span, e.property.name.as_str(), false),
        AstKind::TSQualifiedName(q) if q.left.span() == span => (q.span, q.right.name.as_str(), true),
        _ => return None,
    };
    let b = resolve_export(units, t, prop, &mut Vec::new())?;
    if !is_type {
        let member = nodes.parent_id(node);
        match nodes.parent_kind(member) {
            AstKind::AssignmentExpression(a) if a.left.span() == whole => return None,
            AstKind::UpdateExpression(_)
            | AstKind::ArrayAssignmentTarget(_)
            | AstKind::ObjectAssignmentTarget(_)
            | AstKind::AssignmentTargetWithDefault(_)
            | AstKind::AssignmentTargetPropertyProperty(_)
            | AstKind::AssignmentTargetRest(_) => return None,
            AstKind::UnaryExpression(u) if u.operator == UnaryOperator::Delete => return None,
            AstKind::ForInStatement(f) if f.left.span() == whole => return None,
            AstKind::ForOfStatement(f) if f.left.span() == whole => return None,
            AstKind::CallExpression(c) if c.callee.span() == whole && uses_this(units, b) => return None,
            AstKind::TaggedTemplateExpression(c) if c.tag.span() == whole && uses_this(units, b) => return None,
            _ => {}
        }
    }
    Some((whole, b))
}

/// The edits of module `m`: (span, replacement); one inside a removed span
/// is dropped.
fn edits(
    units: &[Unit],
    m: usize,
    resolved: &HashMap<(usize, SymbolId), Binding>,
    names: &Names,
    typed_only: bool,
    int_aliases: &[(usize, SymbolId)],
) -> Result<Vec<(Span, String)>, String> {
    let u = &units[m];
    let scoping = u.sem.scoping();
    let nodes = u.sem.nodes();
    let program = u.program;
    let text = &u.original;
    let mut out: Vec<(Span, String)> = Vec::new();
    // Removed text: blanks, keeping the lines.
    let blank = |span: Span| -> (Span, String) {
        let s: String = text[span.start as usize..span.end as usize].chars().map(|c| if c == '\n' { '\n' } else { ' ' }).collect();
        (span, s)
    };
    for imp in &u.imports {
        if matches!(imp.target, Target::Module(_)) {
            out.push(blank(imp.span));
        }
    }
    for &d in &u.deletions {
        out.push(blank(d));
    }
    if let Some(d) = u.default_expr
        && !typed_only
    {
        let name = &names.of[&Binding::Default(m)];
        let mut after = String::new();
        match d.name_at {
            Some(at) => {
                out.push(blank(d.prefix));
                out.push((Span::new(at, at), format!(" {name}")));
            }
            None => {
                out.push((d.prefix, format!("const {name} = ")));
                if !text[..d.end as usize].trim_end().ends_with(';') {
                    after.push(';');
                }
            }
        }
        // (A function declaration's is set first: it is hoisted. A class
        // keeps a static `name` member of its own.)
        match (d.anonymous, d.name_at) {
            (Anon::Function, None) => {
                after.push_str(&format!(" Object.defineProperty({name}, \"name\", {{ value: \"default\" }});"));
            }
            (Anon::Class, _) => after.push_str(&format!(
                " if (typeof Object.getOwnPropertyDescriptor({name}, \"name\")?.value === \"string\") \
                 Object.defineProperty({name}, \"name\", {{ value: \"default\" }});"
            )),
            _ => {}
        }
        if !after.is_empty() {
            out.push((Span::new(d.end, d.end), after));
        }
    }
    // A module imported for its types only: its type declarations.
    if typed_only {
        for stmt in &program.body {
            let keep = match stmt {
                Statement::TSTypeAliasDeclaration(_) | Statement::TSInterfaceDeclaration(_) | Statement::ImportDeclaration(_) => true,
                Statement::ExportDeclaration(e) => {
                    matches!(e.declaration, Declaration::TSTypeAliasDeclaration(_) | Declaration::TSInterfaceDeclaration(_))
                }
                _ => false,
            };
            if !keep {
                out.push(blank(stmt.span()));
            }
        }
    }
    // A second `type int = number`: removed.
    for &(am, sym) in int_aliases.iter().skip(1) {
        if am == m {
            out.push(blank(nodes.get_node(scoping.symbol_declaration(sym)).kind().span()));
        }
    }

    let mut shorthands = Shorthands::default();
    shorthands.visit_program(program);
    let shorthand_ref = |node: NodeId| -> bool {
        match nodes.parent_kind(node) {
            AstKind::ObjectProperty(p) => p.shorthand,
            AstKind::AssignmentTargetPropertyIdentifier(_) => true,
            _ => false,
        }
    };
    let rename_at = |out: &mut Vec<(Span, String)>, span: Span, old: &str, new: &str, shorthand: bool| {
        if shorthand {
            out.push((span, format!("{old}: {new}")));
        } else if old != new {
            out.push((span, new.to_string()));
        }
    };
    let mut syms: Vec<SymbolId> = scoping.get_bindings(u.root()).iter().map(|(_, &s)| s).collect();
    syms.sort_by_key(|&s| scoping.symbol_span(s).start);
    for sym in syms {
        let old = scoping.symbol_name(sym);
        if let Some(&target) = resolved.get(&(m, sym)) {
            // An import binding: its references are to the binding it
            // resolves to (assignments: through an accessor that throws).
            for r in scoping.get_resolved_references(sym) {
                let node = r.node_id();
                let span = nodes.get_node(node).kind().span();
                if r.flags().is_write() {
                    rename_at(&mut out, span, old, &format!("{}().v", names.writes[&(m, sym)]), shorthand_ref(node));
                    continue;
                }
                let new = match target {
                    Binding::Namespace(t) => match member_of_namespace(units, nodes, node, t) {
                        Some((whole, b)) => {
                            let new = match b {
                                Binding::Namespace(k) => format!("{}()", names.of[&Binding::Namespace(k)]),
                                b => names.of[&b].clone(),
                            };
                            out.push((whole, new));
                            continue;
                        }
                        None if r.flags().is_type() => {
                            return Err(format!(
                                "{}: this use of a namespace import is not supported yet",
                                loc(&u.path, text, span.start)
                            ));
                        }
                        None => format!("{}()", names.of[&target]),
                    },
                    b => names.of[&b].clone(),
                };
                rename_at(&mut out, span, old, &new, shorthand_ref(node));
            }
            continue;
        }
        let Some(new) = names.of.get(&Binding::Symbol(m, sym)) else { continue };
        if new == old {
            continue;
        }
        let decls = std::iter::once(scoping.symbol_span(sym)).chain(scoping.symbol_redeclarations(sym).iter().map(|r| r.span));
        for span in decls {
            out.push(if shorthands.properties.contains(&span.start) {
                (span, format!("{old}: {new}"))
            } else if shorthands.imports.contains(&span.start) {
                (span, format!("{old} as {new}"))
            } else {
                (span, new.clone())
            });
        }
        for r in scoping.get_resolved_references(sym) {
            let node = r.node_id();
            rename_at(&mut out, nodes.get_node(node).kind().span(), old, new, shorthand_ref(node));
        }
    }
    Ok(out)
}
