//! Front end: parse TypeScript with oxc, select natively compilable functions,
//! lower them to typed IR, and produce the type-erased JS for the rest.

pub mod builtins;
pub mod check;
pub mod erase;
pub mod ints;
pub mod ir;
pub mod marker;
pub mod structs;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use oxc::allocator::Allocator;
use oxc::ast::ast::{ArrowFunctionBody, ArrowFunctionExpression, Function, FunctionBody};
use oxc::ast_visit::{Visit, walk};
use oxc::diagnostics::{LabeledSpan, OxcDiagnostic};
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::{SourceType, Span};
use oxc::syntax::scope::ScopeFlags;

/// How a native function's binding is rewritten in the interpreted module.
#[derive(Debug, Clone)]
pub enum NativeSite {
    /// `function f` (byte span of the name, and of an `export ` prefix).
    Declaration { id_span: (u32, u32), export_prefix: Option<(u32, u32)> },
    /// `const f = <init>` (byte span of the initializer).
    ConstInit { init_span: (u32, u32) },
    /// A class method, installed after the class declaration ends.
    /// `accessor`: 0 for a method, 1 for a getter, 2 for a setter.
    Method { class: String, is_static: bool, key: String, stmt_end: u32, accessor: u8 },
    /// Module-level statements (byte span), replaced by a call of the native
    /// code.
    Segment { span: (u32, u32) },
    /// The constructor of class `class`, called by native `new` only.
    Ctor { class: String },
}

/// A natively compiled function, for source rewriting and reporting.
#[derive(Debug, Clone)]
pub struct NativeDecl {
    pub name: String,
    pub site: NativeSite,
    pub line: u32,
}

/// A class that hands data to native code when it is defined: a static
/// block calls `__tsaot_class(index, this, (o) => { ... })`, where the arrow
/// references private names so that their values can be read from it.
#[derive(Debug, Clone)]
pub struct ClassDecl {
    /// Its home slots are `2 * index` (prototype) and `2 * index + 1`.
    pub index: usize,
    /// Where the static block goes (just inside the class body).
    pub body_start: u32,
    /// Private name variables (`#x`, `#x<set>`) and their slots.
    pub vars: Vec<(String, usize)>,
}

/// A module-level binding that native code accesses through a cell.
#[derive(Debug, Clone)]
pub struct CellDecl {
    pub name: String,
    /// Interpreted code accesses the cell instead of its own binding.
    pub mutable: bool,
    pub init: CellInitSite,
    /// `var`: initialized to `undefined` in the module header.
    pub hoisted_undefined: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum CellInitSite {
    Header,
    /// Byte span of the initializer to wrap.
    Wrap(u32, u32),
    /// Byte offset after the declaring statement.
    After(u32),
    /// Set by native code.
    Native,
}

/// An interpreted reference to a mutable cell's binding.
#[derive(Debug, Clone, Copy)]
pub struct CellRef {
    pub span: (u32, u32),
    pub cell: ir::CellId,
    /// `{ x }` shorthand: must become `{ x: <cell> }`.
    pub shorthand: bool,
}
#[derive(Debug, Clone)]
pub struct Rejected {
    pub name: String,
    pub line: u32,
    /// Line of the construct that caused the rejection.
    pub reason_line: u32,
    pub reason: String,
}

/// Compilation options.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// `++`, `--`, `+=`, `-=`, `*=` on bindings declared `int` wrap like
    /// `x = (x + 1) | 0`, in native and interpreted code alike. Without it,
    /// they must provably stay int32s.
    pub int_wrapping: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { int_wrapping: true }
    }
}

/// A wrapping update of an `int` binding, rewritten in interpreted code.
#[derive(Debug, Clone)]
pub struct IntWrap {
    /// The whole expression.
    pub span: (u32, u32),
    /// The target identifier.
    pub target: (u32, u32),
    pub name: String,
    pub kind: IntWrapKind,
}

#[derive(Debug, Clone, Copy)]
pub enum IntWrapKind {
    /// `++x` / `x++` (`used`: whether the value of a postfix one is).
    Update { inc: bool, prefix: bool, used: bool },
    /// `x op= rhs` (`op`: `+`, `-`, `*` or `>>>`), with the span of `rhs`.
    Compound { op: &'static str, rhs: (u32, u32) },
    /// A stored value (`span`; no target): `(value) | 0`.
    Value,
}

#[derive(Debug)]
pub struct Analysis {
    /// The source with `aot` markers blanked out; same byte offsets as the
    /// original.
    pub source: String,
    pub module: ir::Module,
    /// Parallel to `module.funcs`.
    pub natives: Vec<NativeDecl>,
    pub cells: Vec<CellDecl>,
    pub cell_refs: Vec<CellRef>,
    pub rejected: Vec<Rejected>,
    pub classes: Vec<ClassDecl>,
    pub int_wraps: Vec<IntWrap>,
    /// Object literals that make objects of an `aot type`: span and layout.
    pub struct_sites: Vec<((u32, u32), u32)>,
    /// Arrays of objects of an `aot type`: span and layout.
    pub array_sites: Vec<((u32, u32), u32)>,
}

fn parse_error(diags: impl IntoIterator<Item = OxcDiagnostic>, source: &str, path: &Path) -> String {
    erase::render(diags, source, path)
}

pub fn analyze(original: &str, path: &Path, opts: &Options) -> Result<Analysis, String> {
    let stripped = marker::strip(original);
    let source = stripped.source;

    let allocator = Allocator::default();
    let source_type = SourceType::ts().with_module(true);
    let parsed = Parser::new(&allocator, &source, source_type).parse();
    if !parsed.diagnostics.is_empty() {
        let misplaced = marker::misplaced(original).into_iter().map(|(at, keyword)| {
            let what = if keyword == marker::INTERPRETED {
                "a function or an arrow function"
            } else {
                "a function, an arrow function or a type alias"
            };
            OxcDiagnostic::error(format!("`{keyword}` can only mark {what}"))
                .with_label(Span::sized(at, keyword.len() as u32))
        });
        return Err(parse_error(misplaced.chain(parsed.diagnostics), original, path));
    }
    let program = allocator.alloc(parsed.program);
    let sem = SemanticBuilder::new().build(program);
    if !sem.diagnostics.is_empty() {
        return Err(parse_error(sem.diagnostics, original, path));
    }
    let scoping = sem.semantic.into_scoping();

    let mut errors = builtins::check(program, &scoping);
    // `aot type`s: their layouts and the literals that make their objects.
    let (structs, mut struct_errors) = structs::analyze(program, &scoping, &stripped.aot_types);
    if !struct_errors.is_empty() {
        // Checking assumes well-formed `aot type`s and literals.
        errors.append(&mut struct_errors);
        return Err(erase::render(errors, original, path));
    }
    // The `int` type, over the whole module.
    let mut ints = ints::check(program, &scoping, opts.int_wrapping);
    errors.append(&mut ints.errors);
    let mut out = check::check(program, &scoping, &ints.vars, &stripped.interpreted, &structs);
    out.module.int_wrapping = opts.int_wrapping;

    let line = |pos: u32| source[..pos as usize].matches('\n').count() as u32 + 1;
    let natives: Vec<NativeDecl> = out
        .native
        .iter()
        .map(|c| NativeDecl {
            name: c.name.clone(),
            site: match &c.site {
                &check::Site::Declaration { id, export_prefix } => NativeSite::Declaration {
                    id_span: (id.start, id.end),
                    export_prefix: export_prefix.map(|s| (s.start, s.end)),
                },
                &check::Site::ConstInit { init } => NativeSite::ConstInit { init_span: (init.start, init.end) },
                check::Site::Method { class, is_static, key, stmt_end, accessor } => NativeSite::Method {
                    class: class.clone(),
                    is_static: *is_static,
                    key: key.clone(),
                    stmt_end: *stmt_end,
                    accessor: *accessor,
                },
                check::Site::Segment { span } => NativeSite::Segment { span: (span.start, span.end) },
                check::Site::Ctor { class } => NativeSite::Ctor { class: class.clone() },
            },
            line: line(c.node.span().start),
        })
        .collect();

    // Functions marked `aot` must have been compiled natively.
    let mut marks = MarkedFunctions { marked: &stripped.marked, found: Vec::new() };
    marks.visit_program(program);
    let inside = |outer: Span, inner: Span| outer.start <= inner.start && inner.end <= outer.end;
    // Functions in a constructor or field initializer run interpreted when
    // the class is constructed by interpreted code, even if native `new`
    // compiles them.
    let in_ctor = |span: Span| {
        out.native.iter().filter_map(|c| c.ctor_class()).any(|class| inside(class, span))
            && !out.native.iter().any(|c| matches!(c.site, check::Site::Method { .. }) && inside(c.node.span(), span))
    };
    for m in marks.found {
        if out.native.iter().any(|c| c.node.span() == m.span)
            || (out.closure_spans.contains(&m.span) && !in_ctor(m.span))
        {
            continue;
        }
        // A nested function is compiled together with its enclosing
        // top-level function: report why that one is not native.
        let enclosing =
            out.rejected.iter().find(|r| r.1 != m.span && r.1.start <= m.span.start && m.span.end <= r.1.end);
        if let Some((name, _, reason)) = enclosing {
            errors.push(
                OxcDiagnostic::error(format!("`{}` function inside `{name}` cannot be compiled natively", m.keyword))
                    .with_labels([
                        LabeledSpan::new_primary_with_span(Some(reason.msg.clone()), reason.span),
                        LabeledSpan::new_with_span(Some("marked here".into()), m.span),
                    ]),
            );
            continue;
        }
        errors.push(match out.rejected.iter().find(|r| r.1 == m.span) {
            Some((name, _, reason)) => {
                let mut d =
                    OxcDiagnostic::error(format!("`{}` function `{name}` cannot be compiled natively", m.keyword))
                        .with_labels([LabeledSpan::new_primary_with_span(Some(reason.msg.clone()), reason.span)]);
                if let Some(help) = callee_chain(&reason.msg, &out.rejected) {
                    d = d.with_help(help);
                }
                d
            }
            None => OxcDiagnostic::error(format!("`{}` cannot be applied to {}", m.keyword, m.what))
                .with_label(m.span)
                .with_help(
                    "it is in a class constructor or field initializer, which stay interpreted when \
                     interpreted code constructs the class; `aot` applies to module code, top-level \
                     functions and class methods, and to the functions nested in them",
                ),
        });
    }
    if !errors.is_empty() {
        return Err(erase::render(errors, original, path));
    }

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

    let cells: Vec<CellDecl> = out
        .cells
        .iter()
        .map(|c| CellDecl {
            name: c.name.clone(),
            mutable: c.mutable,
            init: match c.init {
                check::CellInit::Header => CellInitSite::Header,
                check::CellInit::Wrap(s) => CellInitSite::Wrap(s.start, s.end),
                check::CellInit::After(p) => CellInitSite::After(p),
                check::CellInit::Native => CellInitSite::Native,
            },
            hoisted_undefined: c.hoisted_undefined,
        })
        .collect();
    let mutable: std::collections::HashMap<oxc::semantic::SymbolId, ir::CellId> =
        out.cells.iter().enumerate().filter(|(_, c)| c.mutable).map(|(i, c)| (c.symbol, i)).collect();
    let mut refs = CellRefs { scoping: &scoping, mutable: &mutable, out: Vec::new() };
    if !mutable.is_empty() {
        refs.visit_program(program);
    }
    let cell_refs = refs.out;
    let classes = out
        .class_uses
        .iter()
        .map(|u| ClassDecl { index: u.class, body_start: out.classes[u.class].body_start, vars: u.vars.clone() })
        .collect();
    let int_wraps = ints.wraps;
    let mut struct_sites: Vec<((u32, u32), u32)> = structs.sites.iter().map(|(&s, &(e, k))| ((s, e), k)).collect();
    struct_sites.sort();
    let mut array_sites: Vec<((u32, u32), u32)> = structs.array_sites.iter().map(|(&s, &(e, k))| ((s, e), k)).collect();
    array_sites.sort();
    Ok(Analysis {
        source: source.clone(),
        module: out.module,
        natives,
        cells,
        cell_refs,
        rejected,
        classes,
        int_wraps,
        struct_sites,
        array_sites,
    })
}

/// Collects interpreted references to bindings that live in mutable cells.
struct CellRefs<'s> {
    scoping: &'s oxc::semantic::Scoping,
    mutable: &'s std::collections::HashMap<oxc::semantic::SymbolId, ir::CellId>,
    out: Vec<CellRef>,
}

impl CellRefs<'_> {
    fn cell_of(&self, id: &oxc::ast::ast::IdentifierReference) -> Option<ir::CellId> {
        let r = id.reference_id.get()?;
        let sym = self.scoping.get_reference(r).symbol_id()?;
        self.mutable.get(&sym).copied()
    }
}

impl<'a> Visit<'a> for CellRefs<'_> {
    fn visit_identifier_reference(&mut self, it: &oxc::ast::ast::IdentifierReference<'a>) {
        if let Some(cell) = self.cell_of(it) {
            self.out.push(CellRef { span: (it.span.start, it.span.end), cell, shorthand: false });
        }
    }

    fn visit_object_property(&mut self, it: &oxc::ast::ast::ObjectProperty<'a>) {
        if it.shorthand
            && let oxc::ast::ast::Expression::Identifier(id) = &it.value
            && let Some(cell) = self.cell_of(id)
        {
            self.out.push(CellRef { span: (id.span.start, id.span.end), cell, shorthand: true });
            return;
        }
        walk::walk_object_property(self, it);
    }

    fn visit_assignment_target_property_identifier(
        &mut self,
        it: &oxc::ast::ast::AssignmentTargetPropertyIdentifier<'a>,
    ) {
        if let Some(cell) = self.cell_of(&it.binding) {
            let s = it.binding.span;
            self.out.push(CellRef { span: (s.start, s.end), cell, shorthand: true });
            if let Some(init) = &it.init {
                self.visit_expression(init);
            }
            return;
        }
        walk::walk_assignment_target_property_identifier(self, it);
    }
}

/// For "calls `g`, which is not compiled natively", explain why `g` is not.
fn callee_chain(msg: &str, rejected: &[(String, Span, check::Reject)]) -> Option<String> {
    let mut lines = Vec::new();
    let mut msg = msg.to_string();
    let mut seen = HashSet::new();
    while let Some(callee) = msg.strip_prefix("calls `").and_then(|m| m.split('`').next()).map(str::to_string) {
        if !seen.insert(callee.clone()) {
            break;
        }
        let Some((_, _, r)) = rejected.iter().find(|r| r.0 == callee) else { break };
        lines.push(format!("`{callee}` is interpreted: {}", r.msg));
        msg = r.msg.clone();
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

struct Mark {
    span: Span,
    what: &'static str,
    /// `aot` or `compiled`, as written.
    keyword: &'static str,
}

/// Collects functions marked by the `aot` (or `compiled`) keyword or a
/// `"use aot"` (`"use compiled"`) directive.
struct MarkedFunctions<'m> {
    marked: &'m HashMap<u32, &'static str>,
    found: Vec<Mark>,
}

/// The keyword of a `"use aot"` / `"use compiled"` directive, if any.
fn use_aot(body: Option<&FunctionBody>) -> Option<&'static str> {
    body?.directives.iter().find_map(|d| match d.directive.as_str() {
        "use aot" => Some(marker::KEYWORD),
        "use compiled" => Some(marker::COMPILED),
        _ => None,
    })
}

impl<'a> Visit<'a> for MarkedFunctions<'_> {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if let Some(keyword) = self.marked.get(&it.span.start).copied().or_else(|| use_aot(it.body.as_deref())) {
            self.found.push(Mark { span: it.span, what: "this function", keyword });
        }
        walk::walk_function(self, it, flags);
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let body = match &it.body {
            ArrowFunctionBody::FunctionBody(b) => Some(&**b),
            _ => None,
        };
        if let Some(keyword) = self.marked.get(&it.span.start).copied().or_else(|| use_aot(body)) {
            self.found.push(Mark { span: it.span, what: "this arrow function", keyword });
        }
        walk::walk_arrow_function_expression(self, it);
    }
}

/// Full front-end pipeline for the interpreted half: rewrite + type erasure.
pub fn erased_js(path: &Path, analysis: &Analysis) -> Result<String, String> {
    let rewritten = erase::rewrite(&analysis.source, analysis);
    erase::strip_types(&rewritten, path)
}
