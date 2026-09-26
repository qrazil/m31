//! Monomorphisation.
//!
//! Generics are erased here, before lowering, which is the whole reason
//! docs/ir-v0.md never mentions them: the IR only ever sees concrete types.
//! That was decided before any of this existed, and it is why adding generics
//! required no IR change at all.
//!
//! The pass rewrites the program into an equivalent one with no type
//! parameters. `Wrap<int>` becomes a plain type named `Wrap$int`; `id<int>`
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
    /// Methods on generic types (`T Wrap<T>.get()`), instantiated alongside
    /// each instantiation of their type.
    generic_methods: Vec<Func>,
    /// Methods with type parameters of their own, by concrete receiver and
    /// method name, with the receiver's substitution already bound:
    /// `Wrap$int` -> `first` -> (decl, {T: int}).
    own_generic: HashMap<String, HashMap<String, (Func, Subst)>>,
    /// Pending method instantiations: (decl, substitution, receiver's output
    /// name, method's output name). A queue rather than done on the spot
    /// because a type is instantiated in the middle of substituting some
    /// other function, and a method body would clobber that one's `env`.
    method_queue: Vec<(Func, Subst, String, String)>,
    /// Every type declaration of the input, generic or not, by name: for a
    /// method's receiver fields and for telling an enum variant from a
    /// static method.
    decls: HashMap<String, TypeDecl>,
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
    /// The receiver's OUTPUT type name (`Picker`, `Wrap$int`) while an
    /// instance method is being substituted; `None` anywhere else. Unlike
    /// `recv_ty` it is already concrete, so it can key `own_generic`: it is
    /// what lets a bare `pick(xs)` inside a method, or `this.pick(xs)`,
    /// instantiate the receiver's own generic method. Inside a method of a
    /// generic type the written receiver is just `Wrap`, which resolves
    /// to nothing, so only the instantiation being emitted can say it.
    cur_recv: Option<String>,
    /// Every module in the program, to tell `lib.f(..)` -- a call into a
    /// module -- from a method call on a value.
    modules: HashSet<String>,
    /// What each file imports. A module's name is in scope only in the file
    /// that imported it (§2.1), so this and `modules` together decide
    /// whether `lib.f(..)` is a qualifier -- and the lowering settles it the
    /// same way, which the two have to agree on.
    imports_by_module: HashMap<String, Vec<String>>,
    /// The current function's return type as written, for inferring a
    /// generic call's type arguments from `return f(..)`. None at the top
    /// level, which returns nothing.
    cur_ret: Option<Ty>,
    /// Every non-generic free function's parameters as written, so an
    /// argument can be inferred from the parameter it is passed to.
    func_params: HashMap<String, Vec<Param>>,
    /// Every module constant's type as written, by qualified name
    /// (`lib#MAX`). A constant passed to a generic function says what its
    /// type argument is, the way a local's declared type does; and
    /// `lib.TABLE.size()` has to be told from a member of a type `lib.TABLE`.
    const_tys: HashMap<String, Ty>,
}

/// A substitution from type parameter name to concrete type.
type Subst = HashMap<String, Ty>;

impl Mono {
    pub fn run(p: Program) -> Result<Program, Diag> {
        let mut m = Mono {
            generic_types: HashMap::new(),
            generic_funcs: HashMap::new(),
            generic_methods: Vec::new(),
            own_generic: HashMap::new(),
            method_queue: Vec::new(),
            decls: HashMap::new(),
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
            cur_recv: None,
            modules: p.imports_by_module.keys().cloned().collect(),
            imports_by_module: p.imports_by_module.clone(),
            cur_ret: None,
            func_params: HashMap::new(),
            const_tys: p.consts.iter().map(|c| (c.name.clone(), c.ty)).collect(),
        };

        let mut concrete_types = Vec::new();
        for t in p.types.iter().chain(p.prelude.iter()).cloned() {
            m.decls.insert(t.name.clone(), t.clone());
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
            if !f.recv_tparams.is_empty() {
                m.check_generic_recv(&f)?;
                m.generic_methods.push(f);
            } else if f.tparams.is_empty() {
                if f.recv.is_none() {
                    m.func_params.insert(f.name.clone(), f.params.clone());
                }
                concrete_funcs.push(f);
            } else if let Some(r) = f.recv.clone() {
                // Keyed by receiver, never with the free functions: a
                // generic method `first` and a generic function `first` are
                // two different things.
                m.own_generic
                    .entry(r)
                    .or_default()
                    .insert(f.name.clone(), (f, Subst::new()));
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
            let recv = f.recv.clone().filter(|_| !f.is_static);
            let f = m.subst_func(&f, &empty, recv)?;
            m.out_funcs.push(f);
        }

        // The top level is a function body in all but name.
        m.env.clear();
        m.env.push(HashMap::new());
        // Top-level statements belong to the entry module, so a bare call in
        // them resolves against it -- the same rule as inside a function.
        m.cur_module = p.module.clone();
        m.cur_ret = None;
        let toplevel = m.subst_block(&p.toplevel, &empty)?;
        m.env.clear();

        // A constant's type is resolved like any written type, which is what
        // instantiates `Array$int` when nothing but a constant spells it.
        // Its initialiser goes through the same substitution as any other
        // expression so a `lib.T.size()` inside one is repaired too; there
        // are no locals, so the environment is one empty scope.
        let mut consts = Vec::new();
        for c in &p.consts {
            m.env.push(HashMap::new());
            m.cur_module = c.module.clone();
            let init = m
                .subst_expr_as(&c.init, &empty, Some(c.ty))
                .map_err(|d| d.in_module(&c.module))?;
            m.env.clear();
            consts.push(ConstDecl {
                ty: m
                    .subst_ty(c.ty, &empty, c.span)
                    .map_err(|d| d.in_module(&c.module))?,
                init,
                ..c.clone()
            });
        }

        loop {
            if let Some((name, args, span)) = m.queue.pop() {
                m.instantiate_func(&name, &args, span)?;
            } else if let Some((f, sub, recv, name)) = m.method_queue.pop() {
                m.instantiate_method(&f, &sub, &recv, &name)?;
            } else {
                break;
            }
        }
        let generic_methods = m
            .own_generic
            .iter()
            .flat_map(|(r, ms)| ms.keys().map(move |n| format!("{r}.{n}")))
            .collect();

        Ok(Program {
            module: p.module.clone(),
            imports: Vec::new(),
            imports_by_module: p.imports_by_module.clone(),
            types: m.out_types,
            // Monomorphisation emits concrete instantiations into `types`;
            // past this point there is no generic Option left to keep apart.
            prelude: Vec::new(),
            funcs: m.out_funcs,
            consts,
            toplevel,
            ty_exprs: m.out_exprs,
            shown: m.shown,
            generic_methods,
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

    /// `Wrap<int, str>` becomes `Wrap$int$str`. `$` cannot appear in a source
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

    /// An output type's name as the reader wrote it, for a diagnostic:
    /// `Wrap<int>` for `Wrap$int`, `Point` for `lib#Point`. Only the
    /// names the reader's own module uses, which is all a receiver can be.
    fn show_out(&self, name: &str) -> String {
        let Some((base, args)) = self.shown.get(name) else {
            return crate::ast::bare(name).to_string();
        };
        let args: Vec<String> = args
            .iter()
            .map(|a| match a {
                Ty::User(i) => self.show_out(&self.out_exprs[*i as usize].name),
                t => self.ty_key(*t),
            })
            .collect();
        format!("{}<{}>", crate::ast::bare(base), args.join(", "))
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
            let ty = self.subst_ty(f.ty, &sub, f.span)?;
            // A field has to hold something. Unlike a payload, it cannot
            // simply vanish: a construction names it, and so does every read.
            if ty == Ty::Void {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{}` cannot be instantiated with `void`: its field `{}` would hold no value",
                        crate::ast::bare(&decl.name),
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
        let mut f = self.subst_func(&decl, &sub, None)?;
        f.name = mangled;
        f.tparams = Vec::new();
        self.out_funcs.push(f);
        Ok(())
    }

    /// Emit one method at one receiver: `Wrap<T>.get` at `Wrap$int`, or
    /// `Wrap<T>.map<U>` at `Wrap$int` as `map$str`.
    fn instantiate_method(
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
    fn check_generic_recv(&self, f: &Func) -> Result<(), Diag> {
        let r = f
            .recv
            .as_deref()
            .expect("receiver type parameters need a receiver");
        let shown = crate::ast::bare(r);
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
                        // A tuple is a builtin shape, not a module's type:
                        // its slots are as public as the tuple.
                        is_pub: true,
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

    /// Is `name` a module whose name is in scope in the file being
    /// substituted? The lowering's `module_in_scope`, and it has to stay
    /// that: a qualifier the two disagree about is a generic function that
    /// is never instantiated, or a name that resolves two ways.
    fn module_in_scope(&self, name: &str) -> bool {
        self.modules.contains(name)
            && (self.cur_module == name
                || self
                    .imports_by_module
                    .get(&self.cur_module)
                    .is_some_and(|v| v.iter().any(|m| m == name)))
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
                    is_pub: false,
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
                is_pub: f.is_pub,
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

    /// `recv` is the receiver's output type name for an instance method
    /// (`Program`-level name, already instantiated), `None` for a function
    /// or a static method -- see `cur_recv`.
    fn subst_func(&mut self, f: &Func, sub: &Subst, recv: Option<String>) -> Result<Func, Diag> {
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
            recv: f.recv.clone(),
            name: f.name.clone(),
            tparams: Vec::new(),
            recv_tparams: Vec::new(),
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
                    for b in &a.binds {
                        binds.push(Param {
                            ty: self.subst_ty(b.ty, sub, b.span)?,
                            name: b.name.clone(),
                            default: None,
                            embedded: false,
                            is_pub: false,
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
        })
    }

    fn subst_expr(&mut self, e: &Expr, sub: &Subst) -> Result<Expr, Diag> {
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
    fn subst_expr_as(&mut self, e: &Expr, sub: &Subst, want: Option<Ty>) -> Result<Expr, Diag> {
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
                        crate::ast::bare(n)
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
                        let bare = crate::ast::bare(&mangled).to_string();
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
    fn generic_method_call(
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
    fn generic_call(
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
    fn subst_args_as(&mut self, a: &Args, sub: &Subst, params: &[Param]) -> Result<Args, Diag> {
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
    /// type parameter must therefore be determined by a parameter position.
    /// Matching is structural over the WRITTEN types (`List<T>` against a
    /// local declared `List<int>` binds `T`), but an argument only has a
    /// written type if it is a literal, a construction or a local -- see
    /// `arg_ty` and `unify_arg`.
    fn infer(
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
                            crate::ast::bare(&decl.name)
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

    /// The output name of a method call's receiver type, when the receiver
    /// has a written type -- the same few shapes inference reads.
    fn recv_name(&mut self, obj: &Expr, sub: &Subst) -> Option<String> {
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
    fn unify_arg(
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
    fn arg_ty(&mut self, e: &Expr) -> Option<Ty> {
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
