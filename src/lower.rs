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
    /// The monomorphised program's interned type expressions.
    ty_exprs: Vec<TyExpr>,
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
    /// Inside a method: the receiver's type id and its SSA value. Fields are
    /// reached by bare name, which is safe only because nothing shadows
    /// anything -- see `check_shadow`.
    recv: Option<(u32, Value)>,
    ret_ty: Ty,
}

/// `ir_ty` for types that may be void, used where a mismatch is possible.
fn ir_ty_opt(t: Ty) -> Option<IrTy> {
    match t {
        Ty::Void => None,
        other => Some(ir_ty(other)),
    }
}

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
            ty_exprs: Vec::new(),
            strings: Vec::new(),
            types: Vec::new(),
            blocks: Vec::new(),
            cur: 0,
            scopes: Vec::new(),
            owned: Vec::new(),
            stmt_temps: Vec::new(),
            loops: Vec::new(),
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
                span: Span::new(0, 0),
            })
            .collect();
        self.sigs.insert(name.to_string(), Sig { params, ret });
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
                fields.push((f.name.clone(), ir_ty(f.ty)));
            }
            self.typedefs.push(TypeDef {
                name: t.name.clone(),
                fields,
            });
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
        funcs.push(self.lower_func(&entry)?);
        Ok(ir::Module {
            funcs,
            strings: self.strings,
            types: self.typedefs,
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
            if self.field_of(tid, name).is_some() {
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

    /// Read a bare name that is a field of the receiver.
    fn recv_field(&self, name: &str) -> Option<(u32, Value, u32, IrTy)> {
        let (tid, obj) = self.recv?;
        let (idx, fty) = self.field_of(tid, name)?;
        Some((tid, obj, idx, fty))
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
            let v = self.new_val(ir_ty(p.ty));
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
                Some(ir_ty(f.ret))
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
                if ty.is_ref() {
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
                    if ty.is_ref() {
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
                    if ty.is_ref() {
                        self.push(Inst::RcDec { val: v });
                    }
                }
            }
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
                if val.ty != *ty {
                    return Err(Diag::new(
                        init.span(),
                        format!(
                            "type mismatch: expected {}, found {}",
                            ty.name(),
                            val.ty.name()
                        ),
                    ));
                }
                self.check_shadow(name, *span)?;
                // The local must hold a +1. A borrowed source needs one added;
                // an owned temp is handed straight over, so drop it from the
                // pending list rather than releasing it.
                if ty.is_ref() {
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
                    if let Some((tid, obj, idx, fty)) = self.recv_field(name) {
                        let v = self.lower_expr(value)?;
                        let want = self.field_ty(tid, idx);
                        if v.ty != want {
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
                if val.ty != ty {
                    return Err(Diag::new(
                        value.span(),
                        format!(
                            "type mismatch: expected {}, found {}",
                            ty.name(),
                            val.ty.name()
                        ),
                    ));
                }
                if ty.is_ref() {
                    if val.owned {
                        self.stmt_temps.retain(|t| *t != val.val());
                    } else {
                        self.push(Inst::RcInc { val: val.val() });
                    }
                    // Release the previous value only after the new one is
                    // retained, so `s = s;` cannot free what it is assigning.
                    self.push(Inst::RcDec { val: old });
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
                        if val.ty != want {
                            return Err(Diag::new(
                                e.span(),
                                format!(
                                    "type mismatch: expected {}, found {}",
                                    want.name(),
                                    val.ty.name()
                                ),
                            ));
                        }
                        // Returns are owned (+1). Retain a borrowed value
                        // before releasing locals, or returning a local would
                        // hand back a freed object.
                        if want.is_ref() && !val.owned {
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
                if ir_ty_opt(v.ty) != Some(fty) {
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
                Stmt::Decl { .. }
                | Stmt::Return { .. }
                | Stmt::Eval { .. }
                | Stmt::Break { .. }
                | Stmt::Continue { .. }
                | Stmt::SetField { .. } => {}
            }
        }
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
            header_params.push(self.new_val(ir_ty(*ty)));
        }
        let hi = self.blocks.iter().position(|b| b.id == header).unwrap();
        self.blocks[hi].params = header_params.clone();

        let mut exit_params = Vec::new();
        for (_, ty, _) in &carried {
            exit_params.push(self.new_val(ir_ty(*ty)));
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
                format!("type mismatch: expected bool, found {}", c.ty.name()),
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
                format!("type mismatch: expected bool, found {}", c.ty.name()),
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
            join_params.push(self.new_val(ir_ty(*ty)));
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
                if let Some((ty, v)) = self.lookup(name) {
                    return Ok(Val::new(v, ty, false));
                }
                // Inside a method, a bare name may be a field of the
                // receiver. Unambiguous because nothing shadows anything.
                if let Some((tid, obj, idx, fty)) = self.recv_field(name) {
                    let d = self.new_val(fty);
                    self.push(Inst::LoadField {
                        dst: d,
                        obj,
                        tid,
                        idx,
                    });
                    // Borrowed from the receiver, which holds the +1.
                    return Ok(Val::new(d, self.field_ty(tid, idx), false));
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
                                format!("cannot negate a value of type {}", a.ty.name()),
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
                                format!("cannot apply `!` to a value of type {}", a.ty.name()),
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
                let Some((idx, fty)) = self.field_of(tid, field) else {
                    return Err(Diag::new(
                        *span,
                        format!("type `{}` has no field `{field}`", self.tyname(o.ty)),
                    ));
                };
                let d = self.new_val(fty);
                self.push(Inst::LoadField {
                    dst: d,
                    obj: o.val(),
                    tid,
                    idx,
                });
                // A field read is BORROWED from the object, exactly like a
                // local: the object holds the +1, we do not.
                Ok(Val::new(d, self.field_ty(tid, idx), false))
            }

            Expr::MethodCall(obj, m, args, span) => {
                let o = self.lower_expr(obj)?;
                let Some(tid) = self.tdef_of(o.ty) else {
                    return Err(Diag::new(
                        *span,
                        format!("type {} has no methods", self.tyname(o.ty)),
                    ));
                };
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
                if o.owned {
                    self.stmt_temps.push(o.val());
                }
                for (a, p) in slots.iter().zip(params.iter()) {
                    let v = self.lower_expr(a)?;
                    if v.ty != p.ty {
                        return Err(Diag::new(
                            a.span(),
                            format!(
                                "type mismatch: expected {}, found {}",
                                self.tyname(p.ty),
                                self.tyname(v.ty)
                            ),
                        ));
                    }
                    if v.owned {
                        self.stmt_temps.push(v.val());
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
                    let d = self.new_val(ir_ty(ret));
                    self.push(Inst::Call {
                        dst: Some(d),
                        func: key,
                        args: vals,
                    });
                    let owned = ret.is_ref();
                    if owned {
                        self.stmt_temps.push(d);
                    }
                    Ok(Val::new(d, ret, owned))
                }
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

        let d = self.new_val(ir_ty(ret));
        self.push(Inst::Call {
            dst: Some(d),
            func: key,
            args: vec![a.val(), b.val()],
        });
        if ret.is_ref() {
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
            _ => Ok(Val::new(d, ret, ret.is_ref())),
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
                    format!("type mismatch: expected bool, found {}", a.ty.name()),
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
            let b = self.lower_expr(r)?;
            if b.ty != Ty::Bool {
                return Err(Diag::new(
                    r.span(),
                    format!("type mismatch: expected bool, found {}", b.ty.name()),
                ));
            }
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

        // A user type on the left: dispatch to the operator's method.
        if matches!(a.ty, Ty::User(_)) {
            return self.lower_op_overload(op, &a, &b, span);
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
            if a.ty != Ty::Int || b.ty != Ty::Int {
                return Err(Diag::new(
                    span,
                    format!(
                        "cannot apply `{}` to {} and {}",
                        op.spelling(),
                        a.ty.name(),
                        b.ty.name()
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
                format!("cannot compare {} with {}", a.ty.name(), b.ty.name()),
            ));
        }
        if a.ty != Ty::Int && a.ty != Ty::Bool {
            return Err(Diag::new(
                span,
                format!("cannot compare values of type {}", a.ty.name()),
            ));
        }
        if a.ty == Ty::Bool && !matches!(op, Eq | Ne) {
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
        let name = self.typedefs[tid as usize].name.clone();
        let name = name.as_str();
        let fields = self.field_params[tid as usize].clone();
        let slots = self.bind_args(name, &fields, args, span)?;

        let mut given: Vec<Option<Val>> = Vec::new();
        for (e, f) in slots.iter().zip(fields.iter()) {
            let v = self.lower_expr(e)?;
            if v.ty != f.ty {
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
            if v.ty.is_ref() {
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
            let f = match a.ty {
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
            if a.owned {
                self.stmt_temps.push(a.val());
            }
            self.push(Inst::Call {
                dst: None,
                func: f.to_string(),
                args: vec![a.val()],
            });
            return Ok(Val::void());
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
            if v.ty != p.ty {
                return Err(Diag::new(
                    a.span(),
                    format!(
                        "type mismatch: expected {}, found {}",
                        self.tyname(p.ty),
                        self.tyname(v.ty)
                    ),
                ));
            }
            // Arguments are borrowed (§5.1): no retain at the call site. An
            // owned temporary still has to be released after the call, so it
            // stays on the statement's pending list.
            if v.owned {
                self.stmt_temps.push(v.val());
            }
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
            let d = self.new_val(ir_ty(ret));
            self.push(Inst::Call {
                dst: Some(d),
                func: rt_name,
                args: vals,
            });
            // Returns are owned (§5.2).
            let owned = ret.is_ref();
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
        | Stmt::SetField { span, .. }
        | Stmt::If { span, .. } => *span,
    }
}
