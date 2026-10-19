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

mod shake;

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
    /// What the program's build should warn of.
    pub warnings: Vec<String>,
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
    /// Whether each binding is a type only (`type`, or only used so): a
    /// JavaScript module need not have it (its types are elsewhere).
    types: Vec<bool>,
}

/// What a module's file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
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
    /// A CommonJS module (no `import` or `export`, and `module`, `exports`
    /// or `require` not declared): for the error that importing it is.
    commonjs: bool,
    /// Whether its package says it has no side effects (`"sideEffects"` in
    /// the nearest `package.json`): it is then part of the program only if
    /// the program uses what it declares.
    pure: bool,
    /// `export { a } from`: where, the module and the name it re-exports
    /// (which must resolve, used or not).
    reexports: Vec<(Span, usize, String)>,
    /// Its early errors (parse and semantic).
    errors: Vec<OxcDiagnostic>,
    /// Its `import(...)` expressions.
    dynamic: Vec<DynImport>,
    /// Where `import.meta` is, and the scope there.
    metas: Vec<(Span, ScopeId)>,
    /// Whether its code awaits at the top level.
    tla: bool,
    /// What it is warned of (`import()`s left to the engine).
    warnings: Vec<String>,
    /// The modules the engine provides that its `import()`s import (which
    /// the program then links).
    dyn_externals: Vec<String>,
}

/// An `import(...)` expression.
struct DynImport {
    span: Span,
    /// `import(`: what a call of the run-time loader replaces.
    head: Span,
    /// The scope it is in.
    scope: ScopeId,
    to: DynTo,
}

/// What an `import(...)` imports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DynTo {
    /// A module of the program.
    Module(usize),
    /// What the engine loads and interprets at run time (a specifier that
    /// is computed, or that does not resolve when the program is built).
    Fallback,
    /// A module the engine provides, or a source or deferred import: the
    /// engine's own.
    Leave,
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

/// What a specifier resolves to.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// A module of the bundle (a file, or a path an embedder's `Host` knows).
    File(PathBuf),
    /// A module the bundle imports as it is (the engine's, a runtime's).
    External(String),
}

/// Where a bundle's modules come from: the file system, as Luminous
/// resolves them (`bundle_with`), or an embedder (a bundler's plugins,
/// `Beyond.build`).
pub trait Host {
    /// What `spec`, imported by module `from` (`dynamic`: by `import()`;
    /// `attr`: its `with { type }`), is.
    fn resolve(&mut self, from: &Path, spec: &str, dynamic: bool, attr: Option<&str>) -> Result<Resolved, String>;
    /// The source of module `path` imported as `kind` (a module of JSON,
    /// text or bytes as `synthesize` makes it).
    fn load(&mut self, path: &Path, kind: Kind) -> Result<String, String>;
}

/// Luminous's own `Host`: files, resolved as TypeScript and Node do.
struct FsHost<'a> {
    emb: &'a Embedding,
}

impl Host for FsHost<'_> {
    fn resolve(&mut self, from: &Path, spec: &str, _dynamic: bool, _attr: Option<&str>) -> Result<Resolved, String> {
        resolve(from, spec, self.emb)
    }

    fn load(&mut self, path: &Path, kind: Kind) -> Result<String, String> {
        match kind {
            Kind::Script => std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display())),
            _ => synthesize(path, kind),
        }
    }
}

/// Whether the `package.json` nearest to module `path` says that it has no
/// side effects: `"sideEffects": false`, or a list of the files that have
/// (patterns as webpack's: one without a `/` matches a file name).
fn side_effect_free(path: &Path, packages: &mut HashMap<PathBuf, Option<serde_json::Value>>) -> bool {
    let mut dir = path.parent();
    while let Some(d) = dir {
        let manifest = packages
            .entry(d.to_path_buf())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(d.join("package.json")).ok()?;
                serde_json::from_str::<serde_json::Value>(&text).ok()
            })
            .clone();
        if let Some(m) = manifest {
            let rel = path.strip_prefix(d).unwrap_or(path).to_string_lossy().replace('\\', "/");
            return match m.get("sideEffects") {
                Some(serde_json::Value::Bool(false)) => true,
                Some(serde_json::Value::Array(files)) => !files.iter().filter_map(|f| f.as_str()).any(|f| {
                    let f = f.strip_prefix("./").unwrap_or(f);
                    if f.contains('/') {
                        glob(f, &rel)
                    } else {
                        glob(f, rel.rsplit('/').next().unwrap_or(&rel)) || glob(&format!("**/{f}"), &rel)
                    }
                }),
                _ => false,
            };
        }
        dir = d.parent();
    }
    false
}

/// Whether `text` matches glob `pattern` (`*`: any characters but `/`;
/// `**`: any).
fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p {
            [] => t.is_empty(),
            [b'*', b'*', b'/', rest @ ..] => go(rest, t) || (!t.is_empty() && go(p, &t[1..])),
            [b'*', b'*', rest @ ..] => (0..=t.len()).any(|i| go(rest, &t[i..])),
            [b'*', rest @ ..] => {
                (0..=t.len()).take_while(|&i| i == 0 || t[i - 1] != b'/').any(|i| go(rest, &t[i..]))
            }
            [c, rest @ ..] => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// What a program built by an embedder (a runtime built on Luminous) has
/// besides its own modules.
#[derive(Debug, Clone, Default)]
pub struct Embedding {
    /// Modules that run before the entry, in this order (as if it imported
    /// them first).
    pub preludes: Vec<PathBuf>,
    /// Specifier prefixes of the embedder's modules: `prefix` + `name` is
    /// `dir/name`, resolved as a relative path is (`beyond:http` with
    /// `("beyond:", lib)`: `lib/http.ts`).
    pub module_dirs: Vec<(String, PathBuf)>,
    /// Modules the embedder provides natively (C modules): imports of them
    /// stay, as those of the engine's own modules.
    pub externals: Vec<String>,
    /// Globals that are bindings of the embedder's modules (name and the
    /// specifier of the module that exports it under that name), which its
    /// preludes make globals: in the program's own modules, a reference to
    /// such a global that nothing declares is to that binding (an import of
    /// it), which native code then knows (`new Response()` a class's native
    /// constructor). Not where the module assigns the global.
    pub globals: Vec<(String, String)>,
    /// Specifiers that are others' (`fs` is `node:fs`).
    pub aliases: Vec<(String, String)>,
    /// Directories whose modules a program registers by path, for the
    /// embedder's loader to give modules it loads at run time (the
    /// embedder's own; a program that imports at run time registers all
    /// its modules).
    pub shared: Vec<PathBuf>,
    /// A bundle other programs run (`Beyond.build`), not a program Luminous
    /// compiles: the entry's exports stay the bundle's, an `import()` of a
    /// computed specifier stays one, and one that does not resolve is an
    /// error.
    pub library: bool,
}

/// The imports that make the embedder's globals that `program` refers to
/// (and does not declare or assign) bindings: text to append to its source,
/// if there are any.
fn global_imports(sem: &Semantic, emb: &Embedding) -> Option<String> {
    let scoping = sem.scoping();
    let unresolved = scoping.root_unresolved_references();
    let mut by_module: Vec<(&str, Vec<&str>)> = Vec::new();
    for (name, spec) in &emb.globals {
        let Some(refs) = unresolved.get(name.as_str()) else { continue };
        if refs.iter().any(|&r| scoping.get_reference(r).is_write()) {
            continue;
        }
        match by_module.iter_mut().find(|(s, _)| *s == spec.as_str()) {
            Some((_, names)) => names.push(name),
            None => by_module.push((spec, vec![name])),
        }
    }
    if by_module.is_empty() {
        return None;
    }
    // (At the end: the module's own text keeps its lines and columns.)
    let mut text = String::from("\n");
    for (spec, names) in by_module {
        text.push_str(&format!("import {{ {} }} from {};", names.join(", "), serde_json::to_string(spec).unwrap()));
    }
    Some(text)
}

/// Resolve a module specifier imported by `from`.
fn resolve(from: &Path, spec: &str, emb: &Embedding) -> Result<Resolved, String> {
    if let Some((_, to)) = emb.aliases.iter().find(|(a, _)| a == spec) {
        return resolve(from, to, emb);
    }
    if spec.starts_with("qjs:") || spec.starts_with("luminous:") || emb.externals.iter().any(|e| e == spec) {
        return Ok(Resolved::External(spec.to_string()));
    }
    let not_found = || format!("cannot find module `{spec}`");
    let opts = ResolveOptions::luminous();
    for (prefix, dir) in &emb.module_dirs {
        if let Some(name) = spec.strip_prefix(prefix.as_str()) {
            return resolve_path(&dir.join(name), &opts).map(Resolved::File).ok_or_else(not_found);
        }
    }
    let dir = from.parent().unwrap_or(Path::new("."));
    resolve_module(dir, spec, &opts).map(Resolved::File).ok_or_else(not_found)
}

/// How specifiers resolve to files (`resolve_module`): Luminous's own, or a
/// bundler's for its target.
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// The conditions of `package.json` `exports` and `imports` that apply
    /// (in no order: the map's decides).
    pub conditions: Vec<String>,
    /// What is tried, in order, after a path without one (and in a
    /// directory after `index`).
    pub extensions: Vec<String>,
    /// The fields of a `package.json` without `exports` that name its
    /// entry, in order.
    pub main_fields: Vec<String>,
}

impl ResolveOptions {
    /// Luminous's: ES modules in TypeScript or JavaScript.
    pub fn luminous() -> Self {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        ResolveOptions {
            conditions: owned(&["import", "module", "default"]),
            extensions: owned(&[".ts", ".mts", ".js", ".mjs"]),
            main_fields: owned(&["module", "main"]),
        }
    }
}

/// The file specifier `spec` names for a module in directory `dir`: a
/// relative or absolute path, a `#` import of the package there, or a
/// package (`name[/sub]`, `@scope/name[/sub]`) from the nearest
/// `node_modules` that has it.
pub fn resolve_module(dir: &Path, spec: &str, opts: &ResolveOptions) -> Option<PathBuf> {
    if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') || spec == "." || spec == ".." {
        return resolve_path(&dir.join(spec), opts);
    }
    if spec.starts_with('#') {
        return resolve_imports(dir, spec, opts);
    }
    let mut parts = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
    let pkg: Vec<&str> = parts.by_ref().take(if spec.starts_with('@') { 2 } else { 1 }).collect();
    let pkg = pkg.join("/");
    let sub = parts.next().unwrap_or("");
    for d in dir.ancestors() {
        let root = d.join("node_modules").join(&pkg);
        if root.is_dir() {
            return resolve_package(&root, sub, opts);
        }
    }
    None
}

/// A `#name` specifier through the `imports` of the nearest `package.json`
/// to `dir`.
fn resolve_imports(dir: &Path, spec: &str, opts: &ResolveOptions) -> Option<PathBuf> {
    for d in dir.ancestors() {
        let Ok(text) = std::fs::read_to_string(d.join("package.json")) else { continue };
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let target = exports_target(json.get("imports")?, spec, opts)?;
        return if target.starts_with("./") || target.starts_with("../") {
            resolve_path(&d.join(target), opts)
        } else {
            resolve_module(d, &target, opts)
        };
    }
    None
}

/// A file for a path, as TypeScript resolves it: as is, a `.js` path's
/// `.ts` source, with an extension added, or a directory's index.
fn resolve_path(base: &Path, opts: &ResolveOptions) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    let s = base.to_string_lossy();
    let has = |e: &str| opts.extensions.iter().any(|x| x == e);
    for (js, ts) in [(".js", ".ts"), (".mjs", ".mts"), (".cjs", ".cts"), (".jsx", ".tsx"), (".js", ".tsx")] {
        if let Some(stem) = s.strip_suffix(js)
            && has(ts)
        {
            let p = PathBuf::from(format!("{stem}{ts}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for ext in &opts.extensions {
        let p = PathBuf::from(format!("{s}{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    if base.is_dir() {
        for ext in &opts.extensions {
            let p = base.join(format!("index{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// A file of the package at `root` for subpath `sub` (empty: the package):
/// through its `exports` if it has them, else its main fields, or the path
/// itself.
fn resolve_package(root: &Path, sub: &str, opts: &ResolveOptions) -> Option<PathBuf> {
    let json: Option<serde_json::Value> =
        std::fs::read_to_string(root.join("package.json")).ok().and_then(|s| serde_json::from_str(&s).ok());
    if let Some(exports) = json.as_ref().and_then(|j| j.get("exports")) {
        let key = if sub.is_empty() { ".".to_string() } else { format!("./{sub}") };
        let target = exports_target(exports, &key, opts)?;
        return resolve_path(&root.join(target.trim_start_matches("./")), opts);
    }
    if !sub.is_empty() {
        return resolve_path(&root.join(sub), opts);
    }
    for field in &opts.main_fields {
        if let Some(f) = json.as_ref().and_then(|j| j.get(field)).and_then(|f| f.as_str())
            && let Some(p) = resolve_path(&root.join(f), opts)
        {
            return Some(p);
        }
    }
    resolve_path(&root.join("index"), opts)
}

/// `exports` or `imports` (a target, conditions, or subpaths) for subpath
/// `key`.
fn exports_target(exports: &serde_json::Value, key: &str, opts: &ResolveOptions) -> Option<String> {
    use serde_json::Value;
    let subpaths = matches!(exports, Value::Object(m) if m.keys().all(|k| k.starts_with('.') || k.starts_with('#')));
    if !subpaths {
        return if key == "." { conditional(exports, None, opts) } else { None };
    }
    let Value::Object(map) = exports else { return None };
    if let Some(v) = map.get(key) {
        return conditional(v, None, opts);
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
    conditional(v, Some(&star), opts)
}

/// A target of `exports`: a path, conditions (the first that applies, in
/// the map's order), or alternatives (the first that resolves).
fn conditional(v: &serde_json::Value, star: Option<&str>, opts: &ResolveOptions) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::String(s) => Some(match star {
            Some(m) => s.replace('*', m),
            None => s.clone(),
        }),
        Value::Object(m) => {
            m.iter().filter(|(k, _)| opts.conditions.iter().any(|c| c == *k)).find_map(|(_, v)| conditional(v, star, opts))
        }
        Value::Array(a) => a.iter().find_map(|v| conditional(v, star, opts)),
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
    resolve_target: &mut dyn FnMut(&str, Option<String>, bool) -> Result<Target, String>,
    library: bool,
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
        resolve_target(src.value.as_str(), attr, false).map_err(|e| format!("{}: {e}", at(src.span)))
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
                let types = types.into_iter().map(|t| t || d.import_kind.is_type()).collect();
                imports.push(Import { span: d.span, target, bindings, types });
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
    let nodes = unit.sem.nodes();
    let mut warnings = Vec::new();
    let mut dyn_externals = Vec::new();
    let mut failed = None;
    let dynamic_imports: Vec<DynImport> = dynamic
        .imports
        .into_iter()
        .map(|d| {
            let to = if d.phase {
                DynTo::Leave
            } else {
                match &d.spec {
                    Some((s, ty)) => match resolve_target(s, ty.clone(), true) {
                        Ok(Target::Module(k)) => DynTo::Module(k),
                        Ok(Target::External(e)) => {
                            if !library {
                                dyn_externals.push(e);
                            }
                            DynTo::Leave
                        }
                        Err(e) if library => {
                            failed.get_or_insert(format!("{}: {e}", at(d.span)));
                            DynTo::Leave
                        }
                        Err(e) => {
                            warnings.push(format!("{}: {e}: `import(\"{s}\")` is left to run time", at(d.span)));
                            DynTo::Fallback
                        }
                    },
                    // (A bundle's: what runs it resolves it.)
                    None if library => DynTo::Leave,
                    None => {
                        warnings.push(format!(
                            "{}: `import()` of a computed specifier: the module is loaded and interpreted at run time",
                            at(d.span)
                        ));
                        DynTo::Fallback
                    }
                }
            };
            let paren = unit.original[d.span.start as usize..d.source_start as usize].rfind('(').unwrap_or(6);
            DynImport {
                span: d.span,
                head: Span::new(d.span.start, d.span.start + paren as u32 + 1),
                scope: nodes.get_node(d.node).scope_id(),
                to,
            }
        })
        .collect();
    if let Some(e) = failed {
        return Err(e);
    }
    let metas: Vec<(Span, ScopeId)> =
        dynamic.metas.into_iter().map(|(span, node)| (span, nodes.get_node(node).scope_id())).collect();
    // An export name declared twice is an early error.
    let mut seen = HashSet::new();
    for (name, _) in &exports {
        if !seen.insert(name.as_str()) {
            return Err(format!("{}: duplicate export `{name}`", unit.path.display()));
        }
    }
    let unresolved = scoping.root_unresolved_references();
    let commonjs = imports.is_empty()
        && exports.is_empty()
        && stars.is_empty()
        && ["module", "exports", "require"].iter().any(|n| unresolved.contains_key(*n));
    unit.commonjs = commonjs;
    unit.imports = imports;
    unit.exports = exports;
    unit.stars = stars;
    unit.deps = deps;
    unit.deletions = deletions;
    unit.default_expr = default_expr;
    unit.reexports = reexports;
    unit.dynamic = dynamic_imports;
    // (A bundle's `import.meta` is that of the bundle where it runs.)
    unit.metas = if library { Vec::new() } else { metas };
    unit.tla = program.body.iter().any(has_tla);
    unit.warnings = warnings;
    unit.dyn_externals = dyn_externals;
    Ok(())
}

/// A module's `import(...)` expressions and `import.meta`s.
#[derive(Default)]
struct Dynamic {
    imports: Vec<DynSite>,
    metas: Vec<(Span, NodeId)>,
}

struct DynSite {
    span: Span,
    node: NodeId,
    source_start: u32,
    /// A string specifier, and the `type` of `{ with: { type } }` (none
    /// when the options are anything else: then it is left to run time).
    spec: Option<(String, Option<String>)>,
    /// `import.source(...)` or `import.defer(...)`.
    phase: bool,
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
        // Options other than `{ with: { type: "..." } }` (string values):
        // left to run time.
        let ty = match &it.options {
            None => Some(None),
            Some(o) => (|| {
                let Expression::ObjectExpression(o) = o.without_parentheses() else { return None };
                let [ObjectPropertyKind::ObjectProperty(p)] = o.properties.as_slice() else { return None };
                if p.computed || p.method || p.key.static_name().as_deref() != Some("with") {
                    return None;
                }
                let Expression::ObjectExpression(w) = p.value.without_parentheses() else { return None };
                let mut ty = None;
                for q in &w.properties {
                    let ObjectPropertyKind::ObjectProperty(q) = q else { return None };
                    let (false, false, Expression::StringLiteral(v)) = (q.computed, q.method, &q.value) else { return None };
                    if q.key.static_name().as_deref() == Some("type") {
                        ty = Some(v.value.to_string());
                    }
                }
                Some(ty)
            })(),
        };
        self.imports.push(DynSite {
            span: it.span,
            node: it.node_id.get(),
            source_start: it.source.span().start,
            spec: spec.zip(ty),
            phase: it.phase.is_some(),
        });
        walk::walk_import_expression(self, it);
    }

    fn visit_import_meta(&mut self, it: &ImportMeta) {
        self.metas.push((it.span, it.node_id.get()));
    }
}

/// Whether statement `stmt` (of a module's top level) awaits at the top
/// level.
fn has_tla(stmt: &Statement) -> bool {
    #[derive(Default)]
    struct Tla {
        found: bool,
        depth: u32,
    }
    impl<'a> Visit<'a> for Tla {
        fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
            self.found |= self.depth == 0;
            walk::walk_await_expression(self, it);
        }
        fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
            self.found |= it.r#await && self.depth == 0;
            walk::walk_for_of_statement(self, it);
        }
        fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
            self.found |= it.kind == VariableDeclarationKind::AwaitUsing && self.depth == 0;
            walk::walk_variable_declaration(self, it);
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
            // (Field initializers and static blocks are functions of their
            // own; computed keys and the heritage are not.)
            for d in &it.decorators {
                self.visit_decorator(d);
            }
            if let Some(h) = &it.heritage {
                self.visit_expression(&h.expression);
            }
            for el in &it.body.body {
                let key = match el {
                    ClassElement::MethodDefinition(m) if m.computed => Some(&m.key),
                    ClassElement::PropertyDefinition(p) if p.computed => Some(&p.key),
                    ClassElement::AccessorProperty(p) if p.computed => Some(&p.key),
                    _ => None,
                };
                if let Some(k) = key {
                    self.visit_property_key(k);
                }
            }
        }
    }
    let mut v = Tla::default();
    v.visit_statement(stmt);
    v.found
}

/// The source of a module of JSON, text or bytes: its content as a default
/// export.
fn synthesize(path: &Path, kind: Kind) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    synthesize_content(path, kind, bytes)
}

/// The source of module `path` of JSON, text or bytes whose content is
/// `bytes`: its content as a default export.
pub fn synthesize_content(path: &Path, kind: Kind, bytes: Vec<u8>) -> Result<String, String> {
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8 text", path.display()));
    Ok(match kind {
        Kind::Script => text(bytes)?,
        Kind::Json => {
            let text = text(bytes)?;
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| format!("{}: invalid JSON: {e}", path.display()))?;
            format!("export default JSON.parse({});\n", serde_json::to_string(&text).unwrap())
        }
        Kind::Text => format!("export default {};\n", serde_json::to_string(&text(bytes)?).unwrap()),
        Kind::Bytes => {
            let list: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
            format!("export default new Uint8Array([{}]);\n", list.join(","))
        }
    })
}

/// Build the bundle of the program whose entry module is `entry`.
pub fn bundle(entry: &Path) -> Result<Bundle, String> {
    bundle_with(entry, &Embedding::default())
}

/// Build the bundle of the program whose entry module is `entry`, with what
/// an embedder adds (`Embedding`).
pub fn bundle_with(entry: &Path, emb: &Embedding) -> Result<Bundle, String> {
    bundle_host(entry, emb, &mut FsHost { emb })
}

/// Build the bundle of the program whose entry module is `entry`, its
/// modules resolved and loaded by `host`.
pub fn bundle_host(entry: &Path, emb: &Embedding, host: &mut dyn Host) -> Result<Bundle, String> {
    let allocator = Allocator::default();
    let mut units: Vec<Unit> = Vec::new();
    let mut index: HashMap<(PathBuf, Kind), usize> = HashMap::new();
    let mut packages: HashMap<PathBuf, Option<serde_json::Value>> = HashMap::new();
    let mut queue = vec![(entry.to_path_buf(), Kind::Script)];
    index.insert((entry.to_path_buf(), Kind::Script), 0);
    // (The entry's own path, canonical, is its index too.)
    if let Ok(c) = entry.canonicalize() {
        index.insert((c, Kind::Script), 0);
    }
    // The preludes: modules 1, 2, ...
    let mut preludes = Vec::new();
    for p in &emb.preludes {
        let p = p.canonicalize().map_err(|e| format!("reading {}: {e}", p.display()))?;
        let n = queue.len();
        let k = *index.entry((p.clone(), Kind::Script)).or_insert(n);
        if k == n {
            queue.push((p, Kind::Script));
        }
        preludes.push((Target::Module(k), true));
    }
    let mut i = 0;
    while i < queue.len() {
        let (path, kind) = queue[i].clone();
        let mut original = host.load(&path, kind)?;
        // The program's own modules (not the embedder's): its globals are
        // its bindings.
        let own = kind == Kind::Script
            && (i == 0 || i > emb.preludes.len())
            && !emb.module_dirs.iter().any(|(_, d)| d.canonicalize().is_ok_and(|d| path.starts_with(d)));
        let mut pass = 0;
        let (program, ret, mut errors) = loop {
            let stripped = allocator.alloc_str(&marker::strip(&original).source);
            let parsed = Parser::new(&allocator, stripped, SourceType::ts().with_module(true)).parse();
            let errors: Vec<OxcDiagnostic> = parsed.diagnostics.into_iter().collect();
            let program: &Program = allocator.alloc(parsed.program);
            let ret = SemanticBuilder::new().with_build_nodes(true).with_check_syntax_error(true).build(program);
            pass += 1;
            if own && pass == 1 && errors.is_empty() && !emb.globals.is_empty()
                && let Some(more) = global_imports(&ret.semantic, emb)
            {
                original.push_str(&more);
                continue;
            }
            break (program, ret, errors);
        };
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
            commonjs: false,
            pure: side_effect_free(&path, &mut packages),
            reexports: Vec::new(),
            errors,
            dynamic: Vec::new(),
            metas: Vec::new(),
            tla: false,
            warnings: Vec::new(),
            dyn_externals: Vec::new(),
        };
        let mut resolve_target = |spec: &str, attr: Option<String>, dynamic: bool| -> Result<Target, String> {
            Ok(match host.resolve(&path, spec, dynamic, attr.as_deref())? {
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
            records(&mut unit, program, &mut resolve_target, emb.library)?;
        }
        units.push(unit);
        i += 1;
    }
    if let Some(entry) = units.first_mut() {
        entry.deps.splice(0..0, preludes);
    }
    // One module: as written (the compiler reports its errors).
    if units.len() == 1
        && units[0].imports.iter().all(|i| matches!(i.target, Target::External(_)))
        && units[0].dynamic.iter().all(|d| d.to == DynTo::Leave)
        && units[0].dyn_externals.is_empty()
        && units[0].metas.is_empty()
    {
        let u = units.pop().unwrap();
        return Ok(single(&u.path, u.original));
    }
    let shared: Vec<PathBuf> = emb.shared.iter().map(|d| d.canonicalize().unwrap_or_else(|_| d.clone())).collect();
    let mut b = link(&units, emb.library, &shared)?;
    if std::env::var_os("LUMINOUS_DEBUG_BUNDLE").is_some() {
        eprintln!("{}", b.source);
    }
    let link_warnings = std::mem::take(&mut b.warnings);
    b.warnings = units.iter().flat_map(|u| u.warnings.iter().cloned()).chain(link_warnings).collect();
    Ok(b)
}

fn single(path: &Path, original: String) -> Bundle {
    let end = original.len() as u32;
    Bundle {
        source: original.clone(),
        modules: vec![ModuleSource { path: path.to_path_buf(), original, start: 0, end, first_line: 1, map: Vec::new() }],
        warnings: Vec::new(),
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
    /// Modules `import()` imports: the function that evaluates the module
    /// (if it is lazy) and returns a promise of its namespace object.
    loads: HashMap<usize, String>,
    /// Lazily evaluated modules: the function that evaluates one, and the
    /// functions of its runs (see `LazyPlan`).
    inits: HashMap<usize, String>,
    runs: HashMap<usize, Vec<String>>,
    /// Modules with `import.meta`: the function that returns it.
    metas: HashMap<usize, String>,
    /// The function that has the engine load what `import()` imports at run
    /// time (warning that it does).
    fallback: Option<String>,
}

/// How a lazily evaluated module (one that `import()` imports, not
/// otherwise run) is laid out: its top-level statements that run no code
/// (declarations of functions, of classes that run no code, of variables
/// with literals or functions as values, of types...) stay declarations of
/// the program, evaluated when it starts, so they are compiled as those of
/// the other modules; runs of the others become functions that the
/// module's evaluation calls, the variables and classes they declare
/// declared before them (`let x;`, and `x = ...` in their place).
struct LazyPlan {
    /// The run each top-level statement is in (none: it stays a declaration
    /// of the program).
    run_of: Vec<Option<usize>>,
    /// Each run: its first and last statement, and whether it awaits.
    runs: Vec<(usize, usize, bool)>,
}

/// What a run of a lazily evaluated module declares before it: `let` or
/// `var`, the name, and the type annotation's span.
type Hoist = (&'static str, String, Option<Span>);

/// The JavaScript globals the code this adds refers to: no module's
/// top-level binding may take their names.
const GLOBALS_USED: [&str; 7] = ["Object", "Proxy", "Reflect", "Symbol", "TypeError", "JSON", "Uint8Array"];

/// Order, name and merge the modules.
fn link(units: &[Unit], library: bool, shared: &[PathBuf]) -> Result<Bundle, String> {
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
    // Modules that an `import()` of a running module imports, and their
    // dependencies, not run otherwise: evaluated lazily (in the same depth
    // first order) when an `import()` of one runs.
    let mut lazy_order = Vec::new();
    let mut i = 0;
    while i < order.len() + lazy_order.len() {
        let m = if i < order.len() { order[i] } else { lazy_order[i - order.len()] };
        i += 1;
        for d in &units[m].dynamic {
            if let DynTo::Module(t) = d.to
                && state[t] == 0
            {
                visit(t, units, &mut state, &mut lazy_order);
            }
        }
    }
    let mut lazy = vec![false; units.len()];
    for &m in &lazy_order {
        lazy[m] = true;
    }

    // A module's errors (early errors, and imports or re-exports that do not
    // resolve, below) are the program's; but those of a lazy module (and of
    // the lazy modules that import it) make `import()`s of it reject.
    let mut broken: HashMap<usize, String> = HashMap::new();
    let mut warnings = Vec::new();
    for (m, u) in units.iter().enumerate() {
        let err = match u.errors.first() {
            Some(d) => {
                let pos = d.labels.first().map_or(0, |l| l.offset() as u32);
                Some(format!("{}: {}", loc(&u.path, &u.original, pos), d.message))
            }
            None => u.unsupported.clone(),
        };
        match err {
            Some(e) if lazy[m] => {
                broken.insert(m, e);
            }
            Some(_) if !u.errors.is_empty() => {
                return Err(crate::erase::render(u.errors.clone(), &u.original, &u.path));
            }
            Some(e) => return Err(e),
            None => {}
        }
    }
    let mut fail = |m: usize, e: String| -> Result<(), String> {
        if lazy[m] {
            broken.entry(m).or_insert(e);
            Ok(())
        } else {
            Err(e)
        }
    };

    // `export { a } from` must resolve, used or not (as ESM links them).
    for (m, u) in units.iter().enumerate() {
        for (span, t, name) in &u.reexports {
            if resolve_export(units, *t, name, &mut Vec::new()).is_none() {
                fail(
                    m,
                    format!(
                        "{}: module `{}` has no export `{name}` (or more than one through `export *`)",
                        loc(&u.path, &u.original, span.start),
                        units[*t].path.display()
                    ),
                )?;
            }
        }
    }

    // What each import binding resolves to.
    let mut resolved: HashMap<(usize, SymbolId), Binding> = HashMap::new();
    for (m, u) in units.iter().enumerate() {
        for imp in &u.imports {
            let Target::Module(t) = imp.target else { continue };
            for (k, (sym, name)) in imp.bindings.iter().enumerate() {
                let b = match name {
                    Name::Namespace => Binding::Namespace(t),
                    Name::Named(n) => match resolve_export(units, t, n, &mut Vec::new()) {
                        Some(b) => b,
                        // (A type of a JavaScript module: its references are
                        // types, which are erased.)
                        None if imp.types[k] => continue,
                        None if units[t].commonjs => {
                            fail(
                                m,
                                format!(
                                    "{}: `{}` is a CommonJS module (`module.exports`, `require`), which is not supported yet",
                                    loc(&u.path, &u.original, imp.span.start),
                                    units[t].path.display()
                                ),
                            )?;
                            continue;
                        }
                        None => {
                            fail(
                                m,
                                format!(
                                    "{}: module `{}` has no export `{n}` (or more than one through `export *`)",
                                    loc(&u.path, &u.original, imp.span.start),
                                    units[t].path.display()
                                ),
                            )?;
                            continue;
                        }
                    },
                };
                resolved.insert((m, *sym), b);
            }
        }
    }
    // (A module that imports a broken one is.)
    loop {
        let mut changed = false;
        for &m in &lazy_order {
            if broken.contains_key(&m) {
                continue;
            }
            let dep = units[m].deps.iter().find_map(|(t, _)| match t {
                Target::Module(d) => broken.get(d).cloned(),
                _ => None,
            });
            if let Some(e) = dep {
                broken.insert(m, e);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    resolved.retain(|&(m, _), _| !broken.contains_key(&m));
    lazy_order.retain(|m| !broken.contains_key(m));
    for (&m, e) in &broken {
        lazy[m] = false;
        warnings.push(format!("{e}: `import()` of `{}` rejects", units[m].path.display()));
    }
    warnings.sort();
    // Modules whose package says they have no side effects run only if the
    // program uses what they declare (a binding, or the namespace object
    // that holds it): as bundlers do, a package's `index` that re-exports
    // all of its modules brings only those the program uses.
    // A library's exports: those of its entry that are values.
    let mut entry_exports: Vec<(String, Binding)> = Vec::new();
    if library {
        for n in export_names(units, 0) {
            let Some(b) = resolve_export(units, 0, &n, &mut Vec::new()) else {
                return Err(format!("{}: export `{n}` does not resolve", units[0].path.display()));
            };
            if let Binding::Symbol(k, sym) = b
                && !units[k].sem.scoping().symbol_flags(sym).is_value()
            {
                continue;
            }
            entry_exports.push((n, b));
        }
    }
    // (A library is shaken statement by statement.)
    let (keep, removed) = if library {
        let s = shake::shake(units, &order, &lazy_order, &resolved, &entry_exports);
        (s.keep, s.removed)
    } else {
        (used_modules(units, &order, &lazy_order, &resolved, &entry_exports), vec![Vec::new(); units.len()])
    };
    order.retain(|&m| keep[m]);
    lazy_order.retain(|&m| keep[m]);
    for (m, l) in lazy.iter_mut().enumerate() {
        *l &= keep[m];
    }
    let running: Vec<usize> = order.iter().chain(&lazy_order).copied().collect();
    // Modules imported for their types only: their type declarations (as
    // of those that are dropped).
    let typed_only: Vec<usize> =
        (0..units.len()).filter(|&m| state[m] == 0 || (!keep[m] && !broken.contains_key(&m))).collect();
    // (Lazy modules first: what they declare is in place when an `import()`
    // runs.)
    let all: Vec<usize> = typed_only.iter().chain(&lazy_order).chain(&order).copied().collect();

    // How lazy modules are laid out, and which of them are async (they or
    // their lazy dependencies await at the top level).
    let mut plans: HashMap<usize, LazyPlan> = HashMap::new();
    let mut top_classes: HashSet<(usize, SymbolId)> = HashSet::new();
    for &m in &lazy_order {
        plans.insert(m, lazy_plan(units, m, &resolved, &mut top_classes)?);
    }
    let mut is_async = vec![false; units.len()];
    for &m in &lazy_order {
        is_async[m] = units[m].tla;
    }
    loop {
        let mut changed = false;
        for &m in &lazy_order {
            if !is_async[m]
                && units[m].deps.iter().any(|(t, rt)| *rt && matches!(t, Target::Module(d) if lazy[*d] && is_async[*d]))
            {
                is_async[m] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // The modules `import()`s import, and the scopes of those `import()`s;
    // those left to run time.
    let mut loads: Vec<usize> = Vec::new();
    let mut load_sites: HashMap<usize, Vec<(usize, ScopeId)>> = HashMap::new();
    let mut fallback_sites: Vec<(usize, ScopeId)> = Vec::new();
    for &m in &running {
        for d in &units[m].dynamic {
            match d.to {
                DynTo::Module(t) => {
                    if !loads.contains(&t) {
                        loads.push(t);
                    }
                    load_sites.entry(t).or_default().push((m, d.scope));
                }
                DynTo::Fallback => fallback_sites.push((m, d.scope)),
                DynTo::Leave => {}
            }
        }
    }

    // Namespace objects that are needed: of namespace imports used other
    // than as `ns.x` (with their references' scopes), those `import()`
    // returns, and those they hold.
    let mut ns_sites: HashMap<usize, Vec<(usize, ScopeId)>> = HashMap::new();
    // (And where `ns.x` stands for a binding: a scope there must not have
    // the binding's name.)
    let mut member_sites: HashMap<Binding, Vec<(usize, ScopeId)>> = HashMap::new();
    let mut writes: Vec<(usize, SymbolId)> = Vec::new();
    for (&(m, sym), &b) in &resolved {
        if !keep[m] {
            continue;
        }
        let u = &units[m];
        for r in u.sem.scoping().get_resolved_references(sym) {
            if r.flags().is_write() && !writes.contains(&(m, sym)) {
                writes.push((m, sym));
            }
            let Binding::Namespace(t) = b else { continue };
            match member_of_namespace(units, u.sem.nodes(), r.node_id(), t) {
                Some((_, Binding::Namespace(k))) => ns_sites.entry(k).or_default().push((m, u.ref_scope(r.node_id()))),
                Some((_, b)) => member_sites.entry(b).or_default().push((m, u.ref_scope(r.node_id()))),
                None => ns_sites.entry(t).or_default().push((m, u.ref_scope(r.node_id()))),
            }
        }
    }
    writes.sort();
    let mut needed: Vec<usize> = ns_sites.keys().copied().collect();
    // A program that imports modules at run time: its modules, by path, for
    // the embedder's loader to give them (one instance of each module).
    let registry: Vec<usize> = if fallback_sites.is_empty() {
        order
            .iter()
            .copied()
            .filter(|&t| {
                let p = units[t].path.canonicalize().unwrap_or_else(|_| units[t].path.clone());
                shared.iter().any(|d| p.starts_with(d))
            })
            .collect()
    } else {
        order.clone()
    };
    for &t in &registry {
        if !needed.contains(&t) {
            needed.push(t);
        }
    }
    for (_, b) in &entry_exports {
        if let Binding::Namespace(t) = *b
            && !needed.contains(&t)
        {
            needed.push(t);
        }
    }
    for &t in &loads {
        if !needed.contains(&t) && !broken.contains_key(&t) {
            needed.push(t);
        }
    }
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
    if loads.iter().any(|t| broken.contains_key(t)) {
        taken.insert("SyntaxError".into());
    }
    if !fallback_sites.is_empty() {
        taken.extend(["console", "Set", "JSON", "Map", "globalThis"].map(String::from));
    }
    if units.iter().any(|u| !u.metas.is_empty()) {
        taken.extend(["globalThis", "URL"].map(String::from));
    }
    for u in units {
        let scoping = u.sem.scoping();
        for name in scoping.root_unresolved_references().keys() {
            taken.insert(name.to_string());
        }
        // (`declare const x`: the global `x`.)
        for (name, &sym) in scoping.get_bindings(u.root()).iter() {
            if scoping.symbol_flags(sym).is_ambient() {
                taken.insert(name.to_string());
            }
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
        v.extend(member_sites.get(&b).into_iter().flatten().copied());
        for &(m, sym) in importers.get(&b).map(|v| v.as_slice()).unwrap_or(&[]) {
            let u = &units[m];
            v.extend(u.sem.scoping().get_resolved_references(sym).map(|r| (m, u.ref_scope(r.node_id()))));
        }
        v
    };
    let mut names = Names {
        of: HashMap::new(),
        writes: HashMap::new(),
        loads: HashMap::new(),
        inits: HashMap::new(),
        runs: HashMap::new(),
        metas: HashMap::new(),
        fallback: None,
    };
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
            // An ambient declaration names a global: its references stay.
            if scoping.symbol_flags(sym).is_ambient() {
                names.of.insert(b, name.to_string());
                continue;
            }
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
    for &m in &lazy_order {
        let s = stem(&units[m].path);
        let n = pick(&format!("{s}_init"), &[], &taken);
        taken.extend([format!("{n}$s"), format!("{n}$e"), format!("{n}$p"), n.clone()]);
        names.inits.insert(m, n);
        let mut runs = Vec::new();
        for k in 0..plans[&m].runs.len() {
            let n = pick(&format!("{s}_run{}", k + 1), &[], &taken);
            taken.insert(n.clone());
            runs.push(n);
        }
        names.runs.insert(m, runs);
    }
    for &t in &loads {
        let n = pick(&format!("{}_load", stem(&units[t].path)), &load_sites[&t], &taken);
        taken.insert(n.clone());
        names.loads.insert(t, n);
    }
    for &m in &running {
        let u = &units[m];
        if !u.metas.is_empty() {
            let sites: Vec<(usize, ScopeId)> = u.metas.iter().map(|&(_, s)| (m, s)).collect();
            let n = pick(&format!("{}_meta", stem(&u.path)), &sites, &taken);
            taken.extend([format!("{n}$"), n.clone()]);
            names.metas.insert(m, n);
        }
    }
    if !fallback_sites.is_empty() {
        let n = pick("__luminous_import", &fallback_sites, &taken);
        taken.extend([format!("{n}$w"), n.clone()]);
        names.fallback = Some(n);
    }

    // The text of each module, edited, in order; what must run before any
    // of them (`.name` of anonymous default functions, which are hoisted)
    // is put first, on the first line.
    let mut source = String::new();
    let mut modules = Vec::new();
    let mut line = 1u32;
    let mut prelude = String::new();
    // (A module the engine provides is linked when a static import imports
    // it.)
    let mut externals: Vec<&String> = running.iter().flat_map(|&m| &units[m].dyn_externals).collect();
    externals.sort();
    externals.dedup();
    for e in externals {
        prelude.push_str(&format!("import {}; ", serde_json::to_string(e).unwrap()));
    }
    for &m in &all {
        let u = &units[m];
        let typed = typed_only.contains(&m);
        // (Not if tree shaking left the function out.)
        let shaken = u.default_expr.is_some_and(|d| {
            let i = u.program.body.partition_point(|s| s.span().end <= d.prefix.start);
            removed[m].get(i) == Some(&true)
        });
        if let Some(d) = u.default_expr
            && !typed
            && !shaken
            && d.anonymous == Anon::Function
            && d.name_at.is_some()
        {
            prelude.push_str(&format!(
                "Object.defineProperty({}, \"name\", {{ value: \"default\" }}); ",
                names.of[&Binding::Default(m)]
            ));
        }
    }
    if !registry.is_empty() {
        let entries: Vec<String> = registry
            .iter()
            .map(|&t| {
                let path = units[t].path.canonicalize().unwrap_or_else(|_| units[t].path.clone());
                format!("[{}, {}]", serde_json::to_string(&path.display().to_string()).unwrap(), names.of[&Binding::Namespace(t)])
            })
            .collect();
        prelude.push_str(&format!("globalThis[Symbol.for(\"luminous.modules\")] = new Map([{}]); ", entries.join(", ")));
    }
    source.push_str(&prelude);
    for &m in &all {
        let u = &units[m];
        let mut edits = edits(units, m, &resolved, &names, typed_only.contains(&m), &int_aliases, plans.get(&m), &removed[m])?;
        // (An edit inside a removed span is dropped: the longer first, an
        // insertion before what starts where it is.)
        edits.sort_by_key(|e| (e.0.start, e.0.end != e.0.start, std::cmp::Reverse(e.0.end)));
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
    // `import()` of a module of the program: a promise of its namespace
    // object, once it is evaluated (in a later job, as the engine's).
    // (An async function's promise is the intrinsic one, whatever the
    // global `Promise` is.)
    for &t in &loads {
        let body = match (broken.get(&t), names.inits.get(&t)) {
            (Some(e), _) => format!("throw new SyntaxError({});", serde_json::to_string(e).unwrap()),
            (None, Some(init)) => format!("await null; await {init}(); return {}();", names.of[&Binding::Namespace(t)]),
            (None, None) => format!("await null; return {}();", names.of[&Binding::Namespace(t)]),
        };
        source.push_str(&format!("function {}() {{ return (async () => {{ {body} }})(); }}\n", names.loads[&t]));
    }
    // The evaluation of a lazy module: its lazy dependencies', then its
    // runs, once (an error is that of every later evaluation; one in a cycle
    // that is being evaluated returns). (No initializers: an `import()` can
    // run before the end of the program is reached.)
    for &m in &lazy_order {
        let init = &names.inits[&m];
        let mut deps: Vec<usize> = Vec::new();
        for (t, runtime) in &units[m].deps {
            if let (Target::Module(d), true) = (t, runtime)
                && lazy[*d]
                && !deps.contains(d)
            {
                deps.push(*d);
            }
        }
        let mut calls = String::new();
        for d in deps {
            let aw = if is_async[d] { "await " } else { "" };
            calls.push_str(&format!("{aw}{}(); ", names.inits[&d]));
        }
        for (k, run) in names.runs[&m].iter().enumerate() {
            let aw = if plans[&m].runs[k].2 { "await " } else { "" };
            calls.push_str(&format!("{aw}{run}(); "));
        }
        if is_async[m] {
            source.push_str(&format!(
                "var {init}$s, {init}$p; function {init}() {{ if ({init}$p) return {init}$p; if ({init}$s) return; {init}$s = 1; \
                 return {init}$p = (async () => {{ {calls}}})(); }}\n"
            ));
        } else {
            source.push_str(&format!(
                "var {init}$s, {init}$e; function {init}() {{ if ({init}$s) {{ if ({init}$s === 3) throw {init}$e; return; }} {init}$s = 1; \
                 try {{ {calls}}} catch (e) {{ {init}$s = 3; {init}$e = e; throw e; }} {init}$s = 2; }}\n"
            ));
        }
    }
    // `import.meta`: an object with a null prototype, made on first use.
    for (&m, name) in &names.metas {
        let path = units[m].path.canonicalize().unwrap_or_else(|_| units[m].path.clone());
        let dir = path.parent().unwrap_or(Path::new("/"));
        let json = |s: String| serde_json::to_string(&s).unwrap();
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        // (`resolve` through the embedder's resolver if it has one.)
        source.push_str(&format!(
            "var {name}$; function {name}() {{ return {name}$ || ({name}$ = Object.assign(Object.create(null), \
             {{ url: {url}, filename: {filename}, dirname: {dirname}, dir: {dirname}, file: {file}, path: {filename}, main: {main}, \
             resolve(specifier) {{ const r = globalThis[Symbol.for(\"luminous.resolve\")]; \
             return r ? r(`${{specifier}}`, {filename}) : new URL(`${{specifier}}`, {url}).href; }} }})); }}\n",
            url = json(file_url(&path)),
            filename = json(path.display().to_string()),
            dirname = json(dir.display().to_string()),
            file = json(file),
            main = m == 0
        ));
    }
    // `import()` left to run time: the engine loads (and interprets) the
    // module, relative specifiers resolved against the importing module's
    // directory; the first of each specifier is warned of.
    if let Some(f) = &names.fallback {
        source.push_str(&format!(
            "var {f}$w; function {f}(base, specifier, options) {{ \
             let key; try {{ key = `${{specifier}}`; }} catch (e) {{ return (async () => {{ throw e; }})(); }} \
             const hook = globalThis[Symbol.for(\"luminous.import\")]; if (hook) return hook(key, base, options); \
             if (!({f}$w || ({f}$w = new Set())).has(key)) {{ {f}$w.add(key); \
             console.warn(\"Luminous: import(\" + JSON.stringify(key) + \") is not part of the compiled program: \
             the module is loaded and interpreted at run time\"); }} \
             if (key.startsWith(\"./\") || key.startsWith(\"../\")) key = base + key; \
             return import(key, options); }}\n"
        ));
    }
    if !entry_exports.is_empty() {
        let mut specs = Vec::new();
        for (n, b) in &entry_exports {
            let local = match b {
                Binding::Namespace(t) => {
                    // (A namespace object made once, as a binding.)
                    let f = &names.of[&Binding::Namespace(*t)];
                    let v = format!("{f}$export");
                    source.push_str(&format!("var {v} = {f}();\n"));
                    v
                }
                b => names.of[b].clone(),
            };
            let exported = if is_identifier_name(n) { n.clone() } else { serde_json::to_string(n).unwrap() };
            specs.push(if &local == n { local } else { format!("{local} as {exported}") });
        }
        source.push_str(&format!("export {{ {} }};\n", specs.join(", ")));
    }
    Ok(Bundle { source, modules, warnings })
}

/// Whether `s` can be an export name as is (else it is a string).
fn is_identifier_name(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && cs.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// The `file:` URL of an absolute path.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for b in path.display().to_string().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// Which modules the program keeps: those that run (`order`, `lazy_order`)
/// but have no side effects (`Unit::pure`) and declare nothing the kept
/// modules use, nor anything of a namespace object they use, are dropped.
fn used_modules(
    units: &[Unit],
    order: &[usize],
    lazy_order: &[usize],
    resolved: &HashMap<(usize, SymbolId), Binding>,
    exported: &[(String, Binding)],
) -> Vec<bool> {
    let mut keep = vec![false; units.len()];
    let mut work = Vec::new();
    for &m in order.iter().chain(lazy_order) {
        if m == 0 || !units[m].pure {
            keep[m] = true;
            work.push(m);
        }
    }
    let mut materialized = HashSet::new();
    fn mark(k: usize, keep: &mut [bool], work: &mut Vec<usize>) {
        if !keep[k] {
            keep[k] = true;
            work.push(k);
        }
    }
    // (A namespace object holds every export of its module.)
    fn materialize(t: usize, units: &[Unit], keep: &mut [bool], work: &mut Vec<usize>, done: &mut HashSet<usize>) {
        if !done.insert(t) {
            return;
        }
        for n in export_names(units, t) {
            match resolve_export(units, t, &n, &mut Vec::new()) {
                Some(Binding::Symbol(k, _) | Binding::Default(k)) => mark(k, keep, work),
                Some(Binding::Namespace(k)) => materialize(k, units, keep, work, done),
                None => {}
            }
        }
    }
    // (What a library exports is used.)
    for (_, b) in exported {
        match *b {
            Binding::Symbol(k, _) | Binding::Default(k) => mark(k, &mut keep, &mut work),
            Binding::Namespace(t) => materialize(t, units, &mut keep, &mut work, &mut materialized),
        }
    }
    while let Some(m) = work.pop() {
        let u = &units[m];
        for d in &u.dynamic {
            if let DynTo::Module(t) = d.to {
                mark(t, &mut keep, &mut work);
                materialize(t, units, &mut keep, &mut work, &mut materialized);
            }
        }
        let scoping = u.sem.scoping();
        for imp in &u.imports {
            for (sym, _) in &imp.bindings {
                let Some(&b) = resolved.get(&(m, *sym)) else { continue };
                for r in scoping.get_resolved_references(*sym) {
                    if !r.flags().is_value() && !r.flags().is_write() {
                        continue;
                    }
                    match b {
                        Binding::Symbol(k, _) | Binding::Default(k) => mark(k, &mut keep, &mut work),
                        Binding::Namespace(t) => match member_of_namespace(units, u.sem.nodes(), r.node_id(), t) {
                            Some((_, Binding::Symbol(k, _) | Binding::Default(k))) => mark(k, &mut keep, &mut work),
                            Some((_, Binding::Namespace(k))) => {
                                materialize(k, units, &mut keep, &mut work, &mut materialized)
                            }
                            None => materialize(t, units, &mut keep, &mut work, &mut materialized),
                        },
                    }
                }
            }
        }
    }
    keep
}

/// The layout of lazily evaluated module `m` (see `LazyPlan`).
/// `top_classes`: the classes of earlier lazy modules (and then of this
/// one) that stay declarations, which a class that stays one may extend.
fn lazy_plan(
    units: &[Unit],
    m: usize,
    resolved: &HashMap<(usize, SymbolId), Binding>,
    top_classes: &mut HashSet<(usize, SymbolId)>,
) -> Result<LazyPlan, String> {
    let u = &units[m];
    let scoping = u.sem.scoping();
    let at = |span: Span| loc(&u.path, &u.original, span.start);
    let global = |id: &IdentifierReference| id.reference_id.get().is_some_and(|r| scoping.get_reference(r).symbol_id().is_none());
    let class_top = |c: &Class, top_classes: &mut HashSet<(usize, SymbolId)>| -> Result<bool, String> {
        if c.declare {
            return Ok(true);
        }
        if !c.decorators.is_empty() {
            return Err(format!("{}: decorators in a module that `import()` imports are not supported yet", at(c.span)));
        }
        let earlier = |id: &IdentifierReference| {
            let Some(sym) = id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id()) else { return false };
            let b = resolved.get(&(m, sym)).copied().unwrap_or(Binding::Symbol(m, sym));
            matches!(b, Binding::Symbol(k, s) if top_classes.contains(&(k, s)))
        };
        let top = crate::check::pure_class(c, &earlier, &global);
        if top && let Some(id) = &c.id {
            top_classes.insert((m, id.symbol_id()));
        }
        Ok(top)
    };
    let decl_top = |d: &Declaration, top_classes: &mut HashSet<(usize, SymbolId)>| -> Result<bool, String> {
        Ok(match d {
            Declaration::VariableDeclaration(v) if v.declare => true,
            Declaration::VariableDeclaration(v) => {
                if matches!(v.kind, VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing) {
                    return Err(format!(
                        "{}: `using` at the top level of a module that `import()` imports is not supported yet",
                        at(v.span)
                    ));
                }
                v.declarations.iter().all(|d| {
                    matches!(d.id, BindingPattern::BindingIdentifier(_)) && d.init.as_ref().is_none_or(crate::check::pure_init)
                })
            }
            Declaration::ClassDeclaration(c) => class_top(c, top_classes)?,
            _ => true,
        })
    };
    let mut run_of = Vec::new();
    let mut runs: Vec<(usize, usize, bool)> = Vec::new();
    for (i, stmt) in u.program.body.iter().enumerate() {
        let top = match stmt {
            Statement::ExportDeclaration(e) => decl_top(&e.declaration, top_classes)?,
            Statement::ExportDefaultDeclaration(e) => match &e.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(_) | ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => true,
                ExportDefaultDeclarationKind::ClassDeclaration(c) => class_top(c, top_classes)?,
                other => {
                    other.as_expression().is_some_and(crate::check::pure_init)
                        && u.default_expr.is_some_and(|d| d.anonymous == Anon::No)
                }
            },
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::EmptyStatement(_) => true,
            s => match s.as_declaration() {
                Some(d) => decl_top(d, top_classes)?,
                None => false,
            },
        };
        if top {
            run_of.push(None);
            continue;
        }
        let tla = has_tla(stmt);
        match runs.last_mut() {
            Some((_, last, a)) if *last + 1 == i => {
                *last = i;
                *a |= tla;
            }
            _ => runs.push((i, i, tla)),
        }
        run_of.push(Some(runs.len() - 1));
    }
    Ok(LazyPlan { run_of, runs })
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
    plan: Option<&LazyPlan>,
    removed: &[bool],
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
    // (A hashbang but at the start of the program is an error.)
    if let Some(h) = &program.hashbang {
        out.push(blank(h.span));
    }
    if !typed_only {
        for d in &u.dynamic {
            match d.to {
                DynTo::Module(t) => out.push((d.span, format!("{}()", names.loads[&t]))),
                DynTo::Fallback => {
                    let path = u.path.canonicalize().unwrap_or_else(|_| u.path.clone());
                    let mut base = path.parent().unwrap_or(Path::new("/")).display().to_string();
                    if !base.ends_with('/') {
                        base.push('/');
                    }
                    let f = names.fallback.as_ref().unwrap();
                    out.push((d.head, format!("{f}({}, ", serde_json::to_string(&base).unwrap())));
                }
                DynTo::Leave => {}
            }
        }
        for &(span, _) in &u.metas {
            out.push((span, format!("{}()", names.metas[&m])));
        }
    }
    // A lazy module's runs: what each declares before it.
    let mut hoists: Vec<Vec<Hoist>> = plan.map(|p| vec![Vec::new(); p.runs.len()]).unwrap_or_default();
    let run_at = |pos: u32| -> Option<usize> {
        let p = plan?;
        p.runs.iter().position(|&(a, b, _)| program.body[a].span().start <= pos && pos < program.body[b].span().end)
    };
    if let Some(d) = u.default_expr
        && !typed_only
    {
        let name = &names.of[&Binding::Default(m)];
        let mut after = String::new();
        // (In a run of a lazy module: assigned to the binding declared
        // before the run.)
        let run = run_at(d.prefix.start);
        if let Some(r) = run {
            hoists[r].push(("let", name.clone(), None));
        }
        match (d.name_at, run) {
            (Some(at), None) => {
                out.push(blank(d.prefix));
                out.push((Span::new(at, at), format!(" {name}")));
            }
            (Some(at), Some(_)) => {
                out.push((d.prefix, format!(";{name} = ")));
                out.push((Span::new(at, at), format!(" {name}")));
                after.push(';');
            }
            (None, run) => {
                let decl = if run.is_some() { ";" } else { "const " };
                out.push((d.prefix, format!("{decl}{name} = ")));
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
    // Declarations tree shaking left out (a library's).
    for (i, stmt) in program.body.iter().enumerate() {
        if removed.get(i) == Some(&true) {
            out.push(blank(stmt.span()));
        }
    }
    let removed_at = |pos: u32| -> bool {
        let i = program.body.partition_point(|s| s.span().end <= pos);
        removed.get(i) == Some(&true) && program.body[i].span().start <= pos
    };
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
                // (A module of types only keeps none of its code.)
                if typed_only && !r.flags().is_type() {
                    continue;
                }
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
                        // (A type the module's JavaScript does not have: erased.)
                        None if r.flags().is_type() => continue,
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
        // A class's or function's `name` comes from its binding: kept as
        // written (`class Event$1` is still named "Event"). (Not in a module
        // imported for its types: its declarations are gone.)
        if plan.is_none()
            && !typed_only
            && !removed_at(scoping.symbol_span(sym).start)
            && let Some(end) = named_by_binding(nodes, scoping.symbol_declaration(sym))
        {
            out.push((
                Span::new(end, end),
                format!(";Object.defineProperty({new}, \"name\", {{ value: {old:?}, configurable: true }});"),
            ));
        }
    }
    if let Some(plan) = plan {
        lazy_edits(u, m, names, plan, &mut hoists, &mut out)?;
    }
    Ok(out)
}

/// If the declaration `decl` gives a function or class its `name` (a class
/// or function declaration, or a variable initialized with an anonymous
/// function, arrow function or class), where the statement declaring it
/// ends.
fn named_by_binding(nodes: &oxc::semantic::AstNodes, decl: NodeId) -> Option<u32> {
    match nodes.get_node(decl).kind() {
        AstKind::Class(c) if c.is_declaration() => Some(c.span.end),
        AstKind::Function(f) if f.is_declaration() => Some(f.span.end),
        AstKind::VariableDeclarator(d) => {
            let named = match d.init.as_ref().map(|e| e.without_parentheses()) {
                Some(Expression::ArrowFunctionExpression(_)) => true,
                Some(Expression::FunctionExpression(f)) => f.id.is_none(),
                Some(Expression::ClassExpression(c)) => c.id.is_none(),
                _ => false,
            };
            if !named {
                return None;
            }
            match nodes.parent_kind(decl) {
                AstKind::VariableDeclaration(v) => Some(v.span.end),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The edits that lay lazily evaluated module `m` out as `plan` says: the
/// variables (top-level ones, and `var`s anywhere at the top level) and
/// classes that runs declare are declared before the run, and assigned
/// where they were declared; each run is a function.
fn lazy_edits(
    u: &Unit,
    m: usize,
    names: &Names,
    plan: &LazyPlan,
    hoists: &mut [Vec<Hoist>],
    out: &mut Vec<(Span, String)>,
) -> Result<(), String> {
    let scoping = u.sem.scoping();
    let nodes = u.sem.nodes();
    let body = &u.program.body;
    let text = &u.original;
    let root = u.root();
    let run_spans: Vec<Span> = plan.runs.iter().map(|&(a, b, _)| Span::new(body[a].span().start, body[b].span().end)).collect();
    let run_at = |span: Span| run_spans.iter().position(|r| r.start <= span.start && span.end <= r.end);
    let name_of = |sym: SymbolId| names.of.get(&Binding::Symbol(m, sym)).cloned().unwrap_or_else(|| scoping.symbol_name(sym).to_string());
    for node in nodes.iter() {
        let AstKind::VariableDeclaration(v) = node.kind() else { continue };
        let Some(r) = run_at(v.span) else { continue };
        let Some(first) = v.declarations.first().and_then(|d| d.id.get_binding_identifiers().into_iter().next()) else { continue };
        if v.declare || scoping.symbol_scope_id(first.symbol_id()) != root {
            continue;
        }
        let (kw, hoist_kw) = match v.kind {
            VariableDeclarationKind::Var => (3, "var"),
            VariableDeclarationKind::Let => (3, "let"),
            VariableDeclarationKind::Const => (5, "let"),
            _ => return Err(format!("{}: `using` here is not supported yet", loc(&u.path, text, v.span.start))),
        };
        enum Ctx {
            List,
            Single,
            ForInit,
            ForLeft,
        }
        let ctx = match nodes.parent_kind(node.id()) {
            AstKind::Program(_) | AstKind::BlockStatement(_) | AstKind::SwitchCase(_) | AstKind::ExportDeclaration(_) => Ctx::List,
            AstKind::ForStatement(_) => Ctx::ForInit,
            AstKind::ForInStatement(_) | AstKind::ForOfStatement(_) => Ctx::ForLeft,
            _ => Ctx::Single,
        };
        // (A `;` first: what follows might continue the line before.)
        let lead = if matches!(ctx, Ctx::List) { ";" } else { "" };
        out.push((Span::new(v.span.start, v.span.start + kw), lead.to_string()));
        for d in &v.declarations {
            let id_end = d.id.span().end;
            if let Some(t) = &d.type_annotation {
                out.push((Span::new(id_end, t.span.end), String::new()));
            } else if d.definite {
                out.push((Span::new(id_end, id_end + 1), String::new()));
            }
            let annotation = match d.id {
                BindingPattern::BindingIdentifier(_) => d.type_annotation.as_ref().map(|t| t.span),
                _ => None,
            };
            for id in d.id.get_binding_identifiers() {
                hoists[r].push((hoist_kw, name_of(id.symbol_id()), annotation));
            }
            match (&d.init, &ctx) {
                (_, Ctx::ForLeft) => {}
                (None, _) => out.push((d.span, "void 0".into())),
                (Some(_), _) if !matches!(d.id, BindingPattern::BindingIdentifier(_)) => {
                    out.push((Span::new(d.span.start, d.span.start), "(".into()));
                    out.push((Span::new(d.span.end, d.span.end), ")".into()));
                }
                (Some(_), _) => {}
            }
        }
    }
    // Classes: assigned a class expression.
    for (i, stmt) in body.iter().enumerate() {
        let Some(r) = plan.run_of[i] else { continue };
        let class = match stmt {
            Statement::ClassDeclaration(c) => Some(c),
            Statement::ExportDeclaration(e) => match &e.declaration {
                Declaration::ClassDeclaration(c) => Some(c),
                _ => None,
            },
            Statement::ExportDefaultDeclaration(e) => match &e.declaration {
                ExportDefaultDeclarationKind::ClassDeclaration(c) => Some(c),
                _ => None,
            },
            _ => None,
        };
        if let Some(c) = class
            && let Some(id) = &c.id
            && !c.declare
        {
            let name = name_of(id.symbol_id());
            out.push((Span::new(c.span.start, c.span.start), format!(";{name} = ")));
            out.push((Span::new(c.span.end, c.span.end), ";".into()));
            hoists[r].push(("let", name, None));
        }
    }
    // The runs: what they declare (types as edited), then the function.
    let render = |span: Span, out: &[(Span, String)]| -> String {
        let mut es: Vec<&(Span, String)> = out.iter().filter(|(s, _)| span.start <= s.start && s.end <= span.end).collect();
        es.sort_by_key(|e| (e.0.start, e.0.end != e.0.start, std::cmp::Reverse(e.0.end)));
        let mut res = String::new();
        let mut pos = span.start as usize;
        for (s, rep) in es {
            if (s.start as usize) < pos {
                continue;
            }
            res.push_str(&text[pos..s.start as usize]);
            res.push_str(rep);
            pos = s.end as usize;
        }
        res.push_str(&text[pos..span.end as usize]);
        res
    };
    for (r, span) in run_spans.iter().enumerate() {
        let mut head = String::new();
        let mut seen = HashSet::new();
        for (kw, name, annotation) in &hoists[r] {
            // (A `var` declared again: once.)
            if !seen.insert(name.clone()) {
                continue;
            }
            let ty = annotation.map(|a| render(a, out)).unwrap_or_default();
            head.push_str(&format!("{kw} {name}{ty}; "));
        }
        let asy = if plan.runs[r].2 { "async " } else { "" };
        head.push_str(&format!("{asy}function {}() {{ ", names.runs[&m][r]));
        // (Before what else is inserted there.)
        out.insert(0, (Span::new(span.start, span.start), head));
        out.push((Span::new(span.end, span.end), " }".into()));
    }
    Ok(())
}
