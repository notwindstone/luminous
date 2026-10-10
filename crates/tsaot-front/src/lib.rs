//! Front end: parse TypeScript with oxc, select natively compilable functions,
//! lower them to typed IR, and produce the type-erased JS for the rest.

pub mod check;
pub mod erase;
pub mod ir;

use std::path::Path;

use oxc::allocator::Allocator;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;

/// A natively compiled function's declaration, for source rewriting.
#[derive(Debug, Clone)]
pub struct NativeDecl {
    pub name: String,
    /// Byte span of the function's name in the declaration.
    pub id_span: (u32, u32),
    /// Byte span of the `export ` keyword prefix, if exported.
    pub export_prefix: Option<(u32, u32)>,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub struct Rejected {
    pub name: String,
    pub line: u32,
    /// Line of the construct that caused the rejection.
    pub reason_line: u32,
    pub reason: String,
}

#[derive(Debug)]
pub struct Analysis {
    pub module: ir::Module,
    /// Parallel to `module.funcs`.
    pub natives: Vec<NativeDecl>,
    pub rejected: Vec<Rejected>,
}

pub fn analyze(source: &str, path: &Path) -> Result<Analysis, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::ts().with_module(true);
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.diagnostics.is_empty() {
        return Err(erase::render(parsed.diagnostics, source));
    }
    let program = allocator.alloc(parsed.program);
    let sem = SemanticBuilder::new().build(program);
    if !sem.diagnostics.is_empty() {
        return Err(erase::render(sem.diagnostics, source));
    }
    let scoping = sem.semantic.into_scoping();
    let _ = path;

    let out = check::check(program, &scoping);
    let line = |pos: u32| source[..pos as usize].matches('\n').count() as u32 + 1;
    let natives = out
        .native
        .iter()
        .map(|c| {
            let id = c.func.id.as_ref().unwrap();
            NativeDecl {
                name: c.name.clone(),
                id_span: (id.span.start, id.span.end),
                export_prefix: c.export_prefix.map(|s| (s.start, s.end)),
                line: line(c.func.span.start),
            }
        })
        .collect();
    let mut rejected: Vec<Rejected> = out
        .rejected
        .into_iter()
        .map(|(name, span, r)| Rejected {
            name,
            line: line(span.start),
            reason_line: line(r.span.start),
            reason: r.msg,
        })
        .collect();
    rejected.sort_by_key(|r| r.line);
    Ok(Analysis { module: out.module, natives, rejected })
}

/// Full front-end pipeline for the interpreted half: rewrite + type erasure.
pub fn erased_js(source: &str, path: &Path, analysis: &Analysis) -> Result<String, String> {
    let rewritten = erase::rewrite(source, &analysis.natives);
    erase::strip_types(&rewritten, path)
}
