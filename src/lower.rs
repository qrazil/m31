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
    /// The monomorphised program's interned type expressions.
    ty_exprs: Vec<TyExpr>,
    /// Required methods per interface, parallel to `typedefs`; empty for a
    /// struct.
    iface_methods: Vec<Vec<Func>>,
    /// Interface method names, one per dispatch slot, assigned once for the
    /// whole program so a vtable index is a constant at every call site.
    iface_slots: Vec<String>,
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

/// The representation of a surface type, WITHOUT resolving distinct types.
/// Use `Lowerer::irty` instead wherever a distinct type can appear.
fn ir_ty(t: Ty) -> IrTy {
    match t {
        Ty::Int => IrTy::I64,
        Ty::Bool => IrTy::I1,
        Ty::Str => IrTy::Ref,
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
            ty_exprs: Vec::new(),
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
        self.sigs.insert(name.to_string(), Sig { params, ret });
    }

    /// `xs.len()`, `xs.push(v)`, `xs.pop()`.
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
            "len" => {
                if !args.pos.is_empty() {
                    return Err(Diag::new(span, "`len` takes no arguments"));
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
                let v = self.lower_expr(&args.pos[0])?;
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
            "push" | "pop" => Err(Diag::new(
                span,
                format!("`{m}` needs a List; an Array has a fixed length"),
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

    /// `m.set(k, v)`, `m.get(k)`, `m.has(k)`, `m.remove(k)`, `m.len()`.
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
            "len" | "keys" | "values" | "clear" => 0,
            "set" => 2,
            "get" | "has" | "remove" => 1,
            other => return Err(Diag::new(span, format!("a map has no method `{other}`"))),
        };
        if args.pos.len() != arity {
            return Err(Diag::new(
                span,
                format!("`{m}` takes {arity} argument(s), found {}", args.pos.len()),
            ));
        }
        if m == "len" {
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
                let val = self.lower_expr(&args.pos[1])?;
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
                let d = self.new_val(self.irty(v));
                self.push(Inst::Call {
                    dst: Some(d),
                    func: "rt_map_get".to_string(),
                    args: vec![o.val(), key.val()],
                });
                // Borrowed from the map, which holds the +1.
                Ok(Val::new(d, v, false))
            }
            "has" => {
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
                let v = self.lower_expr(&args.pos[1])?;
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
                            "`{key}` and `{other}` would both be emitted as the same \
                             C function; rename one"
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
                // Every method of the embedded type, by name.
                let prefix = format!("{iname}.");
                let mut promoted: Vec<(String, Sig)> = self
                    .sigs
                    .iter()
                    .filter_map(|(k, sig)| {
                        let m = k.strip_prefix(&prefix)?;
                        Some((
                            m.to_string(),
                            Sig {
                                params: sig.params.clone(),
                                ret: sig.ret,
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
                                "`{}` gets `{mname}` from both `{other}` and `{}`; \
                                 give `{}` its own `{mname}` to say which one it means",
                                t.name, f.name, t.name
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
                        ret: sig.ret,
                        recv: Some(t.name.clone()),
                        name: mname,
                        tparams: Vec::new(),
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
                    format!("`{what}` has no parameter `{n}`"),
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
                    "`{what}` takes {} positional argument(s), found {}",
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
        self.builtin("len", vec![Ty::Str], Ty::Int);
        self.builtin("concat", vec![Ty::Str, Ty::Str], Ty::Str);

        // After monomorphisation every Ty::User names a concrete declaration
        // with no arguments, so resolution is a name lookup.
        self.ty_exprs = p.ty_exprs.clone();

        // Type table first: signatures and field types may refer to any type,
        // including one declared later in the file.
        for t in &p.types {
            if self.typedefs.iter().any(|d| d.name == t.name) {
                return Err(Diag::new(
                    t.span,
                    format!("type `{}` is already defined", t.name),
                ));
            }
            let mut fields = Vec::new();
            for f in &t.fields {
                if fields.iter().any(|(n, _): &(String, IrTy)| *n == f.name) {
                    return Err(Diag::new(
                        f.span,
                        format!("duplicate field `{}` in type `{}`", f.name, t.name),
                    ));
                }
                fields.push((f.name.clone(), self.irty(f.ty)));
            }
            self.typedefs.push(TypeDef {
                name: t.name.clone(),
                fields,
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
                if !self.iface_slots.contains(&m.name) {
                    self.iface_slots.push(m.name.clone());
                }
            }
            self.field_surface
                .push(t.fields.iter().map(|f| f.ty).collect());
            self.field_params.push(t.fields.clone());
        }

        for f in &p.funcs {
            // Habit from C, Java and Go. Without this it declares an ordinary
            // function nothing calls, and the program silently does nothing --
            // the worst failure mode for someone who has written C before.
            if f.recv.is_none() && f.name == "main" {
                return Err(Diag::new(
                    f.span,
                    "there is no `main`: statements at the top level are the program",
                ));
            }
            if self.sigs.contains_key(&f.key()) {
                return Err(Diag::new(
                    f.span,
                    format!("`{}` is already defined", f.key()),
                ));
            }
            if let Some(r) = &f.recv {
                if !self.typedefs.iter().any(|d| d.name == *r) {
                    return Err(Diag::new(f.span, format!("unknown type `{r}`")));
                }
            }
            // A type name wins in construction position, so a function
            // sharing one is silently unreachable. Names are case-blind here
            // -- nothing requires a type to be capitalised -- which makes the
            // collision easy to hit by accident.
            if self.typedefs.iter().any(|d| d.name == f.name) {
                return Err(Diag::new(f.span, format!("`{}` is already a type", f.name)));
            }
            self.sigs.insert(
                f.key(),
                Sig {
                    params: f.params.clone(),
                    ret: f.ret,
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
                },
            );
        }

        // There is no `main`. The statements written at the top level are
        // the program, in source order, and they are lowered as the body of
        // one synthesised function. Declarations are order-independent, so a
        // function may be called above its own definition.
        let entry = Func {
            ret: Ty::Void,
            recv: None,
            name: "$main".to_string(),
            tparams: Vec::new(),
            params: Vec::new(),
            body: p.toplevel.clone(),
            span: Span::new(1, 1),
        };

        let mut funcs = Vec::new();
        for f in &p.funcs {
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
            let vt: Vec<Option<String>> = slots
                .iter()
                .map(|m| {
                    let key = format!("{tname}.{m}");
                    self.sigs.contains_key(&key).then_some(key)
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
        if self.binding(name).is_some() {
            return Err(Diag::new(
                span,
                format!("`{name}` is already in scope; shadowing is not allowed, rename one"),
            ));
        }
        if self.sigs.contains_key(name) {
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
                        self.typedefs[tid as usize].name
                    ),
                ));
            }
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
    fn tyname(&self, t: Ty) -> String {
        match t {
            Ty::User(i) => self.ty_exprs[i as usize].name.clone(),
            other => other.name().to_string(),
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
    fn map_kv(&self, t: Ty) -> Option<(Ty, Ty)> {
        let tid = self.tdef_of(t)?;
        if !self.typedefs[tid as usize].name.starts_with("Map$") {
            return None;
        }
        let f = &self.field_surface[tid as usize];
        Some((f[0], f[1]))
    }

    /// A builtin generic stores its element type as its only "field", which
    /// is never laid out -- the runtime owns the representation.
    fn builtin_elem(&self, t: Ty, prefix: &str) -> Option<Ty> {
        let tid = self.tdef_of(t)?;
        if !self.typedefs[tid as usize].name.starts_with(prefix) {
            return None;
        }
        Some(self.field_surface[tid as usize][0])
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
        match self.missing_method(ft, tt) {
            Some(m) => format!(
                "{base}: `{}` needs a method `{m}` to satisfy `{}`",
                self.tyname(got),
                self.tyname(want)
            ),
            None => base,
        }
    }

    /// The first required method `ft` does not satisfy, for diagnostics.
    fn missing_method(&self, ft: u32, tt: u32) -> Option<String> {
        let fname = &self.typedefs[ft as usize].name;
        for m in &self.iface_methods[tt as usize] {
            let key = format!("{fname}.{}", m.name);
            let Some(sig) = self.sigs.get(&key) else {
                return Some(format!("{} {}(..)", self.tyname(m.ret), m.name));
            };
            let same = sig.ret == m.ret
                && sig.params.len() == m.params.len()
                && sig
                    .params
                    .iter()
                    .zip(m.params.iter())
                    .all(|(a, b)| a.ty == b.ty);
            if !same {
                return Some(format!(
                    "{} {}(..) with a matching signature",
                    self.tyname(m.ret),
                    m.name
                ));
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
        if let Some(rname) = &f.recv {
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
                        f.name,
                        f.ret.name()
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
                let val = self.lower_expr(init)?;
                if !self.assignable(val.ty, *ty) {
                    return Err(Diag::new(
                        init.span(),
                        format!(
                            "type mismatch: expected {}, found {}",
                            self.tyname(*ty),
                            self.tyname(val.ty)
                        ),
                    ));
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
                        let v = self.lower_expr(value)?;
                        let want = self.field_ty(tid, idx);
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
                let val = self.lower_expr(value)?;
                if !self.assignable(val.ty, ty) {
                    return Err(Diag::new(
                        value.span(),
                        format!(
                            "type mismatch: expected {}, found {}",
                            self.tyname(ty),
                            self.tyname(val.ty)
                        ),
                    ));
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
                            format!("expected a return value of type {}", t.name()),
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
                        let val = self.lower_expr(e)?;
                        if !self.assignable(val.ty, want) {
                            return Err(Diag::new(
                                e.span(),
                                format!(
                                    "type mismatch: expected {}, found {}",
                                    self.tyname(want),
                                    self.tyname(val.ty)
                                ),
                            ));
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

            Stmt::Eval { expr, .. } => {
                let val = self.lower_expr(expr)?;
                let _ = val;
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
                let v = self.lower_expr(value)?;
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
                let Some(sig) = self.sigs.get(name) else {
                    return Err(Diag::new(*span, format!("unknown function `{name}`")));
                };
                let params = sig.params.clone();
                if sig.ret != Ty::Void {
                    return Err(Diag::new(
                        *span,
                        format!("`spawn` needs a void function; `{name}` returns a value"),
                    ));
                }
                let slots = self.bind_args(name, &params, args, *span)?;
                let mut vals = Vec::new();
                for (a, p) in slots.iter().zip(params.iter()) {
                    let v = self.lower_expr(a)?;
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
                    func: name.clone(),
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
                let v = self.lower_expr(value)?;
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
        let Some(elem) = self.seq_elem(coll.ty) else {
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
            func: "rt_len_of".to_string(),
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
            func: "rt_index_get".to_string(),
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
            Expr::Call(name, args, span) => self.lower_call(name, args, *span),

            Expr::Field(obj, field, span) => {
                let o = self.lower_expr(obj)?;
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("type {} has no fields", self.tyname(o.ty)),
                    ));
                };
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
                let o = self.lower_expr(obj)?;
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("type {} has no methods", self.tyname(o.ty)),
                    ));
                };
                // Collections have built-in methods, typed against their
                // element type rather than declared anywhere.
                if let Some(elem) = self.seq_elem(o.ty) {
                    return self.lower_seq_method(&o, elem, m, args, *span);
                }
                if let Some((k, v)) = self.map_kv(o.ty) {
                    return self.lower_map_method(&o, k, v, m, args, *span);
                }

                // On an interface value the implementation is not known
                // statically: dispatch through the receiver's type header.
                if self.typedefs[tid as usize].is_interface {
                    let iname = self.typedefs[tid as usize].name.clone();
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
                    let slot = self
                        .iface_slots
                        .iter()
                        .position(|x| *x == *m)
                        .expect("every interface method has a slot")
                        as u32;

                    let slots =
                        self.bind_args(&format!("{iname}.{m}"), &decl.params, args, *span)?;
                    let mut vals = vec![o.val()];
                    for (a, p) in slots.iter().zip(decl.params.iter()) {
                        let v = self.lower_expr(a)?;
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
                let Some(sig) = self.sigs.get(&key) else {
                    return Err(Diag::new(
                        *span,
                        format!(
                            "type `{}` has no method `{m}`",
                            self.typedefs[tid as usize].name
                        ),
                    ));
                };
                let params = sig.params.clone();
                let ret = sig.ret;
                let slots = self.bind_args(&key, &params, args, *span)?;

                // The receiver is the hidden first argument, and is borrowed
                // like every other argument (docs/ir-v0.md §5.1).
                let mut vals = vec![o.val()];
                for (a, p) in slots.iter().zip(params.iter()) {
                    let v = self.lower_expr(a)?;
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
        }
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
            And | Or => return None,
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
        let key = format!("{tname}.{mname}");

        let Some(sig) = self.sigs.get(&key) else {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` on `{tname}` needs a method `{} {tname}.{mname}(..)`",
                    op.spelling(),
                    if mname == "cmp" {
                        "int"
                    } else if mname == "eq" {
                        "bool"
                    } else {
                        &tname
                    }
                ),
            ));
        };
        let (params, ret) = (sig.params.clone(), sig.ret);
        if params.len() != 1 || params[0].ty != b.ty {
            return Err(Diag::new(
                span,
                format!(
                    "`{key}` must take one {} parameter to support `{}`",
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
                    "`{key}` must return {} to support `{}`",
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
        let arith = match op {
            Add => Some(ArithOp::Add),
            Sub => Some(ArithOp::Sub),
            Mul => Some(ArithOp::Mul),
            Div => Some(ArithOp::Div),
            Rem => Some(ArithOp::Rem),
            _ => None,
        };

        if let Some(aop) = arith {
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
        if ty != Ty::Int && ty != Ty::Bool {
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
        if self.typedefs[tid as usize].is_interface {
            return Err(Diag::new(
                span,
                format!(
                    "`{}` is an interface; construct a type that satisfies it",
                    self.typedefs[tid as usize].name
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
                        self.typedefs[tid as usize].name
                    ),
                ));
            }
            let v = self.lower_expr(&args.pos[0])?;
            if self.underlying(v.ty) != self.underlying(ty) {
                return Err(Diag::new(
                    args.pos[0].span(),
                    self.mismatch(self.underlying(ty), v.ty),
                ));
            }
            return Ok(Val::new(v.val(), ty, v.owned));
        }
        let tname = self.typedefs[tid as usize].name.clone();
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
        let slots = self.bind_args(name, &fields, args, span)?;

        let mut given: Vec<Option<Val>> = Vec::new();
        for (e, f) in slots.iter().zip(fields.iter()) {
            let v = self.lower_expr(e)?;
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
                Ty::Bool => "rt_print_bool",
                Ty::Str => "rt_print_str",
                Ty::Void => return Err(Diag::new(args.pos[0].span(), "cannot print a void value")),
                // No printing of user types until there is a way for a type
                // to say how it prints. Better a clear refusal than an
                // address.
                Ty::User(_) => {
                    return Err(Diag::new(
                        args.pos[0].span(),
                        format!("cannot print a value of type `{}`", self.tyname(a.ty)),
                    ))
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
        if let Some(base) = match name {
            "int" => Some(Ty::Int),
            "bool" => Some(Ty::Bool),
            "str" => Some(Ty::Str),
            _ => None,
        } {
            if args.pos.len() != 1 || !args.named.is_empty() {
                return Err(Diag::new(span, format!("`{name}` converts one value")));
            }
            let v = self.lower_expr(&args.pos[0])?;
            if self.underlying(v.ty) != base {
                return Err(Diag::new(args.pos[0].span(), self.mismatch(base, v.ty)));
            }
            return Ok(Val::new(v.val(), base, v.owned));
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
            return Err(Diag::new(span, format!("unknown function `{name}`")));
        };
        let params = sig.params.clone();
        let ret = sig.ret;

        let slots = self.bind_args(name, &params, args, span)?;

        let mut vals = Vec::new();
        for (a, p) in slots.iter().zip(params.iter()) {
            let v = self.lower_expr(a)?;
            if !self.assignable(v.ty, p.ty) {
                return Err(Diag::new(a.span(), self.mismatch(p.ty, v.ty)));
            }
            // Arguments are borrowed (§5.1): no retain at the call site. An
            // owned temporary is already on the statement's pending list --
            // the producer put it there -- so it must NOT be added again.
            vals.push(v.val());
        }

        let rt_name = match name {
            "len" => "rt_len".to_string(),
            "concat" => "rt_concat".to_string(),
            // The emitter escapes and prefixes; give it the raw name.
            other => other.to_string(),
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
        | Stmt::If { span, .. } => *span,
    }
}
