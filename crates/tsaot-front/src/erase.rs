//! Produces the interpreted half of a program: the whole module with types
//! erased, where every natively compiled function's binding is re-pointed at
//! its native entry stub. `__tsaot_bind(i, fallback)` returns the stub for
//! native function `i`, which calls `fallback` when its arguments do not
//! match the declared types.
//!
//! ```ts
//! function fib(n: number): number { ... }
//! const sq = (x: number): number => x * x;
//! ```
//! becomes
//! ```js
//! import { __tsaot_bind } from "tsaot:native"; const fib = __tsaot_bind(0, __tsaot_fb_fib);
//! function __tsaot_fb_fib(n) { ... }   // calls inside still go to `fib`
//! const sq = __tsaot_bind(1, (x) => x * x);
//! ```
//! Declarations are bound in the first statement of the module body, so no
//! code can observe them before they are initialized. The import is emitted
//! even without native functions: initializing the `tsaot:native` module is
//! what locks the `Math` and `Number` builtins.

use std::path::Path;

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::diagnostics::{GraphicalReportHandler, GraphicalTheme, NamedSource, OxcDiagnostic};
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};

use crate::{NativeDecl, NativeSite};

pub const NATIVE_MODULE: &str = "tsaot:native";
pub const BIND_FN: &str = "__tsaot_bind";

pub fn fallback_name(name: &str) -> String {
    format!("__tsaot_fb_{name}")
}

/// Rewrite `source` so native functions are routed through their entry stubs.
pub fn rewrite(source: &str, natives: &[NativeDecl]) -> String {
    // (start, end, replacement), applied back to front.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut exports = Vec::new();
    let mut bindings = Vec::new();
    for (i, n) in natives.iter().enumerate() {
        match n.site {
            NativeSite::Declaration { id_span, export_prefix } => {
                edits.push((id_span.0 as usize, id_span.1 as usize, fallback_name(&n.name)));
                if let Some((s, e)) = export_prefix {
                    edits.push((s as usize, e as usize, String::new()));
                    exports.push(n.name.clone());
                }
                bindings.push(format!("{} = {BIND_FN}({i}, {})", n.name, fallback_name(&n.name)));
            }
            NativeSite::ConstInit { init_span: (s, e) } => {
                edits.push((s as usize, s as usize, format!("{BIND_FN}({i}, ")));
                edits.push((e as usize, e as usize, ")".into()));
            }
        }
    }
    edits.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
    let mut out = source.to_string();
    for (s, e, r) in edits {
        out.replace_range(s..e, &r);
    }

    // A named import that ends up unused would be elided by type stripping
    // (as tsc does), so without native functions use a side-effect import.
    let mut header = if natives.is_empty() {
        format!("import \"{NATIVE_MODULE}\";")
    } else {
        format!("import {{ {BIND_FN} }} from \"{NATIVE_MODULE}\";")
    };
    if !bindings.is_empty() {
        header.push_str(&format!(" const {};", bindings.join(", ")));
    }
    // Keep a hashbang line first.
    let at = if out.starts_with("#!") { out.find('\n').map_or(out.len(), |i| i + 1) } else { 0 };
    out.insert_str(at, &format!("{header}\n"));
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
