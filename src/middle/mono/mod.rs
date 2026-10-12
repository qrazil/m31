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

use crate::front::ast::*;
use crate::front::diag::{Diag, Span};

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

mod body;
mod infer;
mod items;
mod types;

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
            return crate::front::ast::bare(name).to_string();
        };
        let args: Vec<String> = args
            .iter()
            .map(|a| match a {
                Ty::User(i) => self.show_out(&self.out_exprs[*i as usize].name),
                t => self.ty_key(*t),
            })
            .collect();
        format!("{}<{}>", crate::front::ast::bare(base), args.join(", "))
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

    fn env_ty(&self, name: &str) -> Option<Ty> {
        self.env.iter().rev().find_map(|s| s.get(name).copied())
    }
}
