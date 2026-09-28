//! Expression lowering: the `lower_expr` dispatch and what is under it
//! -- calls, `new`, binary operators, and their supporting helpers.

use super::{a_or_an, Lowerer, Val, BUILTIN_FNS};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{ArithOp, Cmp, Inst, IrTy, Term, Value};
use std::collections::HashMap;

impl Lowerer {
    /// Bind a call's arguments to a parameter list, by Oro's rule:
    /// **mandatory parameters are positional, optional ones are named.**
    /// Never both, so there is no question of which form to use and no
    /// question of what order optional arguments come in.
    ///
    /// Returns one expression per parameter, in declaration order, with
    /// defaults filled in.
    pub(super) fn bind_args<'a>(
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
    fn lower_static_call(&mut self, key: &str, args: &Args, span: Span) -> Result<Val, Diag> {
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
            // The method itself is user code (src/lower/hold.rs).
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

    fn lower_enum_new(
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
    fn lower_try(&mut self, inner: &Expr, span: Span) -> Result<Val, Diag> {
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
            // A `void` payload has been dropped (mono.rs), so an empty one
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
    fn call_to_str(&mut self, v: &Val, span: Span) -> Result<Option<Val>, Diag> {
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
    fn sibling_call(&mut self, name: &str, args: &Args, span: Span) -> Result<Option<Val>, Diag> {
        if self.recv.is_none() {
            return Ok(None);
        }
        let written = match self.shown.get(name) {
            Some((generic, _)) => crate::ast::bare(generic).to_string(),
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
    fn lower_method_on(
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
                let v = self.lower_expr_as(a, p.ty)?;
                if !self.assignable(v.ty, p.ty) {
                    return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
                }
                // The dispatch itself is user code (src/lower/hold.rs).
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
            // The method itself is user code (src/lower/hold.rs).
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
    fn lower_qualified(
        &mut self,
        modname: &str,
        name: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        let key = format!("{modname}#{name}");
        // A generic function arrives already instantiated, as `first$int`
        // (see mono.rs); the reader wrote `first`, so that is what a
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
    pub(super) fn lower_slot(&mut self, a: &Expr, p: &Param, module: &str) -> Result<Val, Diag> {
        let is_default = p.default.as_ref().is_some_and(|d| std::ptr::eq(d, a));
        if !is_default {
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
        let r = self.lower_expr_as(a, p.ty).and_then(|v| {
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

    /// Lower an expression where the wanted type is known.
    ///
    /// Collection literals have no type of their own -- `[]` says nothing --
    /// so they are legal only where something says what they should be: a
    /// declaration, an assignment, a field, an argument, an enum payload, a
    /// return. Everywhere else the literal forms are refused with a message
    /// saying so, rather than guessing.
    ///
    /// A function's name is the other expression with no type of its own, and
    /// it is a value in exactly the places this function is reached from --
    /// which is the table under "Where the target *is* known" in
    /// docs/closures-decision.md, one row per call site below.
    pub(super) fn lower_expr_as(&mut self, e: &Expr, want: Ty) -> Result<Val, Diag> {
        if let Some(v) = self.lower_fn_ref(e, want)? {
            return Ok(v);
        }
        // A lambda is the same thing unnamed, and is a value in exactly the
        // same places, for the same reason: the target is written down here.
        if let Some(v) = self.lower_lambda(e, want)? {
            return Ok(v);
        }
        match e {
            Expr::SeqLit(..) | Expr::RepeatLit(..) | Expr::MapLit(..) => {
                self.lower_literal(e, Some(want))
            }
            _ => self.lower_expr(e),
        }
    }

    /// `[]`, `[a, b, c]`, `[x; n]`, `{}`, `{k: v}`.
    fn lower_literal(&mut self, e: &Expr, want: Option<Ty>) -> Result<Val, Diag> {
        let span = e.span();
        let Some(want) = want else {
            return Err(Diag::new(
                span,
                "there is nothing here to say what this should be; a collection \
                 literal takes its type from where it is written",
            ));
        };

        // A literal written where a distinct collection is wanted builds the
        // base and takes the distinct identity. That is not the implicit
        // conversion `Price p = 5` is refused for: `5` already has a type,
        // `int`, and would have to change it, while a collection literal has
        // no type at all until the place it is written gives it one.
        if let Some(base) = self.base_of(want) {
            let v = self.lower_literal(e, Some(base))?;
            return Ok(Val::new(v.val(), want, v.owned));
        }

        // A Map wants `{..}`; an Array or List wants `[..]`.
        if let Some((k, v)) = self.map_kv(want) {
            let Expr::MapLit(items, _) = e else {
                return Err(Diag::new(
                    span,
                    format!(
                        "{} is a map; write its entries as {{k: v}}",
                        self.tyname(want)
                    ),
                ));
            };
            self.building_literal = true;
            let m = self.lower_new(want, &Args::default(), span);
            self.building_literal = false;
            let m = m?;
            for (ke, ve) in items {
                let kv = self.lower_expr_as(ke, k)?;
                if !self.assignable(kv.ty, k) {
                    return Err(Diag::new(ke.span(), self.mismatch(k, kv.ty)));
                }
                // The map retains the key only once the value is lowered.
                let kv = self.hold(ke, kv, self.may_run_code(ve, true));
                let vv = self.lower_expr_as(ve, v)?;
                if !self.assignable(vv.ty, v) {
                    return Err(Diag::new(ve.span(), self.mismatch(v, vv.ty)));
                }
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_map_set".to_string(),
                    args: vec![m.val(), kv.val(), vv.val()],
                });
            }
            return Ok(m);
        }

        if want == Ty::Bytes {
            return self.bytes_literal(e);
        }

        let Some(elem) = self.seq_elem(want) else {
            return Err(Diag::new(
                span,
                format!("{} is not a collection", self.tyname(want)),
            ));
        };
        if matches!(e, Expr::MapLit(..)) {
            return Err(Diag::new(
                span,
                format!("{} holds elements; write them as [a, b]", self.tyname(want)),
            ));
        }
        let is_list = self.is_list(want);

        // `[x; n]` -- n copies, built at that length by the runtime for
        // either kind. A List used to be filled by pushing in a loop, which
        // made a negative `n` an empty list and a huge one a process that
        // pushed until it was killed; one call checks `n` the way an Array
        // does, and sizes the buffer once.
        if let Expr::RepeatLit(ve, ne, _) = e {
            let v = self.lower_expr_as(ve, elem)?;
            if !self.assignable(v.ty, elem) {
                return Err(Diag::new(ve.span(), self.mismatch(elem, v.ty)));
            }
            // The runtime retains the fill only once the length is lowered.
            let v = self.hold(ve, v, self.may_run_code(ne, true));
            let n = self.lower_expr(ne)?;
            if self.underlying(n.ty) != Ty::Int {
                return Err(Diag::new(ne.span(), self.mismatch(Ty::Int, n.ty)));
            }
            return Ok(self.build_repeat(want, elem, v, n.val(), is_list));
        }

        let Expr::SeqLit(items, _) = e else {
            unreachable!("only the three literal forms reach here")
        };
        // Every element is lowered before any is stored.
        let later = self.later_in(items);
        let mut vals = Vec::new();
        for (it, later) in items.iter().zip(later) {
            let v = self.lower_expr_as(it, elem)?;
            if !self.assignable(v.ty, elem) {
                return Err(Diag::new(it.span(), self.mismatch(elem, v.ty)));
            }
            vals.push(self.hold(it, v, later));
        }
        self.build_seq(want, elem, vals, is_list, span)
    }

    /// `[]`, `[104, 105]` and `[0; n]` where a `bytes` is wanted.
    ///
    /// The literal is the List literal, because a `bytes` is written the way
    /// a sequence of ints is: there is no second spelling to learn, and every
    /// place that gives a List literal its type gives this one its type too.
    /// There is no `b"..."`. A literal of a MUTABLE type cannot be one shared
    /// immortal object the way a string literal is, so it would allocate
    /// every time it is evaluated while looking like a constant; text that
    /// should become bytes says so, `"GET ".to_bytes()`, and the allocation
    /// is visible where it happens.
    fn bytes_literal(&mut self, e: &Expr) -> Result<Val, Diag> {
        match e {
            Expr::MapLit(_, span) => {
                Err(Diag::new(*span, "bytes holds octets; write them as [a, b]"))
            }
            Expr::RepeatLit(ve, ne, _) => {
                self.check_byte_literal(ve)?;
                let v = self.arg_of(ve, Ty::Int)?;
                let n = self.lower_expr(ne)?;
                if self.underlying(n.ty) != Ty::Int {
                    return Err(Diag::new(ne.span(), self.mismatch(Ty::Int, n.ty)));
                }
                Ok(self.rt_value("rt_bytes_fill", vec![n.val(), v], Ty::Bytes))
            }
            Expr::SeqLit(items, _) => {
                // Sized once for what is written, then filled; each push
                // checks its value, so a computed element out of range traps
                // exactly as `b.push(v)` would.
                let cap = self.new_val(IrTy::I64);
                self.push(Inst::IConst {
                    dst: cap,
                    val: items.len() as i64,
                });
                let b = self.rt_value("rt_bytes_new", vec![cap], Ty::Bytes);
                for it in items {
                    self.check_byte_literal(it)?;
                    let v = self.arg_of(it, Ty::Int)?;
                    self.rt_void("rt_bytes_push", vec![b.val(), v]);
                }
                Ok(b)
            }
            _ => unreachable!("only the three literal forms reach here"),
        }
    }

    /// A List or an Array of `n` copies of `fill`, straight from the
    /// runtime, which retains a reference fill once per slot.
    fn build_repeat(&mut self, ty: Ty, elem: Ty, fill: Val, n: Value, is_list: bool) -> Val {
        let flag = self.new_val(IrTy::I1);
        let refs = self.is_ref(elem);
        self.push(Inst::BConst {
            dst: flag,
            val: refs,
        });
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Call {
            dst: Some(d),
            func: if is_list {
                "rt_list_repeat"
            } else {
                "rt_array_new"
            }
            .to_string(),
            args: vec![n, fill.val(), flag],
        });
        self.stmt_temps.push(d);
        Val::new(d, ty, true)
    }

    /// `[a, b, c]` for a List or an Array. An Array is sized by its elements.
    fn build_seq(
        &mut self,
        ty: Ty,
        elem: Ty,
        vals: Vec<Val>,
        is_list: bool,
        span: Span,
    ) -> Result<Val, Diag> {
        let refs = self.is_ref(elem);
        let d = self.new_val(IrTy::Ref);
        if is_list {
            let flag = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: flag,
                val: refs,
            });
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_list_new".to_string(),
                args: vec![flag],
            });
        } else {
            // An array is allocated at its length with no fill to retain,
            // then each slot is written.
            let n = self.new_val(IrTy::I64);
            self.push(Inst::IConst {
                dst: n,
                val: vals.len() as i64,
            });
            let flag = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: flag,
                val: refs,
            });
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_array_blank".to_string(),
                args: vec![n, flag],
            });
        }
        self.stmt_temps.push(d);
        let out = Val::new(d, ty, true);

        for (i, v) in vals.iter().enumerate() {
            // The collection takes a reference, exactly as push or an index
            // assignment would.
            if refs {
                if v.owned {
                    self.stmt_temps.retain(|t| *t != v.val());
                } else {
                    self.rc_inc(v.val());
                }
            }
            if is_list {
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_list_push".to_string(),
                    args: vec![d, v.val()],
                });
            } else {
                let idx = self.new_val(IrTy::I64);
                self.push(Inst::IConst {
                    dst: idx,
                    val: i as i64,
                });
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_array_put".to_string(),
                    args: vec![d, idx, v.val()],
                });
            }
        }
        let _ = span;
        Ok(out)
    }

    pub(super) fn lower_expr(&mut self, e: &Expr) -> Result<Val, Diag> {
        match e {
            Expr::Float(x, _) => {
                let v = self.new_val(IrTy::F64);
                self.push(Inst::FConst { dst: v, val: *x });
                Ok(Val::new(v, Ty::Float, false))
            }
            Expr::Int(n, _) => {
                let v = self.new_val(IrTy::I64);
                self.push(Inst::IConst { dst: v, val: *n });
                Ok(Val::new(v, Ty::Int, false))
            }
            Expr::Bool(b, _) => {
                let v = self.new_val(IrTy::I1);
                self.push(Inst::BConst { dst: v, val: *b });
                Ok(Val::new(v, Ty::Bool, false))
            }
            Expr::Str(s, _) => {
                let idx = match self.strings.iter().position(|x| x == s) {
                    Some(i) => i as u32,
                    None => {
                        self.strings.push(s.clone());
                        (self.strings.len() - 1) as u32
                    }
                };
                let v = self.new_val(IrTy::Ref);
                self.push(Inst::SConst { dst: v, idx });
                // Immortal: borrowed, never owned. docs/ir-v0.md §5.4
                Ok(Val::new(v, Ty::Str, false))
            }
            Expr::This(span) => self.this_val(*span),
            // A lambda somewhere nothing says what type is wanted. Like a
            // function's name, it is checked against a target and is never
            // a source of inference, so with no target there is nothing to
            // check it against and not even a method name to give it.
            Expr::Lambda(_, _, span) => Err(Self::lambda_no_target(*span)),
            Expr::Var(name, span) => {
                if self.moved.iter().any(|n| n == name) {
                    return Err(Diag::new(
                        *span,
                        format!("`{name}` was moved and cannot be used again"),
                    ));
                }
                if let Some((ty, v)) = self.lookup(name) {
                    return Ok(Val::new(v, ty, false));
                }
                // Inside a method, a bare name may be a field of the
                // receiver. Unambiguous because nothing shadows anything.
                if let Some((tid, obj, path)) = self.recv_field(name) {
                    // A method is in its type's module, so only a field
                    // promoted from another module's type can be private.
                    self.check_field_access(tid, &path, name, *span)?;
                    let (d, fty) = self.load_path(tid, obj, &path);
                    // Borrowed from the receiver, which holds the +1.
                    return Ok(Val::new(d, fty, false));
                }
                // Last, a module constant. The order cannot matter -- no
                // local, parameter or field may take a constant's name
                // (`check_shadow`) -- so this is only the cheapest order.
                if let Some(key) = self.resolve_const(name) {
                    return Ok(self.lower_const_use(&key));
                }
                if let Some(d) = self.foreign_const_hint(name, *span) {
                    return Err(d);
                }
                // A function's name, somewhere nothing says what type is
                // wanted: an argument to an unconstrained generic parameter,
                // an argument to a builtin, an expression statement. It is
                // checked against the interface it is given to and is never
                // a source of inference, so with no target there is nothing
                // to check it against (docs/closures-decision.md, "Where the
                // target is *not* known").
                if let Some(d) = self.fn_ref_no_target(e) {
                    return Err(d);
                }
                Err(self.unknown_variable(name, *span))
            }
            Expr::Un(op, inner, span) => {
                let a = self.lower_expr(inner)?;
                match op {
                    UnOp::Neg if self.underlying(a.ty) == Ty::Float => {
                        // Multiply by -1.0, NOT `0.0 - x`. Subtraction gets
                        // zero wrong: IEEE says 0.0 - 0.0 is +0.0, so `-0.0`
                        // would come out positive. Multiplication flips the
                        // sign bit in every case, zeroes and infinities
                        // included.
                        let m = self.new_val(IrTy::F64);
                        self.push(Inst::FConst { dst: m, val: -1.0 });
                        let d = self.new_val(IrTy::F64);
                        self.push(Inst::Arith {
                            dst: d,
                            op: ArithOp::Mul,
                            lhs: a.val(),
                            rhs: m,
                        });
                        Ok(Val::new(d, Ty::Float, false))
                    }
                    UnOp::Neg => {
                        if a.ty != Ty::Int {
                            return Err(Diag::new(
                                *span,
                                format!("cannot negate a value of type {}", self.tyname(a.ty)),
                            ));
                        }
                        // Lower as 0 - x so the overflow check is shared; this
                        // is also what makes -INT64_MIN trap rather than wrap.
                        let z = self.new_val(IrTy::I64);
                        self.push(Inst::IConst { dst: z, val: 0 });
                        let d = self.new_val(IrTy::I64);
                        self.push(Inst::Arith {
                            dst: d,
                            op: ArithOp::Sub,
                            lhs: z,
                            rhs: a.val(),
                        });
                        Ok(Val::new(d, Ty::Int, false))
                    }
                    UnOp::BitNot => {
                        // Held to `int` alone, like unary minus: a `bool` is
                        // not an integer, and a float has no bit operations.
                        if a.ty != Ty::Int {
                            return Err(Diag::new(
                                *span,
                                format!(
                                    "cannot apply `~` to a value of type {}",
                                    self.tyname(a.ty)
                                ),
                            ));
                        }
                        // `~x` is `x ^ -1`: one bit operation fewer to carry
                        // through the IR and the runtime, and the identity is
                        // exact in two's complement.
                        let ones = self.new_val(IrTy::I64);
                        self.push(Inst::IConst { dst: ones, val: -1 });
                        let d = self.new_val(IrTy::I64);
                        self.push(Inst::Arith {
                            dst: d,
                            op: ArithOp::Xor,
                            lhs: a.val(),
                            rhs: ones,
                        });
                        Ok(Val::new(d, Ty::Int, false))
                    }
                    UnOp::Not => {
                        if a.ty != Ty::Bool {
                            return Err(Diag::new(
                                *span,
                                format!(
                                    "cannot apply `!` to a value of type {}",
                                    self.tyname(a.ty)
                                ),
                            ));
                        }
                        let d = self.new_val(IrTy::I1);
                        self.push(Inst::Not {
                            dst: d,
                            src: a.val(),
                        });
                        Ok(Val::new(d, Ty::Bool, false))
                    }
                }
            }
            Expr::Bin(op, l, r, span) => self.lower_bin(*op, l, r, *span),
            Expr::Call(name, args, span) => {
                // Inside an instance method, a bare call may name a method of
                // the receiver -- its sibling -- exactly as a bare name may
                // name one of its fields.
                if let Some(v) = self.sibling_call(name, args, *span)? {
                    return Ok(v);
                }
                // `order(a, b)` where `order` is a value. Now that a callback
                // can be written down this is the likeliest mistake a
                // newcomer makes, and "unknown function `order`" is no help:
                // the name IS in scope, and a callback is called the way
                // every interface value is, through its method.
                if let Some(d) = self.call_of_a_value(name, *span) {
                    return Err(d);
                }
                // A bare name means this module's declaration, then a
                // builtin. Another module's name is not in scope at all --
                // which is what makes `pub` mean something once every file
                // has been merged into one program. The check lives here
                // rather than in `lower_call`, because the qualified form
                // goes through the same function and has already earned its
                // access.
                let key = self.resolve_fn(name);
                // Not ours: if some other module declares it, say which and
                // how to reach it rather than "unknown function". Never for
                // a builtin, which is in scope everywhere whatever other
                // modules declare -- only some builtins are in `sigs`, so
                // the lookup alone cannot tell.
                if !self.sigs.contains_key(&key) && !BUILTIN_FNS.contains(&name.as_str()) {
                    let suffix = format!("#{name}");
                    if let Some((k, sig)) = self
                        .sigs
                        .iter()
                        .find(|(k, _)| k.ends_with(&suffix) && !k.contains('.'))
                    {
                        let owner = sig.module.clone();
                        let _ = k;
                        return Err(Diag::new(
                            *span,
                            if sig.is_pub {
                                format!(
                                    "`{name}` is declared in `{owner}`; write `{owner}.{name}`{}",
                                    self.and_import(&owner)
                                )
                            } else {
                                format!("`{name}` is private to `{owner}`")
                            },
                        ));
                    }
                }
                if let Some(sig) = self.sigs.get(&key) {
                    if !sig.module.is_empty() && sig.module != self.cur_module {
                        return Err(Diag::new(
                            *span,
                            if sig.is_pub {
                                format!(
                                    "`{name}` is declared in `{}`; write `{}.{name}`{}",
                                    sig.module,
                                    sig.module,
                                    self.and_import(&sig.module)
                                )
                            } else {
                                format!("`{name}` is private to `{}`", sig.module)
                            },
                        ));
                    }
                }
                self.lower_call(&key, args, *span)
            }

            Expr::Field(obj, field, span) => {
                if let Expr::This(ts) = &**obj {
                    self.refuse_this_field(field, *ts, *span)?;
                }
                // `lib.MAX` -- another module's constant, not a field of a
                // variable called `lib`.
                if let Some(key) = self.qualified_const(obj, field, *span)? {
                    return Ok(self.lower_const_use(&key));
                }
                // `lib.by_x` -- another module's function, named as a value
                // where nothing says an interface is wanted. Same refusal as
                // the bare spelling, rather than "unknown variable `lib`".
                if let Some(d) = self.fn_ref_no_target(e) {
                    return Err(d);
                }
                let o = self.lower_expr(obj)?;
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("type {} has no fields", self.tyname(o.ty)),
                    ));
                };
                if !self.type_visible(tid) {
                    return Err(Diag::new(
                        *span,
                        self.not_visible(tid, "its fields cannot be read from here"),
                    ));
                }
                let Some(path) = self.field_path(tid, field) else {
                    return Err(Diag::new(
                        *span,
                        format!("type `{}` has no field `{field}`", self.tyname(o.ty)),
                    ));
                };
                self.check_field_access(tid, &path, field, *span)?;
                let (d, fty) = self.load_path(tid, o.val(), &path);
                // A field read is BORROWED from the object, exactly like a
                // local: the object holds the +1, we do not.
                Ok(Val::new(d, fty, false))
            }

            Expr::MethodCall(obj, m, args, span) => {
                // `greet.hello(..)` -- a module qualifier, not a receiver.
                // Checked before lowering the "receiver", because there is
                // no value to lower.
                if let Expr::Var(modname, _) = &**obj {
                    if self.module_in_scope(modname) && self.lookup(modname).is_none() {
                        return self.lower_qualified(modname, m, args, *span);
                    }
                }
                if let Expr::This(ts) = &**obj {
                    self.this_val(*ts)?;
                    let (rtid, _) = self.recv.expect("this_val checked the receiver");
                    self.refuse_destructor_call(rtid, m, *span)?;
                    if self.sibling_method(m).is_some() {
                        // `m` may be a generic method's instantiation, which
                        // monomorphisation renamed; say it as it was written.
                        let m = match self.shown.get(m) {
                            Some((generic, _)) => crate::ast::bare(generic).to_string(),
                            None => m.clone(),
                        };
                        return Err(Diag::new(
                            *span,
                            format!(
                                "write `{m}(..)`, not `this.{m}(..)`: a method of the \
                                 receiver is called by its bare name, as a field is read"
                            ),
                        ));
                    }
                }
                let o = self.lower_expr(obj)?;
                // `TABLE.sort()`, `xs.push(1)` on a const local: a change the
                // compiler can see, refused here. One it cannot see -- the
                // same table reached through a parameter -- traps at run
                // time instead (runtime/rt.c, `rt_check_mutable`).
                if self.is_mutating_method(o.ty, m) {
                    self.refuse_const_write(obj, *span)?;
                }
                // The receiver is an argument like any other (§5.1): read
                // out of a place, it is held across the arguments and the
                // call when either may run user code -- `h.p.m(..)` whose
                // body replaces `h.p`, or `h.ps.push(f(h))` where `f` does.
                let later = self.method_runs_code(o.ty, m)
                    || args.pos.iter().any(|a| self.may_run_code(a, true))
                    || args.named.iter().any(|(_, a)| self.may_run_code(a, true));
                let o = self.hold(obj, o, later);
                self.lower_method_on(&o, m, args, *span)
            }

            Expr::Index(obj, idx, span) => {
                let o = self.lower_expr(obj)?;
                // `h.ps[f(h)]`: the index can replace the collection before
                // it is read from.
                let o = self.hold(obj, o, self.may_run_code(idx, true));
                // A byte reads as an int, 0 to 255: the language has one
                // integer type, and a byte is a value of it rather than a
                // second kind of number.
                if self.underlying(o.ty) == Ty::Bytes {
                    let i = self.index_of_bytes(idx)?;
                    return Ok(self.rt_value("rt_bytes_get", vec![o.val(), i], Ty::Int));
                }
                let Some(elem) = self.seq_elem(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("{} cannot be indexed", self.tyname(o.ty)),
                    ));
                };
                let i = self.lower_expr(idx)?;
                if self.underlying(i.ty) != Ty::Int {
                    return Err(Diag::new(idx.span(), self.mismatch(Ty::Int, i.ty)));
                }
                let d = self.new_val(self.irty(elem));
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_index_get".to_string(),
                    args: vec![o.val(), i.val()],
                });
                // Borrowed from the collection, which holds the +1 -- the
                // same rule as reading a field.
                Ok(Val::new(d, elem, false))
            }

            Expr::New(ty, args, span) => self.lower_new(*ty, args, *span),
            Expr::EnumNew(ty, variant, args, span) => {
                self.lower_enum_new(*ty, variant, args, *span)
            }
            Expr::Try(inner, span) => self.lower_try(inner, *span),
            Expr::SeqLit(..) | Expr::RepeatLit(..) | Expr::MapLit(..) => {
                self.lower_literal(e, None)
            }
        }
    }

    /// The method an operator desugars to, and whether the result is negated.
    ///
    /// Comparison goes through a single `cmp` returning an int, rather than
    /// four separate methods: one implementation gives a total order, and it
    /// cannot be made inconsistent by defining `<` and `>=` differently.
    fn op_method(op: BinOp) -> Option<(&'static str, bool)> {
        use BinOp::*;
        Some(match op {
            Add => ("add", false),
            Sub => ("sub", false),
            Mul => ("mul", false),
            Div => ("div", false),
            Rem => ("rem", false),
            Eq => ("eq", false),
            Ne => ("eq", true),
            Lt | Le | Gt | Ge => ("cmp", false),
            // The bit operators are not in the overloadable set: they are
            // defined on the bits of an `int`, and a user type has no bits
            // to speak of until it says what they are, which is a method.
            And | Or | BitAnd | BitOr | BitXor | Shl | Shr => return None,
        })
    }

    /// `a OP b` where `a` is a user type: dispatch to the operator's method.
    fn lower_op_overload(&mut self, op: BinOp, a: &Val, b: &Val, span: Span) -> Result<Val, Diag> {
        use BinOp::*;
        let Some((mname, negate)) = Self::op_method(op) else {
            return Err(Diag::new(
                span,
                format!("`{}` cannot be overloaded", op.spelling()),
            ));
        };
        let tid = self.tdef_of(a.ty).expect("checked by caller");
        let tname = self.typedefs[tid as usize].name.clone();
        let shown = self.show_name(&tname);
        let key = format!("{tname}.{mname}");

        if self.sigs.contains_key(&key) {
            self.check_method_access(tid, mname, span)?;
        }
        let Some(sig) = self.sigs.get(&key) else {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` on `{shown}` needs a method `{} {shown}.{mname}(..)`",
                    op.spelling(),
                    if mname == "cmp" {
                        "int"
                    } else if mname == "eq" {
                        "bool"
                    } else {
                        &shown
                    }
                ),
            ));
        };
        let (params, ret) = (sig.params.clone(), sig.ret);
        if params.len() != 1 || params[0].ty != b.ty {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` must take one {} parameter to support `{}`",
                    self.bare_name(&key),
                    self.tyname(a.ty),
                    op.spelling()
                ),
            ));
        }

        let want_ret = match mname {
            "cmp" => Ty::Int,
            "eq" => Ty::Bool,
            _ => a.ty,
        };
        if ret != want_ret {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` must return {} to support `{}`",
                    self.bare_name(&key),
                    self.tyname(want_ret),
                    op.spelling()
                ),
            ));
        }

        let d = self.new_val(self.irty(ret));
        self.push(Inst::Call {
            dst: Some(d),
            func: key,
            args: vec![a.val(), b.val()],
        });
        if self.is_ref(ret) {
            self.stmt_temps.push(d);
        }

        match mname {
            "cmp" => {
                // `a < b` is `a.cmp(b) < 0`.
                let zero = self.new_val(IrTy::I64);
                self.push(Inst::IConst { dst: zero, val: 0 });
                let cmp = match op {
                    Lt => Cmp::Lt,
                    Le => Cmp::Le,
                    Gt => Cmp::Gt,
                    Ge => Cmp::Ge,
                    _ => unreachable!(),
                };
                let out = self.new_val(IrTy::I1);
                self.push(Inst::ICmp {
                    dst: out,
                    cmp,
                    lhs: d,
                    rhs: zero,
                });
                Ok(Val::new(out, Ty::Bool, false))
            }
            "eq" if negate => {
                let out = self.new_val(IrTy::I1);
                self.push(Inst::Not { dst: out, src: d });
                Ok(Val::new(out, Ty::Bool, false))
            }
            _ => Ok(Val::new(d, ret, self.is_ref(ret))),
        }
    }

    fn lower_bin(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> Result<Val, Diag> {
        use BinOp::*;

        // && and || short-circuit, so they are control flow, not arithmetic.
        if matches!(op, And | Or) {
            let a = self.lower_expr(l)?;
            if a.ty != Ty::Bool {
                return Err(Diag::new(
                    l.span(),
                    format!("type mismatch: expected bool, found {}", self.tyname(a.ty)),
                ));
            }
            let rhs_bb = self.new_block();
            let join_bb = self.new_block();
            let short = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: short,
                val: op == Or,
            });

            if op == And {
                self.terminate(Term::Brif {
                    cond: a.val(),
                    then: rhs_bb,
                    then_args: Vec::new(),
                    els: join_bb,
                    els_args: vec![short],
                });
            } else {
                self.terminate(Term::Brif {
                    cond: a.val(),
                    then: join_bb,
                    then_args: vec![short],
                    els: rhs_bb,
                    els_args: Vec::new(),
                });
            }

            self.switch_to(rhs_bb);
            let mark = self.stmt_temps.len();
            let b = self.lower_expr(r)?;
            if b.ty != Ty::Bool {
                return Err(Diag::new(
                    r.span(),
                    format!("type mismatch: expected bool, found {}", self.tyname(b.ty)),
                ));
            }
            // Release what the operand allocated before leaving its block.
            self.flush_temps_since(mark);
            let rhs_end = self.blocks[self.cur].id;
            self.switch_to(rhs_end);
            self.terminate(Term::Jump {
                to: join_bb,
                args: vec![b.val()],
            });

            let p = self.new_val(IrTy::I1);
            let ji = self.blocks.iter().position(|x| x.id == join_bb).unwrap();
            self.blocks[ji].params = vec![p];
            self.switch_to(join_bb);
            return Ok(Val::new(p, Ty::Bool, false));
        }

        // An operator method is user code, with `a` as its receiver and `b`
        // as its argument; a built-in operator is the runtime, so then only
        // what the right operand runs can disturb the left (`h.s + f(h)`).
        let a = self.lower_expr(l)?;
        let user_op = self.has_operator_methods(a.ty);
        let a = self.hold(l, a, user_op || self.may_run_code(r, true));
        let b = self.lower_expr(r)?;
        let b = self.hold(r, b, user_op);

        // str has built-in `+` and `==`; they are the two everyone reaches
        // for, and making them methods on a builtin would need no less code.
        if a.ty == Ty::Str && b.ty == Ty::Str {
            if op == Add {
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_concat".to_string(),
                    args: vec![a.val(), b.val()],
                });
                self.stmt_temps.push(d);
                return Ok(Val::new(d, Ty::Str, true));
            }
            if op == Eq || op == Ne {
                let d = self.new_val(IrTy::I1);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_str_eq".to_string(),
                    args: vec![a.val(), b.val()],
                });
                if op == Ne {
                    let out = self.new_val(IrTy::I1);
                    self.push(Inst::Not { dst: out, src: d });
                    return Ok(Val::new(out, Ty::Bool, false));
                }
                return Ok(Val::new(d, Ty::Bool, false));
            }
        }

        // `bytes` compares by value, like `str`: two buffers holding the same
        // octets are equal. It has no `+` -- appending is `extend`, in place,
        // which is what a buffer is for -- and no ordering, as `str` has none.
        if a.ty == Ty::Bytes && b.ty == Ty::Bytes && (op == Eq || op == Ne) {
            let d = self.rt_value("rt_bytes_eq", vec![a.val(), b.val()], Ty::Bool);
            if op == Ne {
                let out = self.new_val(IrTy::I1);
                self.push(Inst::Not {
                    dst: out,
                    src: d.val(),
                });
                return Ok(Val::new(out, Ty::Bool, false));
            }
            return Ok(d);
        }

        // A distinct type behaves exactly as its base -- it IS an int -- so
        // arithmetic and comparison work, and the result keeps the distinct
        // type. Mixing with the base needs an explicit conversion, which is
        // the point: Price + Price is a Price, Price + int is a mistake.
        if self.base_of(a.ty).is_some() && a.ty == b.ty {
            let u = self.underlying(a.ty);
            let inner = self.lower_bin_prim(op, u, &a, &b, span)?;
            let out_ty = if inner.ty == Ty::Bool { Ty::Bool } else { a.ty };
            return Ok(Val::new(inner.val(), out_ty, false));
        }

        // A distinct type mixed with something else. Blaming a missing
        // operator method would be misleading -- the type has the operator,
        // it is the operands that disagree.
        if self.base_of(a.ty).is_some() || self.base_of(b.ty).is_some() {
            return Err(Diag::new(
                span,
                format!(
                    "cannot apply `{}` to {} and {}; convert one of them",
                    op.spelling(),
                    self.tyname(a.ty),
                    self.tyname(b.ty)
                ),
            ));
        }

        // A user type on the left: dispatch to the operator's method.
        if matches!(a.ty, Ty::User(_)) {
            return self.lower_op_overload(op, &a, &b, span);
        }

        self.lower_bin_prim(op, a.ty, &a, &b, span)
    }

    /// Arithmetic and comparison on primitives, shared by `int` and by any
    /// distinct type whose base is a primitive.
    fn lower_bin_prim(
        &mut self,
        op: BinOp,
        ty: Ty,
        a: &Val,
        b: &Val,
        span: Span,
    ) -> Result<Val, Diag> {
        use BinOp::*;
        let bits = match op {
            BitAnd => Some(ArithOp::And),
            BitOr => Some(ArithOp::Or),
            BitXor => Some(ArithOp::Xor),
            Shl => Some(ArithOp::Shl),
            Shr => Some(ArithOp::Shr),
            _ => None,
        };
        if let Some(bop) = bits {
            if a.ty != b.ty {
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}; convert one of them",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            // `int` only. A float has a bit pattern, but `&` on one would
            // be a truncation or a reinterpretation, and neither should be
            // spelled as an operator -- `to_bits()` says which it is. A
            // `bool` is not an integer here, and the operator it wanted
            // has its own spelling.
            if ty != Ty::Int {
                let hint = match (ty, op) {
                    (Ty::Float, _) => "; bit operations apply only to int".to_string(),
                    (Ty::Bool, BitAnd) => "; for bool use `&&`".to_string(),
                    (Ty::Bool, BitOr) => "; for bool use `||`".to_string(),
                    (Ty::Bool, BitXor) => "; for bool use `!=`".to_string(),
                    _ => String::new(),
                };
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}{hint}",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            let d = self.new_val(IrTy::I64);
            self.push(Inst::Arith {
                dst: d,
                op: bop,
                lhs: a.val(),
                rhs: b.val(),
            });
            return Ok(Val::new(d, Ty::Int, false));
        }
        let arith = match op {
            Add => Some(ArithOp::Add),
            Sub => Some(ArithOp::Sub),
            Mul => Some(ArithOp::Mul),
            Div => Some(ArithOp::Div),
            Rem => Some(ArithOp::Rem),
            _ => None,
        };

        if let Some(aop) = arith {
            // Both operands, not just the left one. This was never checked:
            // the type came from the left operand alone, so `1 + true` was
            // accepted and printed 2, and once float arrived `1.5 + 2` was
            // accepted and silently widened. Nothing here is implicit.
            if a.ty != b.ty {
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}; convert one of them",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            // Float arithmetic is IEEE: it does not trap, it produces an
            // infinity or a NaN. Integer arithmetic traps. Two number types,
            // two honest answers -- and `%` is left off floats the way Go
            // leaves it off, because fmod is a different operation wearing
            // the same spelling.
            if ty == Ty::Float {
                if op == Rem {
                    return Err(Diag::new(
                        span,
                        "`%` is integer remainder; it does not apply to float",
                    ));
                }
                let d = self.new_val(IrTy::F64);
                self.push(Inst::Arith {
                    dst: d,
                    op: aop,
                    lhs: a.val(),
                    rhs: b.val(),
                });
                return Ok(Val::new(d, Ty::Float, false));
            }
            if ty != Ty::Int {
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            let d = self.new_val(IrTy::I64);
            self.push(Inst::Arith {
                dst: d,
                op: aop,
                lhs: a.val(),
                rhs: b.val(),
            });
            return Ok(Val::new(d, Ty::Int, false));
        }

        let cmp = match op {
            Eq => Cmp::Eq,
            Ne => Cmp::Ne,
            Lt => Cmp::Lt,
            Le => Cmp::Le,
            Gt => Cmp::Gt,
            Ge => Cmp::Ge,
            _ => unreachable!(),
        };
        if a.ty != b.ty {
            return Err(Diag::new(
                span,
                format!(
                    "cannot compare {} with {}",
                    self.tyname(a.ty),
                    self.tyname(b.ty)
                ),
            ));
        }
        if ty != Ty::Int && ty != Ty::Bool && ty != Ty::Float {
            return Err(Diag::new(
                span,
                format!("cannot compare values of type {}", self.tyname(a.ty)),
            ));
        }
        if ty == Ty::Bool && !matches!(op, Eq | Ne) {
            return Err(Diag::new(span, "bool supports only `==` and `!=`"));
        }
        let d = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: d,
            cmp,
            lhs: a.val(),
            rhs: b.val(),
        });
        Ok(Val::new(d, Ty::Bool, false))
    }

    fn lower_new(&mut self, ty: Ty, args: &Args, span: Span) -> Result<Val, Diag> {
        let Some(tid) = self.tdef_of(ty) else {
            return Err(Diag::new(
                span,
                format!("unknown type `{}`", self.tyname(ty)),
            ));
        };
        // Only a qualified construction, `lib.Secret(..)`, can name a type
        // from another module here, so this is where its privacy is kept.
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "it cannot be constructed from here"),
            ));
        }
        if self.typedefs[tid as usize].is_interface {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` is an interface; construct a type that satisfies it",
                    self.show_name(&self.typedefs[tid as usize].name)
                ),
            ));
        }
        if self.typedefs[tid as usize].is_distinct {
            // `Price(100)` is a CONVERSION, not a construction: same
            // representation, different identity, nothing emitted.
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{}` converts one value; it is a distinct type, not a struct",
                        self.show_name(&self.typedefs[tid as usize].name)
                    ),
                ));
            }
            // The base says what a literal argument should be, so
            // `Bag([1, 2])` reads the same as `Bag b = [1, 2]`.
            let base = self.base_of(ty).expect("a distinct type has a base");
            let v = self.lower_expr_as(&args.pos[0], base)?;
            if self.underlying(v.ty) != self.underlying(ty) {
                return Err(Diag::new(
                    args.pos[0].span(),
                    self.mismatch(self.underlying(ty), v.ty),
                ));
            }
            return Ok(Val::new(v.val(), ty, v.owned));
        }
        let tname = self.typedefs[tid as usize].name.clone();
        // `List<int>(bag)` -- converting a distinct collection back to its
        // base, spelled as the base type the way `int(price)` is. A
        // collection is otherwise never constructed by name, so one argument
        // of a distinct type over exactly this collection is unambiguous.
        if ["Array$", "List$", "Map$"]
            .iter()
            .any(|p| tname.starts_with(p))
            && !self.building_literal
            && args.pos.len() == 1
            && args.named.is_empty()
        {
            let v = self.lower_expr_as(&args.pos[0], ty)?;
            if self.base_of(v.ty).is_some() && self.underlying(v.ty) == ty {
                return Ok(Val::new(v.val(), ty, v.owned));
            }
            // Anything else falls through to the paths below, every one of
            // which refuses a single positional argument with the message it
            // always gave -- before lowering any argument, so nothing is
            // evaluated twice.
        }
        if tname.starts_with("Map$") {
            let (k, v) = self.map_kv(ty).expect("a map has a key and a value");
            // The key type is checked BEFORE the spelling, and on the `{}`
            // path too: a bad key type must be caught either way, and it is
            // the more useful of the two things to be told.
            let kind = self.map_key_kind(k, span)?;
            if !self.building_literal {
                return Err(Diag::new(
                    span,
                    format!(
                        "write {} as a literal: `{{k: v}}` or `{{}}`",
                        a_or_an(&self.tyname(ty))
                    ),
                ));
            }
            if !args.pos.is_empty() || !args.named.is_empty() {
                return Err(Diag::new(span, "a map takes no arguments"));
            }
            // `MapKey`, then the two refcount flags (runtime/rt.h).
            let kv = self.new_val(IrTy::I64);
            self.push(Inst::IConst { dst: kv, val: kind });
            let mut flags = vec![kv];
            for b in [self.is_ref(k), self.is_ref(v)] {
                let f = self.new_val(IrTy::I1);
                self.push(Inst::BConst { dst: f, val: b });
                flags.push(f);
            }
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_map_new".to_string(),
                args: flags,
            });
            self.stmt_temps.push(d);
            return Ok(Val::new(d, ty, true));
        }
        // `List<int>()` and friends are gone: a collection is written as a
        // literal, and having both spellings would be two ways to say one
        // thing. `lower_literal` builds them now; this path is only reached
        // when someone writes the old form.
        if (tname.starts_with("Array$") || tname.starts_with("List$")) && !self.building_literal {
            return Err(Diag::new(
                span,
                format!(
                    "write {} as a literal: `[a, b]`, `[x; n]`, or `[]`",
                    a_or_an(&self.tyname(ty))
                ),
            ));
        }
        if tname.starts_with("Array$") || tname.starts_with("List$") {
            let elem = self.seq_elem(ty).expect("collection has an element type");
            let refs = self.is_ref(elem);
            let is_lst = tname.starts_with("List$");
            let want = if is_lst { 0 } else { 2 };
            if args.pos.len() != want || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    if is_lst {
                        "a list takes no arguments".to_string()
                    } else {
                        "an array takes two arguments: its length and the value \
                         every element starts at"
                            .to_string()
                    },
                ));
            }
            let mut a = Vec::new();
            if !is_lst {
                let n = self.lower_expr(&args.pos[0])?;
                if self.underlying(n.ty) != Ty::Int {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(Ty::Int, n.ty)));
                }
                let fill = self.lower_expr(&args.pos[1])?;
                if !self.assignable(fill.ty, elem) {
                    return Err(Diag::new(args.pos[1].span(), self.mismatch(elem, fill.ty)));
                }
                // The array retains the fill once per element, in the
                // runtime, so an owned temporary here is still released by
                // the statement as usual.
                a.push(n.val());
                a.push(fill.val());
            }
            let flag = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: flag,
                val: refs,
            });
            a.push(flag);
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Call {
                dst: Some(d),
                func: if is_lst {
                    "rt_list_new"
                } else {
                    "rt_array_new"
                }
                .to_string(),
                args: a,
            });
            self.stmt_temps.push(d);
            return Ok(Val::new(d, ty, true));
        }
        if self.typedefs[tid as usize].is_chan {
            // `Chan<int>(8)` -- one positional argument, the capacity.
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    "a channel takes one argument: its capacity".to_string(),
                ));
            }
            let cap = self.lower_expr(&args.pos[0])?;
            if cap.ty != Ty::Int {
                return Err(Diag::new(
                    args.pos[0].span(),
                    self.mismatch(Ty::Int, cap.ty),
                ));
            }
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_chan_new".to_string(),
                args: vec![cap.val()],
            });
            self.stmt_temps.push(d);
            return Ok(Val::new(d, ty, true));
        }
        let name = self.typedefs[tid as usize].name.clone();
        let name = name.as_str();
        let fields = self.field_params[tid as usize].clone();
        let module = self.type_module[tid as usize].clone();
        // Construction writes every field, so from outside the module it is
        // allowed only when every field it would write is one the caller
        // could write anyway. A private field with a default is written by
        // its own module's default and blocks nothing unless it is named; a
        // private field without one would have to be passed, which is
        // exactly what privacy forbids. That is the point: a type with an
        // invariant hides a field, and then its module's own function is
        // the only way to make one. Checked before binding, so the refusal
        // is about privacy and not about how many arguments there are.
        for (i, f) in fields.iter().enumerate() {
            if self.field_visible(tid, i as u32) {
                continue;
            }
            let shown = self.show_name(name);
            if f.default.is_none() {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{shown}` cannot be constructed outside `{module}`: its field `{}` \
                         is private and has no default. `{module}` must provide a function \
                         that builds one",
                        f.name
                    ),
                ));
            }
            if let Some((_, e)) = args.named.iter().find(|(n, _)| *n == f.name) {
                return Err(Diag::new(
                    e.span(),
                    format!(
                        "field `{}` of `{shown}` is private to `{module}`; only a `pub` \
                         field can be set from another module",
                        f.name
                    ),
                ));
            }
        }
        let slots = self.bind_args(name, &fields, args, span)?;
        // Allocating and storing run no user code, so only a later argument
        // can disturb an earlier one before it is stored.
        let later = self.later_flags(&slots, &fields, false);

        let mut given: Vec<Option<Val>> = Vec::new();
        for (i, (e, f)) in slots.iter().zip(fields.iter()).enumerate() {
            // The field's type is what a literal argument takes. A default
            // is an expression lowered here, at each construction, so a
            // literal default builds a fresh collection for every object
            // rather than one shared by all of them.
            let v = self.lower_slot(e, f, &module)?;
            if !self.assignable(v.ty, f.ty) {
                return Err(Diag::new(
                    e.span(),
                    format!(
                        "type mismatch: field `{}` is {}, found {}",
                        f.name,
                        self.tyname(f.ty),
                        self.tyname(v.ty)
                    ),
                ));
            }
            let v = self.hold(e, v, later[i]);
            given.push(Some(v));
        }

        let obj = self.new_val(IrTy::Ref);
        self.push(Inst::Alloc { dst: obj, tid });
        for (i, g) in given.into_iter().enumerate() {
            let v = g.unwrap();
            // The object takes a +1 on every reference field. An owned
            // temporary is handed straight over; a borrowed one is retained.
            if self.is_ref(v.ty) {
                if v.owned {
                    self.stmt_temps.retain(|t| *t != v.val());
                } else {
                    self.rc_inc(v.val());
                }
            }
            self.push(Inst::StoreField {
                obj,
                tid,
                idx: i as u32,
                val: v.val(),
            });
        }
        self.stmt_temps.push(obj);
        Ok(Val::new(obj, ty, true))
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
    pub(super) fn lower_trap(&mut self, args: &Args, span: Span) -> Result<(), Diag> {
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

    fn lower_call(&mut self, name: &str, args: &Args, span: Span) -> Result<Val, Diag> {
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
            format!("rt_{}", crate::ast::bare(name).trim_start_matches('_'))
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
