//! Typed intermediate representation for natively compiled functions.
//!
//! The IR is fully owned (no AST lifetimes) and fully typed: every expression
//! carries its machine-level type, every local has a fixed type, and the only
//! callees are other native functions or known pure math intrinsics. Code that
//! lowers this IR may rely on these invariants without re-checking anything.

use std::fmt;

/// Machine-level value types. `F64` is the JS `number`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    F64,
    Bool,
    /// Only valid as a function return type.
    Void,
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Ty::F64 => "number",
            Ty::Bool => "boolean",
            Ty::Void => "void",
        })
    }
}

pub type FuncId = usize;
pub type LocalId = usize;
pub type LoopId = usize;

#[derive(Debug, Clone)]
pub struct Module {
    pub funcs: Vec<Func>,
}

#[derive(Debug, Clone)]
pub struct Func {
    /// JS name of the function (the module-level binding).
    pub name: String,
    /// Locals `0..params.len()` are the parameters, in order.
    pub params: Vec<Ty>,
    pub ret: Ty,
    /// Types of all locals, including parameters.
    pub locals: Vec<Ty>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    /// `test` is evaluated before each iteration (`while`/`for`) or after
    /// each iteration (`do`-`while`, `post_test == true`). `update` runs at
    /// the end of every iteration and is the target of `continue`.
    Loop {
        id: LoopId,
        test: Option<Expr>,
        post_test: bool,
        body: Vec<Stmt>,
        update: Option<Expr>,
    },
    Break(LoopId),
    Continue(LoopId),
    Return(Option<Expr>),
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub ty: Ty,
    pub kind: ExprKind,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Num(f64),
    Bool(bool),
    Local(LocalId),
    /// Assignment; evaluates to the assigned value.
    SetLocal(LocalId, Box<Expr>),
    /// `++x`, `x--`, ... on a number local.
    Update {
        local: LocalId,
        delta: f64,
        prefix: bool,
    },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// Comparison of two operands of the same type; result is `Bool`.
    Compare(CmpOp, Box<Expr>, Box<Expr>),
    /// `&&` / `||` with JS short-circuit semantics. Both operands and the
    /// result have the same type; truthiness of the left operand decides.
    Logical(LogicalOp, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    /// JS truthiness of a number or boolean, as a `Bool`.
    Truthy(Box<Expr>),
    Call(FuncId, Vec<Expr>),
    Math(MathFn, Vec<Expr>),
    /// Comma operator: all evaluated, value of the last.
    Seq(Vec<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// Numeric negation.
    Neg,
    /// Boolean not (operand is `Bool`).
    Not,
    /// `~x` (ToInt32, bitwise not).
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Sar,
    Shr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
}

/// Pure numeric intrinsics. Builtins are assumed unmodified at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathFn {
    // Lowered inline.
    Abs,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    Fround,
    /// Two operands; n-ary `Math.min/max` are folded into chains.
    Min,
    Max,
    Imul,
    Clz32,
    Sign,
    IsNaN,
    IsFinite,
    IsInteger,
    IsSafeInteger,
    // Lowered to calls (libm or runtime helpers with exact JS semantics).
    Round,
    Pow,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    Exp,
    Expm1,
    Log,
    Log1p,
    Log2,
    Log10,
    Cbrt,
    Hypot,
}

impl MathFn {
    pub fn arity(self) -> usize {
        match self {
            MathFn::Min | MathFn::Max | MathFn::Imul | MathFn::Pow | MathFn::Atan2 | MathFn::Hypot => 2,
            _ => 1,
        }
    }

    pub fn ret(self) -> Ty {
        match self {
            MathFn::IsNaN | MathFn::IsFinite | MathFn::IsInteger | MathFn::IsSafeInteger => Ty::Bool,
            _ => Ty::F64,
        }
    }

    /// Name of the external C symbol implementing this function, if it is
    /// not lowered inline.
    pub fn symbol(self) -> Option<&'static str> {
        Some(match self {
            MathFn::Round => "tsaot_round",
            MathFn::Pow => "tsaot_pow",
            MathFn::Sin => "sin",
            MathFn::Cos => "cos",
            MathFn::Tan => "tan",
            MathFn::Asin => "asin",
            MathFn::Acos => "acos",
            MathFn::Atan => "atan",
            MathFn::Atan2 => "atan2",
            MathFn::Sinh => "sinh",
            MathFn::Cosh => "cosh",
            MathFn::Tanh => "tanh",
            MathFn::Asinh => "asinh",
            MathFn::Acosh => "acosh",
            MathFn::Atanh => "atanh",
            MathFn::Exp => "exp",
            MathFn::Expm1 => "expm1",
            MathFn::Log => "log",
            MathFn::Log1p => "log1p",
            MathFn::Log2 => "log2",
            MathFn::Log10 => "log10",
            MathFn::Cbrt => "cbrt",
            MathFn::Hypot => "hypot",
            _ => return None,
        })
    }
}

impl Expr {
    pub fn new(ty: Ty, kind: ExprKind) -> Self {
        Expr { ty, kind }
    }
    pub fn num(v: f64) -> Self {
        Expr::new(Ty::F64, ExprKind::Num(v))
    }
}
