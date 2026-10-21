//! A compact, readable listing of the typed IR (`luminous ir --compact`):
//! one statement per line, expressions in a JS-like notation. `lN` is local
//! `N`, `.name` a property, `new{...}` / `new C{...}` an allocation.

use std::fmt::Write;

use crate::ir::*;

pub fn module(m: &Module) -> String {
    let mut out = String::new();
    for (i, f) in m.funcs.iter().enumerate() {
        func(&mut out, m, &format!("lum_fn_{i}"), f);
    }
    for (i, f) in m.closures.iter().enumerate() {
        func(&mut out, m, &format!("closure {i}"), f);
    }
    out
}

fn func(out: &mut String, m: &Module, id: &str, f: &Func) {
    let mut flags = Vec::new();
    if f.is_async {
        flags.push(if f.frameless { "async(frameless)" } else { "async" });
    }
    if f.is_generator {
        flags.push("generator");
    }
    if f.has_this {
        flags.push("this");
    }
    if f.is_closure {
        flags.push("closure");
    }
    let params: Vec<String> = f.params.iter().enumerate().map(|(i, t)| format!("l{i}: {}", ty(*t))).collect();
    let _ = writeln!(
        out,
        "// {id}\nfunction {}({}): {} [{}] locals {}",
        f.name,
        params.join(", "),
        ty(f.ret),
        flags.join(" "),
        f.locals.len()
    );
    if !f.captures.is_empty() {
        let _ = writeln!(out, "  captures {:?}", f.captures);
    }
    let mut p = P { m, out: String::new() };
    p.stmts(&f.body, 1);
    out.push_str(&p.out);
    out.push('\n');
}

fn ty(t: Ty) -> String {
    match t {
        Ty::F64 => "f64".into(),
        Ty::Bool => "bool".into(),
        Ty::Void => "void".into(),
        Ty::Any => "any".into(),
        Ty::I32 => "i32".into(),
        Ty::MaybeF64 => "f64?".into(),
        Ty::Arr(k) => k.name().into(),
        Ty::Struct(k) => format!("struct{k}"),
        Ty::StructArr(k) => format!("struct{k}[]"),
        Ty::StructOrUndef(k) => format!("struct{k}?"),
    }
}

struct P<'a> {
    m: &'a Module,
    out: String,
}

impl P<'_> {
    fn line(&mut self, depth: usize, s: &str) {
        for _ in 0..depth {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn atom(&self, a: AtomId) -> String {
        self.m.atoms.get(a).cloned().unwrap_or_else(|| format!("#{a}"))
    }

    fn stmts(&mut self, ss: &[Stmt], d: usize) {
        for s in ss {
            self.stmt(s, d);
        }
    }

    fn stmt(&mut self, s: &Stmt, d: usize) {
        match s {
            Stmt::Expr(e) => {
                let x = self.expr(e, d);
                self.line(d, &format!("{x};"));
            }
            Stmt::If(c, a, b) => {
                let c = self.expr(c, d);
                self.line(d, &format!("if ({c}) {{"));
                self.stmts(a, d + 1);
                if !b.is_empty() {
                    self.line(d, "} else {");
                    self.stmts(b, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::Loop { id, test, post_test, body, update, per_iteration } => {
                let t = test.as_ref().map(|t| self.expr(t, d)).unwrap_or_default();
                let u = update.as_ref().map(|t| self.expr(t, d)).unwrap_or_default();
                let pi =
                    if per_iteration.is_empty() { String::new() } else { format!(" per-iteration {per_iteration:?}") };
                let kind = if *post_test { "do-while" } else { "loop" };
                self.line(d, &format!("{kind} L{id} ({t}; {u}){pi} {{"));
                self.stmts(body, d + 1);
                self.line(d, "}");
            }
            Stmt::Switch { id, disc, cases } => {
                self.line(d, &format!("switch L{id} (l{disc}) {{"));
                for (c, body) in cases {
                    let c = c.as_ref().map(|c| self.expr(c, d)).unwrap_or("default".into());
                    self.line(d, &format!("case {c}:"));
                    self.stmts(body, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::NewBoxes(ls) => self.line(d, &format!("new-boxes {ls:?};")),
            Stmt::Await { value, target } => {
                let v = self.expr(value, d);
                let t = target_str(target);
                self.line(d, &format!("{t}await {v};"));
            }
            Stmt::Yield { value, kind, target } => {
                let v = self.expr(value, d);
                let t = target_str(target);
                let k = match kind {
                    YieldKind::Value => "yield",
                    YieldKind::Delegate { .. } => "yield*",
                    YieldKind::Initial => "yield(initial)",
                };
                self.line(d, &format!("{t}{k} {v};"));
            }
            Stmt::Break(id) => self.line(d, &format!("break L{id};")),
            Stmt::Continue(id) => self.line(d, &format!("continue L{id};")),
            Stmt::Return(e) => {
                let x = e.as_ref().map(|e| format!(" {}", self.expr(e, d))).unwrap_or_default();
                self.line(d, &format!("return{x};"));
            }
            Stmt::Block(b) => {
                self.line(d, "{");
                self.stmts(b, d + 1);
                self.line(d, "}");
            }
            Stmt::Throw(e) => {
                let x = self.expr(e, d);
                self.line(d, &format!("throw {x};"));
            }
            Stmt::Try { body, catch, finally, exc } => {
                self.line(d, &format!("try{} {{", exc.map(|l| format!(" (exc l{l})")).unwrap_or_default()));
                self.stmts(body, d + 1);
                if let Some((p, c)) = catch {
                    self.line(d, &format!("}} catch ({}) {{", p.map(|l| format!("l{l}")).unwrap_or_default()));
                    self.stmts(c, d + 1);
                }
                if let Some(f) = finally {
                    self.line(d, "} finally {");
                    self.stmts(f, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::IterGuard { iter, next, loop_id, body } => {
                self.line(d, &format!("iter-guard (l{iter}, l{next}, {loop_id:?}) {{"));
                self.stmts(body, d + 1);
                self.line(d, "}");
            }
        }
    }

    /// A body (inlined, predicted): its statements on lines of their own.
    fn body(&mut self, head: String, b: &[Stmt], d: usize) -> String {
        let mut p = P { m: self.m, out: String::new() };
        p.stmts(b, d + 2);
        let mut s = format!("{head} {{\n{}", p.out);
        for _ in 0..d + 1 {
            s.push_str("  ");
        }
        s.push('}');
        s
    }

    fn list(&mut self, xs: &[Expr], d: usize) -> String {
        xs.iter().map(|x| self.expr(x, d)).collect::<Vec<_>>().join(", ")
    }

    fn fields(&mut self, xs: &[(AtomId, Expr)], d: usize) -> String {
        xs.iter().map(|(a, x)| format!("{}: {}", self.atom(*a), self.expr(x, d))).collect::<Vec<_>>().join(", ")
    }

    fn expr(&mut self, e: &Expr, d: usize) -> String {
        use ExprKind as K;
        match &e.kind {
            K::Num(v) => format!("{v}"),
            K::Bool(b) => format!("{b}"),
            K::Local(l) => format!("l{l}"),
            K::SetLocal(l, x) => format!("l{l} = {}", self.expr(x, d)),
            K::InitLocal(l, x) => format!("let l{l}: {} = {}", ty(e.ty), self.expr(x, d)),
            K::Update { local, delta, prefix } => {
                if *prefix {
                    format!("(l{local} += {delta})")
                } else {
                    format!("(l{local} += {delta}, old)")
                }
            }
            K::Unary(op, x) => format!("{op:?}({})", self.expr(x, d)),
            K::Binary(op, a, b) => format!("({} {} {})", self.expr(a, d), binop(*op), self.expr(b, d)),
            K::NumArith(op, a, b) => format!("({} {}# {})", self.expr(a, d), binop(*op), self.expr(b, d)),
            K::Compare(op, a, b) => format!("({} {op:?} {})", self.expr(a, d), self.expr(b, d)),
            K::AnyCompare(op, a, b) => format!("({} {op:?}! {})", self.expr(a, d), self.expr(b, d)),
            K::AnyBinary(op, a, b) => format!("({} {op:?}! {})", self.expr(a, d), self.expr(b, d)),
            K::AnyUnary(op, a) => format!("{op:?}!({})", self.expr(a, d)),
            K::Logical(op, a, b) => format!("({} {op:?} {})", self.expr(a, d), self.expr(b, d)),
            K::Cond(c, a, b) => format!("({} ? {} : {})", self.expr(c, d), self.expr(a, d), self.expr(b, d)),
            K::Truthy(x) => format!("truthy({})", self.expr(x, d)),
            K::Call(k, xs) => format!("fn{k}({})", self.list(xs, d)),
            K::CallDirectThis(k, t, xs) => format!("fn{k}.call({}, {})", self.expr(t, d), self.list(xs, d)),
            K::Math(f, xs) => format!("Math.{f:?}({})", self.list(xs, d)),
            K::Seq(xs) => format!("({})", self.list(xs, d)),
            K::Undefined => "undefined".into(),
            K::Null => "null".into(),
            K::Str(s) => format!("{:?}", self.m.strings.get(*s).cloned().unwrap_or_default()),
            K::Box(x) => format!("box({})", self.expr(x, d)),
            K::Cell(c) => format!("cell:{}", self.m.cells.get(*c).cloned().unwrap_or_default()),
            K::SetCell(c, x) => {
                format!("cell:{} = {}", self.m.cells.get(*c).cloned().unwrap_or_default(), self.expr(x, d))
            }
            K::InitCell(c, x) => {
                format!("init cell:{} = {}", self.m.cells.get(*c).cloned().unwrap_or_default(), self.expr(x, d))
            }
            K::Global(a) => format!("global:{}", self.atom(*a)),
            K::GetProp(o, a) => format!("{}.{}", self.expr(o, d), self.atom(*a)),
            K::SetProp(o, a, x) => format!("{}.{} = {}", self.expr(o, d), self.atom(*a), self.expr(x, d)),
            K::ShapeGet { obj, atom, slot, .. } => format!("{}.{}@{slot}", self.expr(obj, d), self.atom(*atom)),
            K::ShapeSet { obj, atom, slot, value, .. } => {
                format!("{}.{}@{slot} = {}", self.expr(obj, d), self.atom(*atom), self.expr(value, d))
            }
            K::GetElem(o, k) => format!("{}[{}]", self.expr(o, d), self.expr(k, d)),
            K::SetElem(o, k, v) => format!("{}[{}] = {}", self.expr(o, d), self.expr(k, d), self.expr(v, d)),
            K::CallValue(f, xs) => format!("{}({})", self.expr(f, d), self.list(xs, d)),
            K::CallThis(f, t, xs) => format!("{}.call({}, {})", self.expr(f, d), self.expr(t, d), self.list(xs, d)),
            K::CallMethod(o, k, xs) => {
                let key = match k {
                    Key::Atom(a) => format!(".{}", self.atom(*a)),
                    Key::Value(x) => format!("[{}]", self.expr(x, d)),
                };
                format!("{}{key}({})", self.expr(o, d), self.list(xs, d))
            }
            K::New(f, xs) => format!("new {}({})", self.expr(f, d), self.list(xs, d)),
            K::Object(props) => format!("new{{{}}}", self.fields(props, d)),
            K::NewInstance { class, fields } => format!("new C{class}{{{}}}", self.fields(fields, d)),
            K::NewInstanceFull { class, fields, .. } => {
                format!("new C{class}{{{}, ...private}}", self.fields(fields, d))
            }
            K::Array(xs) => format!("new[{}]", self.list(xs, d)),
            K::This => "this".into(),
            K::Closure(k, caps) => format!("closure{k}{caps:?}"),
            K::Void(x) => format!("void {}", self.expr(x, d)),
            K::Lazy(x) => format!("lazy({})", self.expr(x, d)),
            K::Inline { body, locals } => self.body(format!("inline{locals:?}"), body, d),
            K::Predicted { expr, func, body, locals } => {
                let x = self.expr(expr, d);
                self.body(format!("predicted fn{func} [{x}] {locals:?}"), body, d)
            }
            K::ArrayLoop { kind, recv, func, body, locals, .. } => {
                let r = self.expr(recv, d);
                self.body(format!("{r}.{kind:?}(fn{func}) {locals:?}"), body, d)
            }
            K::StructNew { layout, fields, .. } => {
                let fs: Vec<String> = fields.iter().map(|(i, x)| format!("{i}: {}", self.expr(x, d))).collect();
                format!("new struct{layout}{{{}}}", fs.join(", "))
            }
            K::StructGet(k, o, fi) => format!("{}.struct{k}#{fi}", self.expr(o, d)),
            K::StructSet(k, o, fi, v) => format!("{}.struct{k}#{fi} = {}", self.expr(o, d), self.expr(v, d)),
            K::StructCoerce(k, fi, v) => format!("coerce{k}#{fi}({})", self.expr(v, d)),
            K::ProtoMethodIs { class, method, func } => format!("proto(C{class}).{} is fn{func}", self.atom(*method)),
            K::PlainAddOk(a) => format!("plain-add-ok({})", self.atom(*a)),
            K::NullObject => "new{null}".into(),
            K::IsObjectCreate(x) => format!("is-object-create({})", self.expr(x, d)),
            K::ObjectCreate(o, a, x) => format!("{}.{}({})", self.expr(o, d), self.atom(*a), self.expr(x, d)),
            K::HomeValue(k) => format!("home{k}"),
            K::IntOf(x) => format!("int({})", self.expr(x, d)),
            K::CallChecked { func, args, .. } => format!("checked fn{func}({})", self.list(args, d)),
            K::ToObject(x) => format!("ToObject({})", self.expr(x, d)),
            K::DefineField(o, a, x) => format!("define {}.{} = {}", self.expr(o, d), self.atom(*a), self.expr(x, d)),
            K::DeleteProp(o, a) => format!("delete {}.{}", self.expr(o, d), self.atom(*a)),
            K::Length(o, _) => format!("{}.length", self.expr(o, d)),
            other => {
                let s = format!("{other:?}");
                let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
                if s.len() > 160 { format!("{}…", &s[..s.floor_char_boundary(160)]) } else { s }
            }
        }
    }
}

fn binop(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Pow => "**",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Sar => ">>",
        BinOp::Shr => ">>>",
    }
}

fn target_str(t: &AwaitTarget) -> String {
    match t {
        AwaitTarget::Discard => String::new(),
        AwaitTarget::Set(l) => format!("l{l} = "),
        AwaitTarget::Init(l) => format!("let l{l} = "),
        AwaitTarget::Return => "return ".into(),
    }
}
