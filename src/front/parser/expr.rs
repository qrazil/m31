//! Expressions: Pratt precedence climbing, postfix, lambdas, atoms.

use super::*;

/// The height of an expression tree. Recursive, which is safe: it is only
/// ever asked about a tree the parser has already held to `MAX_DEPTH`.
pub(super) fn height(e: &Expr) -> usize {
    1 + match e {
        Expr::Int(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::Str(..)
        | Expr::Var(..)
        | Expr::This(..) => 0,
        Expr::Un(_, x, _) | Expr::Field(x, _, _) | Expr::Try(x, _) | Expr::Lambda(_, x, _) => {
            height(x)
        }
        Expr::Bin(_, a, b, _) | Expr::Index(a, b, _) | Expr::RepeatLit(a, b, _) => {
            height(a).max(height(b))
        }
        Expr::MethodCall(x, _, a, _) => height(x).max(args_height(a)),
        Expr::Call(_, a, _) | Expr::New(_, a, _) | Expr::EnumNew(_, _, a, _) => args_height(a),
        Expr::SeqLit(xs, _) => xs.iter().map(height).max().unwrap_or(0),
        Expr::MapLit(kvs, _) => kvs
            .iter()
            .map(|(k, v)| height(k).max(height(v)))
            .max()
            .unwrap_or(0),
    }
}

/// The height of the tallest argument, or 0 for none.
pub(super) fn args_height(a: &Args) -> usize {
    a.pos
        .iter()
        .chain(a.named.iter().map(|(_, e)| e))
        .map(height)
        .max()
        .unwrap_or(0)
}

/// Binding powers. Higher binds tighter.
///
/// C's order for the operators C and Python agree on, and **Python's** for
/// the bitwise ones: `|` loosest, then `^`, then `&`, then the shifts, all of
/// them tighter than every comparison and the shifts looser than `+`. C puts
/// `&` below `==`, so `x & 1 == 0` there means `x & (1 == 0)` -- a famous
/// trap that the type checker here would reject anyway, but the reading a
/// person gives it is the Python one, so the grammar gives it too.
///
/// `fmt::prec` holds the same numbers; the "formatter preserves meaning"
/// gate catches the two drifting apart.
pub(super) fn infix_bp(t: &Tok) -> Option<(BinOp, u8)> {
    Some(match t {
        Tok::PipePipe => (BinOp::Or, 1),
        Tok::AmpAmp => (BinOp::And, 2),
        Tok::EqEq => (BinOp::Eq, 3),
        Tok::BangEq => (BinOp::Ne, 3),
        Tok::Lt => (BinOp::Lt, 4),
        Tok::LtEq => (BinOp::Le, 4),
        Tok::Gt => (BinOp::Gt, 4),
        Tok::GtEq => (BinOp::Ge, 4),
        Tok::Pipe => (BinOp::BitOr, 5),
        Tok::Caret => (BinOp::BitXor, 6),
        Tok::Amp => (BinOp::BitAnd, 7),
        Tok::Shl => (BinOp::Shl, SHIFT_BP),
        Tok::Plus => (BinOp::Add, 9),
        Tok::Minus => (BinOp::Sub, 9),
        Tok::Star => (BinOp::Mul, 10),
        Tok::Slash => (BinOp::Div, 10),
        Tok::Percent => (BinOp::Rem, 10),
        _ => return None,
    })
}

/// The shifts' binding power, named because `>>` needs it outside the
/// table: it is two tokens, not one (see `Tok::Shl`).
pub(super) const SHIFT_BP: u8 = 8;

pub(super) const UNARY_BP: u8 = 11;

impl Parser {
    pub(super) fn parse_expr_inner(&mut self, min_bp: u8) -> Result<Expr, Diag> {
        let mut lhs = self.parse_prefix()?;
        // The height of `lhs`, measured once the chain first grows.
        let mut h: Option<usize> = None;
        while let Some((op, bp, width)) = self.infix_here() {
            if bp < min_bp {
                break;
            }
            let span = self.span();
            for _ in 0..width {
                self.bump();
            }
            let hl = h.unwrap_or_else(|| height(&lhs));
            // All binary operators here are left-associative, so the right
            // side binds at bp + 1.
            let rhs = self.parse_expr(bp + 1)?;
            let hn = hl.max(height(&rhs)) + 1;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs), span);
            self.check_height(hn, &lhs)?;
            h = Some(hn);
        }
        // A range is loop syntax and has no value, so `..` can only follow an
        // expression inside a `for` header. Anywhere else it is `a .. b`
        // written where a value was wanted, and the reader is better served
        // by that than by "expected `;`".
        if self.peek() == &Tok::DotDot && !self.in_range {
            return Err(Diag::new(
                self.span(),
                "`..` may only be written in a `for` header: a range is loop syntax, \
                 not a value",
            ));
        }
        Ok(lhs)
    }

    /// The binary operator at the cursor, its binding power, and how many
    /// tokens spell it.
    ///
    /// `>>` is the one two-token operator: two `>` with nothing between them,
    /// not even a space. The lexer cannot join them, because the same two
    /// characters close `List<List<int>>`; here, in operator position, no
    /// type argument list can be open, so they can only be a shift.
    /// `a > > b` is not a shift, and stays the parse error it always was.
    fn infix_here(&self) -> Option<(BinOp, u8, usize)> {
        if self.peek() == &Tok::Gt && self.peek_at(1) == &Tok::Gt {
            let (a, b) = (self.toks[self.pos].span, self.toks[self.pos + 1].span);
            if a.line == b.line && a.col + 1 == b.col {
                return Some((BinOp::Shr, SHIFT_BP, 2));
            }
        }
        infix_bp(self.peek()).map(|(op, bp)| (op, bp, 1))
    }

    /// Postfix chain: `.field`, `.method(..)`, `[i]` and `?`, in any order
    /// and any number of times.
    ///
    /// ONE loop, not one per form. Three separate loops could not parse
    /// `src.get(k)?.size()`: the `?` ended the chain and the `.size()` after
    /// it had nowhere to attach.
    fn parse_postfix(&mut self, mut e: Expr) -> Result<Expr, Diag> {
        // The height of `e`, measured once the chain first grows -- a long
        // chain builds a tree as deep as a long nest of parentheses.
        let mut h: Option<usize> = None;
        loop {
            let span = self.span();
            if !matches!(self.peek(), Tok::Dot | Tok::LBracket | Tok::Question) {
                return Ok(e);
            }
            let he = h.unwrap_or_else(|| height(&e));
            let child = match self.peek() {
                Tok::Dot => {
                    self.bump();
                    let (name, _) = self.expect_ident()?;
                    if self.peek() == &Tok::LParen {
                        let args = self.parse_args()?;
                        let ha = args_height(&args);
                        e = Expr::MethodCall(Box::new(e), name, args, span);
                        ha
                    } else {
                        e = Expr::Field(Box::new(e), name, span);
                        0
                    }
                }
                Tok::LBracket => {
                    self.bump();
                    let i = self.parse_expr(0)?;
                    self.expect(Tok::RBracket)?;
                    let hi = height(&i);
                    e = Expr::Index(Box::new(e), Box::new(i), span);
                    hi
                }
                _ => {
                    self.bump();
                    e = Expr::Try(Box::new(e), span);
                    0
                }
            };
            let hn = he.max(child) + 1;
            self.check_height(hn, &e)?;
            h = Some(hn);
        }
    }

    fn parse_prefix(&mut self) -> Result<Expr, Diag> {
        let e = self.parse_atom()?;
        self.parse_postfix(e)
    }

    /// Is a lambda written at the `(` the parser is sitting on?
    ///
    /// Only a parenthesised group whose closing `)` is followed by `=>` is
    /// one, and **nothing else in the grammar may be followed by `=>`** --
    /// it is a token with exactly one use. So the question is settled
    /// without looking at what is between the parentheses, which is what
    /// lets `(a) => a` be refused by naming the parameter's missing type
    /// instead of by reporting a puzzling expression. The alternative, the
    /// two-token test `decl_starts_here` makes at statement position, would
    /// have had to read `(int(x))` as a parameter list.
    fn lambda_here(&self) -> bool {
        let mut depth = 0usize;
        let mut i = 0;
        loop {
            match self.peek_at(i) {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        self.peek_at(i + 1) == &Tok::FatArrow
    }

    /// `(Point a, Point b) => a.x - b.x`.
    ///
    /// Parameter types are written, the body is one expression, and there is
    /// no block form -- docs/closures-decision.md, "Why written parameter
    /// types" and "Why one expression and no block body". Everything else
    /// about a lambda (its method name, how many parameters it must have,
    /// what they must be and what it must return) comes from the interface
    /// it is passed to, and is checked in the lowering, which is the only
    /// place that knows the target.
    fn parse_lambda(&mut self) -> Result<Expr, Diag> {
        let span = self.span();
        self.expect(Tok::LParen)?;
        let mut params: Vec<Param> = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                let at = self.span();
                // `lib.Point p` -- another module's type. `is_ty_at` only
                // knows the names this file declares, so the qualified form
                // is recognised the way `decl_starts_here` recognises it.
                // `(const Point p) => ..`: the type starts past the word.
                let at_ty = usize::from(self.peek() == &Tok::KwConst);
                let qualified = matches!(self.peek_at(at_ty), Tok::Ident(m) if self.imports.contains(m))
                    && self.peek_at(at_ty + 1) == &Tok::Dot;
                if !qualified && !self.is_ty_at(at_ty) {
                    return Err(Diag::new(
                        at,
                        format!(
                            "a lambda's parameters are written with their types, \
                             like every other binding in this language -- write \
                             `(int {}) => ..`",
                            match self.peek() {
                                Tok::Ident(n) => n.clone(),
                                _ => "x".to_string(),
                            }
                        ),
                    ));
                }
                let p = self.parse_param()?;
                if p.default.is_some() {
                    return Err(Diag::new(
                        p.span,
                        format!(
                            "`{}` cannot have a default: a lambda is called through an \
                             interface, which dispatches with no name to give an \
                             optional argument",
                            p.name
                        ),
                    ));
                }
                params.push(p);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        self.expect(Tok::FatArrow)?;
        let body = self.parse_expr(0)?;
        Ok(Expr::Lambda(params, Box::new(body), span))
    }

    /// Is the parser sitting on the `=` of an augmented assignment?
    ///
    /// `n += 1` reaches here as `n`, `+`, `=`: the `+` was taken as an infix
    /// operator and the `=` is where its right operand should be. So the
    /// token before the cursor, if it is adjacent and is an operator, says
    /// what was written. `>>=` is the awkward one -- the lexer makes it `>`
    /// and `>=`, because `>>` is never one token (§1.6).
    ///
    /// Augmented assignment is deliberately absent (§6.1, §9). §9 exists so
    /// that an absence is a decision; a decision should reach the person who
    /// trips over it, and this is the second mistake everyone makes in their
    /// first hour here.
    fn augmented_here(&self) -> Option<&'static str> {
        if self.pos == 0 {
            return None;
        }
        let prev = &self.toks[self.pos - 1];
        let here = self.span();
        let adjacent = |t: &Tok| {
            prev.span.line == here.line
                && prev.span.col + t.spelling().chars().count() as u32 == here.col
        };
        match (&prev.tok, self.peek()) {
            (
                t @ (Tok::Plus
                | Tok::Minus
                | Tok::Star
                | Tok::Slash
                | Tok::Percent
                | Tok::Amp
                | Tok::Pipe
                | Tok::Caret
                | Tok::Shl),
                Tok::Assign,
            ) if adjacent(t) => Some(t.spelling()),
            (t @ Tok::Gt, Tok::GtEq) if adjacent(t) => Some(">>"),
            _ => None,
        }
    }

    fn parse_atom(&mut self) -> Result<Expr, Diag> {
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
            Tok::Tilde => {
                self.bump();
                let e = self.parse_expr(UNARY_BP)?;
                Ok(Expr::Un(UnOp::BitNot, Box::new(e), span))
            }
            Tok::Int(n) => {
                self.bump();
                Ok(Expr::Int(n, span))
            }
            Tok::Float(x) => {
                self.bump();
                Ok(Expr::Float(x, span))
            }
            Tok::KwTrue => {
                self.bump();
                Ok(Expr::Bool(true, span))
            }
            Tok::KwFalse => {
                self.bump();
                Ok(Expr::Bool(false, span))
            }
            Tok::KwThis => {
                self.bump();
                if self.in_default {
                    return Err(Diag::new(
                        span,
                        "`this` cannot appear in a default: a default is evaluated \
                         where the call or construction is written, where `this` is \
                         not this declaration's receiver",
                    ));
                }
                Ok(Expr::This(span))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Str(s, span))
            }
            Tok::LParen if self.lambda_here() => self.nested("lambda", Self::parse_lambda),
            Tok::LParen => {
                self.bump();
                // A range is loop syntax; parentheses do not make one a
                // value, so `(0 .. 3)` is the same mistake as `0 .. 3`.
                let outer = std::mem::replace(&mut self.in_range, false);
                let e = self.parse_expr(0);
                self.in_range = outer;
                let e = e?;
                self.expect(Tok::RParen)?;
                // Remember that the author wrote these -- see `parens`.
                self.parens.push(e.span());
                Ok(e)
            }
            // `[]`, `[a, b, c]`, `[x; n]`
            Tok::LBracket => {
                self.bump();
                if self.eat(&Tok::RBracket) {
                    return Ok(Expr::SeqLit(Vec::new(), span));
                }
                let first = self.parse_expr(0)?;
                if self.eat(&Tok::Semi) {
                    let n = self.parse_expr(0)?;
                    self.expect(Tok::RBracket)?;
                    return Ok(Expr::RepeatLit(Box::new(first), Box::new(n), span));
                }
                let mut items = vec![first];
                while self.eat(&Tok::Comma) {
                    if self.peek() == &Tok::RBracket {
                        break;
                    }
                    items.push(self.parse_expr(0)?);
                }
                self.expect(Tok::RBracket)?;
                Ok(Expr::SeqLit(items, span))
            }
            // `{}`, `{k: v, ..}`. Unambiguous against a block, which only
            // ever appears where a statement may.
            Tok::LBrace => {
                self.bump();
                let mut items = Vec::new();
                if self.peek() != &Tok::RBrace {
                    loop {
                        let k = self.parse_expr(0)?;
                        self.expect(Tok::Colon)?;
                        let v = self.parse_expr(0)?;
                        items.push((k, v));
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                        if self.peek() == &Tok::RBrace {
                            break;
                        }
                    }
                }
                self.expect(Tok::RBrace)?;
                Ok(Expr::MapLit(items, span))
            }
            // `float.from_bits(n)` -- a static method on a built-in type.
            // The grammar's `type "." IDENT [ args ]` always derived it; the
            // parser only ever took that path for a type NAME.
            t if Self::ty_of(&t).is_some() && self.peek_at(1) == &Tok::Dot => {
                let ty = Self::ty_of(&t).expect("checked by the guard");
                self.bump();
                self.bump();
                let (member, _) = self.expect_ident()?;
                let args = if self.peek() == &Tok::LParen {
                    self.parse_args()?
                } else {
                    Args::default()
                };
                Ok(Expr::EnumNew(ty, member, args, span))
            }
            // `int(x)` -- a conversion back to a base type. A type keyword
            // is not otherwise an expression, so this is unambiguous.
            t if Self::ty_of(&t).is_some() && self.peek_at(1) == &Tok::LParen => {
                let name = t.spelling().to_string();
                self.bump();
                let args = self.parse_args()?;
                Ok(Expr::Call(name, args, span))
            }
            // `lib.P.origin(..)` and `lib.Colour.Red` -- a static method or
            // an enum variant on another module's type. Told apart from
            // `lib.f(..)` by the SECOND dot: a module function call has only
            // one.
            //
            // With type arguments the same two, plus a construction:
            // `lib.Wrap<int>(..)`, `lib.Res<int>.Ok(1)`. The parser cannot
            // know that `Wrap` is a type in `lib`, but `lib.x < ...` is not a
            // comparison worth reading this way. `lib.x` alone can be an
            // operand now -- a module may export a constant -- but only a
            // shape like `lib.N < a > (b)` reaches here, which compares a
            // `bool` with `>` and is refused whichever way it is read.
            // Plain `lib.Point(..)` has no such marker and stays a qualified
            // call; the lowerer builds the type when `lib` has one by that
            // name.
            //
            // `lib.TABLE.size()` also lands here, as a "member" of a type
            // `lib.TABLE` -- the parser cannot see that `TABLE` is a
            // constant. Monomorphisation, which can, turns it back into a
            // method call on the constant (`Mono::subst_expr_as`).
            Tok::Ident(m)
                if self.imports.contains(&m)
                    && self.peek_at(1) == &Tok::Dot
                    && matches!(self.peek_at(2), Tok::Ident(_))
                    && (self.peek_at(3) == &Tok::Dot || self.qualified_targs_end().is_some()) =>
            {
                self.bump();
                self.bump();
                let (tname, _) = self.expect_ident()?;
                let mut targs = Vec::new();
                if self.eat(&Tok::Lt) {
                    loop {
                        targs.push(self.expect_ty()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::Gt)?;
                }
                let ty = self.intern(format!("{m}#{tname}"), targs);
                if self.peek() == &Tok::LParen {
                    let args = self.parse_args()?;
                    return Ok(Expr::New(ty, args, span));
                }
                self.expect(Tok::Dot)?;
                let (member, _) = self.expect_ident()?;
                let args = if self.peek() == &Tok::LParen {
                    self.parse_args()?
                } else {
                    Args::default()
                };
                Ok(Expr::EnumNew(ty, member, args, span))
            }
            Tok::Ident(name) if self.is_ty_name(&name) => {
                // Construction takes the same argument shape as a call: a
                // field with no default is positional, one with a default is
                // named. The parser does not enforce that -- `bind_args` in
                // lower.rs does, for calls and constructions alike.
                //
                // `<` here is unambiguously type arguments, not a comparison,
                // because `name` is known to be a type -- the same pre-pass
                // that makes the type-first grammar decidable.
                self.bump();
                let mut targs = Vec::new();
                if self.peek() == &Tok::Lt {
                    self.bump();
                    loop {
                        targs.push(self.expect_ty()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::Gt)?;
                }
                // `Option<int>.Some(1)` -- an enum variant. The enum type is
                // written out because a variant with no payload has nothing
                // to infer it from, and one rule beats a rule with an
                // exception.
                if self.peek() == &Tok::Dot {
                    self.bump();
                    let (variant, _) = self.expect_ident()?;
                    let ty = self.intern(self.qualify(&name), targs);
                    let args = if self.peek() == &Tok::LParen {
                        self.parse_args()?
                    } else {
                        Args::default()
                    };
                    return Ok(Expr::EnumNew(ty, variant, args, span));
                }
                if self.peek() != &Tok::LParen {
                    // Nothing shadows a type, so a bare type name here is
                    // never a variable -- say that rather than demanding a
                    // `(` the writer did not mean to type.
                    return Err(Diag::new(span, format!("`{name}` is a type, not a value")));
                }
                let ty = self.intern(self.qualify(&name), targs);
                let args = self.parse_args()?;
                Ok(Expr::New(ty, args, span))
            }
            Tok::Ident(name) => {
                self.bump();
                if self.peek() == &Tok::LParen {
                    let args = self.parse_args()?;
                    Ok(Expr::Call(name, args, span))
                } else {
                    Ok(Expr::Var(name, span))
                }
            }
            // `n += 1`, `mask |= bit`, `x <<= 2`. The parser has already
            // taken the operator as an infix one and is now looking for its
            // right operand, so the token before the `=` says what was
            // meant. Augmented assignment is deliberately absent (§6.1,
            // §9); a decision the reader trips over should reach them.
            t @ (Tok::Assign | Tok::GtEq) if self.augmented_here().is_some() => {
                let op = self.augmented_here().unwrap();
                let _ = t;
                Err(Diag::new(
                    span,
                    format!(
                        "there is no `{op}=`, and no augmented assignment at all: \
                         write `x = x {op} ..`, or count with `for (int i in 0 .. n)`"
                    ),
                ))
            }
            Tok::Underscore => Err(Diag::new(
                span,
                "`_` is a discard and cannot be used as a value",
            )),
            // A range is loop syntax, not a value: it has no type, cannot be
            // stored and cannot be passed. Saying that beats reporting that
            // `..` surprised the parser.
            Tok::DotDot => Err(Diag::new(
                span,
                "`..` may only be written in a `for` header: a range is loop syntax, \
                 not a value",
            )),
            other => Err(Diag::new(
                span,
                format!("expected an expression, found {}", other.describe()),
            )),
        }
    }
}
