//! Recursive descent for statements, Pratt for expressions.
//!
//! Declarations are type-first (`int x = 5;`), C/Java shaped. That is
//! unambiguous here without C's lexer hack for two reasons: type names are
//! keywords, and there are no pointer declarators. At statement position a
//! type keyword means "declaration" and nothing else. When user-defined type
//! names arrive, the rule becomes `IDENT IDENT` with two tokens of lookahead,
//! which is how Java manages the same grammar.

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::lexer::{Tok, Token};

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

/// Binding powers. Higher binds tighter. Mirrors C's precedence for the
/// operators that exist, which is the whole point of picking C's surface.
fn infix_bp(t: &Tok) -> Option<(BinOp, u8)> {
    Some(match t {
        Tok::PipePipe => (BinOp::Or, 1),
        Tok::AmpAmp => (BinOp::And, 2),
        Tok::EqEq => (BinOp::Eq, 3),
        Tok::BangEq => (BinOp::Ne, 3),
        Tok::Lt => (BinOp::Lt, 4),
        Tok::LtEq => (BinOp::Le, 4),
        Tok::Gt => (BinOp::Gt, 4),
        Tok::GtEq => (BinOp::Ge, 4),
        Tok::Plus => (BinOp::Add, 5),
        Tok::Minus => (BinOp::Sub, 5),
        Tok::Star => (BinOp::Mul, 6),
        Tok::Slash => (BinOp::Div, 6),
        Tok::Percent => (BinOp::Rem, 6),
        _ => return None,
    })
}

const UNARY_BP: u8 = 7;

impl Parser {
    pub fn new(toks: Vec<Token>) -> Self {
        Parser { toks, pos: 0 }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Tok) -> Result<Token, Diag> {
        if self.peek() == &t {
            Ok(self.bump())
        } else {
            Err(Diag::new(
                self.span(),
                format!(
                    "expected `{}`, found {}",
                    t.spelling(),
                    self.peek().describe()
                ),
            ))
        }
    }

    fn ty_of(t: &Tok) -> Option<Ty> {
        Some(match t {
            Tok::KwInt => Ty::Int,
            Tok::KwBool => Ty::Bool,
            Tok::KwStr => Ty::Str,
            Tok::KwVoid => Ty::Void,
            _ => return None,
        })
    }

    fn expect_ty(&mut self) -> Result<Ty, Diag> {
        match Self::ty_of(self.peek()) {
            Some(t) => {
                self.bump();
                Ok(t)
            }
            None => Err(Diag::new(
                self.span(),
                format!("expected a type, found {}", self.peek().describe()),
            )),
        }
    }

    fn expect_ident(&mut self) -> Result<(String, Span), Diag> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Ident(n) => {
                self.bump();
                Ok((n, span))
            }
            other => Err(Diag::new(
                span,
                format!("expected a name, found {}", other.describe()),
            )),
        }
    }

    // ---- items -------------------------------------------------------

    pub fn parse_program(&mut self) -> Result<Program, Diag> {
        let mut funcs = Vec::new();
        while self.peek() != &Tok::Eof {
            funcs.push(self.parse_func()?);
        }
        Ok(Program { funcs })
    }

    fn parse_func(&mut self) -> Result<Func, Diag> {
        let span = self.span();
        let ret = self.expect_ty()?;
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::LParen)?;

        let mut params = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                let pspan = self.span();
                let ty = self.expect_ty()?;
                if ty == Ty::Void {
                    return Err(Diag::new(pspan, "a parameter cannot have type `void`"));
                }
                let (pname, _) = self.expect_ident()?;
                params.push(Param {
                    ty,
                    name: pname,
                    span: pspan,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        let body = self.parse_block()?;
        Ok(Func {
            ret,
            name,
            params,
            body,
            span,
        })
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, Diag> {
        self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        while self.peek() != &Tok::RBrace {
            if self.peek() == &Tok::Eof {
                return Err(Diag::new(self.span(), "expected `}`, found end of file"));
            }
            stmts.push(self.parse_stmt()?);
        }
        self.expect(Tok::RBrace)?;
        Ok(stmts)
    }

    // ---- statements --------------------------------------------------

    fn parse_stmt(&mut self) -> Result<Stmt, Diag> {
        let span = self.span();

        // Declaration: a type keyword at statement position, never anything else.
        if let Some(ty) = Self::ty_of(self.peek()) {
            self.bump();
            if ty == Ty::Void {
                return Err(Diag::new(span, "a variable cannot have type `void`"));
            }
            let (name, _) = self.expect_ident()?;
            self.expect(Tok::Assign)?;
            let init = self.parse_expr(0)?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Decl {
                ty,
                name,
                init,
                span,
            });
        }

        if self.eat(&Tok::KwReturn) {
            let value = if self.peek() == &Tok::Semi {
                None
            } else {
                Some(self.parse_expr(0)?)
            };
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Return { value, span });
        }

        if self.eat(&Tok::KwWhile) {
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr(0)?;
            self.expect(Tok::RParen)?;
            let body = self.parse_block()?;
            return Ok(Stmt::While { cond, body, span });
        }

        if self.eat(&Tok::KwIf) {
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr(0)?;
            self.expect(Tok::RParen)?;
            let then = self.parse_block()?;
            let els = if self.eat(&Tok::KwElse) {
                // `else if` chains by treating the tail as a one-statement block.
                if self.peek() == &Tok::KwIf {
                    Some(vec![self.parse_stmt()?])
                } else {
                    Some(self.parse_block()?)
                }
            } else {
                None
            };
            return Ok(Stmt::If {
                cond,
                then,
                els,
                span,
            });
        }

        // Assignment: IDENT `=`, distinguished from an expression statement by
        // one token of lookahead.
        if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Assign {
            let (name, _) = self.expect_ident()?;
            self.expect(Tok::Assign)?;
            let value = self.parse_expr(0)?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Assign { name, value, span });
        }

        let expr = self.parse_expr(0)?;
        self.expect(Tok::Semi)?;
        Ok(Stmt::Eval { expr, span })
    }

    // ---- expressions (Pratt) -----------------------------------------

    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr, Diag> {
        let mut lhs = self.parse_prefix()?;
        while let Some((op, bp)) = infix_bp(self.peek()) {
            if bp < min_bp {
                break;
            }
            let span = self.span();
            self.bump();
            // All binary operators here are left-associative, so the right
            // side binds at bp + 1.
            let rhs = self.parse_expr(bp + 1)?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_prefix(&mut self) -> Result<Expr, Diag> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Minus => {
                self.bump();
                let e = self.parse_expr(UNARY_BP)?;
                Ok(Expr::Un(UnOp::Neg, Box::new(e), span))
            }
            Tok::Bang => {
                self.bump();
                let e = self.parse_expr(UNARY_BP)?;
                Ok(Expr::Un(UnOp::Not, Box::new(e), span))
            }
            Tok::Int(n) => {
                self.bump();
                Ok(Expr::Int(n, span))
            }
            Tok::KwTrue => {
                self.bump();
                Ok(Expr::Bool(true, span))
            }
            Tok::KwFalse => {
                self.bump();
                Ok(Expr::Bool(false, span))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Str(s, span))
            }
            Tok::LParen => {
                self.bump();
                let e = self.parse_expr(0)?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(name) => {
                self.bump();
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    if self.peek() != &Tok::RParen {
                        loop {
                            args.push(self.parse_expr(0)?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(Tok::RParen)?;
                    Ok(Expr::Call(name, args, span))
                } else {
                    Ok(Expr::Var(name, span))
                }
            }
            other => Err(Diag::new(
                span,
                format!("expected an expression, found {}", other.describe()),
            )),
        }
    }
}
