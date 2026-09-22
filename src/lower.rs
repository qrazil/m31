//! Typecheck and lower the AST to IR, in one pass.
//!
//! These are deliberately fused. A separate checker would have to hand its
//! results to the lowerer through a side table keyed by expression identity,
//! and the two traversals would then be free to disagree about types -- a
//! divergence that is invisible until it miscompiles. One pass cannot
//! disagree with itself. The cost is that only the first error is reported,
//! which is acceptable while `corpus/errors/` compares one diagnostic per
//! program.
//!
//! Refcount insertion happens here too, producing the post-refcount IR that
//! docs/ir-v0.md §4 says backends consume. The ownership protocol is §5:
//! arguments are borrowed, returns are owned (+1).

use std::collections::HashMap;

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{self, ArithOp, Block, BlockId, Cmp, Inst, IrTy, Term, TypeDef, Value};

/// A local binding: its type, its current SSA value, and whether it was
/// declared `const`.
type Binding = (Ty, Value, bool);

/// A lowered expression, plus whether we are holding a +1 on it that somebody
/// must release. Literals are immortal and variables are borrowed from their
/// local, so only a call result arrives owned.
struct Val {
    /// `None` for a void expression. A void call has no value, and inventing
    /// a dummy one put a dead `bconst` in every emitted function.
    v: Option<Value>,
    ty: Ty,
    owned: bool,
}

impl Val {
    fn new(v: Value, ty: Ty, owned: bool) -> Self {
        Val {
            v: Some(v),
            ty,
            owned,
        }
    }

    fn void() -> Self {
        Val {
            v: None,
            ty: Ty::Void,
            owned: false,
        }
    }

    /// The value. Every caller reaches this only after a type check that
    /// excludes `void`, so a `None` here is a compiler bug, not user error.
    fn val(&self) -> Value {
        self.v.expect("void expression used as a value")
    }
}

struct Sig {
    params: Vec<Param>,
    ret: Ty,
    /// Which module declared this, and whether it left the module. Builtins
    /// carry an empty module and are visible everywhere.
    module: String,
    is_pub: bool,
    /// The seam to C: no body to lower, and the call goes to a runtime
    /// symbol found by one name transform. See docs/stdlib-seam.md.
    is_prim: bool,
}

/// One entry per enclosing `while`, so `break` and `continue` know where to
/// jump and which values to carry.
struct LoopCtx {
    header: BlockId,
    exit: BlockId,
    /// Loop-carried variable names, in the same order as the header's and
    /// exit's block parameters.
    carried: Vec<String>,
    /// Scope depth at the top of the loop body. `break`/`continue` must
    /// release every scope inside this one before jumping.
    depth: usize,
}

struct BlockBuf {
    id: BlockId,
    params: Vec<Value>,
    insts: Vec<Inst>,
    term: Option<Term>,
}

pub struct Lowerer {
    sigs: HashMap<String, Sig>,
    /// User-defined types, indexed by `Ty::User` id. Distinct from `types`
    /// below, which is the per-function map from Value to IrTy.
    typedefs: Vec<TypeDef>,
    /// Surface types of each type's fields, parallel to `typedefs`. The IR
    /// only records `Ref`, which cannot distinguish `str` from a user type.
    field_surface: Vec<Vec<Ty>>,
    /// Field declarations, parallel to `typedefs`, so construction can bind
    /// arguments by the same rule as a call.
    field_params: Vec<Vec<Param>>,
    /// Base type of each distinct type, parallel to `typedefs`.
    distinct_base: Vec<Option<Ty>>,
    /// Module and export flag per type, parallel to `typedefs`. Privacy has
    /// to survive the merge into one program, so it travels with the
    /// declaration rather than with the file.
    type_module: Vec<String>,
    type_pub: Vec<bool>,
    /// Surface payload types per variant, parallel to `typedefs`. The IR
    /// records only `Ref`, which cannot tell `str` from a user type, and a
    /// match arm has to bind the payload at its real type.
    variant_surface: Vec<Vec<Vec<Ty>>>,
    /// Every module in the program, so `greet.hello(..)` can be told from a
    /// field access on a variable called `greet`.
    modules: std::collections::HashSet<String>,
    /// What each module imports, so a local can be refused the name of a
    /// module its own file imports. See `Program::imports_by_module`.
    imports_by_module: HashMap<String, Vec<String>>,
    /// The module whose function is being lowered, for privacy checks.
    cur_module: String,
    /// Set while a collection literal is being built. The old constructor
    /// spelling is refused, and the literal lowering reaches the same
    /// allocation path through it.
    building_literal: bool,
    /// Keys of methods declared `static`. They are called on the type and
    /// take no receiver, so a call site has to know which kind it has.
    statics: std::collections::HashSet<String>,
    /// The monomorphised program's interned type expressions.
    ty_exprs: Vec<TyExpr>,
    /// Source spellings of instantiations, for diagnostics (`Program::shown`).
    shown: HashMap<String, (String, Vec<Ty>)>,
    /// Generic methods, by concrete receiver (`Program::generic_methods`).
    generic_methods: std::collections::HashSet<String>,
    /// Required methods per interface, parallel to `typedefs`; empty for a
    /// struct.
    iface_methods: Vec<Vec<Func>>,
    /// Interface method names, one per dispatch slot, assigned once for the
    /// whole program so a vtable index is a constant at every call site.
    iface_slots: Vec<ir::Slot>,
    strings: Vec<String>,
    // per-function state
    types: Vec<IrTy>,
    blocks: Vec<BlockBuf>,
    cur: usize,
    scopes: Vec<HashMap<String, Binding>>,
    /// Names, innermost scope last, whose locals hold a +1 to release on exit.
    owned: Vec<Vec<String>>,
    /// Owned temporaries produced while lowering the current statement.
    stmt_temps: Vec<Value>,
    loops: Vec<LoopCtx>,
    /// Counter for synthetic names, so nested loops do not collide.
    synth: u32,
    /// Locals that have been moved out of. Any later use is refused.
    ///
    /// One bit per local, as docs/types.md §4a describes: this is the whole
    /// of the ownership discipline, and it applies only at the boundary
    /// where a value leaves the thread.
    moved: Vec<String>,
    /// Inside a method: the receiver's type id and its SSA value. Fields are
    /// reached by bare name, which is safe only because nothing shadows
    /// anything -- see `check_shadow`.
    recv: Option<(u32, Value)>,
    ret_ty: Ty,
}

/// Every builtin function, whether it lives in `sigs` (`concat`) or is
/// special-cased in the call path because its type depends on its argument
/// (`print`, `clone`, the channel operations).
///
/// A bare call to one of these always means the builtin, in every module.
/// Nothing may take the name, because that would be shadowing (§4.1): a
/// module-level `print` used to replace the builtin silently in its own
/// module, and to turn every `print` in an importing file into a privacy
/// error about a function that file never asked for.
const BUILTIN_FNS: &[&str] = &["print", "concat", "clone", "send", "recv", "close"];

/// The representation of a surface type, WITHOUT resolving distinct types.
/// Use `Lowerer::irty` instead wherever a distinct type can appear.
fn ir_ty(t: Ty) -> IrTy {
    match t {
        Ty::Int => IrTy::I64,
        Ty::Float => IrTy::F64,
        Ty::Bool => IrTy::I1,
        Ty::Str | Ty::Bytes => IrTy::Ref,
        Ty::User(_) => IrTy::Ref,
        Ty::Void => unreachable!("void is not a value type"),
    }
}

impl Lowerer {
    pub fn new() -> Self {
        Lowerer {
            sigs: HashMap::new(),
            typedefs: Vec::new(),
            field_surface: Vec::new(),
            field_params: Vec::new(),
            distinct_base: Vec::new(),
            type_module: Vec::new(),
            type_pub: Vec::new(),
            variant_surface: Vec::new(),
            modules: std::collections::HashSet::new(),
            imports_by_module: HashMap::new(),
            cur_module: String::new(),
            building_literal: false,
            statics: std::collections::HashSet::new(),
            ty_exprs: Vec::new(),
            shown: HashMap::new(),
            generic_methods: std::collections::HashSet::new(),
            iface_methods: Vec::new(),
            iface_slots: Vec::new(),
            strings: Vec::new(),
            types: Vec::new(),
            blocks: Vec::new(),
            cur: 0,
            scopes: Vec::new(),
            owned: Vec::new(),
            stmt_temps: Vec::new(),
            loops: Vec::new(),
            synth: 0,
            moved: Vec::new(),
            recv: None,
            ret_ty: Ty::Void,
        }
    }

    fn builtin(&mut self, name: &str, params: Vec<Ty>, ret: Ty) {
        let params = params
            .into_iter()
            .enumerate()
            .map(|(i, ty)| Param {
                ty,
                name: format!("a{i}"),
                default: None,
                embedded: false,
                span: Span::new(0, 0),
            })
            .collect();
        self.sigs.insert(
            name.to_string(),
            Sig {
                params,
                ret,
                module: String::new(),
                is_pub: true,
                is_prim: false,
            },
        );
    }

    /// `s.size()`. The only method a `str` has for now; the string library
    /// will land here rather than as free functions.
    fn lower_str_method(&mut self, o: &Val, m: &str, args: &Args, span: Span) -> Result<Val, Diag> {
        if !args.named.is_empty() {
            return Err(Diag::new(span, format!("`{m}` takes no named arguments")));
        }
        // How many positional arguments each takes, and of what.
        let want: &[Ty] = match m {
            "size" | "trim" | "to_upper" | "to_lower" | "parse_int" | "parse_float"
            | "to_bytes" => &[],
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
            "parse_int" | "parse_float" => {
                let inner = if m == "parse_int" { Ty::Int } else { Ty::Float };
                let Some((oty, otid, none_tag, some_tag)) = self.option_of(inner) else {
                    return Err(Diag::new(
                        span,
                        format!("`{m}` has no Option type to return; this is a compiler bug"),
                    ));
                };
                let raw = self.new_val(self.irty(inner));
                let ok = self.new_val(IrTy::I1);
                self.push(Inst::ParseInto {
                    ok,
                    dst: raw,
                    func: if m == "parse_int" {
                        "rt_str_parse_int".to_string()
                    } else {
                        "rt_str_parse_float".to_string()
                    },
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
            "to_str" => {
                // A `str` already is one. Returning it unchanged keeps
                // `v.to_str()` writable whatever `v` is, which is what makes
                // to_str a rule rather than a special case.
                Ok(Val::new(o.val(), Ty::Str, false))
            }
            "len" => Err(Diag::new(span, "`str` has no method `len`; it is `size()`")),
            other => Err(Diag::new(
                span,
                format!(
                    "`str` has no method `{other}`; it has size, substr, contains, \
                     index_of, starts_with, ends_with, split, trim, to_upper, \
                     to_lower, repeat and to_bytes"
                ),
            )),
        }
    }

    /// A runtime call that produces a value. An owned reference result is
    /// put on the statement's pending list, as every producer does, so the
    /// caller only has to say what it made.
    fn rt_value(&mut self, func: &str, args: Vec<Value>, ty: Ty) -> Val {
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

    fn rt_void(&mut self, func: &str, args: Vec<Value>) -> Val {
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
    fn check_byte_literal(&self, e: &Expr) -> Result<(), Diag> {
        let n = match e {
            Expr::Int(n, _) => *n,
            Expr::Un(UnOp::Neg, inner, _) => match &**inner {
                Expr::Int(n, _) if *n != 0 => -*n,
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
    fn index_of_bytes(&mut self, idx: &Expr) -> Result<Value, Diag> {
        let i = self.lower_expr(idx)?;
        if self.underlying(i.ty) != Ty::Int {
            return Err(Diag::new(idx.span(), self.mismatch(Ty::Int, i.ty)));
        }
        Ok(i.val())
    }

    /// Lower one argument that must be of type `want`.
    fn arg_of(&mut self, e: &Expr, want: Ty) -> Result<Value, Diag> {
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
    fn lower_bytes_method(
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
            "push" | "repeat" => &[Ty::Int],
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
                         clear, extend, substr, contains, index_of, starts_with, \
                         ends_with, split, trim, to_upper, to_lower, repeat, hex \
                         and utf8"
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
    fn lower_seq_method(
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
                        self.push(Inst::RcInc { val: v.val() });
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
                        self.push(Inst::RcInc { val: v.val() });
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
                // Ordering `int` and `str` needs nothing from the program. A
                // user type already spells its order as `cmp`, for the
                // comparison operators -- but calling back into generated
                // code means a function REFERENCE in the IR, which does not
                // exist yet and is the same thing closures will need. Better
                // one mechanism later than a second one bolted on here.
                let f = match self.underlying(elem) {
                    Ty::Int => "rt_sort_int",
                    Ty::Float => "rt_sort_float",
                    Ty::Str => "rt_sort_str",
                    _ => {
                        return Err(Diag::new(
                            span,
                            format!(
                                "`sort` orders `int`, `float` and `str`; {} would \
                                 need the compiler to call its own `cmp`, which \
                                 is not possible yet",
                                self.tyname(elem)
                            ),
                        ))
                    }
                };
                self.push(Inst::Call {
                    dst: None,
                    func: f.to_string(),
                    args: vec![o.val()],
                });
                Ok(Val::void())
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
                let d = self.new_val(IrTy::Ref);
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

    /// Find the `Ty` for an already-declared type, by name.
    ///
    /// The arena is populated by monomorphisation from types the source
    /// spells. A container the source only implies -- `List<K>` behind
    /// `Map<K, V>.keys()` -- has a declaration but no arena entry until
    /// something asks for one.
    fn ty_named(&mut self, name: &str) -> Option<Ty> {
        if let Some(i) = self
            .ty_exprs
            .iter()
            .position(|e| e.name == name && e.args.is_empty())
        {
            return Some(Ty::User(i as u32));
        }
        if !self.typedefs.iter().any(|d| d.name == name) {
            return None;
        }
        self.ty_exprs.push(TyExpr {
            name: name.to_string(),
            args: Vec::new(),
        });
        Some(Ty::User((self.ty_exprs.len() - 1) as u32))
    }

    /// The `List<T>` type for a given element type. Monomorphisation
    /// declares one alongside every `Map<K, V>`, so this cannot fail for a
    /// map's key or value type.
    fn list_of(&mut self, elem: Ty) -> Option<Ty> {
        let name = self
            .typedefs
            .iter()
            .enumerate()
            .find(|(i, d)| {
                d.name.starts_with("List$") && self.field_surface[*i].first() == Some(&elem)
            })
            .map(|(_, d)| d.name.clone())?;
        self.ty_named(&name)
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
    fn lower_option_method(
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
        let inner = self.variant_surface[tid as usize]
            .iter()
            .find_map(|p| p.first().copied())
            .expect("Option always carries one payload type");
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
                    idx: 0,
                });
                // Borrowed from the Option, like any payload -- so it is
                // retained here and released by the statement, which is what
                // makes both arms agree about who owns the result.
                if self.is_ref(inner) {
                    self.push(Inst::RcInc { val: got });
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
                        self.push(Inst::RcInc { val: d.val() });
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
        let d = self.new_val(IrTy::Ref);
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
    fn make_option(&mut self, tid: u32, tag: u32, payload: Option<Value>) -> Value {
        let d = self.new_val(IrTy::Ref);
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
    fn lower_map_method(
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
                    self.push(Inst::RcInc { val: raw });
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
                let d = self.new_val(IrTy::Ref);
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
    fn lower_chan_builtin(&mut self, name: &str, args: &Args, span: Span) -> Result<Val, Diag> {
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

    /// Hand a reference across a thread boundary -- `send` or `spawn`.
    ///
    /// Two cases, and conflating them was three separate bugs:
    ///
    /// - We **own** the reference (an owned temporary, or a local this scope
    ///   registered). Then it is a MOVE: transfer it, emit nothing, and
    ///   refuse any later use.
    /// - We only **borrow** it (an element, a field, a parameter). Then the
    ///   +1 belongs to somebody else, so the receiver needs one of its own:
    ///   retain. The value stays usable here, because nothing was given up.
    ///
    /// The receiving side always releases, so both cases balance.
    fn transfer(&mut self, v: &Val, arg: &Expr, span: Span) -> Result<(), Diag> {
        if !self.is_ref(v.ty) || self.chan_elem(v.ty).is_some() {
            return Ok(());
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
        if let Expr::Var(n, s) = arg {
            if self.owns_local(n) {
                return self.mark_moved(n, *s);
            }
            return Err(Diag::new(
                *s,
                format!(
                    "`{n}` is borrowed here and cannot cross a thread boundary; \
                     a reference two threads can reach would race on a non-atomic \
                     refcount. Use clone({n}) to send a copy."
                ),
            ));
        }
        let _ = span;
        Err(Diag::new(
            arg.span(),
            "this value is borrowed from something else and cannot cross a thread \
             boundary; wrap it in clone(..) to send a copy"
                .to_string(),
        ))
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

    /// One forwarder per promoted method: `void Dog.speak()` calling
    /// `Animal.speak()` on the embedded field.
    ///
    /// A method the outer type defines itself always wins.
    ///
    /// Run to a FIXPOINT, because transitivity does not fall out for free:
    /// when Puppy embeds Dog which embeds Animal, `Dog.count_legs` is itself
    /// a forwarder generated in this same pass, so it is not visible until
    /// the round that created it has finished. Each round consults the
    /// forwarders the previous rounds produced.
    fn embed_forwarders(&self, p: &Program) -> Result<Vec<Func>, Diag> {
        let mut out: Vec<Func> = Vec::new();
        // Which embedded field each forwarder came through, keyed the same
        // way as `out`. Two fields offering one name is ambiguous, and
        // telling them apart needs to know where each came from.
        let mut via: HashMap<String, String> = HashMap::new();
        loop {
            let before = out.len();
            self.forward_round(p, &mut out, &mut via)?;
            if out.len() == before {
                return Ok(out);
            }
        }
    }

    /// Two functions must not reach the C backend with the same C name.
    ///
    /// Monomorphisation mangles with `$`, and the emitter rewrites `$` to
    /// `__` -- which a source identifier may also contain. So a generic
    /// `id<T>` instantiated at `int` becomes `id$int` becomes `fn_id__int`,
    /// and a hand-written `id__int` collides with it. The emitter asserts
    /// this, but an assertion is a panic and a core dump on input that is
    /// otherwise valid; the user deserves a diagnostic with a location.
    fn check_c_name_collisions(p: &Program, forwarders: &[Func]) -> Result<(), Diag> {
        let mut seen: HashMap<String, String> = HashMap::new();
        for f in p.funcs.iter().chain(forwarders.iter()) {
            let key = f.key();
            let c = crate::emit_c::c_ident(&key);
            if let Some(other) = seen.insert(c, key.clone()) {
                if other != key {
                    return Err(Diag::new(
                        f.span,
                        format!(
                            "`{}` and `{}` would both be emitted as the same \
                             C function; rename one",
                            crate::ast::bare(&key),
                            crate::ast::bare(&other)
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn forward_round(
        &self,
        p: &Program,
        out: &mut Vec<Func>,
        via: &mut HashMap<String, String>,
    ) -> Result<(), Diag> {
        for (tid, t) in p.types.iter().enumerate() {
            if t.is_interface {
                continue;
            }
            for f in t.fields.iter().filter(|f| f.embedded) {
                let Some(inner) = self.tdef_of(f.ty) else {
                    continue;
                };
                let iname = self.typedefs[inner as usize].name.clone();
                // Every instance method of the embedded type, by name. A
                // static method is not promoted: it has no receiver, so a
                // forwarder would have nothing to forward to, and `Outer`
                // does not gain a `make()` by holding a `Base`. Forwarding
                // one used to call it through a value and fail the whole
                // declaration of the embedding type.
                let prefix = format!("{iname}.");
                let mut promoted: Vec<(String, Sig)> = self
                    .sigs
                    .iter()
                    .filter(|(k, _)| !self.statics.contains(k.as_str()))
                    .filter_map(|(k, sig)| {
                        let m = k.strip_prefix(&prefix)?;
                        Some((
                            m.to_string(),
                            Sig {
                                params: sig.params.clone(),
                                ret: sig.ret,
                                module: sig.module.clone(),
                                is_pub: sig.is_pub,
                                is_prim: sig.is_prim,
                            },
                        ))
                    })
                    .collect();
                // Forwarders already generated count as the inner type's
                // methods, which is what makes deeper embedding work.
                for g in out.iter() {
                    if g.recv.as_deref() == Some(iname.as_str()) {
                        promoted.push((
                            g.name.clone(),
                            Sig {
                                params: g.params.clone(),
                                ret: g.ret,
                                module: g.module.clone(),
                                is_pub: g.is_pub,
                                is_prim: g.is_prim,
                            },
                        ));
                    }
                }
                // `self.sigs` is a HashMap with a randomised hasher, so the
                // order methods come out of it differs between runs of the
                // compiler. Emission follows this order, which made the
                // emitted C non-reproducible whenever a type promoted two or
                // more methods. Sort, so a build is a function of its input.
                promoted.sort_by(|a, b| a.0.cmp(&b.0));
                promoted.dedup_by(|a, b| a.0 == b.0);

                for (mname, sig) in promoted {
                    let key = format!("{}.{mname}", t.name);
                    if self.sigs.contains_key(&key) {
                        continue; // the outer type defines it itself, and wins
                    }
                    if let Some(other) = via.get(&key) {
                        if *other == f.name {
                            continue; // already forwarded through this field
                        }
                        // Two embedded types offer this name and the outer
                        // type does not break the tie. Picking one would be
                        // picking by declaration order, which is not a rule
                        // anyone should have to know. Go rejects this too.
                        return Err(Diag::new(
                            f.span,
                            format!(
                                "`{}` gets `{mname}` from both `{}` and `{}`; \
                                 give `{}` its own `{mname}` to say which one it means",
                                self.bare_name(&t.name),
                                self.bare_name(other),
                                self.bare_name(&f.name),
                                self.bare_name(&t.name)
                            ),
                        ));
                    }
                    via.insert(key, f.name.clone());
                    let args = Args {
                        pos: sig
                            .params
                            .iter()
                            .filter(|q| !q.is_optional())
                            .map(|q| Expr::Var(q.name.clone(), f.span))
                            .collect(),
                        named: sig
                            .params
                            .iter()
                            .filter(|q| q.is_optional())
                            .map(|q| (q.name.clone(), Expr::Var(q.name.clone(), f.span)))
                            .collect(),
                    };
                    let call = Expr::MethodCall(
                        Box::new(Expr::Var(f.name.clone(), f.span)),
                        mname.clone(),
                        args,
                        f.span,
                    );
                    let body = if sig.ret == Ty::Void {
                        vec![Stmt::Eval {
                            expr: call,
                            span: f.span,
                        }]
                    } else {
                        vec![Stmt::Return {
                            value: Some(call),
                            span: f.span,
                        }]
                    };
                    out.push(Func {
                        module: t.module.clone(),
                        is_pub: true,
                        ret: sig.ret,
                        is_static: false,
                        is_prim: false,
                        recv: Some(t.name.clone()),
                        name: mname,
                        tparams: Vec::new(),
                        recv_tparams: Vec::new(),
                        params: sig.params.clone(),
                        body,
                        span: f.span,
                    });
                }
            }
            let _ = tid;
        }
        Ok(())
    }

    /// Bind a call's arguments to a parameter list, by Oro's rule:
    /// **mandatory parameters are positional, optional ones are named.**
    /// Never both, so there is no question of which form to use and no
    /// question of what order optional arguments come in.
    ///
    /// Returns one expression per parameter, in declaration order, with
    /// defaults filled in.
    fn bind_args<'a>(
        &self,
        what: &str,
        params: &'a [Param],
        args: &'a Args,
        span: Span,
    ) -> Result<Vec<&'a Expr>, Diag> {
        let mandatory: Vec<&Param> = params.iter().filter(|p| !p.is_optional()).collect();
        let mut out: Vec<Option<&Expr>> = vec![None; params.len()];

        // Named arguments first: "you named a mandatory parameter" is a more
        // useful thing to say than "wrong number of positional arguments",
        // and it is the mistake someone coming from Python will make.
        for (n, e) in &args.named {
            let Some(i) = params.iter().position(|p| p.name == *n) else {
                return Err(Diag::new(
                    e.span(),
                    format!("`{}` has no parameter `{n}`", self.bare_name(what)),
                ));
            };
            if !params[i].is_optional() {
                return Err(Diag::new(
                    e.span(),
                    format!("`{n}` is mandatory, so it is positional; drop the `{n}:`"),
                ));
            }
            if out[i].is_some() {
                return Err(Diag::new(e.span(), format!("`{n}` given twice")));
            }
            out[i] = Some(e);
        }

        if args.pos.len() != mandatory.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` takes {} positional argument(s), found {}",
                    self.bare_name(what),
                    mandatory.len(),
                    args.pos.len()
                ),
            ));
        }

        let mut next = 0;
        for a in &args.pos {
            while params[next].is_optional() {
                next += 1;
            }
            out[next] = Some(a);
            next += 1;
        }

        Ok(params
            .iter()
            .zip(out)
            .map(|(p, given)| {
                given.unwrap_or_else(|| p.default.as_ref().expect("mandatory unbound"))
            })
            .collect())
    }

    pub fn lower_program(mut self, p: &Program) -> Result<ir::Module, Diag> {
        // Builtins. `print` is special-cased in the call path because it
        // accepts int, bool or str and picks the runtime helper statically;
        // that is not user-visible overloading, which does not exist.
        self.builtin("concat", vec![Ty::Str, Ty::Str], Ty::Str);

        self.imports_by_module = p.imports_by_module.clone();
        self.generic_methods = p.generic_methods.clone();
        for f in &p.funcs {
            if !f.module.is_empty() {
                self.modules.insert(f.module.clone());
            }
        }
        for t in &p.types {
            if !t.module.is_empty() {
                self.modules.insert(t.module.clone());
            }
        }

        // After monomorphisation every Ty::User names a concrete declaration
        // with no arguments, so resolution is a name lookup.
        self.ty_exprs = p.ty_exprs.clone();
        self.shown = p.shown.clone();

        // Type table first: signatures and field types may refer to any type,
        // including one declared later in the file.
        for t in &p.types {
            self.check_not_import(&t.module, crate::ast::bare(&t.name), t.span)
                .map_err(|d| d.in_module(&t.module))?;
            if self.typedefs.iter().any(|d| d.name == t.name) {
                return Err(Diag::new(
                    t.span,
                    format!("type `{}` is already defined", self.show_name(&t.name)),
                ));
            }
            let mut fields = Vec::new();
            for f in &t.fields {
                if fields.iter().any(|(n, _): &(String, IrTy)| *n == f.name) {
                    return Err(Diag::new(
                        f.span,
                        format!(
                            "duplicate field `{}` in type `{}`",
                            f.name,
                            self.show_name(&t.name)
                        ),
                    ));
                }
                fields.push((f.name.clone(), self.irty(f.ty)));
            }
            let mut variants = Vec::new();
            let mut vsurface = Vec::new();
            for v in &t.variants {
                variants.push(ir::Variant {
                    name: v.name.clone(),
                    payload: v.payload.iter().map(|p| self.irty(*p)).collect(),
                });
                vsurface.push(v.payload.clone());
            }
            self.variant_surface.push(vsurface);
            self.type_module.push(t.module.clone());
            self.type_pub.push(t.is_pub);
            self.typedefs.push(TypeDef {
                name: t.name.clone(),
                fields,
                variants,
                is_enum: t.is_enum,
                is_interface: t.is_interface,
                is_chan: t.name.starts_with("Chan$")
                    || t.name.starts_with("Array$")
                    || t.name.starts_with("List$")
                    || t.name.starts_with("Map$"),
                is_distinct: t.distinct_base.is_some(),
                vtable: Vec::new(),
            });
            self.distinct_base.push(t.distinct_base);
            self.iface_methods.push(t.methods.clone());
            // One dispatch slot per distinct interface method name, for the
            // whole program.
            for m in &t.methods {
                let slot = self.slot_of(m);
                if !self.iface_slots.contains(&slot) {
                    self.iface_slots.push(slot);
                }
            }
            self.field_surface
                .push(t.fields.iter().map(|f| f.ty).collect());
            self.field_params.push(t.fields.clone());
        }

        for f in &p.funcs {
            let _tag = f.module.clone();
            // Habit from C, Java and Go. Without this it declares an ordinary
            // function nothing calls, and the program silently does nothing --
            // the worst failure mode for someone who has written C before.
            // Names are interned module-qualified by now, so compare the bare
            // one; comparing `f.name` itself had quietly stopped matching.
            if f.recv.is_none() && crate::ast::bare(&f.name) == "main" {
                return Err(Diag::new(
                    f.span,
                    "there is no `main`: statements at the top level are the program",
                )
                .in_module(&f.module));
            }
            // In every module, not only the entry file. Allowing it in an
            // imported module would give `lib.print(..)` and bare `print(..)`
            // two meanings a reader has to keep apart, for no gain.
            if f.recv.is_none() && BUILTIN_FNS.contains(&crate::ast::bare(&f.name)) {
                return Err(Diag::new(
                    f.span,
                    format!(
                        "`{}` is a builtin function; shadowing is not allowed, rename this one",
                        crate::ast::bare(&f.name)
                    ),
                )
                .in_module(&f.module));
            }
            if f.recv.is_none() {
                self.check_not_import(&f.module, crate::ast::bare(&f.name), f.span)
                    .map_err(|d| d.in_module(&f.module))?;
            }
            if self.sigs.contains_key(&f.key()) {
                return Err(Diag::new(
                    f.span,
                    format!("`{}` is already defined", f.key()),
                ));
            }
            if f.is_static {
                // `Type.name(..)` is also how an enum variant is written, so
                // a static method may not take a variant's name -- there
                // would be no way to say which was meant.
                if let Some(r) = &f.recv {
                    if let Some(d) = self.typedefs.iter().find(|d| d.name == *r) {
                        if d.variants.iter().any(|v| v.name == f.name) {
                            return Err(Diag::new(
                                f.span,
                                format!(
                                    "`{}` already has a variant `{}`, and \
                                     `{}.{}` would be both",
                                    self.bare_name(r),
                                    f.name,
                                    self.bare_name(r),
                                    f.name
                                ),
                            ));
                        }
                    }
                }
                self.statics.insert(f.key());
            }
            if let Some(r) = &f.recv {
                // A module may only add methods to types it declared itself.
                // Otherwise one module could reach into another's private
                // type by declaring a method on it -- and a method written
                // far from its type is hard to find even when it is allowed.
                if let Some(i) = self.typedefs.iter().position(|d| d.name == *r) {
                    let owner = self.type_module[i].clone();
                    if !owner.is_empty() && owner != f.module {
                        return Err(Diag::new(
                            f.span,
                            format!(
                                "`{r}` is declared in `{owner}`; a method may only \
                                 be added to a type its own module declared"
                            ),
                        ));
                    }
                }
                if !self.typedefs.iter().any(|d| d.name == *r) {
                    // A bare receiver that another module declares is the
                    // common mistake here, and "unknown type" does not say
                    // what to do about it.
                    let suffix = format!("#{r}");
                    if let Some(i) = self.typedefs.iter().position(|d| d.name.ends_with(&suffix)) {
                        return Err(Diag::new(
                            f.span,
                            format!(
                                "`{}` is declared in `{}`; a method may only be \
                                 added to a type its own module declared",
                                self.bare_name(r),
                                self.type_module[i]
                            ),
                        ));
                    }
                    return Err(Diag::new(f.span, format!("unknown type `{r}`")));
                }
            }
            // A type name wins in construction position, so a function
            // sharing one is silently unreachable. Names are case-blind here
            // -- nothing requires a type to be capitalised -- which makes the
            // collision easy to hit by accident.
            if self.typedefs.iter().any(|d| d.name == f.name) {
                return Err(Diag::new(
                    f.span,
                    format!("`{}` is already a type", crate::ast::bare(&f.name)),
                ));
            }
            self.sigs.insert(
                f.key(),
                Sig {
                    params: f.params.clone(),
                    ret: f.ret,
                    module: f.module.clone(),
                    is_pub: f.is_pub,
                    is_prim: f.is_prim,
                },
            );
        }

        // Embedding promotes methods by SYNTHESISING FORWARDERS, one per
        // promoted method, rather than by teaching every call site about
        // embedding. Everything downstream -- direct calls, vtables,
        // interface satisfaction -- then works unchanged, and transitivity
        // falls out for free, because an inner type's own forwarders are
        // already methods by the time the outer one looks.
        let forwarders = self.embed_forwarders(p)?;
        Self::check_c_name_collisions(p, &forwarders)?;
        for f in &forwarders {
            self.sigs.insert(
                f.key(),
                Sig {
                    params: f.params.clone(),
                    ret: f.ret,
                    module: f.module.clone(),
                    is_pub: f.is_pub,
                    is_prim: f.is_prim,
                },
            );
        }

        // There is no `main`. The statements written at the top level are
        // the program, in source order, and they are lowered as the body of
        // one synthesised function. Declarations are order-independent, so a
        // function may be called above its own definition.
        let entry = Func {
            module: p.module.clone(),
            is_pub: false,
            ret: Ty::Void,
            is_static: false,
            is_prim: false,
            recv: None,
            name: "$main".to_string(),
            tparams: Vec::new(),
            recv_tparams: Vec::new(),
            params: Vec::new(),
            body: p.toplevel.clone(),
            span: Span::new(1, 1),
        };

        let mut funcs = Vec::new();
        for f in &p.funcs {
            // A primitive has no body to lower: its implementation is the
            // runtime function the call site names. Emitting a definition
            // here would collide with the real one at link time.
            if f.is_prim {
                continue;
            }
            funcs.push(self.lower_func(f)?);
        }
        for f in &forwarders {
            funcs.push(self.lower_func(f)?);
        }
        funcs.push(self.lower_func(&entry)?);
        // Fill each concrete type's vtable now that every method is known.
        let slots = self.iface_slots.clone();
        for i in 0..self.typedefs.len() {
            if self.typedefs[i].is_interface
                || self.typedefs[i].is_chan
                || self.typedefs[i].is_distinct
            {
                continue;
            }
            let tname = self.typedefs[i].name.clone();
            // The fill CHECKS rather than trusts. A bare name lookup put a
            // static -- whose signature is one argument short -- into a slot
            // that the call site then cast to the interface's shape.
            let vt: Vec<Option<String>> = slots
                .iter()
                .map(|slot| {
                    let key = format!("{tname}.{}", slot.name);
                    if self.statics.contains(&key) {
                        return None;
                    }
                    // The SHAPE has to match too. A name lookup alone would
                    // put a method into a slot the call site casts to some
                    // other signature.
                    let sig = self.sigs.get(&key)?;
                    let shape = ir::Slot {
                        name: slot.name.clone(),
                        params: sig.params.iter().map(|p| self.irty(p.ty)).collect(),
                        ret: (sig.ret != Ty::Void).then(|| self.irty(sig.ret)),
                    };
                    (shape == *slot).then_some(key)
                })
                .collect();
            self.typedefs[i].vtable = vt;
        }

        Ok(ir::Module {
            funcs,
            strings: self.strings,
            types: self.typedefs,
            iface_slots: self.iface_slots,
        })
    }

    // ---- function scaffolding ----------------------------------------

    fn new_val(&mut self, t: IrTy) -> Value {
        let v = Value(self.types.len() as u32);
        self.types.push(t);
        v
    }

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(BlockBuf {
            id,
            params: Vec::new(),
            insts: Vec::new(),
            term: None,
        });
        id
    }

    fn switch_to(&mut self, b: BlockId) {
        self.cur = self
            .blocks
            .iter()
            .position(|x| x.id == b)
            .expect("unknown block");
    }

    fn push(&mut self, i: Inst) {
        debug_assert!(
            self.blocks[self.cur].term.is_none(),
            "instruction after terminator"
        );
        self.blocks[self.cur].insts.push(i);
    }

    fn terminate(&mut self, t: Term) {
        if self.blocks[self.cur].term.is_none() {
            self.blocks[self.cur].term = Some(t);
        }
    }

    fn terminated(&self) -> bool {
        self.blocks[self.cur].term.is_some()
    }

    fn lookup(&self, name: &str) -> Option<(Ty, Value)> {
        self.binding(name).map(|(t, v, _)| (t, v))
    }

    /// Refuse any name that is already visible.
    ///
    /// **Nothing shadows anything, anywhere.** Not an outer local, not a
    /// parameter, not a function, not a type. This is what removes the need
    /// for a `this` keyword -- a bare name can only ever mean one thing, so
    /// there is nothing to disambiguate -- and it deletes the entire class of
    /// bugs where a reader and the compiler disagree about which `x` is meant.
    ///
    /// The cost is real and deliberate: the programmer renames.
    fn check_shadow(&self, name: &str, span: Span) -> Result<(), Diag> {
        self.check_not_import(&self.cur_module, name, span)?;
        if self.binding(name).is_some() {
            return Err(Diag::new(
                span,
                format!("`{name}` is already in scope; shadowing is not allowed, rename one"),
            ));
        }
        // Against this module's own function, not the bare name: functions
        // are interned module-qualified, so `sigs` no longer holds the name
        // as it was written.
        if self.sigs.contains_key(&self.resolve_fn(name)) || BUILTIN_FNS.contains(&name) {
            return Err(Diag::new(
                span,
                format!("`{name}` is already a function; shadowing is not allowed, rename one"),
            ));
        }
        if self.typedefs.iter().any(|d| d.name == name) {
            return Err(Diag::new(
                span,
                format!("`{name}` is already a type; shadowing is not allowed, rename one"),
            ));
        }
        if let Some((tid, _)) = self.recv {
            if self.field_path(tid, name).is_some() {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{name}` is already a field of `{}`; shadowing is not allowed, rename one",
                        self.show_name(&self.typedefs[tid as usize].name)
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Refuse a declaration in `module` that takes the name of a module
    /// that file imports -- a local, a parameter, a function or a type.
    ///
    /// `lib.f()` with a local `lib` in scope used to call a method on the
    /// local, so the import was silently shadowed for the rest of the
    /// function and a reader could not tell which `lib` was meant. Only this
    /// file's imports count: a module some other file imports is not in
    /// scope here, and adding an import deep in a library must not break a
    /// name in a file that never mentions it.
    fn check_not_import(&self, module: &str, name: &str, span: Span) -> Result<(), Diag> {
        let imported = self
            .imports_by_module
            .get(module)
            .is_some_and(|v| v.iter().any(|m| m == name));
        if imported {
            return Err(Diag::new(
                span,
                format!("`{name}` is an imported module; shadowing is not allowed, rename one"),
            ));
        }
        Ok(())
    }

    /// A bare name that names a field of the receiver, promoted fields
    /// included. Returns the receiver and the path to reach it.
    fn recv_field(&self, name: &str) -> Option<(u32, Value, Vec<u32>)> {
        let (tid, obj) = self.recv?;
        let path = self.field_path(tid, name)?;
        Some((tid, obj, path))
    }

    fn binding(&self, name: &str) -> Option<Binding> {
        for s in self.scopes.iter().rev() {
            if let Some(x) = s.get(name) {
                return Some(*x);
            }
        }
        None
    }

    /// A type's name, for diagnostics. `Ty::name()` cannot do this because it
    /// has no access to the interning arena.
    /// A type's name as a reader wrote it.
    ///
    /// Declared types are interned module-qualified (`lib#Point`) so that two
    /// modules may each declare a `Point`. Nobody should ever see that
    /// spelling: inside its own module it is `Point`, and from outside it is
    /// `lib.Point`, which is how it would be written.
    /// A type as the source spells it: `List<int>`, `Map<str, lib.Point>`.
    fn tyname(&self, t: Ty) -> String {
        match t {
            Ty::User(i) => self.show_name(&self.ty_exprs[i as usize].name),
            other => other.name().to_string(),
        }
    }

    /// A type's name for a diagnostic: module-qualified unless it is this
    /// module's own, and an instantiation spelled with its type arguments
    /// rather than its mangled C name. Every diagnostic that names a type
    /// goes through here or `tyname`, so none of them leaks a `$`.
    fn show_name(&self, raw: &str) -> String {
        if let Some((base, args)) = self.shown.get(raw) {
            let args: Vec<String> = args.iter().map(|a| self.tyname(*a)).collect();
            return format!("{}<{}>", self.show_name(base), args.join(", "));
        }
        match raw.split_once('#') {
            Some((m, n)) if m == self.cur_module => n.to_string(),
            Some((m, n)) => format!("{m}.{n}"),
            None => raw.to_string(),
        }
    }

    /// A type or function name without its module, for a diagnostic that
    /// names something by its bare name: `Pair<int>` for `lib#Pair$int`,
    /// and `Pair<int>.get` for the method key `lib#Pair$int.get`.
    fn bare_name(&self, raw: &str) -> String {
        let (head, rest) = match raw.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (raw, None),
        };
        let head = match self.shown.get(head) {
            Some((base, args)) => {
                let args: Vec<String> = args.iter().map(|a| self.tyname(*a)).collect();
                format!("{}<{}>", crate::ast::bare(base), args.join(", "))
            }
            None => crate::ast::bare(head).to_string(),
        };
        match rest {
            Some(m) => format!("{head}.{m}"),
            None => head,
        }
    }

    /// The representation of a surface type. A distinct type is represented
    /// exactly as its base -- that is the whole point: `distinct int Price`
    /// is an `i64` at runtime, with no object, no header and no refcount.
    fn irty(&self, t: Ty) -> IrTy {
        match self.base_of(t) {
            Some(b) => self.irty(b),
            None => ir_ty(t),
        }
    }

    /// The base type of a distinct type, if it is one.
    fn base_of(&self, t: Ty) -> Option<Ty> {
        let tid = self.tdef_of(t)?;
        self.distinct_base[tid as usize]
    }

    /// Strip distinctness down to the underlying ordinary type.
    fn underlying(&self, t: Ty) -> Ty {
        match self.base_of(t) {
            Some(b) => self.underlying(b),
            None => t,
        }
    }

    /// The element type of a channel type, if it is one.
    /// Does this type need refcounting? A distinct type follows its base --
    /// `distinct int Price` is not a reference, however it is spelled.
    fn is_ref(&self, t: Ty) -> bool {
        self.underlying(t).is_ref()
    }

    fn chan_elem(&self, t: Ty) -> Option<Ty> {
        self.builtin_elem(t, "Chan$")
    }

    /// The element type of an `Array<T>` or `List<T>`, if it is one.
    fn seq_elem(&self, t: Ty) -> Option<Ty> {
        self.builtin_elem(t, "Array$")
            .or_else(|| self.builtin_elem(t, "List$"))
    }

    fn is_list(&self, t: Ty) -> bool {
        self.builtin_elem(t, "List$").is_some()
    }

    /// The key and value types of a `Map<K, V>`, if it is one.
    ///
    /// Both this and `builtin_elem` look through a distinct type, because
    /// `distinct List<int> Bag` IS a list: indexing it, iterating it and its
    /// built-in methods all work exactly as on the base. Identity is kept
    /// where it matters -- `assignable` still refuses a `Bag` for a
    /// `List<int>` -- and that check never asks this question.
    fn map_kv(&self, t: Ty) -> Option<(Ty, Ty)> {
        let tid = self.tdef_of(self.underlying(t))?;
        if !self.typedefs[tid as usize].name.starts_with("Map$") {
            return None;
        }
        let f = &self.field_surface[tid as usize];
        Some((f[0], f[1]))
    }

    /// A builtin generic stores its element type as its only "field", which
    /// is never laid out -- the runtime owns the representation.
    fn builtin_elem(&self, t: Ty, prefix: &str) -> Option<Ty> {
        let tid = self.tdef_of(self.underlying(t))?;
        if !self.typedefs[tid as usize].name.starts_with(prefix) {
            return None;
        }
        Some(self.field_surface[tid as usize][0])
    }

    /// The dispatch slot an interface method occupies: its name and its
    /// IR-level shape. IR-level rather than surface, because the call site's
    /// cast is built from IR types -- so `int area()` and `Price area()`
    /// share a slot, which is right, and `int m()` and `str m(str)` do not.
    fn slot_of(&self, m: &Func) -> ir::Slot {
        ir::Slot {
            name: m.name.clone(),
            params: m.params.iter().map(|p| self.irty(p.ty)).collect(),
            ret: (m.ret != Ty::Void).then(|| self.irty(m.ret)),
        }
    }

    /// A type WRITTEN in this module has to exist and be reachable.
    ///
    /// `lib.Nope` interns like any other name, so without this it failed
    /// later as a mismatch against whatever it was compared to -- "expected
    /// lib.Nope, found lib.P", which says nothing about the real mistake.
    fn check_named_ty(&self, t: Ty, span: Span) -> Result<(), Diag> {
        let Ty::User(i) = t else { return Ok(()) };
        let raw = self.ty_exprs[i as usize].name.clone();
        let Some(tid) = self.tdef_of(t) else {
            return Err(Diag::new(
                span,
                match raw.split_once('#') {
                    Some((m, n)) => format!("`{m}` has no type `{n}`"),
                    None => format!("unknown type `{raw}`"),
                },
            ));
        };
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "it cannot be named from here"),
            ));
        }
        Ok(())
    }

    /// May the module being lowered name or reach into this type?
    ///
    /// A builtin carries no module and is visible everywhere. Anything else
    /// is visible inside its own module, and outside only if it is `pub`.
    fn type_visible(&self, tid: u32) -> bool {
        let m = &self.type_module[tid as usize];
        m.is_empty() || *m == self.cur_module || self.type_pub[tid as usize]
    }

    /// May the module being lowered call this function or method? The
    /// same rule as for a type: a builtin everywhere, anything else in its
    /// own module, and outside it only if it is `pub`.
    fn sig_visible(&self, sig: &Sig) -> bool {
        sig.module.is_empty() || sig.module == self.cur_module || sig.is_pub
    }

    /// Refuse a method call the module being lowered may not make: the type
    /// has to be visible here, and the method callable from here. A method
    /// found BY NAME -- `to_str` for `print` and `str(..)`, `add`, `eq` and
    /// `cmp` for an operator -- goes through this too. The spelling hides the
    /// call, not the rule: `print(v)` is `v.to_str()`, so it gets exactly
    /// `v.to_str()`'s checks and diagnostics.
    fn check_method_access(&self, tid: u32, m: &str, span: Span) -> Result<(), Diag> {
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "its methods cannot be called from here"),
            ));
        }
        let key = format!("{}.{m}", self.typedefs[tid as usize].name);
        if let Some(sig) = self.sigs.get(&key) {
            if !self.sig_visible(sig) {
                return Err(Diag::new(
                    span,
                    format!("`{m}` is private to `{}`", sig.module),
                ));
            }
        }
        Ok(())
    }

    /// The diagnostic for reaching into a type that is not visible here.
    fn not_visible(&self, tid: u32, what: &str) -> String {
        format!(
            "`{}` is private to `{}`; {what}",
            self.bare_name(&self.typedefs[tid as usize].name),
            self.type_module[tid as usize]
        )
    }

    /// Is `from` usable where `to` is expected?
    ///
    /// Identical types always. Beyond that, a concrete type is assignable to
    /// an interface when it has every required method with a matching
    /// signature -- structurally, with no `implements` clause, so a type
    /// written before the interface existed can satisfy it.
    fn assignable(&self, from: Ty, to: Ty) -> bool {
        if from == to {
            return true;
        }
        let (Some(ft), Some(tt)) = (self.tdef_of(from), self.tdef_of(to)) else {
            return false;
        };
        if !self.typedefs[tt as usize].is_interface || self.typedefs[ft as usize].is_interface {
            return false;
        }
        // A distinct type is ERASED before the IR, so it has no object header
        // of its own and therefore cannot carry its own vtable. Dispatch
        // through one would always find the BASE's method: `distinct Base
        // Wrap` with its own `tag` printed the base's answer, exit 0, no
        // warning and nothing for a sanitiser to see. Over a non-reference
        // base it was worse -- the emitted C did not even compile.
        //
        // This is forced by the representation, not a policy choice.
        if self.typedefs[ft as usize].is_distinct {
            return false;
        }
        self.missing_method(ft, tt).is_none()
    }

    /// "expected X, found Y", plus the reason when Y nearly satisfies an
    /// interface X. Naming the missing method is the difference between a
    /// diagnostic you can act on and one you have to investigate.
    fn mismatch(&self, want: Ty, got: Ty) -> String {
        let base = format!(
            "type mismatch: expected {}, found {}",
            self.tyname(want),
            self.tyname(got)
        );
        let (Some(tt), Some(ft)) = (self.tdef_of(want), self.tdef_of(got)) else {
            return base;
        };
        if !self.typedefs[tt as usize].is_interface || self.typedefs[ft as usize].is_interface {
            return base;
        }
        if self.typedefs[ft as usize].is_distinct {
            return format!(
                "{base}: a distinct type is erased before it reaches the runtime, \
                 so it has no place to carry its own methods and cannot satisfy \
                 an interface. Use its base type, or make it a `type` of its own."
            );
        }
        match self.missing_method(ft, tt) {
            Some(why) => format!("{base}: {why}"),
            None => base,
        }
    }

    /// Why `ft` does not satisfy the interface `tt`, as a sentence naming
    /// the first required method it fails on; `None` when it does satisfy it.
    ///
    /// Satisfaction is judged from the module being lowered, which is where
    /// the conversion to the interface happens, and only methods callable
    /// from there count. An interface is just a way of calling methods
    /// later, so a method the module could not call directly must not
    /// become callable by passing the value through an interface it
    /// declared for the purpose. The owning module converting its own value
    /// and handing out the interface is fine: it can see its own methods,
    /// and exporting behaviour that way is its decision to make.
    fn missing_method(&self, ft: u32, tt: u32) -> Option<String> {
        let needs = |what: String| {
            format!(
                "`{}` needs a method `{what}` to satisfy `{}`",
                self.show_name(&self.typedefs[ft as usize].name),
                self.show_name(&self.typedefs[tt as usize].name)
            )
        };
        let fname = &self.typedefs[ft as usize].name.clone();
        for m in &self.iface_methods[tt as usize] {
            let key = format!("{fname}.{}", m.name);
            // A static has no receiver, so its C signature is one argument
            // short of what the vtable slot is cast to. Accepting one put the
            // receiver pointer into the first declared parameter and dropped
            // the real argument.
            if self.statics.contains(&key) {
                return Some(needs(format!(
                    "{} {}(..) that is not static -- a static method has no \
                     receiver to dispatch on",
                    self.tyname(m.ret),
                    m.name
                )));
            }
            let Some(sig) = self.sigs.get(&key) else {
                return Some(needs(format!("{} {}(..)", self.tyname(m.ret), m.name)));
            };
            // The same two checks a direct call makes, in the same order,
            // so the reason given matches what `v.m()` would have said.
            if !self.type_visible(ft) {
                return Some(format!(
                    "`{}` is private to `{}`, so its method `{}` cannot \
                     satisfy `{}` here",
                    crate::ast::bare(fname),
                    self.type_module[ft as usize],
                    m.name,
                    self.show_name(&self.typedefs[tt as usize].name)
                ));
            }
            if !self.sig_visible(sig) {
                return Some(format!(
                    "`{}` has a method `{}`, but it is private to `{}`, so it \
                     cannot satisfy `{}` here",
                    self.show_name(fname),
                    m.name,
                    sig.module,
                    self.show_name(&self.typedefs[tt as usize].name)
                ));
            }
            let same = sig.ret == m.ret
                && sig.params.len() == m.params.len()
                && sig
                    .params
                    .iter()
                    .zip(m.params.iter())
                    .all(|(a, b)| a.ty == b.ty);
            if !same {
                return Some(needs(format!(
                    "{} {}(..) with a matching signature",
                    self.tyname(m.ret),
                    m.name
                )));
            }
        }
        None
    }

    /// The declaration a type names. `Ty::User` indexes the interning arena;
    /// the IR and the emitter want an index into the type table, and after
    /// monomorphisation the two are related by name alone.
    fn tdef_of(&self, t: Ty) -> Option<u32> {
        let Ty::User(i) = t else { return None };
        let name = &self.ty_exprs[i as usize].name;
        self.typedefs
            .iter()
            .position(|d| d.name == *name)
            .map(|x| x as u32)
    }

    fn rebind(&mut self, name: &str, v: Value) {
        for s in self.scopes.iter_mut().rev() {
            if let Some(slot) = s.get_mut(name) {
                slot.1 = v;
                return;
            }
        }
        unreachable!("rebind of unknown name");
    }

    /// The path of field indices to reach `name` from `tid`, promoting
    /// through embedded fields. Empty prefix means a direct field.
    ///
    /// Breadth-first, so a direct field always wins over a promoted one, and
    /// a shallower promotion wins over a deeper one -- the same rule Go uses.
    fn field_path(&self, tid: u32, name: &str) -> Option<Vec<u32>> {
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

    /// Walk a field path, emitting a load per step, and return the final
    /// value and its surface type.
    fn load_path(&mut self, tid: u32, obj: Value, path: &[u32]) -> (Value, Ty) {
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

    fn field_of(&self, tid: u32, name: &str) -> Option<(u32, IrTy)> {
        self.typedefs[tid as usize]
            .fields
            .iter()
            .position(|(n, _)| n == name)
            .map(|i| (i as u32, self.typedefs[tid as usize].fields[i].1))
    }

    fn lower_func(&mut self, f: &Func) -> Result<ir::Func, Diag> {
        self.lower_func_inner(f).map_err(|d| d.in_module(&f.module))
    }

    fn lower_func_inner(&mut self, f: &Func) -> Result<ir::Func, Diag> {
        self.cur_module = f.module.clone();
        self.types.clear();
        self.blocks.clear();
        self.scopes.clear();
        self.owned.clear();
        self.loops.clear();
        self.moved.clear();
        // Every statement flushes its own temporaries, so this is empty in a
        // well-formed lowering. Clearing it anyway keeps a value from one
        // function's block list out of the next one's, where its number would
        // name a different value entirely.
        self.stmt_temps.clear();
        self.synth = 0;
        self.cur = 0;
        self.ret_ty = f.ret;

        let entry = self.new_block();
        self.switch_to(entry);

        let mut scope = HashMap::new();
        let mut params = Vec::new();

        // A method takes its receiver as a hidden first parameter. It is not
        // nameable, because fields are reached bare.
        self.recv = None;
        // A static method is qualified by a type but takes no receiver, so
        // no hidden first parameter and no bare field names inside it.
        if let Some(rname) = f.recv.as_ref().filter(|_| !f.is_static) {
            let tid = self
                .typedefs
                .iter()
                .position(|d| d.name == *rname)
                .expect("receiver type checked above") as u32;
            let v = self.new_val(IrTy::Ref);
            params.push(v);
            self.recv = Some((tid, v));
        }

        for p in &f.params {
            self.check_not_import(&self.cur_module, &p.name, p.span)?;
            let v = self.new_val(self.irty(p.ty));
            params.push(v);
            if scope.insert(p.name.clone(), (p.ty, v, false)).is_some() {
                return Err(Diag::new(
                    p.span,
                    format!("duplicate parameter `{}`", p.name),
                ));
            }
        }
        self.blocks[self.cur].params = params.clone();
        self.scopes.push(scope);
        // Parameters are BORROWED (docs/ir-v0.md §5.1), so they are never
        // registered as owned and never decremented here.
        self.owned.push(Vec::new());

        self.lower_block(&f.body)?;

        // Fall off the end: release locals and return.
        if !self.terminated() {
            if f.ret != Ty::Void {
                return Err(Diag::new(
                    f.span,
                    format!(
                        "function `{}` must return a value of type {}",
                        self.bare_name(&f.name),
                        self.tyname(f.ret)
                    ),
                ));
            }
            self.release_all();
            self.terminate(Term::Ret { val: None });
        }
        self.scopes.pop();
        self.owned.pop();

        let blocks: Vec<Block> = self
            .blocks
            .iter()
            .map(|b| Block {
                id: b.id,
                params: b.params.clone(),
                insts: b.insts.clone(),
                // An unterminated block here is unreachable (both arms of an
                // `if` returned). Give it a well-formed terminator so every
                // backend sees a total CFG.
                term: b.term.clone().unwrap_or(Term::Ret { val: None }),
            })
            .collect();

        Ok(ir::Func {
            name: f.key(),
            params,
            ret: if f.ret == Ty::Void {
                None
            } else {
                Some(self.irty(f.ret))
            },
            blocks,
            types: self.types.clone(),
            entry,
        })
    }

    // ---- refcount release --------------------------------------------

    /// Emit rc_dec for every owned local in the innermost scope.
    fn release_scope(&mut self) {
        let names = self.owned.last().cloned().unwrap_or_default();
        for name in names.iter().rev() {
            if let Some((ty, v)) = self.lookup(name) {
                if self.is_ref(ty) {
                    self.push(Inst::RcDec { val: v });
                }
            }
        }
    }

    /// Emit rc_dec for every owned local in every enclosing scope, innermost
    /// first. Used on `return` and on falling off the end of a function.
    fn release_all(&mut self) {
        let all: Vec<Vec<String>> = self.owned.clone();
        for names in all.iter().rev() {
            for name in names.iter().rev() {
                if let Some((ty, v)) = self.lookup(name) {
                    if self.is_ref(ty) {
                        self.push(Inst::RcDec { val: v });
                    }
                }
            }
        }
    }

    /// Release owned locals from the innermost scope down to (but not
    /// including) `depth`. Used by `break` and `continue`, which leave every
    /// scope inside the loop body.
    fn release_to_depth(&mut self, depth: usize) {
        let all: Vec<Vec<String>> = self.owned.clone();
        for names in all.iter().skip(depth).rev() {
            for name in names.iter().rev() {
                if let Some((ty, v)) = self.lookup(name) {
                    if self.is_ref(ty) {
                        self.push(Inst::RcDec { val: v });
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
    fn flush_temps_since(&mut self, mark: usize) {
        let temps: Vec<Value> = self.stmt_temps.split_off(mark);
        for v in temps {
            self.push(Inst::RcDec { val: v });
        }
    }

    fn flush_temps(&mut self) {
        let temps = std::mem::take(&mut self.stmt_temps);
        for v in temps {
            self.push(Inst::RcDec { val: v });
        }
    }

    // ---- statements ---------------------------------------------------

    fn lower_block(&mut self, stmts: &[Stmt]) -> Result<(), Diag> {
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
                // The local must hold a +1. A borrowed source needs one added;
                // an owned temp is handed straight over, so drop it from the
                // pending list rather than releasing it.
                if self.is_ref(*ty) {
                    if val.owned {
                        self.stmt_temps.retain(|t| *t != val.val());
                    } else {
                        self.push(Inst::RcInc { val: val.val() });
                    }
                    self.owned.last_mut().unwrap().push(name.clone());
                }
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), (*ty, val.val(), *is_const));
                self.flush_temps();
                Ok(())
            }

            Stmt::Assign { name, value, span } => {
                // A bare name inside a method may be a field of the receiver.
                if self.binding(name).is_none() {
                    if let Some((rtid, robj, path)) = self.recv_field(name) {
                        // Walk to the object that actually owns the field.
                        let (owner, owner_tid) = if path.len() == 1 {
                            (robj, rtid)
                        } else {
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
                                self.push(Inst::RcInc { val: v.val() });
                            }
                            self.push(Inst::StoreField {
                                obj,
                                tid,
                                idx,
                                val: v.val(),
                            });
                            self.push(Inst::RcDec { val: old });
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
                    return Err(Diag::new(*span, format!("unknown variable `{name}`")));
                };
                if is_const {
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
                        self.push(Inst::RcInc { val: val.val() });
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
                        self.push(Inst::RcDec { val: old });
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
                            return Err(Diag::new(e.span(), self.mismatch(want, val.ty)));
                        }
                        // Returns are owned (+1). Retain a borrowed value
                        // before releasing locals, or returning a local would
                        // hand back a freed object.
                        if self.is_ref(want) && !val.owned {
                            self.push(Inst::RcInc { val: val.val() });
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

            Stmt::SetIndex {
                obj,
                index,
                value,
                span,
            } => {
                let o = self.lower_expr(obj)?;
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
                        self.push(Inst::RcInc { val: v.val() });
                    }
                    self.push(Inst::Call {
                        dst: None,
                        func: "rt_index_set".to_string(),
                        args: vec![o.val(), i.val(), v.val()],
                    });
                    self.push(Inst::RcDec { val: old });
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
                let o = self.lower_expr(obj)?;
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        obj.span(),
                        format!("type {} has no fields", self.tyname(o.ty)),
                    ));
                };
                let Some((idx, fty)) = self.field_of(tid, field) else {
                    return Err(Diag::new(
                        *span,
                        format!("type `{}` has no field `{field}`", self.tyname(o.ty)),
                    ));
                };
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
                        self.push(Inst::RcInc { val: v.val() });
                    }
                    self.push(Inst::StoreField {
                        obj: o.val(),
                        tid,
                        idx,
                        val: v.val(),
                    });
                    self.push(Inst::RcDec { val: old });
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
                Stmt::ForIn { body, .. } => Self::assigned_names(body, out),
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
            self.push(Inst::RcInc { val: coll.val() });
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
            self.push(Inst::RcInc { val: e });
            self.owned.last_mut().unwrap().push(name.to_string());
        }
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), (ty, e, true));

        self.loops.push(LoopCtx {
            header,
            exit: exit_bb,
            carried: carried.iter().map(|(x, _, _)| x.clone()).collect(),
            depth: self.owned.len() - 1,
        });
        let lowered = self.lower_block(body);
        self.loops.pop();
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
        });
        let lowered = self.lower_block(body);
        self.loops.pop();
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
        let _ = span;
        Ok(())
    }

    /// Structured `if` lowering with block parameters at the join.
    ///
    /// There are no loops in v0, so the CFG is acyclic and SSA construction
    /// needs no fixpoint: take a snapshot of each variable before the branch,
    /// compare after each arm, and give the join a parameter for every
    /// variable the two arms disagree about.
    /// `Type.name(args)` where `name` is a static method: an ordinary direct
    /// call, with no receiver to pass.
    fn lower_static_call(&mut self, key: &str, args: &Args, span: Span) -> Result<Val, Diag> {
        let sig = self.sigs.get(key).expect("checked by the caller");
        let params = sig.params.clone();
        let module = sig.module.clone();
        let ret = sig.ret;
        let slots = self.bind_args(key, &params, args, span)?;
        let mut vals = Vec::new();
        for (a, p) in slots.iter().zip(params.iter()) {
            let v = self.lower_slot(a, p, &module)?;
            if !self.assignable(v.ty, p.ty) {
                return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
            }
            vals.push(v.val());
        }
        if ret == Ty::Void {
            self.push(Inst::Call {
                dst: None,
                func: key.to_string(),
                args: vals,
            });
            return Ok(Val::void());
        }
        let d = self.new_val(self.irty(ret));
        self.push(Inst::Call {
            dst: Some(d),
            func: key.to_string(),
            args: vals,
        });
        let owned = self.is_ref(ret);
        if owned {
            self.stmt_temps.push(d);
        }
        Ok(Val::new(d, ret, owned))
    }

    fn lower_enum_new(
        &mut self,
        ty: Ty,
        variant: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        if matches!(ty, Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Void) {
            return self.lower_prim_static(ty, variant, args, span);
        }
        let Some(tid) = self.tdef_of(ty) else {
            return Err(Diag::new(
                span,
                format!("unknown type `{}`", self.tyname(ty)),
            ));
        };
        // `Type.name(..)` is two things wearing one spelling: an enum
        // variant, and a call to a static method. The variant wins when
        // there is one, because a type cannot have a variant and a static
        // method of the same name -- that is refused where methods are
        // registered.
        let key = format!("{}.{variant}", self.typedefs[tid as usize].name);
        let is_variant = self.typedefs[tid as usize]
            .variants
            .iter()
            .any(|v| v.name == variant);
        if !is_variant && self.statics.contains(&key) {
            return self.lower_static_call(&key, args, span);
        }
        if !self.typedefs[tid as usize].is_enum {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` has no static method `{variant}`, and is not an enum",
                    self.tyname(ty)
                ),
            ));
        }
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "its variants cannot be named from here"),
            ));
        }
        if !args.named.is_empty() {
            return Err(Diag::new(
                span,
                "a variant's payload is positional; it has no field names",
            ));
        }
        let Some(tag) = self.typedefs[tid as usize]
            .variants
            .iter()
            .position(|v| v.name == variant)
        else {
            let known: Vec<&str> = self.typedefs[tid as usize]
                .variants
                .iter()
                .map(|v| v.name.as_str())
                .collect();
            return Err(Diag::new(
                span,
                format!(
                    "`{}` has no variant `{variant}`; it has {}",
                    self.tyname(ty),
                    known.join(", ")
                ),
            ));
        };

        let want = self.variant_surface[tid as usize][tag].clone();
        if args.pos.len() != want.len() {
            return Err(Diag::new(
                span,
                format!(
                    "`{variant}` carries {} value(s), found {}",
                    want.len(),
                    args.pos.len()
                ),
            ));
        }

        let mut vals = Vec::new();
        for (a, w) in args.pos.iter().zip(want.iter()) {
            let v = self.lower_expr_as(a, *w)?;
            if !self.assignable(v.ty, *w) {
                return Err(Diag::new(a.span(), self.mismatch(*w, v.ty)));
            }
            // The enum holds the payload, exactly as a field would: an owned
            // temporary is handed over, a borrowed value is retained.
            if self.is_ref(*w) {
                if v.owned {
                    self.stmt_temps.retain(|t| *t != v.val());
                } else {
                    self.push(Inst::RcInc { val: v.val() });
                }
            }
            vals.push(v.val());
        }

        let d = self.new_val(IrTy::Ref);
        self.push(Inst::EnumPack {
            dst: d,
            tid,
            tag: tag as u32,
            args: vals,
        });
        self.stmt_temps.push(d);
        Ok(Val::new(d, ty, true))
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
            self.push(Inst::RcInc { val: sc.val() });
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
            if a.binds.len() != want.len() {
                return Err(Diag::new(
                    a.span,
                    format!(
                        "`{}` carries {} value(s), and this case binds {}",
                        a.variant,
                        want.len(),
                        a.binds.len()
                    ),
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
            for (idx, b) in a.binds.iter().enumerate() {
                self.check_shadow(&b.name, b.span)?;
                let d = self.new_val(self.irty(b.ty));
                self.push(Inst::EnumPayload {
                    dst: d,
                    obj: sc.val(),
                    tid,
                    idx: idx as u32,
                });
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(b.name.clone(), (b.ty, d, false));
            }

            self.lower_block(&a.body)?;
            let live = !self.terminated();
            if live {
                self.release_scope();
            }
            self.scopes.pop();
            self.owned.pop();
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

    /// `e?` -- give me the value, or return the failure from here.
    ///
    /// Sugar for a `match` whose failing arm returns unchanged:
    ///
    ///     match (e) {
    ///         case Ok(T v):   { v }
    ///         case Err(E x):  { return Result<_, E>.Err(x); }
    ///     }
    ///
    /// It works on an `Option` in a function returning an `Option` too.
    ///
    /// The error types must match EXACTLY. Rust converts via `From`; we have
    /// no such mechanism and inventing one here would be a large feature
    /// hiding inside a small one. Requiring a match is restrictive and
    /// honest, and relaxing it later cannot change what an existing program
    /// means.
    fn lower_try(&mut self, inner: &Expr, span: Span) -> Result<Val, Diag> {
        let v = self.lower_expr(inner)?;

        let Some(vtid) = self
            .tdef_of(v.ty)
            .filter(|t| self.typedefs[*t as usize].is_enum)
        else {
            return Err(Diag::new(
                span,
                format!(
                    "`?` needs an Option or a Result; {} is neither",
                    self.tyname(v.ty)
                ),
            ));
        };
        let vname = self.typedefs[vtid as usize].name.clone();
        let is_result = vname.starts_with("Result$");
        if !is_result && !vname.starts_with("Option$") {
            return Err(Diag::new(
                span,
                format!(
                    "`?` needs an Option or a Result; {} is neither",
                    self.show_name(&vname)
                ),
            ));
        }

        // The enclosing function has to be able to carry the failure out.
        let ret = self.ret_ty;
        let Some(rtid) = self
            .tdef_of(ret)
            .filter(|t| self.typedefs[*t as usize].is_enum)
        else {
            return Err(Diag::new(
                span,
                format!(
                    "`?` can only be used in a function returning an Option or a \
                     Result; this one returns {}",
                    self.tyname(ret)
                ),
            ));
        };
        let rname = self.typedefs[rtid as usize].name.clone();
        if is_result != rname.starts_with("Result$") {
            return Err(Diag::new(
                span,
                format!(
                    "`?` on {} needs a function returning a Result, not {}",
                    a_or_an(&self.show_name(&vname)),
                    self.show_name(&rname)
                ),
            ));
        }

        // Exact error types, for a Result.
        if is_result {
            let ve = self.variant_surface[vtid as usize][1][0];
            let re = self.variant_surface[rtid as usize][1][0];
            if ve != re {
                return Err(Diag::new(
                    span,
                    format!(
                        "`?` needs the same error type on both sides: this fails with \
                         {}, and the function returns {}",
                        self.tyname(ve),
                        self.tyname(re)
                    ),
                ));
            }
        }

        let ok_tag = self.typedefs[vtid as usize]
            .variants
            .iter()
            .position(|x| x.name == "Ok" || x.name == "Some")
            .expect("Option and Result each have a success variant") as u32;
        let payload = self.variant_surface[vtid as usize][ok_tag as usize][0];

        // The scrutinee has to outlive both paths and may be a temporary, so
        // it is held the way `match` holds one.
        self.synth += 1;
        let hold = format!("$try{}", self.synth);
        self.scopes.push(HashMap::new());
        self.owned.push(Vec::new());
        if v.owned {
            self.stmt_temps.retain(|t| *t != v.val());
        } else {
            self.push(Inst::RcInc { val: v.val() });
        }
        self.scopes
            .last_mut()
            .unwrap()
            .insert(hold.clone(), (v.ty, v.val(), true));
        self.owned.last_mut().unwrap().push(hold.clone());

        let tag = self.new_val(IrTy::I64);
        self.push(Inst::EnumTag {
            dst: tag,
            obj: v.val(),
            tid: vtid,
        });
        let k = self.new_val(IrTy::I64);
        self.push(Inst::IConst {
            dst: k,
            val: ok_tag as i64,
        });
        let good = self.new_val(IrTy::I1);
        self.push(Inst::ICmp {
            dst: good,
            cmp: Cmp::Eq,
            lhs: tag,
            rhs: k,
        });

        let ok_bb = self.new_block();
        let bad_bb = self.new_block();
        self.terminate(Term::Brif {
            cond: good,
            then: ok_bb,
            then_args: Vec::new(),
            els: bad_bb,
            els_args: Vec::new(),
        });

        // The failing path: rebuild the failure at the function's own return
        // type and leave. A Result and an Option differ only in whether
        // there is a payload to carry.
        self.switch_to(bad_bb);
        let fail_tag = self.typedefs[rtid as usize]
            .variants
            .iter()
            .position(|x| x.name == "Err" || x.name == "None")
            .expect("Option and Result each have a failure variant") as u32;
        let carried = if is_result {
            let e = self.new_val(self.irty(self.variant_surface[vtid as usize][1][0]));
            self.push(Inst::EnumPayload {
                dst: e,
                obj: v.val(),
                tid: vtid,
                idx: 0,
            });
            // Borrowed from the value we are about to release, so the new
            // failure takes a reference of its own.
            if self.is_ref(self.variant_surface[vtid as usize][1][0]) {
                self.push(Inst::RcInc { val: e });
            }
            Some(e)
        } else {
            None
        };
        let out = self.make_option(rtid, fail_tag, carried);
        self.stmt_temps.retain(|t| *t != out);
        // The early return has to release what the statement has allocated
        // so far -- but only on THIS path. `flush_temps` empties the pending
        // list, and the success path continues in the same statement and
        // still needs it: without restoring it, every temporary created
        // before the `?` leaked whenever the value was Ok.
        let pending = self.stmt_temps.clone();
        self.flush_temps();
        self.release_all();
        self.terminate(Term::Ret { val: Some(out) });
        self.stmt_temps = pending;

        // The succeeding path: the payload, retained because it is borrowed
        // from a value whose scope ends here.
        self.switch_to(ok_bb);
        let got = self.new_val(self.irty(payload));
        self.push(Inst::EnumPayload {
            dst: got,
            obj: v.val(),
            tid: vtid,
            idx: 0,
        });
        let owned = self.is_ref(payload);
        if owned {
            self.push(Inst::RcInc { val: got });
        }
        self.release_scope();
        self.scopes.pop();
        self.owned.pop();
        if owned {
            self.stmt_temps.push(got);
        }
        Ok(Val::new(got, payload, owned))
    }

    /// `v.to_str()`, for `print` and for `str(v)`.
    ///
    /// Found BY NAME, the way `add`, `eq` and `cmp` already are. There is no
    /// blessed `ToStr` type, because none is needed: interfaces here are
    /// structural, so a program that wants to pass "anything printable"
    /// around declares `interface ToStr { str to_str(); }` itself and every
    /// type with the method satisfies it with no further ceremony. Blessing
    /// one would buy nothing and freeze a name.
    ///
    /// Returns None when the type has no such method, so the caller can give
    /// a diagnostic that fits what it was doing.
    fn call_to_str(&mut self, v: &Val, span: Span) -> Result<Option<Val>, Diag> {
        let Some(tid) = self.tdef_of(v.ty) else {
            return Ok(None);
        };
        let tname = self.typedefs[tid as usize].name.clone();

        // On an interface value the implementation is not known statically.
        if self.typedefs[tid as usize].is_interface {
            let Some(decl) = self.iface_methods[tid as usize]
                .iter()
                .find(|x| x.name == "to_str")
                .cloned()
            else {
                return Ok(None);
            };
            if decl.ret != Ty::Str || !decl.params.is_empty() {
                return Ok(None);
            }
            // By name AND shape: the slot has to be the one this
            // interface's declaration occupies, not merely one that shares
            // the name.
            let want = self.slot_of(&decl);
            let slot = self
                .iface_slots
                .iter()
                .position(|x| *x == want)
                .expect("every interface method has a slot") as u32;
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::CallIface {
                dst: Some(d),
                slot,
                name: "to_str".to_string(),
                args: vec![v.val()],
                ret: Some(IrTy::Ref),
            });
            self.stmt_temps.push(d);
            return Ok(Some(Val::new(d, Ty::Str, true)));
        }

        let key = format!("{tname}.to_str");
        if !self.sigs.contains_key(&key) {
            return Ok(None);
        }
        self.check_method_access(tid, "to_str", span)?;
        let sig = &self.sigs[&key];
        if sig.ret != Ty::Str || !sig.params.is_empty() {
            return Err(Diag::new(
                span,
                format!("`{key}` must take no arguments and return str to be used here"),
            ));
        }
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Call {
            dst: Some(d),
            func: key,
            args: vec![v.val()],
        });
        self.stmt_temps.push(d);
        Ok(Some(Val::new(d, Ty::Str, true)))
    }

    /// The key a bare name has in `sigs`: this module's own declaration if
    /// there is one, otherwise the name as written -- which is how builtins
    /// and the prelude stay reachable from everywhere.
    fn resolve_fn(&self, name: &str) -> String {
        if !self.cur_module.is_empty() {
            let qualified = format!("{}#{name}", self.cur_module);
            if self.sigs.contains_key(&qualified) {
                return qualified;
            }
        }
        name.to_string()
    }

    /// `mod.name(args)` -- a call into another module.
    fn lower_qualified(
        &mut self,
        modname: &str,
        name: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
        let key = format!("{modname}#{name}");
        // A generic function arrives already instantiated, as `first$int`
        // (see mono.rs); the reader wrote `first`, so that is what a
        // diagnostic names. `$` never appears in a source identifier.
        let name = name.split('$').next().unwrap_or(name);
        // `lib.Point(3, 4)` -- a construction, not a call. The parser cannot
        // tell the two apart, because it never sees another module's
        // declarations, so it is settled here where both tables are known.
        // A type of that name exists only if `lib` declared one: the name is
        // module-qualified, so nothing here can find a type from elsewhere.
        if !self.sigs.contains_key(&key) {
            if let Some(ty) = self.ty_named(&key) {
                return self.lower_new(ty, args, span);
            }
        }
        let Some(sig) = self.sigs.get(&key) else {
            return Err(Diag::new(
                span,
                format!("`{modname}` has no function `{name}`"),
            ));
        };
        if sig.module != modname {
            return Err(Diag::new(
                span,
                format!("`{name}` is not declared in `{modname}`"),
            ));
        }
        if !sig.is_pub {
            return Err(Diag::new(
                span,
                format!("`{name}` is private to `{modname}`; mark it `pub` to export it"),
            ));
        }
        self.lower_call(&key, args, span)
    }

    /// Lower one argument of a call or a construction, as `bind_args` slotted
    /// it.
    ///
    /// An argument the caller wrote is the caller's expression. A DEFAULT is
    /// the declaration's, and means what it meant where it was written: it
    /// is lowered with the declaring module's names and privacy, and sees no
    /// local and no receiver field. It used to be lowered as if the caller
    /// had written it -- so a public type whose field defaulted to a private
    /// one could not be constructed outside its module, a bare call in a
    /// default resolved against the caller's module, and a default naming
    /// `y` read whatever the caller happened to call `y`.
    fn lower_slot(&mut self, a: &Expr, p: &Param, module: &str) -> Result<Val, Diag> {
        let is_default = p.default.as_ref().is_some_and(|d| std::ptr::eq(d, a));
        if !is_default {
            return self.lower_expr_as(a, p.ty);
        }
        let module = if module.is_empty() {
            self.cur_module.clone()
        } else {
            module.to_string()
        };
        let saved_module = std::mem::replace(&mut self.cur_module, module.clone());
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        let saved_recv = self.recv.take();
        let r = self.lower_expr_as(a, p.ty).and_then(|v| {
            if self.assignable(v.ty, p.ty) {
                Ok(v)
            } else {
                Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)))
            }
        });
        self.cur_module = saved_module;
        self.scopes = saved_scopes;
        self.recv = saved_recv;
        // Its span is a line of the declaring file, so the error is too.
        r.map_err(|d| d.in_module(&module))
    }

    /// Lower an expression where the wanted type is known.
    ///
    /// Collection literals have no type of their own -- `[]` says nothing --
    /// so they are legal only where something says what they should be: a
    /// declaration, an assignment, a field, an argument, an enum payload, a
    /// return. Everywhere else the literal forms are refused with a message
    /// saying so, rather than guessing.
    fn lower_expr_as(&mut self, e: &Expr, want: Ty) -> Result<Val, Diag> {
        match e {
            Expr::SeqLit(..) | Expr::RepeatLit(..) | Expr::MapLit(..) => {
                self.lower_literal(e, Some(want))
            }
            _ => self.lower_expr(e),
        }
    }

    /// `[]`, `[a, b, c]`, `[x; n]`, `{}`, `{k: v}`.
    fn lower_literal(&mut self, e: &Expr, want: Option<Ty>) -> Result<Val, Diag> {
        let span = e.span();
        let Some(want) = want else {
            return Err(Diag::new(
                span,
                "there is nothing here to say what this should be; a collection \
                 literal takes its type from where it is written",
            ));
        };

        // A literal written where a distinct collection is wanted builds the
        // base and takes the distinct identity. That is not the implicit
        // conversion `Price p = 5` is refused for: `5` already has a type,
        // `int`, and would have to change it, while a collection literal has
        // no type at all until the place it is written gives it one.
        if let Some(base) = self.base_of(want) {
            let v = self.lower_literal(e, Some(base))?;
            return Ok(Val::new(v.val(), want, v.owned));
        }

        // A Map wants `{..}`; an Array or List wants `[..]`.
        if let Some((k, v)) = self.map_kv(want) {
            let Expr::MapLit(items, _) = e else {
                return Err(Diag::new(
                    span,
                    format!(
                        "{} is a map; write its entries as {{k: v}}",
                        self.tyname(want)
                    ),
                ));
            };
            self.building_literal = true;
            let m = self.lower_new(want, &Args::default(), span);
            self.building_literal = false;
            let m = m?;
            for (ke, ve) in items {
                let kv = self.lower_expr_as(ke, k)?;
                if !self.assignable(kv.ty, k) {
                    return Err(Diag::new(ke.span(), self.mismatch(k, kv.ty)));
                }
                let vv = self.lower_expr_as(ve, v)?;
                if !self.assignable(vv.ty, v) {
                    return Err(Diag::new(ve.span(), self.mismatch(v, vv.ty)));
                }
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_map_set".to_string(),
                    args: vec![m.val(), kv.val(), vv.val()],
                });
            }
            return Ok(m);
        }

        if want == Ty::Bytes {
            return self.bytes_literal(e);
        }

        let Some(elem) = self.seq_elem(want) else {
            return Err(Diag::new(
                span,
                format!("{} is not a collection", self.tyname(want)),
            ));
        };
        if matches!(e, Expr::MapLit(..)) {
            return Err(Diag::new(
                span,
                format!("{} holds elements; write them as [a, b]", self.tyname(want)),
            ));
        }
        let is_list = self.is_list(want);

        // `[x; n]` -- n copies, built at that length by the runtime for
        // either kind. A List used to be filled by pushing in a loop, which
        // made a negative `n` an empty list and a huge one a process that
        // pushed until it was killed; one call checks `n` the way an Array
        // does, and sizes the buffer once.
        if let Expr::RepeatLit(ve, ne, _) = e {
            let v = self.lower_expr_as(ve, elem)?;
            if !self.assignable(v.ty, elem) {
                return Err(Diag::new(ve.span(), self.mismatch(elem, v.ty)));
            }
            let n = self.lower_expr(ne)?;
            if self.underlying(n.ty) != Ty::Int {
                return Err(Diag::new(ne.span(), self.mismatch(Ty::Int, n.ty)));
            }
            return Ok(self.build_repeat(want, elem, v, n.val(), is_list));
        }

        let Expr::SeqLit(items, _) = e else {
            unreachable!("only the three literal forms reach here")
        };
        let mut vals = Vec::new();
        for it in items {
            let v = self.lower_expr_as(it, elem)?;
            if !self.assignable(v.ty, elem) {
                return Err(Diag::new(it.span(), self.mismatch(elem, v.ty)));
            }
            vals.push(v);
        }
        self.build_seq(want, elem, vals, is_list, span)
    }

    /// `[]`, `[104, 105]` and `[0; n]` where a `bytes` is wanted.
    ///
    /// The literal is the List literal, because a `bytes` is written the way
    /// a sequence of ints is: there is no second spelling to learn, and every
    /// place that gives a List literal its type gives this one its type too.
    /// There is no `b"..."`. A literal of a MUTABLE type cannot be one shared
    /// immortal object the way a string literal is, so it would allocate
    /// every time it is evaluated while looking like a constant; text that
    /// should become bytes says so, `"GET ".to_bytes()`, and the allocation
    /// is visible where it happens.
    fn bytes_literal(&mut self, e: &Expr) -> Result<Val, Diag> {
        match e {
            Expr::MapLit(_, span) => {
                Err(Diag::new(*span, "bytes holds octets; write them as [a, b]"))
            }
            Expr::RepeatLit(ve, ne, _) => {
                self.check_byte_literal(ve)?;
                let v = self.arg_of(ve, Ty::Int)?;
                let n = self.lower_expr(ne)?;
                if self.underlying(n.ty) != Ty::Int {
                    return Err(Diag::new(ne.span(), self.mismatch(Ty::Int, n.ty)));
                }
                Ok(self.rt_value("rt_bytes_fill", vec![n.val(), v], Ty::Bytes))
            }
            Expr::SeqLit(items, _) => {
                // Sized once for what is written, then filled; each push
                // checks its value, so a computed element out of range traps
                // exactly as `b.push(v)` would.
                let cap = self.new_val(IrTy::I64);
                self.push(Inst::IConst {
                    dst: cap,
                    val: items.len() as i64,
                });
                let b = self.rt_value("rt_bytes_new", vec![cap], Ty::Bytes);
                for it in items {
                    self.check_byte_literal(it)?;
                    let v = self.arg_of(it, Ty::Int)?;
                    self.rt_void("rt_bytes_push", vec![b.val(), v]);
                }
                Ok(b)
            }
            _ => unreachable!("only the three literal forms reach here"),
        }
    }

    /// A List or an Array of `n` copies of `fill`, straight from the
    /// runtime, which retains a reference fill once per slot.
    fn build_repeat(&mut self, ty: Ty, elem: Ty, fill: Val, n: Value, is_list: bool) -> Val {
        let flag = self.new_val(IrTy::I1);
        let refs = self.is_ref(elem);
        self.push(Inst::BConst {
            dst: flag,
            val: refs,
        });
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Call {
            dst: Some(d),
            func: if is_list {
                "rt_list_repeat"
            } else {
                "rt_array_new"
            }
            .to_string(),
            args: vec![n, fill.val(), flag],
        });
        self.stmt_temps.push(d);
        Val::new(d, ty, true)
    }

    /// `[a, b, c]` for a List or an Array. An Array is sized by its elements.
    fn build_seq(
        &mut self,
        ty: Ty,
        elem: Ty,
        vals: Vec<Val>,
        is_list: bool,
        span: Span,
    ) -> Result<Val, Diag> {
        let refs = self.is_ref(elem);
        let d = self.new_val(IrTy::Ref);
        if is_list {
            let flag = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: flag,
                val: refs,
            });
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_list_new".to_string(),
                args: vec![flag],
            });
        } else {
            // An array is allocated at its length with no fill to retain,
            // then each slot is written.
            let n = self.new_val(IrTy::I64);
            self.push(Inst::IConst {
                dst: n,
                val: vals.len() as i64,
            });
            let flag = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: flag,
                val: refs,
            });
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_array_blank".to_string(),
                args: vec![n, flag],
            });
        }
        self.stmt_temps.push(d);
        let out = Val::new(d, ty, true);

        for (i, v) in vals.iter().enumerate() {
            // The collection takes a reference, exactly as push or an index
            // assignment would.
            if refs {
                if v.owned {
                    self.stmt_temps.retain(|t| *t != v.val());
                } else {
                    self.push(Inst::RcInc { val: v.val() });
                }
            }
            if is_list {
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_list_push".to_string(),
                    args: vec![d, v.val()],
                });
            } else {
                let idx = self.new_val(IrTy::I64);
                self.push(Inst::IConst {
                    dst: idx,
                    val: i as i64,
                });
                self.push(Inst::Call {
                    dst: None,
                    func: "rt_array_put".to_string(),
                    args: vec![d, idx, v.val()],
                });
            }
        }
        let _ = span;
        Ok(out)
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

    // ---- expressions ---------------------------------------------------

    fn lower_expr(&mut self, e: &Expr) -> Result<Val, Diag> {
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
                    let (d, fty) = self.load_path(tid, obj, &path);
                    // Borrowed from the receiver, which holds the +1.
                    return Ok(Val::new(d, fty, false));
                }
                Err(Diag::new(*span, format!("unknown variable `{name}`")))
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
                                format!("`{name}` is declared in `{owner}`; write `{owner}.{name}`")
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
                                    "`{name}` is declared in `{}`; write `{}.{name}`",
                                    sig.module, sig.module
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
                    if self.modules.contains(modname) && self.lookup(modname).is_none() {
                        return self.lower_qualified(modname, m, args, *span);
                    }
                }
                let o = self.lower_expr(obj)?;
                // `str` is not a declared type, so it has no entry in the
                // type table -- but it still answers `size()`, because one
                // rule for asking how big a thing is beats a free function
                // for strings and a method for everything else.
                if self.underlying(o.ty) == Ty::Str {
                    return self.lower_str_method(&o, m, args, *span);
                }
                if self.underlying(o.ty) == Ty::Bytes {
                    return self.lower_bytes_method(&o, m, args, *span);
                }
                if matches!(self.underlying(o.ty), Ty::Int | Ty::Float | Ty::Bool) {
                    return self.lower_prim_method(&o, m, args, *span);
                }
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("type {} has no methods", self.tyname(o.ty)),
                    ));
                };
                // Collections have built-in methods, typed against their
                // element type rather than declared anywhere. A distinct
                // collection has them too, but a method it declares itself
                // comes first -- the same rule as a real method shadowing an
                // embedded one.
                let own = format!("{}.{m}", self.typedefs[tid as usize].name);
                if !self.sigs.contains_key(&own) {
                    if let Some(elem) = self.seq_elem(o.ty) {
                        return self.lower_seq_method(&o, elem, m, args, *span);
                    }
                    if let Some((k, v)) = self.map_kv(o.ty) {
                        return self.lower_map_method(&o, k, v, m, args, *span);
                    }
                }
                if self.typedefs[tid as usize].is_enum
                    && self.typedefs[tid as usize].name.starts_with("Option$")
                {
                    return self.lower_option_method(&o, tid, m, args, *span);
                }
                self.check_method_access(tid, m, *span)?;
                // A static method has no receiver, so it cannot be reached
                // through a value even though the spelling looks the same.
                let skey = format!("{}.{m}", self.typedefs[tid as usize].name);
                if self.statics.contains(&skey) {
                    return Err(Diag::new(
                        *span,
                        format!(
                            "`{m}` is a static method; call it on the type, as \
                             `{}.{m}(..)`",
                            self.show_name(&self.typedefs[tid as usize].name)
                        ),
                    ));
                }

                // On an interface value the implementation is not known
                // statically: dispatch through the receiver's type header.
                if self.typedefs[tid as usize].is_interface {
                    let iname = self.show_name(&self.typedefs[tid as usize].name);
                    let Some(decl) = self.iface_methods[tid as usize]
                        .iter()
                        .find(|x| x.name == *m)
                        .cloned()
                    else {
                        return Err(Diag::new(
                            *span,
                            format!("interface `{iname}` has no method `{m}`"),
                        ));
                    };
                    let want = self.slot_of(&decl);
                    let slot = self
                        .iface_slots
                        .iter()
                        .position(|x| *x == want)
                        .expect("every interface method has a slot")
                        as u32;

                    let slots =
                        self.bind_args(&format!("{iname}.{m}"), &decl.params, args, *span)?;
                    let mut vals = vec![o.val()];
                    for (a, p) in slots.iter().zip(decl.params.iter()) {
                        let v = self.lower_expr_as(a, p.ty)?;
                        if !self.assignable(v.ty, p.ty) {
                            return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
                        }
                        vals.push(v.val());
                    }

                    if decl.ret == Ty::Void {
                        self.push(Inst::CallIface {
                            dst: None,
                            slot,
                            name: m.clone(),
                            args: vals,
                            ret: None,
                        });
                        return Ok(Val::void());
                    }
                    let d = self.new_val(self.irty(decl.ret));
                    self.push(Inst::CallIface {
                        dst: Some(d),
                        slot,
                        name: m.clone(),
                        args: vals,
                        ret: Some(self.irty(decl.ret)),
                    });
                    let owned = self.is_ref(decl.ret);
                    if owned {
                        self.stmt_temps.push(d);
                    }
                    return Ok(Val::new(d, decl.ret, owned));
                }

                let key = format!("{}.{m}", self.typedefs[tid as usize].name);
                if self.generic_methods.contains(&key) {
                    return Err(Diag::new(
                        *span,
                        format!(
                            "`{m}` is a generic method, and its type arguments are \
                             inferred where the receiver's type is written down; \
                             call it on a local, a parameter, a field or a \
                             construction"
                        ),
                    ));
                }
                let Some(sig) = self.sigs.get(&key) else {
                    return Err(Diag::new(
                        *span,
                        format!(
                            "type `{}` has no method `{m}`",
                            self.show_name(&self.typedefs[tid as usize].name)
                        ),
                    ));
                };
                let params = sig.params.clone();
                let module = sig.module.clone();
                let ret = sig.ret;
                let slots = self.bind_args(&key, &params, args, *span)?;

                // The receiver is the hidden first argument, and is borrowed
                // like every other argument (docs/ir-v0.md §5.1).
                let mut vals = vec![o.val()];
                for (a, p) in slots.iter().zip(params.iter()) {
                    let v = self.lower_slot(a, p, &module)?;
                    if !self.assignable(v.ty, p.ty) {
                        return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
                    }
                    vals.push(v.val());
                }

                if ret == Ty::Void {
                    self.push(Inst::Call {
                        dst: None,
                        func: key,
                        args: vals,
                    });
                    Ok(Val::void())
                } else {
                    let d = self.new_val(self.irty(ret));
                    self.push(Inst::Call {
                        dst: Some(d),
                        func: key,
                        args: vals,
                    });
                    let owned = self.is_ref(ret);
                    if owned {
                        self.stmt_temps.push(d);
                    }
                    Ok(Val::new(d, ret, owned))
                }
            }

            Expr::Index(obj, idx, span) => {
                let o = self.lower_expr(obj)?;
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

    /// A method on `int`, `float` or `bool`.
    ///
    /// `to_str` on all three, so that `v.to_str()` means the same thing
    /// whatever `v` is. Beyond that only what cannot be written in the
    /// language itself: wrapping arithmetic, which the checked operators
    /// refuse by design, and a float's bit pattern, which no arithmetic
    /// reaches. Everything else a number might answer belongs in a library.
    fn lower_prim_method(
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
        let func = match prim {
            Ty::Int => "rt_int_to_str",
            Ty::Float => "rt_float_to_str",
            _ => "rt_bool_to_str",
        };
        let d = self.new_val(IrTy::Ref);
        self.push(Inst::Call {
            dst: Some(d),
            func: func.to_string(),
            args: vec![o.val()],
        });
        self.stmt_temps.push(d);
        Ok(Val::new(d, Ty::Str, true))
    }

    /// `T.name(..)` where `T` is a built-in type: a static method.
    ///
    /// There is one, `float.from_bits(n)`. It is static rather than a method
    /// on `int` for the reason §6.6 gives for parsing: the source is always
    /// an `int` and it is the TARGET that the name has to say, which a method
    /// dispatched on the source cannot.
    fn lower_prim_static(
        &mut self,
        ty: Ty,
        name: &str,
        args: &Args,
        span: Span,
    ) -> Result<Val, Diag> {
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

    /// The method an operator desugars to, and whether the result is negated.
    ///
    /// Comparison goes through a single `cmp` returning an int, rather than
    /// four separate methods: one implementation gives a total order, and it
    /// cannot be made inconsistent by defining `<` and `>=` differently.
    fn op_method(op: BinOp) -> Option<(&'static str, bool)> {
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
    fn lower_op_overload(&mut self, op: BinOp, a: &Val, b: &Val, span: Span) -> Result<Val, Diag> {
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

    fn lower_bin(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> Result<Val, Diag> {
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

        let a = self.lower_expr(l)?;
        let b = self.lower_expr(r)?;

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
    fn lower_bin_prim(
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

    /// The surface type of a field, recovered from the declaration.
    fn field_ty(&self, tid: u32, idx: u32) -> Ty {
        self.field_surface[tid as usize][idx as usize]
    }

    fn field_tyname(&self, tid: u32, idx: u32) -> String {
        self.tyname(self.field_ty(tid, idx))
    }

    fn lower_new(&mut self, ty: Ty, args: &Args, span: Span) -> Result<Val, Diag> {
        let Some(tid) = self.tdef_of(ty) else {
            return Err(Diag::new(
                span,
                format!("unknown type `{}`", self.tyname(ty)),
            ));
        };
        // Only a qualified construction, `lib.Secret(..)`, can name a type
        // from another module here, so this is where its privacy is kept.
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "it cannot be constructed from here"),
            ));
        }
        if self.typedefs[tid as usize].is_interface {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` is an interface; construct a type that satisfies it",
                    self.show_name(&self.typedefs[tid as usize].name)
                ),
            ));
        }
        if self.typedefs[tid as usize].is_distinct {
            // `Price(100)` is a CONVERSION, not a construction: same
            // representation, different identity, nothing emitted.
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{}` converts one value; it is a distinct type, not a struct",
                        self.show_name(&self.typedefs[tid as usize].name)
                    ),
                ));
            }
            // The base says what a literal argument should be, so
            // `Bag([1, 2])` reads the same as `Bag b = [1, 2]`.
            let base = self.base_of(ty).expect("a distinct type has a base");
            let v = self.lower_expr_as(&args.pos[0], base)?;
            if self.underlying(v.ty) != self.underlying(ty) {
                return Err(Diag::new(
                    args.pos[0].span(),
                    self.mismatch(self.underlying(ty), v.ty),
                ));
            }
            return Ok(Val::new(v.val(), ty, v.owned));
        }
        let tname = self.typedefs[tid as usize].name.clone();
        // `List<int>(bag)` -- converting a distinct collection back to its
        // base, spelled as the base type the way `int(price)` is. A
        // collection is otherwise never constructed by name, so one argument
        // of a distinct type over exactly this collection is unambiguous.
        if ["Array$", "List$", "Map$"]
            .iter()
            .any(|p| tname.starts_with(p))
            && !self.building_literal
            && args.pos.len() == 1
            && args.named.is_empty()
        {
            let v = self.lower_expr_as(&args.pos[0], ty)?;
            if self.base_of(v.ty).is_some() && self.underlying(v.ty) == ty {
                return Ok(Val::new(v.val(), ty, v.owned));
            }
            // Anything else falls through to the paths below, every one of
            // which refuses a single positional argument with the message it
            // always gave -- before lowering any argument, so nothing is
            // evaluated twice.
        }
        if tname.starts_with("Map$") && !self.building_literal {
            // The key type is still checked, because `{}` reaches this path
            // with the flag set and a bad key type must be caught either way.
            let (k, _) = self.map_kv(ty).expect("a map has a key and a value");
            let ku = self.underlying(k);
            if ku == Ty::Int || ku == Ty::Str {
                return Err(Diag::new(
                    span,
                    format!(
                        "write {} as a literal: `{{k: v}}` or `{{}}`",
                        a_or_an(&self.tyname(ty))
                    ),
                ));
            }
        }
        if tname.starts_with("Map$") {
            let (k, v) = self.map_kv(ty).expect("a map has a key and a value");
            let ku = self.underlying(k);
            if ku != Ty::Int && ku != Ty::Str {
                return Err(Diag::new(
                    span,
                    format!(
                        "a map key must be int or str, found {}; hashing a user \
                         type would need a Hashable interface, which does not exist yet",
                        self.tyname(k)
                    ),
                ));
            }
            if !args.pos.is_empty() || !args.named.is_empty() {
                return Err(Diag::new(span, "a map takes no arguments"));
            }
            let mut flags = Vec::new();
            for b in [ku == Ty::Str, self.is_ref(k), self.is_ref(v)] {
                let f = self.new_val(IrTy::I1);
                self.push(Inst::BConst { dst: f, val: b });
                flags.push(f);
            }
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_map_new".to_string(),
                args: flags,
            });
            self.stmt_temps.push(d);
            return Ok(Val::new(d, ty, true));
        }
        // `List<int>()` and friends are gone: a collection is written as a
        // literal, and having both spellings would be two ways to say one
        // thing. `lower_literal` builds them now; this path is only reached
        // when someone writes the old form.
        if (tname.starts_with("Array$") || tname.starts_with("List$")) && !self.building_literal {
            return Err(Diag::new(
                span,
                format!(
                    "write {} as a literal: `[a, b]`, `[x; n]`, or `[]`",
                    a_or_an(&self.tyname(ty))
                ),
            ));
        }
        if tname.starts_with("Array$") || tname.starts_with("List$") {
            let elem = self.seq_elem(ty).expect("collection has an element type");
            let refs = self.is_ref(elem);
            let is_lst = tname.starts_with("List$");
            let want = if is_lst { 0 } else { 2 };
            if args.pos.len() != want || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    if is_lst {
                        "a list takes no arguments".to_string()
                    } else {
                        "an array takes two arguments: its length and the value \
                         every element starts at"
                            .to_string()
                    },
                ));
            }
            let mut a = Vec::new();
            if !is_lst {
                let n = self.lower_expr(&args.pos[0])?;
                if self.underlying(n.ty) != Ty::Int {
                    return Err(Diag::new(args.pos[0].span(), self.mismatch(Ty::Int, n.ty)));
                }
                let fill = self.lower_expr(&args.pos[1])?;
                if !self.assignable(fill.ty, elem) {
                    return Err(Diag::new(args.pos[1].span(), self.mismatch(elem, fill.ty)));
                }
                // The array retains the fill once per element, in the
                // runtime, so an owned temporary here is still released by
                // the statement as usual.
                a.push(n.val());
                a.push(fill.val());
            }
            let flag = self.new_val(IrTy::I1);
            self.push(Inst::BConst {
                dst: flag,
                val: refs,
            });
            a.push(flag);
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Call {
                dst: Some(d),
                func: if is_lst {
                    "rt_list_new"
                } else {
                    "rt_array_new"
                }
                .to_string(),
                args: a,
            });
            self.stmt_temps.push(d);
            return Ok(Val::new(d, ty, true));
        }
        if self.typedefs[tid as usize].is_chan {
            // `Chan<int>(8)` -- one positional argument, the capacity.
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    "a channel takes one argument: its capacity".to_string(),
                ));
            }
            let cap = self.lower_expr(&args.pos[0])?;
            if cap.ty != Ty::Int {
                return Err(Diag::new(
                    args.pos[0].span(),
                    self.mismatch(Ty::Int, cap.ty),
                ));
            }
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Call {
                dst: Some(d),
                func: "rt_chan_new".to_string(),
                args: vec![cap.val()],
            });
            self.stmt_temps.push(d);
            return Ok(Val::new(d, ty, true));
        }
        let name = self.typedefs[tid as usize].name.clone();
        let name = name.as_str();
        let fields = self.field_params[tid as usize].clone();
        let module = self.type_module[tid as usize].clone();
        let slots = self.bind_args(name, &fields, args, span)?;

        let mut given: Vec<Option<Val>> = Vec::new();
        for (e, f) in slots.iter().zip(fields.iter()) {
            // The field's type is what a literal argument takes. A default
            // is an expression lowered here, at each construction, so a
            // literal default builds a fresh collection for every object
            // rather than one shared by all of them.
            let v = self.lower_slot(e, f, &module)?;
            if !self.assignable(v.ty, f.ty) {
                return Err(Diag::new(
                    e.span(),
                    format!(
                        "type mismatch: field `{}` is {}, found {}",
                        f.name,
                        self.tyname(f.ty),
                        self.tyname(v.ty)
                    ),
                ));
            }
            given.push(Some(v));
        }

        let obj = self.new_val(IrTy::Ref);
        self.push(Inst::Alloc { dst: obj, tid });
        for (i, g) in given.into_iter().enumerate() {
            let v = g.unwrap();
            // The object takes a +1 on every reference field. An owned
            // temporary is handed straight over; a borrowed one is retained.
            if self.is_ref(v.ty) {
                if v.owned {
                    self.stmt_temps.retain(|t| *t != v.val());
                } else {
                    self.push(Inst::RcInc { val: v.val() });
                }
            }
            self.push(Inst::StoreField {
                obj,
                tid,
                idx: i as u32,
                val: v.val(),
            });
        }
        self.stmt_temps.push(obj);
        Ok(Val::new(obj, ty, true))
    }

    fn lower_call(&mut self, name: &str, args: &Args, span: Span) -> Result<Val, Diag> {
        // `print` accepts int, bool or str and selects the runtime helper from
        // the static argument type. Not user-visible overloading.
        if name == "print" {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(
                    span,
                    format!(
                        "`print` takes 1 argument, found {}",
                        args.pos.len() + args.named.len()
                    ),
                ));
            }
            let a = self.lower_expr(&args.pos[0])?;
            let f = match self.underlying(a.ty) {
                Ty::Int => "rt_print",
                Ty::Float => "rt_print_float",
                Ty::Bool => "rt_print_bool",
                Ty::Str => "rt_print_str",
                Ty::Void => return Err(Diag::new(args.pos[0].span(), "cannot print a void value")),
                // `print(v)` is `v.to_str()`, and `bytes` has none: which
                // text a run of octets is -- hex, decoded UTF-8, escaped --
                // is the program's choice, not print's.
                Ty::Bytes => {
                    return Err(Diag::new(
                        args.pos[0].span(),
                        "cannot print `bytes`; say which text you mean, \
                         `hex()` or `utf8()`",
                    ))
                }
                // A user type says how it prints by having a `to_str`
                // method -- found by name, like `add` and `cmp`. Without
                // one, a refusal naming the method beats printing an
                // address.
                Ty::User(_) => {
                    let Some(text) = self.call_to_str(&a, args.pos[0].span())? else {
                        return Err(Diag::new(
                            args.pos[0].span(),
                            format!(
                                "cannot print a value of type `{}`; give it a \
                                 method `str {}.to_str()`",
                                self.tyname(a.ty),
                                self.tyname(a.ty)
                            ),
                        ));
                    };
                    self.push(Inst::Call {
                        dst: None,
                        func: "rt_print_str".to_string(),
                        args: vec![text.val()],
                    });
                    return Ok(Val::void());
                }
            };
            self.push(Inst::Call {
                dst: None,
                func: f.to_string(),
                args: vec![a.val()],
            });
            return Ok(Val::void());
        }

        // `int(x)` and friends: convert a distinct value back to its base.
        // Same representation, so nothing is emitted.
        // `int(x)`, `float(x)`, `bool(x)`, `str(x)`.
        //
        // Two jobs behind one spelling: unwrapping a distinct type back to
        // its base, which is free and changes only identity, and converting
        // between the two number types, which is a real conversion. The
        // argument is lowered ONCE and then dispatched on, because lowering
        // it in each branch would evaluate `int(f())` twice.
        if let Some(base) = match name {
            "int" => Some(Ty::Int),
            "float" => Some(Ty::Float),
            "bool" => Some(Ty::Bool),
            "str" => Some(Ty::Str),
            "bytes" => Some(Ty::Bytes),
            _ => None,
        } {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(span, format!("`{name}` converts one value")));
            }
            let v = self.lower_expr(&args.pos[0])?;
            let from = self.underlying(v.ty);

            // Same representation: nothing is emitted.
            if from == base {
                return Ok(Val::new(v.val(), base, v.owned));
            }

            // Nothing here is implicit, so a mixed expression stays an error
            // the writer resolves rather than a silent widening.
            if base == Ty::Float && from == Ty::Int {
                let d = self.new_val(IrTy::F64);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_i2f_val".to_string(),
                    args: vec![v.val()],
                });
                return Ok(Val::new(d, Ty::Float, false));
            }
            // `str(v)` is `v.to_str()`, so the conversion family reads the
            // same whatever it is applied to -- a number, a bool, or a user
            // type that wrote the method itself.
            if base == Ty::Str {
                if matches!(from, Ty::Int | Ty::Float | Ty::Bool) {
                    let func = match from {
                        Ty::Int => "rt_int_to_str",
                        Ty::Float => "rt_float_to_str",
                        _ => "rt_bool_to_str",
                    };
                    let d = self.new_val(IrTy::Ref);
                    self.push(Inst::Call {
                        dst: Some(d),
                        func: func.to_string(),
                        args: vec![v.val()],
                    });
                    self.stmt_temps.push(d);
                    return Ok(Val::new(d, Ty::Str, true));
                }
                if from == Ty::Bytes {
                    return Err(Diag::new(
                        args.pos[0].span(),
                        "`str(..)` is `to_str`, which `bytes` does not have; \
                         say which text you mean, `hex()` or `utf8()`",
                    ));
                }
                if let Some(text) = self.call_to_str(&v, args.pos[0].span())? {
                    return Ok(text);
                }
            }
            if base == Ty::Int && from == Ty::Float {
                // The C cast is UNDEFINED for a NaN or for a value outside
                // the integer range -- exactly the sort of thing gcc and
                // clang disagree about at -O2. Truncate toward zero, and
                // trap rather than take whatever the hardware felt like.
                let d = self.new_val(IrTy::I64);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_f2i_checked".to_string(),
                    args: vec![v.val()],
                });
                return Ok(Val::new(d, Ty::Int, false));
            }
            return Err(Diag::new(args.pos[0].span(), self.mismatch(base, v.ty)));
        }

        // `clone(x)` -- a SHALLOW copy. We chose reference types, so `=`
        // aliases; this is the explicit way to get a second object. Shallow
        // because a deep copy would have to decide what copying each field
        // means, which is a question only the program can answer.
        if name == "clone" {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(span, "`clone` takes one argument"));
            }
            let v = self.lower_expr(&args.pos[0])?;
            if self.seq_elem(v.ty).is_some() {
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_seq_clone".to_string(),
                    args: vec![v.val()],
                });
                self.stmt_temps.push(d);
                return Ok(Val::new(d, v.ty, true));
            }
            if self.underlying(v.ty) == Ty::Str {
                // Immutable, so a copy is indistinguishable by value -- but
                // not by identity, and identity is what a thread boundary
                // cares about.
                let d = self.new_val(IrTy::Ref);
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_str_clone".to_string(),
                    args: vec![v.val()],
                });
                self.stmt_temps.push(d);
                return Ok(Val::new(d, v.ty, true));
            }
            // Mutable, so here the copy is observable by value too: writing
            // one leaves the other alone. Its bytes are not references, so
            // shallow and deep are the same copy.
            if self.underlying(v.ty) == Ty::Bytes {
                let c = self.rt_value("rt_bytes_clone", vec![v.val()], Ty::Bytes);
                return Ok(Val::new(c.val(), v.ty, true));
            }
            let Some(tid) = self.tdef_of(v.ty) else {
                return Err(Diag::new(
                    args.pos[0].span(),
                    format!(
                        "{} is copied by assignment; there is nothing to clone",
                        self.tyname(v.ty)
                    ),
                ));
            };
            let td = &self.typedefs[tid as usize];
            if td.is_interface || td.is_chan || td.is_distinct {
                return Err(Diag::new(
                    args.pos[0].span(),
                    format!("`{}` cannot be cloned", self.tyname(v.ty)),
                ));
            }

            // Field by field: the copy holds the same references, each
            // retained once more.
            let n = self.typedefs[tid as usize].fields.len();
            let d = self.new_val(IrTy::Ref);
            self.push(Inst::Alloc { dst: d, tid });
            for i in 0..n {
                let (_, fty) = self.typedefs[tid as usize].fields[i].clone();
                let cur = self.new_val(fty);
                self.push(Inst::LoadField {
                    dst: cur,
                    obj: v.val(),
                    tid,
                    idx: i as u32,
                });
                if fty == IrTy::Ref {
                    self.push(Inst::RcInc { val: cur });
                }
                self.push(Inst::StoreField {
                    obj: d,
                    tid,
                    idx: i as u32,
                    val: cur,
                });
            }
            self.stmt_temps.push(d);
            return Ok(Val::new(d, v.ty, true));
        }

        // Channel builtins. They are here rather than in `sigs` because
        // their types depend on the channel's element type.
        if matches!(name, "send" | "recv" | "close") {
            return self.lower_chan_builtin(name, args, span);
        }

        let Some(sig) = self.sigs.get(name) else {
            if name == "len" {
                return Err(Diag::new(
                    span,
                    "there is no `len`; every collection and `str` answers \
                     `.size()`",
                ));
            }
            return Err(Diag::new(span, format!("unknown function `{name}`")));
        };
        let params = sig.params.clone();
        let module = sig.module.clone();
        let ret = sig.ret;
        let sig_is_prim = sig.is_prim;

        let slots = self.bind_args(name, &params, args, span)?;

        let mut vals = Vec::new();
        for (a, p) in slots.iter().zip(params.iter()) {
            let v = self.lower_slot(a, p, &module)?;
            if !self.assignable(v.ty, p.ty) {
                return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
            }
            // Arguments are borrowed (§5.1): no retain at the call site. An
            // owned temporary is already on the statement's pending list --
            // the producer put it there -- so it must NOT be added again.
            vals.push(v.val());
        }

        let rt_name = if sig_is_prim {
            // The seam's whole lowering rule: strip the module qualifier,
            // strip the reserved `__`, prefix `rt_`. No table of special
            // cases, and a runtime function nobody wrote is a link error
            // naming the exact symbol. See docs/stdlib-seam.md.
            format!("rt_{}", crate::ast::bare(name).trim_start_matches('_'))
        } else {
            match name {
                "concat" => "rt_concat".to_string(),
                // The emitter escapes and prefixes; give it the raw name.
                other => other.to_string(),
            }
        };

        if ret == Ty::Void {
            self.push(Inst::Call {
                dst: None,
                func: rt_name,
                args: vals,
            });
            Ok(Val::void())
        } else {
            let d = self.new_val(self.irty(ret));
            self.push(Inst::Call {
                dst: Some(d),
                func: rt_name,
                args: vals,
            });
            // Returns are owned (§5.2).
            let owned = self.is_ref(ret);
            if owned {
                self.stmt_temps.push(d);
            }
            Ok(Val::new(d, ret, owned))
        }
    }
}

/// A name with its indefinite article, for a diagnostic: `an Array<int>`,
/// `a List<int>`. By the first letter, which is right for every type name
/// the language has.
fn a_or_an(name: &str) -> String {
    let vowel = name
        .chars()
        .next()
        .is_some_and(|c| "aeiouAEIOU".contains(c));
    format!("{} {name}", if vowel { "an" } else { "a" })
}

fn stmt_span(s: &Stmt) -> Span {
    match s {
        Stmt::Decl { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Eval { span, .. }
        | Stmt::While { span, .. }
        | Stmt::Break { span, .. }
        | Stmt::Continue { span, .. }
        | Stmt::ForIn { span, .. }
        | Stmt::Spawn { span, .. }
        | Stmt::SetIndex { span, .. }
        | Stmt::SetField { span, .. }
        | Stmt::Match { span, .. }
        | Stmt::If { span, .. } => *span,
    }
}
