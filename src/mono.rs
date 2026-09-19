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
    out_types: Vec<TypeDecl>,
    out_funcs: Vec<Func>,
    /// Pending function instantiations: (generic name, type arguments).
    queue: Vec<(String, Vec<Ty>, Span)>,
    /// Declared types of locals, as WRITTEN (unsubstituted), so inference can
    /// unify structurally against them. Cleared per function.
    env: Vec<HashMap<String, Ty>>,
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
            out_types: Vec::new(),
            out_funcs: Vec::new(),
            queue: Vec::new(),
            env: Vec::new(),
        };

        let mut concrete_types = Vec::new();
        for t in p.types.clone() {
            if t.tparams.is_empty() {
                concrete_types.push(t);
            } else {
                m.generic_types.insert(t.name.clone(), t);
            }
        }
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
        let toplevel = m.subst_block(&p.toplevel, &empty)?;
        m.env.clear();

        while let Some((name, args, span)) = m.queue.pop() {
            m.instantiate_func(&name, &args, span)?;
        }

        Ok(Program {
            types: m.out_types,
            funcs: m.out_funcs,
            toplevel,
            ty_exprs: m.out_exprs,
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
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
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

        if let Some(decl) = self.generic_types.get(&e.name).cloned() {
            if args.len() != decl.tparams.len() {
                return Err(Diag::new(
                    span,
                    format!(
                        "type `{}` takes {} type argument(s), found {}",
                        e.name,
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
            return Err(Diag::new(span, format!("type `{}` is not generic", e.name)));
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
                span: f.span,
            });
        }
        let _ = span;
        self.out_types.push(TypeDecl {
            name: mangled.to_string(),
            tparams: Vec::new(),
            fields,
            span: decl.span,
        });
        Ok(())
    }

    fn instantiate_func(&mut self, name: &str, args: &[Ty], span: Span) -> Result<(), Diag> {
        let mangled = self.mangle(name, args);
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
                    "function `{name}` takes {} type argument(s), found {}",
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

    fn subst_type_decl(&mut self, t: &TypeDecl, sub: &Subst) -> Result<TypeDecl, Diag> {
        let mut fields = Vec::new();
        for f in &t.fields {
            fields.push(Param {
                ty: self.subst_ty(f.ty, sub, f.span)?,
                name: f.name.clone(),
                span: f.span,
            });
        }
        Ok(TypeDecl {
            name: t.name.clone(),
            tparams: Vec::new(),
            fields,
            span: t.span,
        })
    }

    fn subst_func(&mut self, f: &Func, sub: &Subst) -> Result<Func, Diag> {
        let ret = self.subst_ty(f.ret, sub, f.span)?;
        let mut params = Vec::new();
        let mut scope = HashMap::new();
        for p in &f.params {
            scope.insert(p.name.clone(), p.ty);
            params.push(Param {
                ty: self.subst_ty(p.ty, sub, p.span)?,
                name: p.name.clone(),
                span: p.span,
            });
        }
        self.env.clear();
        self.env.push(scope);
        let body = self.subst_block(&f.body, sub)?;
        self.env.clear();
        Ok(Func {
            ret,
            name: f.name.clone(),
            tparams: Vec::new(),
            params,
            body,
            span: f.span,
        })
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
            Stmt::While { cond, body, span } => Stmt::While {
                cond: self.subst_expr(cond, sub)?,
                body: self.subst_block(body, sub)?,
                span: *span,
            },
            Stmt::Break { span } => Stmt::Break { span: *span },
            Stmt::Continue { span } => Stmt::Continue { span: *span },
        })
    }

    fn subst_expr(&mut self, e: &Expr, sub: &Subst) -> Result<Expr, Diag> {
        Ok(match e {
            Expr::Int(..) | Expr::Bool(..) | Expr::Str(..) | Expr::Var(..) => e.clone(),
            Expr::Bin(op, l, r, s) => Expr::Bin(
                *op,
                Box::new(self.subst_expr(l, sub)?),
                Box::new(self.subst_expr(r, sub)?),
                *s,
            ),
            Expr::Un(op, x, s) => Expr::Un(*op, Box::new(self.subst_expr(x, sub)?), *s),
            Expr::Field(o, f, s) => Expr::Field(Box::new(self.subst_expr(o, sub)?), f.clone(), *s),
            Expr::New(ty, args, s) => {
                let ty = self.subst_ty(*ty, sub, *s)?;
                let mut out = Vec::new();
                for (n, a) in args {
                    out.push((n.clone(), self.subst_expr(a, sub)?));
                }
                Expr::New(ty, out, *s)
            }
            Expr::Call(name, args, s) => {
                let mut out = Vec::new();
                for a in args {
                    out.push(self.subst_expr(a, sub)?);
                }
                // A call to a generic function needs its type arguments
                // inferred from the argument types, then the instantiation
                // queued and the name rewritten to the mangled one. Inference
                // is deliberately shallow -- see `infer`.
                if let Some(decl) = self.generic_funcs.get(name).cloned() {
                    let targs = self.infer(&decl, &out, sub, *s)?;
                    let mangled = self.mangle(name, &targs);
                    self.queue.push((name.clone(), targs, *s));
                    return Ok(Expr::Call(mangled, out, *s));
                }
                Expr::Call(name.clone(), out, *s)
            }
        })
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
        if args.len() != decl.params.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` takes {} argument(s), found {}",
                    decl.name,
                    decl.params.len(),
                    args.len()
                ),
            ));
        }
        let mut found: Subst = Subst::new();
        for (p, a) in decl.params.iter().zip(args.iter()) {
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
            _ => None,
        }
    }
}
