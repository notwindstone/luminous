//! The `Math` and `Number` builtins that native code relies on are locked:
//! the runtime makes them non-writable and non-configurable before any JS
//! runs (see `runtime/tsaot_rt.c`), so patching them throws a `TypeError`.
//! This pass turns the patches that are visible in the source into compile
//! errors instead.

use oxc::ast::ast::*;
use oxc::ast_visit::{Visit, walk};
use oxc::diagnostics::OxcDiagnostic;
use oxc::semantic::Scoping;
use oxc::span::{GetSpan, Span};

/// Every own property of `Math` (ES2026); all are locked at runtime.
#[rustfmt::skip]
pub const MATH_PROPS: &[&str] = &[
    "abs", "acos", "acosh", "asin", "asinh", "atan", "atanh", "atan2", "cbrt", "ceil", "clz32", "cos", "cosh", "exp",
    "expm1", "f16round", "floor", "fround", "hypot", "imul", "log", "log1p", "log10", "log2", "max", "min", "pow",
    "random", "round", "sign", "sin", "sinh", "sqrt", "sumPrecise", "tan", "tanh", "trunc", "E", "LN10", "LN2",
    "LOG10E", "LOG2E", "PI", "SQRT1_2", "SQRT2",
];

/// Locked `Number` properties: the predicates are locked at runtime, the
/// constants are already non-writable per the spec.
pub const NUMBER_PROPS: &[&str] = &[
    "isNaN",
    "isFinite",
    "isInteger",
    "isSafeInteger",
    "EPSILON",
    "MAX_VALUE",
    "MIN_VALUE",
    "MAX_SAFE_INTEGER",
    "MIN_SAFE_INTEGER",
    "POSITIVE_INFINITY",
    "NEGATIVE_INFINITY",
    "NaN",
];

const LOCKED_GLOBALS: &[&str] = &["Math", "Number"];

pub fn check(program: &Program, scoping: &Scoping) -> Vec<OxcDiagnostic> {
    let mut v = Lint { scoping, errors: Vec::new() };
    v.visit_program(program);
    v.errors
}

struct Lint<'s> {
    scoping: &'s Scoping,
    errors: Vec<OxcDiagnostic>,
}

impl Lint<'_> {
    /// The name of an unshadowed global identifier.
    fn global<'e>(&self, e: &'e Expression) -> Option<&'e str> {
        let Expression::Identifier(id) = e.get_inner_expression() else { return None };
        let r = id.reference_id.get()?;
        self.scoping.get_reference(r).symbol_id().is_none().then(|| id.name.as_str())
    }

    /// `Math` / `Number` (possibly as `globalThis.Math`).
    fn locked_object<'e>(&self, e: &'e Expression) -> Option<&'e str> {
        if let Some(name) = self.global(e) {
            return LOCKED_GLOBALS.contains(&name).then_some(name);
        }
        let (object, prop) = static_member(e.get_inner_expression())?;
        (self.global(object) == Some("globalThis") && LOCKED_GLOBALS.contains(&prop)).then_some(prop)
    }

    /// Describes `member` if it names a locked property, e.g. `Math.sqrt`.
    fn locked_member(&self, member: &MemberExpression) -> Option<String> {
        let (object, prop) = match member {
            MemberExpression::StaticMemberExpression(m) => (&m.object, m.property.name.as_str()),
            MemberExpression::ComputedMemberExpression(m) => match m.expression.get_inner_expression() {
                Expression::StringLiteral(s) => (&m.object, s.value.as_str()),
                _ => return None,
            },
            MemberExpression::PrivateFieldExpression(_) => return None,
        };
        if self.global(object) == Some("globalThis") && LOCKED_GLOBALS.contains(&prop) {
            return Some(format!("globalThis.{prop}"));
        }
        let props = match self.locked_object(object)? {
            "Math" => MATH_PROPS,
            _ => NUMBER_PROPS,
        };
        props.contains(&prop).then(|| format!("{}.{prop}", self.locked_object(object).unwrap()))
    }

    fn target(&mut self, t: &SimpleAssignmentTarget, span: Span) {
        let what = match t {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let unresolved =
                    id.reference_id.get().is_some_and(|r| self.scoping.get_reference(r).symbol_id().is_none());
                (unresolved && LOCKED_GLOBALS.contains(&id.name.as_str())).then(|| id.name.to_string())
            }
            t => t.as_member_expression().and_then(|m| self.locked_member(m)),
        };
        if let Some(what) = what {
            self.error(span, format!("`{what}` cannot be reassigned"));
        }
    }

    fn error(&mut self, span: Span, msg: String) {
        self.errors.push(
            OxcDiagnostic::error(msg)
                .with_label(span)
                .with_help("tsaot locks the `Math` and `Number` builtins so that compiled code can rely on them"),
        );
    }
}

fn static_member<'e>(e: &'e Expression) -> Option<(&'e Expression<'e>, &'e str)> {
    match e {
        Expression::StaticMemberExpression(m) => Some((&m.object, m.property.name.as_str())),
        _ => None,
    }
}

impl<'a> Visit<'a> for Lint<'_> {
    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        if let Some(t) = it.left.as_simple_assignment_target() {
            self.target(t, it.left.span());
        }
        walk::walk_assignment_expression(self, it);
    }

    fn visit_update_expression(&mut self, it: &UpdateExpression<'a>) {
        self.target(&it.argument, it.span);
        walk::walk_update_expression(self, it);
    }

    fn visit_unary_expression(&mut self, it: &UnaryExpression<'a>) {
        if it.operator == UnaryOperator::Delete
            && let Some(m) = it.argument.get_inner_expression().as_member_expression()
            && let Some(what) = self.locked_member(m)
        {
            self.error(it.span, format!("`{what}` cannot be deleted"));
        }
        walk::walk_unary_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        // Object.defineProperty(Math, ...), Reflect.set(Number, ...), ...
        if let Some((object, method)) = static_member(it.callee.get_inner_expression())
            && let Some(api) = self.global(object)
            && matches!(
                (api, method),
                ("Object", "defineProperty" | "defineProperties" | "assign" | "setPrototypeOf")
                    | ("Reflect", "set" | "defineProperty" | "deleteProperty" | "setPrototypeOf")
            )
            && let Some(target) = it.arguments.first().and_then(|a| a.as_expression())
            && let Some(name) = self.locked_object(target)
        {
            self.error(it.span, format!("`{api}.{method}` on `{name}` would modify a locked builtin"));
        }
        walk::walk_call_expression(self, it);
    }
}
