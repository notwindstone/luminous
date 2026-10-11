//! Produces the interpreted half of a program: the whole module with types
//! erased and rewritten so that it shares state with native code.
//!
//! ```ts
//! let count = 0;
//! function fib(n: number): number { ... }
//! const sq = (x: number): number => x * x;
//! class P { norm(): number { ... } }
//! count++;
//! ```
//! becomes
//! ```js
//! import "tsaot:native"; import { __tsaot_bind, ... } from "tsaot:native";
//! const fib = __tsaot_bind(0, __tsaot_fb_fib);      // first statement
//! let count = __tsaot_init(0, 0);                    // cell initialized
//! function __tsaot_fb_fib(n) { ... }                 // the fallback
//! const sq = __tsaot_bind(1, (x) => x * x);
//! class P { norm() { ... } };__tsaot_method(2, P, 0, "norm");
//! __tsaot_m.count++;                                 // through the cell
//! ```
//! - `__tsaot_bind(i, fallback)` creates the function object of native
//!   function `i`, which calls `fallback` when the arguments do not match the
//!   declared types. Declarations are bound in the first statement of the
//!   module body, so no code can observe them before they are initialized.
//! - `__tsaot_method` replaces a class method the same way.
//! - Module bindings that native code uses live in cells: `__tsaot_init`
//!   stores the value when the declaration runs, and bindings that are
//!   reassigned are accessed through `__tsaot_m` everywhere.
//!
//! The bare import is emitted even without native functions: initializing
//! the `tsaot:native` module is what locks the `Math` and `Number` builtins.

use std::path::Path;

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::diagnostics::{GraphicalReportHandler, GraphicalTheme, NamedSource, OxcDiagnostic};
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};

use std::collections::HashMap;

use crate::{Analysis, CellInitSite, IntWrapKind, NativeSite};

pub const NATIVE_MODULE: &str = "tsaot:native";
pub const BIND_FN: &str = "__tsaot_bind";

pub fn fallback_name(name: &str) -> String {
    format!("__tsaot_fb_{name}")
}

pub const INIT_FN: &str = "__tsaot_init";
pub const CELLS_OBJ: &str = "__tsaot_m";
pub const METHOD_FN: &str = "__tsaot_method";
pub const CLASS_FN: &str = "__tsaot_class";
pub const STRUCT_FN: &str = "__tsaot_struct";

/// A JS string literal for `s`.
fn js_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Rewrite `source` so native functions are routed through their JS entries
/// and module bindings shared with native code live in cells.
pub fn rewrite(source: &str, a: &Analysis) -> String {
    // Replacements (start < end) and insertions (start == end). At equal
    // positions, replacements are applied first and insertions accumulate in
    // order, outermost last.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut prefixes: HashMap<usize, Vec<String>> = HashMap::new();
    let mut suffixes: HashMap<usize, Vec<String>> = HashMap::new();
    let mut exports = Vec::new();
    let mut header = Vec::new();

    // Stores into `int` bindings that wrap, as native code computes them:
    // `x = (x + 1) | 0`, the old value of `x++` being `(x - 1) | 0` after
    // it; `(value) | 0` for other stores.
    let mut rewritten_targets = Vec::new();
    for w in &a.int_wraps {
        let (s, e) = (w.span.0 as usize, w.span.1 as usize);
        if let IntWrapKind::Value = w.kind {
            // Innermost: before any wrapper of the same initializer.
            prefixes.entry(s).or_default().push("(".into());
            suffixes.entry(e).or_default().push(") | 0".into());
            continue;
        }
        // The binding may live in a cell.
        let target = match a.cell_refs.iter().find(|r| r.span == w.target) {
            Some(r) => format!("{CELLS_OBJ}.{}", a.cells[r.cell].name),
            None => w.name.clone(),
        };
        rewritten_targets.push(w.target);
        match w.kind {
            IntWrapKind::Update { inc, prefix, used } => {
                let (op, back) = if inc { ('+', '-') } else { ('-', '+') };
                let set = format!("{target} = ({target} {op} 1) | 0");
                let text =
                    if prefix || !used { format!("({set})") } else { format!("({set}, ({target} {back} 1) | 0)") };
                edits.push((s, e, text));
            }
            IntWrapKind::Value => unreachable!(),
            IntWrapKind::Compound { op, rhs: (rs, re) } => {
                edits.push((s, rs as usize, format!("({target} = ({target} {op} (")));
                suffixes.entry(re as usize).or_default().push(")) | 0)".into());
            }
        }
    }
    // Object literals that make objects of an `aot type`: from a plain
    // object, checked (native code makes the same objects directly).
    for &((s, e), k) in &a.struct_sites {
        prefixes.entry(s as usize).or_default().push(format!("{STRUCT_FN}({k}, "));
        suffixes.entry(e as usize).or_default().insert(0, ")".into());
    }
    // Native module code: its statements become one call (a native
    // function without parameters never declines to its fallback).
    let mut segments = Vec::new();
    for (i, n) in a.natives.iter().enumerate() {
        match &n.site {
            &NativeSite::Segment { span: (s, e) } => {
                segments.push((s as usize, e as usize, format!("{BIND_FN}({i}, () => {{}})();")));
            }
            &NativeSite::Declaration { id_span, export_prefix } => {
                edits.push((id_span.0 as usize, id_span.1 as usize, fallback_name(&n.name)));
                if let Some((s, e)) = export_prefix {
                    edits.push((s as usize, e as usize, String::new()));
                    exports.push(n.name.clone());
                }
                header.push(format!("const {} = {BIND_FN}({i}, {});", n.name, fallback_name(&n.name)));
            }
            &NativeSite::ConstInit { init_span: (s, e) } => {
                prefixes.entry(s as usize).or_default().push(format!("{BIND_FN}({i}, "));
                suffixes.entry(e as usize).or_default().push(")".into());
            }
            NativeSite::Method { class, is_static, key, stmt_end, accessor } => {
                // Over the interpreted method or accessor, which becomes the
                // fallback.
                let key = js_string(key);
                suffixes
                    .entry(*stmt_end as usize)
                    .or_default()
                    .push(format!(";{METHOD_FN}({i}, {class}, {}, {key}, {accessor});", *is_static as u8));
            }
        }
    }
    for (k, c) in a.cells.iter().enumerate() {
        if c.hoisted_undefined {
            header.push(format!("{INIT_FN}({k}, undefined);"));
        }
        match c.init {
            CellInitSite::Header => header.push(format!("{INIT_FN}({k}, {});", c.name)),
            CellInitSite::Wrap(s, e) => {
                prefixes.entry(s as usize).or_default().push(format!("{INIT_FN}({k}, "));
                suffixes.entry(e as usize).or_default().push(")".into());
            }
            CellInitSite::After(p) => {
                suffixes.entry(p as usize).or_default().push(format!(";{INIT_FN}({k}, {});", c.name));
            }
            CellInitSite::Native => {}
        }
    }
    for r in &a.cell_refs {
        if rewritten_targets.contains(&r.span) {
            continue;
        }
        let name = &a.cells[r.cell].name;
        let text = if r.shorthand { format!("{name}: {CELLS_OBJ}.{name}") } else { format!("{CELLS_OBJ}.{name}") };
        edits.push((r.span.0 as usize, r.span.1 as usize, text));
    }
    for cl in &a.classes {
        // Hands the class and its private names to native code, first
        // thing when the class is defined.
        let mut refs = String::new();
        for (var, _) in &cl.vars {
            match var.strip_suffix("<set>") {
                Some(name) => refs.push_str(&format!(" o.{name} = 0;")),
                None => refs.push_str(&format!(" o.{var};")),
            }
        }
        let block = format!("static {{ {CLASS_FN}({}, this, (o) => {{{refs} }}); }}", cl.index);
        prefixes.entry(cl.body_start as usize).or_default().push(block);
    }
    for (pos, v) in prefixes {
        // Innermost wrapper was pushed first; the outermost goes first in text.
        edits.push((pos, pos, v.into_iter().rev().collect()));
    }
    for (pos, v) in suffixes {
        edits.push((pos, pos, v.concat()));
    }
    // Nothing else of a segment's statements remains.
    edits.retain(|&(s, e, _)| !segments.iter().any(|&(ss, se, _)| ss <= s && e <= se));
    edits.extend(segments);
    // Back to front; at equal starts, the replacement before the insertion.
    edits.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
    let mut out = source.to_string();
    for (s, e, r) in edits {
        out.replace_range(s..e, &r);
    }

    // The bare import is what initializes the native module (and so locks
    // the builtins) even when nothing else is needed: named imports that end
    // up unused are elided by type stripping.
    let mut head = format!(
        "import \"{NATIVE_MODULE}\"; import {{ {BIND_FN}, {INIT_FN}, {CELLS_OBJ}, {METHOD_FN}, {CLASS_FN}, {STRUCT_FN} }} from \"{NATIVE_MODULE}\";"
    );
    for h in header {
        head.push(' ');
        head.push_str(&h);
    }
    // Keep a hashbang line first.
    let at = if out.starts_with("#!") { out.find('\n').map_or(out.len(), |i| i + 1) } else { 0 };
    out.insert_str(at, &format!("{head}\n"));
    if !exports.is_empty() {
        out.push_str(&format!("\nexport {{ {} }};\n", exports.join(", ")));
    }
    out
}

/// Strip TypeScript syntax, producing an ES module QuickJS can run.
pub fn strip_types(source: &str, path: &Path) -> Result<String, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::ts().with_module(true);
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.diagnostics.is_empty() {
        return Err(render(parsed.diagnostics, source, path));
    }
    let mut program = parsed.program;
    let sem = SemanticBuilder::new().with_excess_capacity(2.0).build(&program);
    if !sem.diagnostics.is_empty() {
        return Err(render(sem.diagnostics, source, path));
    }
    let scoping = sem.semantic.into_scoping();
    let options = TransformOptions::default();
    let ret = Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
    if !ret.diagnostics.is_empty() {
        return Err(render(ret.diagnostics, source, path));
    }
    Ok(Codegen::new().build(&program).code)
}

/// Render diagnostics with source snippets (colored when stderr is a terminal).
pub(crate) fn render(diags: impl IntoIterator<Item = OxcDiagnostic>, source: &str, path: &Path) -> String {
    use std::io::IsTerminal;
    let theme =
        if std::io::stderr().is_terminal() { GraphicalTheme::unicode() } else { GraphicalTheme::unicode_nocolor() };
    let handler = GraphicalReportHandler::new_themed(theme).with_width(100).with_links(false);
    let mut out = String::new();
    for d in diags {
        let report = d.with_source_code(NamedSource::new(path.to_string_lossy(), source.to_string()));
        let _ = handler.render_report(&mut out, report.as_ref());
    }
    out.trim_end().to_string()
}
