//! Local scope: binding lookup, the receiver's fields, sibling calls,
//! and the field paths embedding produces.

use super::{Binding, Lowerer, Val};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{Inst, IrTy, Value};

impl Lowerer {
    pub(super) fn lookup(&self, name: &str) -> Option<(Ty, Value)> {
        self.binding(name).map(|(t, v, _)| (t, v))
    }

    /// A bare name that names a field of the receiver, promoted fields
    /// included. Returns the receiver and the path to reach it.
    pub(super) fn recv_field(&self, name: &str) -> Option<(u32, Value, Vec<u32>)> {
        let (tid, obj) = self.recv?;
        let path = self.field_path(tid, name)?;
        Some((tid, obj, path))
    }

    /// `this`: the receiver of the instance method being lowered, BORROWED
    /// exactly like a parameter -- the caller holds the +1, so reading it
    /// costs nothing, and a `return this;` retains it the way returning a
    /// parameter does.
    pub(super) fn this_val(&mut self, span: Span) -> Result<Val, Diag> {
        let Some((tid, v)) = self.recv else {
            return Err(Diag::new(
                span,
                format!(
                    "`this` means nothing here: {}, so there is no receiver",
                    self.no_recv
                ),
            ));
        };
        let name = self.typedefs[tid as usize].name.clone();
        let ty = self
            .ty_named(&name)
            .expect("the receiver's type is declared");
        Ok(Val::new(v, ty, false))
    }

    /// The receiver's own instance method called `name` -- declared, or
    /// promoted from an embedded type -- as its key in `sigs`. These are
    /// what a bare call inside a method may name, and what `this.name(..)`
    /// is refused for.
    pub(super) fn sibling_method(&self, name: &str) -> Option<String> {
        let (tid, _) = self.recv?;
        let key = format!("{}.{name}", self.typedefs[tid as usize].name);
        (self.sigs.contains_key(&key) && !self.statics.contains(&key)).then_some(key)
    }

    pub(super) fn binding(&self, name: &str) -> Option<Binding> {
        for s in self.scopes.iter().rev() {
            if let Some(x) = s.get(name) {
                return Some(*x);
            }
        }
        None
    }

    /// The type that declares the last field on `path` from `tid`, and that
    /// field's index in it: what `field_visible` is asked about.
    pub(super) fn path_owner(&self, tid: u32, path: &[u32]) -> (u32, u32) {
        let mut cur = tid;
        for idx in &path[..path.len() - 1] {
            cur = self
                .tdef_of(self.field_ty(cur, *idx))
                .expect("an embedded field is a user type");
        }
        (cur, *path.last().expect("a field path is never empty"))
    }

    pub(super) fn rebind(&mut self, name: &str, v: Value) {
        for s in self.scopes.iter_mut().rev() {
            if let Some(slot) = s.get_mut(name) {
                slot.1 = v;
                return;
            }
        }
        unreachable!("rebind of unknown name");
    }

    /// Set `name`'s `is_const` bit in whichever scope binds it, returning
    /// what it was. A `const` block flips this to `true` on entry and
    /// restores the returned value on exit -- restore, not unconditionally
    /// clear, so freezing a local that is already `const` (or already inside
    /// an enclosing `const` block) is a no-op rather than a temporary hole
    /// (docs/const-decision.md, "`const` is also a block"). This is the
    /// entire reuse of the existing const-mutation check: `refuse_const_write`
    /// and `Stmt::Assign` both read this same bit through `binding`, so
    /// scoping it to a region needs no change to either.
    pub(super) fn set_const_flag(&mut self, name: &str, is_const: bool) -> bool {
        for s in self.scopes.iter_mut().rev() {
            if let Some(slot) = s.get_mut(name) {
                return std::mem::replace(&mut slot.2, is_const);
            }
        }
        unreachable!("set_const_flag of unknown name")
    }

    /// The path of field indices to reach `name` from `tid`, promoting
    /// through embedded fields. Empty prefix means a direct field.
    ///
    /// Breadth-first, so a direct field always wins over a promoted one, and
    /// a shallower promotion wins over a deeper one -- the same rule Go uses.
    pub(super) fn field_path(&self, tid: u32, name: &str) -> Option<Vec<u32>> {
        if self.field_of(tid, name).is_some() {
            let (i, _) = self.field_of(tid, name).unwrap();
            return Some(vec![i]);
        }
        for (i, p) in self.field_params[tid as usize].iter().enumerate() {
            if !p.embedded {
                continue;
            }
            let Some(inner) = self.tdef_of(p.ty) else {
                continue;
            };
            if let Some(mut rest) = self.field_path(inner, name) {
                let mut path = vec![i as u32];
                path.append(&mut rest);
                return Some(path);
            }
        }
        None
    }

    /// Every name a method of `tid` could reach as a bare field read: this
    /// type's own fields, then whatever an embedded one promotes. Duplicates
    /// are dropped, so each name is asked about once; which of two a name
    /// actually reaches is `field_path`'s business.
    pub(super) fn reachable_field_names(&self, tid: u32, out: &mut Vec<String>) {
        for (n, _) in &self.typedefs[tid as usize].fields {
            if !out.iter().any(|x| x == n) {
                out.push(n.clone());
            }
        }
        for p in &self.field_params[tid as usize] {
            if !p.embedded {
                continue;
            }
            if let Some(inner) = self.tdef_of(p.ty) {
                self.reachable_field_names(inner, out);
            }
        }
    }

    /// Walk a field path, emitting a load per step, and return the final
    /// value and its surface type.
    pub(super) fn load_path(&mut self, tid: u32, obj: Value, path: &[u32]) -> (Value, Ty) {
        let mut cur_tid = tid;
        let mut cur = obj;
        let mut ty = Ty::Void;
        for idx in path {
            let (_, fty) = self.typedefs[cur_tid as usize].fields[*idx as usize].clone();
            let d = self.new_val(fty);
            self.push(Inst::LoadField {
                dst: d,
                obj: cur,
                tid: cur_tid,
                idx: *idx,
            });
            ty = self.field_ty(cur_tid, *idx);
            cur = d;
            if let Some(next) = self.tdef_of(ty) {
                cur_tid = next;
            }
        }
        (cur, ty)
    }

    pub(super) fn field_of(&self, tid: u32, name: &str) -> Option<(u32, IrTy)> {
        self.typedefs[tid as usize]
            .fields
            .iter()
            .position(|(n, _)| n == name)
            .map(|i| (i as u32, self.typedefs[tid as usize].fields[i].1))
    }

    /// The key a bare name has in `sigs`: this module's own declaration if
    /// there is one, otherwise the name as written -- which is how builtins
    /// and the prelude stay reachable from everywhere.
    pub(super) fn resolve_fn(&self, name: &str) -> String {
        if !self.cur_module.is_empty() {
            let qualified = format!("{}#{name}", self.cur_module);
            if self.sigs.contains_key(&qualified) {
                return qualified;
            }
        }
        name.to_string()
    }
}
