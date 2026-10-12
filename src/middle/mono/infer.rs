//! Inferring type arguments at a generic call: unification over argument types.

use super::*;

impl Mono {
    /// Infer a generic function's type arguments from its call.
    ///
    /// There is no explicit `f<int>(x)` syntax, on purpose: after a name that
    /// is not known to be a type, `<` is ambiguous with comparison — the
    /// problem that pushed Go to `f[int](x)` and Rust to a turbofish. Every
    /// type parameter must therefore be determined by a parameter position.
    /// Matching is structural over the WRITTEN types (`List<T>` against a
    /// local declared `List<int>` binds `T`), but an argument only has a
    /// written type if it is a literal, a construction or a local -- see
    /// `arg_ty` and `unify_arg`.
    pub(super) fn infer(
        &mut self,
        decl: &Func,
        args: &[Expr],
        sub: &Subst,
        span: Span,
        want: Option<Ty>,
    ) -> Result<Vec<Ty>, Diag> {
        // Only mandatory parameters are positional, so those are what the
        // positional arguments line up with. Arity itself is checked later,
        // by the lowering, against a better error.
        let mandatory: Vec<&Param> = decl.params.iter().filter(|p| !p.is_optional()).collect();
        let mut found: Subst = Subst::new();
        for (p, a) in mandatory.iter().zip(args.iter()) {
            self.unify_arg(p.ty, a, &decl.tparams, sub, span, &mut found);
        }
        // Then the type the value goes to, for whatever the arguments left
        // open. Arguments first: they are the call's own, and when both
        // speak and disagree the lowering reports the mismatch against the
        // destination, which is the clearer message.
        if let Some(w) = want {
            self.unify(decl.ret, w, &decl.tparams, sub, span, &mut found);
        }
        let mut out = Vec::new();
        for tp in &decl.tparams {
            match found.get(tp) {
                Some(t) => out.push(*t),
                None => {
                    return Err(Diag::new(
                        span,
                        format!(
                            "cannot infer type parameter `{tp}` of `{}` from its arguments or \
                             from where its value goes; bind an argument, or the result, to a \
                             local with a written type first",
                            crate::front::ast::bare(&decl.name)
                        ),
                    ))
                }
            }
        }
        Ok(out)
    }

    /// Match a parameter's written type against an argument's written type,
    /// binding any type parameter it reaches. Structural, so `Wrap<T>` against
    /// `Wrap<int>` binds `T`, not only a bare `T`.
    pub(super) fn unify(
        &mut self,
        pty: Ty,
        aty: Ty,
        tparams: &[String],
        sub: &Subst,
        span: Span,
        found: &mut Subst,
    ) {
        let Ty::User(pi) = pty else { return };
        let pe = self.src_exprs[pi as usize].clone();

        if pe.args.is_empty() && tparams.contains(&pe.name) {
            if let Ok(t) = self.subst_ty(aty, sub, span) {
                found.entry(pe.name).or_insert(t);
            }
            return;
        }

        let Ty::User(ai) = aty else { return };
        let ae = self.src_exprs[ai as usize].clone();
        if ae.name != pe.name || ae.args.len() != pe.args.len() {
            return;
        }
        for (p, a) in pe.args.iter().zip(ae.args.iter()) {
            self.unify(*p, *a, tparams, sub, span, found);
        }
    }

    /// The output name of a method call's receiver type, when the receiver
    /// has a written type -- the same few shapes inference reads.
    pub(super) fn recv_name(&mut self, obj: &Expr, sub: &Subst) -> Option<String> {
        // `this` is the method's own receiver, whose output name is known
        // even on a generic type, where the written `Wrap` resolves to
        // nothing.
        if let (Expr::This(_), Some(r)) = (obj, &self.cur_recv) {
            return Some(r.clone());
        }
        let t = self.arg_ty(obj)?;
        let span = obj.span();
        match self.subst_ty(t, sub, span).ok()? {
            Ty::User(i) => Some(self.out_exprs[i as usize].name.clone()),
            _ => None,
        }
    }

    /// Match a parameter's written type against an argument expression.
    ///
    /// A collection literal has no type of its own -- the lowering types it
    /// from its context -- but its ELEMENTS may: `first([1, 2])` against
    /// `List<T>` binds `T` from the `1`. The literal is looked through to the
    /// element the parameter's type argument lines up with, and only when the
    /// parameter is itself written as the matching collection; against a bare
    /// `T` there is nothing to say which collection `[1, 2]` is. An empty
    /// literal says nothing, and leaves the parameter to another argument or
    /// to the diagnostic.
    pub(super) fn unify_arg(
        &mut self,
        pty: Ty,
        a: &Expr,
        tparams: &[String],
        sub: &Subst,
        span: Span,
        found: &mut Subst,
    ) {
        let written = match pty {
            Ty::User(pi) => Some(self.src_exprs[pi as usize].clone()),
            _ => None,
        };
        let seq_elem = written
            .as_ref()
            .filter(|e| matches!(e.name.as_str(), "List" | "Array") && e.args.len() == 1)
            .map(|e| e.args[0]);
        match a {
            Expr::SeqLit(items, _) => {
                if let (Some(elem), Some(first)) = (seq_elem, items.first()) {
                    self.unify_arg(elem, first, tparams, sub, span, found);
                }
            }
            Expr::RepeatLit(fill, _, _) => {
                if let Some(elem) = seq_elem {
                    self.unify_arg(elem, fill, tparams, sub, span, found);
                }
            }
            Expr::MapLit(items, _) => {
                let kv = written
                    .as_ref()
                    .filter(|e| e.name == "Map" && e.args.len() == 2)
                    .map(|e| (e.args[0], e.args[1]));
                if let (Some((k, v)), Some((ka, va))) = (kv, items.first()) {
                    self.unify_arg(k, ka, tparams, sub, span, found);
                    self.unify_arg(v, va, tparams, sub, span, found);
                }
            }
            _ => {
                if let Some(aty) = self.arg_ty(a) {
                    self.unify(pty, aty, tparams, sub, span, found);
                }
            }
        }
    }

    /// The written type of an argument expression, for inference only.
    /// Literals, constructions and locals cover the container cases; anything
    /// else leaves the parameter uninferred and produces a diagnostic rather
    /// than a wrong guess.
    pub(super) fn arg_ty(&mut self, e: &Expr) -> Option<Ty> {
        match e {
            Expr::Int(..) => Some(Ty::Int),
            Expr::Float(..) => Some(Ty::Float),
            Expr::Bool(..) => Some(Ty::Bool),
            Expr::Str(..) => Some(Ty::Str),
            Expr::New(ty, ..) => Some(*ty),
            // `Type.name(..)` is an enum variant or a static method, and
            // only a variant is sure to be of the type written in front.
            Expr::EnumNew(ty, name, ..) => {
                let Ty::User(i) = ty else { return None };
                let d = self.decls.get(&self.src_exprs[*i as usize].name)?;
                d.variants.iter().any(|v| v.name == *name).then_some(*ty)
            }
            // A local first -- nothing may shadow a constant, so the order
            // only matters for speed -- then this module's constant.
            Expr::Var(n, _) => self.env_ty(n).or_else(|| {
                let key = if self.cur_module.is_empty() {
                    n.clone()
                } else {
                    format!("{}#{n}", self.cur_module)
                };
                self.const_tys.get(&key).copied()
            }),
            // `lib.TABLE`, another module's constant.
            Expr::Field(o, n, _) => match &**o {
                Expr::Var(m, _) if self.module_in_scope(m) && self.env_ty(m).is_none() => {
                    self.const_tys.get(&format!("{m}#{n}")).copied()
                }
                _ => None,
            },
            Expr::This(_) => self.recv_ty,
            _ => None,
        }
    }
}
