//! The `int` type: `type int = number`, checked over the whole module.
//!
//! Every store into an `int` (initializer, `=`, compound assignment, `++` /
//! `--`, `return` from a function annotated `int`, parameter default) is
//! classified by the value stored:
//! - an int32 (a literal in range, another `int`, a bitwise result, a call
//!   to a function returning `int`, `Math.imul`...): stored as is;
//! - an integer that may not fit (`+`, `-`, `*` of integers, `>>>`): it wraps
//!   like `| 0`, natively and in the interpreted code (rewritten here). With
//!   `--no-int-wrapping` it is an error instead, except for a `for` counter
//!   stepped by one under an int bound (`i < n`);
//! - anything else (fractions, `/`, `%`, values of unknown type): an error.
//!
//! This pass reports the errors and the stores the interpreted code must
//! wrap; native code wraps every store it cannot see to be an int32 (a no-op
//! for one), so both halves agree.

use std::collections::HashSet;

use oxc::ast::ast::*;
use oxc::ast_visit::{Visit, walk};
use oxc::diagnostics::OxcDiagnostic;
use oxc::semantic::{Scoping, SymbolId};
use oxc::span::{GetSpan, Span};
use oxc::syntax::scope::ScopeFlags;

use crate::{IntWrap, IntWrapKind};

/// Whether a type annotation is `int`.
pub fn is_int_type(t: &TSType) -> bool {
    match t {
        TSType::TSParenthesizedType(p) => is_int_type(&p.type_annotation),
        TSType::TSTypeReference(r) => matches!(&r.type_name, TSTypeName::IdentifierReference(id) if id.name == "int"),
        _ => false,
    }
}

fn is_int_annotation(a: Option<&oxc::allocator::Box<TSTypeAnnotation>>) -> bool {
    a.is_some_and(|a| is_int_type(&a.type_annotation))
}

/// The bindings declared `int` (variables and parameters), and the
/// functions annotated to return `int`.
pub fn int_symbols(program: &Program, scoping: &Scoping) -> (HashSet<SymbolId>, HashSet<SymbolId>) {
    struct Ints<'s> {
        scoping: &'s Scoping,
        vars: HashSet<SymbolId>,
        fns: HashSet<SymbolId>,
    }
    impl<'a> Visit<'a> for Ints<'_> {
        fn visit_variable_declarator(&mut self, d: &VariableDeclarator<'a>) {
            if let BindingPattern::BindingIdentifier(id) = &d.id {
                if is_int_annotation(d.type_annotation.as_ref()) {
                    self.vars.extend(id.symbol_id.get());
                }
                let returns_int = match d.init.as_ref().map(|e| e.without_parentheses()) {
                    Some(Expression::ArrowFunctionExpression(a)) => is_int_annotation(a.return_type.as_ref()),
                    Some(Expression::FunctionExpression(f)) => is_int_annotation(f.return_type.as_ref()),
                    _ => false,
                };
                if returns_int
                    && let Some(s) = id.symbol_id.get()
                    && self.scoping.symbol_flags(s).is_const_variable()
                {
                    self.fns.insert(s);
                }
            }
            walk::walk_variable_declarator(self, d);
        }
        fn visit_formal_parameter(&mut self, p: &FormalParameter<'a>) {
            if let BindingPattern::BindingIdentifier(id) = &p.pattern
                && is_int_annotation(p.type_annotation.as_ref())
            {
                self.vars.extend(id.symbol_id.get());
            }
            walk::walk_formal_parameter(self, p);
        }
        fn visit_function(&mut self, f: &Function<'a>, flags: ScopeFlags) {
            if f.r#type == FunctionType::FunctionDeclaration
                && is_int_annotation(f.return_type.as_ref())
                && let Some(id) = &f.id
            {
                self.fns.extend(id.symbol_id.get());
            }
            walk::walk_function(self, f, flags);
        }
    }
    let mut v = Ints { scoping, vars: HashSet::new(), fns: HashSet::new() };
    v.visit_program(program);
    (v.vars, v.fns)
}

/// What a stored value is known to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Int32,
    /// An integer, possibly out of the int32 range.
    Integer,
    /// Not known to be an integer, and why.
    Other(&'static str),
}

impl Class {
    /// The class of an integer computed from these.
    fn integer_of(a: Class, b: Class) -> Class {
        match (a, b) {
            (Class::Other(r), _) | (_, Class::Other(r)) => Class::Other(r),
            _ => Class::Integer,
        }
    }
    fn either(a: Class, b: Class) -> Class {
        match (a, b) {
            (Class::Int32, Class::Int32) => Class::Int32,
            (Class::Other(r), _) | (_, Class::Other(r)) => Class::Other(r),
            _ => Class::Integer,
        }
    }
}

const UNKNOWN: &str = "is not known to be an integer";
const FRACTION: &str = "may not be an integer (`/`, `%`, `**` and most `Math` functions can give fractions, NaN \
                        or -0)";

pub struct Outcome {
    pub errors: Vec<OxcDiagnostic>,
    pub wraps: Vec<IntWrap>,
    /// Bindings declared `int`.
    pub vars: HashSet<SymbolId>,
}

pub fn check(program: &Program, scoping: &Scoping, wrapping: bool) -> Outcome {
    let (vars, fns) = int_symbols(program, scoping);
    let mut cx = Cx {
        scoping,
        vars: &vars,
        fns: &fns,
        wrapping,
        rets: Vec::new(),
        counters: Vec::new(),
        unused: Vec::new(),
        errors: Vec::new(),
        wraps: Vec::new(),
    };
    if !vars.is_empty() || !fns.is_empty() {
        cx.visit_program(program);
    }
    let Cx { errors, wraps, .. } = cx;
    Outcome { errors, wraps, vars }
}

struct Cx<'s> {
    scoping: &'s Scoping,
    vars: &'s HashSet<SymbolId>,
    fns: &'s HashSet<SymbolId>,
    wrapping: bool,
    /// Per enclosing function: whether it returns `int`.
    rets: Vec<bool>,
    /// `for` counters whose update clause is being visited (strict mode:
    /// proven to stay in range by the loop test).
    counters: Vec<SymbolId>,
    /// Updates whose value is not used (statements, `for` updates).
    unused: Vec<Span>,
    errors: Vec<OxcDiagnostic>,
    wraps: Vec<IntWrap>,
}

impl Cx<'_> {
    fn symbol(&self, id: &IdentifierReference) -> Option<SymbolId> {
        self.scoping.get_reference(id.reference_id.get()?).symbol_id()
    }

    fn is_int_ident(&self, id: &IdentifierReference) -> bool {
        self.symbol(id).is_some_and(|s| self.vars.contains(&s))
    }

    fn is_global(&self, id: &IdentifierReference, name: &str) -> bool {
        id.name == name && self.symbol(id).is_none()
    }

    fn classify(&self, e: &Expression) -> Class {
        match e {
            Expression::ParenthesizedExpression(p) => self.classify(&p.expression),
            Expression::TSAsExpression(x) => self.classify(&x.expression),
            Expression::TSSatisfiesExpression(x) => self.classify(&x.expression),
            Expression::TSNonNullExpression(x) => self.classify(&x.expression),
            Expression::NumericLiteral(n) => literal(n.value),
            Expression::Identifier(id) if self.is_int_ident(id) => Class::Int32,
            Expression::UnaryExpression(u) => match u.operator {
                UnaryOperator::BitwiseNot => Class::Int32,
                UnaryOperator::UnaryNegation => match u.argument.without_parentheses() {
                    Expression::NumericLiteral(n) => literal(-n.value),
                    // -(-2^31) does not fit; -0 wraps to 0.
                    a => Class::integer_of(self.classify(a), Class::Int32),
                },
                UnaryOperator::UnaryPlus => self.classify(&u.argument),
                _ => Class::Other(UNKNOWN),
            },
            Expression::BinaryExpression(b) => {
                use BinaryOperator as B;
                match b.operator {
                    B::BitwiseOR | B::BitwiseAnd | B::BitwiseXOR | B::ShiftLeft | B::ShiftRight => Class::Int32,
                    B::ShiftRightZeroFill => Class::Integer,
                    B::Addition | B::Subtraction | B::Multiplication => {
                        Class::integer_of(self.classify(&b.left), self.classify(&b.right))
                    }
                    B::Division | B::Remainder | B::Exponential => Class::Other(FRACTION),
                    _ => Class::Other(UNKNOWN),
                }
            }
            Expression::ConditionalExpression(c) => {
                Class::either(self.classify(&c.consequent), self.classify(&c.alternate))
            }
            // One of the operands.
            Expression::LogicalExpression(l) => Class::either(self.classify(&l.left), self.classify(&l.right)),
            Expression::SequenceExpression(s) => {
                s.expressions.last().map_or(Class::Other(UNKNOWN), |e| self.classify(e))
            }
            // The value of a store into an int is that int.
            Expression::AssignmentExpression(a) if self.int_target(&a.left).is_some() => Class::Int32,
            Expression::UpdateExpression(u) if matches!(&u.argument, SimpleAssignmentTarget::AssignmentTargetIdentifier(id) if self.is_int_ident(id)) => {
                Class::Int32
            }
            Expression::CallExpression(c) => self.classify_call(c),
            _ => Class::Other(UNKNOWN),
        }
    }

    fn classify_call(&self, c: &CallExpression) -> Class {
        match c.callee.without_parentheses() {
            Expression::Identifier(id) if self.symbol(id).is_some_and(|s| self.fns.contains(&s)) => Class::Int32,
            Expression::StaticMemberExpression(m) if matches!(&m.object, Expression::Identifier(o) if self.is_global(o, "Math")) =>
            {
                let args: Vec<Class> = c
                    .arguments
                    .iter()
                    .map(|a| a.as_expression().map_or(Class::Other(UNKNOWN), |e| self.classify(e)))
                    .collect();
                match m.property.name.as_str() {
                    "imul" | "clz32" => Class::Int32,
                    "min" | "max" if !args.is_empty() => args.into_iter().reduce(Class::either).unwrap(),
                    "abs" if args.len() == 1 => Class::integer_of(args[0], Class::Int32),
                    _ => Class::Other(FRACTION),
                }
            }
            _ => Class::Other(UNKNOWN),
        }
    }

    /// The `int` binding an assignment target names.
    fn int_target(&self, t: &AssignmentTarget) -> Option<String> {
        let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = t.as_simple_assignment_target()? else {
            return None;
        };
        self.is_int_ident(id).then(|| id.name.to_string())
    }

    fn error(&mut self, span: Span, what: &str, why: &str) {
        let help = if why == UNKNOWN || why.starts_with("may not be an integer") || why.starts_with("is not an integer")
        {
            "convert it with `| 0` if truncation is intended"
        } else {
            "convert it with `| 0` if wrapping is intended"
        };
        self.errors
            .push(OxcDiagnostic::error(format!("{what}, but this value {why}")).with_label(span).with_help(help));
    }

    /// A store of `value` into int `what`: an error, or whether it wraps.
    fn store(&mut self, what: &str, value: &Expression, span: Span, counter: bool) -> bool {
        match self.classify(value) {
            Class::Int32 => false,
            Class::Integer if self.wrapping => true,
            Class::Integer if counter => false,
            Class::Integer => {
                self.error(
                    span,
                    what,
                    "may leave the int32 range (with `--no-int-wrapping`, only a `for` counter bounded by an int, \
                     as in `i < n`, steps by one)",
                );
                false
            }
            Class::Other(why) => {
                self.error(span, what, why);
                false
            }
        }
    }

    fn mark_unused(&mut self, e: &Expression) {
        match e.without_parentheses() {
            Expression::UpdateExpression(u) => self.unused.push(u.span),
            Expression::SequenceExpression(s) => s.expressions.iter().for_each(|e| self.mark_unused(e)),
            _ => {}
        }
    }

    fn wrap_value(&mut self, value: &Expression) {
        let s = value.span();
        self.wraps.push(IntWrap {
            span: (s.start, s.end),
            target: (s.start, s.start),
            name: String::new(),
            kind: IntWrapKind::Value,
        });
    }

    /// For a `for` loop in strict mode: the int counter `i` that its update
    /// steps by one in the direction its test bounds (`i < n` with `n` an
    /// int32, `i++`), if the body does not assign it.
    fn counter_of(&self, f: &ForStatement) -> Option<SymbolId> {
        let Some(Expression::BinaryExpression(t)) = f.test.as_ref().map(|t| t.without_parentheses()) else {
            return None;
        };
        let Expression::Identifier(i) = t.left.without_parentheses() else { return None };
        let sym = self.symbol(i).filter(|s| self.vars.contains(s))?;
        let bound = self.classify(&t.right) == Class::Int32;
        let lit = match t.right.without_parentheses() {
            Expression::NumericLiteral(n) => Some(n.value),
            _ => None,
        };
        let up = match t.operator {
            BinaryOperator::LessThan => bound,
            BinaryOperator::LessEqualThan => lit.is_some_and(|k| k < i32::MAX as f64) && bound,
            _ => false,
        };
        let down = match t.operator {
            BinaryOperator::GreaterThan => bound,
            BinaryOperator::GreaterEqualThan => lit.is_some_and(|k| k > i32::MIN as f64) && bound,
            _ => false,
        };
        let step = match f.update.as_ref()?.without_parentheses() {
            Expression::UpdateExpression(u) => match &u.argument {
                SimpleAssignmentTarget::AssignmentTargetIdentifier(id) if self.symbol(id) == Some(sym) => {
                    Some(u.operator == UpdateOperator::Increment)
                }
                _ => None,
            },
            _ => None,
        }?;
        if !(step && up || !step && down) {
            return None;
        }
        let mut w = Writes { scoping: self.scoping, sym, found: false };
        w.visit_statement(&f.body);
        (!w.found).then_some(sym)
    }
}

fn literal(v: f64) -> Class {
    if v.fract() != 0.0 || !v.is_finite() {
        Class::Other("is not an integer")
    } else if v == 0.0 && v.is_sign_negative() {
        // -0: stored as 0.
        Class::Integer
    } else if v >= i32::MIN as f64 && v <= i32::MAX as f64 {
        Class::Int32
    } else {
        Class::Other("is an integer literal out of the int32 range")
    }
}

/// Whether a statement assigns a symbol.
struct Writes<'s> {
    scoping: &'s Scoping,
    sym: SymbolId,
    found: bool,
}

impl<'a> Visit<'a> for Writes<'_> {
    fn visit_simple_assignment_target(&mut self, t: &SimpleAssignmentTarget<'a>) {
        if let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = t
            && let Some(r) = id.reference_id.get()
            && self.scoping.get_reference(r).symbol_id() == Some(self.sym)
        {
            self.found = true;
        }
        walk::walk_simple_assignment_target(self, t);
    }
}

impl<'a> Visit<'a> for Cx<'_> {
    fn visit_expression_statement(&mut self, s: &ExpressionStatement<'a>) {
        self.mark_unused(&s.expression);
        walk::walk_expression_statement(self, s);
    }

    fn visit_function(&mut self, f: &Function<'a>, flags: ScopeFlags) {
        self.rets.push(is_int_annotation(f.return_type.as_ref()) && !f.r#async && !f.generator);
        walk::walk_function(self, f, flags);
        self.rets.pop();
    }

    fn visit_arrow_function_expression(&mut self, a: &ArrowFunctionExpression<'a>) {
        let int = is_int_annotation(a.return_type.as_ref()) && !a.r#async;
        if int
            && let Some(e) = a.body.as_expression()
            && self.store("the function returns `int`", e, e.span(), false)
        {
            self.wrap_value(e);
        }
        self.rets.push(int);
        walk::walk_arrow_function_expression(self, a);
        self.rets.pop();
    }

    fn visit_return_statement(&mut self, r: &ReturnStatement<'a>) {
        if self.rets.last() == Some(&true) {
            match &r.argument {
                Some(e) => {
                    if self.store("the function returns `int`", e, r.span, false) {
                        self.wrap_value(e);
                    }
                }
                None => self.error(r.span, "the function returns `int`", "is `undefined`"),
            }
        }
        walk::walk_return_statement(self, r);
    }

    fn visit_variable_declarator(&mut self, d: &VariableDeclarator<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &d.id
            && id.symbol_id.get().is_some_and(|s| self.vars.contains(&s))
        {
            let what = format!("`{}` is an `int`", id.name);
            match &d.init {
                Some(init) => {
                    if self.store(&what, init, d.span, false) {
                        self.wrap_value(init);
                    }
                }
                None => {
                    self.errors.push(OxcDiagnostic::error("an `int` variable needs an initializer").with_label(d.span))
                }
            }
        }
        walk::walk_variable_declarator(self, d);
    }

    fn visit_formal_parameter(&mut self, p: &FormalParameter<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &p.pattern
            && id.symbol_id.get().is_some_and(|s| self.vars.contains(&s))
            && let Some(init) = &p.initializer
        {
            let what = format!("`{}` is an `int`", id.name);
            if self.store(&what, init, p.span, false) {
                self.wrap_value(init);
            }
        }
        walk::walk_formal_parameter(self, p);
    }

    fn visit_for_statement(&mut self, f: &ForStatement<'a>) {
        if let Some(u) = &f.update {
            self.mark_unused(u);
        }
        let counter = if self.wrapping { None } else { self.counter_of(f) };
        if let Some(init) = &f.init {
            self.visit_for_statement_init(init);
        }
        if let Some(t) = &f.test {
            self.visit_expression(t);
        }
        self.visit_statement(&f.body);
        if let Some(u) = &f.update {
            self.counters.extend(counter);
            self.visit_expression(u);
            if counter.is_some() {
                self.counters.pop();
            }
        }
    }

    fn visit_update_expression(&mut self, u: &UpdateExpression<'a>) {
        if let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument
            && let Some(sym) = self.symbol(id).filter(|s| self.vars.contains(s))
        {
            let counter = self.counters.contains(&sym);
            if self.wrapping {
                let t = u.argument.span();
                self.wraps.push(IntWrap {
                    span: (u.span.start, u.span.end),
                    target: (t.start, t.end),
                    name: id.name.to_string(),
                    kind: IntWrapKind::Update {
                        inc: u.operator == UpdateOperator::Increment,
                        prefix: u.prefix,
                        used: !self.unused.contains(&u.span),
                    },
                });
            } else if !counter {
                self.error(
                    u.span,
                    &format!("`{}` is an `int`", id.name),
                    "may leave the int32 range (with `--no-int-wrapping`, only a `for` counter bounded by an int, \
                     as in `i < n`, steps by one)",
                );
            }
        }
        walk::walk_update_expression(self, u);
    }

    fn visit_assignment_expression(&mut self, a: &AssignmentExpression<'a>) {
        if let Some(name) = self.int_target(&a.left) {
            let what = format!("`{name}` is an `int`");
            let t = a.left.span();
            use AssignmentOperator as A;
            let op = match a.operator {
                A::Assign => None,
                A::Addition => Some(BinaryOperator::Addition),
                A::Subtraction => Some(BinaryOperator::Subtraction),
                A::Multiplication => Some(BinaryOperator::Multiplication),
                A::ShiftRightZeroFill => Some(BinaryOperator::ShiftRightZeroFill),
                A::BitwiseOR | A::BitwiseAnd | A::BitwiseXOR | A::ShiftLeft | A::ShiftRight => {
                    Some(BinaryOperator::BitwiseOR)
                }
                _ => Some(BinaryOperator::Division),
            };
            match op {
                None => {
                    if self.store(&what, &a.right, a.span, false) {
                        self.wrap_value(&a.right);
                    }
                }
                Some(BinaryOperator::BitwiseOR) => {}
                Some(BinaryOperator::Division) => self.error(a.span, &what, FRACTION),
                Some(op) => {
                    // `x op= e`: an integer when `e` is one.
                    let class = match op {
                        BinaryOperator::ShiftRightZeroFill => Class::Integer,
                        _ => Class::integer_of(self.classify(&a.right), Class::Int32),
                    };
                    match class {
                        Class::Other(why) => self.error(a.span, &what, why),
                        _ if !self.wrapping => self.error(
                            a.span,
                            &what,
                            "may leave the int32 range (with `--no-int-wrapping`, only a `for` counter bounded by \
                             an int, as in `i < n`, steps by one)",
                        ),
                        _ => {
                            let r = a.right.span();
                            let sym = match op {
                                BinaryOperator::Addition => "+",
                                BinaryOperator::Subtraction => "-",
                                BinaryOperator::Multiplication => "*",
                                _ => ">>>",
                            };
                            self.wraps.push(IntWrap {
                                span: (a.span.start, a.span.end),
                                target: (t.start, t.end),
                                name,
                                kind: IntWrapKind::Compound { op: sym, rhs: (r.start, r.end) },
                            });
                        }
                    }
                }
            }
        }
        walk::walk_assignment_expression(self, a);
    }
}
