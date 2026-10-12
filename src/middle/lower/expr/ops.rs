//! Binary operators: primitive arithmetic and operator overloads.

use super::*;

impl Lowerer {
    /// The method an operator desugars to, and whether the result is negated.
    ///
    /// Comparison goes through a single `cmp` returning an int, rather than
    /// four separate methods: one implementation gives a total order, and it
    /// cannot be made inconsistent by defining `<` and `>=` differently.
    pub(super) fn op_method(op: BinOp) -> Option<(&'static str, bool)> {
        use BinOp::*;
        Some(match op {
            Add => ("add", false),
            Sub => ("sub", false),
            Mul => ("mul", false),
            Div => ("div", false),
            Rem => ("rem", false),
            Eq => ("eq", false),
            Ne => ("eq", true),
            Lt | Le | Gt | Ge => ("cmp", false),
            // The bit operators are not in the overloadable set: they are
            // defined on the bits of an `int`, and a user type has no bits
            // to speak of until it says what they are, which is a method.
            And | Or | BitAnd | BitOr | BitXor | Shl | Shr => return None,
        })
    }

    /// `a OP b` where `a` is a user type: dispatch to the operator's method.
    pub(super) fn lower_op_overload(
        &mut self,
        op: BinOp,
        a: &Val,
        b: &Val,
        span: Span,
    ) -> Result<Val, Diag> {
        use BinOp::*;
        let Some((mname, negate)) = Self::op_method(op) else {
            return Err(Diag::new(
                span,
                format!("`{}` cannot be overloaded", op.spelling()),
            ));
        };
        let tid = self.tdef_of(a.ty).expect("checked by caller");
        let tname = self.typedefs[tid as usize].name.clone();
        let shown = self.show_name(&tname);
        let key = format!("{tname}.{mname}");

        if self.sigs.contains_key(&key) {
            self.check_method_access(tid, mname, span)?;
        }
        let Some(sig) = self.sigs.get(&key) else {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` on `{shown}` needs a method `{} {shown}.{mname}(..)`",
                    op.spelling(),
                    if mname == "cmp" {
                        "int"
                    } else if mname == "eq" {
                        "bool"
                    } else {
                        &shown
                    }
                ),
            ));
        };
        let (params, ret) = (sig.params.clone(), sig.ret);
        if params.len() != 1 || params[0].ty != b.ty {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` must take one {} parameter to support `{}`",
                    self.bare_name(&key),
                    self.tyname(a.ty),
                    op.spelling()
                ),
            ));
        }

        let want_ret = match mname {
            "cmp" => Ty::Int,
            "eq" => Ty::Bool,
            _ => a.ty,
        };
        if ret != want_ret {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` must return {} to support `{}`",
                    self.bare_name(&key),
                    self.tyname(want_ret),
                    op.spelling()
                ),
            ));
        }

        let d = self.new_val(self.irty(ret));
        self.push(Inst::Call {
            dst: Some(d),
            func: key,
            args: vec![a.val(), b.val()],
        });
        if self.is_ref(ret) {
            self.stmt_temps.push(d);
        }

        match mname {
            "cmp" => {
                // `a < b` is `a.cmp(b) < 0`.
                let zero = self.new_val(IrTy::I64);
                self.push(Inst::IConst { dst: zero, val: 0 });
                let cmp = match op {
                    Lt => Cmp::Lt,
                    Le => Cmp::Le,
                    Gt => Cmp::Gt,
                    Ge => Cmp::Ge,
                    _ => unreachable!(),
                };
                let out = self.new_val(IrTy::I1);
                self.push(Inst::ICmp {
                    dst: out,
                    cmp,
                    lhs: d,
                    rhs: zero,
                });
                Ok(Val::new(out, Ty::Bool, false))
            }
            "eq" if negate => {
                let out = self.new_val(IrTy::I1);
                self.push(Inst::Not { dst: out, src: d });
                Ok(Val::new(out, Ty::Bool, false))
            }
            _ => Ok(Val::new(d, ret, self.is_ref(ret))),
        }
    }

    pub(super) fn lower_bin(
        &mut self,
        op: BinOp,
        l: &Expr,
        r: &Expr,
        span: Span,
    ) -> Result<Val, Diag> {
        use BinOp::*;

        // && and || short-circuit, so they are control flow, not arithmetic.
        if matches!(op, And | Or) {
            let a = self.lower_expr(l)?;
            if a.ty != Ty::Bool {
                return Err(Diag::new(
                    l.span(),
                    format!("type mismatch: expected bool, found {}", self.tyname(a.ty)),
                ));
            }
            let rhs_bb = self.new_block();
            let join_bb = self.new_block();
            let short = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: short,
                val: op == Or,
            });

            if op == And {
                self.terminate(Term::Brif {
                    cond: a.val(),
                    then: rhs_bb,
                    then_args: Vec::new(),
                    els: join_bb,
                    els_args: vec![short],
                });
            } else {
                self.terminate(Term::Brif {
                    cond: a.val(),
                    then: join_bb,
                    then_args: vec![short],
                    els: rhs_bb,
                    els_args: Vec::new(),
                });
            }

            self.switch_to(rhs_bb);
            let mark = self.stmt_temps.len();
            let b = self.lower_expr(r)?;
            if b.ty != Ty::Bool {
                return Err(Diag::new(
                    r.span(),
                    format!("type mismatch: expected bool, found {}", self.tyname(b.ty)),
                ));
            }
            // Release what the operand allocated before leaving its block.
            self.flush_temps_since(mark);
            let rhs_end = self.blocks[self.cur].id;
            self.switch_to(rhs_end);
            self.terminate(Term::Jump {
                to: join_bb,
                args: vec![b.val()],
            });

            let p = self.new_val(IrTy::I1);
            let ji = self.blocks.iter().position(|x| x.id == join_bb).unwrap();
            self.blocks[ji].params = vec![p];
            self.switch_to(join_bb);
            return Ok(Val::new(p, Ty::Bool, false));
        }

        // An operator method is user code, with `a` as its receiver and `b`
        // as its argument; a built-in operator is the runtime, so then only
        // what the right operand runs can disturb the left (`h.s + f(h)`).
        let a = self.lower_expr(l)?;
        let user_op = self.has_operator_methods(a.ty);
        let a = self.hold(l, a, user_op || self.may_run_code(r, true));
        let b = self.lower_expr(r)?;
        let b = self.hold(r, b, user_op);

        // str has built-in `+` and `==`; they are the two everyone reaches
        // for, and making them methods on a builtin would need no less code.
        if a.ty == Ty::Str && b.ty == Ty::Str {
            if op == Add {
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_concat".to_string(),
                    args: vec![a.val(), b.val()],
                });
                self.stmt_temps.push(d);
                return Ok(Val::new(d, Ty::Str, true));
            }
            if op == Eq || op == Ne {
                let d = self.new_val(IrTy::I1);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_str_eq".to_string(),
                    args: vec![a.val(), b.val()],
                });
                if op == Ne {
                    let out = self.new_val(IrTy::I1);
                    self.push(Inst::Not { dst: out, src: d });
                    return Ok(Val::new(out, Ty::Bool, false));
                }
                return Ok(Val::new(d, Ty::Bool, false));
            }
        }

        // `bytes` compares by value, like `str`: two buffers holding the same
        // octets are equal. It has no `+` -- appending is `extend`, in place,
        // which is what a buffer is for -- and no ordering, as `str` has none.
        if a.ty == Ty::Bytes && b.ty == Ty::Bytes && (op == Eq || op == Ne) {
            let d = self.rt_value("rt_bytes_eq", vec![a.val(), b.val()], Ty::Bool);
            if op == Ne {
                let out = self.new_val(IrTy::I1);
                self.push(Inst::Not {
                    dst: out,
                    src: d.val(),
                });
                return Ok(Val::new(out, Ty::Bool, false));
            }
            return Ok(d);
        }

        // A distinct type behaves exactly as its base -- it IS an int -- so
        // arithmetic and comparison work, and the result keeps the distinct
        // type. Mixing with the base needs an explicit conversion, which is
        // the point: Price + Price is a Price, Price + int is a mistake.
        if self.base_of(a.ty).is_some() && a.ty == b.ty {
            let u = self.underlying(a.ty);
            let inner = self.lower_bin_prim(op, u, &a, &b, span)?;
            let out_ty = if inner.ty == Ty::Bool { Ty::Bool } else { a.ty };
            return Ok(Val::new(inner.val(), out_ty, false));
        }

        // A distinct type mixed with something else. Blaming a missing
        // operator method would be misleading -- the type has the operator,
        // it is the operands that disagree.
        if self.base_of(a.ty).is_some() || self.base_of(b.ty).is_some() {
            return Err(Diag::new(
                span,
                format!(
                    "cannot apply `{}` to {} and {}; convert one of them",
                    op.spelling(),
                    self.tyname(a.ty),
                    self.tyname(b.ty)
                ),
            ));
        }

        // A user type on the left: dispatch to the operator's method.
        if matches!(a.ty, Ty::User(_)) {
            return self.lower_op_overload(op, &a, &b, span);
        }

        self.lower_bin_prim(op, a.ty, &a, &b, span)
    }

    /// Arithmetic and comparison on primitives, shared by `int` and by any
    /// distinct type whose base is a primitive.
    pub(super) fn lower_bin_prim(
        &mut self,
        op: BinOp,
        ty: Ty,
        a: &Val,
        b: &Val,
        span: Span,
    ) -> Result<Val, Diag> {
        use BinOp::*;
        let bits = match op {
            BitAnd => Some(ArithOp::And),
            BitOr => Some(ArithOp::Or),
            BitXor => Some(ArithOp::Xor),
            Shl => Some(ArithOp::Shl),
            Shr => Some(ArithOp::Shr),
            _ => None,
        };
        if let Some(bop) = bits {
            if a.ty != b.ty {
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}; convert one of them",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            // `int` only. A float has a bit pattern, but `&` on one would
            // be a truncation or a reinterpretation, and neither should be
            // spelled as an operator -- `to_bits()` says which it is. A
            // `bool` is not an integer here, and the operator it wanted
            // has its own spelling.
            if ty != Ty::Int {
                let hint = match (ty, op) {
                    (Ty::Float, _) => "; bit operations apply only to int".to_string(),
                    (Ty::Bool, BitAnd) => "; for bool use `&&`".to_string(),
                    (Ty::Bool, BitOr) => "; for bool use `||`".to_string(),
                    (Ty::Bool, BitXor) => "; for bool use `!=`".to_string(),
                    _ => String::new(),
                };
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}{hint}",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            let d = self.new_val(IrTy::I64);
            self.push(Inst::Arith {
                dst: d,
                op: bop,
                lhs: a.val(),
                rhs: b.val(),
            });
            return Ok(Val::new(d, Ty::Int, false));
        }
        let arith = match op {
            Add => Some(ArithOp::Add),
            Sub => Some(ArithOp::Sub),
            Mul => Some(ArithOp::Mul),
            Div => Some(ArithOp::Div),
            Rem => Some(ArithOp::Rem),
            _ => None,
        };

        if let Some(aop) = arith {
            // Both operands, not just the left one. This was never checked:
            // the type came from the left operand alone, so `1 + true` was
            // accepted and printed 2, and once float arrived `1.5 + 2` was
            // accepted and silently widened. Nothing here is implicit.
            if a.ty != b.ty {
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}; convert one of them",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            // Float arithmetic is IEEE: it does not trap, it produces an
            // infinity or a NaN. Integer arithmetic traps. Two number types,
            // two honest answers -- and `%` is left off floats the way Go
            // leaves it off, because fmod is a different operation wearing
            // the same spelling.
            if ty == Ty::Float {
                if op == Rem {
                    return Err(Diag::new(
                        span,
                        "`%` is integer remainder; it does not apply to float",
                    ));
                }
                let d = self.new_val(IrTy::F64);
                self.push(Inst::Arith {
                    dst: d,
                    op: aop,
                    lhs: a.val(),
                    rhs: b.val(),
                });
                return Ok(Val::new(d, Ty::Float, false));
            }
            if ty != Ty::Int {
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}",
                        op.spelling(),
                        self.tyname(a.ty),
                        self.tyname(b.ty)
                    ),
                ));
            }
            let d = self.new_val(IrTy::I64);
            self.push(Inst::Arith {
                dst: d,
                op: aop,
                lhs: a.val(),
                rhs: b.val(),
            });
            return Ok(Val::new(d, Ty::Int, false));
        }

        let cmp = match op {
            Eq => Cmp::Eq,
            Ne => Cmp::Ne,
            Lt => Cmp::Lt,
            Le => Cmp::Le,
            Gt => Cmp::Gt,
            Ge => Cmp::Ge,
            _ => unreachable!(),
        };
        if a.ty != b.ty {
            return Err(Diag::new(
                span,
                format!(
                    "cannot compare {} with {}",
                    self.tyname(a.ty),
                    self.tyname(b.ty)
                ),
            ));
        }
        if ty != Ty::Int && ty != Ty::Bool && ty != Ty::Float {
            return Err(Diag::new(
                span,
                format!("cannot compare values of type {}", self.tyname(a.ty)),
            ));
        }
        if ty == Ty::Bool && !matches!(op, Eq | Ne) {
            return Err(Diag::new(span, "bool supports only `==` and `!=`"));
        }
        let d = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: d,
            cmp,
            lhs: a.val(),
            rhs: b.val(),
        });
        Ok(Val::new(d, Ty::Bool, false))
    }
}
