//! Type syntax: reading a type, and the type-name tables it consults.

use super::*;

impl Parser {
    /// Is this name usable as a type here -- a declared type, or a type
    /// parameter currently in scope?
    pub(super) fn is_ty_name(&self, name: &str) -> bool {
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
    pub(super) fn qualify(&self, name: &str) -> String {
        if self.module.is_empty()
            || self.builtin.iter().any(|b| b == name)
            || self.tparams.iter().any(|t| t == name)
            || !self.own.iter().any(|o| o == name)
        {
            return name.to_string();
        }
        format!("{}#{name}", self.module)
    }

    pub(super) fn intern(&mut self, name: String, args: Vec<Ty>) -> Ty {
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
    pub(super) fn is_ty_at(&self, n: usize) -> bool {
        match self.peek_at(n) {
            Tok::Ident(name) => self.is_ty_name(name),
            t => Self::ty_of(t).is_some(),
        }
    }

    /// Does a declaration start here? A type keyword always does. A type
    /// NAME does when an identifier follows it, possibly past a `<...>`
    /// argument list -- `Wrap<int> b` is a declaration, `Wrap<int>(..)` is a
    /// construction.
    pub(super) fn decl_starts_here(&self) -> bool {
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
        // `this` counts as a name here only so that `Point this = ..` is read
        // as the declaration it was meant to be and refused by name, rather
        // than as an expression followed by a puzzling stray word.
        matches!(self.peek_at(i), Tok::Ident(_) | Tok::KwThis)
    }

    /// At `lib . Name < ... >`, the offset just past the `>` -- provided
    /// everything between the angle brackets could be a type argument list
    /// and a `(` or `.` follows it, which is what makes it a construction or
    /// a member of a generic type rather than anything else.
    pub(super) fn qualified_targs_end(&self) -> Option<usize> {
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

    pub(super) fn expect_ty(&mut self) -> Result<Ty, Diag> {
        self.nested("type", Self::expect_ty_inner)
    }

    pub(super) fn ty_of(t: &Tok) -> Option<Ty> {
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
                // Type arguments: `Wrap<int>`, `Pair<K, V>`.
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
}
