//! Abstract syntax tree.
//!
//! Deliberately thin: the AST exists to be typechecked and lowered, and every
//! interesting transformation happens on the IR instead. See docs/ir-v0.md §4.

use crate::diag::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Int,
    Bool,
    Str,
    Void,
}

impl Ty {
    pub fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Bool => "bool",
            Ty::Str => "str",
            Ty::Void => "void",
        }
    }

    /// Whether values of this type are reference counted. Only `str` today;
    /// this is the single predicate the refcount pass consults.
    pub fn is_ref(self) -> bool {
        matches!(self, Ty::Str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn spelling(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64, Span),
    Bool(bool, Span),
    Str(String, Span),
    Var(String, Span),
    Bin(BinOp, Box<Expr>, Box<Expr>, Span),
    Un(UnOp, Box<Expr>, Span),
    Call(String, Vec<Expr>, Span),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Bool(_, s)
            | Expr::Str(_, s)
            | Expr::Var(_, s)
            | Expr::Bin(_, _, _, s)
            | Expr::Un(_, _, s)
            | Expr::Call(_, _, s) => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `int x = expr;`
    Decl { ty: Ty, name: String, init: Expr, span: Span },
    /// `x = expr;`
    Assign { name: String, value: Expr, span: Span },
    /// `return expr;` / `return;`
    Return { value: Option<Expr>, span: Span },
    /// A call evaluated for effect.
    ExprStmt { expr: Expr, span: Span },
    /// `if (cond) { .. } else { .. }`
    If { cond: Expr, then: Vec<Stmt>, els: Option<Vec<Stmt>>, span: Span },
}

#[derive(Debug, Clone)]
pub struct Param {
    pub ty: Ty,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Func {
    pub ret: Ty,
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub funcs: Vec<Func>,
}
