//! Calls: argument binding, static, method and qualified calls, `trap`.

use super::*;

impl Lowerer {
    /// Bind a call's arguments to a parameter list, by Oro's rule:
    /// **mandatory parameters are positional, optional ones are named.**
    /// Never both, so there is no question of which form to use and no
    /// question of what order optional arguments come in.
    ///
    /// Returns one expression per parameter, in declaration order, with
    /// defaults filled in.
    pub(in crate::middle::lower) fn bind_args<'a>(
        &self,
        what: &str,
        params: &'a [Param],
        args: &'a Args,
        span: Span,
    ) -> Result<Vec<&'a Expr>, Diag> {
        let mandatory: Vec<&Param> = params.iter().filter(|p| !p.is_optional()).collect();
        let mut out: Vec<Option<&Expr>> = vec![None; params.len()];

        // Named arguments first: "you named a mandatory parameter" is a more
        // useful thing to say than "wrong number of positional arguments",
        // and it is the mistake someone coming from Python will make.
        for (n, e) in &args.named {
            let Some(i) = params.iter().position(|p| p.name == *n) else {
                return Err(Diag::new(
                    e.span(),
                    format!("`{}` has no parameter `{n}`", self.bare_name(what)),
                ));
            };
            if !params[i].is_optional() {
                return Err(Diag::new(
                    e.span(),
                    format!("`{n}` is mandatory, so it is positional; drop the `{n}:`"),
                ));
            }
            if out[i].is_some() {
                return Err(Diag::new(e.span(), format!("`{n}` given twice")));
            }
            out[i] = Some(e);
        }

        if args.pos.len() != mandatory.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` takes {} positional argument(s), found {}",
                    self.bare_name(what),
                    mandatory.len(),
                    args.pos.len()
                ),
            ));
        }

        let mut next = 0;
        for a in &args.pos {
            while params[next].is_optional() {
                next += 1;
            }
            out[next] = Some(a);
            next += 1;
        }

        Ok(params
            .iter()
            .zip(out)
            .map(|(p, given)| {
                given.unwrap_or_else(|| p.default.as_ref().expect("mandatory unbound"))
            })
            .collect())
    }

    /// Structured `if` lowering with block parameters at the join.
    ///
    /// There are no loops in v0, so the CFG is acyclic and SSA construction
    /// needs no fixpoint: take a snapshot of each variable before the branch,
    /// compare after each arm, and give the join a parameter for every
    /// variable the two arms disagree about.
    /// `Type.name(args)` where `name` is a static method: an ordinary direct
    /// call, with no receiver to pass.
    pub(super) fn lower_static_call(
        &mut self,
        key: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        let sig = self.sigs.get(key).expect("checked by the caller");
        let params = sig.params.clone();
        let module = sig.module.clone();
        let ret = sig.ret;
        let slots = self.bind_args(key, &params, args, span)?;
        let mut vals = Vec::new();
        for (a, p) in slots.iter().zip(params.iter()) {
            let v = self.lower_slot(a, p, &module)?;
            if !self.assignable(v.ty, p.ty) {
                return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
            }
            // The method itself is user code (src/middle/lower/hold.rs).
            let v = self.hold(a, v, true);
            vals.push(v.val());
        }
        if ret == Ty::Void {
            self.push(Inst::Call {
                dst: None,
                func: key.to_string(),
                args: vals,
            });
            return Ok(Val::void());
        }
        let d = self.new_val(self.irty(ret));
        self.push(Inst::Call {
            dst: Some(d),
            func: key.to_string(),
            args: vals,
        });
        let owned = self.is_ref(ret);
        if owned {
            self.stmt_temps.push(d);
        }
        Ok(Val::new(d, ret, owned))
    }

    /// `v.to_str()`, for `print` and for `str(v)`.
    ///
    /// Found BY NAME, the way `add`, `eq` and `cmp` already are. There is no
    /// blessed `ToStr` type, because none is needed: interfaces here are
    /// structural, so a program that wants to pass "anything printable"
    /// around declares `interface ToStr { str to_str(); }` itself and every
    /// type with the method satisfies it with no further ceremony. Blessing
    /// one would buy nothing and freeze a name.
    ///
    /// Returns None when the type has no such method, so the caller can give
    /// a diagnostic that fits what it was doing.
    pub(super) fn call_to_str(&mut self, v: &Val, span: Span) -> Result<Option<Val>, Diag> {
        let Some(tid) = self.tdef_of(v.ty) else {
            return Ok(None);
        };
        let tname = self.typedefs[tid as usize].name.clone();

        // On an interface value the implementation is not known statically.
        if self.typedefs[tid as usize].is_interface {
            let Some(decl) = self.iface_methods[tid as usize]
                .iter()
                .find(|x| x.name == "to_str")
                .cloned()
            else {
                return Ok(None);
            };
            if decl.ret != Ty::Str || !decl.params.is_empty() {
                return Ok(None);
            }
            // By name AND shape: the slot has to be the one this
            // interface's declaration occupies, not merely one that shares
            // the name.
            let want = self.slot_of(&decl);
            let slot = self
                .iface_slots
                .iter()
                .position(|x| *x == want)
                .expect("every interface method has a slot") as u32;
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::CallIface {
                dst: Some(d),
                slot,
                name: "to_str".to_string(),
                args: vec![v.val()],
                ret: Some(IrTy::Ref),
            });
            self.stmt_temps.push(d);
            return Ok(Some(Val::new(d, Ty::Str, true)));
        }

        let key = format!("{tname}.to_str");
        if !self.sigs.contains_key(&key) {
            return Ok(None);
        }
        self.check_method_access(tid, "to_str", span)?;
        let sig = &self.sigs[&key];
        if sig.ret != Ty::Str || !sig.params.is_empty() {
            return Err(Diag::new(
                span,
                format!("`{key}` must take no arguments and return str to be used here"),
            ));
        }
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Call {
            dst: Some(d),
            func: key,
            args: vec![v.val()],
        });
        self.stmt_temps.push(d);
        Ok(Some(Val::new(d, Ty::Str, true)))
    }

    /// A bare call `m(args)` inside an instance method whose receiver has an
    /// instance method `m`: lowered as `this.m(args)`. `None` when the name
    /// is not a sibling, so the ordinary function lookup runs.
    ///
    /// A sibling that is also the name of a function in scope -- this
    /// module's, the entry file's or a builtin -- is refused rather than
    /// ranked. Either ranking would make a call's meaning depend on a
    /// declaration somewhere else in the module: adding a method would
    /// silently redirect every bare call to a function of the same name.
    /// Nothing shadows anything in this language (§4.1).
    ///
    /// `name` may already be an instantiation: monomorphisation rewrites a
    /// call to a generic function to its mangled name before this runs, so
    /// the name as written is recovered from `shown` first.
    pub(super) fn sibling_call(
        &mut self,
        name: &str,
        args: &Args,
        span: Span,
    ) -> Result<Option<Val>, Diag> {
        if self.recv.is_none() {
            return Ok(None);
        }
        let written = match self.shown.get(name) {
            Some((generic, _)) => crate::front::ast::bare(generic).to_string(),
            None => name.to_string(),
        };
        let (rtid, _) = self.recv.expect("checked above");
        self.refuse_destructor_call(rtid, &written, span)?;
        // A bare call to one of the receiver's GENERIC methods arrives
        // already renamed to its instantiation, `pick(xs)` as `pick$int(xs)`:
        // monomorphisation knows the receiver's type here and did the
        // inference. It refuses a generic function of the same name itself,
        // so a mangled name that is the receiver's method can only be this.
        let generic_sibling = written != name && self.sibling_method(name).is_some();
        if generic_sibling {
            if self.sigs.contains_key(&self.resolve_fn(&written))
                || BUILTIN_FNS.contains(&written.as_str())
            {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{written}` is both a method of `{}` and a function, so a bare \
                         `{written}(..)` here could mean either; rename one",
                        self.show_name(&self.typedefs[rtid as usize].name)
                    ),
                ));
            }
            let this = self.this_val(span)?;
            return self
                .lower_method_on(&this, &name.to_string(), args, span)
                .map(Some);
        }
        if self.sibling_method(&written).is_none() {
            // A static sibling has no receiver to be called on, so it is not
            // reachable bare -- say how it is reached instead of reporting an
            // unknown function the reader can see declared.
            let (tid, _) = self.recv.expect("checked above");
            let tname = self.typedefs[tid as usize].name.clone();
            let is_static = self.statics.contains(&format!("{tname}.{written}"));
            if is_static && !self.sigs.contains_key(&self.resolve_fn(&written)) {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{written}` is a static method; call it on the type, as `{}.{written}(..)`",
                        self.show_name(&tname)
                    ),
                ));
            }
            return Ok(None);
        }
        let func_too = written != name
            || self.sigs.contains_key(&self.resolve_fn(&written))
            || BUILTIN_FNS.contains(&written.as_str());
        if func_too {
            let (tid, _) = self.recv.expect("checked above");
            return Err(Diag::new(
                span,
                format!(
                    "`{written}` is both a method of `{}` and a function, so a bare \
                     `{written}(..)` here could mean either; rename one",
                    self.show_name(&self.typedefs[tid as usize].name)
                ),
            ));
        }
        let this = self.this_val(span)?;
        self.lower_method_on(&this, &written, args, span).map(Some)
    }

    /// `o.m(args)` once the receiver is a value: a built-in method of a
    /// `str`, a number, a collection or an `Option`, an interface dispatch,
    /// or a declared method. Split out of the `MethodCall` lowering because
    /// a bare sibling call inside a method, `m(args)`, is the same call with
    /// `this` as the receiver.
    pub(super) fn lower_method_on(
        &mut self,
        o: &Val,
        m: &String,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        // A distinct type over a primitive, `str` or `bytes` may declare
        // methods of its own -- `str Price.show()` -- and one it declares
        // comes before its base's built-in ones, the rule a distinct
        // collection already follows below. Without this the call went
        // to the base's built-in table and a declared method on
        // `distinct int Price` could never be called.
        if let Some(t) = self.tdef_of(o.ty) {
            self.refuse_destructor_call(t, m, span)?;
        }
        let declared = self.tdef_of(o.ty).is_some_and(|t| {
            self.sigs
                .contains_key(&format!("{}.{m}", self.typedefs[t as usize].name))
        });
        // `str` is not a declared type, so it has no entry in the
        // type table -- but it still answers `size()`, because one
        // rule for asking how big a thing is beats a free function
        // for strings and a method for everything else.
        if declared {
            // Falls through to the declared-method path below.
        } else if self.underlying(o.ty) == Ty::Str {
            return self.lower_str_method(o, m, args, span);
        } else if self.underlying(o.ty) == Ty::Bytes {
            return self.lower_bytes_method(o, m, args, span);
        } else if matches!(self.underlying(o.ty), Ty::Int | Ty::Float | Ty::Bool) {
            return self.lower_prim_method(o, m, args, span);
        }
        let Some(tid) = self.tdef_of(o.ty) else {
            return Err(Diag::new(
                span,
                format!("type {} has no methods", self.tyname(o.ty)),
            ));
        };
        // Collections have built-in methods, typed against their
        // element type rather than declared anywhere. A distinct
        // collection has them too, but a method it declares itself
        // comes first -- the same rule as a real method shadowing an
        // embedded one.
        let own = format!("{}.{m}", self.typedefs[tid as usize].name);
        if !self.sigs.contains_key(&own) {
            if let Some(elem) = self.seq_elem(o.ty) {
                return self.lower_seq_method(o, elem, m, args, span);
            }
            if let Some((k, v)) = self.map_kv(o.ty) {
                return self.lower_map_method(o, k, v, m, args, span);
            }
        }
        if self.typedefs[tid as usize].is_enum
            && self.typedefs[tid as usize].name.starts_with("Option$")
        {
            return self.lower_option_method(o, tid, m, args, span);
        }
        if self.typedefs[tid as usize].is_enum
            && self.typedefs[tid as usize].name.starts_with("Result$")
        {
            return self.lower_result_method(o, tid, m, args, span);
        }
        self.check_method_access(tid, m, span)?;
        // A static method has no receiver, so it cannot be reached
        // through a value even though the spelling looks the same.
        let skey = format!("{}.{m}", self.typedefs[tid as usize].name);
        if self.statics.contains(&skey) {
            return Err(Diag::new(
                span,
                format!(
                    "`{m}` is a static method; call it on the type, as \
                         `{}.{m}(..)`",
                    self.show_name(&self.typedefs[tid as usize].name)
                ),
            ));
        }

        // On an interface value the implementation is not known
        // statically: dispatch through the receiver's type header.
        if self.typedefs[tid as usize].is_interface {
            let iname = self.show_name(&self.typedefs[tid as usize].name);
            let Some(decl) = self.iface_methods[tid as usize]
                .iter()
                .find(|x| x.name == *m)
                .cloned()
            else {
                return Err(Diag::new(
                    span,
                    format!("interface `{iname}` has no method `{m}`"),
                ));
            };
            let want = self.slot_of(&decl);
            let slot = self
                .iface_slots
                .iter()
                .position(|x| *x == want)
                .expect("every interface method has a slot") as u32;

            let slots = self.bind_args(&format!("{iname}.{m}"), &decl.params, args, span)?;
            let mut vals = vec![o.val()];
            for (a, p) in slots.iter().zip(decl.params.iter()) {
                self.refuse_unfrozen_arg(a, p)?;
                let v = self.lower_expr_as(a, p.ty)?;
                if !self.assignable(v.ty, p.ty) {
                    return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
                }
                // The dispatch itself is user code (src/middle/lower/hold.rs).
                let v = self.hold(a, v, true);
                vals.push(v.val());
            }

            if decl.ret == Ty::Void {
                self.push(Inst::CallIface {
                    dst: None,
                    slot,
                    name: m.clone(),
                    args: vals,
                    ret: None,
                });
                return Ok(Val::void());
            }
            let d = self.new_val(self.irty(decl.ret));
            self.push(Inst::CallIface {
                dst: Some(d),
                slot,
                name: m.clone(),
                args: vals,
                ret: Some(self.irty(decl.ret)),
            });
            let owned = self.is_ref(decl.ret);
            if owned {
                self.stmt_temps.push(d);
            }
            return Ok(Val::new(d, decl.ret, owned));
        }

        let key = format!("{}.{m}", self.typedefs[tid as usize].name);
        if self.generic_methods.contains(&key) {
            return Err(Diag::new(
                span,
                format!(
                    "`{m}` is a generic method, and its type arguments are \
                     inferred where the receiver's type is written down; \
                     call it on a local, a parameter, a field or a \
                     construction"
                ),
            ));
        }
        let Some(sig) = self.sigs.get(&key) else {
            return Err(Diag::new(
                span,
                format!(
                    "type `{}` has no method `{m}`",
                    self.show_name(&self.typedefs[tid as usize].name)
                ),
            ));
        };
        let params = sig.params.clone();
        let module = sig.module.clone();
        let ret = sig.ret;
        let slots = self.bind_args(&key, &params, args, span)?;

        // The receiver is the hidden first argument, and is borrowed
        // like every other argument (docs/ir-v0.md §5.1). A defaulted
        // argument is lowered in the method's own module (`lower_slot`).
        let mut vals = vec![o.val()];
        for (a, p) in slots.iter().zip(params.iter()) {
            let v = self.lower_slot(a, p, &module)?;
            if !self.assignable(v.ty, p.ty) {
                return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
            }
            // The method itself is user code (src/middle/lower/hold.rs).
            let v = self.hold(a, v, true);
            vals.push(v.val());
        }

        if ret == Ty::Void {
            self.push(Inst::Call {
                dst: None,
                func: key,
                args: vals,
            });
            Ok(Val::void())
        } else {
            let d = self.new_val(self.irty(ret));
            self.push(Inst::Call {
                dst: Some(d),
                func: key,
                args: vals,
            });
            let owned = self.is_ref(ret);
            if owned {
                self.stmt_temps.push(d);
            }
            Ok(Val::new(d, ret, owned))
        }
    }

    /// `mod.name(args)` -- a call into another module.
    pub(super) fn lower_qualified(
        &mut self,
        modname: &str,
        name: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        let key = format!("{modname}#{name}");
        // A generic function arrives already instantiated, as `first$int`
        // (see mono/); the reader wrote `first`, so that is what a
        // diagnostic names. `$` never appears in a source identifier.
        let name = name.split('$').next().unwrap_or(name);
        // `lib.Point(3, 4)` -- a construction, not a call. The parser cannot
        // tell the two apart, because it never sees another module's
        // declarations, so it is settled here where both tables are known.
        // A type of that name exists only if `lib` declared one: the name is
        // module-qualified, so nothing here can find a type from elsewhere.
        if !self.sigs.contains_key(&key) {
            if let Some(ty) = self.ty_named(&key) {
                return self.lower_new(ty, args, span);
            }
        }
        let Some(sig) = self.sigs.get(&key) else {
            return Err(Diag::new(
                span,
                format!("`{modname}` has no function `{name}`"),
            ));
        };
        if sig.module != modname {
            return Err(Diag::new(
                span,
                format!("`{name}` is not declared in `{modname}`"),
            ));
        }
        if !sig.is_pub {
            return Err(Diag::new(
                span,
                format!("`{name}` is private to `{modname}`; mark it `pub` to export it"),
            ));
        }
        self.lower_call(&key, args, span)
    }

    /// Lower one argument of a call or a construction, as `bind_args` slotted
    /// it.
    ///
    /// An argument the caller wrote is the caller's expression. A DEFAULT is
    /// the declaration's, and means what it meant where it was written: it
    /// is lowered with the declaring module's names and privacy, and sees no
    /// local and no receiver field. It used to be lowered as if the caller
    /// had written it -- so a public type whose field defaulted to a private
    /// one could not be constructed outside its module, a bare call in a
    /// default resolved against the caller's module, and a default naming
    /// `y` read whatever the caller happened to call `y`.
    pub(in crate::middle::lower) fn lower_slot(
        &mut self,
        a: &Expr,
        p: &Param,
        module: &str,
    ) -> Result<Val, Diag> {
        let is_default = p.default.as_ref().is_some_and(|d| std::ptr::eq(d, a));
        if !is_default {
            self.refuse_unfrozen_arg(a, p)?;
            return self.lower_expr_as(a, p.ty);
        }
        let module = if module.is_empty() {
            self.cur_module.clone()
        } else {
            module.to_string()
        };
        let saved_module = std::mem::replace(&mut self.cur_module, module.clone());
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        let saved_recv = self.recv.take();
        // A `const` parameter's default is checked like an argument, in
        // the declaring module with no locals: only a constant passes.
        let r = self
            .refuse_unfrozen_arg(a, p)
            .and_then(|_| self.lower_expr_as(a, p.ty))
            .and_then(|v| {
                if self.assignable(v.ty, p.ty) {
                    Ok(v)
                } else {
                    Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)))
                }
            });
        self.cur_module = saved_module;
        self.scopes = saved_scopes;
        self.recv = saved_recv;
        // Its span is a line of the declaring file, so the error is too.
        r.map_err(|d| d.in_module(&module))
    }

    /// `trap(msg);` -- stop the program, because it has a bug.
    ///
    /// The runtime's own traps cover the mistakes the language can see: an
    /// index out of range, an overflow. `trap` is the same thing for the
    /// ones only the program can see -- an argument outside what a function
    /// accepts, an invariant that does not hold. It is for a bug and not for
    /// the world (docs/errors-decision.md): a failure the caller should
    /// handle is a `Result`, and nothing can catch a trap.
    ///
    /// It never returns, so the block ends here: a function whose last
    /// statement is a `trap` needs no return after it, and a statement after
    /// one is unreachable, as after `return`. That is also why it is a
    /// statement and never a value -- there is no value it could give.
    pub(in crate::middle::lower) fn lower_trap(
        &mut self,
        args: &Args,
        span: Span,
    ) -> Result<(), Diag> {
        if args.pos.len() != 1 || !args.named.is_empty() {
            return Err(Diag::new(
                span,
                format!(
                    "`trap` takes 1 argument, its message, found {}",
                    args.pos.len() + args.named.len()
                ),
            ));
        }
        let m = self.lower_expr(&args.pos[0])?;
        if self.underlying(m.ty) != Ty::Str {
            return Err(Diag::new(args.pos[0].span(), self.mismatch(Ty::Str, m.ty)));
        }
        self.push(Inst::Call {
            dst: None,
            func: "rt_panic".to_string(),
            args: vec![m.val()],
        });
        // Nothing after the call runs, so nothing pending is released: the
        // statement's temporaries die with the process.
        self.stmt_temps.clear();
        self.terminate(Term::Ret { val: None });
        Ok(())
    }

    pub(super) fn lower_call(&mut self, name: &str, args: &Args, span: Span) -> Result<Val, Diag> {
        if name == "trap" {
            return Err(Diag::new(
                span,
                "`trap` is a statement: it never returns, so it has no value to give",
            ));
        }
        // `print` accepts int, bool or str and selects the runtime helper from
        // the static argument type. Not user-visible overloading.
        if name == "print" {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    format!(
                        "`print` takes 1 argument, found {}",
                        args.pos.len() + args.named.len()
                    ),
                ));
            }
            let a = self.lower_expr(&args.pos[0])?;
            let f = match self.underlying(a.ty) {
                Ty::Int => "rt_print",
                // Formatted in the language, then printed as the string.
                Ty::Float => {
                    let text = self.float_text("format", Ty::Str, a.val(), args.pos[0].span())?;
                    self.push(Inst::Call {
                        dst: None,
                        func: "rt_print_str".to_string(),
                        args: vec![text],
                    });
                    return Ok(Val::void());
                }
                Ty::Bool => "rt_print_bool",
                Ty::Str => "rt_print_str",
                Ty::Void => return Err(Diag::new(args.pos[0].span(), "cannot print a void value")),
                // `print(v)` is `v.to_str()`, and `bytes` has none: which
                // text a run of octets is -- hex, decoded UTF-8, escaped --
                // is the program's choice, not print's.
                Ty::Bytes => {
                    return Err(Diag::new(
                        args.pos[0].span(),
                        "cannot print `bytes`; say which text you mean, \
                         `hex()` or `utf8()`",
                    ))
                }
                // A user type says how it prints by having a `to_str`
                // method -- found by name, like `add` and `cmp`. Without
                // one, a refusal naming the method beats printing an
                // address.
                Ty::User(_) => {
                    // `to_str` is user code, with the value as its receiver.
                    let a = self.hold(&args.pos[0], a, true);
                    let Some(text) = self.call_to_str(&a, args.pos[0].span())? else {
                        return Err(Diag::new(
                            args.pos[0].span(),
                            format!(
                                "cannot print a value of type `{}`; give it a \
                                 method `str {}.to_str()`",
                                self.tyname(a.ty),
                                self.tyname(a.ty)
                            ),
                        ));
                    };
                    self.push(Inst::Call {
                        dst: None,
                        func: "rt_print_str".to_string(),
                        args: vec![text.val()],
                    });
                    return Ok(Val::void());
                }
            };
            self.push(Inst::Call {
                dst: None,
                func: f.to_string(),
                args: vec![a.val()],
            });
            return Ok(Val::void());
        }

        // `int(x)` and friends: convert a distinct value back to its base.
        // Same representation, so nothing is emitted.
        // `int(x)`, `float(x)`, `bool(x)`, `str(x)`.
        //
        // Two jobs behind one spelling: unwrapping a distinct type back to
        // its base, which is free and changes only identity, and converting
        // between the two number types, which is a real conversion. The
        // argument is lowered ONCE and then dispatched on, because lowering
        // it in each branch would evaluate `int(f())` twice.
        if let Some(base) = match name {
            "int" => Some(Ty::Int),
            "float" => Some(Ty::Float),
            "bool" => Some(Ty::Bool),
            "str" => Some(Ty::Str),
            "bytes" => Some(Ty::Bytes),
            _ => None,
        } {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(span, format!("`{name}` converts one value")));
            }
            let v = self.lower_expr(&args.pos[0])?;
            let from = self.underlying(v.ty);

            // Same representation: nothing is emitted.
            if from == base {
                return Ok(Val::new(v.val(), base, v.owned));
            }

            // Nothing here is implicit, so a mixed expression stays an error
            // the writer resolves rather than a silent widening.
            if base == Ty::Float && from == Ty::Int {
                let d = self.new_val(IrTy::F64);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_i2f_val".to_string(),
                    args: vec![v.val()],
                });
                return Ok(Val::new(d, Ty::Float, false));
            }
            // `str(v)` is `v.to_str()`, so the conversion family reads the
            // same whatever it is applied to -- a number, a bool, or a user
            // type that wrote the method itself.
            if base == Ty::Str {
                if matches!(from, Ty::Int | Ty::Float | Ty::Bool) {
                    let d = self.prim_to_str(from, v.val(), args.pos[0].span())?;
                    return Ok(Val::new(d, Ty::Str, true));
                }
                if from == Ty::Bytes {
                    return Err(Diag::new(
                        args.pos[0].span(),
                        "`str(..)` is `to_str`, which `bytes` does not have; \
                         say which text you mean, `hex()` or `utf8()`",
                    ));
                }
                // `to_str` is user code, with the value as its receiver.
                let held = self.hold(&args.pos[0], v, true);
                if let Some(text) = self.call_to_str(&held, args.pos[0].span())? {
                    return Ok(text);
                }
            }
            if base == Ty::Int && from == Ty::Float {
                // The C cast is UNDEFINED for a NaN or for a value outside
                // the integer range -- exactly the sort of thing gcc and
                // clang disagree about at -O2. Truncate toward zero, and
                // trap rather than take whatever the hardware felt like.
                let d = self.new_val(IrTy::I64);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_f2i_checked".to_string(),
                    args: vec![v.val()],
                });
                return Ok(Val::new(d, Ty::Int, false));
            }
            return Err(Diag::new(args.pos[0].span(), self.mismatch(base, v.ty)));
        }

        // `clone(x)` -- a SHALLOW copy. We chose reference types, so `=`
        // aliases; this is the explicit way to get a second object. Shallow
        // because a deep copy would have to decide what copying each field
        // means, which is a question only the program can answer.
        if name == "clone" {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(span, "`clone` takes one argument"));
            }
            let v = self.lower_expr(&args.pos[0])?;
            if self.seq_elem(v.ty).is_some() {
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_seq_clone".to_string(),
                    args: vec![v.val()],
                });
                self.stmt_temps.push(d);
                return Ok(Val::new(d, v.ty, true));
            }
            // A map too, so that every constant collection has the same way
            // out: `clone(TABLE)` is the copy that can be changed.
            if self.map_kv(v.ty).is_some() {
                let c = self.rt_value("rt_map_clone", vec![v.val()], v.ty);
                return Ok(Val::new(c.val(), v.ty, true));
            }
            if self.underlying(v.ty) == Ty::Str {
                // Immutable, so a copy is indistinguishable by value -- but
                // not by identity, and identity is what a thread boundary
                // cares about.
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_str_clone".to_string(),
                    args: vec![v.val()],
                });
                self.stmt_temps.push(d);
                return Ok(Val::new(d, v.ty, true));
            }
            // Mutable, so here the copy is observable by value too: writing
            // one leaves the other alone. Its bytes are not references, so
            // shallow and deep are the same copy.
            if self.underlying(v.ty) == Ty::Bytes {
                let c = self.rt_value("rt_bytes_clone", vec![v.val()], Ty::Bytes);
                return Ok(Val::new(c.val(), v.ty, true));
            }
            let Some(tid) = self.tdef_of(v.ty) else {
                return Err(Diag::new(
                    args.pos[0].span(),
                    format!(
                        "{} is copied by assignment; there is nothing to clone",
                        self.tyname(v.ty)
                    ),
                ));
            };
            let td = &self.typedefs[tid as usize];
            if td.is_interface || td.is_chan || td.is_distinct {
                return Err(Diag::new(
                    args.pos[0].span(),
                    format!("`{}` cannot be cloned", self.tyname(v.ty)),
                ));
            }
            // A value that owns a resource cannot be copied
            // (docs/destructors-decision.md): the copy would hold the same
            // descriptor, handle or slot, and whichever died first would
            // release it under the other. Only the type's OWN destructor
            // matters, not one it can reach: the clone is shallow, so a
            // `Log` holding an `io.File` clones into a second `Log` sharing
            // that one File, which is closed once, when the last goes.
            // No run-time check is needed behind this one: an interface
            // cannot be cloned at all, and a generic is concrete by here.
            if self.has_destructor(tid) {
                let t = self.tyname(v.ty);
                return Err(Diag::new(
                    args.pos[0].span(),
                    format!(
                        "`{t}` cannot be cloned: it owns a resource (it has a destructor), and \
                         a copy would release it a second time; share the reference \
                         instead (`=` aliases it), or give `{t}` a method that makes a real second resource"
                    ),
                ));
            }

            // An enum has no fields: what it carries is a tag and the
            // payload of whichever variant that tag names. The field loop
            // below would copy nothing and hand back a fresh object with tag
            // 0 -- a `Circle(7)` cloning into an `Empty`, silently. So an
            // enum is copied by an instruction of its own.
            if self.typedefs[tid as usize].is_enum {
                // A VALUE enum is copied by the assignment that binds the
                // result: there is no object, so there is no second object to
                // make and no identity to tell one from the other. `clone`
                // still means what it says -- writing one cannot be seen
                // through the other -- because an enum's payload cannot be
                // written at all.
                if self.typedefs[tid as usize].is_value {
                    return Ok(Val::new(v.val(), v.ty, false));
                }
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::EnumClone {
                    dst: d,
                    src: v.val(),
                    tid,
                });
                self.stmt_temps.push(d);
                return Ok(Val::new(d, v.ty, true));
            }

            // Field by field: the copy holds the same references, each
            // retained once more.
            let n = self.typedefs[tid as usize].fields.len();
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Alloc { dst: d, tid });
            for i in 0..n {
                let (_, fty) = self.typedefs[tid as usize].fields[i].clone();
                let cur = self.new_val(fty);
                self.push(Inst::LoadField {
                    dst: cur,
                    obj: v.val(),
                    tid,
                    idx: i as u32,
                });
                if fty == IrTy::Ref {
                    self.rc_inc(cur);
                }
                self.push(Inst::StoreField {
                    obj: d,
                    tid,
                    idx: i as u32,
                    val: cur,
                });
            }
            self.stmt_temps.push(d);
            return Ok(Val::new(d, v.ty, true));
        }

        // Channel builtins. They are here rather than in `sigs` because
        // their types depend on the channel's element type.
        if matches!(name, "send" | "recv" | "close") {
            return self.lower_chan_builtin(name, args, span);
        }

        let Some(sig) = self.sigs.get(name) else {
            if name == "len" {
                return Err(Diag::new(
                    span,
                    "there is no `len`; every collection and `str` answers \
                     `.size()`",
                ));
            }
            return Err(Diag::new(span, format!("unknown function `{name}`")));
        };
        let params = sig.params.clone();
        let module = sig.module.clone();
        let ret = sig.ret;
        let sig_is_prim = sig.is_prim;

        let slots = self.bind_args(name, &params, args, span)?;
        // A primitive or `concat` is the runtime and runs no user code; any
        // other function is the program's.
        let later = self.later_flags(&slots, &params, !(sig_is_prim || name == "concat"));

        let mut vals = Vec::new();
        for (i, (a, p)) in slots.iter().zip(params.iter()).enumerate() {
            let v = self.lower_slot(a, p, &module)?;
            if !self.assignable(v.ty, p.ty) {
                return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
            }
            // Arguments are borrowed (§5.1): no retain at the call site for
            // a local, a parameter or a temporary -- an owned temporary is
            // already on the statement's pending list, so it must NOT be
            // added again. Only a value read out of a place is held, and
            // only when something after it may run user code.
            let v = self.hold(a, v, later[i]);
            vals.push(v.val());
        }

        let rt_name = if sig_is_prim {
            // The seam's whole lowering rule: strip the module qualifier,
            // strip the reserved `__`, prefix `rt_`. No table of special
            // cases, and a runtime function nobody wrote is a link error
            // naming the exact symbol. See docs/stdlib-seam.md.
            format!(
                "rt_{}",
                crate::front::ast::bare(name).trim_start_matches('_')
            )
        } else {
            match name {
                "concat" => "rt_concat".to_string(),
                // The emitter escapes and prefixes; give it the raw name.
                other => other.to_string(),
            }
        };

        if ret == Ty::Void {
            self.push(Inst::Call {
                dst: None,
                func: rt_name,
                args: vals,
            });
            Ok(Val::void())
        } else {
            let d = self.new_val(self.irty(ret));
            self.push(Inst::Call {
                dst: Some(d),
                func: rt_name,
                args: vals,
            });
            // Returns are owned (§5.2).
            let owned = self.is_ref(ret);
            if owned {
                self.stmt_temps.push(d);
            }
            Ok(Val::new(d, ret, owned))
        }
    }
}
