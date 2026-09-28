//! Declaration checking: shadowing, reserved methods, embedding
//! forwarders, and the per-function entry point into lowering a body.

use super::{quoted, Lowerer, Sig, BUILTIN_FNS, DESTRUCTOR, ENTRY};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{self, Block, Term};
use std::collections::HashMap;

impl Lowerer {
    /// A name nothing in scope answers to.
    ///
    /// The one case worth more than "unknown": a function in the entry file
    /// reaching for a variable declared at the top level. The name IS there,
    /// two lines up, so "unknown variable" reads like a compiler bug. It is
    /// not one -- the top-level statements are the program's body, so that
    /// variable is a local of the body, and a function can no more see it
    /// than it can see a local of another function. There is no module
    /// state to make it anything else, deliberately
    /// (docs/module-state-decision.md). So the diagnostic says which of the
    /// two things it is, and names both ways out.
    pub(super) fn unknown_variable(&self, name: &str, span: Span) -> Diag {
        if !self.in_entry
            && self.cur_module == self.entry_module
            && self.entry_locals.contains(name)
        {
            return Diag::new(
                span,
                format!(
                    "`{name}` is a local of the program body: the statements at the \
                     top level ARE the body, so a function cannot see them -- pass \
                     it in as an argument, or declare it `const`"
                ),
            );
        }
        // A module of this program, named in a file that did not import it.
        // `lib.f()`, `lib.MAX` and `lib.Type` all arrive here once the
        // qualifier has been refused, and "unknown variable `lib`" hides the
        // one thing the reader needs: the module exists, this file just
        // cannot see it (§2.1). After the check above, which is about a name
        // that IS declared here.
        if self.modules.contains(name) && !self.module_imports(&self.cur_module, name) {
            return Diag::new(
                span,
                format!(
                    "`{name}` is a module of this program, but this file does not \
                     import it -- write `import {name};` at the top"
                ),
            );
        }
        Diag::new(span, format!("unknown variable `{name}`"))
    }

    /// One forwarder per promoted method: `void Dog.speak()` calling
    /// `Animal.speak()` on the embedded field.
    ///
    /// A method the outer type defines itself always wins.
    ///
    /// Run to a FIXPOINT, because transitivity does not fall out for free:
    /// when Puppy embeds Dog which embeds Animal, `Dog.count_legs` is itself
    /// a forwarder generated in this same pass, so it is not visible until
    /// the round that created it has finished. Each round consults the
    /// forwarders the previous rounds produced.
    pub(super) fn embed_forwarders(&self, p: &Program) -> Result<Vec<Func>, Diag> {
        let mut out: Vec<Func> = Vec::new();
        // Which embedded field each forwarder came through, keyed the same
        // way as `out`. Two fields offering one name is ambiguous, and
        // telling them apart needs to know where each came from.
        let mut via: HashMap<String, String> = HashMap::new();
        loop {
            let before = out.len();
            self.forward_round(p, &mut out, &mut via)?;
            if out.len() == before {
                return Ok(out);
            }
        }
    }

    /// Two functions must not reach the C backend with the same C name.
    ///
    /// Monomorphisation mangles with `$`, and the emitter rewrites `$` to
    /// `__` -- which a source identifier may also contain. So a generic
    /// `id<T>` instantiated at `int` becomes `id$int` becomes `fn_id__int`,
    /// and a hand-written `id__int` collides with it. The emitter asserts
    /// this, but an assertion is a panic and a core dump on input that is
    /// otherwise valid; the user deserves a diagnostic with a location.
    pub(super) fn check_c_name_collisions(p: &Program, forwarders: &[Func]) -> Result<(), Diag> {
        let mut seen: HashMap<String, String> = HashMap::new();
        for f in p.funcs.iter().chain(forwarders.iter()) {
            let key = f.key();
            let c = crate::emit_c::c_ident(&key);
            if let Some(other) = seen.insert(c, key.clone()) {
                if other != key {
                    return Err(Diag::new(
                        f.span,
                        format!(
                            "`{}` and `{}` would both be emitted as the same \
                             C function; rename one",
                            crate::ast::bare(&key),
                            crate::ast::bare(&other)
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn forward_round(
        &self,
        p: &Program,
        out: &mut Vec<Func>,
        via: &mut HashMap<String, String>,
    ) -> Result<(), Diag> {
        for (tid, t) in p.types.iter().enumerate() {
            if t.is_interface {
                continue;
            }
            for f in t.fields.iter().filter(|f| f.embedded) {
                let Some(inner) = self.tdef_of(f.ty) else {
                    continue;
                };
                let iname = self.typedefs[inner as usize].name.clone();
                // Every instance method of the embedded type, by name. A
                // static method is not promoted: it has no receiver, so a
                // forwarder would have nothing to forward to, and `Outer`
                // does not gain a `make()` by holding a `Base`. Forwarding
                // one used to call it through a value and fail the whole
                // declaration of the embedding type.
                let prefix = format!("{iname}.");
                let mut promoted: Vec<(String, Sig)> = self
                    .sigs
                    .iter()
                    .filter(|(k, _)| !self.statics.contains(k.as_str()))
                    .filter_map(|(k, sig)| {
                        let m = k.strip_prefix(&prefix)?;
                        // Nor is a destructor. The embedded value is a field,
                        // so it is released -- and its own destructor runs --
                        // when the outer object dies; a forwarder would run
                        // it a second time, on an object still alive.
                        if m == DESTRUCTOR {
                            return None;
                        }
                        Some((
                            m.to_string(),
                            Sig {
                                params: sig.params.clone(),
                                ret: sig.ret,
                                module: sig.module.clone(),
                                is_pub: sig.is_pub,
                                is_prim: sig.is_prim,
                            },
                        ))
                    })
                    .collect();
                // Forwarders already generated count as the inner type's
                // methods, which is what makes deeper embedding work.
                for g in out.iter() {
                    if g.recv.as_deref() == Some(iname.as_str()) {
                        promoted.push((
                            g.name.clone(),
                            Sig {
                                params: g.params.clone(),
                                ret: g.ret,
                                module: g.module.clone(),
                                is_pub: g.is_pub,
                                is_prim: g.is_prim,
                            },
                        ));
                    }
                }
                // `self.sigs` is a HashMap with a randomised hasher, so the
                // order methods come out of it differs between runs of the
                // compiler. Emission follows this order, which made the
                // emitted C non-reproducible whenever a type promoted two or
                // more methods. Sort, so a build is a function of its input.
                promoted.sort_by(|a, b| a.0.cmp(&b.0));
                promoted.dedup_by(|a, b| a.0 == b.0);

                for (mname, sig) in promoted {
                    let key = format!("{}.{mname}", t.name);
                    if self.sigs.contains_key(&key) {
                        continue; // the outer type defines it itself, and wins
                    }
                    if let Some(other) = via.get(&key) {
                        if *other == f.name {
                            continue; // already forwarded through this field
                        }
                        // Two embedded types offer this name and the outer
                        // type does not break the tie. Picking one would be
                        // picking by declaration order, which is not a rule
                        // anyone should have to know. Go rejects this too.
                        return Err(Diag::new(
                            f.span,
                            format!(
                                "`{}` gets `{mname}` from both `{}` and `{}`; \
                                 give `{}` its own `{mname}` to say which one it means",
                                self.bare_name(&t.name),
                                self.bare_name(other),
                                self.bare_name(&f.name),
                                self.bare_name(&t.name)
                            ),
                        ));
                    }
                    via.insert(key, f.name.clone());
                    let args = Args {
                        pos: sig
                            .params
                            .iter()
                            .filter(|q| !q.is_optional())
                            .map(|q| Expr::Var(q.name.clone(), f.span))
                            .collect(),
                        named: sig
                            .params
                            .iter()
                            .filter(|q| q.is_optional())
                            .map(|q| (q.name.clone(), Expr::Var(q.name.clone(), f.span)))
                            .collect(),
                    };
                    let call = Expr::MethodCall(
                        Box::new(Expr::Var(f.name.clone(), f.span)),
                        mname.clone(),
                        args,
                        f.span,
                    );
                    let body = if sig.ret == Ty::Void {
                        vec![Stmt::Eval {
                            expr: call,
                            span: f.span,
                        }]
                    } else {
                        vec![Stmt::Return {
                            value: Some(call),
                            span: f.span,
                        }]
                    };
                    out.push(Func {
                        module: t.module.clone(),
                        is_pub: true,
                        ret: sig.ret,
                        is_static: false,
                        is_prim: false,
                        recv: Some(t.name.clone()),
                        name: mname,
                        tparams: Vec::new(),
                        recv_tparams: Vec::new(),
                        params: sig.params.clone(),
                        body,
                        span: f.span,
                    });
                }
            }
            let _ = tid;
        }
        Ok(())
    }

    /// Refuse any name that is already visible.
    ///
    /// **Nothing shadows anything, anywhere.** Not an outer local, not a
    /// parameter, not a function, not a type. This is what removes the need
    /// for a `this` keyword -- a bare name can only ever mean one thing, so
    /// there is nothing to disambiguate -- and it deletes the entire class of
    /// bugs where a reader and the compiler disagree about which `x` is meant.
    ///
    /// The cost is real and deliberate: the programmer renames.
    pub(super) fn check_shadow(&self, name: &str, span: Span) -> Result<(), Diag> {
        self.check_not_import(&self.cur_module, name, span)?;
        if self.binding(name).is_some() {
            return Err(Diag::new(
                span,
                format!("`{name}` is already in scope; shadowing is not allowed, rename one"),
            ));
        }
        // Against this module's own function, not the bare name: functions
        // are interned module-qualified, so `sigs` no longer holds the name
        // as it was written.
        if self.sigs.contains_key(&self.resolve_fn(name)) || BUILTIN_FNS.contains(&name) {
            return Err(Diag::new(
                span,
                format!("`{name}` is already a function; shadowing is not allowed, rename one"),
            ));
        }
        if self.typedefs.iter().any(|d| d.name == name) {
            return Err(Diag::new(
                span,
                format!("`{name}` is already a type; shadowing is not allowed, rename one"),
            ));
        }
        if self.resolve_const(name).is_some() {
            return Err(Diag::new(
                span,
                format!("`{name}` is already a constant; shadowing is not allowed, rename one"),
            ));
        }
        if let Some((tid, _)) = self.recv {
            // A field this module cannot see does not claim the name: were
            // it otherwise, adding a private field to a library type would
            // break every module whose embedding type used that name for a
            // local, and privacy would leak through the error message.
            let visible = self
                .field_path(tid, name)
                .is_some_and(|p| self.check_field_access(tid, &p, name, span).is_ok());
            if visible {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{name}` is already a field of `{}`; shadowing is not allowed, rename one",
                        self.show_name(&self.typedefs[tid as usize].name)
                    ),
                ));
            }
        }
        Ok(())
    }

    /// A type WRITTEN in this module has to exist and be reachable.
    ///
    /// `lib.Nope` interns like any other name, so without this it failed
    /// later as a mismatch against whatever it was compared to -- "expected
    /// lib.Nope, found lib.P", which says nothing about the real mistake.
    pub(super) fn check_named_ty(&self, t: Ty, span: Span) -> Result<(), Diag> {
        let Ty::User(i) = t else { return Ok(()) };
        let raw = self.ty_exprs[i as usize].name.clone();
        let Some(tid) = self.tdef_of(t) else {
            return Err(Diag::new(
                span,
                match raw.split_once('#') {
                    Some((m, n)) => format!("`{m}` has no type `{n}`"),
                    None => format!("unknown type `{raw}`"),
                },
            ));
        };
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "it cannot be named from here"),
            ));
        }
        Ok(())
    }

    /// The IR name of a reserved method (`cmp`, `eq`, `hash`) this type has
    /// with the one signature it may have -- or None.
    ///
    /// The check is on the SURFACE signature, not the IR one, and that is
    /// the whole point. `int P.cmp(P)` and `int Outer.cmp(Inner)` have the
    /// same IR shape, `int64 (Obj *, Obj *)`, so the vtable's shape test
    /// cannot tell them apart. The runtime calls `cmp` with two elements of
    /// the list it is sorting; handed the second one, `Outer.cmp(Inner)`
    /// would read an `Inner`'s fields out of an `Outer`. So the parameter
    /// has to be the receiver's own type, checked here.
    ///
    /// `check_reserved_decls` has already refused every wrong shape a
    /// program can WRITE, so the only thing this rejects is the forwarder
    /// embedding synthesises, which keeps the embedded type's parameter.
    pub(super) fn reserved_method(&self, tid: u32, name: &str) -> Option<String> {
        let key = format!("{}.{name}", self.typedefs[tid as usize].name);
        if self.statics.contains(&key) {
            return None;
        }
        let sig = self.sigs.get(&key)?;
        if sig.params.len() != usize::from(name != "hash") {
            return None;
        }
        if let Some(p) = sig.params.first() {
            if p.is_optional() || self.tdef_of(p.ty) != Some(tid) {
                return None;
            }
        }
        let want = if name == "eq" { Ty::Bool } else { Ty::Int };
        (sig.ret == want).then_some(key)
    }

    /// Which `MapKey` (runtime/rt.h) a key type is, refusing one that is
    /// none of them.
    ///
    /// An `int` or a `str` the runtime hashes itself. Anything else has to
    /// hash and compare ITSELF, through the two reserved methods the
    /// compiler stores in its TypeInfo -- so the rule is the one §6.2
    /// already uses for operators: declare the methods and the feature
    /// works. `eq` is the method `==` already desugars to, which is what
    /// keeps a map and the operator from disagreeing about which keys are
    /// the same one; `hash` is the only new name.
    pub(super) fn map_key_kind(&mut self, k: Ty, span: Span) -> Result<i64, Diag> {
        let base = self.underlying(k);
        match base {
            Ty::Int => return Ok(0),
            Ty::Str => return Ok(1),
            _ => {}
        }
        let shown = self.tyname(k);
        let Some(tid) = self.tdef_of(base) else {
            return Err(Diag::new(
                span,
                format!(
                    "a map key is an `int`, a `str`, or a type that declares both \
                     `int T.hash()` and `bool T.eq(T other)`; `{shown}` is none of those"
                ),
            ));
        };
        // Same reason as `sort`: the runtime hands `eq` two keys, and an
        // interface's two keys can be different types.
        if self.typedefs[tid as usize].is_interface {
            return Err(Diag::new(
                span,
                format!(
                    "a map hashes and compares each key through its own type, and \
                     `{shown}` is an interface: two keys can be different types, and \
                     one's `eq` would be handed the other. Key the map on the concrete \
                     type instead."
                ),
            ));
        }
        let missing: Vec<&str> = ["hash", "eq"]
            .into_iter()
            .filter(|m| self.reserved_method(tid, m).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(Diag::new(
                span,
                format!(
                    "a map key is an `int`, a `str`, or a type that declares both \
                     `int {shown}.hash()` and `bool {shown}.eq({shown} other)`, and that \
                     hashes equal keys equally; {}",
                    self.reserved_missing(tid, &missing)
                ),
            ));
        }
        // Both are found by name, so both obey privacy -- the same rule
        // `to_str` and `cmp` follow. Using another module's type as a key
        // therefore needs `pub` on both.
        self.check_method_access(tid, "hash", span)?;
        self.check_method_access(tid, "eq", span)?;
        Ok(2)
    }

    /// Why `tid` cannot supply these reserved methods, as the tail of a
    /// diagnostic. Every name passed in must actually be missing.
    pub(super) fn reserved_missing(&self, tid: u32, names: &[&str]) -> String {
        let shown = self.show_name(&self.typedefs[tid as usize].name);
        let mut absent: Vec<String> = Vec::new();
        let mut clauses: Vec<String> = Vec::new();
        for name in names {
            let key = format!("{}.{name}", self.typedefs[tid as usize].name);
            let Some(sig) = self.sigs.get(&key) else {
                absent.push(format!("no `{name}`"));
                continue;
            };
            // The one shape that survives `check_reserved_decls` and is
            // still unusable: promoted from an embedded field, so it takes
            // that field's type.
            let via = sig.params.first().and_then(|p| {
                let pt = self.tdef_of(p.ty)?;
                self.field_params[tid as usize]
                    .iter()
                    .any(|f| f.embedded && self.tdef_of(f.ty) == Some(pt))
                    .then_some(p.ty)
            });
            clauses.push(match via {
                Some(t) => format!(
                    "`{shown}` inherits `{name}` from the embedded `{inner}`, and \
                     that one takes {an_inner}, not {an_outer} -- a promoted method \
                     keeps the embedded type's parameter, so `{shown}` has to \
                     declare its own",
                    inner = self.tyname(t),
                    an_inner = quoted(&self.tyname(t)),
                    an_outer = quoted(&shown),
                ),
                None => format!("`{shown}.{name}` does not have that signature"),
            });
        }
        if !absent.is_empty() {
            clauses.insert(0, format!("`{shown}` declares {}", absent.join(" and ")));
        }
        clauses.join("; ")
    }

    pub(super) fn lower_func(&mut self, f: &Func) -> Result<ir::Func, Diag> {
        self.lower_func_inner(f).map_err(|d| d.in_module(&f.module))
    }

    fn lower_func_inner(&mut self, f: &Func) -> Result<ir::Func, Diag> {
        self.cur_module = f.module.clone();
        self.in_entry = f.name == ENTRY;
        self.types.clear();
        self.blocks.clear();
        self.scopes.clear();
        self.owned.clear();
        self.loops.clear();
        self.moved.clear();
        self.payload_binds.clear();
        // Every statement flushes its own temporaries, so this is empty in a
        // well-formed lowering. Clearing it anyway keeps a value from one
        // function's block list out of the next one's, where its number would
        // name a different value entirely.
        self.stmt_temps.clear();
        self.freezes.clear();
        self.synth = 0;
        self.cur = 0;
        self.ret_ty = f.ret;
        self.cur_lambda = self.lambda_targets.get(&f.key()).cloned();

        let entry = self.new_block();
        self.switch_to(entry);

        let mut scope = HashMap::new();
        let mut params = Vec::new();

        // A method takes its receiver as a hidden first parameter. Its fields
        // are reached bare; the receiver as a whole is `this`.
        self.recv = None;
        self.no_recv = if f.name == "$main" {
            "top-level code is not inside a method".to_string()
        } else if f.is_static {
            format!(
                "`{}.{}` is a static method, called on the type rather than a value",
                self.bare_name(f.recv.as_deref().unwrap_or_default()),
                f.name
            )
        } else if f.recv.is_none() {
            format!("`{}` is a function, not a method", self.bare_name(&f.name))
        } else {
            String::new()
        };
        // A static method is qualified by a type but takes no receiver, so
        // no hidden first parameter and no bare field names inside it.
        if let Some(rname) = f.recv.as_ref().filter(|_| !f.is_static) {
            let tid = self
                .typedefs
                .iter()
                .position(|d| d.name == *rname)
                .expect("receiver type checked above") as u32;
            // Represented as its type is: a reference for a struct or an
            // enum, but a plain int for a method on `distinct int Price`,
            // because a distinct type is erased to its base.
            let rty = self
                .ty_named(rname)
                .expect("the receiver's type is declared");
            let v = self.new_val(self.irty(rty));
            params.push(v);
            self.recv = Some((tid, v));
        }

        for p in &f.params {
            self.check_not_import(&self.cur_module, &p.name, p.span)?;
            // A parameter would win over the constant in every lookup, so
            // taking a constant's name would silently hide it for the whole
            // body.
            if self.resolve_const(&p.name).is_some() {
                return Err(Diag::new(
                    p.span,
                    format!(
                        "`{}` is already a constant; shadowing is not allowed, rename one",
                        p.name
                    ),
                ));
            }
            let v = self.new_val(self.irty(p.ty));
            params.push(v);
            if scope.insert(p.name.clone(), (p.ty, v, false)).is_some() {
                return Err(Diag::new(
                    p.span,
                    format!("duplicate parameter `{}`", p.name),
                ));
            }
        }
        self.blocks[self.cur].params = params.clone();
        self.scopes.push(scope);
        // Parameters are BORROWED (docs/ir-v0.md §5.1), so they are never
        // registered as owned and never decremented here.
        self.owned.push(Vec::new());

        self.lower_block(&f.body)?;

        // Fall off the end: release locals and return.
        if !self.terminated() {
            if f.ret != Ty::Void {
                return Err(Diag::new(
                    f.span,
                    format!(
                        "function `{}` must return a value of type {}",
                        self.bare_name(&f.name),
                        self.tyname(f.ret)
                    ),
                ));
            }
            self.release_all();
            self.restore_freezes_to(0);
            self.terminate(Term::Ret { val: None });
        }
        self.scopes.pop();
        self.owned.pop();

        let blocks: Vec<Block> = self
            .blocks
            .iter()
            .map(|b| Block {
                id: b.id,
                params: b.params.clone(),
                insts: b.insts.clone(),
                // An unterminated block here is unreachable (both arms of an
                // `if` returned). Give it a well-formed terminator so every
                // backend sees a total CFG.
                term: b.term.clone().unwrap_or(Term::Ret { val: None }),
            })
            .collect();

        Ok(ir::Func {
            name: f.key(),
            params,
            ret: if f.ret == Ty::Void {
                None
            } else {
                Some(self.irty(f.ret))
            },
            blocks,
            types: self.types.clone(),
            entry,
        })
    }
}
