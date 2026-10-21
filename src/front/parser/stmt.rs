//! Statements, including declarations at statement position.

use super::*;

impl Parser {
    pub(super) fn parse_stmt_inner(&mut self) -> Result<Stmt, Diag> {
        let span = self.span();

        // `const a { .. }` / `const a, b, c { .. }` -- the block form
        // (docs/const-decision.md, "`const` is also a block"). Two tokens of
        // lookahead past `const` tell it apart from the declaration below: a
        // plain identifier followed by `,` or `{` names an existing local for
        // the block; followed by anything else (another identifier, `<`, for
        // a generic type) it is `const TYPE NAME = ..`.
        if self.peek() == &Tok::KwConst {
            if let Tok::Ident(_) = self.peek_at(1) {
                if matches!(self.peek_at(2), Tok::Comma | Tok::LBrace) {
                    return self.parse_const_block(span);
                }
            }
        }

        // Declaration. Three spellings, all decidable with two tokens:
        //   const <ty> x = ..    a type keyword or type name after `const`
        //   int x = ..           a type keyword at statement position
        //   Point p = ..         a known type NAME followed by an identifier
        let is_const = self.peek() == &Tok::KwConst;
        let is_decl = is_const || self.decl_starts_here();
        if is_decl {
            if is_const {
                self.bump();
            }
            let ty = self.expect_ty()?;
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
                is_const,
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

        if self.eat(&Tok::KwSpawn) {
            let (name, _) = self.expect_ident()?;
            let args = self.parse_args()?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Spawn { name, args, span });
        }

        if self.eat(&Tok::KwBreak) {
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Break { span });
        }

        if self.eat(&Tok::KwContinue) {
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Continue { span });
        }

        if self.eat(&Tok::KwFor) {
            self.expect(Tok::LParen)?;
            let ty = self.expect_ty()?;
            if ty == Ty::Void {
                return Err(Diag::new(span, "`void` is not a value type"));
            }
            let (name, _) = self.expect_ident()?;
            // `for (int i = 0; ..)` is the first mistake every programmer
            // arriving from C, Java, Go, Rust or Python makes, and the
            // absence is a decision (§5.5, §9). Say so here rather than
            // reporting that `=` surprised the parser.
            if self.peek() == &Tok::Assign {
                return Err(Diag::new(
                    self.span(),
                    "there is no three-clause `for`; write `for (int i in 0 .. n)` \
                     to count, or `for (T x in xs)` over a collection",
                ));
            }
            self.expect(Tok::KwIn)?;
            let outer = std::mem::replace(&mut self.in_range, true);
            let iter = self.parse_expr(0);
            self.in_range = outer;
            let iter = iter?;
            // `0 .. n`: a range, and the only place one may be written.
            if self.eat(&Tok::DotDot) {
                let to = self.parse_expr(0)?;
                self.expect(Tok::RParen)?;
                let body = self.parse_block()?;
                return Ok(Stmt::ForRange {
                    ty,
                    name,
                    from: iter,
                    to,
                    body,
                    span,
                });
            }
            self.expect(Tok::RParen)?;
            let body = self.parse_block()?;
            return Ok(Stmt::ForIn {
                ty,
                name,
                iter,
                body,
                span,
            });
        }

        if self.eat(&Tok::KwWhile) {
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr(0)?;
            self.expect(Tok::RParen)?;
            let body = self.parse_block()?;
            return Ok(Stmt::While { cond, body, span });
        }

        if self.eat(&Tok::KwMatch) {
            self.expect(Tok::LParen)?;
            let scrutinee = self.parse_expr(0)?;
            self.expect(Tok::RParen)?;
            self.expect(Tok::LBrace)?;
            let mut arms: Vec<MatchArm> = Vec::new();
            while self.peek() != &Tok::RBrace {
                if self.peek() == &Tok::Eof {
                    return Err(Diag::new(self.span(), "expected `}`, found end of file"));
                }
                let aspan = self.span();
                self.expect(Tok::KwCase)?;
                let (variant, _) = self.expect_ident()?;
                // `case Some(int v):` -- bindings are type-first, like every
                // other binding in the language.
                let mut binds = Vec::new();
                if self.eat(&Tok::LParen) {
                    loop {
                        let bspan = self.span();
                        // `_` discards the payload: no name, and no type,
                        // because the variant already says what it carries.
                        if self.eat(&Tok::Underscore) {
                            binds.push(None);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                            continue;
                        }
                        let ty = self.expect_ty()?;
                        if ty == Ty::Void {
                            return Err(Diag::new(bspan, "`void` is not a value type"));
                        }
                        if self.peek() == &Tok::Underscore {
                            return Err(Diag::new(
                                self.span(),
                                "a discard is written `_` alone, without a type: \
                                 write `case V(_)`, not `case V(T _)`",
                            ));
                        }
                        let (name, _) = self.expect_ident()?;
                        binds.push(Some(Param {
                            ty,
                            name,
                            default: None,
                            embedded: false,
                            is_pub: false,
                            is_const: false,
                            span: bspan,
                        }));
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                }
                self.expect(Tok::Colon)?;
                // Braces are mandatory, as they are for `if` and `while`, so
                // there is no dangling-statement question and no fallthrough
                // to wonder about.
                let body = self.parse_block()?;
                arms.push(MatchArm {
                    variant,
                    binds,
                    body,
                    span: aspan,
                });
            }
            self.expect(Tok::RBrace)?;
            return Ok(Stmt::Match {
                scrutinee,
                arms,
                span,
            });
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

        // Otherwise: parse an expression, then look for `=`. Doing it this
        // way rather than with lookahead means `x` and `o.f` and any future
        // lvalue form all take the same path.
        let lhs = self.parse_expr(0)?;
        if self.eat(&Tok::Assign) {
            let value = self.parse_expr(0)?;
            self.expect(Tok::Semi)?;
            return match lhs {
                Expr::Var(name, _) => Ok(Stmt::Assign { name, value, span }),
                Expr::Field(obj, field, _) => Ok(Stmt::SetField {
                    obj: *obj,
                    field,
                    value,
                    span,
                }),
                Expr::Index(obj, index, _) => Ok(Stmt::SetIndex {
                    obj: *obj,
                    index: *index,
                    value,
                    span,
                }),
                // Replacing the receiver would change which object the
                // caller's reference points at from inside a call that only
                // borrowed it; there is nothing it could sensibly mean.
                Expr::This(s) => Err(Diag::new(
                    s,
                    "`this` cannot be assigned: it is the method's receiver, not a variable",
                )),
                other => Err(Diag::new(other.span(), "cannot assign to this expression")),
            };
        }
        self.expect(Tok::Semi)?;
        Ok(Stmt::Eval { expr: lhs, span })
    }
}
