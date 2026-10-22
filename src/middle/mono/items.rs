//! Instantiating generic functions and methods.

use super::*;

impl Mono {
    pub(super) fn instantiate_func(
        &mut self,
        name: &str,
        args: &[Ty],
        span: Span,
    ) -> Result<(), Diag> {
        let mangled = self.mangle(name, args);
        self.shown
            .insert(mangled.clone(), (name.to_string(), args.to_vec()));
        if !self.done.insert(mangled.clone()) {
            return Ok(());
        }
        let decl = self
            .generic_funcs
            .get(name)
            .cloned()
            .expect("queued a non-generic");
        if args.len() != decl.tparams.len() {
            return Err(Diag::new(
                span,
                format!(
                    "function `{}` takes {} type argument(s), found {}",
                    crate::front::ast::bare(name),
                    decl.tparams.len(),
                    args.len()
                ),
            ));
        }
        let sub: Subst = decl
            .tparams
            .iter()
            .cloned()
            .zip(args.iter().copied())
            .collect();
        let mut f = self.subst_func(&decl, &sub, None)?;
        f.name = mangled;
        f.tparams = Vec::new();
        self.out_funcs.push(f);
        Ok(())
    }

    /// Emit one method at one receiver: `Wrap<T>.get` at `Wrap$int`, or
    /// `Wrap<T>.map<U>` at `Wrap$int` as `map$str`.
    pub(super) fn instantiate_method(
        &mut self,
        decl: &Func,
        sub: &Subst,
        recv: &str,
        name: &str,
    ) -> Result<(), Diag> {
        if !self.done.insert(format!("{recv}.{name}")) {
            return Ok(());
        }
        let this = (!decl.is_static).then(|| recv.to_string());
        let mut f = self.subst_func(decl, sub, this)?;
        f.recv = Some(recv.to_string());
        f.name = name.to_string();
        f.tparams = Vec::new();
        f.recv_tparams = Vec::new();
        self.out_funcs.push(f);
        Ok(())
    }

    /// `T Wrap<T>.get()` must name a generic type of its own module, with as
    /// many parameters as the type has. Checked at the declaration, because
    /// a method on a type nothing instantiates is otherwise never looked at.
    pub(super) fn check_generic_recv(&self, f: &Func) -> Result<(), Diag> {
        let r = f
            .recv
            .as_deref()
            .expect("receiver type parameters need a receiver");
        let shown = crate::front::ast::bare(r);
        let Some(d) = self.decls.get(r) else {
            let suffix = format!("#{r}");
            if let Some(d) = self.decls.values().find(|d| d.name.ends_with(&suffix)) {
                return Err(Diag::new(
                    f.span,
                    format!(
                        "`{shown}` is declared in `{}`; a method may only be added \
                         to a type its own module declared",
                        d.module
                    ),
                ));
            }
            return Err(Diag::new(f.span, format!("unknown type `{shown}`")));
        };
        if d.tparams.is_empty() {
            return Err(Diag::new(
                f.span,
                format!("type `{shown}` is not generic; write the receiver as `{shown}`"),
            ));
        }
        if d.tparams.len() != f.recv_tparams.len() {
            return Err(Diag::new(
                f.span,
                format!(
                    "type `{shown}` takes {} type argument(s), found {}",
                    d.tparams.len(),
                    f.recv_tparams.len()
                ),
            ));
        }
        Ok(())
    }

    /// `recv` is the receiver's output type name for an instance method
    /// (`Program`-level name, already instantiated), `None` for a function
    /// or a static method -- see `cur_recv`.
    pub(super) fn subst_func(
        &mut self,
        f: &Func,
        sub: &Subst,
        recv: Option<String>,
    ) -> Result<Func, Diag> {
        self.cur_module = f.module.clone();
        self.cur_ret = Some(f.ret);
        let ret = self.subst_ty(f.ret, sub, f.span)?;
        let mut params = Vec::new();
        let mut scope = HashMap::new();
        // A method's receiver fields are in scope bare, so inference can
        // read their written types like a local's. On a generic receiver
        // they are written in the TYPE's parameter names, which mean what
        // they say here only when the method named them the same way.
        if let Some(d) = f.recv.as_ref().and_then(|r| self.decls.get(r)) {
            if !f.is_static && d.tparams == f.recv_tparams {
                for fld in &d.fields {
                    scope.insert(fld.name.clone(), fld.ty);
                }
            }
        }
        for p in &f.params {
            scope.insert(p.name.clone(), p.ty);
            params.push(Param {
                ty: self.subst_ty(p.ty, sub, p.span)?,
                name: p.name.clone(),
                default: match &p.default {
                    Some(e) => Some(self.subst_expr(e, sub)?),
                    None => None,
                },
                embedded: p.embedded,
                is_pub: false,
                is_const: p.is_const,
                span: p.span,
            });
        }
        self.env.clear();
        self.env.push(scope);
        self.recv_ty = match &f.recv {
            Some(r) if !f.is_static => Some(self.src_ty_named(r)),
            _ => None,
        };
        self.cur_recv = recv;
        let body = self.subst_block(&f.body, sub);
        self.recv_ty = None;
        self.cur_recv = None;
        let body = body?;
        self.env.clear();
        Ok(Func {
            module: f.module.clone(),
            is_pub: f.is_pub,
            ret,
            is_static: f.is_static,
            is_prim: f.is_prim,
            native: f.native.clone(),
            recv: f.recv.clone(),
            name: f.name.clone(),
            tparams: Vec::new(),
            recv_tparams: Vec::new(),
            params,
            body,
            span: f.span,
        })
    }
}
