//! Items: the program, constants, `type` and `enum` declarations, functions.

use super::*;

/// What `scan_fn_tparams` finds ahead of a function's parameter list: the
/// receiver, if it is a method, and the two lists of type parameters.
struct FnHead {
    recv_name: Option<(String, Span)>,
    recv_tparams: Vec<String>,
    tparams: Vec<String>,
}

impl Parser {
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
        let mut consts = Vec::new();
        let mut imports: Vec<Import> = Vec::new();

        // Imports come first, so a reader knows a file's dependencies without
        // reading the file.
        while self.peek() == &Tok::KwImport {
            let span = self.span();
            self.bump();
            let (first, _) = self.expect_ident()?;
            let mut path = vec![first];
            while self.eat(&Tok::Dot) {
                path.push(self.expect_ident()?.0);
            }
            self.expect(Tok::Semi)?;
            let name = path.last().expect("a path has a segment").clone();
            let dotted = path.join(".");
            if name == module {
                return Err(Diag::new(
                    span,
                    if path.len() == 1 {
                        format!("`{name}` imports itself")
                    } else {
                        format!(
                            "`{dotted}` has this file's own module name, `{name}`: \
                             module names are globally unique"
                        )
                    },
                ));
            }
            if let Some(prev) = imports.iter().find(|i| i.name == name) {
                return Err(Diag::new(
                    span,
                    if prev.path == path {
                        format!("`{dotted}` is imported twice")
                    } else {
                        format!(
                            "`{dotted}` and `{}` would both be written `{name}` \
                             in this file: an import is named by its last segment \
                             and there are no aliases, so a file cannot import both",
                            prev.dotted()
                        )
                    },
                ));
            }
            self.imports.push(name.clone());
            imports.push(Import { name, path, span });
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
            } else if self.peek() == &Tok::KwConst {
                // At the top level `const` is always a module constant, in
                // the entry file as much as in a library: one spelling, one
                // meaning. A `const` local is written inside a block.
                consts.push(self.parse_const_decl(is_pub)?);
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
            consts,
            toplevel,
            ty_exprs: std::mem::take(&mut self.ty_exprs),
            shown: std::collections::HashMap::new(),
            generic_methods: std::collections::HashSet::new(),
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
        // -- `T unwrap<T>(Wrap<T> b)` -- so accept any identifier here and let
        // the shape decide. A statement can never be IDENT IDENT `(`.
        if !matches!(self.peek(), Tok::Ident(_)) && Self::ty_of(self.peek()).is_none() {
            return false;
        }
        let mut i = 1;
        // A return type may be another module's: `io.Buffer fresh()`. That is
        // three tokens, not one, and reading only the first left the name
        // where the type was expected -- so the whole declaration was read as
        // `io.Buffer fresh` the variable, and the `(` was a syntax error. A
        // qualified CALL (`io.buffer()`) is the same three tokens followed by
        // `(` rather than a name, so the test below still tells them apart.
        if self.peek_at(1) == &Tok::Dot && matches!(self.peek_at(2), Tok::Ident(_)) {
            i = 3;
        }
        // Skip type arguments on the return type: `Wrap<int> f(..)`.
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
        if !matches!(self.peek_at(i), Tok::Ident(_)) {
            return false;
        }
        i += 1;
        // The receiver's type parameters: `T Wrap<T>.get()`. Skipped here
        // and told apart from the function's own below by the `.` after.
        if self.peek_at(i) == &Tok::Lt {
            let mut j = i;
            let mut depth = 0;
            loop {
                match self.peek_at(j) {
                    Tok::Lt => depth += 1,
                    Tok::Gt => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    Tok::Eof => return false,
                    _ => {}
                }
                j += 1;
            }
            if self.peek_at(j) == &Tok::Dot {
                i = j;
            }
        }
        // A qualified method name: `int Rect.area()`.
        if self.peek_at(i) == &Tok::Dot {
            i += 1;
            // `this` is accepted as the method name only so that
            // `int Rect.this()` reaches the declaration and is refused as a
            // keyword there, instead of failing as a malformed statement.
            if !matches!(self.peek_at(i), Tok::Ident(_) | Tok::KwThis) {
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

    /// `[pub] const <type> NAME = <expr>;` at the top level. The type is
    /// mandatory, as it is everywhere else: the language always writes the
    /// type of a name down, and a constant is read far from where it is
    /// declared, which is exactly where a written type pays for itself.
    fn parse_const_decl(&mut self, is_pub: bool) -> Result<ConstDecl, Diag> {
        let span = self.span();
        self.expect(Tok::KwConst)?;
        let ty = self.expect_ty()?;
        if ty == Ty::Void {
            return Err(Diag::new(span, "a constant cannot have type `void`"));
        }
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::Assign)?;
        let init = self.parse_expr(0)?;
        self.expect(Tok::Semi)?;
        // Qualified like a free function, so two modules may each have a
        // private `LIMIT`.
        let name = if self.module.is_empty() {
            name
        } else {
            format!("{}#{name}", self.module)
        };
        Ok(ConstDecl {
            module: self.module.clone(),
            is_pub,
            ty,
            name,
            init,
            span,
        })
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
                // An interface's methods are what it IS; one it hid would be
                // a requirement nobody outside could see to satisfy.
                if self.peek() == &Tok::KwPub {
                    return Err(Diag::new(
                        mspan,
                        "an interface's methods are as visible as the interface; \
                         `pub` goes on the interface, not on a method",
                    ));
                }
                let ret = self.expect_ty()?;
                let (mname, _) = self.expect_ident()?;
                self.expect(Tok::LParen)?;
                let mut params = Vec::new();
                if self.peek() != &Tok::RParen {
                    loop {
                        let p = self.parse_param()?;
                        // A default is a NAME the caller writes (§4.2:
                        // `f(1, by: 3)`), and a call through an interface
                        // has no name to write -- the slot takes positions.
                        // Declared here, it compiled and then did neither
                        // thing: `s.go(3)` said the argument was missing and
                        // `s.go(3, by: 3)` said `by` was positional. Refused
                        // where it is written instead, so the two ways to
                        // reach a method cannot disagree.
                        if p.default.is_some() {
                            return Err(Diag::new(
                                p.span,
                                format!(
                                    "`{mname}` is an interface method, so `{}` may not have a \
                                     default: a call through an interface passes positions, not \
                                     names, and the default could never be used",
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
                    recv_tparams: Vec::new(),
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
            // `pub` on a field, as on any declaration, exports it. The span
            // starts at the keyword so a diagnostic points at the whole field.
            let fspan = self.span();
            let is_pub = self.eat(&Tok::KwPub);
            if is_pub && self.peek() == &Tok::KwPub {
                return Err(Diag::new(self.span(), "`pub` is written once"));
            }
            let mut f = self.parse_param()?;
            // A field changes with its object, or not at all: what is frozen
            // is a value, never one slot of a type. `const` on a parameter
            // is §4.2's; on a field it would be a second meaning.
            if f.is_const {
                return Err(Diag::new(
                    fspan,
                    format!(
                        "a field cannot be `const`: a field is frozen when the object \
                         holding it is, and not on its own -- drop the `const` on `{}`",
                        f.name
                    ),
                ));
            }
            f.is_pub = is_pub;
            f.span = fspan;
            fields.push(f);
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
            // A variant is not a field: the enum is one value that is one of
            // its variants, so hiding a variant would make `match` on it
            // impossible to write exhaustively outside the module.
            if self.peek() == &Tok::KwPub {
                return Err(Diag::new(
                    vspan,
                    "a variant is as visible as its enum; `pub` goes on the enum, not on a variant",
                ));
            }
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
                    crate::front::ast::bare(&name)
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
        let FnHead {
            recv_name,
            recv_tparams,
            tparams,
        } = self.scan_fn_tparams()?;
        // A method on a generic type names that type's parameters, so the
        // signature says what `T` is without sending the reader to the type
        // declaration. Refused before the return type is read, which is
        // where it used to fail, as an unknown type `T`.
        if let Some((r, rspan)) = recv_name.filter(|_| recv_tparams.is_empty()) {
            if self.generic_own.contains(&r) {
                return Err(Diag::new(
                    rspan,
                    format!(
                        "`{r}` is generic; a method on it names its type \
                         parameters on the receiver, as `{r}<T>.name(..)`"
                    ),
                ));
            }
        }
        if let Some(t) = tparams.iter().find(|t| recv_tparams.contains(t)) {
            return Err(Diag::new(
                span,
                format!(
                    "type parameter `{t}` is already the receiver's; a method's \
                     own type parameters need names of their own"
                ),
            ));
        }
        self.tparams = recv_tparams.iter().chain(tparams.iter()).cloned().collect();
        let ret = self.expect_ty()?;
        let (first, _) = self.expect_ident()?;
        // `Wrap<T>.get` -- only a receiver takes type parameters before a
        // `.`; after a plain name they are the function's own, read below.
        let written_recv_tparams = if self.peek() == &Tok::Lt && !recv_tparams.is_empty() {
            self.parse_tparams()?
        } else {
            Vec::new()
        };
        debug_assert_eq!(written_recv_tparams, recv_tparams);
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
            recv_tparams,
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
    /// its own type parameters. Returns the receiver's type parameters --
    /// `Wrap<T>.get` -- and the function's own, either of which may be empty.
    fn scan_fn_tparams(&mut self) -> Result<FnHead, Diag> {
        let save = self.pos;
        let mut recv = Vec::new();
        let mut recv_name = None;
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
        if let Tok::Ident(first) = self.peek().clone() {
            let first_span = self.span();
            self.bump();
            self.scan_names(&mut out);
            // A `.` after the list means it was the receiver's.
            if self.eat(&Tok::Dot) {
                recv_name = Some((first, first_span));
                recv = std::mem::take(&mut out);
                if matches!(self.peek(), Tok::Ident(_)) {
                    self.bump();
                    self.scan_names(&mut out);
                }
            }
        }
        self.pos = save;
        Ok(FnHead {
            recv_name,
            recv_tparams: recv,
            tparams: out,
        })
    }

    /// `<A, B>` as a list of names, for `scan_fn_tparams`. Stops at the
    /// first thing that is not one, leaving the real parse to report it.
    fn scan_names(&mut self, out: &mut Vec<String>) {
        if self.peek() != &Tok::Lt {
            return;
        }
        self.bump();
        while let Tok::Ident(n) = self.peek().clone() {
            out.push(n);
            self.bump();
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if self.peek() == &Tok::Gt {
            self.bump();
        }
    }

    /// `const a { .. }` / `const a, b, c { .. }`, having already peeked that
    /// this is the block form and not the declaration.
    pub(super) fn parse_const_block(&mut self, span: Span) -> Result<Stmt, Diag> {
        self.expect(Tok::KwConst)?;
        let mut names = Vec::new();
        loop {
            names.push(self.expect_ident()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let body = self.parse_block()?;
        Ok(Stmt::ConstBlock { names, body, span })
    }

    pub(super) fn parse_block(&mut self) -> Result<Vec<Stmt>, Diag> {
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
}
