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
    /// A user-defined type, indexed into the program's type table.
    User(u32),
}

impl Ty {
    pub fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Bool => "bool",
            Ty::Str => "str",
            Ty::Void => "void",
            // Named through the type table by the caller where a real name
            // is needed; this fallback only appears in internal messages.
            Ty::User(_) => "<type>",
        }
    }

    /// Whether values of this type are reference counted. Only `str` today;
    /// this is the single predicate the refcount pass consults.
    pub fn is_ref(self) -> bool {
        matches!(self, Ty::Str | Ty::User(_))
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

/// A user-defined type declaration.
#[derive(Debug, Clone)]
pub struct TypeDecl {
    pub name: String,
    pub fields: Vec<Param>,
    pub span: Span,
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
    /// `expr.field`
    Field(Box<Expr>, String, Span),
    /// `Point(x: 1, y: 2)` -- construction is always by field name, so a
    /// field reordering in the declaration cannot silently transpose values.
    New(String, Vec<(String, Expr)>, Span),
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
            | Expr::Call(_, _, s)
            | Expr::Field(_, _, s)
            | Expr::New(_, _, s) => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `int x = expr;` / `const int x = expr;`
    Decl {
        ty: Ty,
        name: String,
        init: Expr,
        is_const: bool,
        span: Span,
    },
    /// `x = expr;`
    Assign {
        name: String,
        value: Expr,
        span: Span,
    },
    /// `obj.field = expr;`
    SetField {
        obj: Expr,
        field: String,
        value: Expr,
        span: Span,
    },
    /// `return expr;` / `return;`
    Return { value: Option<Expr>, span: Span },
    /// A call evaluated for effect.
    Eval { expr: Expr, span: Span },
    /// `break;`
    Break { span: Span },
    /// `continue;`
    Continue { span: Span },
    /// `while (cond) { .. }`
    While {
        cond: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `if (cond) { .. } else { .. }`
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Option<Vec<Stmt>>,
        span: Span,
    },
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
    pub types: Vec<TypeDecl>,
    pub funcs: Vec<Func>,
}
