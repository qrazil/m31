//! Keeping borrowed operands alive: the caller's half of docs/ir-v0.md §5.1.
//!
//! An argument is borrowed, so the CALLER must keep it alive for the whole
//! call. For a local, a parameter or `this` that is free: the caller's frame
//! (or its caller's) holds the reference, and no expression can reassign a
//! local. For an owned temporary it is free too: the statement holds it until
//! its end. For a string literal or a constant it is free because they are
//! immortal.
//!
//! It is NOT free for a value read out of a place some other code can reach
//! -- a field (`h.p`, or a bare field name inside a method) or an element
//! (`xs[0]`) -- nor, since `stable` decides by exclusion, for any other
//! borrowed value not in the list above. The only reference to such a value
//! may be the one the place holds, and if anything evaluated after the read
//! runs user code -- a later argument, or the call itself -- that code can
//! overwrite the place, drop the last reference, and free the value while it
//! is still in use:
//!
//!     void f(P p, H h) { h.p = P(6); print(p.x); }
//!     f(h.p, h);        // p is freed by the time it is printed
//!
//! So such an operand is *held*: retained as soon as it is read, and
//! registered as a temporary of the statement, which releases it after the
//! operation. A held operand is then indistinguishable from an owned
//! temporary, and everything downstream already knows what to do with one --
//! a consumer that stores it adopts the +1 instead of adding another.
//!
//! The retain goes right after the read, before any later operand is
//! lowered, because evaluation is left to right: in `f(h.p, g(h))` the call
//! `g(h)` can replace `h.p` before `f` is ever entered.
//!
//! What is NOT held, and why that is sound:
//!   - locals, parameters, `this`, literals and constants (above);
//!   - owned temporaries (above);
//!   - a place in a frozen object, which a store would trap on before it
//!     released anything (docs/const-decision.md): any chain of elements
//!     read out of a module constant, whose whole graph is immortal; and any
//!     chain of fields and List/Array elements out of a local or parameter
//!     that an active `const` block names, that a `const` declaration
//!     bound, or that is a `const` parameter, since every freeze is deep. Anything read through `this` is
//!     still held: see `frozen_place`;
//!   - a place read that nothing after it can disturb: `may_run_code` is false
//!     for every later operand and the operation itself runs no user code.
//!     Between the read and its last use only the runtime executes, and the
//!     runtime never writes a field or an element the program can see, except
//!     through the release of a reference -- see `releases`.
//!
//! This is also why a chain `a.b.c` costs nothing in between: each load is
//! consumed by the next one before anything else runs.

use super::{Lowerer, Val, DESTRUCTOR};
use crate::front::ast::*;
use crate::middle::ir::Inst;

impl Lowerer {
    /// Hold `v`, the value `e` evaluated to, if it is borrowed from a place
    /// and `later` says something evaluated after it may run user code. See
    /// the module comment for the rule.
    pub(super) fn hold(&mut self, e: &Expr, v: Val, later: bool) -> Val {
        // `is_managed`, not `is_ref`: a value enum is a copy, so there is no
        // place for it to be borrowed FROM and nothing to hold.
        if !later || v.owned || v.v.is_none() || !self.is_managed(v.ty) || self.stable(e) {
            return v;
        }
        let val = v.val();
        self.push(Inst::RcInc { val });
        self.stmt_temps.push(val);
        Val::new(val, v.ty, true)
    }

    /// For the arguments of one call or construction, in the order they are
    /// evaluated (`bind_args`): whether anything after argument `i` -- a
    /// later argument, or the operation itself when `op_runs_code` -- may run
    /// user code. A slot filled by the parameter's default is judged without
    /// the caller's locals, as it is lowered (`lower_slot`).
    pub(super) fn later_flags(
        &self,
        slots: &[&Expr],
        params: &[Param],
        op_runs_code: bool,
    ) -> Vec<bool> {
        let mut out = vec![op_runs_code; slots.len()];
        let mut acc = op_runs_code;
        for i in (0..slots.len()).rev() {
            out[i] = acc;
            let is_default = params
                .get(i)
                .and_then(|p| p.default.as_ref())
                .is_some_and(|d| std::ptr::eq(d, slots[i]));
            acc = acc || self.may_run_code(slots[i], !is_default);
        }
        out
    }

    /// `later_flags` for operands the caller wrote, with nothing after them.
    pub(super) fn later_in(&self, es: &[Expr]) -> Vec<bool> {
        let refs: Vec<&Expr> = es.iter().collect();
        self.later_flags(&refs, &[], false)
    }

    /// Whether the value `e` evaluates to is kept alive by something that
    /// outlives the operation it is an operand of, whatever else runs.
    fn stable(&self, e: &Expr) -> bool {
        match e {
            Expr::Str(..) | Expr::This(_) => true,
            // A local or parameter is held by a frame; a constant is
            // immortal. A bare field name inside a method is neither -- it
            // is a read of `this.name`, a place.
            Expr::Var(name, _) => {
                self.lookup(name).is_some()
                    || (self.recv_field(name).is_none() && self.resolve_const(name).is_some())
            }
            Expr::Field(..) | Expr::Index(..) => self.immortal_place(e) || self.frozen_place(e),
            _ => false,
        }
    }

    /// A module constant, or a chain of fields and elements read out of one:
    /// `lib.MAX`, `TABLE[i]`, `GRID[i][j]`. A constant's whole graph is
    /// static, immortal data (src/middle/lower/consts.rs), so every object
    /// reachable from it lives for the whole program and none of its slots
    /// can ever be written -- at any depth.
    pub(super) fn immortal_place(&self, e: &Expr) -> bool {
        if self.is_module_const(e) {
            return true;
        }
        match e {
            Expr::Field(o, _, _) | Expr::Index(o, _, _) => self.immortal_place(o),
            _ => false,
        }
    }

    /// A chain of fields and List/Array elements -- `a.f`, `a[i]`, `a.b.c`,
    /// `a.ps[i].x`, a promoted field -- read out of a local or parameter `a`
    /// whose object is frozen for at least as long as the operation runs:
    ///
    ///   - named by a `const` block that is active here (`freezes`): the
    ///     block cannot be left in the middle of a statement, and a block in
    ///     a callee that meets the object already frozen leaves it frozen
    ///     when it ends (`rt_freeze_leave` thaws only what its own
    ///     `rt_freeze_enter` froze);
    ///   - or bound by a `const` declaration (`snapshots`, with the binding
    ///     still `const`): `rt_snapshot` returns an object frozen for good;
    ///   - or a `const` parameter (`const_params`): every call site checked
    ///     its argument frozen (`refuse_unfrozen_arg`), by something that
    ///     lasts at least as long as the call -- a snapshot, a module
    ///     constant, or a `const` block or `const` parameter of a caller,
    ///     none of which can end while this frame runs.
    ///
    /// Every freeze is deep (docs/const-decision.md, "The third decision"):
    /// a block freezes everything the named object reaches, and a snapshot
    /// shares only parts that are frozen for good, copying any part a block
    /// froze. So every object on the chain is frozen too, and a store into
    /// any of them traps (`rt_check_mutable`: field stores, `rt_index_set`,
    /// and every List and Map method that drops or moves an element). Each
    /// slot read keeps the value it had, and each object is kept alive by
    /// the frozen slot it was read from, down from the local or parameter
    /// that keeps the root alive -- the same reason it is stable itself. A
    /// SSA value names one object for its whole life, so whichever name the
    /// frozen object is reached by, the answer is about the object.
    ///
    /// A promoted field is read through the embedded object, which is a
    /// separate object but reachable from the root, so it is frozen with it.
    ///
    /// Still held: anything read through `this` or a bare field name inside
    /// a method, because whether the receiver is frozen is the caller's
    /// knowledge, not this function's; and a step this function does not
    /// know the type of (`static_ty` gives up), since it cannot tell whether
    /// the step is a field of an object at all.
    pub(super) fn frozen_place(&self, e: &Expr) -> bool {
        let (obj, field) = match e {
            Expr::Field(o, f, _) => (&**o, Some(f)),
            Expr::Index(o, _, _) => (&**o, None),
            _ => return false,
        };
        let ty = match obj {
            Expr::Var(name, _) => {
                let Some(ty) = self.frozen_name(name) else {
                    return false;
                };
                ty
            }
            Expr::Field(..) | Expr::Index(..) => {
                if !self.frozen_place(obj) {
                    return false;
                }
                let Some(ty) = self.static_ty(obj, true) else {
                    return false;
                };
                ty
            }
            _ => return false,
        };
        match field {
            // A field of a user struct, declared on the type or promoted
            // from an embedded one.
            Some(f) => {
                if self.seq_elem(ty).is_some() || self.map_kv(ty).is_some() {
                    return false;
                }
                let Some(tid) = self.tdef_of(ty) else {
                    return false;
                };
                let td = &self.typedefs[tid as usize];
                !td.is_enum && !td.has_no_object() && self.field_path(tid, f).is_some()
            }
            // An element of a List or an Array, which the runtime refuses to
            // change while it is frozen. A `bytes` element is an int, and
            // nothing else can be indexed.
            None => self.seq_elem(ty).is_some(),
        }
    }

    /// The type of the local or parameter `name` when the object it holds
    /// is frozen for at least as long as the current operation: named by an
    /// active `const` block, bound by a `const` declaration, or a `const`
    /// parameter. The roots `frozen_place` follows chains out of; see there.
    pub(super) fn frozen_name(&self, name: &str) -> Option<Ty> {
        let (ty, val, is_const) = self.binding(name)?;
        let frozen = self.freezes.iter().any(|(v, _)| *v == val)
            || (is_const && (self.snapshots.contains(&val) || self.const_params.contains(&val)));
        frozen.then_some(ty)
    }

    /// Whether evaluating `e` might run user code: a function or method the
    /// program declared, an operator method, an interface dispatch, a
    /// destructor. Syntactic and conservative -- `true` whenever it cannot
    /// tell, since a wrong `true` costs a retain and a wrong `false` is a
    /// use-after-free.
    ///
    /// `scoped` is false for a parameter's default, which is lowered in the
    /// declaring module with no locals in scope, so the caller's locals must
    /// not be consulted for its types.
    pub(super) fn may_run_code(&self, e: &Expr, scoped: bool) -> bool {
        let any = |es: &[Expr]| es.iter().any(|x| self.may_run_code(x, scoped));
        match e {
            Expr::Int(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::Str(..)
            | Expr::This(_)
            | Expr::Var(..) => false,
            Expr::Field(o, _, _) => self.may_run_code(o, scoped),
            Expr::Index(o, i, _) => self.may_run_code(o, scoped) || self.may_run_code(i, scoped),
            Expr::Un(_, x, _) => self.may_run_code(x, scoped),
            Expr::Bin(op, l, r, _) => {
                if self.may_run_code(l, scoped) || self.may_run_code(r, scoped) {
                    return true;
                }
                match op {
                    // The right operand is lowered in its own block and its
                    // temporaries are released there, in the middle of the
                    // enclosing expression -- a release, and so a destructor.
                    BinOp::And | BinOp::Or => self.has_destructors,
                    // An operator method runs when the left operand is a
                    // user type (`lower_bin`); one that is not known is
                    // assumed to be one.
                    _ => match self.static_ty(l, scoped) {
                        Some(t) => self.has_operator_methods(t),
                        None => true,
                    },
                }
            }
            Expr::MethodCall(o, m, args, _) => {
                if self.may_run_code(o, scoped)
                    || any(&args.pos)
                    || args.named.iter().any(|(_, x)| self.may_run_code(x, scoped))
                {
                    return true;
                }
                match self.static_ty(o, scoped) {
                    Some(t) => self.method_runs_code(t, m),
                    // A module-qualified call, `lib.f()`, lands here too.
                    None => true,
                }
            }
            Expr::SeqLit(items, _) => any(items),
            Expr::RepeatLit(a, b, _) => {
                self.may_run_code(a, scoped) || self.may_run_code(b, scoped)
            }
            Expr::MapLit(items, _) => items
                .iter()
                .any(|(k, v)| self.may_run_code(k, scoped) || self.may_run_code(v, scoped)),
            // A variant's construction is an allocation; the same spelling
            // can also be a static method call, which is user code.
            Expr::EnumNew(ty, variant, args, _) => {
                let is_variant = self.tdef_of(*ty).is_some_and(|t| {
                    let td = &self.typedefs[t as usize];
                    td.is_enum && td.variants.iter().any(|v| v.name == *variant)
                });
                !is_variant || any(&args.pos)
            }
            // A call, a construction (whose field defaults are expressions
            // of their own), and `?` (which may return, releasing every
            // local) are all taken to run code.
            // A lambda is a construction too -- of the synthesised type
            // whose fields are its captures -- so it is treated as one.
            Expr::Call(..) | Expr::New(..) | Expr::Try(..) | Expr::Lambda(..) => true,
        }
    }

    /// Whether `a OP b` with `a` of type `t` dispatches to a method.
    pub(super) fn has_operator_methods(&self, t: Ty) -> bool {
        matches!(t, Ty::User(_)) && self.base_of(t).is_none()
    }

    /// Whether calling method `m` on a value of type `t` runs user code: a
    /// declared method or an interface dispatch always does; a built-in
    /// method of a collection, a `str`, a `bytes`, a number, an `Option` or
    /// a `Result` is the runtime, and runs user code only through a release
    /// (`releases`).
    pub(super) fn method_runs_code(&self, t: Ty, m: &str) -> bool {
        if let Some(tid) = self.tdef_of(t) {
            let td = &self.typedefs[tid as usize];
            if td.is_interface || self.sigs.contains_key(&format!("{}.{m}", td.name)) {
                return true;
            }
        }
        self.releases(t, m)
    }

    /// Whether built-in method `m` on `t` can run a destructor. Only the
    /// methods that drop references a collection holds can -- `clear` on a
    /// List, and `clear`, `remove` and `set` (which overwrites) on a Map --
    /// and only in a program that declares a destructor, since without one a
    /// release runs nothing but the runtime's own drop functions.
    ///
    /// The runtime no longer touches a receiver after releasing from it --
    /// the release paths detach first (docs/reentrancy-decision.md) -- so the
    /// hold on the receiver itself is belt-and-braces. It stays because this
    /// same answer is what makes `may_run_code` true for those methods, and
    /// so what holds every OTHER borrowed operand of the statement they
    /// appear in: a destructor reached from here can still free those.
    fn releases(&self, t: Ty, m: &str) -> bool {
        if !self.has_destructors {
            return false;
        }
        if self.is_list(t) {
            return m == "clear";
        }
        self.map_kv(t).is_some() && matches!(m, "clear" | "remove" | "set")
    }

    /// The type of `e` where it can be read off without lowering anything.
    /// What `may_run_code` needs to tell a built-in operator or method from
    /// a user one, and what a diagnostic needs to say what a name already
    /// is; `None` means "do not know".
    pub(super) fn static_ty(&self, e: &Expr, scoped: bool) -> Option<Ty> {
        match e {
            Expr::Int(..) => Some(Ty::Int),
            Expr::Float(..) => Some(Ty::Float),
            Expr::Bool(..) => Some(Ty::Bool),
            Expr::Str(..) => Some(Ty::Str),
            // Outside its scope -- a default, judged from the caller -- even
            // a constant would resolve against the wrong module, so a name
            // is not known there.
            Expr::Var(..) if !scoped => None,
            Expr::Var(name, _) => {
                if let Some((t, _)) = self.lookup(name) {
                    return Some(t);
                }
                if let Some((tid, _, path)) = self.recv_field(name) {
                    return Some(self.path_ty(tid, &path));
                }
                let key = self.resolve_const(name)?;
                Some(self.consts[&key].decl.ty)
            }
            Expr::Field(o, f, _) => {
                let tid = self.tdef_of(self.static_ty(o, scoped)?)?;
                let path = self.field_path(tid, f)?;
                Some(self.path_ty(tid, &path))
            }
            Expr::Index(o, _, _) => {
                let t = self.static_ty(o, scoped)?;
                if self.underlying(t) == Ty::Bytes {
                    return Some(Ty::Int);
                }
                self.seq_elem(t)
            }
            Expr::Un(_, x, _) => self.static_ty(x, scoped),
            Expr::MethodCall(o, m, args, _) if m == "size" && args.pos.is_empty() => {
                let t = self.static_ty(o, scoped)?;
                (!self.method_runs_code(t, m)).then_some(Ty::Int)
            }
            _ => None,
        }
    }

    /// The surface type at the end of a field path, as `load_path` walks it.
    fn path_ty(&self, tid: u32, path: &[u32]) -> Ty {
        let mut cur = tid;
        let mut ty = Ty::Void;
        for idx in path {
            ty = self.field_ty(cur, *idx);
            if let Some(next) = self.tdef_of(ty) {
                cur = next;
            }
        }
        ty
    }

    /// Whether the program declares any destructor, computed once.
    pub(super) fn any_destructor(&self) -> bool {
        let suffix = format!(".{DESTRUCTOR}");
        self.sigs.keys().any(|k| k.ends_with(&suffix))
    }
}
