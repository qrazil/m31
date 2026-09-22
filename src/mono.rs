//! Monomorphisation.
//!
//! Generics are erased here, before lowering, which is the whole reason
//! docs/ir-v0.md never mentions them: the IR only ever sees concrete types.
//! That was decided before any of this existed, and it is why adding generics
//! required no IR change at all.
//!
//! The pass rewrites the program into an equivalent one with no type
//! parameters. `Box<int>` becomes a plain type named `Box$int`; `id<int>`
//! becomes a function named `id$int`. Everything downstream — the type
//! checker, the lowering, the backends — is unchanged and unaware.
//!
//! Instantiation is a worklist over the reachable set, so an unused generic
//! is never instantiated and never type-checked against types it was not
//! written for.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diag, Span};

pub struct Mono {
    /// Generic declarations, by name.
    generic_types: HashMap<String, TypeDecl>,
    generic_funcs: HashMap<String, Func>,
    /// The interned type expressions of the input program.
    src_exprs: Vec<TyExpr>,
    /// The output program's arena, built as we go.
    out_exprs: Vec<TyExpr>,
    /// Instantiations already emitted, by mangled name.
    done: HashSet<String>,
    /// Each instantiation's source spelling, by mangled name (`Program::shown`).
    shown: HashMap<String, (String, Vec<Ty>)>,
    out_types: Vec<TypeDecl>,
    out_funcs: Vec<Func>,
    /// Pending function instantiations: (generic name, type arguments).
    queue: Vec<(String, Vec<Ty>, Span)>,
    /// Declared types of locals, as WRITTEN (unsubstituted), so inference can
    /// unify structurally against them. Cleared per function.
    env: Vec<HashMap<String, Ty>>,
    /// The module whose function is being substituted, for resolving a bare
    /// call name to this module's own declaration.
    cur_module: String,
    /// The receiver's type as written, in `src_exprs`, while an instance
    /// method is being substituted; `None` anywhere else. It is what `this`
    /// contributes when it is passed to a generic function and the type
    /// argument has to be inferred from it.
    recv_ty: Option<Ty>,
}

/// A substitution from type parameter name to concrete type.
type Subst = HashMap<String, Ty>;

impl Mono {
    pub fn run(p: Program) -> Result<Program, Diag> {
        let mut m = Mono {
            generic_types: HashMap::new(),
            generic_funcs: HashMap::new(),
            src_exprs: p.ty_exprs.clone(),
            out_exprs: Vec::new(),
            done: HashSet::new(),
            shown: HashMap::new(),
            out_types: Vec::new(),
            out_funcs: Vec::new(),
            queue: Vec::new(),
            env: Vec::new(),
            cur_module: String::new(),
            recv_ty: None,
        };

        let mut concrete_types = Vec::new();
        for t in p.types.iter().chain(p.prelude.iter()).cloned() {
            if t.tparams.is_empty() {
                concrete_types.push(t);
            } else {
                m.generic_types.insert(t.name.clone(), t);
            }
        }
        // Two instantiations every program can reach through a built-in
        // method whose return type the source never spells: `split` gives a
        // List<str>, and every `index_of` gives an Option<int>. Nothing else
        // would create them, and the alternative -- working out from the
        // syntax whether the program could possibly call one -- is a guess
        // where this is a fact. A container carries no drop or walk
        // function, and Option<int> holds no reference, so an unused one
        // costs a typedef.
        let span = Span::new(1, 1);
        m.builtin_decl("List", &[Ty::Str], span);
        m.enum_decl("Option", &[Ty::Int], span)?;
        // `parse_float` gives one of these, and nothing in the source need
        // ever spell it. Same fact-not-guess reasoning as List<str> above.
        m.enum_decl("Option", &[Ty::Float], span)?;
        // `bytes` has the same two: `split` gives a List<bytes> and `utf8`
        // an Option<str>, and a program can reach either without spelling
        // it -- `s.to_bytes().utf8()` never writes a type at all. Declared
        // unconditionally for the same reason as List<str>; the Option
        // costs one small drop and walk function a program may not call.
        m.builtin_decl("List", &[Ty::Bytes], span);
        m.enum_decl("Option", &[Ty::Str], span)?;

        let mut concrete_funcs = Vec::new();
        for f in p.funcs.clone() {
            if f.tparams.is_empty() {
                concrete_funcs.push(f);
            } else {
                m.generic_funcs.insert(f.name.clone(), f);
            }
        }

        // Seed with everything non-generic. Anything a generic declaration
        // needs is discovered from here.
        let empty = Subst::new();
        for t in concrete_types {
            let t = m.subst_type_decl(&t, &empty)?;
            m.out_types.push(t);
        }
        for f in concrete_funcs {
            let f = m.subst_func(&f, &empty)?;
            m.out_funcs.push(f);
        }

        // The top level is a function body in all but name.
        m.env.clear();
        m.env.push(HashMap::new());
        // Top-level statements belong to the entry module, so a bare call in
        // them resolves against it -- the same rule as inside a function.
        m.cur_module = p.module.clone();
        let toplevel = m.subst_block(&p.toplevel, &empty)?;
        m.env.clear();

        while let Some((name, args, span)) = m.queue.pop() {
            m.instantiate_func(&name, &args, span)?;
        }

        Ok(Program {
            module: p.module.clone(),
            imports: Vec::new(),
            imports_by_module: p.imports_by_module.clone(),
            types: m.out_types,
            // Monomorphisation emits concrete instantiations into `types`;
            // past this point there is no generic Option left to keep apart.
            prelude: Vec::new(),
            funcs: m.out_funcs,
            toplevel,
            ty_exprs: m.out_exprs,
            shown: m.shown,
        })
    }

    fn intern(&mut self, name: String, args: Vec<Ty>) -> Ty {
        let e = TyExpr { name, args };
        match self.out_exprs.iter().position(|x| *x == e) {
            Some(i) => Ty::User(i as u32),
            None => {
                self.out_exprs.push(e);
                Ty::User((self.out_exprs.len() - 1) as u32)
            }
        }
    }

    /// `Box<int, str>` becomes `Box$int$str`. `$` cannot appear in a source
    /// identifier, so a mangled name can never collide with a declared one.
    fn mangle(&self, name: &str, args: &[Ty]) -> String {
        if args.is_empty() {
            return name.to_string();
        }
        let parts: Vec<String> = args.iter().map(|a| self.ty_key(*a)).collect();
        format!("{name}${}", parts.join("$"))
    }

    /// A stable name for an already-resolved type, used to build mangled
    /// names. Only called on output types, whose arguments are empty.
    fn ty_key(&self, t: Ty) -> String {
        match t {
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Void => "void".into(),
            Ty::User(i) => self.out_exprs[i as usize].name.clone(),
        }
    }

    /// Resolve a type written inside a declaration into a concrete output
    /// type, applying `sub` to any type parameters and instantiating any
    /// generic type it names.
    fn subst_ty(&mut self, t: Ty, sub: &Subst, span: Span) -> Result<Ty, Diag> {
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
                        crate::ast::bare(&e.name),
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
                format!("type `{}` is not generic", crate::ast::bare(&e.name)),
            ));
        }
        Ok(self.intern(e.name, Vec::new()))
    }

    fn instantiate_type(
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
            fields.push(Param {
                ty: self.subst_ty(f.ty, &sub, f.span)?,
                name: f.name.clone(),
                default: match &f.default {
                    Some(e) => Some(self.subst_expr(e, &sub)?),
                    None => None,
                },
                embedded: f.embedded,
                span: f.span,
            });
        }
        // A variant's payload types substitute like a field's.
        let mut variants = Vec::new();
        for v in &decl.variants {
            let mut payload = Vec::new();
            for t in &v.payload {
                payload.push(self.subst_ty(*t, &sub, v.span)?);
            }
            variants.push(EnumVariant {
                name: v.name.clone(),
                payload,
                span: v.span,
            });
        }
        let _ = span;
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
            span: decl.span,
        });
        Ok(())
    }

    fn instantiate_func(&mut self, name: &str, args: &[Ty], span: Span) -> Result<(), Diag> {
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
                    crate::ast::bare(name),
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
        let mut f = self.subst_func(&decl, &sub)?;
        f.name = mangled;
        f.tparams = Vec::new();
        self.out_funcs.push(f);
        Ok(())
    }

    /// Instantiate one of the compiler's own generic enums, for a built-in
    /// method whose return type the source never spells.
    fn enum_decl(&mut self, name: &str, args: &[Ty], span: Span) -> Result<(), Diag> {
        let Some(decl) = self.generic_types.get(name).cloned() else {
            return Ok(());
        };
        let mangled = self.mangle(name, args);
        self.instantiate_type(&decl, &mangled, args, span)
    }

    /// Declare one instantiation of a runtime-owned container, and return its
    /// mangled name. The runtime owns the layout, so the declaration only has
    /// to carry the type arguments for the rest of the compiler to read.
    fn builtin_decl(&mut self, name: &str, args: &[Ty], span: Span) -> String {
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
                        span,
                    })
                    .collect(),
                methods: Vec::new(),
                is_interface: false,
                variants: Vec::new(),
                is_enum: false,
                distinct_base: None,
                span,
            });
        }
        mangled
    }

    /// A bare call name, resolved against the module being substituted: a
    /// generic function is declared `mod#id` but called `id` from inside its
    /// own module, exactly as in the lowering.
    fn resolve_fn(&self, name: &str) -> String {
        if !self.cur_module.is_empty() {
            let q = format!("{}#{name}", self.cur_module);
            if self.generic_funcs.contains_key(&q) {
                return q;
            }
        }
        name.to_string()
    }

    /// An interface's method SIGNATURES substitute like anything else.
    ///
    /// They used to be cloned unchanged, so `interface Getter<T> { T get(); }`
    /// at `Getter<int>` still required a method returning `T` -- which after
    /// interning resolved to the instantiated interface itself. No concrete
    /// type could satisfy a generic interface at all.
    fn subst_iface_methods(&mut self, ms: &[Func], sub: &Subst) -> Result<Vec<Func>, Diag> {
        let mut out = Vec::new();
        for m in ms {
            let mut params = Vec::new();
            for p in &m.params {
                params.push(Param {
                    ty: self.subst_ty(p.ty, sub, p.span)?,
                    name: p.name.clone(),
                    default: None,
                    embedded: false,
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
                params,
                body: Vec::new(),
                span: m.span,
            });
        }
        Ok(out)
    }

    fn subst_type_decl(&mut self, t: &TypeDecl, sub: &Subst) -> Result<TypeDecl, Diag> {
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
            span: t.span,
        })
    }

    fn subst_func(&mut self, f: &Func, sub: &Subst) -> Result<Func, Diag> {
        self.cur_module = f.module.clone();
        let ret = self.subst_ty(f.ret, sub, f.span)?;
        let mut params = Vec::new();
        let mut scope = HashMap::new();
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
                span: p.span,
            });
        }
        self.env.clear();
        self.env.push(scope);
        self.recv_ty = match &f.recv {
            Some(r) if !f.is_static => Some(self.src_ty_named(r)),
            _ => None,
        };
        let body = self.subst_block(&f.body, sub);
        self.recv_ty = None;
        let body = body?;
        self.env.clear();
        Ok(Func {
            module: f.module.clone(),
            is_pub: f.is_pub,
            ret,
            is_static: f.is_static,
            is_prim: f.is_prim,
            recv: f.recv.clone(),
            name: f.name.clone(),
            tparams: Vec::new(),
            params,
            body,
            span: f.span,
        })
    }

    /// A plain type name in the source arena, interned if the source never
    /// spelled it as a type expression -- a type named only in its own
    /// declaration and its methods' receivers has no entry of its own.
    fn src_ty_named(&mut self, name: &str) -> Ty {
        let e = TyExpr {
            name: name.to_string(),
            args: Vec::new(),
        };
        if let Some(i) = self.src_exprs.iter().position(|x| *x == e) {
            return Ty::User(i as u32);
        }
        self.src_exprs.push(e);
        Ty::User((self.src_exprs.len() - 1) as u32)
    }

    fn subst_block(&mut self, stmts: &[Stmt], sub: &Subst) -> Result<Vec<Stmt>, Diag> {
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

    fn env_ty(&self, name: &str) -> Option<Ty> {
        self.env.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn subst_stmt(&mut self, s: &Stmt, sub: &Subst) -> Result<Stmt, Diag> {
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
                let init = self.subst_expr(init, sub)?;
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
                value: self.subst_expr(value, sub)?,
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
                    Some(e) => Some(self.subst_expr(e, sub)?),
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
                    for b in &a.binds {
                        binds.push(Param {
                            ty: self.subst_ty(b.ty, sub, b.span)?,
                            name: b.name.clone(),
                            default: None,
                            embedded: false,
                            span: b.span,
                        });
                    }
                    self.env.push(HashMap::new());
                    for b in &a.binds {
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
                    let targs = self.infer(&decl, &out.pos, sub, *span)?;
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
        })
    }

    fn subst_expr(&mut self, e: &Expr, sub: &Subst) -> Result<Expr, Diag> {
        Ok(match e {
            Expr::Int(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::Str(..)
            | Expr::Var(..)
            | Expr::This(..) => e.clone(),
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
                let ty = self.subst_ty(*ty, sub, *s)?;
                Expr::EnumNew(ty, variant.clone(), self.subst_args(args, sub)?, *s)
            }
            Expr::MethodCall(obj, m, args, s) => Expr::MethodCall(
                Box::new(self.subst_expr(obj, sub)?),
                m.clone(),
                self.subst_args(args, sub)?,
                *s,
            ),
            Expr::Call(name, args, s) => {
                let out = self.subst_args(args, sub)?;
                // A call to a generic function needs its type arguments
                // inferred from the argument types, then the instantiation
                // queued and the name rewritten to the mangled one. Inference
                // is deliberately shallow -- see `infer`.
                let name = &self.resolve_fn(name);
                if let Some(decl) = self.generic_funcs.get(name).cloned() {
                    // Infer from the arguments AS WRITTEN, not from `out`.
                    // There are two type arenas -- `src_exprs` for the input
                    // program and `out_exprs` for what substitution produces
                    // -- and `unify` reads `src_exprs`. A substituted
                    // `Expr::New` carries an `out_exprs` index, so unifying
                    // against it indexed the wrong arena and inference
                    // failed for a constructed temporary while succeeding
                    // for a local. `unify` substitutes what it binds, which
                    // is what `sub` is threaded through for.
                    let targs = self.infer(&decl, &args.pos, sub, *s)?;
                    let mangled = self.mangle(name, &targs);
                    self.queue.push((name.clone(), targs, *s));
                    return Ok(Expr::Call(mangled, out, *s));
                }
                Expr::Call(name.clone(), out, *s)
            }
        })
    }

    fn subst_args(&mut self, a: &Args, sub: &Subst) -> Result<Args, Diag> {
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

    /// Infer a generic function's type arguments from its call.
    ///
    /// There is no explicit `f<int>(x)` syntax, on purpose: after a name that
    /// is not known to be a type, `<` is ambiguous with comparison — the
    /// problem that pushed Go to `f[int](x)` and Rust to a turbofish. Every
    /// type parameter must therefore be determined by a parameter position,
    /// and this matches shallowly: a parameter written exactly as `T` binds
    /// `T` to that argument's type. Nested matching (`List<T>` against
    /// `List<int>`) is not attempted yet.
    fn infer(
        &mut self,
        decl: &Func,
        args: &[Expr],
        sub: &Subst,
        span: Span,
    ) -> Result<Vec<Ty>, Diag> {
        // Only mandatory parameters are positional, so those are what the
        // positional arguments line up with. Arity itself is checked later,
        // by the lowering, against a better error.
        let mandatory: Vec<&Param> = decl.params.iter().filter(|p| !p.is_optional()).collect();
        let mut found: Subst = Subst::new();
        for (p, a) in mandatory.iter().zip(args.iter()) {
            if let Some(aty) = self.arg_ty(a) {
                self.unify(p.ty, aty, &decl.tparams, sub, span, &mut found);
            }
        }
        let mut out = Vec::new();
        for tp in &decl.tparams {
            match found.get(tp) {
                Some(t) => out.push(*t),
                None => {
                    return Err(Diag::new(
                        span,
                        format!(
                            "cannot infer type parameter `{tp}` of `{}` from these arguments; \
                             bind the argument to a local with a written type first",
                            decl.name
                        ),
                    ))
                }
            }
        }
        Ok(out)
    }

    /// Match a parameter's written type against an argument's written type,
    /// binding any type parameter it reaches. Structural, so `Box<T>` against
    /// `Box<int>` binds `T`, not only a bare `T`.
    fn unify(
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

    /// The written type of an argument expression, for inference only.
    /// Literals, constructions and locals cover the container cases; anything
    /// else leaves the parameter uninferred and produces a diagnostic rather
    /// than a wrong guess.
    fn arg_ty(&mut self, e: &Expr) -> Option<Ty> {
        match e {
            Expr::Int(..) => Some(Ty::Int),
            Expr::Bool(..) => Some(Ty::Bool),
            Expr::Str(..) => Some(Ty::Str),
            Expr::New(ty, ..) => Some(*ty),
            Expr::Var(n, _) => self.env_ty(n),
            Expr::This(_) => self.recv_ty,
            _ => None,
        }
    }
}
