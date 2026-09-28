//! Statement lowering: blocks, loops, `if` and `match`, and the scope
//! snapshot/restore their branches share.

use super::{stmt_span, Binding, LoopCtx, Lowerer, PayloadBind};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{ArithOp, BlockId, Cmp, Inst, IrTy, Term, Value};
use std::collections::HashMap;

impl Lowerer {
    pub(super) fn lower_block(&mut self, stmts: &[Stmt]) -> Result<(), Diag> {
        for s in stmts {
            if self.terminated() {
                // Unreachable code after return. Silently dropping it would
                // hide a real mistake.
                return Err(Diag::new(stmt_span(s), "unreachable statement"));
            }
            self.lower_stmt(s)?;
        }
        Ok(())
    }

    fn lower_stmt(&mut self, s: &Stmt) -> Result<(), Diag> {
        match s {
            Stmt::Decl {
                ty,
                name,
                init,
                is_const,
                span,
            } => {
                self.check_named_ty(*ty, *span)?;
                let val = self.lower_expr_as(init, *ty)?;
                if !self.assignable(val.ty, *ty) {
                    return Err(Diag::new(init.span(), self.mismatch(*ty, val.ty)));
                }
                self.check_shadow(name, *span)?;
                let snapshot = *is_const && self.const_snapshots(*ty);
                if snapshot {
                    self.refuse_const_resource(*ty, *span)?;
                }
                // The local must hold a +1. A borrowed source needs one added;
                // an owned temp is handed straight over, so drop it from the
                // pending list rather than releasing it.
                if self.is_ref(*ty) {
                    if val.owned {
                        self.stmt_temps.retain(|t| *t != val.val());
                    } else {
                        self.rc_inc(val.val());
                    }
                    self.owned.last_mut().unwrap().push(name.clone());
                }
                // A `const` binds a frozen snapshot (docs/const-decision.md):
                // after the hand-over, so the local's +1 is counted and a
                // value nothing else holds is frozen in place, while a shared
                // one is deep-copied. `rt_snapshot` takes the +1 and hands
                // one back, on the same object or on the copy.
                let bound = if snapshot {
                    let d = self.new_val(IrTy::Ref);
                    self.push(Inst::Call {
                        dst: Some(d),
                        func: "rt_snapshot".to_string(),
                        args: vec![val.val()],
                    });
                    d
                } else {
                    val.val()
                };
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), (*ty, bound, *is_const));
                self.flush_temps();
                Ok(())
            }

            Stmt::Assign { name, value, span } => {
                // A bare name inside a method may be a field of the receiver.
                if self.binding(name).is_none() {
                    if let Some((rtid, robj, path)) = self.recv_field(name) {
                        self.check_field_access(rtid, &path, name, *span)?;
                        // Walk to the object that actually owns the field.
                        let (owner, owner_tid) = if path.len() == 1 {
                            (robj, rtid)
                        } else {
                            // Not held (src/lower/hold.rs), though read out
                            // of the receiver before the value runs: an
                            // embedded field has no name a program can
                            // write, so it keeps the object it was built
                            // with for as long as the receiver lives.
                            let (o, oty) = self.load_path(rtid, robj, &path[..path.len() - 1]);
                            (o, self.tdef_of(oty).expect("embedded field is a type"))
                        };
                        let idx = *path.last().unwrap();
                        let tid = owner_tid;
                        let obj = owner;
                        let (_, fty) = self.typedefs[tid as usize].fields[idx as usize].clone();
                        let want = self.field_ty(tid, idx);
                        let v = self.lower_expr_as(value, want)?;
                        if !self.assignable(v.ty, want) {
                            return Err(Diag::new(
                                value.span(),
                                format!(
                                    "type mismatch: field `{name}` is {}, found {}",
                                    self.tyname(want),
                                    self.tyname(v.ty)
                                ),
                            ));
                        }
                        // The receiver may be frozen: a method has no way to
                        // say it changes `this`, so `c.bump()` on a const
                        // `c` compiles, and is caught here.
                        self.check_mutable(obj);
                        if fty == IrTy::Ref {
                            let old = self.new_val(IrTy::Ref);
                            self.push(Inst::LoadField {
                                dst: old,
                                obj,
                                tid,
                                idx,
                            });
                            if v.owned {
                                self.stmt_temps.retain(|t| *t != v.val());
                            } else {
                                self.rc_inc(v.val());
                            }
                            self.push(Inst::StoreField {
                                obj,
                                tid,
                                idx,
                                val: v.val(),
                            });
                            self.rc_dec(old);
                        } else {
                            self.push(Inst::StoreField {
                                obj,
                                tid,
                                idx,
                                val: v.val(),
                            });
                        }
                        self.flush_temps();
                        return Ok(());
                    }
                }
                let Some((ty, old, is_const)) = self.binding(name) else {
                    // The same words as for a `const` local: to the reader
                    // they are one idea, a name that cannot be reassigned.
                    if self.resolve_const(name).is_some() {
                        return Err(Diag::new(*span, format!("cannot assign to const `{name}`")));
                    }
                    return Err(self.unknown_variable(name, *span));
                };
                if is_const {
                    // A loop variable is bound the same way, and saying only
                    // "const" would report a decision without naming it.
                    if self.loop_vars.iter().any(|v| v == name) {
                        return Err(Diag::new(
                            *span,
                            format!(
                                "`{name}` is the loop's variable and cannot be assigned: \
                                 the loop binds it afresh each time round, and assigning \
                                 it could not change where the loop goes -- use another \
                                 local"
                            ),
                        ));
                    }
                    return Err(Diag::new(*span, format!("cannot assign to const `{name}`")));
                }
                let val = self.lower_expr_as(value, ty)?;
                if !self.assignable(val.ty, ty) {
                    return Err(Diag::new(value.span(), self.mismatch(ty, val.ty)));
                }
                if self.is_ref(ty) {
                    if val.owned {
                        self.stmt_temps.retain(|t| *t != val.val());
                    } else {
                        self.rc_inc(val.val());
                    }
                    // Release the previous value only if we held it. A
                    // PARAMETER is borrowed (docs/ir-v0.md §5.1) and is
                    // deliberately not registered as owned, so releasing its
                    // old value would free the caller's reference -- and
                    // never registering the new one would leak it. Assigning
                    // to a parameter therefore takes ownership from here on.
                    let held = self
                        .owned
                        .iter()
                        .any(|names| names.iter().any(|n| n == name));
                    if held {
                        self.rc_dec(old);
                    } else {
                        self.owned.last_mut().unwrap().push(name.clone());
                    }
                }
                self.rebind(name, val.val());
                self.flush_temps();
                Ok(())
            }

            Stmt::Return { value, span } => {
                match (value, self.ret_ty) {
                    (None, Ty::Void) => {
                        self.release_all();
                        self.terminate(Term::Ret { val: None });
                    }
                    (None, t) => {
                        return Err(Diag::new(
                            *span,
                            format!("expected a return value of type {}", self.tyname(t)),
                        ))
                    }
                    (Some(e), Ty::Void) => {
                        let _ = e;
                        return Err(Diag::new(
                            *span,
                            "cannot return a value from a void function",
                        ));
                    }
                    (Some(e), want) => {
                        let val = self.lower_expr_as(e, want)?;
                        if !self.assignable(val.ty, want) {
                            // A lambda's body is this `return`, and it was
                            // never written as one: say what the interface
                            // asked for, which is where the truth is.
                            if let Some((iface, method)) = self.cur_lambda.clone() {
                                let head =
                                    format!("`{iface}.{method}` returns {}", self.tyname(want));
                                return Err(Diag::new(
                                    e.span(),
                                    if val.ty == Ty::Void {
                                        // The commonest shape of this
                                        // mistake: a body written for its
                                        // effect where a value is wanted.
                                        format!("{head}, but this lambda's body has no value")
                                    } else {
                                        format!(
                                            "{head}, this lambda's body is {}",
                                            self.a_ty(val.ty)
                                        )
                                    },
                                ));
                            }
                            return Err(Diag::new(e.span(), self.mismatch(want, val.ty)));
                        }
                        // Returns are owned (+1). Retain a borrowed value
                        // before releasing locals, or returning a local would
                        // hand back a freed object.
                        if self.is_ref(want) && !val.owned {
                            self.rc_inc(val.val());
                        }
                        if val.owned {
                            self.stmt_temps.retain(|t| *t != val.val());
                        }
                        self.flush_temps();
                        self.release_all();
                        self.terminate(Term::Ret {
                            val: Some(val.val()),
                        });
                    }
                }
                Ok(())
            }

            Stmt::Eval { expr, span } => {
                if let Expr::Call(name, args, cspan) = expr {
                    if name == "trap" {
                        return self.lower_trap(args, *cspan);
                    }
                }
                let val = self.lower_expr(expr)?;
                // A Result thrown away is the classic quiet bug -- C's
                // fclose problem. It is an ERROR rather than a warning
                // because the language has no warnings and should not grow
                // the category for this.
                //
                // An Option is exempt: ignoring one is often reasonable, and
                // the failure it reports is absence rather than something
                // going wrong.
                if let Some(tid) = self.tdef_of(val.ty) {
                    if self.typedefs[tid as usize].name.starts_with("Result$") {
                        return Err(Diag::new(
                            *span,
                            "this Result is discarded; handle it with `match`, \
                             propagate it with `?`, or bind it to a name",
                        ));
                    }
                }
                self.flush_temps();
                Ok(())
            }

            Stmt::If {
                cond,
                then,
                els,
                span,
            } => self.lower_if(cond, then, els.as_deref(), *span),

            Stmt::While { cond, body, span } => self.lower_while(cond, body, *span),
            Stmt::Match {
                scrutinee,
                arms,
                span,
            } => self.lower_match(scrutinee, arms, *span),

            Stmt::ForIn {
                ty,
                name,
                iter,
                body,
                span,
            } => self.lower_forin(*ty, name, iter, body, *span),

            Stmt::ForRange {
                ty,
                name,
                from,
                to,
                body,
                span,
            } => self.lower_forrange(*ty, name, from, to, body, *span),

            Stmt::SetIndex {
                obj,
                index,
                value,
                span,
            } => {
                self.refuse_const_write(obj, *span)?;
                let o = self.lower_expr(obj)?;
                // `h.ps[i] = f(h)`: the index or the value can replace the
                // collection before the store (src/lower/hold.rs).
                let later = self.may_run_code(index, true) || self.may_run_code(value, true);
                let o = self.hold(obj, o, later);
                // No refcounts to move: a byte is a value. The runtime traps
                // on an index out of range and on a value outside 0..255, and
                // a constant outside it is refused here.
                if self.underlying(o.ty) == Ty::Bytes {
                    let i = self.index_of_bytes(index)?;
                    self.check_byte_literal(value)?;
                    let v = self.arg_of(value, Ty::Int)?;
                    self.rt_void("rt_bytes_set", vec![o.val(), i, v]);
                    self.flush_temps();
                    return Ok(());
                }
                let Some(elem) = self.seq_elem(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("{} cannot be indexed", self.tyname(o.ty)),
                    ));
                };
                let i = self.lower_expr(index)?;
                if self.underlying(i.ty) != Ty::Int {
                    return Err(Diag::new(index.span(), self.mismatch(Ty::Int, i.ty)));
                }
                let v = self.lower_expr_as(value, elem)?;
                if !self.assignable(v.ty, elem) {
                    return Err(Diag::new(value.span(), self.mismatch(elem, v.ty)));
                }
                // Retain the new element, store, then release the old -- in
                // that order, so `xs[i] = xs[i];` cannot free what it stores.
                if self.is_ref(elem) {
                    let old = self.new_val(IrTy::Ref);
                    self.push(Inst::Call {
                        dst: Some(old),
                        func: "rt_index_get".to_string(),
                        args: vec![o.val(), i.val()],
                    });
                    if v.owned {
                        self.stmt_temps.retain(|t| *t != v.val());
                    } else {
                        self.rc_inc(v.val());
                    }
                    self.push(Inst::Call {
                        dst: None,
                        func: "rt_index_set".to_string(),
                        args: vec![o.val(), i.val(), v.val()],
                    });
                    self.rc_dec(old);
                } else {
                    self.push(Inst::Call {
                        dst: None,
                        func: "rt_index_set".to_string(),
                        args: vec![o.val(), i.val(), v.val()],
                    });
                }
                self.flush_temps();
                Ok(())
            }

            Stmt::Spawn { name, args, span } => {
                let key = self.resolve_fn(name);
                let Some(sig) = self.sigs.get(&key) else {
                    return Err(Diag::new(*span, format!("unknown function `{name}`")));
                };
                let params = sig.params.clone();
                let module = sig.module.clone();
                if sig.ret != Ty::Void {
                    return Err(Diag::new(
                        *span,
                        format!("`spawn` needs a void function; `{name}` returns a value"),
                    ));
                }
                let slots = self.bind_args(name, &params, args, *span)?;
                let mut vals = Vec::new();
                for (a, p) in slots.iter().zip(params.iter()) {
                    let v = self.lower_slot(a, p, &module)?;
                    if !self.assignable(v.ty, p.ty) {
                        return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
                    }
                    // A spawn hands every reference to the new thread, which
                    // becomes the only one that may reach it. A channel is
                    // exempt -- it is how threads share.
                    self.transfer(&v, a, *span)?;
                    vals.push(v.val());
                }
                self.push(Inst::Spawn {
                    func: key.clone(),
                    args: vals,
                });
                self.flush_temps();
                Ok(())
            }

            Stmt::SetField {
                obj,
                field,
                value,
                span,
            } => {
                if let Expr::This(ts) = obj {
                    self.refuse_this_field(field, *ts, *span)?;
                }
                self.refuse_const_write(obj, *span)?;
                let o = self.lower_expr(obj)?;
                // `h.p.x = f(h)`: the value can replace `h.p` before the
                // store, which must then land in the object read first --
                // alive, not freed (src/lower/hold.rs).
                let o = self.hold(obj, o, self.may_run_code(value, true));
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        obj.span(),
                        format!("type {} has no fields", self.tyname(o.ty)),
                    ));
                };
                // A write is held to the same two checks as a read. The type
                // check was missing before fields could be private: a value
                // of a private type handed out by a `pub` function could be
                // written through, though not read.
                if !self.type_visible(tid) {
                    return Err(Diag::new(
                        *span,
                        self.not_visible(tid, "its fields cannot be written from here"),
                    ));
                }
                let Some((idx, fty)) = self.field_of(tid, field) else {
                    return Err(Diag::new(
                        *span,
                        format!("type `{}` has no field `{field}`", self.tyname(o.ty)),
                    ));
                };
                self.check_field_access(tid, &[idx], field, *span)?;
                let v = self.lower_expr_as(value, self.field_ty(tid, idx))?;
                if !self.assignable(v.ty, self.field_ty(tid, idx)) {
                    return Err(Diag::new(
                        value.span(),
                        format!(
                            "type mismatch: field `{field}` is {}, found {}",
                            self.field_tyname(tid, idx),
                            self.tyname(v.ty)
                        ),
                    ));
                }
                // Frozen objects reach here through anything the compiler
                // cannot see through -- a parameter, an element, another
                // local -- so every store to an existing object checks.
                self.check_mutable(o.val());
                // Retain the new value, then release the old -- in that order,
                // so `p.f = p.f;` cannot free what it is assigning.
                if fty == IrTy::Ref {
                    let old = self.new_val(IrTy::Ref);
                    self.push(Inst::LoadField {
                        dst: old,
                        obj: o.val(),
                        tid,
                        idx,
                    });
                    if v.owned {
                        self.stmt_temps.retain(|t| *t != v.val());
                    } else {
                        self.rc_inc(v.val());
                    }
                    self.push(Inst::StoreField {
                        obj: o.val(),
                        tid,
                        idx,
                        val: v.val(),
                    });
                    self.rc_dec(old);
                } else {
                    self.push(Inst::StoreField {
                        obj: o.val(),
                        tid,
                        idx,
                        val: v.val(),
                    });
                }
                self.flush_temps();
                Ok(())
            }

            Stmt::Break { span } => {
                let Some(l) = self.loops.last() else {
                    return Err(Diag::new(*span, "`break` outside a loop"));
                };
                let (exit, carried, depth) = (l.exit, l.carried.clone(), l.depth);
                self.loops.last_mut().expect("checked above").broke = true;
                self.release_to_depth(depth);
                let args: Vec<Value> = carried
                    .iter()
                    .map(|n| self.lookup(n).map(|x| x.1).unwrap())
                    .collect();
                self.terminate(Term::Jump { to: exit, args });
                Ok(())
            }

            Stmt::Continue { span } => {
                let Some(l) = self.loops.last() else {
                    return Err(Diag::new(*span, "`continue` outside a loop"));
                };
                let (header, carried, depth) = (l.header, l.carried.clone(), l.depth);
                self.release_to_depth(depth);
                let args: Vec<Value> = carried
                    .iter()
                    .map(|n| self.lookup(n).map(|x| x.1).unwrap())
                    .collect();
                self.terminate(Term::Jump { to: header, args });
                Ok(())
            }
        }
    }

    /// Collect the names a statement list assigns to, including inside
    /// nested `if`/`while` bodies.
    ///
    /// Loops need this up front. An `if` can compare the two arms after
    /// lowering them, but a loop header dominates its own body, so its block
    /// parameters must exist *before* the body is lowered -- and we only know
    /// which variables are loop-carried by looking. This is the cheap
    /// alternative to incremental SSA construction with incomplete blocks,
    /// and it is exact for the statements v0 has.
    fn assigned_names(stmts: &[Stmt], out: &mut Vec<String>) {
        for s in stmts {
            match s {
                Stmt::Assign { name, .. } => {
                    if !out.contains(name) {
                        out.push(name.clone());
                    }
                }
                Stmt::Match { arms, .. } => {
                    for a in arms {
                        Self::assigned_names(&a.body, out);
                    }
                }
                Stmt::If { then, els, .. } => {
                    Self::assigned_names(then, out);
                    if let Some(e) = els {
                        Self::assigned_names(e, out);
                    }
                }
                Stmt::While { body, .. } => Self::assigned_names(body, out),
                Stmt::ForIn { body, .. } | Stmt::ForRange { body, .. } => {
                    Self::assigned_names(body, out)
                }
                Stmt::Decl { .. }
                | Stmt::Return { .. }
                | Stmt::Eval { .. }
                | Stmt::Break { .. }
                | Stmt::Continue { .. }
                | Stmt::Spawn { .. }
                | Stmt::SetIndex { .. }
                | Stmt::SetField { .. } => {}
            }
        }
    }

    /// `for (int i in a .. b) { .. }` -- the counting loop.
    ///
    /// The same shape as `lower_forin`, and for the same reason: the counter
    /// advances at the TOP of the body, so `continue` cannot skip it. That
    /// is the whole point of the construct. Written as a `while`, the
    /// increment is the body's last statement and **every `continue` has to
    /// repeat it by hand** -- forget one and the program does not terminate.
    /// All three of the applications in `apps/` recorded writing exactly
    /// that loop (`apps/tui` §4, `apps/git` §8, `apps/markdown` §1.2), and
    /// it is not a mistake the compiler can catch after the fact.
    ///
    /// The counter is bound `const`, like any other loop variable, so the
    /// body cannot assign it either.
    ///
    /// Both bounds are evaluated ONCE, left then right, before the loop --
    /// so `for (int i in 0 .. xs.size())` measures once, and pushing inside
    /// the body does not extend the iteration. The same choice `for ... in`
    /// makes for a collection's length, and the predictable one.
    ///
    /// `idx + 1` cannot overflow: the increment runs only after `idx < end`
    /// held, so the sum is at most `end`.
    fn lower_forrange(
        &mut self,
        ty: Ty,
        name: &str,
        from: &Expr,
        to: &Expr,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), Diag> {
        self.check_shadow(name, span)?;
        if ty != Ty::Int {
            return Err(Diag::new(
                span,
                format!(
                    "a range counts in `int`, and this counter is {}",
                    self.a_ty(ty)
                ),
            ));
        }

        // Unique per loop, for the reason `lower_forin` gives.
        self.synth += 1;
        let idx_name = format!("$i{}", self.synth);

        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());

        let lo = self.lower_expr(from)?;
        if self.underlying(lo.ty) != Ty::Int {
            self.scopes.pop();
            self.owned.pop();
            return Err(Diag::new(from.span(), self.mismatch(Ty::Int, lo.ty)));
        }
        let hi = self.lower_expr(to)?;
        if self.underlying(hi.ty) != Ty::Int {
            self.scopes.pop();
            self.owned.pop();
            return Err(Diag::new(to.span(), self.mismatch(Ty::Int, hi.ty)));
        }
        let (start, end) = (lo.val(), hi.val());
        self.scopes
            .last_mut()
            .unwrap()
            .insert(idx_name.clone(), (Ty::Int, start, false));
        self.flush_temps();

        let mut names = vec![idx_name.clone()];
        Self::assigned_names(body, &mut names);
        names.retain(|x| self.lookup(x).is_some());
        names.sort();
        let carried: Vec<(String, Ty, Value)> = names
            .iter()
            .map(|x| {
                let (t, v) = self.lookup(x).unwrap();
                (x.clone(), t, v)
            })
            .collect();

        let header = self.new_block();
        let body_bb = self.new_block();
        let exit_bb = self.new_block();

        let entry_args: Vec<Value> = carried.iter().map(|(_, _, v)| *v).collect();
        self.terminate(Term::Jump {
            to: header,
            args: entry_args,
        });

        let mut hp = Vec::new();
        let mut ep = Vec::new();
        for (_, t, _) in &carried {
            hp.push(self.new_val(self.irty(*t)));
            ep.push(self.new_val(self.irty(*t)));
        }
        let hi_block = self.blocks.iter().position(|b| b.id == header).unwrap();
        self.blocks[hi_block].params = hp.clone();
        let ei = self.blocks.iter().position(|b| b.id == exit_bb).unwrap();
        self.blocks[ei].params = ep.clone();

        self.switch_to(header);
        for ((x, _, _), p) in carried.iter().zip(hp.iter()) {
            self.rebind(x, *p);
        }
        let idx = self.lookup(&idx_name).unwrap().1;
        let cond = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: cond,
            cmp: Cmp::Lt,
            lhs: idx,
            rhs: end,
        });
        self.terminate(Term::Brif {
            cond,
            then: body_bb,
            then_args: Vec::new(),
            els: exit_bb,
            els_args: hp.clone(),
        });

        self.switch_to(body_bb);
        // Advance FIRST, and hand the body the value from before it.
        let one = self.new_val(IrTy::I64);
        self.push(Inst::IConst { dst: one, val: 1 });
        let next = self.new_val(IrTy::I64);
        self.push(Inst::Arith {
            dst: next,
            op: ArithOp::Add,
            lhs: idx,
            rhs: one,
        });
        self.rebind(&idx_name, next);

        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());
        // An `int`, so there is nothing to retain and nothing to release.
        // `true` is `is_const`: the counter belongs to the loop.
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), (Ty::Int, idx, true));
        self.loop_vars.push(name.to_string());

        self.loops.push(LoopCtx {
            header,
            exit: exit_bb,
            carried: carried.iter().map(|(x, _, _)| x.clone()).collect(),
            depth: self.owned.len() - 1,
            broke: false,
        });
        let lowered = self.lower_block(body);
        self.loops.pop();
        self.loop_vars.pop();
        lowered?;

        let live = !self.terminated();
        if live {
            self.release_scope();
        }
        self.scopes.pop();
        self.owned.pop();

        if live {
            let back: Vec<Value> = carried
                .iter()
                .map(|(x, _, _)| self.lookup(x).unwrap().1)
                .collect();
            self.terminate(Term::Jump {
                to: header,
                args: back,
            });
        }

        self.switch_to(exit_bb);
        for ((x, _, _), p) in carried.iter().zip(ep.iter()) {
            self.rebind(x, *p);
        }
        self.release_scope();
        self.scopes.pop();
        self.owned.pop();
        Ok(())
    }

    /// `for (T x in xs) { .. }`
    ///
    /// Structured like `while`, with one wrinkle that decides the shape: the
    /// index must be incremented at the TOP of the body, not the bottom.
    /// `continue` jumps to the header carrying the loop variables as they
    /// stand, so an increment at the bottom would be skipped and the loop
    /// would never advance. Incrementing first, and reading the element at
    /// the pre-increment value, makes `continue` correct for free.
    ///
    /// The length is read once, before the loop. Pushing to a list while
    /// iterating it therefore does not extend the iteration -- the same
    /// choice Go makes for slices, and the predictable one.
    fn lower_forin(
        &mut self,
        ty: Ty,
        name: &str,
        iter: &Expr,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), Diag> {
        self.check_shadow(name, span)?;

        // Synthetic names must be UNIQUE per loop. Release resolves an owned
        // name through `lookup`, which finds the innermost binding -- so two
        // nested loops both using `$coll` made the outer one's release
        // resolve to the inner collection: released twice, and the outer
        // never. Nothing in the surface language shadows, which is exactly
        // why the released-by-name scheme is otherwise safe.
        self.synth += 1;
        let coll_name = format!("$coll{}", self.synth);
        let idx_name = format!("$i{}", self.synth);

        // Evaluate the collection once, into a scope of its own so it is
        // released when the loop ends.
        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());

        let coll = self.lower_expr(iter)?;
        // A `bytes` iterates like a List<int>: the same loop, with the
        // runtime calls that read a byte rather than a slot.
        let is_bytes = self.underlying(coll.ty) == Ty::Bytes;
        let (len_fn, get_fn) = if is_bytes {
            ("rt_bytes_len", "rt_bytes_get")
        } else {
            ("rt_len_of", "rt_index_get")
        };
        let elem = if is_bytes {
            Some(Ty::Int)
        } else {
            self.seq_elem(coll.ty)
        };
        let Some(elem) = elem else {
            self.scopes.pop();
            self.owned.pop();
            return Err(Diag::new(
                iter.span(),
                format!("{} cannot be iterated", self.tyname(coll.ty)),
            ));
        };
        if !self.assignable(elem, ty) {
            self.scopes.pop();
            self.owned.pop();
            return Err(Diag::new(span, self.mismatch(ty, elem)));
        }
        if coll.owned {
            self.stmt_temps.retain(|t| *t != coll.val());
        } else {
            self.rc_inc(coll.val());
        }
        self.scopes
            .last_mut()
            .unwrap()
            .insert(coll_name.clone(), (coll.ty, coll.val(), true));
        self.owned.last_mut().unwrap().push(coll_name.clone());

        let n = self.new_val(IrTy::I64);
        self.push(Inst::Call {
            dst: Some(n),
            func: len_fn.to_string(),
            args: vec![coll.val()],
        });

        let zero = self.new_val(IrTy::I64);
        self.push(Inst::IConst { dst: zero, val: 0 });
        self.scopes
            .last_mut()
            .unwrap()
            .insert(idx_name.clone(), (Ty::Int, zero, false));
        self.flush_temps();

        // From here the shape is `while ($i < $n)`, hand-built so the
        // increment can sit at the top of the body.
        let mut names = vec![idx_name.clone()];
        Self::assigned_names(body, &mut names);
        names.retain(|x| self.lookup(x).is_some());
        names.sort();
        let carried: Vec<(String, Ty, Value)> = names
            .iter()
            .map(|x| {
                let (t, v) = self.lookup(x).unwrap();
                (x.clone(), t, v)
            })
            .collect();

        let header = self.new_block();
        let body_bb = self.new_block();
        let exit_bb = self.new_block();

        let entry_args: Vec<Value> = carried.iter().map(|(_, _, v)| *v).collect();
        self.terminate(Term::Jump {
            to: header,
            args: entry_args,
        });

        let mut hp = Vec::new();
        let mut ep = Vec::new();
        for (_, t, _) in &carried {
            hp.push(self.new_val(self.irty(*t)));
            ep.push(self.new_val(self.irty(*t)));
        }
        let hi = self.blocks.iter().position(|b| b.id == header).unwrap();
        self.blocks[hi].params = hp.clone();
        let ei = self.blocks.iter().position(|b| b.id == exit_bb).unwrap();
        self.blocks[ei].params = ep.clone();

        self.switch_to(header);
        for ((x, _, _), p) in carried.iter().zip(hp.iter()) {
            self.rebind(x, *p);
        }
        let idx = self.lookup(&idx_name).unwrap().1;
        let cond = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: cond,
            cmp: Cmp::Lt,
            lhs: idx,
            rhs: n,
        });
        self.terminate(Term::Brif {
            cond,
            then: body_bb,
            then_args: Vec::new(),
            els: exit_bb,
            els_args: hp.clone(),
        });

        self.switch_to(body_bb);
        // Increment FIRST, so `continue` advances; read at the old index.
        let one = self.new_val(IrTy::I64);
        self.push(Inst::IConst { dst: one, val: 1 });
        let next = self.new_val(IrTy::I64);
        self.push(Inst::Arith {
            dst: next,
            op: ArithOp::Add,
            lhs: idx,
            rhs: one,
        });
        self.rebind(&idx_name, next);

        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());
        let e = self.new_val(self.irty(elem));
        self.push(Inst::Call {
            dst: Some(e),
            func: get_fn.to_string(),
            args: vec![coll.val(), idx],
        });
        // The element is borrowed from the collection, so the loop variable
        // retains it for the duration of the body, exactly like a binding.
        if self.is_ref(elem) {
            self.rc_inc(e);
            self.owned.last_mut().unwrap().push(name.to_string());
        }
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), (ty, e, true));
        self.loop_vars.push(name.to_string());

        self.loops.push(LoopCtx {
            header,
            exit: exit_bb,
            carried: carried.iter().map(|(x, _, _)| x.clone()).collect(),
            depth: self.owned.len() - 1,
            broke: false,
        });
        let lowered = self.lower_block(body);
        self.loops.pop();
        self.loop_vars.pop();
        lowered?;

        let live = !self.terminated();
        if live {
            self.release_scope();
        }
        self.scopes.pop();
        self.owned.pop();

        if live {
            let back: Vec<Value> = carried
                .iter()
                .map(|(x, _, _)| self.lookup(x).unwrap().1)
                .collect();
            self.terminate(Term::Jump {
                to: header,
                args: back,
            });
        }

        self.switch_to(exit_bb);
        for ((x, _, _), p) in carried.iter().zip(ep.iter()) {
            self.rebind(x, *p);
        }
        self.release_scope();
        self.scopes.pop();
        self.owned.pop();
        Ok(())
    }

    /// `while` lowering. This is the first construct with a back edge.
    ///
    ///     jump header(x0, ..)
    ///   header(xh, ..):          <- loop-carried variables live here
    ///     cond = ..
    ///     brif cond, body, exit
    ///   body:
    ///     ..
    ///     jump header(x', ..)
    ///   exit:
    ///
    /// `exit` carries the same parameters as the header. Without `break` it
    /// would not need any -- the header dominates exit, so a variable could
    /// just resolve to the header parameter. But a `break` jumps to exit from
    /// inside the body with *different* values, so exit is a genuine merge
    /// point and needs its own parameters.
    fn lower_while(&mut self, cond: &Expr, body: &[Stmt], span: Span) -> Result<(), Diag> {
        let mut names = Vec::new();
        Self::assigned_names(body, &mut names);
        // Only variables that exist in the enclosing scope are loop-carried;
        // anything declared inside the body is fresh each iteration.
        names.retain(|n| self.lookup(n).is_some());
        names.sort();

        let carried: Vec<(String, Ty, Value)> = names
            .iter()
            .map(|n| {
                let (ty, v) = self.lookup(n).unwrap();
                (n.clone(), ty, v)
            })
            .collect();

        let header = self.new_block();
        let body_bb = self.new_block();
        let exit_bb = self.new_block();

        let entry_args: Vec<Value> = carried.iter().map(|(_, _, v)| *v).collect();
        self.terminate(Term::Jump {
            to: header,
            args: entry_args,
        });

        let mut header_params = Vec::new();
        for (_, ty, _) in &carried {
            header_params.push(self.new_val(self.irty(*ty)));
        }
        let hi = self.blocks.iter().position(|b| b.id == header).unwrap();
        self.blocks[hi].params = header_params.clone();

        let mut exit_params = Vec::new();
        for (_, ty, _) in &carried {
            exit_params.push(self.new_val(self.irty(*ty)));
        }
        let ei = self.blocks.iter().position(|b| b.id == exit_bb).unwrap();
        self.blocks[ei].params = exit_params.clone();

        self.switch_to(header);
        for ((name, _, _), p) in carried.iter().zip(header_params.iter()) {
            self.rebind(name, *p);
        }

        let c = self.lower_expr(cond)?;
        if c.ty != Ty::Bool {
            return Err(Diag::new(
                cond.span(),
                format!("type mismatch: expected bool, found {}", self.tyname(c.ty)),
            ));
        }
        self.flush_temps();
        self.terminate(Term::Brif {
            cond: c.val(),
            then: body_bb,
            then_args: Vec::new(),
            els: exit_bb,
            els_args: header_params.clone(),
        });

        self.switch_to(body_bb);
        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());
        self.loops.push(LoopCtx {
            header,
            exit: exit_bb,
            carried: carried.iter().map(|(n, _, _)| n.clone()).collect(),
            // The body scope we just pushed is the boundary: break and
            // continue release everything inside it, and nothing outside.
            depth: self.owned.len() - 1,
            broke: false,
        });
        let lowered = self.lower_block(body);
        let broke = self.loops.pop().is_some_and(|l| l.broke);
        lowered?;
        let body_live = !self.terminated();
        if body_live {
            // Release anything the body declared, once per iteration.
            self.release_scope();
        }
        self.scopes.pop();
        self.owned.pop();

        if body_live {
            let back_args: Vec<Value> = carried
                .iter()
                .map(|(n, _, _)| self.lookup(n).map(|x| x.1).unwrap())
                .collect();
            self.terminate(Term::Jump {
                to: header,
                args: back_args,
            });
        }

        self.switch_to(exit_bb);
        // After the loop, each carried variable is the exit parameter, which
        // merges the header's value with whatever any `break` supplied.
        for ((name, _, _), p) in carried.iter().zip(exit_params.iter()) {
            self.rebind(name, *p);
        }
        // `while (true)` with no `break` out of it does not fall through,
        // so the code after it is unreachable and a function ending in one
        // needs no return after it. Only the literal: there is no constant
        // folding, and "the condition is the word `true`" is a rule a reader
        // can check by eye. The exit block still has the header's edge in
        // the CFG, so it is terminated here as the unreachable filler
        // `lower_func` would give it.
        if matches!(cond, Expr::Bool(true, _)) && !broke {
            self.terminate(Term::Ret { val: None });
        }
        let _ = span;
        Ok(())
    }

    /// `match (e) { case V(int x): { .. } .. }`
    ///
    /// Exhaustive and without fallthrough, so the shape is a chain of tag
    /// tests ending in an unconditional jump: the last variant needs no test,
    /// because if it were not that one the match would not have compiled.
    fn lower_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], span: Span) -> Result<(), Diag> {
        // The scrutinee has to outlive every arm, and it may be a temporary,
        // so it is bound into a scope of its own -- the same shape `for .. in`
        // uses for the collection it walks.
        self.synth += 1;
        let hold = format!("$match{}", self.synth);
        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());

        let sc = match self.lower_expr(scrutinee) {
            Ok(v) => v,
            Err(e) => {
                self.scopes.pop();
                self.owned.pop();
                return Err(e);
            }
        };
        let Some(tid) = self
            .tdef_of(sc.ty)
            .filter(|t| self.typedefs[*t as usize].is_enum)
        else {
            self.scopes.pop();
            self.owned.pop();
            return Err(Diag::new(
                scrutinee.span(),
                format!("`match` needs an enum; {} is not one", self.tyname(sc.ty)),
            ));
        };

        if sc.owned {
            self.stmt_temps.retain(|t| *t != sc.val());
        } else {
            self.rc_inc(sc.val());
        }
        self.scopes
            .last_mut()
            .unwrap()
            .insert(hold.clone(), (sc.ty, sc.val(), true));
        self.owned.last_mut().unwrap().push(hold.clone());

        if !self.type_visible(tid) {
            self.scopes.pop();
            self.owned.pop();
            return Err(Diag::new(
                scrutinee.span(),
                self.not_visible(tid, "its variants cannot be matched from here"),
            ));
        }
        let names: Vec<String> = self.typedefs[tid as usize]
            .variants
            .iter()
            .map(|v| v.name.clone())
            .collect();

        // Resolve every arm to a tag first, so a bad arm is reported before
        // any code is emitted for it.
        let mut seen: Vec<usize> = Vec::new();
        for a in arms {
            let Some(tag) = names.iter().position(|n| *n == a.variant) else {
                return Err(Diag::new(
                    a.span,
                    format!(
                        "`{}` has no variant `{}`; it has {}",
                        self.tyname(sc.ty),
                        a.variant,
                        names.join(", ")
                    ),
                ));
            };
            if seen.contains(&tag) {
                return Err(Diag::new(
                    a.span,
                    format!("`{}` is already handled by an earlier case", a.variant),
                ));
            }
            let want = self.variant_surface[tid as usize][tag].clone();
            // A case binds every value the variant carries, or none of them.
            //
            // None is `case Tag:`, the spelling a payload-less variant
            // already uses, and it means what it looks like: this arm does
            // not use the payload. `apps/git` had twenty bindings literally
            // named `ignored`, each spelling out a payload type -- `case
            // Tree(List<object.Entry> ignored)` -- only to throw it away,
            // and `apps/tui`'s `Constraint` has four methods that are 28
            // such arms between them.
            //
            // **This is not a `default`.** Every variant still needs its own
            // case, so adding one is still a compile error at every `match`
            // that has to learn about it, which is the whole reason the
            // check exists (§5.6, docs/errors-decision.md). What it drops is
            // the requirement to name values the arm does not read.
            //
            // All or nothing, with no `_` for one of several: one rule, and
            // an arm that wants two of three payloads can name all three.
            if !a.binds.is_empty() && a.binds.len() != want.len() {
                let head = format!(
                    "`{}` carries {} value(s), and this case binds {}",
                    a.variant,
                    want.len(),
                    a.binds.len()
                );
                // Offering "bind none" only makes sense when there is a
                // payload to decline; a variant that carries nothing has
                // `case Tag:` as its only spelling either way.
                return Err(Diag::new(
                    a.span,
                    if want.is_empty() {
                        head
                    } else {
                        format!(
                            "{head}; bind every one of them, or write `case {}:` to bind none",
                            a.variant
                        )
                    },
                ));
            }
            for (b, w) in a.binds.iter().zip(want.iter()) {
                if !self.assignable(*w, b.ty) {
                    return Err(Diag::new(b.span, self.mismatch(b.ty, *w)));
                }
            }
            seen.push(tag);
        }

        // Exhaustive: no `default`, so adding a variant is a compile error at
        // every match that has to learn about it. That is the whole reason to
        // have the compiler check this.
        let missing: Vec<&str> = names
            .iter()
            .enumerate()
            .filter(|(i, _)| !seen.contains(i))
            .map(|(_, n)| n.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(Diag::new(
                span,
                format!(
                    "`match` must handle every variant of `{}`; missing {}",
                    self.tyname(sc.ty),
                    missing.join(", ")
                ),
            ));
        }

        self.flush_temps();

        let tag_v = self.new_val(IrTy::I64);
        self.push(Inst::EnumTag {
            dst: tag_v,
            obj: sc.val(),
            tid,
        });

        let join_bb = self.new_block();
        let before = self.snapshot();

        let mut ends: Vec<(BlockId, HashMap<String, Binding>)> = Vec::new();
        for (i, a) in arms.iter().enumerate() {
            let tag = seen[i];
            let body_bb = self.new_block();
            let last = i + 1 == arms.len();

            if last {
                // Exhaustive, so whatever is left must be this one.
                self.terminate(Term::Jump {
                    to: body_bb,
                    args: Vec::new(),
                });
            } else {
                let next_bb = self.new_block();
                let k = self.new_val(IrTy::I64);
                self.push(Inst::IConst {
                    dst: k,
                    val: tag as i64,
                });
                let c = self.new_val(IrTy::I1);
                self.push(Inst::ICmp {
                    dst: c,
                    cmp: Cmp::Eq,
                    lhs: tag_v,
                    rhs: k,
                });
                self.terminate(Term::Brif {
                    cond: c,
                    then: body_bb,
                    then_args: Vec::new(),
                    els: next_bb,
                    els_args: Vec::new(),
                });
                self.switch_to(next_bb);
                self.restore(&before);
            }

            let resume = self.blocks[self.cur].id;
            self.switch_to(body_bb);
            self.restore(&before);
            self.scopes.push(HashMap::new());
            self.owned.push(Vec::new());

            // Payload bindings are BORROWED from the enum, exactly like a
            // field read: the scrutinee holds the +1 for the whole match.
            //
            // Borrowed for READING, that is. A payload binding is also the
            // arm's own name for that payload, and nothing else names it, so
            // it may be TAKEN out of the enum to cross a thread boundary --
            // `transfer` does that, and `payload_binds` is what tells it
            // which names it may do it to.
            let binds_from = self.payload_binds.len();
            for (idx, b) in a.binds.iter().enumerate() {
                self.check_shadow(&b.name, b.span)?;
                let d = self.new_val(self.irty(b.ty));
                self.push(Inst::EnumPayload {
                    dst: d,
                    obj: sc.val(),
                    tid,
                    tag: tag as u32,
                    idx: idx as u32,
                });
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(b.name.clone(), (b.ty, d, false));
                if self.is_ref(b.ty) {
                    self.payload_binds.push(PayloadBind {
                        name: b.name.clone(),
                        hold: hold.clone(),
                        tid,
                        tag: tag as u32,
                        idx: idx as u32,
                        depth: self.scopes.len(),
                        sole_owner: sc.owned,
                    });
                }
            }

            self.lower_block(&a.body)?;
            let live = !self.terminated();
            if live {
                self.release_scope();
            }
            self.scopes.pop();
            self.owned.pop();
            // The arm's payload names leave with the arm. A sibling arm may
            // bind the same name, and one arm having moved it must not make
            // the other's unusable.
            let gone: Vec<String> = self
                .payload_binds
                .drain(binds_from..)
                .map(|pb| pb.name)
                .collect();
            self.moved.retain(|n| !gone.contains(n));
            if live {
                ends.push((self.blocks[self.cur].id, self.snapshot()));
            }
            if !last {
                self.switch_to(resume);
            }
        }

        if ends.is_empty() {
            // Every arm returned; nothing reaches the join.
            self.switch_to(join_bb);
            self.restore(&before);
            self.release_scope();
            self.scopes.pop();
            self.owned.pop();
            self.terminate(Term::Ret { val: None });
            return Ok(());
        }

        // Which variables do the arms disagree about?
        let mut changed: Vec<(String, Ty)> = Vec::new();
        for (name, (ty, v0, _)) in &before {
            if ends
                .iter()
                .any(|(_, snap)| snap.get(name).map(|x| x.1).unwrap_or(*v0) != *v0)
            {
                changed.push((name.clone(), *ty));
            }
        }
        changed.sort_by(|a, b| a.0.cmp(&b.0));

        let mut join_params = Vec::new();
        for (_, ty) in &changed {
            join_params.push(self.new_val(self.irty(*ty)));
        }
        let ji = self.blocks.iter().position(|b| b.id == join_bb).unwrap();
        self.blocks[ji].params = join_params.clone();

        for (end, snap) in &ends {
            let args: Vec<Value> = changed
                .iter()
                .map(|(n, _)| snap.get(n).map(|x| x.1).unwrap_or(before[n].1))
                .collect();
            self.switch_to(*end);
            self.terminate(Term::Jump { to: join_bb, args });
        }

        self.switch_to(join_bb);
        self.restore(&before);
        for ((name, _), p) in changed.iter().zip(join_params.iter()) {
            self.rebind(name, *p);
        }
        // Release the scrutinee now that no arm can still be reading it.
        self.release_scope();
        self.scopes.pop();
        self.owned.pop();
        Ok(())
    }

    fn lower_if(
        &mut self,
        cond: &Expr,
        then: &[Stmt],
        els: Option<&[Stmt]>,
        span: Span,
    ) -> Result<(), Diag> {
        let c = self.lower_expr(cond)?;
        if c.ty != Ty::Bool {
            return Err(Diag::new(
                cond.span(),
                format!("type mismatch: expected bool, found {}", self.tyname(c.ty)),
            ));
        }
        self.flush_temps();

        let then_bb = self.new_block();
        let else_bb = self.new_block();
        let join_bb = self.new_block();

        self.terminate(Term::Brif {
            cond: c.val(),
            then: then_bb,
            then_args: Vec::new(),
            els: else_bb,
            els_args: Vec::new(),
        });

        let before = self.snapshot();

        // then arm
        self.switch_to(then_bb);
        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());
        self.lower_block(then)?;
        let then_live = !self.terminated();
        if then_live {
            self.release_scope();
        }
        self.scopes.pop();
        self.owned.pop();
        let after_then = self.snapshot();
        let then_end = self.blocks[self.cur].id;

        // else arm
        self.switch_to(else_bb);
        self.restore(&before);
        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());
        if let Some(e) = els {
            self.lower_block(e)?;
        }
        let else_live = !self.terminated();
        if else_live {
            self.release_scope();
        }
        self.scopes.pop();
        self.owned.pop();
        let after_else = self.snapshot();
        let else_end = self.blocks[self.cur].id;

        if !then_live && !else_live {
            // Both arms returned; nothing reaches the join.
            self.switch_to(join_bb);
            self.restore(&before);
            self.terminate(Term::Ret { val: None });
            let _ = span;
            return Ok(());
        }

        // Which variables do the two arms disagree about?
        let mut changed: Vec<(String, Ty)> = Vec::new();
        for (name, (ty, v0, _)) in &before {
            let a = if then_live {
                after_then.get(name).map(|x| x.1)
            } else {
                None
            };
            let b = if else_live {
                after_else.get(name).map(|x| x.1)
            } else {
                None
            };
            let differs = match (a, b) {
                (Some(x), Some(y)) => x != y,
                (Some(x), None) => x != *v0,
                (None, Some(y)) => y != *v0,
                (None, None) => false,
            };
            if differs {
                changed.push((name.clone(), *ty));
            }
        }
        changed.sort_by(|a, b| a.0.cmp(&b.0));

        let mut join_params = Vec::new();
        for (_, ty) in &changed {
            join_params.push(self.new_val(self.irty(*ty)));
        }
        let ji = self.blocks.iter().position(|b| b.id == join_bb).unwrap();
        self.blocks[ji].params = join_params.clone();

        if then_live {
            let args: Vec<Value> = changed
                .iter()
                .map(|(n, _)| after_then.get(n).map(|x| x.1).unwrap_or(before[n].1))
                .collect();
            self.switch_to(then_end);
            self.terminate(Term::Jump { to: join_bb, args });
        }
        if else_live {
            let args: Vec<Value> = changed
                .iter()
                .map(|(n, _)| after_else.get(n).map(|x| x.1).unwrap_or(before[n].1))
                .collect();
            self.switch_to(else_end);
            self.terminate(Term::Jump { to: join_bb, args });
        }

        self.switch_to(join_bb);
        // Rebuild the outer scope: everything as it was, except the variables
        // the join now carries as parameters.
        self.restore(&before);
        for ((name, _), p) in changed.iter().zip(join_params.iter()) {
            self.rebind(name, *p);
        }
        Ok(())
    }

    fn snapshot(&self) -> HashMap<String, Binding> {
        let mut out = HashMap::new();
        for s in &self.scopes {
            for (k, v) in s {
                out.insert(k.clone(), *v);
            }
        }
        out
    }

    fn restore(&mut self, snap: &HashMap<String, Binding>) {
        for (name, (_, v, _)) in snap {
            if self.lookup(name).is_some() {
                self.rebind(name, *v);
            }
        }
    }
}
