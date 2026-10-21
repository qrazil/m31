//! Substitution through statements and expressions, and the generic call sites.

use super::*;

impl Mono {
    pub(super) fn subst_block(&mut self, stmts: &[Stmt], sub: &Subst) -> Result<Vec<Stmt>, Diag> {
        self.env.push(HashMap::new());
        let mut out = Vec::new();
        for s in stmts {
            match self.subst_stmt(s, sub) {
                Ok(x) => out.push(x),
                Err(e) => {
                    self.env.pop();
                    return Err(e);
                }
            }
        }
        self.env.pop();
        Ok(out)
    }

    pub(super) fn subst_stmt(&mut self, s: &Stmt, sub: &Subst) -> Result<Stmt, Diag> {
        Ok(match s {
            Stmt::Decl {
                ty,
                name,
                init,
                is_const,
                span,
            } => {
                // Record the type AS WRITTEN, before substitution, so
                // inference can unify against its structure.
                let init = self.subst_expr_as(init, sub, Some(*ty))?;
                self.env
                    .last_mut()
                    .expect("no scope")
                    .insert(name.clone(), *ty);
                Stmt::Decl {
                    ty: self.subst_ty(*ty, sub, *span)?,
                    name: name.clone(),
                    init,
                    is_const: *is_const,
                    span: *span,
                }
            }
            Stmt::Assign { name, value, span } => Stmt::Assign {
                name: name.clone(),
                value: self.subst_expr_as(value, sub, self.env_ty(name))?,
                span: *span,
            },
            Stmt::SetIndex {
                obj,
                index,
                value,
                span,
            } => Stmt::SetIndex {
                obj: self.subst_expr(obj, sub)?,
                index: self.subst_expr(index, sub)?,
                value: self.subst_expr(value, sub)?,
                span: *span,
            },
            Stmt::SetField {
                obj,
                field,
                value,
                span,
            } => Stmt::SetField {
                obj: self.subst_expr(obj, sub)?,
                field: field.clone(),
                value: self.subst_expr(value, sub)?,
                span: *span,
            },
            Stmt::Return { value, span } => Stmt::Return {
                value: match value {
                    Some(e) => Some(self.subst_expr_as(e, sub, self.cur_ret)?),
                    None => None,
                },
                span: *span,
            },
            Stmt::Eval { expr, span } => Stmt::Eval {
                expr: self.subst_expr(expr, sub)?,
                span: *span,
            },
            Stmt::Match {
                scrutinee,
                arms,
                span,
            } => {
                let scrutinee = self.subst_expr(scrutinee, sub)?;
                let mut out = Vec::new();
                for a in arms {
                    // A binding's written type may name a type parameter, so
                    // it substitutes like any other declaration.
                    let mut binds = Vec::new();
                    // A discard has no type to substitute and no name to bind.
                    // The slot stays, though: payloads are positional.
                    for b in &a.binds {
                        binds.push(match b {
                            Some(b) => Some(Param {
                                ty: self.subst_ty(b.ty, sub, b.span)?,
                                name: b.name.clone(),
                                default: None,
                                embedded: false,
                                is_pub: false,
                                is_const: false,
                                span: b.span,
                            }),
                            None => None,
                        });
                    }
                    self.env.push(HashMap::new());
                    for b in a.binds.iter().flatten() {
                        self.env
                            .last_mut()
                            .expect("no scope")
                            .insert(b.name.clone(), b.ty);
                    }
                    let body = self.subst_block(&a.body, sub);
                    self.env.pop();
                    out.push(MatchArm {
                        variant: a.variant.clone(),
                        binds,
                        body: body?,
                        span: a.span,
                    });
                }
                Stmt::Match {
                    scrutinee,
                    arms: out,
                    span: *span,
                }
            }
            Stmt::If {
                cond,
                then,
                els,
                span,
            } => Stmt::If {
                cond: self.subst_expr(cond, sub)?,
                then: self.subst_block(then, sub)?,
                els: match els {
                    Some(e) => Some(self.subst_block(e, sub)?),
                    None => None,
                },
                span: *span,
            },
            Stmt::ForRange {
                ty,
                name,
                from,
                to,
                body,
                span,
            } => Stmt::ForRange {
                ty: self.subst_ty(*ty, sub, *span)?,
                name: name.clone(),
                from: self.subst_expr(from, sub)?,
                to: self.subst_expr(to, sub)?,
                body: self.subst_block(body, sub)?,
                span: *span,
            },
            Stmt::ForIn {
                ty,
                name,
                iter,
                body,
                span,
            } => Stmt::ForIn {
                ty: self.subst_ty(*ty, sub, *span)?,
                name: name.clone(),
                iter: self.subst_expr(iter, sub)?,
                body: self.subst_block(body, sub)?,
                span: *span,
            },
            Stmt::While { cond, body, span } => Stmt::While {
                cond: self.subst_expr(cond, sub)?,
                body: self.subst_block(body, sub)?,
                span: *span,
            },
            Stmt::Spawn { name, args, span } => {
                let out = self.subst_args(args, sub)?;
                let name = &self.resolve_fn(name);
                if let Some(decl) = self.generic_funcs.get(name).cloned() {
                    // From the arguments AS WRITTEN -- see `generic_call`.
                    let targs = self.infer(&decl, &args.pos, sub, *span, None)?;
                    let mangled = self.mangle(name, &targs);
                    self.queue.push((name.clone(), targs, *span));
                    return Ok(Stmt::Spawn {
                        name: mangled,
                        args: out,
                        span: *span,
                    });
                }
                Stmt::Spawn {
                    name: name.clone(),
                    args: out,
                    span: *span,
                }
            }
            Stmt::Break { span } => Stmt::Break { span: *span },
            Stmt::Continue { span } => Stmt::Continue { span: *span },
            Stmt::ConstBlock { names, body, span } => Stmt::ConstBlock {
                names: names.clone(),
                body: self.subst_block(body, sub)?,
                span: *span,
            },
        })
    }

    pub(super) fn subst_expr(&mut self, e: &Expr, sub: &Subst) -> Result<Expr, Diag> {
        self.subst_expr_as(e, sub, None)
    }

    /// Substitute an expression whose value goes somewhere with a written
    /// type -- a declaration, an assignment, a return, a parameter. That
    /// type is `want`, and it is what lets a generic call whose type
    /// parameter appears only in its return type be inferred:
    /// `Result<int, str> r = fail("no");` for `Result<T, str> fail<T>(..)`.
    /// The same places the lowering types a collection literal from
    /// (`lower_expr_as`), for the same reason: the value has no type of its
    /// own to offer, and its destination does.
    pub(super) fn subst_expr_as(
        &mut self,
        e: &Expr,
        sub: &Subst,
        want: Option<Ty>,
    ) -> Result<Expr, Diag> {
        Ok(match e {
            Expr::Int(..) | Expr::Float(..) | Expr::Bool(..) | Expr::Str(..) | Expr::This(..) => {
                e.clone()
            }
            // A generic function's name, not being called. A function's name
            // is a value where a one-method interface is expected
            // (docs/closures-decision.md), but a generic one has no single
            // code address to put in a vtable slot -- an interface cannot
            // declare a generic method for the same reason -- and nothing
            // here says which instantiation was meant. Said here because a
            // generic declaration is instantiated away before the lowering
            // sees it, which would otherwise report an unknown variable.
            //
            // A local or a parameter of the same name is that local, not the
            // function: nothing in `sigs` holds an uninstantiated generic, so
            // the no-shadowing check never refused the binding and a program
            // that does this compiles today.
            Expr::Var(n, s)
                if self.env_ty(n).is_none()
                    && self.generic_funcs.contains_key(&self.resolve_fn(n)) =>
            {
                return Err(Diag::new(
                    *s,
                    format!(
                        "`{}` is a generic function, so it cannot be a callback: a vtable \
                         slot holds one code address and a generic function has one per \
                         instantiation. Wrap the instantiation you want in a function of \
                         its own and pass that",
                        crate::front::ast::bare(n)
                    ),
                ))
            }
            Expr::Var(..) => e.clone(),
            Expr::Bin(op, l, r, s) => Expr::Bin(
                *op,
                Box::new(self.subst_expr(l, sub)?),
                Box::new(self.subst_expr(r, sub)?),
                *s,
            ),
            Expr::Un(op, x, s) => Expr::Un(*op, Box::new(self.subst_expr(x, sub)?), *s),
            Expr::Field(o, f, s) => Expr::Field(Box::new(self.subst_expr(o, sub)?), f.clone(), *s),
            Expr::Index(o, i, s) => Expr::Index(
                Box::new(self.subst_expr(o, sub)?),
                Box::new(self.subst_expr(i, sub)?),
                *s,
            ),
            Expr::New(ty, args, s) => {
                let ty = self.subst_ty(*ty, sub, *s)?;
                Expr::New(ty, self.subst_args(args, sub)?, *s)
            }
            Expr::Try(e, s) => Expr::Try(Box::new(self.subst_expr(e, sub)?), *s),
            // A lambda written inside a generic function is instantiated
            // with it: its written parameter types may name the function's
            // type parameters, and its body is ordinary code. The lowering
            // then synthesises one type per instantiation, because that is
            // what this pass has made -- two separate lambdas, each already
            // concrete. No generic machinery reaches the synthesis.
            //
            // The body is substituted with no `want`: what it must produce
            // comes from the target interface, which only the lowering
            // knows. Information flows target -> callback, never back.
            Expr::Lambda(ps, body, s) => {
                let mut out = Vec::new();
                for p in ps {
                    out.push(Param {
                        ty: self.subst_ty(p.ty, sub, p.span)?,
                        ..p.clone()
                    });
                }
                Expr::Lambda(out, Box::new(self.subst_expr(body, sub)?), *s)
            }
            Expr::SeqLit(items, s) => {
                let mut out = Vec::new();
                for e in items {
                    out.push(self.subst_expr(e, sub)?);
                }
                Expr::SeqLit(out, *s)
            }
            Expr::RepeatLit(v, n, s) => Expr::RepeatLit(
                Box::new(self.subst_expr(v, sub)?),
                Box::new(self.subst_expr(n, sub)?),
                *s,
            ),
            Expr::MapLit(items, s) => {
                let mut out = Vec::new();
                for (k, v) in items {
                    out.push((self.subst_expr(k, sub)?, self.subst_expr(v, sub)?));
                }
                Expr::MapLit(out, *s)
            }
            Expr::EnumNew(ty, variant, args, s) => {
                // `lib.TABLE.size()`: the parser read `lib.TABLE` as a type,
                // because it cannot see another module's constants. This
                // pass can, so the tree is put back the way it was meant --
                // a method call on the constant `lib.TABLE` -- before
                // anything tries to resolve `TABLE` as a type.
                if let Ty::User(i) = ty {
                    let e = &self.src_exprs[*i as usize];
                    if e.args.is_empty() && self.const_tys.contains_key(&e.name) {
                        if let Some((m, n)) = e.name.split_once('#') {
                            let recv = Expr::Field(
                                Box::new(Expr::Var(m.to_string(), *s)),
                                n.to_string(),
                                *s,
                            );
                            let call =
                                Expr::MethodCall(Box::new(recv), variant.clone(), args.clone(), *s);
                            return self.subst_expr_as(&call, sub, want);
                        }
                    }
                }
                let ty = self.subst_ty(*ty, sub, *s)?;
                // `Wrap.make(1)` for `static T Wrap.make<T>(T v)`: a
                // static method with type parameters of its own, whose
                // receiver is the type written in front.
                let mut member = variant.clone();
                if let Ty::User(i) = ty {
                    let recv = self.out_exprs[i as usize].name.clone();
                    let found = self
                        .own_generic
                        .get(&recv)
                        .and_then(|ms| ms.get(variant))
                        .cloned();
                    if let Some((decl, rsub)) = found {
                        member = self
                            .generic_method_call(decl, rsub, recv, variant, args, sub, *s, want)?;
                    }
                }
                Expr::EnumNew(ty, member, self.subst_args(args, sub)?, *s)
            }
            Expr::MethodCall(obj, m, args, s) => {
                // `lib.first(xs)` is a call, not a method: the parser cannot
                // tell a module qualifier from a receiver, so it reaches here
                // as a method call on a variable named `lib`. The lowering
                // settles it the same way -- a program's module, and no local
                // of that name in scope -- and this has to agree, or a generic
                // function reached through its module is never instantiated
                // and the lowering finds nothing called `first`.
                if let Expr::Var(modname, _) = &**obj {
                    let key = format!("{modname}#{m}");
                    if self.module_in_scope(modname)
                        && self.env_ty(modname).is_none()
                        && self.generic_funcs.contains_key(&key)
                    {
                        let mangled = self.generic_call(&key, args, sub, *s, want)?;
                        // Keep the qualified shape so the lowering still
                        // checks the call against `lib`'s privacy; it builds
                        // `lib#first$int` from it, which is the
                        // instantiation's name.
                        let bare = crate::front::ast::bare(&mangled).to_string();
                        return Ok(Expr::MethodCall(
                            obj.clone(),
                            bare,
                            self.subst_args(args, sub)?,
                            *s,
                        ));
                    }
                }
                // `lib.f(..)` to a non-generic function: its arguments have
                // parameters to be inferred against, like a bare call's.
                if let Expr::Var(modname, _) = &**obj {
                    let key = format!("{modname}#{m}");
                    if self.module_in_scope(modname) && self.env_ty(modname).is_none() {
                        if let Some(params) = self.func_params.get(&key).cloned() {
                            return Ok(Expr::MethodCall(
                                obj.clone(),
                                m.clone(),
                                self.subst_args_as(args, sub, &params)?,
                                *s,
                            ));
                        }
                    }
                }
                // A method with type parameters of its own is instantiated
                // like a generic function, once the receiver's type is known.
                // Only a receiver whose type is written down can say -- see
                // `recv_name`; any other reaches the lowering as it is, and
                // `Program::generic_methods` lets that explain itself.
                if let Some(recv) = self.recv_name(obj, sub) {
                    let found = self
                        .own_generic
                        .get(&recv)
                        .and_then(|ms| ms.get(m))
                        .cloned();
                    if let Some((decl, rsub)) = found {
                        let name =
                            self.generic_method_call(decl, rsub, recv, m, args, sub, *s, want)?;
                        return Ok(Expr::MethodCall(
                            Box::new(self.subst_expr(obj, sub)?),
                            name,
                            self.subst_args(args, sub)?,
                            *s,
                        ));
                    }
                }
                Expr::MethodCall(
                    Box::new(self.subst_expr(obj, sub)?),
                    m.clone(),
                    self.subst_args(args, sub)?,
                    *s,
                )
            }
            Expr::Call(written, args, s) => {
                // A call to a generic function needs its type arguments
                // inferred from the argument types, then the instantiation
                // queued and the name rewritten to the mangled one. Inference
                // is deliberately shallow -- see `infer`.
                let name = &self.resolve_fn(written);
                // Inside an instance method a bare call may name a sibling
                // method (the lowering's `sibling_call`), and one with type
                // parameters of its own is instantiated here like any other
                // generic method: the receiver is this method's own, so its
                // type is known even though nothing spells it. The call keeps
                // its bare shape, renamed to the instantiation; the lowering
                // recognises `pick$int` as the receiver's method from there.
                if let Some(recv) = self.cur_recv.clone() {
                    let found = self
                        .own_generic
                        .get(&recv)
                        .and_then(|ms| ms.get(written))
                        .filter(|(decl, _)| !decl.is_static)
                        .cloned();
                    if let Some((decl, rsub)) = found {
                        // A generic function of the same name would make the
                        // bare call ambiguous. The lowering refuses that for
                        // every other pairing, but once both are renamed to
                        // their instantiations it can no longer see this one.
                        if self.generic_funcs.contains_key(name) {
                            return Err(Diag::new(
                                *s,
                                format!(
                                    "`{written}` is both a method of `{}` and a function, \
                                     so a bare `{written}(..)` here could mean either; \
                                     rename one",
                                    self.show_out(&recv)
                                ),
                            ));
                        }
                        let out = self.subst_args(args, sub)?;
                        let inst = self
                            .generic_method_call(decl, rsub, recv, written, args, sub, *s, want)?;
                        return Ok(Expr::Call(inst, out, *s));
                    }
                }
                if self.generic_funcs.contains_key(name) {
                    let out = self.subst_args(args, sub)?;
                    let mangled = self.generic_call(name, args, sub, *s, want)?;
                    return Ok(Expr::Call(mangled, out, *s));
                }
                // A plain function: its parameters are context for its
                // arguments. The name stays as written; the lowering
                // resolves it.
                let own = format!("{}#{written}", self.cur_module);
                let params = self.func_params.get(&own).cloned();
                let out = match params {
                    Some(params) => self.subst_args_as(args, sub, &params)?,
                    None => self.subst_args(args, sub)?,
                };
                Expr::Call(written.clone(), out, *s)
            }
        })
    }

    /// The same as `generic_call`, for a method with type parameters of its
    /// own at a known receiver: infer, queue, and return the method's
    /// instantiated name.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn generic_method_call(
        &mut self,
        decl: Func,
        rsub: Subst,
        recv: String,
        m: &str,
        args: &Args,
        sub: &Subst,
        span: Span,
        want: Option<Ty>,
    ) -> Result<String, Diag> {
        let targs = self.infer(&decl, &args.pos, sub, span, want)?;
        let name = self.mangle(m, &targs);
        self.shown
            .insert(name.clone(), (m.to_string(), targs.clone()));
        let mut full = rsub;
        full.extend(decl.tparams.iter().cloned().zip(targs));
        self.method_queue.push((decl, full, recv, name.clone()));
        Ok(name)
    }

    /// Infer a generic function's type arguments at one call, queue that
    /// instantiation, and return its mangled name.
    pub(super) fn generic_call(
        &mut self,
        name: &str,
        args: &Args,
        sub: &Subst,
        span: Span,
        want: Option<Ty>,
    ) -> Result<String, Diag> {
        let decl = self.generic_funcs.get(name).cloned().expect("generic");
        // Infer from the arguments AS WRITTEN, not from their substituted
        // form. There are two type arenas -- `src_exprs` for the input
        // program and `out_exprs` for what substitution produces -- and
        // `unify` reads `src_exprs`. A substituted `Expr::New` carries an
        // `out_exprs` index, so unifying against it indexed the wrong arena
        // and inference failed for a constructed temporary while succeeding
        // for a local. `unify` substitutes what it binds, which is what
        // `sub` is threaded through for.
        let targs = self.infer(&decl, &args.pos, sub, span, want)?;
        let mangled = self.mangle(name, &targs);
        self.queue.push((name.to_string(), targs, span));
        Ok(mangled)
    }

    /// Arguments to a function whose parameters are known and concrete:
    /// each is substituted with its parameter's written type as context.
    /// Positional ones line up with the mandatory parameters, named ones
    /// with the parameter of that name.
    pub(super) fn subst_args_as(
        &mut self,
        a: &Args,
        sub: &Subst,
        params: &[Param],
    ) -> Result<Args, Diag> {
        let mandatory: Vec<Ty> = params
            .iter()
            .filter(|p| !p.is_optional())
            .map(|p| p.ty)
            .collect();
        let mut pos = Vec::new();
        for (i, e) in a.pos.iter().enumerate() {
            pos.push(self.subst_expr_as(e, sub, mandatory.get(i).copied())?);
        }
        let mut named = Vec::new();
        for (n, e) in &a.named {
            let want = params.iter().find(|p| p.name == *n).map(|p| p.ty);
            named.push((n.clone(), self.subst_expr_as(e, sub, want)?));
        }
        Ok(Args { pos, named })
    }

    pub(super) fn subst_args(&mut self, a: &Args, sub: &Subst) -> Result<Args, Diag> {
        let mut pos = Vec::new();
        for e in &a.pos {
            pos.push(self.subst_expr(e, sub)?);
        }
        let mut named = Vec::new();
        for (n, e) in &a.named {
            named.push((n.clone(), self.subst_expr(e, sub)?));
        }
        Ok(Args { pos, named })
    }
}
