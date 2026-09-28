//! Function references and lambdas: capture rewriting into the wrapper
//! method a lambda actually becomes.

use super::{Lowerer, Sig, Val, BUILTIN_FNS};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{Inst, IrTy, TypeDef};

impl Lowerer {
    /// The function an expression names, when it names one: its key in
    /// `sigs` and the spelling to put in a diagnostic.
    ///
    /// `None` means the expression is not a function's name at all, and the
    /// ordinary lowering runs and gives its own message. Privacy is NOT
    /// judged here -- a private function is still *found*, so that the
    /// refusal can say so rather than "unknown variable".
    fn fn_ref_name(&self, e: &Expr) -> Option<(String, String, Span)> {
        match e {
            Expr::Var(name, span) => {
                // A local, a field of the receiver and a module constant all
                // hold the name against a function -- and none of them can
                // collide with one, because nothing shadows anything (§4.1).
                // Checking them anyway keeps this from depending on that.
                if self.lookup(name).is_some()
                    || self.recv_field(name).is_some()
                    || self.resolve_const(name).is_some()
                {
                    return None;
                }
                let key = self.resolve_fn(name);
                if self.sigs.contains_key(&key) && !key.contains('.') {
                    return Some((key, name.clone(), *span));
                }
                // Another module's, named bare. Found so that the refusal
                // can name the module, exactly as a bare CALL of it does.
                // `min` rather than `find`: two modules may declare the name,
                // and a HashMap has no order, so `find` would name a
                // different one on different runs.
                let suffix = format!("#{name}");
                let k = self
                    .sigs
                    .keys()
                    .filter(|k| k.ends_with(&suffix) && !k.contains('.'))
                    .min()?;
                Some((k.clone(), name.clone(), *span))
            }
            // `mod.by_x`, the qualified form. `mod` is a module rather than a
            // variable only when no local has taken the name.
            Expr::Field(obj, field, span) => {
                let Expr::Var(m, _) = &**obj else { return None };
                if !self.module_in_scope(m) || self.lookup(m).is_some() {
                    return None;
                }
                let key = format!("{m}#{field}");
                self.sigs
                    .contains_key(&key)
                    .then(|| (key, format!("{m}.{field}"), *span))
            }
            _ => None,
        }
    }

    /// The refusal for a function's name written where nothing says what
    /// type is wanted. `None` when the expression names no function, so the
    /// caller's own diagnostic stands.
    pub(super) fn fn_ref_no_target(&self, e: &Expr) -> Option<Diag> {
        let (key, shown, span) = self.fn_ref_name(e)?;
        // Privacy first, so that a private function of another module is not
        // described as merely being in the wrong position.
        if let Err(d) = self.check_fn_ref_access(&key, &shown, span) {
            return Some(d);
        }
        Some(Diag::new(
            span,
            format!(
                "`{shown}` is a function; it becomes a value only where a one-method \
                 interface is expected, and nothing here says one is -- bind it to a \
                 local of the interface type first"
            ),
        ))
    }

    /// `by_x` where a one-method interface is expected. `Ok(None)` when the
    /// expression does not name a function; otherwise it is the wrapper's
    /// construction, or the reason there is none.
    pub(super) fn lower_fn_ref(&mut self, e: &Expr, want: Ty) -> Result<Option<Val>, Diag> {
        let Some((key, shown, span)) = self.fn_ref_name(e) else {
            return Ok(None);
        };
        self.check_fn_ref_access(&key, &shown, span)?;
        let tid = self.fn_ref_wrapper(&key, &shown, want, span)?;
        let ty = self
            .ty_named(&self.typedefs[tid as usize].name.clone())
            .expect("the wrapper type was just declared");
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Alloc { dst: d, tid });
        self.stmt_temps.push(d);
        Ok(Some(Val::new(d, ty, true)))
    }

    /// The synthesised type for a (function, interface) pair, made once.
    ///
    /// Everything this refuses is refused here rather than by the ordinary
    /// satisfaction check, because there is no type yet to report a mismatch
    /// on: the expression's only type is the one it is being checked against.
    fn fn_ref_wrapper(
        &mut self,
        key: &str,
        shown: &str,
        want: Ty,
        span: Span,
    ) -> Result<u32, Diag> {
        let m = self.fn_ref_method(key, shown, want, span)?;
        let iname = self.typedefs[self.tdef_of(want).expect("checked") as usize]
            .name
            .clone();
        if let Some(tid) = self.fn_refs.get(&(key.to_string(), iname.clone())) {
            return Ok(*tid);
        }

        let sig = self.sigs.get(key).expect("checked");
        let module = sig.module.clone();
        let ret = m.ret;
        // Parameter names come from neither side. The interface's would have
        // to pass the no-shadowing checks in the module the wrapper lands in,
        // and a `$` cannot appear in a source identifier, so these can
        // collide with nothing a program can write.
        let params: Vec<Param> = m
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| Param {
                ty: p.ty,
                name: format!("$a{i}"),
                default: None,
                embedded: false,
                is_pub: false,
                span,
            })
            .collect();
        let mname = m.name.clone();

        // `__` is reserved in every identifier (§10.1), so no program can
        // write this name or collide with it. The counter is for the pair
        // that would otherwise spell the same name as another -- possible
        // only through monomorphisation's own `$`, and cheap to rule out.
        let mut tname = format!("__ref${key}${iname}");
        let mut n = 2;
        while self.typedefs.iter().any(|d| d.name == tname) {
            tname = format!("__ref${key}${iname}${n}");
            n += 1;
        }

        let tid = self.typedefs.len() as u32;
        self.typedefs.push(TypeDef {
            name: tname.clone(),
            fields: Vec::new(),
            variants: Vec::new(),
            is_enum: false,
            is_value: false,
            is_interface: false,
            is_chan: false,
            is_distinct: false,
            vtable: Vec::new(),
            destructor: None,
            resource: None,
            // A forwarder is not a value anyone compares, hashes or sorts:
            // it has no fields, and the interface it satisfies is the only
            // thing ever asked of it.
            cmp: None,
            hash: None,
            eq: None,
        });
        self.field_surface.push(Vec::new());
        self.field_params.push(Vec::new());
        self.distinct_base.push(None);
        self.variant_surface.push(Vec::new());
        self.iface_methods.push(Vec::new());
        // The wrapper belongs to the module that declared the FUNCTION, so
        // that its forwarding call is an ordinary same-module call and a
        // private function stays callable from it. The reference site's own
        // right to name that function was settled by `check_fn_ref_access`
        // just above; the wrapper is `pub` because the interface value it
        // becomes is handed to whoever asked for it.
        self.type_module.push(module.clone());
        self.type_pub.push(true);
        self.fn_refs.insert((key.to_string(), iname), tid);

        // `int __ref$by_x$Less.cmp(Point $a0, Point $a1) { return by_x($a0, $a1); }`
        let call = Expr::Call(
            key.to_string(),
            Args {
                pos: params
                    .iter()
                    .map(|p| Expr::Var(p.name.clone(), span))
                    .collect(),
                named: Vec::new(),
            },
            span,
        );
        let body = if ret == Ty::Void {
            vec![Stmt::Eval { expr: call, span }]
        } else {
            vec![Stmt::Return {
                value: Some(call),
                span,
            }]
        };
        let f = Func {
            module: module.clone(),
            is_pub: true,
            ret,
            is_static: false,
            is_prim: false,
            recv: Some(tname.clone()),
            name: mname,
            tparams: Vec::new(),
            recv_tparams: Vec::new(),
            params: params.clone(),
            body,
            span,
        };
        self.sigs.insert(
            f.key(),
            Sig {
                params,
                ret,
                module,
                is_pub: true,
                is_prim: false,
            },
        );
        self.synth_funcs.push(f);
        Ok(tid)
    }

    /// The one method the target interface declares, once every reason a
    /// function cannot stand in for it has been ruled out.
    fn fn_ref_method(&self, key: &str, shown: &str, want: Ty, span: Span) -> Result<Func, Diag> {
        let bad = |why: String| Diag::new(span, why);
        let not_one = || {
            format!(
                "`{shown}` is a function; it becomes a value only where a one-method \
                 interface is expected, and `{}` is not one -- bind it to a local of \
                 the interface type first",
                self.tyname(want)
            )
        };
        let Some(tt) = self.tdef_of(want) else {
            return Err(bad(not_one()));
        };
        if !self.typedefs[tt as usize].is_interface {
            return Err(bad(not_one()));
        }
        let ms = &self.iface_methods[tt as usize];
        if ms.len() != 1 {
            // Not the same mistake as the one above: the reader wrote an
            // interface, so say what is wrong with THIS interface. A function
            // is one operation and can only ever be one method.
            return Err(bad(format!(
                "`{}` declares {} methods, so no function can satisfy it: a function \
                 is one operation, and `{shown}` could only ever supply one of them \
                 -- declare a type with all of them and pass one of those",
                self.tyname(want),
                ms.len()
            )));
        }
        let m = ms[0].clone();
        let sig = self.sigs.get(key).expect("fn_ref_name found it");

        // The receiver is not a parameter. A method gets its value from
        // `this`; the wrapper's `this` carries nothing, so a method that
        // declares no parameters has no way to be given anything, and no
        // function can satisfy it.
        if m.params.is_empty() {
            let n = sig.params.len();
            return Err(bad(format!(
                "`{shown}` takes {n} parameter{}; `{}.{}` takes none and gets its value \
                 from the receiver, so no function can satisfy it -- give the type a \
                 `{}` method instead",
                if n == 1 { "" } else { "s" },
                self.tyname(want),
                m.name,
                m.name
            )));
        }
        // Exact, in both directions, and the same comparison a type's method
        // goes through in `missing_method`: no variance, no defaulted
        // parameter standing in for a missing one.
        let same = sig.ret == m.ret
            && sig.params.len() == m.params.len()
            && sig
                .params
                .iter()
                .zip(m.params.iter())
                .all(|(a, b)| a.ty == b.ty);
        if !same {
            let sh = |ps: &[Param], ret: Ty| {
                format!(
                    "{} ({})",
                    self.tyname(ret),
                    ps.iter()
                        .map(|p| self.tyname(p.ty))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Err(bad(format!(
                "`{shown}` is `{}`, and `{}.{}` is `{}`; a function satisfies a \
                 one-method interface only on an exact match -- same parameter types, \
                 in order, and the same return type",
                sh(&sig.params, sig.ret),
                self.tyname(want),
                m.name,
                sh(&m.params, m.ret)
            )));
        }
        Ok(m)
    }

    /// The field holding the receiver of the method a lambda was written in.
    /// `$` cannot appear in a source identifier, so it collides with nothing.
    const SELF: &'static str = "$self";

    /// `(Point a, Point b) => a.x - b.x` where a one-method interface is
    /// expected. `Ok(None)` when the expression is not a lambda.
    pub(super) fn lower_lambda(&mut self, e: &Expr, want: Ty) -> Result<Option<Val>, Diag> {
        let Expr::Lambda(params, body, span) = e else {
            return Ok(None);
        };
        let m = self.lambda_method(params, want, *span)?;
        self.lambda_check(body)?;
        for (i, p) in params.iter().enumerate() {
            // A lambda's parameters are declarations like any other, so
            // §4.1 applies to them: they may not shadow a local, a
            // parameter, a field of the receiver, a function, a type, a
            // constant or an import. The cost lands on short names in
            // functions that have short locals, and the fix is a rename.
            self.check_shadow(&p.name, p.span)?;
            if params[..i].iter().any(|q| q.name == p.name) {
                return Err(Diag::new(
                    p.span,
                    format!("duplicate parameter `{}`", p.name),
                ));
            }
        }
        // Every lambda inside this one is checked here too, in the scope the
        // OUTER lambda is written in: by the time an inner one is lowered
        // the enclosing method is the outer lambda's, whose receiver is the
        // outer lambda object, and the names of this scope are no longer
        // visible to be shadowed.
        self.lambda_inner_shadow(body)?;
        let body = self.lambda_rewrite(body);
        let caps = self.lambda_captures(&body, *span)?;
        let ctor = self.lambda_wrapper(params, &body, &m, want, &caps, *span)?;
        self.lower_expr(&ctor).map(Some)
    }

    /// The refusal for `f(..)` where `f` is a value in scope rather than a
    /// function. `None` when the name is not one, so the ordinary "unknown
    /// function" stands.
    ///
    /// There is exactly one call syntax in this language and it is never
    /// `value(args)`: an interface value is called through its method,
    /// which is what makes `f(x)`, `obj.handler(x)` and `xs[0](y)` need no
    /// disambiguation rules at all. This says so at the one place a reader
    /// coming from a language with function values will write it.
    pub(super) fn call_of_a_value(&self, name: &str, span: Span) -> Option<Diag> {
        let ty = self.static_ty(&Expr::Var(name.to_string(), span), true)?;
        let what = self.tyname(ty);
        // A one-method interface can say exactly what to write instead.
        let one = self
            .tdef_of(ty)
            .filter(|t| self.typedefs[*t as usize].is_interface)
            .map(|t| &self.iface_methods[t as usize])
            .filter(|ms| ms.len() == 1)
            .map(|ms| ms[0].name.clone());
        Some(Diag::new(
            span,
            match one {
                Some(m) => format!(
                    "`{name}` is a `{what}`, not a function: a callback is called \
                     through its method, so write `{name}.{m}(..)`"
                ),
                None => format!("`{name}` is a `{what}`, not a function"),
            },
        ))
    }

    /// The refusal for a lambda written where nothing says what type is
    /// wanted: an argument to a builtin or to an unconstrained type
    /// parameter, an expression statement.
    pub(super) fn lambda_no_target(span: Span) -> Diag {
        Diag::new(
            span,
            "a lambda takes its method name from the interface it is passed to, \
             and nothing here expects one; declare an interface and bind it to a \
             local of that type",
        )
    }

    /// The one method the target interface declares, once the lambda's shape
    /// has been checked against it. Everything but the return type is known
    /// here; the body's type is checked where the body is lowered.
    fn lambda_method(&self, params: &[Param], want: Ty, span: Span) -> Result<Func, Diag> {
        let not_one = || {
            Diag::new(
                span,
                format!(
                    "a lambda takes its method name from the one-method interface it \
                     is passed to, and `{}` is not one; declare an interface and bind \
                     it to a local of that type",
                    self.tyname(want)
                ),
            )
        };
        let Some(tt) = self.tdef_of(want) else {
            return Err(not_one());
        };
        if !self.typedefs[tt as usize].is_interface {
            return Err(not_one());
        }
        let ms = &self.iface_methods[tt as usize];
        if ms.len() != 1 {
            // The reader wrote an interface, so say what is wrong with THIS
            // interface, and name its methods: which one the lambda was
            // meant to be is the question they have to answer.
            return Err(Diag::new(
                span,
                format!(
                    "`{}` declares {} methods ({}); a lambda supplies one, so write a \
                     type with all of them and pass one of those",
                    self.tyname(want),
                    ms.len(),
                    ms.iter()
                        .map(|m| format!("`{}`", m.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        let m = ms[0].clone();
        // Arity and parameter types name the INTERFACE METHOD, not the
        // lambda, because that is where the truth is: the lambda is being
        // checked against it and cannot change it.
        if m.params.len() != params.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{}.{}` takes {} parameter{}, this lambda takes {}",
                    self.tyname(want),
                    m.name,
                    m.params.len(),
                    if m.params.len() == 1 { "" } else { "s" },
                    params.len()
                ),
            ));
        }
        if m.params.iter().zip(params).any(|(a, b)| a.ty != b.ty) {
            let sh = |ps: &[Param]| {
                ps.iter()
                    .map(|p| self.tyname(p.ty))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(Diag::new(
                span,
                format!(
                    "`{}.{}` takes ({}), this lambda takes ({})",
                    self.tyname(want),
                    m.name,
                    sh(&m.params),
                    sh(params)
                ),
            ));
        }
        Ok(m)
    }

    /// What a lambda's body may not contain, wherever it appears in it.
    fn lambda_check(&self, e: &Expr) -> Result<(), Diag> {
        if let Expr::Try(_, span) = e {
            return Err(Diag::new(
                *span,
                "`?` returns from the enclosing function, and a lambda has no \
                 enclosing function a reader can act on; write a named function",
            ));
        }
        // A bare call that is both a method of the receiver and a function
        // is ambiguous here for the same reason it is outside a lambda --
        // and the rewrite below would silently pick the method, so the
        // refusal has to be made before it.
        if let Expr::Call(name, _, span) = e {
            if self.sibling_method(name).is_some() {
                let written = match self.shown.get(name) {
                    Some((generic, _)) => crate::ast::bare(generic).to_string(),
                    None => name.clone(),
                };
                if self.sigs.contains_key(&self.resolve_fn(&written))
                    || BUILTIN_FNS.contains(&written.as_str())
                {
                    let (tid, _) = self.recv.expect("sibling_method checked the receiver");
                    return Err(Diag::new(
                        *span,
                        format!(
                            "`{written}` is both a method of `{}` and a function, so a \
                             bare `{written}(..)` here could mean either; rename one",
                            self.show_name(&self.typedefs[tid as usize].name)
                        ),
                    ));
                }
            }
        }
        Self::kids(e, &mut |k| self.lambda_check(k))
    }

    /// `check_shadow` for the parameters of every lambda nested inside this
    /// one, in the scope this one is written in.
    fn lambda_inner_shadow(&self, e: &Expr) -> Result<(), Diag> {
        if let Expr::Lambda(ps, _, _) = e {
            for p in ps {
                self.check_shadow(&p.name, p.span)?;
            }
        }
        Self::kids(e, &mut |k| self.lambda_inner_shadow(k))
    }

    /// Every immediate subexpression of `e`, for a walk that only reads.
    fn kids(e: &Expr, f: &mut impl FnMut(&Expr) -> Result<(), Diag>) -> Result<(), Diag> {
        let args = |a: &Args, f: &mut dyn FnMut(&Expr) -> Result<(), Diag>| {
            for x in a.pos.iter().chain(a.named.iter().map(|(_, x)| x)) {
                f(x)?;
            }
            Ok(())
        };
        match e {
            Expr::Int(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::Str(..)
            | Expr::Var(..)
            | Expr::This(..) => Ok(()),
            Expr::Un(_, x, _) | Expr::Field(x, _, _) | Expr::Try(x, _) | Expr::Lambda(_, x, _) => {
                f(x)
            }
            Expr::Bin(_, a, b, _) | Expr::Index(a, b, _) | Expr::RepeatLit(a, b, _) => {
                f(a)?;
                f(b)
            }
            Expr::MethodCall(x, _, a, _) => {
                f(x)?;
                args(a, f)
            }
            Expr::Call(_, a, _) | Expr::New(_, a, _) | Expr::EnumNew(_, _, a, _) => args(a, f),
            Expr::SeqLit(xs, _) => xs.iter().try_for_each(f),
            Expr::MapLit(kvs, _) => kvs.iter().try_for_each(|(k, v)| {
                f(k)?;
                f(v)
            }),
        }
    }

    /// A lambda's body, with everything it says about the enclosing
    /// receiver said through `$self` instead.
    ///
    /// Inside the synthesised method the receiver is the LAMBDA, so `this`,
    /// a bare field name and a bare sibling call would all mean the wrong
    /// object -- or nothing at all. Each becomes the same thing written
    /// against the captured receiver, and the capture walk below then sees
    /// one name, `$self`, however many of the three the body used.
    ///
    /// The three are rewritten identically because the language already
    /// treats them identically: "a method of the receiver is called by its
    /// bare name, **as a field is read**". They are all `this.`, unwritten,
    /// so a lambda that mentions any of them captures `this` -- which is
    /// what `=` does with `this`: it retains the object. Field reads
    /// therefore stay live, as they read the same object a reader is
    /// looking at, and a field's REASSIGNMENT is visible through the lambda
    /// exactly as it is through any other name for that object.
    ///
    /// Nested lambdas are rewritten too, and correctly: an inner body's
    /// `$self` is a field of the outer lambda, so when the inner one is
    /// lowered this same rewrite turns it into `$self.$self`, which reaches
    /// the original receiver through the outer lambda it captured.
    fn lambda_rewrite(&self, e: &Expr) -> Expr {
        let go = |x: &Expr| Box::new(self.lambda_rewrite(x));
        let args = |a: &Args| Args {
            pos: a.pos.iter().map(|x| self.lambda_rewrite(x)).collect(),
            named: a
                .named
                .iter()
                .map(|(n, x)| (n.clone(), self.lambda_rewrite(x)))
                .collect(),
        };
        let this = |s: Span| Box::new(Expr::Var(Self::SELF.to_string(), s));
        match e {
            Expr::This(s) => Expr::Var(Self::SELF.to_string(), *s),
            Expr::Var(n, s) if self.lookup(n).is_none() && self.recv_field(n).is_some() => {
                Expr::Field(this(*s), n.clone(), *s)
            }
            Expr::Call(n, a, s) if self.sibling_method(n).is_some() => {
                Expr::MethodCall(this(*s), n.clone(), args(a), *s)
            }
            Expr::Int(..) | Expr::Float(..) | Expr::Bool(..) | Expr::Str(..) | Expr::Var(..) => {
                e.clone()
            }
            Expr::Un(op, x, s) => Expr::Un(*op, go(x), *s),
            Expr::Field(o, f, s) => Expr::Field(go(o), f.clone(), *s),
            Expr::Try(x, s) => Expr::Try(go(x), *s),
            Expr::Lambda(ps, x, s) => Expr::Lambda(ps.clone(), go(x), *s),
            Expr::Bin(op, a, b, s) => Expr::Bin(*op, go(a), go(b), *s),
            Expr::Index(a, b, s) => Expr::Index(go(a), go(b), *s),
            Expr::RepeatLit(a, b, s) => Expr::RepeatLit(go(a), go(b), *s),
            Expr::MethodCall(o, m, a, s) => Expr::MethodCall(go(o), m.clone(), args(a), *s),
            Expr::Call(n, a, s) => Expr::Call(n.clone(), args(a), *s),
            Expr::New(t, a, s) => Expr::New(*t, args(a), *s),
            Expr::EnumNew(t, v, a, s) => Expr::EnumNew(*t, v.clone(), args(a), *s),
            Expr::SeqLit(xs, s) => {
                Expr::SeqLit(xs.iter().map(|x| self.lambda_rewrite(x)).collect(), *s)
            }
            Expr::MapLit(kvs, s) => Expr::MapLit(
                kvs.iter()
                    .map(|(k, v)| (self.lambda_rewrite(k), self.lambda_rewrite(v)))
                    .collect(),
                *s,
            ),
        }
    }

    /// What a lambda's body takes from the scope around it: for each, the
    /// field it becomes, the expression that fills the field at the site,
    /// and its type.
    ///
    /// **There is no capture list and there is nothing to infer.** A name
    /// the body mentions that resolves to a local or a parameter here is a
    /// capture; a function, a type, a module constant or an import is not,
    /// because the synthesised method is declared in this module and reaches
    /// them the same way this body does.
    ///
    /// A lambda's own parameters -- and a nested lambda's -- are never
    /// mistaken for captures: nothing shadows anything, so a parameter's
    /// name cannot also be a name in scope here.
    fn lambda_captures(&mut self, body: &Expr, span: Span) -> Result<Vec<(Param, Expr)>, Diag> {
        let mut names: Vec<String> = Vec::new();
        Self::mentions(body, &mut names);
        let mut out = Vec::new();
        for name in names {
            let (ty, init) = if name == Self::SELF {
                // The enclosing receiver. `this_val` gives the diagnostic
                // for a lambda that mentions one where there is none.
                (self.this_val(span)?.ty, Expr::This(span))
            } else if let Some((ty, _)) = self.lookup(&name) {
                (ty, Expr::Var(name.clone(), span))
            } else {
                continue;
            };
            out.push((
                Param {
                    ty,
                    name,
                    default: None,
                    embedded: false,
                    is_pub: false,
                    span,
                },
                init,
            ));
        }
        Ok(out)
    }

    /// Every name the body mentions, in the order it first mentions them,
    /// so the capture fields of one lambda are always laid out the same way.
    fn mentions(e: &Expr, out: &mut Vec<String>) {
        if let Expr::Var(n, _) = e {
            if !out.iter().any(|x| x == n) {
                out.push(n.clone());
            }
        }
        let _ = Self::kids(e, &mut |k| {
            Self::mentions(k, out);
            Ok(())
        });
    }

    /// The synthesised type and method for one lambda, and the construction
    /// that makes one: `__lam$3$Less(n, xs)`.
    fn lambda_wrapper(
        &mut self,
        params: &[Param],
        body: &Expr,
        m: &Func,
        want: Ty,
        caps: &[(Param, Expr)],
        span: Span,
    ) -> Result<Expr, Diag> {
        let iname = self.typedefs[self.tdef_of(want).expect("checked") as usize]
            .name
            .clone();
        // Not cached, and not named after the site: two lambdas at one
        // source position are two types whenever monomorphisation made two
        // of them, and each instantiation has its own concrete capture
        // types. A counter is what makes the name unique AND the emission
        // reproducible, since lowering order is fixed.
        self.lambdas += 1;
        let tname = format!("__lam${}${iname}", self.lambdas);
        let tid = self.typedefs.len() as u32;
        self.typedefs.push(TypeDef {
            name: tname.clone(),
            fields: caps
                .iter()
                .map(|(p, _)| (p.name.clone(), self.irty(p.ty)))
                .collect(),
            variants: Vec::new(),
            is_enum: false,
            is_value: false,
            is_interface: false,
            is_chan: false,
            is_distinct: false,
            vtable: Vec::new(),
            // A synthesised type cannot declare a destructor -- it is
            // anonymous, there is nowhere to write one -- and it needs
            // none: its fields are released like any other type's, so a
            // captured `File` is closed when the lambda's count reaches
            // zero. It is not a value anyone compares, hashes or sorts
            // either; the interface it satisfies is all that is asked of it.
            destructor: None,
            resource: None,
            cmp: None,
            hash: None,
            eq: None,
        });
        self.field_surface
            .push(caps.iter().map(|(p, _)| p.ty).collect());
        self.field_params
            .push(caps.iter().map(|(p, _)| p.clone()).collect());
        self.distinct_base.push(None);
        self.variant_surface.push(Vec::new());
        self.iface_methods.push(Vec::new());
        self.type_module.push(self.cur_module.clone());
        self.type_pub.push(true);

        // The method IS the interface's method: same name, same signature.
        // The parameter names are the lambda's own, because its body uses
        // them; they passed `check_shadow` at the site, so they collide with
        // nothing the synthesised method can see either.
        let f = Func {
            module: self.cur_module.clone(),
            is_pub: true,
            ret: m.ret,
            is_static: false,
            is_prim: false,
            recv: Some(tname.clone()),
            name: m.name.clone(),
            tparams: Vec::new(),
            recv_tparams: Vec::new(),
            params: params.to_vec(),
            // One expression, so: return it, or -- where the interface says
            // the method returns nothing -- evaluate it and discard the
            // value, which is what an expression statement does.
            body: vec![if m.ret == Ty::Void {
                Stmt::Eval {
                    expr: body.clone(),
                    span,
                }
            } else {
                Stmt::Return {
                    value: Some(body.clone()),
                    span,
                }
            }],
            span,
        };
        self.sigs.insert(
            f.key(),
            Sig {
                params: params.to_vec(),
                ret: m.ret,
                module: self.cur_module.clone(),
                is_pub: true,
                is_prim: false,
            },
        );
        self.lambda_targets
            .insert(f.key(), (self.tyname(want), m.name.clone()));
        self.synth_funcs.push(f);

        // The captures are FIELDS, so the lambda is an ordinary
        // construction and every rule about storing a value in a field
        // applies to it without being restated: the +1, the deep copy a
        // `const` takes, the resource a constant may not hold, the
        // destructor that runs when the last reference goes.
        let ty = self
            .ty_named(&tname)
            .expect("the lambda's type was just declared");
        let _ = tid;
        Ok(Expr::New(
            ty,
            Args {
                pos: caps.iter().map(|(_, e)| e.clone()).collect(),
                named: Vec::new(),
            },
            span,
        ))
    }
}
