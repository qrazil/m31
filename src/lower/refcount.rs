//! Refcount placement (`rc_inc`/`rc_dec`, scope release) and the
//! ownership rules it depends on: thread transfer and payload binding.

use super::{Lowerer, PayloadBind, Val, NO_CLONE, OWNER_MOVES};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{Inst, Value};

impl Lowerer {
    /// Hand a reference across a thread boundary -- `send` or `spawn`.
    ///
    /// Only a value this scope OWNS may cross -- an owned temporary, or a
    /// local this scope registered. It is a MOVE: transfer it, emit nothing,
    /// and refuse any later use.
    ///
    /// A BORROWED one -- an element, a field, a parameter, `this` -- cannot
    /// cross at all. An earlier draft retained it instead and handed the
    /// receiver its own +1, which is exactly the race moved-not-shared
    /// exists to prevent: two threads on one non-atomic count. `clone` is
    /// how you send something you also want to keep.
    ///
    /// A module constant is the one exception, below: immortal, so neither
    /// thread ever writes its count, and immutable, so there is nothing to
    /// race on.
    ///
    /// A `case V(T x):` binding is the other. It reads as borrowed, but it is
    /// the arm's own name for a payload the `match` alone holds, so it can be
    /// TAKEN out of the enum -- see `take_payload`.
    pub(super) fn transfer(&mut self, v: &Val, arg: &Expr, span: Span) -> Result<(), Diag> {
        if !self.is_ref(v.ty) || self.chan_elem(v.ty).is_some() {
            return Ok(());
        }
        // A value enum crossing a thread boundary. `rt_check_unique` walks an
        // object graph and there is no object here, and the move rules below
        // are stated about reference types -- so rather than reason about
        // what a copy means at a boundary, the type goes back to being a heap
        // object and the whole program is lowered again. Then a `spawn` of an
        // `Option<int>` behaves exactly as it did before this optimisation
        // existed, diagnostic for diagnostic. A channel's element is already
        // excluded by `value_enums`; this is `spawn`.
        if self.is_value_enum(v.ty) {
            return Err(self.box_it(v.ty, span, "cross a thread boundary"));
        }
        // A `case V(T x):` binding is moved by taking the payload out of the
        // enum, which has its own uniqueness check -- on the enum, which
        // covers the payload -- so it is decided before the one below.
        if let Expr::Var(n, s) = arg {
            if !v.owned && self.payload_bind(n).is_some() {
                return self.take_payload(n, v, *s);
            }
        }
        // The value must be UNIQUE when it crosses. Retaining a borrowed one
        // instead would leave two threads sharing a non-atomic refcount,
        // which is precisely the race moved-not-shared exists to prevent --
        // so a borrowed value cannot cross at all, and `clone` is the way to
        // send something you also want to keep.
        self.push(Inst::Call {
            dst: None,
            func: "rt_check_unique".to_string(),
            args: vec![v.val()],
        });

        if v.owned {
            self.stmt_temps.retain(|t| *t != v.val());
            return Ok(());
        }
        if let Expr::This(s) = arg {
            let msg = match self.resource_of(v.ty) {
                Some(t) => format!(
                    "`this` is borrowed from the caller and cannot cross a thread \
                     boundary, and `{t}` cannot be cloned -- {NO_CLONE}. {OWNER_MOVES}"
                ),
                None => "`this` is borrowed from the caller and cannot cross a thread \
                         boundary; send clone(this) instead"
                    .to_string(),
            };
            return Err(Diag::new(*s, msg));
        }
        // A module constant crosses as it is. It is immortal, so neither
        // thread ever writes its count -- the retain and release are
        // no-ops -- and it is immutable, so there is nothing to race on.
        // rt_check_unique above passes it for the same reason.
        if self.is_module_const(arg) {
            return Ok(());
        }
        if let Expr::Var(n, s) = arg {
            if self.owns_local(n) {
                return self.mark_moved(n, *s);
            }
            return Err(Diag::new(
                *s,
                format!(
                    "`{n}` is borrowed here and cannot cross a thread boundary; \
                     a reference two threads can reach would race on a non-atomic \
                     refcount. {}",
                    self.send_a_copy(n, v.ty)
                ),
            ));
        }
        let _ = span;
        Err(Diag::new(
            arg.span(),
            format!(
                "this value is borrowed from something else and cannot cross a thread \
                 boundary; {}",
                match self.resource_of(v.ty) {
                    Some(t) => format!("and `{t}` cannot be cloned -- {NO_CLONE}. {OWNER_MOVES}"),
                    None => "wrap it in clone(..) to send a copy".to_string(),
                }
            ),
        ))
    }

    /// How to send something you also want to keep -- or, for a type that
    /// owns a resource, the truth that you cannot.
    ///
    /// `clone` refuses a type with a destructor (docs/destructors-decision.md)
    /// and `net.Conn`, `net.Listener` and `io.File` are exactly the types a
    /// server hands to a worker, so advising `clone` there sends the reader
    /// to a second error. Name the spelling that works instead.
    fn send_a_copy(&self, n: &str, ty: Ty) -> String {
        match self.resource_of(ty) {
            Some(t) => format!("`{t}` cannot be cloned -- {NO_CLONE}. {OWNER_MOVES}"),
            None => format!("Use clone({n}) to send a copy."),
        }
    }

    /// The name of the type `ty` owns a resource as -- that is, the types
    /// `clone` refuses because they have a destructor. `None` for everything
    /// that can be copied.
    fn resource_of(&self, ty: Ty) -> Option<String> {
        let tid = self.tdef_of(ty)?;
        let td = &self.typedefs[tid as usize];
        if td.is_interface || td.is_chan || td.is_distinct {
            return None;
        }
        self.has_destructor(tid).then(|| self.tyname(ty))
    }

    /// The `case V(T x):` binding `n` names, if it is one and it is still in
    /// scope in the arm that bound it.
    fn payload_bind(&self, n: &str) -> Option<&PayloadBind> {
        self.payload_binds.iter().rev().find(|pb| pb.name == n)
    }

    /// Move a `case V(T x):` binding across a thread boundary by TAKING the
    /// payload out of the enum.
    ///
    /// The binding is the arm's own name for that payload and nothing else
    /// names it, so the only other reference is the enum's own -- and the
    /// enum is a temporary the `match` alone holds. Clearing the slot hands
    /// that +1 to the receiver, and the enum's release then skips the slot.
    ///
    /// Two conditions, and both are load-bearing:
    ///
    /// - **The match must be the sole owner of the enum.** `sole_owner` says
    ///   the scrutinee was a temporary, so no name outside the match reaches
    ///   it; `rt_check_unique` on the ENUM says no other reference does
    ///   either, which a temporary alone does not prove (a method may return
    ///   a retained reference to something it keeps). That check covers the
    ///   payload too: the whole graph reachable from the enum has to be
    ///   private, and the payload is in it.
    /// - **The move must happen in the arm's own block**, for the reason
    ///   `mark_moved` gives: the move set has no control-flow graph, so a
    ///   move inside a nested loop or branch would run a different number of
    ///   times than it was checked. The arm's block itself is fine however
    ///   many times the whole `match` runs -- each time round it matches a
    ///   fresh enum.
    fn take_payload(&mut self, n: &str, v: &Val, span: Span) -> Result<(), Diag> {
        if self.moved.iter().any(|m| m == n) {
            return Err(Diag::new(span, format!("`{n}` was already moved")));
        }
        let pb = self.payload_bind(n).expect("checked by the caller");
        let (hold, tid, tag, idx, depth, sole_owner) = (
            pb.hold.clone(),
            pb.tid,
            pb.tag,
            pb.idx,
            pb.depth,
            pb.sole_owner,
        );
        if !sole_owner {
            // The tail only when a copy is possible at all: advising `clone`
            // on a `net.Conn` would send the reader to a second error.
            let tail = match self.resource_of(v.ty) {
                Some(_) => String::new(),
                None => format!(" Or use clone({n}) to send a copy."),
            };
            return Err(Diag::new(
                span,
                format!(
                    "`{n}` lives inside a value this `match` did not create, so it \
                     cannot be taken out and moved -- whoever else holds that value \
                     would be left with a hole. Match the call that produces it \
                     directly, so the `match` is its only owner.{tail}"
                ),
            ));
        }
        if self.scopes.len() != depth {
            return Err(Diag::new(
                span,
                format!(
                    "`{n}` is bound by the enclosing `case` and cannot be moved from \
                     inside a nested block; a move inside a loop or a branch would run \
                     a different number of times than it was checked. Move it in the \
                     `case` body itself."
                ),
            ));
        }
        let obj = self
            .lookup(&hold)
            .expect("the match holds the scrutinee for the whole arm")
            .1;
        // The ENUM must be unique, not just the payload: taking the payload
        // out edits the enum, and an enum a second reference reaches would be
        // left with a cleared slot under it. Its graph includes the payload,
        // so this one check answers both questions.
        self.push(Inst::Call {
            dst: None,
            func: "rt_check_unique".to_string(),
            args: vec![obj],
        });
        self.push(Inst::EnumTake { obj, tid, tag, idx });
        // The value read out of the slot is now owned by the receiver, and it
        // was never a statement temporary, so there is nothing to un-register.
        let _ = v;
        self.moved.push(n.to_string());
        Ok(())
    }

    fn owns_local(&self, name: &str) -> bool {
        self.owned.iter().any(|ns| ns.iter().any(|n| n == name))
    }

    /// Mark a local as moved. Any later use is a compile error.
    fn mark_moved(&mut self, name: &str, span: Span) -> Result<(), Diag> {
        if self.binding(name).is_none() {
            return Ok(());
        }
        if self.moved.contains(&name.to_string()) {
            return Err(Diag::new(span, format!("`{name}` was already moved")));
        }
        // A move may only take a local declared in THIS scope.
        //
        // The move set is one flat set of names per function, with no notion
        // of the control-flow graph, so a move inside a loop body is checked
        // once and executed every iteration, and a move inside one arm of an
        // `if` silently suppresses the release on the arm that did not move.
        // Both are real bugs and both vanish if a move can only reach
        // something the current scope owns. The cost is a rename or an inner
        // binding, which the diagnostic asks for.
        if !self
            .owned
            .last()
            .is_some_and(|ns| ns.iter().any(|n| n == name))
        {
            return Err(Diag::new(
                span,
                format!(
                    "`{name}` is declared outside this block and cannot be moved from \
                     here; a move inside a loop or a branch would run a different \
                     number of times than it was checked. Bind it in this block first."
                ),
            ));
        }
        self.moved.push(name.to_string());
        // A moved local must not be released at scope end: the value now
        // belongs to whoever received it.
        for names in self.owned.iter_mut() {
            names.retain(|n| n != name);
        }
        Ok(())
    }

    /// Whether values of this type are REFCOUNTED.
    ///
    /// The representational question, and the only one the refcount traffic
    /// should ever ask. `is_ref` is the SURFACE question -- "is this a
    /// reference type" in the sense of reference §3.7 -- and the two now
    /// differ for exactly one kind of type: a value enum is a reference type
    /// that is not managed. Every rule the language states about reference
    /// types (a thread boundary moves, `clone` makes a second object, `==`
    /// is the type's `eq`) keeps asking `is_ref`, which is why making an
    /// enum a value changes nothing a program can see.
    pub(crate) fn is_managed(&self, t: Ty) -> bool {
        self.is_ref(t) && !self.is_value_enum(t)
    }

    /// `rc_inc`, but only on a value the IR says is managed.
    ///
    /// Every retain in the lowering goes through here rather than pushing the
    /// instruction, so a value enum reaching a site that would have retained
    /// it costs nothing and cannot emit `rc_inc` on a struct. The IR verifier
    /// checks that no `rc_inc` escaped this.
    pub(super) fn rc_inc(&mut self, val: Value) {
        if self.types[val.0 as usize].is_managed() {
            self.push(Inst::RcInc { val });
        }
    }

    pub(super) fn rc_dec(&mut self, val: Value) {
        if self.types[val.0 as usize].is_managed() {
            self.push(Inst::RcDec { val });
        }
    }

    /// Give up on representing `t` as a value and ask for the whole program
    /// to be lowered again with it boxed (`lower_program`).
    ///
    /// The `Diag` returned is never shown: the driver sees a non-empty
    /// demotion list and starts over. It is worded anyway, because a bug that
    /// let it escape should say what happened.
    fn box_it(&mut self, t: Ty, span: Span, why: &str) -> Diag {
        let name = match self.underlying(t) {
            Ty::User(i) => self.ty_exprs[i as usize].name.clone(),
            _ => unreachable!("only a user type can be a value enum"),
        };
        self.demote.borrow_mut().insert(name.clone());
        Diag::new(
            span,
            format!("internal: `{name}` must be a heap object to {why}; lowering again"),
        )
    }

    /// Emit rc_dec for every owned local in the innermost scope.
    pub(super) fn release_scope(&mut self) {
        let names = self.owned.last().cloned().unwrap_or_default();
        for name in names.iter().rev() {
            if let Some((ty, v)) = self.lookup(name) {
                if self.is_ref(ty) {
                    self.rc_dec(v);
                }
            }
        }
    }

    /// Emit rc_dec for every owned local in every enclosing scope, innermost
    /// first. Used on `return` and on falling off the end of a function.
    pub(super) fn release_all(&mut self) {
        let all: Vec<Vec<String>> = self.owned.clone();
        for names in all.iter().rev() {
            for name in names.iter().rev() {
                if let Some((ty, v)) = self.lookup(name) {
                    if self.is_ref(ty) {
                        self.rc_dec(v);
                    }
                }
            }
        }
    }

    /// Release owned locals from the innermost scope down to (but not
    /// including) `depth`. Used by `break` and `continue`, which leave every
    /// scope inside the loop body.
    pub(super) fn release_to_depth(&mut self, depth: usize) {
        let all: Vec<Vec<String>> = self.owned.clone();
        for names in all.iter().skip(depth).rev() {
            for name in names.iter().rev() {
                if let Some((ty, v)) = self.lookup(name) {
                    if self.is_ref(ty) {
                        self.rc_dec(v);
                    }
                }
            }
        }
    }

    /// Release only the temporaries registered since `mark`.
    ///
    /// Needed by `&&` and `||`: the right-hand operand is lowered into its
    /// own block, and anything it allocates must be released THERE. Left to
    /// the statement's flush, the release lands in the merge block, which the
    /// short-circuit edge reaches without ever having run the operand -- so
    /// the value does not dominate its own release, and the emitted C reads
    /// an uninitialised pointer.
    pub(super) fn flush_temps_since(&mut self, mark: usize) {
        let temps: Vec<Value> = self.stmt_temps.split_off(mark);
        for v in temps {
            self.rc_dec(v);
        }
    }

    pub(super) fn flush_temps(&mut self) {
        let temps = std::mem::take(&mut self.stmt_temps);
        for v in temps {
            self.rc_dec(v);
        }
    }
}
