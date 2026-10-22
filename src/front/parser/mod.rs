//! Recursive descent for statements, Pratt for expressions.
//!
//! Declarations are type-first (`int x = 5;`), C/Java shaped. That is
//! unambiguous here without C's lexer hack for two reasons: type names are
//! keywords, and there are no pointer declarators. At statement position a
//! type keyword means "declaration" and nothing else. When user-defined type
//! names arrive, the rule becomes `IDENT IDENT` with two tokens of lookahead,
//! which is how Java manages the same grammar.

use crate::front::ast::*;
use crate::front::diag::{Diag, Span};
use crate::front::lexer::{Tok, Token};

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// Names declared by `type`, from a pre-pass, so a type can be used
    /// before it is declared and `IDENT IDENT` is decidable with two tokens.
    type_names: Vec<String>,
    /// Modules this file imports, so `lib.Point` reads as a type.
    imports: Vec<String>,
    /// Type names DECLARED IN THIS FILE, as opposed to builtin ones. A name
    /// in here is interned module-qualified, so two files may each declare a
    /// `Point` without becoming the same type in the shared arena.
    own: Vec<String>,
    builtin: Vec<String>,
    /// The generic types this file declares, so a method on one written
    /// without its type parameters (`T Wrap.get()`) is refused with the
    /// spelling it needs rather than an unknown `T`.
    generic_own: Vec<String>,
    /// Type parameter names in scope while parsing a generic declaration.
    /// `T` inside `type Wrap<T>` must parse as a type even though no `type T`
    /// exists.
    tparams: Vec<String>,
    /// Interned type expressions; `Ty::User` indexes this.
    ty_exprs: Vec<TyExpr>,
    /// The module currently being parsed; every declaration records it.
    module: String,
    /// This file came out of `lib/` and is compiled into the compiler, so it
    /// may write `prim` and name `__`-prefixed functions. See
    /// docs/stdlib-seam.md -- the restriction is the only thing keeping the
    /// seam from being a foreign function interface.
    stdlib: bool,
    /// How deeply the parser is nested right now: one per statement,
    /// expression and type it is inside. See `MAX_DEPTH`.
    depth: usize,
    /// Set while a parameter's or a field's default is being read. A default
    /// is evaluated where the call or the construction is written, not
    /// inside the declaration it belongs to, so a `this` in one would be
    /// whatever method happens to be calling -- refused where it is written.
    in_default: bool,
    /// Set while the lower bound of a `for` header's range is being read --
    /// the one place `..` may follow an expression. Everywhere else an
    /// expression that stops at `..` is a range written where no range can
    /// go, and saying so beats reporting that `;` was expected.
    in_range: bool,
    /// Where each parenthesised expression the source WROTE begins: the span
    /// of the expression inside the parentheses, not of the `(`.
    ///
    /// Parentheses do not survive into the AST -- `(a && b) || c` and
    /// `a && b || c` are one tree -- so a formatter printing from the AST
    /// cannot know which of them a reader put there on purpose. It printed
    /// the minimum precedence needs, which turned `apps/git`'s SHA-1 round
    /// (`f = d ^ (bb & (c ^ d))`, checkable line by line against FIPS 180-4)
    /// into `f = d ^ bb & (c ^ d)`, and `apps/tui`'s `(off & BOLD) != 0`
    /// into `off & BOLD != 0`. The parentheses were the checking.
    ///
    /// This is the same trick the lexer plays for a literal's spelling
    /// (`Lexer::spellings`): keep what the tokens lose, keyed by position.
    /// A span is the position of the one token an expression starts at, so
    /// it names the node uniquely; and if it ever did not, the cost is one
    /// redundant pair of parentheses, which changes no meaning and still
    /// reaches a fixed point.
    ///
    /// Only `parse_atom`'s grouping `(` is recorded. The parentheses of an
    /// `if`, a `while`, a `for`, a `match`, a call and a lambda are syntax,
    /// not grouping, and the formatter writes those itself.
    pub parens: Vec<Span>,
}

/// The deepest a program may nest, counting statements, expressions and
/// types together.
///
/// Every pass after the parser -- monomorphisation, lowering, the formatter
/// -- walks the tree recursively, and so does the parser itself. A program
/// nested a thousand deep overflowed the stack and killed the compiler with
/// a Rust abort instead of a diagnostic. The limit is here, once, because
/// the parser is the one pass that sees the nesting before the tree exists;
/// making every later pass iterative would be a rewrite of all of them for a
/// shape no one writes by hand.
///
/// The number is measured, not guessed. With no limit, in a debug build on
/// the default 8MB main-thread stack, the most expensive shape per level is
/// a nested block -- `if`, `while`, `for` and `match` alike, about 25KB a
/// level -- and 328 of them was the most that survived. Parentheses, which
/// only the parser recurses on, got to 488; generic types to 420. 256 keeps
/// more than a fifth of the stack spare in the worst case, and every shape
/// at exactly this depth -- blocks inside and outside functions, `else if`
/// chains, parentheses, calls, method chains, literals, nested generic types
/// -- compiles, runs and formats. It is also far past anything a person
/// writes: C requires a compiler to accept only 63 levels of parentheses.
///
/// If a pass grows a much larger stack frame, this is the number to revisit;
/// the corpus has a program at the limit so that it fails first.
///
/// What counts is the depth of the TREE, not of the parser's recursion. A
/// flat `a + b + c + ...` is parsed by a loop but builds a left-leaning tree
/// as deep as the chain is long, and every later pass recurses down it, so a
/// chain of 5000 terms is refused as surely as 5000 parentheses.
const MAX_DEPTH: usize = 256;

mod decl;
mod expr;
mod native;
mod stmt;
mod types;

use native::RawNative;

impl Parser {
    /// A parser that continues an existing type arena.
    ///
    /// Every file must intern into ONE arena. `Ty::User` is an index into it,
    /// so per-file arenas concatenated afterwards leave every index but the
    /// first file's pointing at whatever the first file happened to put
    /// there -- which compiled cleanly and called the wrong method.
    pub fn with_arena(toks: Vec<Token>, ty_exprs: Vec<TyExpr>) -> Self {
        let mut p = Self::new(toks);
        p.ty_exprs = ty_exprs;
        p
    }

    /// Mark this file as standard library source. Set by the module loader
    /// for an embedded module and by nothing else, so a file on disk can
    /// never claim it.
    pub fn stdlib(mut self) -> Self {
        self.stdlib = true;
        self
    }

    pub fn new(toks: Vec<Token>) -> Self {
        // Pre-pass: every `type IDENT` in the stream. This is what makes a
        // type-first grammar decidable without C's lexer hack -- the parser
        // knows the type names before it starts, so `Point p` is a
        // declaration and `foo p` is an error, not an ambiguity.
        // The builtin collections and channels: the runtime owns their
        // representation, so there is no `type Chan<T>` in the source to
        // find, but they must parse as type names like any other.
        let builtin: Vec<String> = [
            "Chan", "Array", "List", "Map",
            // Declared by the compiler rather than by the program -- see
            // `prelude_types`. Named here so a source file can write
            // `Option<int>` without having declared it.
            "Option", "Result",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut own: Vec<String> = Vec::new();
        let mut generic_own: Vec<String> = Vec::new();
        let mut type_names = builtin.clone();
        for w in toks.windows(3) {
            if matches!(w[0].tok, Tok::KwType | Tok::KwInterface | Tok::KwEnum)
                && w[2].tok == Tok::Lt
            {
                if let Tok::Ident(n) = &w[1].tok {
                    generic_own.push(n.clone());
                }
            }
        }
        for w in toks.windows(2) {
            if w[0].tok == Tok::KwType || w[0].tok == Tok::KwInterface || w[0].tok == Tok::KwEnum {
                if let Tok::Ident(n) = &w[1].tok {
                    type_names.push(n.clone());
                    own.push(n.clone());
                }
            }
        }
        // `distinct <type> Name;` -- the name follows the base type, which
        // is not always one token: `distinct List<int> Bag;` and
        // `distinct lib.Point Here;` both have more in the middle. Walk past
        // the base the way `expect_ty` would read it -- an optional `mod.`,
        // the name, then a balanced `<...>` -- and take the identifier after.
        // Assuming a fixed position registered `int` or `<` as the name and
        // left the real one unusable as a type.
        for (i, t) in toks.iter().enumerate() {
            if t.tok != Tok::KwDistinct {
                continue;
            }
            let at = |j: usize| toks.get(j).map(|t| &t.tok);
            let mut j = i + 1;
            if matches!(at(j), Some(Tok::Ident(_))) && at(j + 1) == Some(&Tok::Dot) {
                j += 2;
            }
            j += 1;
            if at(j) == Some(&Tok::Lt) {
                let mut depth = 0usize;
                while let Some(t) = at(j) {
                    match t {
                        Tok::Lt => depth += 1,
                        Tok::Gt => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        Tok::Semi | Tok::Eof => break,
                        _ => {}
                    }
                    j += 1;
                }
            }
            if let Some(Tok::Ident(n)) = at(j) {
                type_names.push(n.clone());
                own.push(n.clone());
            }
        }
        Parser {
            toks,
            pos: 0,
            type_names,
            own,
            builtin,
            generic_own,
            imports: Vec::new(),
            tparams: Vec::new(),
            ty_exprs: Vec::new(),
            module: String::new(),
            stdlib: false,
            depth: 0,
            in_default: false,
            in_range: false,
            parens: Vec::new(),
        }
    }

    /// Run one nested parse, refusing it past `MAX_DEPTH`. `what` names the
    /// kind of thing that nested too far, for the diagnostic.
    fn nested<T>(
        &mut self,
        what: &str,
        f: impl FnOnce(&mut Self) -> Result<T, Diag>,
    ) -> Result<T, Diag> {
        if self.depth >= MAX_DEPTH {
            return Err(self.too_deep(what, self.span()));
        }
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn too_deep(&self, what: &str, span: Span) -> Diag {
        Diag::new(
            span,
            format!("{what} nested too deeply (limit {MAX_DEPTH})"),
        )
    }

    /// A loop just made the tree under `e` one level taller without
    /// recursing -- a binary operator or a postfix link. `h` is the height
    /// of `e`, kept by the loop so the chain is not re-measured each time.
    fn check_height(&self, h: usize, e: &Expr) -> Result<(), Diag> {
        if self.depth + h > MAX_DEPTH {
            return Err(self.too_deep("expression", e.span()));
        }
        Ok(())
    }

    fn parse_stmt(&mut self) -> Result<Stmt, Diag> {
        self.nested("statement", Self::parse_stmt_inner)
    }

    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr, Diag> {
        self.nested("expression", |p| p.parse_expr_inner(min_bp))
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

    fn expect_ident(&mut self) -> Result<(String, Span), Diag> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Ident(n) => {
                // `__` is reserved, and this is the one funnel every name in
                // the language goes through -- declarations, uses, fields,
                // methods and arguments alike. Reserving it outright is what
                // lets the standard library's seam to C be invisible: a
                // program cannot call a primitive, and cannot declare
                // something a primitive's name would collide with. C reserves
                // the same prefix for the same reason.
                if n.starts_with("__") && !self.stdlib {
                    self.bump();
                    return Err(Diag::new(
                        span,
                        format!("`{n}` is reserved: a name may not begin with `__`"),
                    ));
                }
                self.bump();
                Ok((n, span))
            }
            // Said outright, because "expected a name" reads as if the
            // parser lost its place when the program only chose a reserved
            // word for a variable.
            Tok::KwThis => Err(Diag::new(
                span,
                "`this` is a keyword -- it names a method's receiver -- and \
                 cannot be used as a name",
            )),
            Tok::Underscore => Err(Diag::new(
                span,
                "`_` is a discard, not a name: only a `case` payload may be written \
                 `_` (`case Err(_)`)",
            )),
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

    /// A parameter or field: `int x` is mandatory, `int x = 0` is optional,
    /// and `const T x` takes only a frozen argument (§4.2). Whether `const`
    /// is allowed where it was written is the caller's to say: a field
    /// refuses it.
    fn parse_param(&mut self) -> Result<Param, Diag> {
        let span = self.span();
        // Only a field has a visibility, and the type declaration eats its
        // `pub` before getting here. Anywhere else the word would surface
        // as a confusing "expected a type".
        if self.peek() == &Tok::KwPub {
            return Err(Diag::new(
                span,
                "`pub` exports a declaration or a field; a parameter has no visibility",
            ));
        }
        let is_const = self.eat(&Tok::KwConst);
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
                is_pub: false,
                is_const,
                span,
            });
        }
        let (name, _) = self.expect_ident()?;
        let default = if self.eat(&Tok::Assign) {
            self.in_default = true;
            let e = self.parse_expr(0);
            self.in_default = false;
            Some(e?)
        } else {
            None
        };
        Ok(Param {
            ty,
            name,
            default,
            embedded: false,
            is_pub: false,
            is_const,
            span,
        })
    }
}
