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
//!   - a place read that nothing after it can disturb: `may_run_code` is false
//!     for every later operand and the operation itself runs no user code.
//!     Between the read and its last use only the runtime executes, and the
//!     runtime never writes a field or an element the program can see, except
//!     through the release of a reference -- see `releases`.
//!
//! This is also why a chain `a.b.c` costs nothing in between: each load is
//! consumed by the next one before anything else runs.

use super::{Lowerer, Val, DESTRUCTOR};
use crate::ast::*;
use crate::ir::Inst;

impl Lowerer {
    /// Hold `v`, the value `e` evaluated to, if it is borrowed from a place
    /// and `later` says something evaluated after it may run user code. See
    /// the module comment for the rule.
    pub(super) fn hold(&mut self, e: &Expr, v: Val, later: bool) -> Val {
        if !later || v.owned || v.v.is_none() || !self.is_ref(v.ty) || self.stable(e) {
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
            _ => false,
        }
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
            Expr::Call(..) | Expr::New(..) | Expr::Try(..) => true,
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
    /// The runtime touches the receiver after such a release (`rt_list_clear`
    /// goes on to the next element), so a destructor that reaches the place
    /// the receiver was read from and overwrites it would free the receiver
    /// under the runtime's feet.
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
    /// Only what `may_run_code` needs to tell a built-in operator or method
    /// from a user one; `None` means "do not know".
    fn static_ty(&self, e: &Expr, scoped: bool) -> Option<Ty> {
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
