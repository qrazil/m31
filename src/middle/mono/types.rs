//! Instantiating generic types, and substituting inside type declarations.

use super::*;

impl Mono {
    /// Resolve a type written inside a declaration into a concrete output
    /// type, applying `sub` to any type parameters and instantiating any
    /// generic type it names.
    pub(super) fn subst_ty(&mut self, t: Ty, sub: &Subst, span: Span) -> Result<Ty, Diag> {
        let Ty::User(i) = t else { return Ok(t) };
        let e = self.src_exprs[i as usize].clone();

        // A bare name that is a type parameter resolves to its binding.
        if e.args.is_empty() {
            if let Some(bound) = sub.get(&e.name) {
                return Ok(*bound);
            }
        }

        let mut args = Vec::new();
        for a in &e.args {
            args.push(self.subst_ty(*a, sub, span)?);
        }

        // `Chan<T>` has no declaration to instantiate: the runtime owns its
        // layout. Emit an opaque type carrying the element type, so the rest
        // of the compiler can see what a channel carries without knowing how
        // it is built.
        if matches!(e.name.as_str(), "Chan" | "Array" | "List" | "Map") {
            let want = if e.name == "Map" { 2 } else { 1 };
            if args.len() != want {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{}` takes {want} type argument(s), found {}",
                        e.name,
                        args.len()
                    ),
                ));
            }
            // `keys()` and `values()` hand back a List whose element type
            // is only implied by the map's -- nothing in the source spells
            // `List<K>`, so nothing would instantiate it. Do it here, where
            // the map's arguments are known. A container carries no drop or
            // walk function, so an unused one costs a typedef and nothing
            // else.
            if e.name == "Map" && args.len() == 2 {
                self.builtin_decl("List", &args[0..1], span);
                self.builtin_decl("List", &args[1..2], span);
                // `get` hands back an Option<V>. Nothing in the source spells
                // it, so nothing else would instantiate it.
                self.enum_decl("Option", &args[1..2], span)?;
            }
            // `index_of` hands back an Option<int> whatever the elements are.
            if matches!(e.name.as_str(), "Array" | "List") && args.len() == 1 {
                self.enum_decl("Option", &[Ty::Int], span)?;
            }
            let mangled = self.builtin_decl(&e.name, &args, span);
            return Ok(self.intern(mangled, Vec::new()));
        }

        if let Some(decl) = self.generic_types.get(&e.name).cloned() {
            if args.len() != decl.tparams.len() {
                return Err(Diag::new(
                    span,
                    format!(
                        "type `{}` takes {} type argument(s), found {}",
                        crate::front::ast::bare(&e.name),
                        decl.tparams.len(),
                        args.len()
                    ),
                ));
            }
            let mangled = self.mangle(&e.name, &args);
            self.instantiate_type(&decl, &mangled, &args, span)?;
            return Ok(self.intern(mangled, Vec::new()));
        }

        if !args.is_empty() {
            return Err(Diag::new(
                span,
                format!("type `{}` is not generic", crate::front::ast::bare(&e.name)),
            ));
        }
        Ok(self.intern(e.name, Vec::new()))
    }

    pub(super) fn instantiate_type(
        &mut self,
        decl: &TypeDecl,
        mangled: &str,
        args: &[Ty],
        span: Span,
    ) -> Result<(), Diag> {
        self.shown
            .insert(mangled.to_string(), (decl.name.clone(), args.to_vec()));
        if !self.done.insert(mangled.to_string()) {
            return Ok(());
        }
        let sub: Subst = decl
            .tparams
            .iter()
            .cloned()
            .zip(args.iter().copied())
            .collect();
        let mut fields = Vec::new();
        for f in &decl.fields {
            let ty = self.subst_ty(f.ty, &sub, f.span)?;
            // A field has to hold something. Unlike a payload, it cannot
            // simply vanish: a construction names it, and so does every read.
            if ty == Ty::Void {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{}` cannot be instantiated with `void`: its field `{}` would hold no value",
                        crate::front::ast::bare(&decl.name),
                        f.name
                    ),
                ));
            }
            fields.push(Param {
                ty,
                name: f.name.clone(),
                default: match &f.default {
                    Some(e) => Some(self.subst_expr(e, &sub)?),
                    None => None,
                },
                embedded: f.embedded,
                is_pub: f.is_pub,
                is_const: f.is_const,
                span: f.span,
            });
        }
        // A variant's payload types substitute like a field's -- except that
        // a `void` one is no value at all, so it is dropped: `Ok(T)` at
        // `Result<void, E>` is a variant that carries nothing, constructed
        // `Result<void, E>.Ok` and matched `case Ok:` exactly like any other
        // payload-less variant. That is what lets a function that can fail
        // but has nothing to return say so, rather than inventing an `int`.
        let mut variants = Vec::new();
        for v in &decl.variants {
            let mut payload = Vec::new();
            for t in &v.payload {
                let t = self.subst_ty(*t, &sub, v.span)?;
                if t != Ty::Void {
                    payload.push(t);
                }
            }
            variants.push(EnumVariant {
                name: v.name.clone(),
                payload,
                span: v.span,
            });
        }
        let _ = span;
        // The type's methods come with it, each at this instantiation. One
        // with type parameters of its own waits for a call to say what they
        // are; the rest are queued now.
        for m in self.generic_methods.clone() {
            if m.recv.as_deref() != Some(decl.name.as_str()) {
                continue;
            }
            let msub: Subst = m
                .recv_tparams
                .iter()
                .cloned()
                .zip(args.iter().copied())
                .collect();
            if m.tparams.is_empty() {
                let name = m.name.clone();
                self.method_queue.push((m, msub, mangled.to_string(), name));
            } else {
                self.own_generic
                    .entry(mangled.to_string())
                    .or_default()
                    .insert(m.name.clone(), (m, msub));
            }
        }
        let methods = self.subst_iface_methods(&decl.methods, &sub)?;
        self.out_types.push(TypeDecl {
            name: mangled.to_string(),
            module: decl.module.clone(),
            is_pub: decl.is_pub,
            tparams: Vec::new(),
            fields,
            methods,
            is_interface: decl.is_interface,
            variants,
            is_enum: decl.is_enum,
            distinct_base: decl.distinct_base,
            is_const: decl.is_const,
            span: decl.span,
        });
        Ok(())
    }

    /// Instantiate one of the compiler's own generic enums, for a built-in
    /// method whose return type the source never spells.
    pub(super) fn enum_decl(&mut self, name: &str, args: &[Ty], span: Span) -> Result<(), Diag> {
        let Some(decl) = self.generic_types.get(name).cloned() else {
            return Ok(());
        };
        let mangled = self.mangle(name, args);
        self.instantiate_type(&decl, &mangled, args, span)
    }

    /// Declare one instantiation of a runtime-owned container, and return its
    /// mangled name. The runtime owns the layout, so the declaration only has
    /// to carry the type arguments for the rest of the compiler to read.
    pub(super) fn builtin_decl(&mut self, name: &str, args: &[Ty], span: Span) -> String {
        let mangled = self.mangle(name, args);
        self.shown
            .insert(mangled.clone(), (name.to_string(), args.to_vec()));
        if self.done.insert(mangled.clone()) {
            self.out_types.push(TypeDecl {
                name: mangled.clone(),
                module: String::new(),
                is_pub: true,
                tparams: Vec::new(),
                fields: args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| Param {
                        ty: *a,
                        name: format!("$t{i}"),
                        default: None,
                        embedded: false,
                        // A tuple is a builtin shape, not a module's type:
                        // its slots are as public as the tuple.
                        is_pub: true,
                        is_const: false,
                        span,
                    })
                    .collect(),
                methods: Vec::new(),
                is_interface: false,
                variants: Vec::new(),
                is_enum: false,
                distinct_base: None,
                is_const: false,
                span,
            });
        }
        mangled
    }

    /// An interface's method SIGNATURES substitute like anything else.
    ///
    /// They used to be cloned unchanged, so `interface Getter<T> { T get(); }`
    /// at `Getter<int>` still required a method returning `T` -- which after
    /// interning resolved to the instantiated interface itself. No concrete
    /// type could satisfy a generic interface at all.
    pub(super) fn subst_iface_methods(
        &mut self,
        ms: &[Func],
        sub: &Subst,
    ) -> Result<Vec<Func>, Diag> {
        let mut out = Vec::new();
        for m in ms {
            let mut params = Vec::new();
            for p in &m.params {
                params.push(Param {
                    ty: self.subst_ty(p.ty, sub, p.span)?,
                    name: p.name.clone(),
                    default: None,
                    embedded: false,
                    is_pub: false,
                    is_const: p.is_const,
                    span: p.span,
                });
            }
            out.push(Func {
                module: m.module.clone(),
                is_pub: m.is_pub,
                ret: self.subst_ty(m.ret, sub, m.span)?,
                is_static: m.is_static,
                is_prim: m.is_prim,
                recv: m.recv.clone(),
                name: m.name.clone(),
                tparams: Vec::new(),
                recv_tparams: Vec::new(),
                params,
                body: Vec::new(),
                span: m.span,
            });
        }
        Ok(out)
    }

    pub(super) fn subst_type_decl(&mut self, t: &TypeDecl, sub: &Subst) -> Result<TypeDecl, Diag> {
        let mut fields = Vec::new();
        for f in &t.fields {
            fields.push(Param {
                ty: self.subst_ty(f.ty, sub, f.span)?,
                name: f.name.clone(),
                default: match &f.default {
                    Some(e) => Some(self.subst_expr(e, sub)?),
                    None => None,
                },
                embedded: f.embedded,
                is_pub: f.is_pub,
                is_const: f.is_const,
                span: f.span,
            });
        }
        let mut variants = Vec::new();
        for v in &t.variants {
            let mut payload = Vec::new();
            for p in &v.payload {
                payload.push(self.subst_ty(*p, sub, v.span)?);
            }
            variants.push(EnumVariant {
                name: v.name.clone(),
                payload,
                span: v.span,
            });
        }
        Ok(TypeDecl {
            name: t.name.clone(),
            module: t.module.clone(),
            is_pub: t.is_pub,
            tparams: Vec::new(),
            fields,
            methods: self.subst_iface_methods(&t.methods, sub)?,
            is_interface: t.is_interface,
            variants,
            is_enum: t.is_enum,
            distinct_base: match t.distinct_base {
                Some(b) => Some(self.subst_ty(b, sub, t.span)?),
                None => None,
            },
            is_const: t.is_const,
            span: t.span,
        })
    }
}
