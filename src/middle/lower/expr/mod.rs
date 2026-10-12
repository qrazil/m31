//! Expression lowering: the `lower_expr` dispatch and what is under it
//! -- calls, `new`, binary operators, and their supporting helpers.

use super::{a_or_an, Lowerer, Val, BUILTIN_FNS};
use crate::front::ast::*;
use crate::front::diag::{Diag, Span};
use crate::middle::ir::{ArithOp, Cmp, Inst, IrTy, Term, Value};
use std::collections::HashMap;

mod call;
mod enums;
mod literal;
mod ops;

impl Lowerer {
    /// Lower an expression where the wanted type is known.
    ///
    /// Collection literals have no type of their own -- `[]` says nothing --
    /// so they are legal only where something says what they should be: a
    /// declaration, an assignment, a field, an argument, an enum payload, a
    /// return. Everywhere else the literal forms are refused with a message
    /// saying so, rather than guessing.
    ///
    /// A function's name is the other expression with no type of its own, and
    /// it is a value in exactly the places this function is reached from --
    /// which is the table under "Where the target *is* known" in
    /// docs/closures-decision.md, one row per call site below.
    pub(super) fn lower_expr_as(&mut self, e: &Expr, want: Ty) -> Result<Val, Diag> {
        if let Some(v) = self.lower_fn_ref(e, want)? {
            return Ok(v);
        }
        // A lambda is the same thing unnamed, and is a value in exactly the
        // same places, for the same reason: the target is written down here.
        if let Some(v) = self.lower_lambda(e, want)? {
            return Ok(v);
        }
        match e {
            Expr::SeqLit(..) | Expr::RepeatLit(..) | Expr::MapLit(..) => {
                self.lower_literal(e, Some(want))
            }
            _ => self.lower_expr(e),
        }
    }

    pub(super) fn lower_expr(&mut self, e: &Expr) -> Result<Val, Diag> {
        match e {
            Expr::Float(x, _) => {
                let v = self.new_val(IrTy::F64);
                self.push(Inst::FConst { dst: v, val: *x });
                Ok(Val::new(v, Ty::Float, false))
            }
            Expr::Int(n, _) => {
                let v = self.new_val(IrTy::I64);
                self.push(Inst::IConst { dst: v, val: *n });
                Ok(Val::new(v, Ty::Int, false))
            }
            Expr::Bool(b, _) => {
                let v = self.new_val(IrTy::I1);
                self.push(Inst::BConst { dst: v, val: *b });
                Ok(Val::new(v, Ty::Bool, false))
            }
            Expr::Str(s, _) => {
                let idx = match self.strings.iter().position(|x| x == s) {
                    Some(i) => i as u32,
                    None => {
                        self.strings.push(s.clone());
                        (self.strings.len() - 1) as u32
                    }
                };
                let v = self.new_val(IrTy::Ref);
                self.push(Inst::SConst { dst: v, idx });
                // Immortal: borrowed, never owned. docs/ir-v0.md §5.4
                Ok(Val::new(v, Ty::Str, false))
            }
            Expr::This(span) => self.this_val(*span),
            // A lambda somewhere nothing says what type is wanted. Like a
            // function's name, it is checked against a target and is never
            // a source of inference, so with no target there is nothing to
            // check it against and not even a method name to give it.
            Expr::Lambda(_, _, span) => Err(Self::lambda_no_target(*span)),
            Expr::Var(name, span) => {
                if self.moved.iter().any(|n| n == name) {
                    return Err(Diag::new(
                        *span,
                        format!("`{name}` was moved and cannot be used again"),
                    ));
                }
                if let Some((ty, v)) = self.lookup(name) {
                    return Ok(Val::new(v, ty, false));
                }
                // Inside a method, a bare name may be a field of the
                // receiver. Unambiguous because nothing shadows anything.
                if let Some((tid, obj, path)) = self.recv_field(name) {
                    // A method is in its type's module, so only a field
                    // promoted from another module's type can be private.
                    self.check_field_access(tid, &path, name, *span)?;
                    let (d, fty) = self.load_path(tid, obj, &path);
                    // Borrowed from the receiver, which holds the +1.
                    return Ok(Val::new(d, fty, false));
                }
                // Last, a module constant. The order cannot matter -- no
                // local, parameter or field may take a constant's name
                // (`check_shadow`) -- so this is only the cheapest order.
                if let Some(key) = self.resolve_const(name) {
                    return Ok(self.lower_const_use(&key));
                }
                if let Some(d) = self.foreign_const_hint(name, *span) {
                    return Err(d);
                }
                // A function's name, somewhere nothing says what type is
                // wanted: an argument to an unconstrained generic parameter,
                // an argument to a builtin, an expression statement. It is
                // checked against the interface it is given to and is never
                // a source of inference, so with no target there is nothing
                // to check it against (docs/closures-decision.md, "Where the
                // target is *not* known").
                if let Some(d) = self.fn_ref_no_target(e) {
                    return Err(d);
                }
                Err(self.unknown_variable(name, *span))
            }
            Expr::Un(op, inner, span) => {
                let a = self.lower_expr(inner)?;
                match op {
                    UnOp::Neg if self.underlying(a.ty) == Ty::Float => {
                        // Multiply by -1.0, NOT `0.0 - x`. Subtraction gets
                        // zero wrong: IEEE says 0.0 - 0.0 is +0.0, so `-0.0`
                        // would come out positive. Multiplication flips the
                        // sign bit in every case, zeroes and infinities
                        // included.
                        let m = self.new_val(IrTy::F64);
                        self.push(Inst::FConst { dst: m, val: -1.0 });
                        let d = self.new_val(IrTy::F64);
                        self.push(Inst::Arith {
                            dst: d,
                            op: ArithOp::Mul,
                            lhs: a.val(),
                            rhs: m,
                        });
                        Ok(Val::new(d, Ty::Float, false))
                    }
                    UnOp::Neg => {
                        if a.ty != Ty::Int {
                            return Err(Diag::new(
                                *span,
                                format!("cannot negate a value of type {}", self.tyname(a.ty)),
                            ));
                        }
                        // Lower as 0 - x so the overflow check is shared; this
                        // is also what makes -INT64_MIN trap rather than wrap.
                        let z = self.new_val(IrTy::I64);
                        self.push(Inst::IConst { dst: z, val: 0 });
                        let d = self.new_val(IrTy::I64);
                        self.push(Inst::Arith {
                            dst: d,
                            op: ArithOp::Sub,
                            lhs: z,
                            rhs: a.val(),
                        });
                        Ok(Val::new(d, Ty::Int, false))
                    }
                    UnOp::BitNot => {
                        // Held to `int` alone, like unary minus: a `bool` is
                        // not an integer, and a float has no bit operations.
                        if a.ty != Ty::Int {
                            return Err(Diag::new(
                                *span,
                                format!(
                                    "cannot apply `~` to a value of type {}",
                                    self.tyname(a.ty)
                                ),
                            ));
                        }
                        // `~x` is `x ^ -1`: one bit operation fewer to carry
                        // through the IR and the runtime, and the identity is
                        // exact in two's complement.
                        let ones = self.new_val(IrTy::I64);
                        self.push(Inst::IConst { dst: ones, val: -1 });
                        let d = self.new_val(IrTy::I64);
                        self.push(Inst::Arith {
                            dst: d,
                            op: ArithOp::Xor,
                            lhs: a.val(),
                            rhs: ones,
                        });
                        Ok(Val::new(d, Ty::Int, false))
                    }
                    UnOp::Not => {
                        if a.ty != Ty::Bool {
                            return Err(Diag::new(
                                *span,
                                format!(
                                    "cannot apply `!` to a value of type {}",
                                    self.tyname(a.ty)
                                ),
                            ));
                        }
                        let d = self.new_val(IrTy::I1);
                        self.push(Inst::Not {
                            dst: d,
                            src: a.val(),
                        });
                        Ok(Val::new(d, Ty::Bool, false))
                    }
                }
            }
            Expr::Bin(op, l, r, span) => self.lower_bin(*op, l, r, *span),
            Expr::Call(name, args, span) => {
                // Inside an instance method, a bare call may name a method of
                // the receiver -- its sibling -- exactly as a bare name may
                // name one of its fields.
                if let Some(v) = self.sibling_call(name, args, *span)? {
                    return Ok(v);
                }
                // `order(a, b)` where `order` is a value. Now that a callback
                // can be written down this is the likeliest mistake a
                // newcomer makes, and "unknown function `order`" is no help:
                // the name IS in scope, and a callback is called the way
                // every interface value is, through its method.
                if let Some(d) = self.call_of_a_value(name, *span) {
                    return Err(d);
                }
                // A bare name means this module's declaration, then a
                // builtin. Another module's name is not in scope at all --
                // which is what makes `pub` mean something once every file
                // has been merged into one program. The check lives here
                // rather than in `lower_call`, because the qualified form
                // goes through the same function and has already earned its
                // access.
                let key = self.resolve_fn(name);
                // Not ours: if some other module declares it, say which and
                // how to reach it rather than "unknown function". Never for
                // a builtin, which is in scope everywhere whatever other
                // modules declare -- only some builtins are in `sigs`, so
                // the lookup alone cannot tell.
                if !self.sigs.contains_key(&key) && !BUILTIN_FNS.contains(&name.as_str()) {
                    let suffix = format!("#{name}");
                    if let Some((k, sig)) = self
                        .sigs
                        .iter()
                        .find(|(k, _)| k.ends_with(&suffix) && !k.contains('.'))
                    {
                        let owner = sig.module.clone();
                        let _ = k;
                        return Err(Diag::new(
                            *span,
                            if sig.is_pub {
                                format!(
                                    "`{name}` is declared in `{owner}`; write `{owner}.{name}`{}",
                                    self.and_import(&owner)
                                )
                            } else {
                                format!("`{name}` is private to `{owner}`")
                            },
                        ));
                    }
                }
                if let Some(sig) = self.sigs.get(&key) {
                    if !sig.module.is_empty() && sig.module != self.cur_module {
                        return Err(Diag::new(
                            *span,
                            if sig.is_pub {
                                format!(
                                    "`{name}` is declared in `{}`; write `{}.{name}`{}",
                                    sig.module,
                                    sig.module,
                                    self.and_import(&sig.module)
                                )
                            } else {
                                format!("`{name}` is private to `{}`", sig.module)
                            },
                        ));
                    }
                }
                self.lower_call(&key, args, *span)
            }

            Expr::Field(obj, field, span) => {
                if let Expr::This(ts) = &**obj {
                    self.refuse_this_field(field, *ts, *span)?;
                }
                // `lib.MAX` -- another module's constant, not a field of a
                // variable called `lib`.
                if let Some(key) = self.qualified_const(obj, field, *span)? {
                    return Ok(self.lower_const_use(&key));
                }
                // `lib.by_x` -- another module's function, named as a value
                // where nothing says an interface is wanted. Same refusal as
                // the bare spelling, rather than "unknown variable `lib`".
                if let Some(d) = self.fn_ref_no_target(e) {
                    return Err(d);
                }
                let o = self.lower_expr(obj)?;
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("type {} has no fields", self.tyname(o.ty)),
                    ));
                };
                if !self.type_visible(tid) {
                    return Err(Diag::new(
                        *span,
                        self.not_visible(tid, "its fields cannot be read from here"),
                    ));
                }
                let Some(path) = self.field_path(tid, field) else {
                    return Err(Diag::new(
                        *span,
                        format!("type `{}` has no field `{field}`", self.tyname(o.ty)),
                    ));
                };
                self.check_field_access(tid, &path, field, *span)?;
                let (d, fty) = self.load_path(tid, o.val(), &path);
                // A field read is BORROWED from the object, exactly like a
                // local: the object holds the +1, we do not.
                Ok(Val::new(d, fty, false))
            }

            Expr::MethodCall(obj, m, args, span) => {
                // `greet.hello(..)` -- a module qualifier, not a receiver.
                // Checked before lowering the "receiver", because there is
                // no value to lower.
                if let Expr::Var(modname, _) = &**obj {
                    if self.module_in_scope(modname) && self.lookup(modname).is_none() {
                        return self.lower_qualified(modname, m, args, *span);
                    }
                }
                if let Expr::This(ts) = &**obj {
                    self.this_val(*ts)?;
                    let (rtid, _) = self.recv.expect("this_val checked the receiver");
                    self.refuse_destructor_call(rtid, m, *span)?;
                    if self.sibling_method(m).is_some() {
                        // `m` may be a generic method's instantiation, which
                        // monomorphisation renamed; say it as it was written.
                        let m = match self.shown.get(m) {
                            Some((generic, _)) => crate::front::ast::bare(generic).to_string(),
                            None => m.clone(),
                        };
                        return Err(Diag::new(
                            *span,
                            format!(
                                "write `{m}(..)`, not `this.{m}(..)`: a method of the \
                                 receiver is called by its bare name, as a field is read"
                            ),
                        ));
                    }
                }
                let o = self.lower_expr(obj)?;
                // `TABLE.sort()`, `xs.push(1)` on a const local: a change the
                // compiler can see, refused here. One it cannot see -- the
                // same table reached through a parameter -- traps at run
                // time instead (runtime/rt.c, `rt_check_mutable`).
                if self.is_mutating_method(o.ty, m) {
                    self.refuse_const_write(obj, *span)?;
                }
                // The receiver is an argument like any other (§5.1): read
                // out of a place, it is held across the arguments and the
                // call when either may run user code -- `h.p.m(..)` whose
                // body replaces `h.p`, or `h.ps.push(f(h))` where `f` does.
                let later = self.method_runs_code(o.ty, m)
                    || args.pos.iter().any(|a| self.may_run_code(a, true))
                    || args.named.iter().any(|(_, a)| self.may_run_code(a, true));
                let o = self.hold(obj, o, later);
                self.lower_method_on(&o, m, args, *span)
            }

            Expr::Index(obj, idx, span) => {
                let o = self.lower_expr(obj)?;
                // `h.ps[f(h)]`: the index can replace the collection before
                // it is read from.
                let o = self.hold(obj, o, self.may_run_code(idx, true));
                // A byte reads as an int, 0 to 255: the language has one
                // integer type, and a byte is a value of it rather than a
                // second kind of number.
                if self.underlying(o.ty) == Ty::Bytes {
                    let i = self.index_of_bytes(idx)?;
                    return Ok(self.rt_value("rt_bytes_get", vec![o.val(), i], Ty::Int));
                }
                let Some(elem) = self.seq_elem(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("{} cannot be indexed", self.tyname(o.ty)),
                    ));
                };
                let i = self.lower_expr(idx)?;
                if self.underlying(i.ty) != Ty::Int {
                    return Err(Diag::new(idx.span(), self.mismatch(Ty::Int, i.ty)));
                }
                let d = self.new_val(self.irty(elem));
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_index_get".to_string(),
                    args: vec![o.val(), i.val()],
                });
                // Borrowed from the collection, which holds the +1 -- the
                // same rule as reading a field.
                Ok(Val::new(d, elem, false))
            }

            Expr::New(ty, args, span) => self.lower_new(*ty, args, *span),
            Expr::EnumNew(ty, variant, args, span) => {
                self.lower_enum_new(*ty, variant, args, *span)
            }
            Expr::Try(inner, span) => self.lower_try(inner, *span),
            Expr::SeqLit(..) | Expr::RepeatLit(..) | Expr::MapLit(..) => {
                self.lower_literal(e, None)
            }
        }
    }
}
