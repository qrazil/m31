//! Enum construction and the `?` operator.

use super::*;

impl Lowerer {
    pub(super) fn lower_enum_new(
        &mut self,
        ty: Ty,
        variant: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if matches!(ty, Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Void) {
            return self.lower_prim_static(ty, variant, args, span);
        }
        let Some(tid) = self.tdef_of(ty) else {
            return Err(Diag::new(
                span,
                format!("unknown type `{}`", self.tyname(ty)),
            ));
        };
        // `Type.name(..)` is two things wearing one spelling: an enum
        // variant, and a call to a static method. The variant wins when
        // there is one, because a type cannot have a variant and a static
        // method of the same name -- that is refused where methods are
        // registered.
        self.refuse_destructor_call(tid, variant, span)?;
        let key = format!("{}.{variant}", self.typedefs[tid as usize].name);
        let is_variant = self.typedefs[tid as usize]
            .variants
            .iter()
            .any(|v| v.name == variant);
        if !is_variant && self.statics.contains(&key) {
            return self.lower_static_call(&key, args, span);
        }
        if !self.typedefs[tid as usize].is_enum {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` has no static method `{variant}`, and is not an enum",
                    self.tyname(ty)
                ),
            ));
        }
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "its variants cannot be named from here"),
            ));
        }
        if !args.named.is_empty() {
            return Err(Diag::new(
                span,
                "a variant's payload is positional; it has no field names",
            ));
        }
        let Some(tag) = self.typedefs[tid as usize]
            .variants
            .iter()
            .position(|v| v.name == variant)
        else {
            let known: Vec<&str> = self.typedefs[tid as usize]
                .variants
                .iter()
                .map(|v| v.name.as_str())
                .collect();
            return Err(Diag::new(
                span,
                format!(
                    "`{}` has no variant `{variant}`; it has {}",
                    self.tyname(ty),
                    known.join(", ")
                ),
            ));
        };

        let want = self.variant_surface[tid as usize][tag].clone();
        if args.pos.len() != want.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{variant}` carries {} value(s), found {}",
                    want.len(),
                    args.pos.len()
                ),
            ));
        }

        let mut vals = Vec::new();
        for (a, w) in args.pos.iter().zip(want.iter()) {
            let v = self.lower_expr_as(a, *w)?;
            if !self.assignable(v.ty, *w) {
                return Err(Diag::new(a.span(), self.mismatch(*w, v.ty)));
            }
            // The enum holds the payload, exactly as a field would: an owned
            // temporary is handed over, a borrowed value is retained.
            if self.is_ref(*w) {
                if v.owned {
                    self.stmt_temps.retain(|t| *t != v.val());
                } else {
                    self.rc_inc(v.val());
                }
            }
            vals.push(v.val());
        }

        let d = self.enum_val(tid);
        self.push(Inst::EnumPack {
            dst: d,
            tid,
            tag: tag as u32,
            args: vals,
        });
        self.stmt_temps.push(d);
        Ok(Val::new(d, ty, true))
    }

    /// `e?` -- give me the value, or return the failure from here.
    ///
    /// Sugar for a `match` whose failing arm returns unchanged:
    ///
    ///     match (e) {
    ///         case Ok(T v):   { v }
    ///         case Err(E x):  { return Result<_, E>.Err(x); }
    ///     }
    ///
    /// It works on an `Option` in a function returning an `Option` too.
    ///
    /// The error types must match EXACTLY. Rust converts via `From`; we have
    /// no such mechanism and inventing one here would be a large feature
    /// hiding inside a small one. Requiring a match is restrictive and
    /// honest, and relaxing it later cannot change what an existing program
    /// means.
    pub(super) fn lower_try(&mut self, inner: &Expr, span: Span) -> Result<Val, Diag> {
        let v = self.lower_expr(inner)?;

        let Some(vtid) = self
            .tdef_of(v.ty)
            .filter(|t| self.typedefs[*t as usize].is_enum)
        else {
            return Err(Diag::new(
                span,
                format!(
                    "`?` needs an Option or a Result; {} is neither",
                    self.tyname(v.ty)
                ),
            ));
        };
        let vname = self.typedefs[vtid as usize].name.clone();
        let is_result = vname.starts_with("Result$");
        if !is_result && !vname.starts_with("Option$") {
            return Err(Diag::new(
                span,
                format!(
                    "`?` needs an Option or a Result; {} is neither",
                    self.show_name(&vname)
                ),
            ));
        }

        // The enclosing function has to be able to carry the failure out.
        let ret = self.ret_ty;
        let Some(rtid) = self
            .tdef_of(ret)
            .filter(|t| self.typedefs[*t as usize].is_enum)
        else {
            return Err(Diag::new(
                span,
                format!(
                    "`?` can only be used in a function returning an Option or a \
                     Result; this one returns {}",
                    self.tyname(ret)
                ),
            ));
        };
        let rname = self.typedefs[rtid as usize].name.clone();
        if is_result != rname.starts_with("Result$") {
            return Err(Diag::new(
                span,
                format!(
                    "`?` on {} needs a function returning a Result, not {}",
                    a_or_an(&self.show_name(&vname)),
                    self.show_name(&rname)
                ),
            ));
        }

        // Exact error types, for a Result.
        if is_result {
            // A `void` payload has been dropped (mono/), so an empty one
            // is `void`.
            let err_of = |lw: &Self, t: u32| {
                lw.variant_surface[t as usize][1]
                    .first()
                    .copied()
                    .unwrap_or(Ty::Void)
            };
            let (ve, re) = (err_of(self, vtid), err_of(self, rtid));
            if ve != re {
                return Err(Diag::new(
                    span,
                    format!(
                        "`?` needs the same error type on both sides: this fails with \
                         {}, and the function returns {}",
                        self.tyname(ve),
                        self.tyname(re)
                    ),
                ));
            }
        }

        let ok_tag = self.typedefs[vtid as usize]
            .variants
            .iter()
            .position(|x| x.name == "Ok" || x.name == "Some")
            .expect("Option and Result each have a success variant") as u32;
        // `None` for `Result<void, E>`: the success carries nothing, and `e?`
        // is then a statement rather than a value.
        let payload = self.variant_surface[vtid as usize][ok_tag as usize]
            .first()
            .copied();

        // The scrutinee has to outlive both paths and may be a temporary, so
        // it is held the way `match` holds one.
        self.synth += 1;
        let hold = format!("$try{}", self.synth);
        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());
        if v.owned {
            self.stmt_temps.retain(|t| *t != v.val());
        } else {
            self.rc_inc(v.val());
        }
        self.scopes
            .last_mut()
            .unwrap()
            .insert(hold.clone(), (v.ty, v.val(), true));
        self.owned.last_mut().unwrap().push(hold.clone());

        let tag = self.new_val(IrTy::I64);
        self.push(Inst::EnumTag {
            dst: tag,
            obj: v.val(),
            tid: vtid,
        });
        let k = self.new_val(IrTy::I64);
        self.push(Inst::IConst {
            dst: k,
            val: ok_tag as i64,
        });
        let good = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: good,
            cmp: Cmp::Eq,
            lhs: tag,
            rhs: k,
        });

        let ok_bb = self.new_block();
        let bad_bb = self.new_block();
        self.terminate(Term::Brif {
            cond: good,
            then: ok_bb,
            then_args: Vec::new(),
            els: bad_bb,
            els_args: Vec::new(),
        });

        // The failing path: rebuild the failure at the function's own return
        // type and leave. A Result and an Option differ only in whether
        // there is a payload to carry.
        self.switch_to(bad_bb);
        let fail_tag = self.typedefs[rtid as usize]
            .variants
            .iter()
            .position(|x| x.name == "Err" || x.name == "None")
            .expect("Option and Result each have a failure variant") as u32;
        let err_ty = self.variant_surface[vtid as usize][1].first().copied();
        let carried = if let Some(err_ty) = err_ty.filter(|_| is_result) {
            let e = self.new_val(self.irty(err_ty));
            // The tag is the scrutinee's OWN failure variant, not the
            // enclosing function's: this reads the value that arrived.
            let carried_tag = self.typedefs[vtid as usize]
                .variants
                .iter()
                .position(|x| x.name == "Err")
                .expect("a Result has an Err") as u32;
            self.push(Inst::EnumPayload {
                dst: e,
                obj: v.val(),
                tid: vtid,
                tag: carried_tag,
                idx: 0,
            });
            // Borrowed from the value we are about to release, so the new
            // failure takes a reference of its own.
            if self.is_ref(err_ty) {
                self.rc_inc(e);
            }
            Some(e)
        } else {
            None
        };
        let out = self.make_option(rtid, fail_tag, carried);
        self.stmt_temps.retain(|t| *t != out);
        // The early return has to release what the statement has allocated
        // so far -- but only on THIS path. `flush_temps` empties the pending
        // list, and the success path continues in the same statement and
        // still needs it: without restoring it, every temporary created
        // before the `?` leaked whenever the value was Ok.
        let pending = self.stmt_temps.clone();
        self.flush_temps();
        self.release_all();
        self.restore_freezes_to(0);
        self.terminate(Term::Ret { val: Some(out) });
        self.stmt_temps = pending;

        // The succeeding path: the payload, retained because it is borrowed
        // from a value whose scope ends here.
        self.switch_to(ok_bb);
        let Some(payload) = payload else {
            self.release_scope();
            self.scopes.pop();
            self.owned.pop();
            return Ok(Val::void());
        };
        let got = self.new_val(self.irty(payload));
        self.push(Inst::EnumPayload {
            dst: got,
            obj: v.val(),
            tid: vtid,
            tag: ok_tag,
            idx: 0,
        });
        let owned = self.is_ref(payload);
        if owned {
            self.rc_inc(got);
        }
        self.release_scope();
        self.scopes.pop();
        self.owned.pop();
        if owned {
            self.stmt_temps.push(got);
        }
        Ok(Val::new(got, payload, owned))
    }
}
