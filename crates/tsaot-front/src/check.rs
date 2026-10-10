//! Strict-subset checker: decides which module-level functions can be compiled
//! natively and lowers them to the typed IR.
//!
//! The checker is deliberately conservative. A function is native only if
//! every construct in its body is understood here, with exact JS semantics;
//! it does not trust TypeScript's (unsound) type system beyond annotations on
//! parameters, which are re-checked at runtime by the entry stubs. Anything
//! else keeps running in the QuickJS interpreter.

use std::collections::HashMap;

use oxc::ast::ast::*;
use oxc::semantic::{Scoping, SymbolId};
use oxc::span::{GetSpan, Span};

use crate::ir::{self, BinOp, CmpOp, Expr, ExprKind, LogicalOp, MathFn, Stmt, Ty, UnOp};

/// Source location of a rejection.
#[derive(Debug, Clone)]
pub struct Reject {
    pub span: Span,
    pub msg: String,
}

type R<T> = Result<T, Reject>;

fn reject<T>(span: Span, msg: impl Into<String>) -> R<T> {
    Err(Reject { span, msg: msg.into() })
}

/// The syntax of a candidate function.
#[derive(Clone, Copy)]
pub enum FnNode<'a> {
    Function(&'a Function<'a>),
    Arrow(&'a ArrowFunctionExpression<'a>),
}

impl FnNode<'_> {
    pub fn span(&self) -> Span {
        match self {
            FnNode::Function(f) => f.span,
            FnNode::Arrow(a) => a.span,
        }
    }
}

/// Where a candidate is bound, which decides how the source is rewritten.
#[derive(Clone, Copy)]
pub enum Site {
    /// `function f() {}`; `export_prefix` spans `export ` if present.
    Declaration { id: Span, export_prefix: Option<Span> },
    /// `const f = <init>` where `init` is an arrow or function expression.
    ConstInit { init: Span },
}

/// A module-level function binding that might be compiled natively.
pub struct Candidate<'a> {
    pub name: String,
    pub node: FnNode<'a>,
    pub symbol: SymbolId,
    pub site: Site,
    /// Whether other native functions may call this one directly. False for
    /// `const` bindings that may still be in their TDZ when native code runs.
    direct_callable: bool,
    params: Vec<(SymbolId, Ty)>,
    ret: Option<Ty>,
}

#[derive(Clone, Copy)]
enum ConstVal {
    Num(f64),
    Bool(bool),
}

pub struct Outcome<'a> {
    /// Native functions, in source order; index == `ir::FuncId`.
    pub native: Vec<Candidate<'a>>,
    pub module: ir::Module,
    /// Rejected candidates with the reason they stay interpreted.
    pub rejected: Vec<(String, Span, Reject)>,
}

pub fn check<'a>(program: &'a Program<'a>, scoping: &'a Scoping) -> Outcome<'a> {
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    let mut consts = HashMap::new();
    collect_toplevel(program, scoping, &mut candidates, &mut rejected, &mut consts);

    if scoping.root_unresolved_references().contains_key("eval") {
        // Direct eval can read and rebind module-level bindings, so nothing
        // about this module can be assumed statically.
        for c in candidates.drain(..) {
            let span = c.node.span();
            rejected.push((c.name, span, Reject { span, msg: "module uses `eval`".into() }));
        }
    }

    // Fixpoint: a function is native only if all its native callees are. A
    // pass that rejects nothing is self-consistent.
    loop {
        let mut cx =
            ModuleCx { scoping, consts: &consts, cands: &candidates, state: vec![State::Todo; candidates.len()] };
        for i in 0..candidates.len() {
            cx.ensure(i);
        }
        let failed: Vec<usize> = (0..candidates.len()).filter(|&i| matches!(cx.state[i], State::Failed(_))).collect();
        if failed.is_empty() {
            let funcs = cx
                .state
                .into_iter()
                .map(|s| match s {
                    State::Done(f) => f,
                    _ => unreachable!(),
                })
                .collect();
            return Outcome { native: candidates, module: ir::Module { funcs }, rejected };
        }
        let mut reasons: HashMap<usize, Reject> = HashMap::new();
        for &i in &failed {
            if let State::Failed(r) = &cx.state[i] {
                reasons.insert(i, r.clone());
            }
        }
        let mut keep = Vec::new();
        for (i, c) in candidates.into_iter().enumerate() {
            match reasons.remove(&i) {
                Some(r) => rejected.push((c.name, c.node.span(), r)),
                None => keep.push(c),
            }
        }
        candidates = keep;
    }
}

fn collect_toplevel<'a>(
    program: &'a Program<'a>,
    scoping: &'a Scoping,
    out: &mut Vec<Candidate<'a>>,
    rejected: &mut Vec<(String, Span, Reject)>,
    consts: &mut HashMap<SymbolId, ConstVal>,
) {
    // Module-level consts can be folded, and const-bound functions called
    // directly from native code, only while no code has run yet: before the
    // first statement that is not a pure declaration, nothing can observe
    // them in their TDZ.
    let mut pure_prefix = true;
    let mut add = |name: String, node: FnNode<'a>, symbol: SymbolId, site: Site, direct_callable: bool| {
        if scoping.get_resolved_references(symbol).any(|r| r.is_write()) {
            let r = Reject { span: node.span(), msg: format!("`{name}` is reassigned") };
            rejected.push((name, node.span(), r));
            return;
        }
        match header(node) {
            Ok((params, ret)) => out.push(Candidate { name, node, symbol, site, direct_callable, params, ret }),
            Err(r) => rejected.push((name, node.span(), r)),
        }
    };
    for stmt in &program.body {
        let (decl, export_prefix) = match stmt {
            Statement::ExportDeclaration(e) => {
                (&e.declaration, Some(Span::new(e.span.start, e.declaration.span().start)))
            }
            _ => match stmt.as_declaration() {
                Some(d) => (d, None),
                None => {
                    if !matches!(stmt, Statement::ImportDeclaration(_) | Statement::EmptyStatement(_)) {
                        pure_prefix = false;
                    }
                    continue;
                }
            },
        };
        match decl {
            Declaration::FunctionDeclaration(f) if f.body.is_some() => {
                let Some(id) = &f.id else { continue };
                let symbol = id.symbol_id.get().expect("semantic analysis ran");
                let site = Site::Declaration { id: id.span, export_prefix };
                // Function declarations are hoisted: always initialized.
                add(id.name.to_string(), FnNode::Function(f), symbol, site, true);
            }
            Declaration::FunctionDeclaration(_) => {} // overload signature or `declare`
            Declaration::VariableDeclaration(v) if v.kind == VariableDeclarationKind::Const && !v.declare => {
                for d in &v.declarations {
                    let (BindingPattern::BindingIdentifier(id), Some(init)) = (&d.id, &d.init) else {
                        pure_prefix = false;
                        continue;
                    };
                    let symbol = id.symbol_id.get().unwrap();
                    let site = Site::ConstInit { init: init.span() };
                    // Creating a function runs no code, so it keeps the prefix pure.
                    match init.without_parentheses() {
                        Expression::ArrowFunctionExpression(a) => {
                            add(id.name.to_string(), FnNode::Arrow(a), symbol, site, pure_prefix);
                        }
                        Expression::FunctionExpression(f) => {
                            add(id.name.to_string(), FnNode::Function(f), symbol, site, pure_prefix);
                        }
                        _ => match literal(init) {
                            Some(val) if pure_prefix => {
                                consts.insert(symbol, val);
                            }
                            _ => pure_prefix = false,
                        },
                    }
                }
            }
            Declaration::VariableDeclaration(v) if v.declare => {}
            Declaration::VariableDeclaration(_) | Declaration::ClassDeclaration(_) => pure_prefix = false,
            // Pure type-level declarations; enums and namespaces emit code but
            // nothing that can call into this module.
            _ => {}
        }
    }
}

fn literal(e: &Expression) -> Option<ConstVal> {
    match e.without_parentheses() {
        Expression::NumericLiteral(n) => Some(ConstVal::Num(n.value)),
        Expression::BooleanLiteral(b) => Some(ConstVal::Bool(b.value)),
        Expression::UnaryExpression(u) if u.operator == UnaryOperator::UnaryNegation => match literal(&u.argument)? {
            ConstVal::Num(v) => Some(ConstVal::Num(-v)),
            ConstVal::Bool(_) => None,
        },
        _ => None,
    }
}

fn ts_type(t: &TSType) -> Option<Ty> {
    match t {
        TSType::TSNumberKeyword(_) => Some(Ty::F64),
        TSType::TSBooleanKeyword(_) => Some(Ty::Bool),
        TSType::TSVoidKeyword(_) => Some(Ty::Void),
        TSType::TSParenthesizedType(p) => ts_type(&p.type_annotation),
        _ => None,
    }
}

/// Parameter symbols with their types, and the annotated return type if any.
type Header = (Vec<(SymbolId, Ty)>, Option<Ty>);

fn header(node: FnNode) -> R<Header> {
    let (span, is_async, generator, generic, this_param, params, return_type) = match node {
        FnNode::Function(f) => (
            f.span,
            f.r#async,
            f.generator,
            f.type_parameters.is_some(),
            f.this_param.is_some(),
            &f.params,
            &f.return_type,
        ),
        FnNode::Arrow(a) => (a.span, a.r#async, false, a.type_parameters.is_some(), false, &a.params, &a.return_type),
    };
    if is_async {
        return reject(span, "async functions are not supported yet");
    }
    if generator {
        return reject(span, "generators are not supported yet");
    }
    if generic {
        return reject(span, "generic functions are not supported yet");
    }
    if this_param {
        return reject(span, "`this` parameters are not supported");
    }
    if let Some(rest) = &params.rest {
        return reject(rest.span, "rest parameters are not supported yet");
    }
    let formal = params;
    let mut params = Vec::new();
    for p in &formal.items {
        let BindingPattern::BindingIdentifier(id) = &p.pattern else {
            return reject(p.span, "destructuring parameters are not supported");
        };
        if p.initializer.is_some() || p.optional {
            return reject(p.span, "optional parameters are not supported");
        }
        let ty = p.type_annotation.as_ref().and_then(|a| ts_type(&a.type_annotation));
        match ty {
            Some(ty @ (Ty::F64 | Ty::Bool)) => params.push((id.symbol_id.get().unwrap(), ty)),
            _ => return reject(p.span, format!("parameter `{}` must be annotated `number` or `boolean`", id.name)),
        }
    }
    let ret = match return_type {
        None => None,
        Some(a) => match ts_type(&a.type_annotation) {
            Some(t) => Some(t),
            None => return reject(a.span, "return type must be `number`, `boolean` or `void`"),
        },
    };
    Ok((params, ret))
}

#[derive(Clone)]
enum State {
    Todo,
    InProgress,
    Done(ir::Func),
    Failed(Reject),
}

struct ModuleCx<'m, 'a> {
    scoping: &'a Scoping,
    consts: &'m HashMap<SymbolId, ConstVal>,
    cands: &'m [Candidate<'a>],
    state: Vec<State>,
}

impl<'m, 'a> ModuleCx<'m, 'a> {
    fn ensure(&mut self, i: usize) {
        if !matches!(self.state[i], State::Todo) {
            return;
        }
        self.state[i] = State::InProgress;
        let c = &self.cands[i];
        let mut fx = FnCx {
            m: self,
            locals: Vec::new(),
            mutable: Vec::new(),
            sym_local: HashMap::new(),
            loops: Vec::new(),
            next_loop: 0,
            label: None,
            ret: c.ret,
            returns: Vec::new(),
        };
        let result = fx.function(c);
        self.state[i] = match result {
            Ok(f) => State::Done(f),
            Err(r) => State::Failed(r),
        };
    }

    /// Signature of candidate `i` as seen by a caller.
    fn signature(&mut self, i: usize, call_span: Span) -> R<(Vec<Ty>, Ty)> {
        if !self.cands[i].direct_callable {
            return reject(
                call_span,
                format!(
                    "calls `{}`, which may be uninitialized when this runs (its `const` is declared after top-level code)",
                    self.cands[i].name
                ),
            );
        }
        let params = self.cands[i].params.iter().map(|p| p.1).collect();
        if let Some(ret) = self.cands[i].ret {
            // Annotated: callers may proceed optimistically, even while the
            // callee is still being checked (recursion). The fixpoint undoes
            // this if the callee turns out not to be native.
            if let State::Failed(_) = self.state[i] {
                return reject(call_span, format!("calls `{}`, which is not compiled natively", self.cands[i].name));
            }
            return Ok((params, ret));
        }
        self.ensure(i);
        match &self.state[i] {
            State::Done(f) => Ok((params, f.ret)),
            State::InProgress => reject(
                call_span,
                format!("recursive call to `{}`, which has no return type annotation", self.cands[i].name),
            ),
            State::Failed(_) => {
                reject(call_span, format!("calls `{}`, which is not compiled natively", self.cands[i].name))
            }
            State::Todo => unreachable!(),
        }
    }
}

struct FnCx<'x, 'm, 'a> {
    m: &'x mut ModuleCx<'m, 'a>,
    locals: Vec<Ty>,
    mutable: Vec<bool>,
    /// Locals that have been declared so far in program order. A reference to
    /// a local symbol not in here would hit its TDZ.
    sym_local: HashMap<SymbolId, ir::LocalId>,
    loops: Vec<(Option<String>, ir::LoopId)>,
    next_loop: ir::LoopId,
    /// Label attached to the loop statement about to be checked.
    label: Option<String>,
    ret: Option<Ty>,
    returns: Vec<(Ty, Span)>,
}

impl FnCx<'_, '_, '_> {
    fn function(&mut self, c: &Candidate) -> R<ir::Func> {
        let mut params = Vec::new();
        for &(sym, ty) in &c.params {
            self.declare(sym, ty, true);
            params.push(ty);
        }
        // Directives ("use strict", "use aot") have no effect in module code.
        let stmts = match c.node {
            FnNode::Function(f) => self.stmts(&f.body.as_ref().unwrap().statements)?,
            FnNode::Arrow(a) => match &a.body {
                ArrowFunctionBody::FunctionBody(b) => self.stmts(&b.statements)?,
                body => {
                    // Concise body: `(x) => expr` returns `expr`.
                    let e = body.as_expression().expect("concise arrow body is an expression");
                    let x = self.expr(e)?;
                    self.returns.push((x.ty, e.span()));
                    if let Some(expected) = self.ret
                        && expected != x.ty
                    {
                        return reject(
                            e.span(),
                            format!("returns `{}` from a function declared to return `{expected}`", x.ty),
                        );
                    }
                    vec![Stmt::Return(Some(x))]
                }
            },
        };
        let completes = completes_list(&stmts);
        let ret = match self.ret {
            Some(t) => t,
            None => {
                let mut ret = None;
                for &(t, span) in &self.returns {
                    match ret {
                        None => ret = Some(t),
                        Some(r) if r != t => {
                            return reject(span, format!("returns both `{r}` and `{t}`"));
                        }
                        _ => {}
                    }
                }
                ret.unwrap_or(Ty::Void)
            }
        };
        if ret != Ty::Void && completes {
            return reject(c.node.span(), "not all code paths return a value (would return `undefined`)");
        }
        Ok(ir::Func { name: c.name.clone(), params, ret, locals: std::mem::take(&mut self.locals), body: stmts })
    }

    fn declare(&mut self, sym: SymbolId, ty: Ty, mutable: bool) -> ir::LocalId {
        let id = self.locals.len();
        self.locals.push(ty);
        self.mutable.push(mutable);
        self.sym_local.insert(sym, id);
        id
    }

    // ---- statements ----

    fn stmts(&mut self, list: &[Statement]) -> R<Vec<Stmt>> {
        let mut out = Vec::with_capacity(list.len());
        for s in list {
            if let Some(s) = self.stmt(s)? {
                out.push(s);
            }
        }
        Ok(out)
    }

    fn body(&mut self, s: &Statement) -> R<Vec<Stmt>> {
        Ok(match self.stmt(s)? {
            Some(Stmt::Block(b)) => b,
            Some(s) => vec![s],
            None => vec![],
        })
    }

    fn stmt(&mut self, s: &Statement) -> R<Option<Stmt>> {
        let label = self.label.take();
        if label.is_some()
            && !matches!(s, Statement::WhileStatement(_) | Statement::DoWhileStatement(_) | Statement::ForStatement(_))
        {
            return reject(s.span(), "labels are only supported on loops");
        }
        Ok(Some(match s {
            Statement::EmptyStatement(_) => return Ok(None),
            Statement::ExpressionStatement(e) => Stmt::Expr(self.expr(&e.expression)?),
            Statement::BlockStatement(b) => Stmt::Block(self.stmts(&b.body)?),
            Statement::VariableDeclaration(v) => Stmt::Block(self.var_decl(v)?),
            Statement::IfStatement(i) => {
                let test = self.cond(&i.test)?;
                let then = self.body(&i.consequent)?;
                let els = match &i.alternate {
                    Some(a) => self.body(a)?,
                    None => vec![],
                };
                Stmt::If(test, then, els)
            }
            Statement::WhileStatement(w) => {
                let test = self.cond(&w.test)?;
                self.lp(label, |cx, id| {
                    Ok(Stmt::Loop { id, test: Some(test), post_test: false, body: cx.body(&w.body)?, update: None })
                })?
            }
            Statement::DoWhileStatement(w) => self.lp(label, |cx, id| {
                let body = cx.body(&w.body)?;
                let test = cx.cond(&w.test)?;
                Ok(Stmt::Loop { id, test: Some(test), post_test: true, body, update: None })
            })?,
            Statement::ForStatement(f) => {
                let init = match &f.init {
                    None => vec![],
                    Some(ForStatementInit::VariableDeclaration(v)) => self.var_decl(v)?,
                    Some(init) => vec![Stmt::Expr(self.expr(init.to_expression())?)],
                };
                let test = f.test.as_ref().map(|t| self.cond(t)).transpose()?;
                let lp = self.lp(label, |cx, id| {
                    let body = cx.body(&f.body)?;
                    let update = f.update.as_ref().map(|u| cx.expr(u)).transpose()?;
                    Ok(Stmt::Loop { id, test, post_test: false, body, update })
                })?;
                let mut block = init;
                block.push(lp);
                Stmt::Block(block)
            }
            Statement::LabeledStatement(l) => {
                self.label = Some(l.label.name.to_string());
                return self.stmt(&l.body);
            }
            Statement::BreakStatement(b) => Stmt::Break(self.jump_target(b.label.as_ref(), b.span)?),
            Statement::ContinueStatement(c) => Stmt::Continue(self.jump_target(c.label.as_ref(), c.span)?),
            Statement::ReturnStatement(r) => self.ret_stmt(r)?,
            other => return reject(other.span(), format!("unsupported statement: {}", describe_stmt(other))),
        }))
    }

    fn lp(&mut self, label: Option<String>, f: impl FnOnce(&mut Self, ir::LoopId) -> R<Stmt>) -> R<Stmt> {
        let id = self.next_loop;
        self.next_loop += 1;
        self.loops.push((label, id));
        let r = f(self, id);
        self.loops.pop();
        r
    }

    fn jump_target(&self, label: Option<&LabelIdentifier>, span: Span) -> R<ir::LoopId> {
        let found = match label {
            None => self.loops.last(),
            Some(l) => self.loops.iter().rev().find(|(n, _)| n.as_deref() == Some(l.name.as_str())),
        };
        match found {
            Some(&(_, id)) => Ok(id),
            None => reject(span, "jump target not found"),
        }
    }

    fn ret_stmt(&mut self, r: &ReturnStatement) -> R<Stmt> {
        let arg = r.argument.as_ref().map(|a| self.expr(a)).transpose()?;
        let ty = arg.as_ref().map_or(Ty::Void, |a| a.ty);
        match self.ret {
            Some(Ty::Void) if ty == Ty::Void => {}
            Some(expected) if expected != ty => {
                return reject(r.span, format!("returns `{ty}` from a function declared to return `{expected}`"));
            }
            _ => {}
        }
        self.returns.push((ty, r.span));
        Ok(Stmt::Return(arg))
    }

    fn var_decl(&mut self, v: &VariableDeclaration) -> R<Vec<Stmt>> {
        let mutable = match v.kind {
            VariableDeclarationKind::Let => true,
            VariableDeclarationKind::Const => false,
            _ => return reject(v.span, "only `let` and `const` declarations are supported"),
        };
        let mut out = Vec::new();
        for d in &v.declarations {
            let BindingPattern::BindingIdentifier(id) = &d.id else {
                return reject(d.span, "destructuring is not supported");
            };
            let Some(init) = &d.init else {
                return reject(d.span, format!("`{}` must be initialized", id.name));
            };
            // The initializer is checked before the binding exists, so a
            // self-reference is rejected exactly like the TDZ would throw.
            let init = self.expr(init)?;
            if let Some(a) = &d.type_annotation
                && ts_type(&a.type_annotation) != Some(init.ty)
            {
                return reject(a.span, format!("annotation does not match initializer type `{}`", init.ty));
            }
            if init.ty == Ty::Void {
                return reject(d.span, "cannot store the result of a `void` function");
            }
            let local = self.declare(id.symbol_id.get().unwrap(), init.ty, mutable);
            out.push(Stmt::Expr(Expr::new(init.ty, ExprKind::SetLocal(local, Box::new(init)))));
        }
        Ok(out)
    }

    // ---- expressions ----

    /// An expression used as a condition: converted with JS truthiness.
    fn cond(&mut self, e: &Expression) -> R<Expr> {
        let x = self.expr(e)?;
        truthy(x, e.span())
    }

    fn expr(&mut self, e: &Expression) -> R<Expr> {
        let span = e.span();
        Ok(match e {
            Expression::NumericLiteral(n) => Expr::num(n.value),
            Expression::BooleanLiteral(b) => Expr::new(Ty::Bool, ExprKind::Bool(b.value)),
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression)?,
            Expression::Identifier(id) => self.ident(id)?,
            Expression::StaticMemberExpression(m) => match builtin_const(m, self.m.scoping) {
                Some(v) => Expr::num(v),
                None => return reject(span, "property access is not supported"),
            },
            Expression::UnaryExpression(u) => self.unary(u)?,
            Expression::BinaryExpression(b) => self.binary(b)?,
            Expression::LogicalExpression(l) => {
                let op = match l.operator {
                    LogicalOperator::And => LogicalOp::And,
                    LogicalOperator::Or => LogicalOp::Or,
                    LogicalOperator::Coalesce => return reject(span, "`??` is not supported on non-nullable values"),
                };
                let a = self.expr(&l.left)?;
                let b = self.expr(&l.right)?;
                if a.ty != b.ty || a.ty == Ty::Void {
                    return reject(
                        span,
                        format!("operands of `{}` have types `{}` and `{}`", l.operator.as_str(), a.ty, b.ty),
                    );
                }
                Expr::new(a.ty, ExprKind::Logical(op, Box::new(a), Box::new(b)))
            }
            Expression::ConditionalExpression(c) => {
                let test = self.cond(&c.test)?;
                let a = self.expr(&c.consequent)?;
                let b = self.expr(&c.alternate)?;
                if a.ty != b.ty {
                    return reject(span, format!("conditional branches have types `{}` and `{}`", a.ty, b.ty));
                }
                Expr::new(a.ty, ExprKind::Cond(Box::new(test), Box::new(a), Box::new(b)))
            }
            Expression::AssignmentExpression(a) => self.assign(a)?,
            Expression::UpdateExpression(u) => {
                let (local, ty) = self.target_local(&u.argument)?;
                if ty != Ty::F64 {
                    return reject(span, "`++`/`--` require a `number`");
                }
                let delta = if u.operator == UpdateOperator::Increment { 1.0 } else { -1.0 };
                Expr::new(Ty::F64, ExprKind::Update { local, delta, prefix: u.prefix })
            }
            Expression::SequenceExpression(s) => {
                let xs = s.expressions.iter().map(|x| self.expr(x)).collect::<R<Vec<_>>>()?;
                Expr::new(xs.last().unwrap().ty, ExprKind::Seq(xs))
            }
            Expression::CallExpression(c) => self.call(c)?,
            // Type-level wrappers are no-ops when they do not change the type.
            Expression::TSNonNullExpression(n) => self.expr(&n.expression)?,
            Expression::TSAsExpression(a) => self.same_type(&a.expression, &a.type_annotation, span)?,
            Expression::TSSatisfiesExpression(a) => self.same_type(&a.expression, &a.type_annotation, span)?,
            other => return reject(span, format!("unsupported expression: {}", describe_expr(other))),
        })
    }

    fn same_type(&mut self, e: &Expression, t: &TSType, span: Span) -> R<Expr> {
        let x = self.expr(e)?;
        if ts_type(t) != Some(x.ty) {
            return reject(span, "type assertions that change the type are not supported");
        }
        Ok(x)
    }

    fn ident(&mut self, id: &IdentifierReference) -> R<Expr> {
        let scoping = self.m.scoping;
        let sym = id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id());
        match sym {
            Some(sym) => {
                if let Some(&l) = self.sym_local.get(&sym) {
                    return Ok(Expr::new(self.locals[l], ExprKind::Local(l)));
                }
                match self.m.consts.get(&sym) {
                    Some(ConstVal::Num(v)) => Ok(Expr::num(*v)),
                    Some(ConstVal::Bool(b)) => Ok(Expr::new(Ty::Bool, ExprKind::Bool(*b))),
                    None => reject(id.span, format!("`{}` is not a local variable or compile-time constant", id.name)),
                }
            }
            None => match id.name.as_str() {
                "NaN" => Ok(Expr::num(f64::NAN)),
                "Infinity" => Ok(Expr::num(f64::INFINITY)),
                name => reject(id.span, format!("global `{name}` is not supported")),
            },
        }
    }

    fn target_local(&self, t: &SimpleAssignmentTarget) -> R<(ir::LocalId, Ty)> {
        let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = t else {
            return reject(t.span(), "only local variables can be assigned");
        };
        let scoping = self.m.scoping;
        let sym = id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id());
        match sym.and_then(|s| self.sym_local.get(&s)) {
            Some(&l) if self.mutable[l] => Ok((l, self.locals[l])),
            Some(_) => reject(id.span, format!("assignment to constant `{}`", id.name)),
            None => reject(id.span, format!("`{}` is not a local variable", id.name)),
        }
    }

    fn assign(&mut self, a: &AssignmentExpression) -> R<Expr> {
        let Some(target) = a.left.as_simple_assignment_target() else {
            return reject(a.span, "destructuring assignment is not supported");
        };
        let (local, ty) = self.target_local(target)?;
        let rhs = self.expr(&a.right)?;
        let value = if a.operator == AssignmentOperator::Assign {
            if rhs.ty != ty {
                return reject(a.span, format!("cannot assign `{}` to a `{ty}` variable", rhs.ty));
            }
            rhs
        } else {
            let op = match a.operator {
                AssignmentOperator::Addition => BinOp::Add,
                AssignmentOperator::Subtraction => BinOp::Sub,
                AssignmentOperator::Multiplication => BinOp::Mul,
                AssignmentOperator::Division => BinOp::Div,
                AssignmentOperator::Remainder => BinOp::Rem,
                AssignmentOperator::Exponential => BinOp::Pow,
                AssignmentOperator::ShiftLeft => BinOp::Shl,
                AssignmentOperator::ShiftRight => BinOp::Sar,
                AssignmentOperator::ShiftRightZeroFill => BinOp::Shr,
                AssignmentOperator::BitwiseOR => BinOp::BitOr,
                AssignmentOperator::BitwiseXOR => BinOp::BitXor,
                AssignmentOperator::BitwiseAnd => BinOp::BitAnd,
                _ => return reject(a.span, format!("`{}` is not supported", a.operator.as_str())),
            };
            if ty != Ty::F64 || rhs.ty != Ty::F64 {
                return reject(a.span, format!("`{}` requires `number` operands", a.operator.as_str()));
            }
            let cur = Expr::new(ty, ExprKind::Local(local));
            Expr::new(Ty::F64, ExprKind::Binary(op, Box::new(cur), Box::new(rhs)))
        };
        Ok(Expr::new(ty, ExprKind::SetLocal(local, Box::new(value))))
    }

    fn unary(&mut self, u: &UnaryExpression) -> R<Expr> {
        let x = self.expr(&u.argument)?;
        let num = |x: &Expr| -> R<()> {
            if x.ty == Ty::F64 {
                Ok(())
            } else {
                reject(u.span, format!("`{}` requires a `number`", u.operator.as_str()))
            }
        };
        Ok(match u.operator {
            UnaryOperator::UnaryNegation => {
                num(&x)?;
                Expr::new(Ty::F64, ExprKind::Unary(UnOp::Neg, Box::new(x)))
            }
            UnaryOperator::UnaryPlus => {
                num(&x)?;
                x
            }
            UnaryOperator::BitwiseNot => {
                num(&x)?;
                Expr::new(Ty::F64, ExprKind::Unary(UnOp::BitNot, Box::new(x)))
            }
            UnaryOperator::LogicalNot => {
                let b = truthy(x, u.span)?;
                Expr::new(Ty::Bool, ExprKind::Unary(UnOp::Not, Box::new(b)))
            }
            op => return reject(u.span, format!("`{}` is not supported", op.as_str())),
        })
    }

    fn binary(&mut self, b: &BinaryExpression) -> R<Expr> {
        use BinaryOperator as B;
        let x = self.expr(&b.left)?;
        let y = self.expr(&b.right)?;
        let cmp = match b.operator {
            B::LessThan => Some(CmpOp::Lt),
            B::LessEqualThan => Some(CmpOp::Le),
            B::GreaterThan => Some(CmpOp::Gt),
            B::GreaterEqualThan => Some(CmpOp::Ge),
            B::Equality | B::StrictEquality => Some(CmpOp::Eq),
            B::Inequality | B::StrictInequality => Some(CmpOp::Ne),
            _ => None,
        };
        if let Some(op) = cmp {
            let ok = x.ty == y.ty
                && match x.ty {
                    Ty::F64 => true,
                    Ty::Bool => matches!(op, CmpOp::Eq | CmpOp::Ne),
                    Ty::Void => false,
                };
            if !ok {
                return reject(
                    b.span,
                    format!("cannot compare `{}` with `{}` using `{}`", x.ty, y.ty, b.operator.as_str()),
                );
            }
            return Ok(Expr::new(Ty::Bool, ExprKind::Compare(op, Box::new(x), Box::new(y))));
        }
        let op = match b.operator {
            B::Addition => BinOp::Add,
            B::Subtraction => BinOp::Sub,
            B::Multiplication => BinOp::Mul,
            B::Division => BinOp::Div,
            B::Remainder => BinOp::Rem,
            B::Exponential => BinOp::Pow,
            B::ShiftLeft => BinOp::Shl,
            B::ShiftRight => BinOp::Sar,
            B::ShiftRightZeroFill => BinOp::Shr,
            B::BitwiseOR => BinOp::BitOr,
            B::BitwiseXOR => BinOp::BitXor,
            B::BitwiseAnd => BinOp::BitAnd,
            op => return reject(b.span, format!("`{}` is not supported", op.as_str())),
        };
        if x.ty != Ty::F64 || y.ty != Ty::F64 {
            return reject(
                b.span,
                format!("`{}` requires `number` operands, got `{}` and `{}`", b.operator.as_str(), x.ty, y.ty),
            );
        }
        Ok(Expr::new(Ty::F64, ExprKind::Binary(op, Box::new(x), Box::new(y))))
    }

    fn call(&mut self, c: &CallExpression) -> R<Expr> {
        if c.optional {
            return reject(c.span, "optional calls are not supported");
        }
        if c.type_arguments.is_some() {
            return reject(c.span, "explicit type arguments are not supported");
        }
        let mut args = Vec::with_capacity(c.arguments.len());
        for a in &c.arguments {
            match a.as_expression() {
                Some(e) => args.push(self.expr(e)?),
                None => return reject(a.span(), "spread arguments are not supported"),
            }
        }
        match &c.callee {
            Expression::Identifier(id) => {
                let scoping = self.m.scoping;
                let sym = id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id());
                let Some(i) = sym.and_then(|s| self.m.cands.iter().position(|c| c.symbol == s)) else {
                    return reject(c.span, format!("call to `{}`, which is not a natively compiled function", id.name));
                };
                let (params, ret) = self.m.signature(i, c.span)?;
                let tys: Vec<Ty> = args.iter().map(|a| a.ty).collect();
                if tys != params {
                    return reject(
                        c.span,
                        format!("call to `{}` with argument types {tys:?}, expected {params:?}", id.name),
                    );
                }
                Ok(Expr::new(ret, ExprKind::Call(i, args)))
            }
            Expression::StaticMemberExpression(m) => {
                let Some(f) = builtin_fn(m, self.m.scoping) else {
                    return reject(c.span, "method calls are not supported");
                };
                if args.iter().any(|a| a.ty != Ty::F64) {
                    return reject(c.span, "math functions require `number` arguments");
                }
                math_call(f, args, c.span)
            }
            _ => reject(c.span, "unsupported callee"),
        }
    }
}

fn truthy(x: Expr, span: Span) -> R<Expr> {
    match x.ty {
        Ty::Bool => Ok(x),
        Ty::F64 => Ok(Expr::new(Ty::Bool, ExprKind::Truthy(Box::new(x)))),
        Ty::Void => reject(span, "`void` value used as a condition"),
    }
}

fn math_call(f: MathFn, mut args: Vec<Expr>, span: Span) -> R<Expr> {
    if matches!(f, MathFn::Min | MathFn::Max) {
        // Math.min()/max() are n-ary; fold into a left-to-right chain, which
        // keeps evaluation order and NaN / -0 semantics.
        if args.is_empty() {
            return Ok(Expr::num(if f == MathFn::Min { f64::INFINITY } else { f64::NEG_INFINITY }));
        }
        let mut it = args.into_iter();
        let mut acc = it.next().unwrap();
        for a in it {
            acc = Expr::new(Ty::F64, ExprKind::Math(f, vec![acc, a]));
        }
        return Ok(acc);
    }
    if args.len() != f.arity() {
        return reject(span, format!("expected {} argument(s)", f.arity()));
    }
    let ret = f.ret();
    args.truncate(f.arity());
    Ok(Expr::new(ret, ExprKind::Math(f, args)))
}

/// `Math.X` / `Number.X` where the object is the (unshadowed) global.
fn global_member<'s>(m: &'s StaticMemberExpression, scoping: &Scoping) -> Option<(&'s str, &'s str)> {
    if m.optional {
        return None;
    }
    let Expression::Identifier(obj) = &m.object else { return None };
    // A reference that resolves to a symbol means a declaration shadows the global.
    if obj.reference_id.get().is_none_or(|r| scoping.get_reference(r).symbol_id().is_some()) {
        return None;
    }
    Some((obj.name.as_str(), m.property.name.as_str()))
}

fn builtin_const(m: &StaticMemberExpression, scoping: &Scoping) -> Option<f64> {
    use std::f64::consts as c;
    Some(match global_member(m, scoping)? {
        ("Math", "PI") => c::PI,
        ("Math", "E") => c::E,
        ("Math", "LN2") => c::LN_2,
        ("Math", "LN10") => c::LN_10,
        ("Math", "LOG2E") => c::LOG2_E,
        ("Math", "LOG10E") => c::LOG10_E,
        ("Math", "SQRT2") => c::SQRT_2,
        ("Math", "SQRT1_2") => c::FRAC_1_SQRT_2,
        ("Number", "EPSILON") => f64::EPSILON,
        ("Number", "MAX_VALUE") => f64::MAX,
        ("Number", "MIN_VALUE") => 5e-324,
        ("Number", "MAX_SAFE_INTEGER") => 9007199254740991.0,
        ("Number", "MIN_SAFE_INTEGER") => -9007199254740991.0,
        ("Number", "POSITIVE_INFINITY") => f64::INFINITY,
        ("Number", "NEGATIVE_INFINITY") => f64::NEG_INFINITY,
        ("Number", "NaN") => f64::NAN,
        _ => return None,
    })
}

fn builtin_fn(m: &StaticMemberExpression, scoping: &Scoping) -> Option<MathFn> {
    use MathFn::*;
    Some(match global_member(m, scoping)? {
        ("Math", "abs") => Abs,
        ("Math", "sqrt") => Sqrt,
        ("Math", "floor") => Floor,
        ("Math", "ceil") => Ceil,
        ("Math", "trunc") => Trunc,
        ("Math", "fround") => Fround,
        ("Math", "min") => Min,
        ("Math", "max") => Max,
        ("Math", "imul") => Imul,
        ("Math", "clz32") => Clz32,
        ("Math", "sign") => Sign,
        ("Math", "round") => Round,
        ("Math", "pow") => Pow,
        ("Math", "sin") => Sin,
        ("Math", "cos") => Cos,
        ("Math", "tan") => Tan,
        ("Math", "asin") => Asin,
        ("Math", "acos") => Acos,
        ("Math", "atan") => Atan,
        ("Math", "atan2") => Atan2,
        ("Math", "sinh") => Sinh,
        ("Math", "cosh") => Cosh,
        ("Math", "tanh") => Tanh,
        ("Math", "asinh") => Asinh,
        ("Math", "acosh") => Acosh,
        ("Math", "atanh") => Atanh,
        ("Math", "exp") => Exp,
        ("Math", "expm1") => Expm1,
        ("Math", "log") => Log,
        ("Math", "log1p") => Log1p,
        ("Math", "log2") => Log2,
        ("Math", "log10") => Log10,
        ("Math", "cbrt") => Cbrt,
        ("Math", "hypot") => Hypot,
        ("Number", "isNaN") => IsNaN,
        ("Number", "isFinite") => IsFinite,
        ("Number", "isInteger") => IsInteger,
        ("Number", "isSafeInteger") => IsSafeInteger,
        _ => return None,
    })
}

// ---- reachability ----

/// Whether execution can fall off the end of `list`.
fn completes_list(list: &[Stmt]) -> bool {
    list.iter().all(completes)
}

fn completes(s: &Stmt) -> bool {
    match s {
        Stmt::Expr(_) => true,
        Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) => false,
        Stmt::Block(b) => completes_list(b),
        Stmt::If(_, a, b) => completes_list(a) || completes_list(b),
        Stmt::Loop { id, test, body, .. } => {
            let infinite = matches!(test, None | Some(Expr { kind: ExprKind::Bool(true), .. }));
            !infinite || breaks_to(body, *id)
        }
    }
}

fn breaks_to(list: &[Stmt], id: ir::LoopId) -> bool {
    list.iter().any(|s| match s {
        Stmt::Break(t) => *t == id,
        Stmt::Block(b) => breaks_to(b, id),
        Stmt::If(_, a, b) => breaks_to(a, id) || breaks_to(b, id),
        Stmt::Loop { body, .. } => breaks_to(body, id),
        _ => false,
    })
}

fn describe_stmt(s: &Statement) -> &'static str {
    match s {
        Statement::ThrowStatement(_) => "throw",
        Statement::TryStatement(_) => "try",
        Statement::SwitchStatement(_) => "switch",
        Statement::ForInStatement(_) => "for-in",
        Statement::ForOfStatement(_) => "for-of",
        Statement::FunctionDeclaration(_) => "nested function",
        Statement::ClassDeclaration(_) => "class",
        _ => "this statement kind",
    }
}

fn describe_expr(e: &Expression) -> &'static str {
    match e {
        Expression::StringLiteral(_) | Expression::TemplateLiteral(_) => "string",
        Expression::NullLiteral(_) => "null",
        Expression::BigIntLiteral(_) => "bigint",
        Expression::ArrayExpression(_) => "array literal",
        Expression::ObjectExpression(_) => "object literal",
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => "closure",
        Expression::ThisExpression(_) => "this",
        Expression::NewExpression(_) => "new",
        Expression::AwaitExpression(_) => "await",
        Expression::ComputedMemberExpression(_) => "indexing",
        Expression::ImportExpression(_) => "dynamic import",
        _ => "this expression kind",
    }
}
