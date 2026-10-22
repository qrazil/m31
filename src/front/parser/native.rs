//! `prim "c"`: the parameters of a call into assembly or a C-ABI function.
//!
//! Everything here is about the WRITTEN declaration (docs/ffi-decision.md):
//! which parameter is a span, what its bounds are, which integers the callee
//! may be handed. The call site and the emitter read the result and need not
//! parse anything again.

use super::*;

/// The most arguments a callee receives -- the six integer argument
/// registers of System V AMD64, the portable subset of what AAPCS64 and RV64
/// offer. No argument is ever passed on the stack.
pub(crate) const MAX_NATIVE_ARGS: usize = 6;

/// A span bound as written, names not yet resolved to parameters.
#[derive(Debug)]
pub(super) enum RawExpr {
    Lit(i64),
    Name(String, Span),
    Add(Box<RawExpr>, Box<RawExpr>),
    Mul(Box<RawExpr>, Box<RawExpr>),
}

/// One parameter as written.
pub(super) enum RawNative {
    Scalar {
        allowed: Vec<i64>,
    },
    Range,
    Span {
        mode: SpanMode,
        lo: RawExpr,
        hi: RawExpr,
    },
}

impl Parser {
    /// `[in|out|inout] bytes name[LO .. HI]`, `[range] int name`,
    /// `int name in A | B`. The `Param` is what the rest of the compiler
    /// sees, so a span is simply a `bytes` or `Array<int>` argument there.
    pub(super) fn parse_native_param(&mut self) -> Result<(Param, RawNative), Diag> {
        let span = self.span();
        let mode = match self.peek().clone() {
            Tok::KwIn => Some(SpanMode::In),
            Tok::Ident(w) if w == "out" && self.is_ty_at(1) => Some(SpanMode::Out),
            Tok::Ident(w) if w == "inout" && self.is_ty_at(1) => Some(SpanMode::InOut),
            _ => None,
        };
        if mode.is_some() {
            self.bump();
        }
        let range = mode.is_none()
            && matches!(self.peek(), Tok::Ident(w) if w == "range")
            && self.is_ty_at(1);
        if range {
            self.bump();
        }
        let ty_span = self.span();
        let ty = self.expect_ty()?;
        let (name, _) = self.expect_ident()?;
        let param = Param {
            ty,
            name: name.clone(),
            default: None,
            embedded: false,
            is_pub: false,
            is_const: false,
            span,
        };
        let is_int = ty == Ty::Int;
        let is_array_int = matches!(ty, Ty::User(i)
            if self.ty_exprs[i as usize].name == "Array"
                && self.ty_exprs[i as usize].args == [Ty::Int]);
        match mode {
            Some(mode) => {
                if ty != Ty::Bytes && !is_array_int {
                    return Err(Diag::new(
                        ty_span,
                        format!(
                            "a `{}` span is a `bytes` or an `Array<int>`: `{name}` is neither",
                            mode.word()
                        ),
                    ));
                }
                self.expect(Tok::LBracket)?;
                let lo = self.parse_raw_expr()?;
                self.expect(Tok::DotDot)?;
                let hi = self.parse_raw_expr()?;
                self.expect(Tok::RBracket)?;
                Ok((param, RawNative::Span { mode, lo, hi }))
            }
            None => {
                if !is_int {
                    let why = if ty == Ty::Bytes || is_array_int {
                        format!(
                            "a `bytes` or `Array<int>` parameter of a `prim \"c\"` is a span: \
                             write `in bytes {name}[LO .. HI]` (or `out`, `inout`)"
                        )
                    } else {
                        "a parameter of a `prim \"c\"` is an `int` or a span (`in bytes name[LO .. HI]`)"
                            .to_string()
                    };
                    return Err(Diag::new(ty_span, why));
                }
                if range {
                    return Ok((param, RawNative::Range));
                }
                let mut allowed = Vec::new();
                if self.eat(&Tok::KwIn) {
                    loop {
                        let at = self.span();
                        match self.peek().clone() {
                            Tok::Int(n) => {
                                self.bump();
                                allowed.push(n);
                            }
                            t => {
                                return Err(Diag::new(
                                    at,
                                    format!(
                                    "expected an integer literal in the list of values `{name}` \
                                         may take, found {}",
                                    t.describe()
                                ),
                                ))
                            }
                        }
                        if !self.eat(&Tok::Pipe) {
                            break;
                        }
                    }
                }
                Ok((param, RawNative::Scalar { allowed }))
            }
        }
    }

    fn parse_raw_expr(&mut self) -> Result<RawExpr, Diag> {
        let mut lhs = self.parse_raw_term()?;
        while self.eat(&Tok::Plus) {
            let rhs = self.parse_raw_term()?;
            lhs = RawExpr::Add(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_raw_term(&mut self) -> Result<RawExpr, Diag> {
        let mut lhs = self.parse_raw_factor()?;
        while self.eat(&Tok::Star) {
            let rhs = self.parse_raw_factor()?;
            lhs = RawExpr::Mul(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_raw_factor(&mut self) -> Result<RawExpr, Diag> {
        let at = self.span();
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(RawExpr::Lit(n))
            }
            Tok::Ident(n) => {
                self.bump();
                Ok(RawExpr::Name(n, at))
            }
            Tok::LParen => {
                self.bump();
                let e = self.parse_raw_expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            t => Err(Diag::new(
                at,
                format!(
                    "a span bound is made of integer literals, the `int` parameters of this \
                     declaration, `+`, `*` and parentheses; found {}",
                    t.describe()
                ),
            )),
        }
    }

    /// The checks a `prim "c"` declaration has to pass as a whole, and the
    /// resolved form the lowerer and the emitter read.
    pub(super) fn check_native(
        &self,
        span: Span,
        name: &str,
        ret: Ty,
        nonblocking: bool,
        params: &[Param],
        raw: Vec<RawNative>,
    ) -> Result<NativeDecl, Diag> {
        let bare_name = bare(name);
        if !bare_name.starts_with("__k_") {
            return Err(Diag::new(
                span,
                format!(
                    "a `prim \"c\"` is named `__k_...` (`{bare_name}` is not): the callee is \
                     `rt_k_...`, a function written for this convention, so it can never be \
                     one that expects an object"
                ),
            ));
        }
        if ret != Ty::Int && ret != Ty::Void {
            return Err(Diag::new(
                span,
                "a `prim \"c\"` returns `int` (the callee's result register) or `void`",
            ));
        }
        let passed = raw
            .iter()
            .filter(|r| !matches!(r, RawNative::Range))
            .count();
        if passed > MAX_NATIVE_ARGS {
            return Err(Diag::new(
                span,
                format!(
                    "a `prim \"c\"` passes at most {MAX_NATIVE_ARGS} arguments to the callee \
                     (`{bare_name}` passes {passed}); `range` parameters do not count, and a \
                     kernel that wants more takes a state buffer as a span"
                ),
            ));
        }
        // Names a bound may mention: the `int` parameters, scalar or range.
        let is_int_param: Vec<bool> = raw
            .iter()
            .map(|r| matches!(r, RawNative::Scalar { .. } | RawNative::Range))
            .collect();
        let resolve = |e: &RawExpr| -> Result<SpanExpr, Diag> {
            fn go(e: &RawExpr, params: &[Param], is_int: &[bool]) -> Result<SpanExpr, Diag> {
                match e {
                    RawExpr::Lit(n) => Ok(SpanExpr::Lit(*n)),
                    RawExpr::Name(n, at) => match params.iter().position(|p| p.name == *n) {
                        Some(i) if is_int[i] => Ok(SpanExpr::Param(i)),
                        Some(_) => Err(Diag::new(
                            *at,
                            format!("`{n}` is a span; a span bound is made of `int` parameters"),
                        )),
                        None => Err(Diag::new(
                            *at,
                            format!("`{n}` is not a parameter of this declaration"),
                        )),
                    },
                    RawExpr::Add(a, b) => Ok(SpanExpr::Add(
                        Box::new(go(a, params, is_int)?),
                        Box::new(go(b, params, is_int)?),
                    )),
                    RawExpr::Mul(a, b) => Ok(SpanExpr::Mul(
                        Box::new(go(a, params, is_int)?),
                        Box::new(go(b, params, is_int)?),
                    )),
                }
            }
            go(e, params, &is_int_param)
        };
        let mut out = Vec::new();
        for r in &raw {
            out.push(match r {
                RawNative::Scalar { allowed } => NativeParam::Scalar {
                    allowed: allowed.clone(),
                },
                RawNative::Range => NativeParam::Range,
                RawNative::Span { mode, lo, hi, .. } => NativeParam::Span {
                    mode: *mode,
                    lo: resolve(lo)?,
                    hi: resolve(hi)?,
                },
            });
        }
        Ok(NativeDecl {
            nonblocking,
            params: out,
        })
    }
}
