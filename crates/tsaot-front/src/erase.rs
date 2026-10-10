//! Produces the interpreted half of a program: the whole module with types
//! erased, where every natively compiled function is renamed to a fallback
//! and its original binding is re-pointed at the native entry stub.
//!
//! ```ts
//! function fib(n: number): number { ... }
//! ```
//! becomes
//! ```js
//! import { __tsaot_bind } from "tsaot:native";
//! const [fib] = __tsaot_bind(__tsaot_fb_fib);
//! function __tsaot_fb_fib(n) { ... }   // calls inside still go to `fib`
//! ```
//! The binding statement is the first statement of the module body, so no
//! code can observe `fib` before it is initialized.

use std::path::Path;

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};

use crate::NativeDecl;

pub const NATIVE_MODULE: &str = "tsaot:native";
pub const BIND_FN: &str = "__tsaot_bind";

pub fn fallback_name(name: &str) -> String {
    format!("__tsaot_fb_{name}")
}

/// Rewrite `source` so native functions are routed through their entry stubs.
pub fn rewrite(source: &str, natives: &[NativeDecl]) -> String {
    if natives.is_empty() {
        return source.to_string();
    }
    // (start, end, replacement), applied back to front.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut exports = Vec::new();
    for n in natives {
        edits.push((n.id_span.0 as usize, n.id_span.1 as usize, fallback_name(&n.name)));
        if let Some((s, e)) = n.export_prefix {
            edits.push((s as usize, e as usize, String::new()));
            exports.push(n.name.clone());
        }
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = source.to_string();
    for (s, e, r) in edits {
        out.replace_range(s..e, &r);
    }

    let names: Vec<&str> = natives.iter().map(|n| n.name.as_str()).collect();
    let fallbacks: Vec<String> = natives.iter().map(|n| fallback_name(&n.name)).collect();
    let header = format!(
        "import {{ {BIND_FN} }} from \"{NATIVE_MODULE}\"; const [{}] = {BIND_FN}({});",
        names.join(", "),
        fallbacks.join(", ")
    );
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
        return Err(render(parsed.diagnostics, source));
    }
    let mut program = parsed.program;
    let sem = SemanticBuilder::new().with_excess_capacity(2.0).build(&program);
    if !sem.diagnostics.is_empty() {
        return Err(render(sem.diagnostics, source));
    }
    let scoping = sem.semantic.into_scoping();
    let options = TransformOptions::default();
    let ret = Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
    if !ret.diagnostics.is_empty() {
        return Err(render(ret.diagnostics, source));
    }
    Ok(Codegen::new().build(&program).code)
}

pub(crate) fn render(diags: impl IntoIterator<Item = oxc::diagnostics::OxcDiagnostic>, source: &str) -> String {
    diags.into_iter().map(|d| format!("{:?}", d.with_source_code(source.to_string()))).collect::<Vec<_>>().join("\n")
}
