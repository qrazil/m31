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
    /// Names declared by `type`, from a pre-pass, so a type can be used
    /// before it is declared and `IDENT IDENT` is decidable with two tokens.
    type_names: Vec<String>,
    /// Type parameter names in scope while parsing a generic declaration.
    /// `T` inside `type Box<T>` must parse as a type even though no `type T`
    /// exists.
    tparams: Vec<String>,
    /// Interned type expressions; `Ty::User` indexes this.
    ty_exprs: Vec<TyExpr>,
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
        // Pre-pass: every `type IDENT` in the stream. This is what makes a
        // type-first grammar decidable without C's lexer hack -- the parser
        // knows the type names before it starts, so `Point p` is a
        // declaration and `foo p` is an error, not an ambiguity.
        // The builtin collections and channels: the runtime owns their
        // representation, so there is no `type Chan<T>` in the source to
        // find, but they must parse as type names like any other.
        let mut type_names = vec![
            "Chan".to_string(),
            "Array".to_string(),
            "List".to_string(),
            "Map".to_string(),
        ];
        for w in toks.windows(2) {
            if w[0].tok == Tok::KwType || w[0].tok == Tok::KwInterface {
                if let Tok::Ident(n) = &w[1].tok {
                    type_names.push(n.clone());
                }
            }
        }
        // `distinct <type> Name;` -- the name is the third token, and the
        // type in the middle may itself be a name.
        for w in toks.windows(3) {
            if w[0].tok == Tok::KwDistinct {
                if let Tok::Ident(n) = &w[2].tok {
                    type_names.push(n.clone());
                }
            }
        }
        Parser {
            toks,
            pos: 0,
            type_names,
            tparams: Vec::new(),
            ty_exprs: Vec::new(),
        }
    }

    /// Is this name usable as a type here -- a declared type, or a type
    /// parameter currently in scope?
    fn is_ty_name(&self, name: &str) -> bool {
        self.type_names.iter().any(|n| n == name) || self.tparams.iter().any(|n| n == name)
    }

    fn intern(&mut self, name: String, args: Vec<Ty>) -> Ty {
        let e = TyExpr { name, args };
        match self.ty_exprs.iter().position(|x| *x == e) {
            Some(i) => Ty::User(i as u32),
            None => {
                self.ty_exprs.push(e);
                Ty::User((self.ty_exprs.len() - 1) as u32)
            }
        }
    }

    /// Is the token at `n` the start of a type?
    fn is_ty_at(&self, n: usize) -> bool {
        match self.peek_at(n) {
            Tok::Ident(name) => self.is_ty_name(name),
            t => Self::ty_of(t).is_some(),
        }
    }

    /// Does a declaration start here? A type keyword always does. A type
    /// NAME does when an identifier follows it, possibly past a `<...>`
    /// argument list -- `Box<int> b` is a declaration, `Box<int>(..)` is a
    /// construction.
    fn decl_starts_here(&self) -> bool {
        if Self::ty_of(self.peek()).is_some() {
            return true;
        }
        if !self.is_ty_at(0) {
            return false;
        }
        let mut i = 1;
        if self.peek_at(1) == &Tok::Lt {
            let mut depth = 0;
            loop {
                match self.peek_at(i) {
                    Tok::Lt => depth += 1,
                    Tok::Gt => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    Tok::Eof => return false,
                    _ => {}
                }
                i += 1;
            }
        }
        matches!(self.peek_at(i), Tok::Ident(_))
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
        if let Tok::Ident(name) = self.peek().clone() {
            if self.is_ty_name(&name) {
                self.bump();
                // Type arguments: `Box<int>`, `Pair<K, V>`.
                let mut args = Vec::new();
                if self.peek() == &Tok::Lt {
                    self.bump();
                    loop {
                        args.push(self.expect_ty()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::Gt)?;
                }
                return Ok(self.intern(name, args));
            }
        }
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

    /// `f(a, b, opt: c)` -- positional arguments first, then named ones.
    /// Once a named argument appears, everything after it must be named too,
    /// so a reader never has to count commas to find which parameter a value
    /// lands in.
    fn parse_args(&mut self) -> Result<Args, Diag> {
        let mut args = Args::default();
        self.expect(Tok::LParen)?;
        if self.peek() != &Tok::RParen {
            loop {
                let named = matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Colon;
                if named {
                    let (n, s) = self.expect_ident()?;
                    self.expect(Tok::Colon)?;
                    if args.named.iter().any(|(x, _)| *x == n) {
                        return Err(Diag::new(s, format!("`{n}` given twice")));
                    }
                    args.named.push((n, self.parse_expr(0)?));
                } else {
                    let s = self.span();
                    if !args.named.is_empty() {
                        return Err(Diag::new(
                            s,
                            "positional arguments must come before named ones",
                        ));
                    }
                    args.pos.push(self.parse_expr(0)?);
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        Ok(args)
    }

    /// A parameter or field: `int x` is mandatory, `int x = 0` is optional.
    fn parse_param(&mut self) -> Result<Param, Diag> {
        let span = self.span();
        let ty = self.expect_ty()?;
        if ty == Ty::Void {
            return Err(Diag::new(span, "`void` is not a value type"));
        }
        // `Animal;` with no name is an embedded field, named after its type.
        if self.peek() == &Tok::Semi {
            let Ty::User(i) = ty else {
                return Err(Diag::new(
                    span,
                    "only a user type can be embedded; give this field a name",
                ));
            };
            return Ok(Param {
                ty,
                name: self.ty_exprs[i as usize].name.clone(),
                default: None,
                embedded: true,
                span,
            });
        }
        let (name, _) = self.expect_ident()?;
        let default = if self.eat(&Tok::Assign) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Param {
            ty,
            name,
            default,
            embedded: false,
            span,
        })
    }

    // ---- items -------------------------------------------------------

    pub fn parse_program(&mut self) -> Result<Program, Diag> {
        let mut funcs = Vec::new();
        let mut types = Vec::new();
        let mut toplevel = Vec::new();
        while self.peek() != &Tok::Eof {
            if self.peek() == &Tok::KwDistinct {
                types.push(self.parse_distinct()?);
            } else if self.peek() == &Tok::KwType || self.peek() == &Tok::KwInterface {
                types.push(self.parse_type_decl()?);
            } else if self.starts_func() {
                funcs.push(self.parse_func()?);
            } else {
                // Anything else at the top level is a statement, and runs.
                toplevel.push(self.parse_stmt()?);
            }
        }
        Ok(Program {
            types,
            funcs,
            toplevel,
            ty_exprs: std::mem::take(&mut self.ty_exprs),
        })
    }

    /// Distinguish a function declaration from a top-level statement.
    ///
    /// Both can begin with a type: `int f(..) {` declares, `int x = ..;`
    /// does not. The difference is a `(` after the name -- and after any
    /// generic parameter list. Three tokens of lookahead, no backtracking.
    fn starts_func(&self) -> bool {
        // The return type may be a type parameter the parser has not met yet
        // -- `T unwrap<T>(Box<T> b)` -- so accept any identifier here and let
        // the shape decide. A statement can never be IDENT IDENT `(`.
        if !matches!(self.peek(), Tok::Ident(_)) && Self::ty_of(self.peek()).is_none() {
            return false;
        }
        let mut i = 1;
        // Skip type arguments on the return type: `Box<int> f(..)`.
        if self.peek_at(1) == &Tok::Lt {
            let mut depth = 0;
            loop {
                match self.peek_at(i) {
                    Tok::Lt => depth += 1,
                    Tok::Gt => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    Tok::Eof => return false,
                    _ => {}
                }
                i += 1;
            }
        }
        if !matches!(self.peek_at(i), Tok::Ident(_)) {
            return false;
        }
        i += 1;
        // A qualified method name: `int Rect.area()`.
        if self.peek_at(i) == &Tok::Dot {
            i += 1;
            if !matches!(self.peek_at(i), Tok::Ident(_)) {
                return false;
            }
            i += 1;
        }
        // Skip the function's own type parameters: `T id<T>(T x)`.
        if self.peek_at(i) == &Tok::Lt {
            let mut depth = 0;
            loop {
                match self.peek_at(i) {
                    Tok::Lt => depth += 1,
                    Tok::Gt => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    Tok::Eof => return false,
                    _ => {}
                }
                i += 1;
            }
        }
        self.peek_at(i) == &Tok::LParen
    }

    /// `distinct int Price;`
    fn parse_distinct(&mut self) -> Result<TypeDecl, Diag> {
        let span = self.span();
        self.expect(Tok::KwDistinct)?;
        let base = self.expect_ty()?;
        if base == Ty::Void {
            return Err(Diag::new(span, "`void` is not a value type"));
        }
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::Semi)?;
        Ok(TypeDecl {
            name,
            tparams: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            is_interface: false,
            distinct_base: Some(base),
            span,
        })
    }

    fn parse_type_decl(&mut self) -> Result<TypeDecl, Diag> {
        let span = self.span();
        // Keyword first, so the kind is known before the name:
        //   type Point { .. }        a struct
        //   interface HasArea { .. } an interface
        let is_interface = self.peek() == &Tok::KwInterface;
        self.bump();
        let (name, _) = self.expect_ident()?;
        let tparams = self.parse_tparams()?;
        self.tparams = tparams.clone();
        self.expect(Tok::LBrace)?;

        if is_interface {
            // Signatures only: `int area();`
            let mut methods = Vec::new();
            while self.peek() != &Tok::RBrace {
                if self.peek() == &Tok::Eof {
                    return Err(Diag::new(self.span(), "expected `}`, found end of file"));
                }
                let mspan = self.span();
                let ret = self.expect_ty()?;
                let (mname, _) = self.expect_ident()?;
                self.expect(Tok::LParen)?;
                let mut params = Vec::new();
                if self.peek() != &Tok::RParen {
                    loop {
                        params.push(self.parse_param()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(Tok::RParen)?;
                self.expect(Tok::Semi)?;
                methods.push(Func {
                    ret,
                    recv: Some(name.clone()),
                    name: mname,
                    tparams: Vec::new(),
                    params,
                    body: Vec::new(),
                    span: mspan,
                });
            }
            self.expect(Tok::RBrace)?;
            self.tparams.clear();
            return Ok(TypeDecl {
                name,
                tparams,
                fields: Vec::new(),
                methods,
                is_interface: true,
                distinct_base: None,
                span,
            });
        }
        let mut fields = Vec::new();
        while self.peek() != &Tok::RBrace {
            if self.peek() == &Tok::Eof {
                return Err(Diag::new(self.span(), "expected `}`, found end of file"));
            }
            fields.push(self.parse_param()?);
            self.expect(Tok::Semi)?;
        }
        self.expect(Tok::RBrace)?;
        self.tparams.clear();
        Ok(TypeDecl {
            name,
            tparams,
            fields,
            methods: Vec::new(),
            is_interface: false,
            distinct_base: None,
            span,
        })
    }

    /// `<T>` / `<K, V>` after a declaration's name, or nothing.
    fn parse_tparams(&mut self) -> Result<Vec<String>, Diag> {
        let mut out = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                let (n, s) = self.expect_ident()?;
                if out.contains(&n) {
                    return Err(Diag::new(s, format!("duplicate type parameter `{n}`")));
                }
                out.push(n);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::Gt)?;
        }
        Ok(out)
    }

    fn parse_func(&mut self) -> Result<Func, Diag> {
        let span = self.span();
        // A generic function's return type may mention its own parameters, so
        // the `<T>` list has to be read before the return type. It sits after
        // the name in the source, so scan ahead for it first.
        let tparams = self.scan_fn_tparams()?;
        self.tparams = tparams.clone();
        let ret = self.expect_ty()?;
        let (first, _) = self.expect_ident()?;
        let (recv, name) = if self.eat(&Tok::Dot) {
            let (m, _) = self.expect_ident()?;
            (Some(first), m)
        } else {
            (None, first)
        };
        let after = self.parse_tparams()?;
        debug_assert_eq!(after, tparams);
        self.expect(Tok::LParen)?;

        let mut params = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                params.push(self.parse_param()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        let body = self.parse_block()?;
        self.tparams.clear();
        Ok(Func {
            ret,
            recv,
            name,
            tparams,
            params,
            body,
            span,
        })
    }

    /// Look ahead past `<ret> <name>` for a `<T, ..>` list, without consuming
    /// anything. Needed because a generic function's return type can mention
    /// its own type parameters.
    fn scan_fn_tparams(&mut self) -> Result<Vec<String>, Diag> {
        let save = self.pos;
        let mut out = Vec::new();
        // Skip the return type: a keyword, or a name with optional arguments.
        if matches!(self.peek(), Tok::Ident(_)) || Self::ty_of(self.peek()).is_some() {
            self.bump();
            if self.peek() == &Tok::Lt {
                let mut depth = 0;
                loop {
                    match self.peek() {
                        Tok::Lt => depth += 1,
                        Tok::Gt => {
                            depth -= 1;
                            if depth == 0 {
                                self.bump();
                                break;
                            }
                        }
                        Tok::Eof => break,
                        _ => {}
                    }
                    self.bump();
                }
            }
        }
        if matches!(self.peek(), Tok::Ident(_)) {
            self.bump();
            if self.peek() == &Tok::Lt {
                self.bump();
                while let Tok::Ident(n) = self.peek().clone() {
                    out.push(n);
                    self.bump();
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
        }
        self.pos = save;
        Ok(out)
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
            self.expect(Tok::KwIn)?;
            let iter = self.parse_expr(0)?;
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
                other => Err(Diag::new(other.span(), "cannot assign to this expression")),
            };
        }
        self.expect(Tok::Semi)?;
        Ok(Stmt::Eval { expr: lhs, span })
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

    /// Postfix chain: `.field` for now.
    fn parse_postfix(&mut self, mut e: Expr) -> Result<Expr, Diag> {
        while self.peek() == &Tok::Dot {
            let span = self.span();
            self.bump();
            let (name, _) = self.expect_ident()?;
            e = if self.peek() == &Tok::LParen {
                let args = self.parse_args()?;
                Expr::MethodCall(Box::new(e), name, args, span)
            } else {
                Expr::Field(Box::new(e), name, span)
            };
        }
        // Indexing, and chains of it: `grid[i][j]`.
        while self.peek() == &Tok::LBracket {
            let span = self.span();
            self.bump();
            let i = self.parse_expr(0)?;
            self.expect(Tok::RBracket)?;
            e = Expr::Index(Box::new(e), Box::new(i), span);
            // A postfix chain may continue after an index: `xs[0].name`.
            if self.peek() == &Tok::Dot {
                return self.parse_postfix(e);
            }
        }
        Ok(e)
    }

    fn parse_prefix(&mut self) -> Result<Expr, Diag> {
        let e = self.parse_atom()?;
        self.parse_postfix(e)
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
            // `int(x)` -- a conversion back to a base type. A type keyword
            // is not otherwise an expression, so this is unambiguous.
            t if Self::ty_of(&t).is_some() && self.peek_at(1) == &Tok::LParen => {
                let name = t.spelling().to_string();
                self.bump();
                let args = self.parse_args()?;
                Ok(Expr::Call(name, args, span))
            }
            Tok::Ident(name) if self.is_ty_name(&name) => {
                // Construction: always by field name, so reordering fields in
                // the declaration cannot silently transpose arguments.
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
                if self.peek() != &Tok::LParen {
                    // Nothing shadows a type, so a bare type name here is
                    // never a variable -- say that rather than demanding a
                    // `(` the writer did not mean to type.
                    return Err(Diag::new(span, format!("`{name}` is a type, not a value")));
                }
                let ty = self.intern(name, targs);
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
            other => Err(Diag::new(
                span,
                format!("expected an expression, found {}", other.describe()),
            )),
        }
    }
}
