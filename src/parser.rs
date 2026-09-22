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
    /// Modules this file imports, so `lib.Point` reads as a type.
    imports: Vec<String>,
    /// Type names DECLARED IN THIS FILE, as opposed to builtin ones. A name
    /// in here is interned module-qualified, so two files may each declare a
    /// `Point` without becoming the same type in the shared arena.
    own: Vec<String>,
    builtin: Vec<String>,
    /// Type parameter names in scope while parsing a generic declaration.
    /// `T` inside `type Box<T>` must parse as a type even though no `type T`
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

/// The height of an expression tree. Recursive, which is safe: it is only
/// ever asked about a tree the parser has already held to `MAX_DEPTH`.
fn height(e: &Expr) -> usize {
    1 + match e {
        Expr::Int(..) | Expr::Float(..) | Expr::Bool(..) | Expr::Str(..) | Expr::Var(..) => 0,
        Expr::Un(_, x, _) | Expr::Field(x, _, _) | Expr::Try(x, _) => height(x),
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
fn args_height(a: &Args) -> usize {
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
const SHIFT_BP: u8 = 8;

const UNARY_BP: u8 = 11;

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
        let mut type_names = builtin.clone();
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
            imports: Vec::new(),
            tparams: Vec::new(),
            ty_exprs: Vec::new(),
            module: String::new(),
            stdlib: false,
            depth: 0,
        }
    }

    /// Is this name usable as a type here -- a declared type, or a type
    /// parameter currently in scope?
    fn is_ty_name(&self, name: &str) -> bool {
        self.type_names.iter().any(|n| n == name) || self.tparams.iter().any(|n| n == name)
    }

    /// The interned spelling of a type name written in this file.
    ///
    /// A name this file declares becomes `module#Name`. The arena is shared
    /// across every file, so without this two modules each declaring `Point`
    /// would dedupe to one entry and become the same type. Builtins and type
    /// parameters stay bare: they mean the same thing in every file.
    ///
    /// `#` cannot appear in a source identifier, so a qualified name can
    /// never collide with one someone wrote.
    fn qualify(&self, name: &str) -> String {
        if self.module.is_empty()
            || self.builtin.iter().any(|b| b == name)
            || self.tparams.iter().any(|t| t == name)
            || !self.own.iter().any(|o| o == name)
        {
            return name.to_string();
        }
        format!("{}#{name}", self.module)
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
        // `lib.P v = ..` is a declaration; `lib.f(..)` is a call. Both begin
        // with a module name and a dot, so the difference is what follows
        // the name after it -- an identifier, or a `(`.
        if let Tok::Ident(m) = self.peek() {
            if self.imports.iter().any(|x| x == m) && self.peek_at(1) == &Tok::Dot {
                let mut i = 2;
                if !matches!(self.peek_at(i), Tok::Ident(_)) {
                    return false;
                }
                i += 1;
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
                return matches!(self.peek_at(i), Tok::Ident(_));
            }
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

    /// At `lib . Name < ... >`, the offset just past the `>` -- provided
    /// everything between the angle brackets could be a type argument list
    /// and a `(` or `.` follows it, which is what makes it a construction or
    /// a member of a generic type rather than anything else.
    fn qualified_targs_end(&self) -> Option<usize> {
        if self.peek_at(3) != &Tok::Lt {
            return None;
        }
        let mut i = 3;
        let mut depth = 0usize;
        loop {
            match self.peek_at(i) {
                Tok::Lt => depth += 1,
                Tok::Gt => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Tok::Ident(_) | Tok::Comma | Tok::Dot => {}
                t if Self::ty_of(t).is_some() => {}
                _ => return None,
            }
            i += 1;
        }
        i += 1;
        matches!(self.peek_at(i), Tok::LParen | Tok::Dot).then_some(i)
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

    fn expect_ty(&mut self) -> Result<Ty, Diag> {
        self.nested("type", Self::expect_ty_inner)
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

    fn ty_of(t: &Tok) -> Option<Ty> {
        Some(match t {
            Tok::KwInt => Ty::Int,
            Tok::KwFloat => Ty::Float,
            Tok::KwBool => Ty::Bool,
            Tok::KwStr => Ty::Str,
            Tok::KwBytes => Ty::Bytes,
            Tok::KwVoid => Ty::Void,
            _ => return None,
        })
    }

    fn expect_ty_inner(&mut self) -> Result<Ty, Diag> {
        // `lib.P` -- a type from another module. It interns to the same
        // `lib#P` the declaring file produced, which is what makes the two
        // spellings name one type.
        if let Tok::Ident(m) = self.peek().clone() {
            if self.imports.contains(&m) && self.peek_at(1) == &Tok::Dot {
                let span = self.span();
                self.bump();
                self.bump();
                let (n, _) = self.expect_ident()?;
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
                let _ = span;
                return Ok(self.intern(format!("{m}#{n}"), args));
            }
        }
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
                let name = self.qualify(&name);
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

    /// The two enums the language declares for you.
    ///
    /// They are built in for one hard reason: a built-in method cannot
    /// return a user-defined type, because the compiler has to know what
    /// `xs.index_of(v)` gives back. Without a blessed `Option` that method
    /// cannot exist at all, and neither can `parse_int`. The second reason is
    /// composition -- two libraries with their own `Result` cannot pass one
    /// through the other.
    ///
    /// They are built AS AST, not parsed from a prelude string, so they land
    /// in the same interned type arena as the program and shift nobody's line
    /// numbers. Past that they are completely ordinary enums: the same
    /// `match`, the same exhaustiveness, no special construction syntax.
    fn prelude_types(&mut self) -> Vec<TypeDecl> {
        // Nothing here can fail, so the span is never rendered. It still has
        // to be a real line, because a diagnostic would try to quote it.
        let span = Span::new(1, 1);
        let mut out = Vec::new();

        for (name, tparams, variants) in [
            (
                "Option",
                vec!["T"],
                vec![("None", vec![]), ("Some", vec!["T"])],
            ),
            (
                "Result",
                vec!["T", "E"],
                vec![("Ok", vec!["T"]), ("Err", vec!["E"])],
            ),
        ] {
            let variants = variants
                .into_iter()
                .map(|(vname, payload): (&str, Vec<&str>)| EnumVariant {
                    name: vname.to_string(),
                    payload: payload
                        .into_iter()
                        .map(|p| self.intern(p.to_string(), Vec::new()))
                        .collect(),
                    span,
                })
                .collect();
            out.push(TypeDecl {
                name: name.to_string(),
                module: String::new(),
                is_pub: true,
                tparams: tparams.into_iter().map(str::to_string).collect(),
                fields: Vec::new(),
                methods: Vec::new(),
                is_interface: false,
                variants,
                is_enum: true,
                distinct_base: None,
                span,
            });
        }
        out
    }

    pub fn parse_program(&mut self, module: &str) -> Result<Program, Diag> {
        self.module = module.to_string();
        let mut funcs = Vec::new();
        let mut types = Vec::new();
        let mut toplevel = Vec::new();
        let mut imports: Vec<Import> = Vec::new();

        // Imports come first, so a reader knows a file's dependencies without
        // reading the file.
        while self.peek() == &Tok::KwImport {
            let span = self.span();
            self.bump();
            let (name, _) = self.expect_ident()?;
            self.expect(Tok::Semi)?;
            if name == module {
                return Err(Diag::new(span, format!("`{name}` imports itself")));
            }
            if imports.iter().any(|i| i.name == name) {
                return Err(Diag::new(span, format!("`{name}` is imported twice")));
            }
            self.imports.push(name.clone());
            imports.push(Import { name, span });
        }

        while self.peek() != &Tok::Eof {
            if self.peek() == &Tok::KwImport {
                return Err(Diag::new(
                    self.span(),
                    "every `import` must come before the first declaration",
                ));
            }
            // Private is the default: forgetting to mark something private
            // would export it permanently, and forgetting to mark something
            // public is a one-word fix.
            let is_pub = self.eat(&Tok::KwPub);
            if self.peek() == &Tok::KwDistinct {
                types.push(self.parse_distinct(is_pub)?);
            } else if self.peek() == &Tok::KwEnum {
                types.push(self.parse_enum_decl(is_pub)?);
            } else if self.peek() == &Tok::KwType || self.peek() == &Tok::KwInterface {
                types.push(self.parse_type_decl(is_pub)?);
            } else if self.starts_func() {
                funcs.push(self.parse_func(is_pub)?);
            } else if is_pub {
                return Err(Diag::new(
                    self.span(),
                    "`pub` marks a declaration; a statement has nothing to export",
                ));
            } else {
                // Anything else at the top level is a statement, and runs.
                toplevel.push(self.parse_stmt()?);
            }
        }
        let prelude = self.prelude_types();
        // A program may not redeclare what the compiler already declared.
        for t in &types {
            if let Some(p) = prelude.iter().find(|p| p.name == t.name) {
                let _ = p;
                return Err(Diag::new(
                    t.span,
                    format!(
                        "`{}` is declared by the language; it cannot be redeclared",
                        t.name
                    ),
                ));
            }
        }
        Ok(Program {
            module: module.to_string(),
            imports,
            imports_by_module: std::collections::HashMap::new(),
            types,
            prelude,
            funcs,
            toplevel,
            ty_exprs: std::mem::take(&mut self.ty_exprs),
            shown: std::collections::HashMap::new(),
        })
    }

    /// Distinguish a function declaration from a top-level statement.
    ///
    /// Both can begin with a type: `int f(..) {` declares, `int x = ..;`
    /// does not. The difference is a `(` after the name -- and after any
    /// generic parameter list. Three tokens of lookahead, no backtracking.
    fn starts_func(&self) -> bool {
        // `static` and `prim` can only begin a function declaration, so
        // either settles the question by itself.
        if self.peek() == &Tok::KwStatic || self.peek() == &Tok::KwPrim {
            return true;
        }
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
    fn parse_distinct(&mut self, is_pub: bool) -> Result<TypeDecl, Diag> {
        let span = self.span();
        self.expect(Tok::KwDistinct)?;
        let base = self.expect_ty()?;
        if base == Ty::Void {
            return Err(Diag::new(span, "`void` is not a value type"));
        }
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::Semi)?;
        let name = self.qualify(&name);
        Ok(TypeDecl {
            name,
            module: self.module.clone(),
            is_pub,
            tparams: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            is_interface: false,
            variants: Vec::new(),
            is_enum: false,
            distinct_base: Some(base),
            span,
        })
    }

    fn parse_type_decl(&mut self, is_pub: bool) -> Result<TypeDecl, Diag> {
        let span = self.span();
        // Keyword first, so the kind is known before the name:
        //   type Point { .. }        a struct
        //   interface HasArea { .. } an interface
        let is_interface = self.peek() == &Tok::KwInterface;
        self.bump();
        let (name, _) = self.expect_ident()?;
        let name = self.qualify(&name);
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
                    module: self.module.clone(),
                    is_pub: true,
                    ret,
                    // An interface lists instance methods: a static one has
                    // no receiver, so there is nothing to dispatch on.
                    is_static: false,
                    is_prim: false,
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
                module: self.module.clone(),
                is_pub,
                tparams,
                fields: Vec::new(),
                methods,
                is_interface: true,
                variants: Vec::new(),
                is_enum: false,
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
            module: self.module.clone(),
            is_pub,
            tparams,
            fields,
            methods: Vec::new(),
            is_interface: false,
            variants: Vec::new(),
            is_enum: false,
            distinct_base: None,
            span,
        })
    }

    /// `enum Option<T> { None; Some(T); }`
    ///
    /// Variants are semicolon-terminated like fields, so a declaration reads
    /// the same shape whichever kind it is. A payload is a positional list of
    /// types with no names: a variant is not a struct, and if there is enough
    /// in it to want field names then the payload should BE a struct.
    fn parse_enum_decl(&mut self, is_pub: bool) -> Result<TypeDecl, Diag> {
        let span = self.span();
        self.expect(Tok::KwEnum)?;
        let (name, _) = self.expect_ident()?;
        let name = self.qualify(&name);
        let tparams = self.parse_tparams()?;
        self.tparams = tparams.clone();
        self.expect(Tok::LBrace)?;

        let mut variants: Vec<EnumVariant> = Vec::new();
        while self.peek() != &Tok::RBrace {
            if self.peek() == &Tok::Eof {
                return Err(Diag::new(self.span(), "expected `}`, found end of file"));
            }
            let vspan = self.span();
            let (vname, _) = self.expect_ident()?;
            if variants.iter().any(|v| v.name == vname) {
                return Err(Diag::new(
                    vspan,
                    format!("duplicate variant `{vname}` in enum `{name}`"),
                ));
            }
            let mut payload = Vec::new();
            if self.eat(&Tok::LParen) {
                if self.peek() == &Tok::RParen {
                    return Err(Diag::new(
                        self.span(),
                        format!(
                            "`{vname}` has an empty payload; write `{vname};` for a \
                             variant that carries nothing"
                        ),
                    ));
                }
                loop {
                    let pspan = self.span();
                    let t = self.expect_ty()?;
                    if t == Ty::Void {
                        return Err(Diag::new(pspan, "`void` is not a value type"));
                    }
                    payload.push(t);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen)?;
            }
            self.expect(Tok::Semi)?;
            variants.push(EnumVariant {
                name: vname,
                payload,
                span: vspan,
            });
        }
        self.expect(Tok::RBrace)?;
        self.tparams.clear();

        if variants.is_empty() {
            return Err(Diag::new(
                span,
                format!(
                    "enum `{}` has no variants, so no value of it can exist",
                    crate::ast::bare(&name)
                ),
            ));
        }
        Ok(TypeDecl {
            name,
            module: self.module.clone(),
            is_pub,
            tparams,
            fields: Vec::new(),
            methods: Vec::new(),
            is_interface: false,
            variants,
            is_enum: true,
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

    fn parse_func(&mut self, is_pub: bool) -> Result<Func, Diag> {
        let span = self.span();
        // A generic function's return type may mention its own parameters, so
        // `static Point Point.origin()` -- a method on the TYPE. Read first,
        // so everything after it parses exactly like an ordinary method.
        // The seam to C (docs/stdlib-seam.md). Read before `static` so the
        // two cannot be written in either order -- a primitive is a free
        // function, and `prim static` would have to be rejected anyway.
        let is_prim = self.eat(&Tok::KwPrim);
        if is_prim && !self.stdlib {
            return Err(Diag::new(
                span,
                "`prim` is the standard library's seam to C and is only \
                 available to source the compiler ships; there is no foreign \
                 function interface yet",
            ));
        }
        let is_static = self.eat(&Tok::KwStatic);
        // the `<T>` list has to be read before the return type. It sits after
        // the name in the source, so scan ahead for it first.
        let tparams = self.scan_fn_tparams()?;
        self.tparams = tparams.clone();
        let ret = self.expect_ty()?;
        let (first, _) = self.expect_ident()?;
        let (recv, name) = if self.eat(&Tok::Dot) {
            let (m, _) = self.expect_ident()?;
            // The receiver names a type, so it is qualified the same way the
            // type's own declaration was.
            (Some(self.qualify(&first)), m)
        } else {
            (None, first)
        };
        if is_static && recv.is_none() {
            return Err(Diag::new(
                span,
                "`static` describes a method on a type; write it as \
                 `static T Type.name(..)`",
            ));
        }
        // A free function is qualified by its module the same way a type is,
        // so two modules may each have a private `helper`. A METHOD is not:
        // its receiver is already qualified, and `lib#Rect.area` is unique
        // without touching the method name.
        let name = if recv.is_none() && !self.module.is_empty() {
            format!("{}#{name}", self.module)
        } else {
            name
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
        // A primitive's body is C's, so there is none to read. Everything
        // above this point parsed exactly like an ordinary function, which
        // is the point: its types are checked and monomorphised like
        // anybody else's.
        let body = if is_prim {
            self.prim_signature_only(span, &recv, &tparams)?;
            Vec::new()
        } else {
            self.parse_block()?
        };
        self.tparams.clear();
        Ok(Func {
            module: self.module.clone(),
            is_pub,
            ret,
            is_static,
            is_prim,
            recv,
            name,
            tparams,
            params,
            body,
            span,
        })
    }

    /// What a `prim` may not be. The checks are here rather than in the
    /// lowerer because they are all about the written declaration, and
    /// because refusing them at the source keeps the set of C symbols a
    /// program can reach equal to the list of `prim` lines in one file.
    fn prim_signature_only(
        &mut self,
        span: Span,
        recv: &Option<String>,
        tparams: &[String],
    ) -> Result<(), Diag> {
        if recv.is_some() {
            return Err(Diag::new(
                span,
                "a `prim` is a free function: the seam is one C symbol per \
                 declaration, and a method would put it behind a receiver",
            ));
        }
        if !tparams.is_empty() {
            return Err(Diag::new(
                span,
                "a `prim` may not be generic: one declaration is one C \
                 symbol, and a generic one would need a runtime function per \
                 instantiation",
            ));
        }
        self.expect(Tok::Semi)?;
        Ok(())
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

    fn parse_stmt_inner(&mut self) -> Result<Stmt, Diag> {
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
                        let ty = self.expect_ty()?;
                        if ty == Ty::Void {
                            return Err(Diag::new(bspan, "`void` is not a value type"));
                        }
                        let (name, _) = self.expect_ident()?;
                        binds.push(Param {
                            ty,
                            name,
                            default: None,
                            embedded: false,
                            span: bspan,
                        });
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
                other => Err(Diag::new(other.span(), "cannot assign to this expression")),
            };
        }
        self.expect(Tok::Semi)?;
        Ok(Stmt::Eval { expr: lhs, span })
    }

    // ---- expressions (Pratt) -----------------------------------------

    fn parse_expr_inner(&mut self, min_bp: u8) -> Result<Expr, Diag> {
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
            // `lib.Box<int>(..)`, `lib.Res<int>.Ok(1)`. The parser cannot
            // know that `Box` is a type in `lib`, but `lib.x < ...` can never
            // be a comparison worth reading this way: a module exports
            // functions and types, not values, so `lib.x` alone is not an
            // operand. Plain `lib.Point(..)` has no such marker and stays a
            // qualified call; the lowerer builds the type when `lib` has one
            // by that name.
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
            other => Err(Diag::new(
                span,
                format!("expected an expression, found {}", other.describe()),
            )),
        }
    }
}
