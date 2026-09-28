//! The builtin methods on `str`, `bytes`, the sequence types, `Option`,
//! `Result`, `Map`, channels and the primitive types.

use super::{Lowerer, Val};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{ArithOp, Cmp, Inst, IrTy, Term, Value};

impl Lowerer {
    /// `s.size()`. The only method a `str` has for now; the string library
    /// will land here rather than as free functions.
    pub(super) fn lower_str_method(
        &mut self,
        o: &Val,
        m: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if !args.named.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no named arguments")));
        }
        // How many positional arguments each takes, and of what.
        let want: &[Ty] = match m {
            "size" | "trim" | "to_upper" | "to_lower" | "parse_int" | "parse_float"
            | "to_bytes" | "chars" => &[],
            "byte_at" => &[Ty::Int],
            "substr" => &[Ty::Int, Ty::Int],
            "repeat" => &[Ty::Int],
            "contains" | "starts_with" | "ends_with" | "index_of" | "split" => &[Ty::Str],
            _ => &[Ty::Void], // unknown; reported below
        };
        if want != [Ty::Void] && args.pos.len() != want.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{m}` takes {} argument(s), found {}",
                    want.len(),
                    args.pos.len()
                ),
            ));
        }
        let mut av = Vec::new();
        if want != [Ty::Void] {
            for (a, w) in args.pos.iter().zip(want.iter()) {
                let v = self.lower_expr(a)?;
                if !self.assignable(v.ty, *w) {
                    return Err(Diag::new(a.span(), self.mismatch(*w, v.ty)));
                }
                av.push(v.val());
            }
        }

        match m {
            "size" => {
                let d = self.new_val(IrTy::I64);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_len".to_string(),
                    args: vec![o.val()],
                });
                Ok(Val::new(d, Ty::Int, false))
            }
            "byte_at" => {
                let d = self.new_val(IrTy::I64);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_str_byte_at".to_string(),
                    args: vec![o.val(), av[0]],
                });
                Ok(Val::new(d, Ty::Int, false))
            }
            // Parsing answers "did it parse", which is Option-shaped. It is
            // lossy on purpose -- "not a number" and "out of range" are both
            // None -- because a built-in cannot return a library's own error
            // type, and the question it answers does not need one.
            // Parsing a float is language source, lib/__floatfmt.src, which
            // builds the Option itself.
            "parse_float" => {
                let Some((oty, ..)) = self.option_of(Ty::Float) else {
                    return Err(Diag::new(
                        span,
                        "`parse_float` has no Option type to return; this is a compiler bug",
                    ));
                };
                let d = self.float_text("parse", oty, o.val(), span)?;
                Ok(Val::new(d, oty, true))
            }
            "parse_int" => {
                let Some((oty, otid, none_tag, some_tag)) = self.option_of(Ty::Int) else {
                    return Err(Diag::new(
                        span,
                        "`parse_int` has no Option type to return; this is a compiler bug",
                    ));
                };
                let raw = self.new_val(IrTy::I64);
                let ok = self.new_val(IrTy::I1);
                self.push(Inst::ParseInto {
                    ok,
                    dst: raw,
                    func: "rt_str_parse_int".to_string(),
                    src: o.val(),
                });
                let d = self.select_option(ok, raw, otid, some_tag, none_tag);
                Ok(Val::new(d, oty, true))
            }
            // Every one of these builds a NEW string, so the caller owns it.
            "substr" | "trim" | "to_upper" | "to_lower" | "repeat" => {
                let (func, extra): (&str, Vec<Value>) = match m {
                    "substr" => ("rt_str_substr", av.clone()),
                    "trim" => ("rt_str_trim", vec![]),
                    "repeat" => ("rt_str_repeat", av.clone()),
                    _ => {
                        let up = self.new_val(IrTy::I1);
                        self.push(Inst::BConst {
                            dst: up,
                            val: m == "to_upper",
                        });
                        ("rt_str_case", vec![up])
                    }
                };
                let d = self.new_val(IrTy::Ref);
                let mut a = vec![o.val()];
                a.extend(extra);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: func.to_string(),
                    args: a,
                });
                self.stmt_temps.push(d);
                Ok(Val::new(d, Ty::Str, true))
            }
            "starts_with" | "ends_with" | "contains" => {
                let d = self.new_val(IrTy::I1);
                if m == "contains" {
                    // Substring search, reusing find: "is it in there" is the
                    // same question `contains` answers on a collection.
                    let at = self.new_val(IrTy::I64);
                    self.push(Inst::Call {
                        dst: Some(at),
                        func: "rt_str_find".to_string(),
                        args: vec![o.val(), av[0]],
                    });
                    let zero = self.new_val(IrTy::I64);
                    self.push(Inst::IConst { dst: zero, val: 0 });
                    self.push(Inst::ICmp {
                        dst: d,
                        cmp: Cmp::Ge,
                        lhs: at,
                        rhs: zero,
                    });
                } else {
                    let func = if m == "starts_with" {
                        "rt_str_starts_with"
                    } else {
                        "rt_str_ends_with"
                    };
                    self.push(Inst::Call {
                        dst: Some(d),
                        func: func.to_string(),
                        args: vec![o.val(), av[0]],
                    });
                }
                Ok(Val::new(d, Ty::Bool, false))
            }
            "index_of" => {
                let Some((oty, otid, none_tag, some_tag)) = self.option_of(Ty::Int) else {
                    return Err(Diag::new(
                        span,
                        "`index_of` has no Option type to return; this is a compiler bug",
                    ));
                };
                let raw = self.new_val(IrTy::I64);
                self.push(Inst::Call {
                    dst: Some(raw),
                    func: "rt_str_find".to_string(),
                    args: vec![o.val(), av[0]],
                });
                let d = self.wrap_option(raw, otid, some_tag, none_tag);
                Ok(Val::new(d, oty, true))
            }
            "split" => {
                let Some(lty) = self.list_of(Ty::Str) else {
                    return Err(Diag::new(
                        span,
                        "`split` has no List type to return; this is a compiler bug",
                    ));
                };
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_str_split".to_string(),
                    args: vec![o.val(), av[0]],
                });
                self.stmt_temps.push(d);
                Ok(Val::new(d, lty, true))
            }
            // A copy, because a `bytes` is mutable and the string is not.
            // It cannot fail: a `str` is already a run of bytes, and this
            // only hands them over without interpreting them.
            "to_bytes" => Ok(self.rt_value("rt_str_to_bytes", vec![o.val()], Ty::Bytes)),
            // The Unicode scalar values, decoded in language source
            // (lib/__text.src). A list rather than a new loop form, so
            // `for (int c in s.chars())` needs nothing the language lacks.
            "chars" => {
                let Some(lty) = self.list_of(Ty::Int) else {
                    return Err(Diag::new(
                        span,
                        "`chars` has no List<int> type to return; this is a compiler bug",
                    ));
                };
                let d = self.text_call("chars", o.val(), span)?;
                Ok(Val::new(d, lty, true))
            }
            "to_str" => {
                // A `str` already is one. Returning it unchanged keeps
                // `v.to_str()` writable whatever `v` is, which is what makes
                // to_str a rule rather than a special case.
                Ok(Val::new(o.val(), Ty::Str, false))
            }
            "len" => Err(Diag::new(span, "`str` has no method `len`; it is `size()`")),
            // The name a reader will guess for "how many characters". There
            // is one spelling, and it shows that the count walks the string.
            "char_count" | "length" => Err(Diag::new(
                span,
                format!(
                    "`str` has no method `{m}`; `size()` counts bytes, and \
                     `chars().size()` counts code points"
                ),
            )),
            other => Err(Diag::new(
                span,
                format!(
                    "`str` has no method `{other}`; it has size, substr, contains, \
                     index_of, starts_with, ends_with, split, trim, to_upper, \
                     to_lower, repeat, byte_at, chars and to_bytes"
                ),
            )),
        }
    }

    /// A runtime call that produces a value. An owned reference result is
    /// put on the statement's pending list, as every producer does, so the
    /// caller only has to say what it made.
    pub(super) fn rt_value(&mut self, func: &str, args: Vec<Value>, ty: Ty) -> Val {
        let d = self.new_val(self.irty(ty));
        self.push(Inst::Call {
            dst: Some(d),
            func: func.to_string(),
            args,
        });
        let owned = self.is_ref(ty);
        if owned {
            self.stmt_temps.push(d);
        }
        Val::new(d, ty, owned)
    }

    pub(super) fn rt_void(&mut self, func: &str, args: Vec<Value>) -> Val {
        self.push(Inst::Call {
            dst: None,
            func: func.to_string(),
            args,
        });
        Val::void()
    }

    /// Refuse an integer literal that cannot be a byte, where one is written
    /// straight into a `bytes`. Anything computed is checked by the runtime,
    /// which traps; a constant can be refused before the program runs, and
    /// `[1, 2, 256]` is always a typo rather than a condition to handle.
    pub(super) fn check_byte_literal(&self, e: &Expr) -> Result<(), Diag> {
        let n = match e {
            Expr::Int(n, _) => *n,
            Expr::Un(UnOp::Neg, inner, _) => match &**inner {
                // Wrapping: `-0x8000_0000_0000_0000` is a legal literal
                // negated, and must be refused as a byte, not panic here.
                Expr::Int(n, _) if *n != 0 => n.wrapping_neg(),
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        if !(0..=255).contains(&n) {
            return Err(Diag::new(
                e.span(),
                format!("{n} is not a byte; a byte is an int from 0 to 255"),
            ));
        }
        Ok(())
    }

    /// An index into a `bytes`, by the rule a List's follows: an `int`, or
    /// a distinct type over one.
    pub(super) fn index_of_bytes(&mut self, idx: &Expr) -> Result<Value, Diag> {
        let i = self.lower_expr(idx)?;
        if self.underlying(i.ty) != Ty::Int {
            return Err(Diag::new(idx.span(), self.mismatch(Ty::Int, i.ty)));
        }
        Ok(i.val())
    }

    /// Lower one argument that must be of type `want`.
    pub(super) fn arg_of(&mut self, e: &Expr, want: Ty) -> Result<Value, Diag> {
        let v = self.lower_expr(e)?;
        if !self.assignable(v.ty, want) {
            return Err(Diag::new(e.span(), self.mismatch(want, v.ty)));
        }
        Ok(v.val())
    }

    /// The methods of `bytes`.
    ///
    /// The names are `str`'s wherever `str` has the question, with the same
    /// meaning over octets -- `b.index_of(sub)` finds a run of bytes, as
    /// `s.index_of(sub)` finds a run of text -- so learning one teaches the
    /// other. On top of that, what a buffer needs and `str` cannot have
    /// because it is immutable: `push`, `pop`, `clear` and `extend`, which
    /// change `b` in place. Everything else returns a new `bytes`.
    pub(super) fn lower_bytes_method(
        &mut self,
        o: &Val,
        m: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if !args.named.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no named arguments")));
        }
        let want: &[Ty] = match m {
            "size" | "pop" | "clear" | "trim" | "to_upper" | "to_lower" | "hex" | "utf8" => &[],
            "push" | "repeat" | "truncate" | "drop_front" => &[Ty::Int],
            "substr" => &[Ty::Int, Ty::Int],
            "extend" | "contains" | "index_of" | "starts_with" | "ends_with" | "split" => {
                &[Ty::Bytes]
            }
            // A text form would have to pick an encoding or an escaping and
            // then pretend it was the only one. `hex()` and `utf8()` each
            // say which they mean, and `print` and `str()` go through
            // `to_str`, so leaving it out is what makes them refuse too.
            "to_str" => {
                return Err(Diag::new(
                    span,
                    "`bytes` has no `to_str`: say which text you mean, \
                     `hex()` or `utf8()`",
                ))
            }
            "byte_at" => {
                return Err(Diag::new(
                    span,
                    "`bytes` has no `byte_at`; index it, as `b[i]`",
                ))
            }
            "len" => {
                return Err(Diag::new(
                    span,
                    "`bytes` has no method `len`; it is `size()`",
                ))
            }
            other => {
                return Err(Diag::new(
                    span,
                    format!(
                        "`bytes` has no method `{other}`; it has size, push, pop, \
                         clear, truncate, drop_front, extend, substr, contains, \
                         index_of, starts_with, ends_with, split, trim, to_upper, \
                         to_lower, repeat, hex and utf8"
                    ),
                ))
            }
        };
        if args.pos.len() != want.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{m}` takes {} argument(s), found {}",
                    want.len(),
                    args.pos.len()
                ),
            ));
        }
        if m == "push" {
            self.check_byte_literal(&args.pos[0])?;
        }
        let mut av = vec![o.val()];
        for (a, w) in args.pos.iter().zip(want.iter()) {
            av.push(self.arg_of(a, *w)?);
        }

        Ok(match m {
            "size" => self.rt_value("rt_bytes_len", av, Ty::Int),
            "push" => self.rt_void("rt_bytes_push", av),
            "pop" => self.rt_value("rt_bytes_pop", av, Ty::Int),
            "clear" => self.rt_void("rt_bytes_clear", av),
            // The two ways a buffer shrinks short of empty, both in place
            // and both keeping the allocation, as `clear` does.
            "truncate" => self.rt_void("rt_bytes_truncate", av),
            "drop_front" => self.rt_void("rt_bytes_drop_front", av),
            "extend" => self.rt_void("rt_bytes_extend", av),
            "substr" => self.rt_value("rt_bytes_substr", av, Ty::Bytes),
            "trim" => self.rt_value("rt_bytes_trim", av, Ty::Bytes),
            "repeat" => self.rt_value("rt_bytes_repeat", av, Ty::Bytes),
            "to_upper" | "to_lower" => {
                let up = self.new_val(IrTy::I1);
                self.push(Inst::BConst {
                    dst: up,
                    val: m == "to_upper",
                });
                av.push(up);
                self.rt_value("rt_bytes_case", av, Ty::Bytes)
            }
            "starts_with" => self.rt_value("rt_bytes_starts_with", av, Ty::Bool),
            "ends_with" => self.rt_value("rt_bytes_ends_with", av, Ty::Bool),
            "contains" => {
                // The same question `str.contains` answers, through the same
                // search `index_of` uses.
                let at = self.rt_value("rt_bytes_find", av, Ty::Int);
                let zero = self.new_val(IrTy::I64);
                self.push(Inst::IConst { dst: zero, val: 0 });
                let d = self.new_val(IrTy::I1);
                self.push(Inst::ICmp {
                    dst: d,
                    cmp: Cmp::Ge,
                    lhs: at.val(),
                    rhs: zero,
                });
                Val::new(d, Ty::Bool, false)
            }
            "index_of" => {
                let Some((oty, otid, none_tag, some_tag)) = self.option_of(Ty::Int) else {
                    return Err(Diag::new(
                        span,
                        "`index_of` has no Option type to return; this is a compiler bug",
                    ));
                };
                let raw = self.rt_value("rt_bytes_find", av, Ty::Int);
                let d = self.wrap_option(raw.val(), otid, some_tag, none_tag);
                Val::new(d, oty, true)
            }
            "split" => {
                let Some(lty) = self.list_of(Ty::Bytes) else {
                    return Err(Diag::new(
                        span,
                        "`split` has no List type to return; this is a compiler bug",
                    ));
                };
                self.rt_value("rt_bytes_split", av, lty)
            }
            "hex" => self.rt_value("rt_bytes_hex", av, Ty::Str),
            // Decoding can fail, so it answers with an Option, the way
            // `parse_int` does: "is this UTF-8" is a yes-or-no question and a
            // built-in cannot return a library's own error type. The string
            // is only built when the answer is yes, and the Option takes
            // ownership of it.
            "utf8" => {
                let Some((oty, otid, none_tag, some_tag)) = self.option_of(Ty::Str) else {
                    return Err(Diag::new(
                        span,
                        "`utf8` has no Option type to return; this is a compiler bug",
                    ));
                };
                let raw = self.new_val(IrTy::Ref);
                let ok = self.new_val(IrTy::I1);
                self.push(Inst::ParseInto {
                    ok,
                    dst: raw,
                    func: "rt_bytes_utf8".to_string(),
                    src: o.val(),
                });
                let d = self.select_option(ok, raw, otid, some_tag, none_tag);
                Val::new(d, oty, true)
            }
            _ => unreachable!("every name was checked above"),
        })
    }

    /// `xs.size()`, `xs.push(v)`, `xs.pop()`.
    pub(super) fn lower_seq_method(
        &mut self,
        o: &Val,
        elem: Ty,
        m: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if !args.named.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no named arguments")));
        }
        let growable = self.is_list(o.ty);
        match m {
            "size" => {
                if !args.pos.is_empty() {
                    return Err(Diag::new(span, "`size` takes no arguments"));
                }
                let d = self.new_val(IrTy::I64);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_len_of".to_string(),
                    args: vec![o.val()],
                });
                Ok(Val::new(d, Ty::Int, false))
            }
            "push" if growable => {
                if args.pos.len() != 1 {
                    return Err(Diag::new(span, "`push` takes one argument"));
                }
                let v = self.lower_expr_as(&args.pos[0], elem)?;
                if !self.assignable(v.ty, elem) {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(elem, v.ty)));
                }
                // The list takes a reference, exactly as a field would.
                if self.is_ref(elem) {
                    if v.owned {
                        self.stmt_temps.retain(|t| *t != v.val());
                    } else {
                        self.rc_inc(v.val());
                    }
                }
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_list_push".to_string(),
                    args: vec![o.val(), v.val()],
                });
                Ok(Val::void())
            }
            "pop" if growable => {
                if !args.pos.is_empty() {
                    return Err(Diag::new(span, "`pop` takes no arguments"));
                }
                let d = self.new_val(self.irty(elem));
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_list_pop".to_string(),
                    args: vec![o.val()],
                });
                // The list gives up its reference; the caller receives it.
                let owned = self.is_ref(elem);
                if owned {
                    self.stmt_temps.push(d);
                }
                Ok(Val::new(d, elem, owned))
            }
            "insert" if growable => {
                if args.pos.len() != 2 {
                    return Err(Diag::new(span, "`insert` takes an index and a value"));
                }
                let i = self.lower_expr(&args.pos[0])?;
                if self.underlying(i.ty) != Ty::Int {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(Ty::Int, i.ty)));
                }
                let v = self.lower_expr_as(&args.pos[1], elem)?;
                if !self.assignable(v.ty, elem) {
                    return Err(Diag::new(args.pos[1].span(), self.mismatch(elem, v.ty)));
                }
                // The list takes a reference, exactly as `push` does.
                if self.is_ref(elem) {
                    if v.owned {
                        self.stmt_temps.retain(|t| *t != v.val());
                    } else {
                        self.rc_inc(v.val());
                    }
                }
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_list_insert".to_string(),
                    args: vec![o.val(), i.val(), v.val()],
                });
                Ok(Val::void())
            }
            "remove_at" if growable => {
                if args.pos.len() != 1 {
                    return Err(Diag::new(span, "`remove_at` takes one argument"));
                }
                let i = self.lower_expr(&args.pos[0])?;
                if self.underlying(i.ty) != Ty::Int {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(Ty::Int, i.ty)));
                }
                let d = self.new_val(self.irty(elem));
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_list_remove_at".to_string(),
                    args: vec![o.val(), i.val()],
                });
                // The list gives up its reference; the caller receives it.
                let owned = self.is_ref(elem);
                if owned {
                    self.stmt_temps.push(d);
                }
                Ok(Val::new(d, elem, owned))
            }
            "clear" if growable => {
                if !args.pos.is_empty() {
                    return Err(Diag::new(span, "`clear` takes no arguments"));
                }
                let refs = self.new_val(IrTy::I1);
                let is_ref = self.is_ref(elem);
                self.push(Inst::BConst {
                    dst: refs,
                    val: is_ref,
                });
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_list_clear".to_string(),
                    args: vec![o.val(), refs],
                });
                Ok(Val::void())
            }
            "sort" => {
                if !args.pos.is_empty() {
                    return Err(Diag::new(span, "`sort` takes no arguments"));
                }
                // Ordering `int`, `float` and `str` needs nothing from the
                // program. A user type orders itself with `cmp`, the method
                // the comparison operators already use, and the runtime
                // reaches it without any function reference in the IR: it is
                // holding the element, the element's header names its
                // TypeInfo, and the compiler put `cmp` there
                // (docs/closures-decision.md, runtime/rt.c `rt_sort_obj`).
                let f = match self.underlying(elem) {
                    Ty::Int => "rt_sort_int",
                    Ty::Float => "rt_sort_float",
                    Ty::Str => "rt_sort_str",
                    base => {
                        let shown = self.tyname(elem);
                        let Some(tid) = self.tdef_of(base) else {
                            return Err(Diag::new(
                                span,
                                format!(
                                    "`sort` orders `int`, `float` and `str`, and any type \
                                     that declares `int T.cmp(T other)`; `{shown}` is none \
                                     of those"
                                ),
                            ));
                        };
                        // An interface would break the one assumption
                        // `rt_sort_obj` makes: that both elements handed to
                        // a `cmp` are of the type that declared it. Two
                        // elements of a `List<Shape>` can be a Circle and a
                        // Square, and `Circle.cmp` would be given a Square.
                        if self.typedefs[tid as usize].is_interface {
                            return Err(Diag::new(
                                span,
                                format!(
                                    "`sort` orders each element by its own type's `cmp`, \
                                     and `{shown}` is an interface: two elements can be \
                                     different types, and one's `cmp` would be handed the \
                                     other. Sort a list of the concrete type instead."
                                ),
                            ));
                        }
                        if self.reserved_method(tid, "cmp").is_none() {
                            return Err(Diag::new(
                                span,
                                format!(
                                    "`sort` orders `int`, `float` and `str`, and any type \
                                     that declares `int {shown}.cmp({shown} other)` -- the \
                                     same method `<` uses; {}",
                                    self.reserved_missing(tid, &["cmp"])
                                ),
                            ));
                        }
                        // Found by name, so it obeys privacy like every
                        // other by-name lookup: `print(v)` needs a visible
                        // `to_str`, `a < b` a visible `cmp`, and so does
                        // this, which is the same call written by the
                        // runtime instead of by the program.
                        self.check_method_access(tid, "cmp", span)?;
                        "rt_sort_obj"
                    }
                };
                self.push(Inst::Call {
                    dst: None,
                    func: f.to_string(),
                    args: vec![o.val()],
                });
                Ok(Val::void())
            }
            // A fresh collection of the same kind holding `from .. to`.
            //
            // `str` and `bytes` already answer this question, as `substr`,
            // and the contract here is theirs: half-open, so `slice(i, i)`
            // is empty and `to - from` is the size, and out of bounds
            // **traps** rather than clamping -- a range the program computed
            // wrong is a bug, and a quietly shortened answer hides it in
            // whatever computed the range. The name differs because a list
            // holds no text; the shape of the call is what has to match.
            //
            // On an `Array` as well as a `List`, because they already share
            // `size`, `contains`, `index_of`, `reverse` and `sort` and "one
            // name for each question" (reference §3.9) is the rule. Each
            // answers with its own kind.
            "slice" => {
                if args.pos.len() != 2 {
                    return Err(Diag::new(span, "`slice` takes a start and an end"));
                }
                let from = self.lower_expr(&args.pos[0])?;
                if self.underlying(from.ty) != Ty::Int {
                    return Err(Diag::new(
                        args.pos[0].span(),
                        self.mismatch(Ty::Int, from.ty),
                    ));
                }
                let to = self.lower_expr(&args.pos[1])?;
                if self.underlying(to.ty) != Ty::Int {
                    return Err(Diag::new(args.pos[1].span(), self.mismatch(Ty::Int, to.ty)));
                }
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_seq_slice".to_string(),
                    args: vec![o.val(), from.val(), to.val()],
                });
                // A fresh object, so the caller holds the +1, and the
                // receiver's own type -- a distinct one included, as `clone`
                // does.
                self.stmt_temps.push(d);
                Ok(Val::new(d, o.ty, true))
            }
            "reverse" => {
                if !args.pos.is_empty() {
                    return Err(Diag::new(span, "`reverse` takes no arguments"));
                }
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_seq_reverse".to_string(),
                    args: vec![o.val()],
                });
                Ok(Val::void())
            }
            "index_of" => {
                if args.pos.len() != 1 {
                    return Err(Diag::new(span, "`index_of` takes one argument"));
                }
                let v = self.lower_expr(&args.pos[0])?;
                if !self.assignable(v.ty, elem) {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(elem, v.ty)));
                }
                let u = self.underlying(elem);
                if u.is_ref() && u != Ty::Str {
                    return Err(Diag::new(
                        span,
                        format!(
                            "`index_of` compares `int`, `float`, `bool` and \
                             `str`; {} would need its own comparison",
                            self.tyname(elem)
                        ),
                    ));
                }
                let Some((oty, otid, none_tag, some_tag)) = self.option_of(Ty::Int) else {
                    return Err(Diag::new(
                        span,
                        "`index_of` has no Option type to return; this is a compiler bug",
                    ));
                };
                let kind = self.new_val(IrTy::I64);
                self.push(Inst::IConst {
                    dst: kind,
                    val: match u {
                        Ty::Str => 1,
                        Ty::Float => 2,
                        _ => 0,
                    },
                });
                let raw = self.new_val(IrTy::I64);
                self.push(Inst::Call {
                    dst: Some(raw),
                    func: "rt_seq_index_of".to_string(),
                    args: vec![o.val(), v.val(), kind],
                });
                // The runtime's -1 never reaches the language: it becomes a
                // None here, which is why index_of waited for Option instead
                // of shipping a sentinel into a language meant to be frozen.
                let zero = self.new_val(IrTy::I64);
                self.push(Inst::IConst { dst: zero, val: 0 });
                let found = self.new_val(IrTy::I1);
                self.push(Inst::ICmp {
                    dst: found,
                    cmp: Cmp::Ge,
                    lhs: raw,
                    rhs: zero,
                });

                let some_bb = self.new_block();
                let none_bb = self.new_block();
                let join_bb = self.new_block();
                self.terminate(Term::Brif {
                    cond: found,
                    then: some_bb,
                    then_args: Vec::new(),
                    els: none_bb,
                    els_args: Vec::new(),
                });

                self.switch_to(some_bb);
                let some = self.make_option(otid, some_tag, Some(raw));
                self.stmt_temps.retain(|t| *t != some);
                self.terminate(Term::Jump {
                    to: join_bb,
                    args: vec![some],
                });

                self.switch_to(none_bb);
                let nothing = self.make_option(otid, none_tag, None);
                self.stmt_temps.retain(|t| *t != nothing);
                self.terminate(Term::Jump {
                    to: join_bb,
                    args: vec![nothing],
                });

                self.switch_to(join_bb);
                let d = self.enum_val(otid);
                let ji = self.blocks.iter().position(|b| b.id == join_bb).unwrap();
                self.blocks[ji].params = vec![d];
                self.stmt_temps.push(d);
                Ok(Val::new(d, oty, true))
            }
            "contains" => {
                if args.pos.len() != 1 {
                    return Err(Diag::new(span, "`contains` takes one argument"));
                }
                let v = self.lower_expr(&args.pos[0])?;
                if !self.assignable(v.ty, elem) {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(elem, v.ty)));
                }
                // A user type would need its own `eq`, which this does not
                // reach -- say so rather than comparing addresses silently.
                let u = self.underlying(elem);
                if u.is_ref() && u != Ty::Str {
                    return Err(Diag::new(
                        span,
                        format!(
                            "`contains` compares `int`, `float`, `bool` and \
                             `str`; {} would need its own comparison",
                            self.tyname(elem)
                        ),
                    ));
                }
                // A slot is one machine word whatever it holds, so the
                // runtime is told what is in it.
                let kind = self.new_val(IrTy::I64);
                self.push(Inst::IConst {
                    dst: kind,
                    val: match u {
                        Ty::Str => 1,
                        Ty::Float => 2,
                        _ => 0,
                    },
                });
                let d = self.new_val(IrTy::I1);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_seq_contains".to_string(),
                    args: vec![o.val(), v.val(), kind],
                });
                Ok(Val::new(d, Ty::Bool, false))
            }
            "join" => {
                if args.pos.len() != 1 {
                    return Err(Diag::new(span, "`join` takes one argument"));
                }
                // The inverse of `split` on either: parts of bytes rejoin
                // into bytes, with a bytes separator.
                if self.underlying(elem) == Ty::Bytes {
                    let sep = self.arg_of(&args.pos[0], Ty::Bytes)?;
                    return Ok(self.rt_value("rt_bytes_join", vec![o.val(), sep], Ty::Bytes));
                }
                if self.underlying(elem) != Ty::Str {
                    return Err(Diag::new(
                        span,
                        format!(
                            "`join` needs a collection of `str` or `bytes`; this one holds {}",
                            self.tyname(elem)
                        ),
                    ));
                }
                let sep = self.lower_expr(&args.pos[0])?;
                if !self.assignable(sep.ty, Ty::Str) {
                    return Err(Diag::new(
                        args.pos[0].span(),
                        self.mismatch(Ty::Str, sep.ty),
                    ));
                }
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_str_join".to_string(),
                    args: vec![o.val(), sep.val()],
                });
                self.stmt_temps.push(d);
                Ok(Val::new(d, Ty::Str, true))
            }
            "push" | "pop" | "insert" | "remove_at" | "clear" => Err(Diag::new(
                span,
                format!("`{m}` needs a List; an Array has a fixed length"),
            )),
            "len" => Err(Diag::new(
                span,
                format!(
                    "`{}` has no method `len`; it is `size()`",
                    self.tyname(o.ty)
                ),
            )),
            "has" => Err(Diag::new(
                span,
                format!(
                    "`{}` has no method `has`; it is `contains()`",
                    self.tyname(o.ty)
                ),
            )),
            // Java has both remove(int) and remove(Object) and the overload
            // is a standing trap. One name, and it says which it means.
            "remove" => Err(Diag::new(
                span,
                format!(
                    "`{}` has no method `remove`; use `remove_at(i)` to drop \
                     the element at an index",
                    self.tyname(o.ty)
                ),
            )),
            other => Err(Diag::new(
                span,
                format!("`{}` has no method `{other}`", self.tyname(o.ty)),
            )),
        }
    }

    /// `o.is_some()`, `o.is_none()`, `o.or(default)`. Three, and no more.
    ///
    /// An Option that can only be opened with `match` turns every map read
    /// into four lines, which would make returning one a downgrade. An
    /// Option with a library of combinators is a second language to learn.
    /// These three carry their weight; `match` remains the only way to get
    /// at the payload and keep it.
    ///
    /// `is_none` is kept even though it is `!o.is_some()`. The single-`cmp`
    /// rule it appears to violate is about something else: four comparison
    /// methods could disagree with each other, because each is a separate
    /// implementation the author writes. `is_none` is generated from
    /// `is_some` by the compiler and cannot drift from it. What is left is
    /// only whether `if (!o.is_some())` reads as well as `if (o.is_none())`,
    /// and it does not.
    ///
    /// No `unwrap`. Trapping on None is what `get` used to do, and putting
    /// it back behind a shorter name would undo the point.
    pub(super) fn lower_option_method(
        &mut self,
        o: &Val,
        tid: u32,
        m: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if !args.named.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no named arguments")));
        }
        // `Option<void>` has no payload at all (mono.rs drops a void one):
        // it can answer `is_some`, and has nothing for `or` to hand back.
        let inner = self.variant_surface[tid as usize]
            .iter()
            .find_map(|p| p.first().copied());
        let Some(inner) = inner.or(if m == "or" { None } else { Some(Ty::Void) }) else {
            return Err(Diag::new(
                span,
                format!(
                    "`or` gives back the value inside, and {} carries none",
                    self.show_name(&self.typedefs[tid as usize].name)
                ),
            ));
        };
        let some_tag = self.typedefs[tid as usize]
            .variants
            .iter()
            .position(|v| v.name == "Some")
            .expect("Option has Some") as u32;

        let is_some = |lw: &mut Self| {
            let tag = lw.new_val(IrTy::I64);
            lw.push(Inst::EnumTag {
                dst: tag,
                obj: o.val(),
                tid,
            });
            let k = lw.new_val(IrTy::I64);
            lw.push(Inst::IConst {
                dst: k,
                val: some_tag as i64,
            });
            let c = lw.new_val(IrTy::I1);
            lw.push(Inst::ICmp {
                dst: c,
                cmp: Cmp::Eq,
                lhs: tag,
                rhs: k,
            });
            c
        };

        match m {
            "is_some" | "is_none" => {
                if !args.pos.is_empty() {
                    return Err(Diag::new(span, format!("`{m}` takes no arguments")));
                }
                let c = is_some(self);
                if m == "is_some" {
                    return Ok(Val::new(c, Ty::Bool, false));
                }
                // Generated from is_some, so the two cannot drift apart.
                let d = self.new_val(IrTy::I1);
                self.push(Inst::Not { dst: d, src: c });
                Ok(Val::new(d, Ty::Bool, false))
            }
            "or" => {
                if args.pos.len() != 1 {
                    return Err(Diag::new(span, "`or` takes one argument"));
                }
                let c = is_some(self);
                let some_bb = self.new_block();
                let else_bb = self.new_block();
                let join_bb = self.new_block();
                self.terminate(Term::Brif {
                    cond: c,
                    then: some_bb,
                    then_args: Vec::new(),
                    els: else_bb,
                    els_args: Vec::new(),
                });

                self.switch_to(some_bb);
                let got = self.new_val(self.irty(inner));
                self.push(Inst::EnumPayload {
                    dst: got,
                    obj: o.val(),
                    tid,
                    tag: some_tag,
                    idx: 0,
                });
                // Borrowed from the Option, like any payload -- so it is
                // retained here and released by the statement, which is what
                // makes both arms agree about who owns the result.
                if self.is_ref(inner) {
                    self.rc_inc(got);
                }
                self.terminate(Term::Jump {
                    to: join_bb,
                    args: vec![got],
                });

                self.switch_to(else_bb);
                // The fallback is LAZY -- it is lowered into its own block
                // and the `Some` path never runs it. So anything it
                // allocates has to be released HERE. Left to the statement's
                // flush, the release lands in the join block, which the
                // `Some` edge reaches without ever having evaluated the
                // fallback: it read an uninitialised pointer and segfaulted,
                // or freed a stale one twice on a later pass. Exactly the
                // hazard `&&` and `||` already have `flush_temps_since` for.
                let mark = self.stmt_temps.len();
                let d = self.lower_expr_as(&args.pos[0], inner)?;
                if !self.assignable(d.ty, inner) {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(inner, d.ty)));
                }
                if self.is_ref(inner) {
                    if d.owned {
                        self.stmt_temps.retain(|t| *t != d.val());
                    } else {
                        self.rc_inc(d.val());
                    }
                }
                self.flush_temps_since(mark);
                self.terminate(Term::Jump {
                    to: join_bb,
                    args: vec![d.val()],
                });

                self.switch_to(join_bb);
                let out = self.new_val(self.irty(inner));
                let ji = self.blocks.iter().position(|b| b.id == join_bb).unwrap();
                self.blocks[ji].params = vec![out];
                let owned = self.is_ref(inner);
                if owned {
                    self.stmt_temps.push(out);
                }
                Ok(Val::new(out, inner, owned))
            }
            other => Err(Diag::new(
                span,
                format!(
                    "an Option has `is_some`, `is_none` and `or`; `{other}` is \
                     not one of them -- use `match` to take the value out"
                ),
            )),
        }
    }

    /// `r.is_ok()`, `r.is_err()`. Two, for symmetry with Option's pair: a
    /// caller that only needs to know whether something failed -- counting
    /// failures, an `if` that decides what to try next -- should not have to
    /// write a four-line `match` that binds a payload only to ignore it.
    /// `?` propagates the failure; `is_ok` asks without taking anything
    /// apart. `is_err` is generated from `is_ok`, for the reason `is_none`
    /// is generated from `is_some`.
    ///
    /// No `or`. An Option's None carries nothing, so falling back loses
    /// nothing; a Result's Err carries the reason, and a one-call way to
    /// throw it away would make discarding it easy, which the discarded-
    /// Result error exists to prevent.
    pub(super) fn lower_result_method(
        &mut self,
        r: &Val,
        tid: u32,
        m: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if !matches!(m, "is_ok" | "is_err") {
            return Err(Diag::new(
                span,
                format!(
                    "a Result has `is_ok` and `is_err`; `{m}` is not one of \
                     them -- use `match` to take the value out, or `?` to \
                     pass the failure on"
                ),
            ));
        }
        if !args.named.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no named arguments")));
        }
        if !args.pos.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no arguments")));
        }
        let ok_tag = self.typedefs[tid as usize]
            .variants
            .iter()
            .position(|v| v.name == "Ok")
            .expect("Result has Ok") as u32;
        let tag = self.new_val(IrTy::I64);
        self.push(Inst::EnumTag {
            dst: tag,
            obj: r.val(),
            tid,
        });
        let k = self.new_val(IrTy::I64);
        self.push(Inst::IConst {
            dst: k,
            val: ok_tag as i64,
        });
        let c = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: c,
            cmp: Cmp::Eq,
            lhs: tag,
            rhs: k,
        });
        if m == "is_ok" {
            return Ok(Val::new(c, Ty::Bool, false));
        }
        // Generated from is_ok, so the two cannot drift apart.
        let d = self.new_val(IrTy::I1);
        self.push(Inst::Not { dst: d, src: c });
        Ok(Val::new(d, Ty::Bool, false))
    }

    /// The `Option<T>` type for a given payload type, and the tags of its
    /// two variants. Monomorphisation instantiates one wherever a built-in
    /// method needs it, so this cannot fail for those.
    fn option_of(&mut self, inner: Ty) -> Option<(Ty, u32, u32, u32)> {
        let name = self
            .typedefs
            .iter()
            .enumerate()
            .find(|(i, d)| {
                d.is_enum
                    && d.name.starts_with("Option$")
                    && self.variant_surface[*i]
                        .iter()
                        .any(|p| p.first() == Some(&inner))
            })
            .map(|(_, d)| d.name.clone())?;
        let ty = self.ty_named(&name)?;
        let tid = self.tdef_of(ty)?;
        let none = self.typedefs[tid as usize]
            .variants
            .iter()
            .position(|v| v.name == "None")? as u32;
        let some = self.typedefs[tid as usize]
            .variants
            .iter()
            .position(|v| v.name == "Some")? as u32;
        Some((ty, tid, none, some))
    }

    /// `Some(v)` when `cond`, `None` otherwise -- as one value, through a
    /// join block.
    fn select_option(
        &mut self,
        cond: Value,
        v: Value,
        tid: u32,
        some_tag: u32,
        none_tag: u32,
    ) -> Value {
        let some_bb = self.new_block();
        let none_bb = self.new_block();
        let join_bb = self.new_block();
        self.terminate(Term::Brif {
            cond,
            then: some_bb,
            then_args: Vec::new(),
            els: none_bb,
            els_args: Vec::new(),
        });

        self.switch_to(some_bb);
        let some = self.make_option(tid, some_tag, Some(v));
        self.stmt_temps.retain(|t| *t != some);
        self.terminate(Term::Jump {
            to: join_bb,
            args: vec![some],
        });

        self.switch_to(none_bb);
        let nothing = self.make_option(tid, none_tag, None);
        self.stmt_temps.retain(|t| *t != nothing);
        self.terminate(Term::Jump {
            to: join_bb,
            args: vec![nothing],
        });

        self.switch_to(join_bb);
        let d = self.enum_val(tid);
        let ji = self.blocks.iter().position(|b| b.id == join_bb).unwrap();
        self.blocks[ji].params = vec![d];
        self.stmt_temps.push(d);
        d
    }

    /// Wrap a runtime index into an `Option<int>`: negative is `None`.
    ///
    /// The sentinel never reaches the language -- it is turned into a None
    /// here, which is the whole reason `index_of` waited for Option instead
    /// of shipping a -1 into something meant to be frozen.
    fn wrap_option(&mut self, raw: Value, tid: u32, some_tag: u32, none_tag: u32) -> Value {
        let zero = self.new_val(IrTy::I64);
        self.push(Inst::IConst { dst: zero, val: 0 });
        let found = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: found,
            cmp: Cmp::Ge,
            lhs: raw,
            rhs: zero,
        });

        self.select_option(found, raw, tid, some_tag, none_tag)
    }

    /// Build `Some(v)` or `None` of the given Option type.
    pub(super) fn make_option(&mut self, tid: u32, tag: u32, payload: Option<Value>) -> Value {
        let d = self.enum_val(tid);
        self.push(Inst::EnumPack {
            dst: d,
            tid,
            tag,
            args: payload.into_iter().collect(),
        });
        self.stmt_temps.push(d);
        d
    }

    /// `m.set(k, v)`, `m.get(k)`, `m.contains(k)`, `m.remove(k)`, `m.size()`.
    ///
    /// `get` on a missing key traps, like an out-of-range index: there is no
    /// null to return, so the honest choices are to trap or to force every
    /// read through a check. `has` is the check.
    pub(super) fn lower_map_method(
        &mut self,
        o: &Val,
        k: Ty,
        v: Ty,
        m: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if !args.named.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no named arguments")));
        }
        let arity = match m {
            "size" | "keys" | "values" | "clear" => 0,
            "set" => 2,
            "get" | "contains" | "remove" => 1,
            "len" => return Err(Diag::new(span, "a map has no method `len`; it is `size()`")),
            "has" => {
                return Err(Diag::new(
                    span,
                    "a map has no method `has`; it is `contains()`, which asks about a key",
                ))
            }
            other => return Err(Diag::new(span, format!("a map has no method `{other}`"))),
        };
        if args.pos.len() != arity {
            return Err(Diag::new(
                span,
                format!("`{m}` takes {arity} argument(s), found {}", args.pos.len()),
            ));
        }
        if m == "size" {
            let d = self.new_val(IrTy::I64);
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_map_len".to_string(),
                args: vec![o.val()],
            });
            return Ok(Val::new(d, Ty::Int, false));
        }

        if m == "clear" {
            self.push(Inst::Call {
                dst: None,
                func: "rt_map_clear".to_string(),
                args: vec![o.val()],
            });
            return Ok(Val::void());
        }

        // A fresh List, so the caller owns it (§5.2) and it holds its own
        // reference to every element.
        if m == "keys" || m == "values" {
            let elem = if m == "keys" { k } else { v };
            let Some(lty) = self.list_of(elem) else {
                return Err(Diag::new(
                    span,
                    format!("`{m}` has no List type to return; this is a compiler bug"),
                ));
            };
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Call {
                dst: Some(d),
                func: if m == "keys" {
                    "rt_map_keys".to_string()
                } else {
                    "rt_map_values".to_string()
                },
                args: vec![o.val()],
            });
            self.stmt_temps.push(d);
            return Ok(Val::new(d, lty, true));
        }

        let key = self.lower_expr(&args.pos[0])?;
        if !self.assignable(key.ty, k) {
            return Err(Diag::new(args.pos[0].span(), self.mismatch(k, key.ty)));
        }
        // `set` takes a value after the key; the key must outlive it.
        let later = m == "set" && self.may_run_code(&args.pos[1], true);
        let key = self.hold(&args.pos[0], key, later);
        match m {
            "set" => {
                let val = self.lower_expr_as(&args.pos[1], v)?;
                if !self.assignable(val.ty, v) {
                    return Err(Diag::new(args.pos[1].span(), self.mismatch(v, val.ty)));
                }
                // The map retains the key and the value itself, so an owned
                // temporary here is still released by the statement.
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_map_set".to_string(),
                    args: vec![o.val(), key.val(), val.val()],
                });
                Ok(Val::void())
            }
            "get" => {
                // Returns `Option<V>`, not the value.
                //
                // It used to trap on a missing key, because with no way to
                // express absence the honest choices were to trap or to make
                // every read go through a check. Absence is expressible now,
                // and an Option cannot be forgotten the way a preceding
                // `contains` can -- nor does it hash the key twice.
                let Some((oty, otid, none_tag, some_tag)) = self.option_of(v) else {
                    return Err(Diag::new(
                        span,
                        "`get` has no Option type to return; this is a compiler bug",
                    ));
                };
                let has = self.new_val(IrTy::I1);
                self.push(Inst::Call {
                    dst: Some(has),
                    func: "rt_map_has".to_string(),
                    args: vec![o.val(), key.val()],
                });

                let some_bb = self.new_block();
                let none_bb = self.new_block();
                let join_bb = self.new_block();
                self.terminate(Term::Brif {
                    cond: has,
                    then: some_bb,
                    then_args: Vec::new(),
                    els: none_bb,
                    els_args: Vec::new(),
                });

                self.switch_to(some_bb);
                let raw = self.new_val(self.irty(v));
                self.push(Inst::Call {
                    dst: Some(raw),
                    func: "rt_map_get".to_string(),
                    args: vec![o.val(), key.val()],
                });
                // The map keeps its reference; the Option takes one of its
                // own, exactly as any container would.
                if self.is_ref(v) {
                    self.rc_inc(raw);
                }
                let some = self.make_option(otid, some_tag, Some(raw));
                self.stmt_temps.retain(|t| *t != some);
                self.terminate(Term::Jump {
                    to: join_bb,
                    args: vec![some],
                });

                self.switch_to(none_bb);
                let nothing = self.make_option(otid, none_tag, None);
                self.stmt_temps.retain(|t| *t != nothing);
                self.terminate(Term::Jump {
                    to: join_bb,
                    args: vec![nothing],
                });

                self.switch_to(join_bb);
                let d = self.enum_val(otid);
                let ji = self.blocks.iter().position(|b| b.id == join_bb).unwrap();
                self.blocks[ji].params = vec![d];
                self.stmt_temps.push(d);
                Ok(Val::new(d, oty, true))
            }
            "contains" => {
                let d = self.new_val(IrTy::I1);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_map_has".to_string(),
                    args: vec![o.val(), key.val()],
                });
                Ok(Val::new(d, Ty::Bool, false))
            }
            _ => {
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_map_remove".to_string(),
                    args: vec![o.val(), key.val()],
                });
                Ok(Val::void())
            }
        }
    }

    /// `send(c, v)`, `recv(c)`, `close(c)`.
    ///
    /// `send` MOVES its value: the sender gives up its reference and the
    /// receiver acquires it, with no retain or release in between. That is
    /// the rule that keeps rc_inc/rc_dec non-atomic -- only one thread can
    /// reach the value at a time -- and it is enforced by the move checker,
    /// which refuses any later use of a moved local.
    pub(super) fn lower_chan_builtin(
        &mut self,
        name: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if !args.named.is_empty() {
            return Err(Diag::new(
                span,
                format!("`{name}` takes no named arguments"),
            ));
        }
        let want = if name == "send" { 2 } else { 1 };
        if args.pos.len() != want {
            return Err(Diag::new(
                span,
                format!(
                    "`{name}` takes {want} argument(s), found {}",
                    args.pos.len()
                ),
            ));
        }
        let c = self.lower_expr(&args.pos[0])?;
        let Some(elem) = self.chan_elem(c.ty) else {
            return Err(Diag::new(
                args.pos[0].span(),
                format!("`{name}` needs a channel, found {}", self.tyname(c.ty)),
            ));
        };

        match name {
            "close" => {
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_chan_close".to_string(),
                    args: vec![c.val()],
                });
                Ok(Val::void())
            }
            "send" => {
                let v = self.lower_expr_as(&args.pos[1], elem)?;
                if !self.assignable(v.ty, elem) {
                    return Err(Diag::new(args.pos[1].span(), self.mismatch(elem, v.ty)));
                }
                self.transfer(&v, &args.pos[1], span)?;
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_chan_send".to_string(),
                    args: vec![c.val(), v.val()],
                });
                Ok(Val::void())
            }
            _ => {
                let d = self.new_val(self.irty(elem));
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_chan_recv".to_string(),
                    args: vec![c.val()],
                });
                // The receiver acquires the sender's reference: owned, with
                // no retain, because the send gave one up.
                let owned = self.is_ref(elem);
                if owned {
                    self.stmt_temps.push(d);
                }
                Ok(Val::new(d, elem, owned))
            }
        }
    }

    /// A method on `int`, `float` or `bool`.
    ///
    /// `to_str` on all three, so that `v.to_str()` means the same thing
    /// whatever `v` is. Beyond that only what cannot be written in the
    /// language itself: wrapping arithmetic, which the checked operators
    /// refuse by design, and a float's bit pattern, which no arithmetic
    /// reaches. Everything else a number might answer belongs in a library.
    pub(super) fn lower_prim_method(
        &mut self,
        o: &Val,
        m: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        let prim = self.underlying(o.ty);
        let wrap = match m {
            "wrapping_add" => Some(ArithOp::WrapAdd),
            "wrapping_sub" => Some(ArithOp::WrapSub),
            "wrapping_mul" => Some(ArithOp::WrapMul),
            _ => None,
        };
        if let (Ty::Int, Some(wop)) = (prim, wrap) {
            // A distinct int keeps its type, as it does under `+`: the
            // wrapping forms are the same arithmetic with a different answer
            // at the edges, not a conversion.
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(span, format!("`{m}` takes one argument")));
            }
            let b = self.lower_expr(&args.pos[0])?;
            if b.ty != o.ty {
                return Err(Diag::new(args.pos[0].span(), self.mismatch(o.ty, b.ty)));
            }
            let d = self.new_val(IrTy::I64);
            self.push(Inst::Arith {
                dst: d,
                op: wop,
                lhs: o.val(),
                rhs: b.val(),
            });
            return Ok(Val::new(d, o.ty, false));
        }
        if prim == Ty::Float && m == "to_bits" {
            if !args.pos.is_empty() || !args.named.is_empty() {
                return Err(Diag::new(span, "`to_bits` takes no arguments"));
            }
            // The runtime already moves floats through int64 slots by bit
            // pattern (rt_f2i, a memcpy); this is that move, made visible.
            let d = self.new_val(IrTy::I64);
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_f2i".to_string(),
                args: vec![o.val()],
            });
            return Ok(Val::new(d, Ty::Int, false));
        }
        if m != "to_str" {
            return Err(Diag::new(
                span,
                format!("`{}` has no method `{m}`", self.tyname(o.ty)),
            ));
        }
        if !args.pos.is_empty() || !args.named.is_empty() {
            return Err(Diag::new(span, "`to_str` takes no arguments"));
        }
        let d = self.prim_to_str(prim, o.val(), span)?;
        Ok(Val::new(d, Ty::Str, true))
    }

    /// The text of an `int`, a `float` or a `bool`, owned by the statement.
    /// `v.to_str()` and `str(v)` both come here, so they cannot disagree.
    pub(super) fn prim_to_str(&mut self, prim: Ty, v: Value, span: Span) -> Result<Value, Diag> {
        if prim == Ty::Float {
            return self.float_text("format", Ty::Str, v, span);
        }
        let func = if prim == Ty::Int {
            "rt_int_to_str"
        } else {
            "rt_bool_to_str"
        };
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Call {
            dst: Some(d),
            func: func.to_string(),
            args: vec![v],
        });
        self.stmt_temps.push(d);
        Ok(d)
    }

    /// A call to `format` or `parse` in lib/__floatfmt.src, which is how a
    /// float becomes text and text a float: the conversion is language
    /// source, not runtime C. Both take one borrowed argument and return
    /// what `ret` says -- a `str` for `format`, an `Option<float>` for
    /// `parse`. The second of those is a value enum, so the destination's
    /// shape has to come from the type rather than be assumed to be a
    /// reference.
    ///
    /// The loader includes the module whenever a program could get here (see
    /// `modules::mentions_float`); if that ever misjudges, this says so
    /// instead of leaving an undefined function to the C compiler.
    pub(super) fn float_text(
        &mut self,
        name: &str,
        ret: Ty,
        arg: Value,
        span: Span,
    ) -> Result<Value, Diag> {
        let fm = crate::stdlib::FLOATFMT;
        // The module converting floats cannot convert one itself: the call
        // would be to itself, and it would recurse until the stack ran out.
        if self.cur_module == fm {
            return Err(Diag::new(
                span,
                "the float formatter cannot format or parse a float itself: \
                 that would call itself",
            ));
        }
        let key = format!("{fm}#{name}");
        if !self.sigs.contains_key(&key) {
            return Err(Diag::new(
                span,
                format!("`{key}` was not loaded for a float conversion; this is a compiler bug"),
            ));
        }
        let d = self.new_val(self.irty(ret));
        self.push(Inst::Call {
            dst: Some(d),
            func: key,
            args: vec![arg],
        });
        if self.types[d.0 as usize].is_managed() {
            self.stmt_temps.push(d);
        }
        Ok(d)
    }

    /// A call to `chars` or `from_chars` in lib/__text.src, which is how a
    /// `str` becomes code points and code points a `str`: UTF-8 is bit
    /// manipulation the language can write, so it is source, not runtime C.
    /// One borrowed argument, an owned reference back -- the shape of
    /// `float_text`, and guarded the same two ways.
    fn text_call(&mut self, name: &str, arg: Value, span: Span) -> Result<Value, Diag> {
        let tm = crate::stdlib::TEXT;
        // The module cannot use what it defines through the method spelling:
        // the call would be to itself.
        if self.cur_module == tm {
            return Err(Diag::new(
                span,
                "the text module cannot call `chars` or `from_chars` itself: \
                 that would call itself",
            ));
        }
        let key = format!("{tm}#{name}");
        if !self.sigs.contains_key(&key) {
            return Err(Diag::new(
                span,
                format!(
                    "`{key}` was not loaded for a code point conversion; this is a compiler bug"
                ),
            ));
        }
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Call {
            dst: Some(d),
            func: key,
            args: vec![arg],
        });
        self.stmt_temps.push(d);
        Ok(d)
    }

    /// `T.name(..)` where `T` is a built-in type: a static method.
    ///
    /// There are two, `float.from_bits(n)` and `str.from_chars(xs)`. They are
    /// static rather than methods on the source for the reason §6.6 gives for
    /// parsing: the source is always an `int` (or a list of them) and it is
    /// the TARGET that the name has to say, which a method dispatched on the
    /// source cannot.
    pub(super) fn lower_prim_static(
        &mut self,
        ty: Ty,
        name: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if ty == Ty::Str && name == "from_chars" {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(span, "`from_chars` takes one argument"));
            }
            let Some(lty) = self.list_of(Ty::Int) else {
                return Err(Diag::new(
                    span,
                    "`from_chars` has no List<int> type to take; this is a compiler bug",
                ));
            };
            let xs = self.lower_expr_as(&args.pos[0], lty)?;
            if !self.assignable(xs.ty, lty) {
                return Err(Diag::new(args.pos[0].span(), self.mismatch(lty, xs.ty)));
            }
            let d = self.text_call("from_chars", xs.val(), span)?;
            return Ok(Val::new(d, Ty::Str, true));
        }
        // Rust's `char::from_u32`, Python's `chr`: the spelling a reader will
        // reach for. One code point is a list of one; one spelling, not two.
        if ty == Ty::Str && (name == "from_char" || name == "chr") {
            return Err(Diag::new(
                span,
                format!(
                    "`str` has no static method `{name}`; one code point \
                     is `str.from_chars([c])`"
                ),
            ));
        }
        if ty != Ty::Float || name != "from_bits" {
            return Err(Diag::new(
                span,
                format!("`{}` has no static method `{name}`", self.tyname(ty)),
            ));
        }
        if args.pos.len() != 1 || !args.named.is_empty() {
            return Err(Diag::new(span, "`from_bits` takes one argument"));
        }
        let n = self.lower_expr(&args.pos[0])?;
        if n.ty != Ty::Int {
            return Err(Diag::new(args.pos[0].span(), self.mismatch(Ty::Int, n.ty)));
        }
        let d = self.new_val(IrTy::F64);
        self.push(Inst::Call {
            dst: Some(d),
            func: "rt_i2f".to_string(),
            args: vec![n.val()],
        });
        Ok(Val::new(d, Ty::Float, false))
    }
}
