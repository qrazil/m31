//! Module-level constants: checking them, computing them, and handing their
//! values to the lowering. docs/reference.md §4.4.
//!
//! A constant's initialiser is evaluated HERE, by the compiler, never by the
//! program. That is what makes a constant free to use: a collection is laid
//! out as static data in the emitted C (src/emit_c.rs, `emit_statics`), a
//! scalar is folded into every use, and nothing runs before the program's
//! first statement -- so there is no initialisation order to get wrong,
//! across modules or within one, and no cost at run time.
//!
//! The evaluator is deliberately small. A constant expression is a literal,
//! another constant, an operator applied to those, or a collection literal
//! of those. No calls: a call would need the whole language at compile time,
//! which is a second implementation of it that could disagree with the
//! first. Arithmetic follows the run-time rules exactly -- an int overflow
//! or a division by zero, which would trap at run time, is refused here.

use super::{Lowerer, Val, BUILTIN_FNS};
use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{Inst, IrTy, StaticObj, StaticSlot};

/// The most slots one constant collection may have. It is written out in the
/// emitted C, so this bounds the C file and the compile time rather than
/// anything at run time: `[0; 1000000000]` is eight gigabytes of source.
/// Big enough for any table written by hand or generated into a file.
const MAX_SLOTS: i64 = 1 << 20;

/// A constant's value, computed. A collection is already a static object --
/// `Obj` is its index in `Lowerer::static_objs` -- so two constants naming one
/// table share it, and a table inside a table is the same object as the
/// constant it came from.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum CVal {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Obj(u32),
}

/// One declared constant, and its value once computed.
#[derive(Debug, Clone)]
pub(super) struct ConstInfo {
    pub decl: ConstDecl,
    pub val: Option<CVal>,
}

/// The hash the runtime's map uses for a key -- runtime/rt.c, `hash_int` and
/// `hash_key`. A constant map is laid out by the compiler, so each key has to
/// sit where the runtime's probe will look for it; this is the one place the
/// compiler and the runtime must agree bit for bit, and rt.c says so beside
/// its copy.
fn map_hash(key: &CVal) -> u64 {
    match key {
        // splitmix64's finaliser.
        CVal::Int(x) => {
            let mut z = (*x as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        // FNV-1a over the bytes.
        CVal::Str(s) => {
            let mut h: u64 = 1_469_598_103_934_665_603;
            for b in s.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(1_099_511_628_211);
            }
            h
        }
        _ => unreachable!("a map key is an int or a str; the type check said so"),
    }
}

impl Lowerer {
    /// Collect every module constant, refuse the names it may not take, and
    /// compute every value -- used or not, so a constant that is wrong is an
    /// error whether or not anything reads it yet.
    pub(super) fn register_consts(&mut self, p: &Program) -> Result<(), Diag> {
        for c in &p.consts {
            let bare = crate::ast::bare(&c.name).to_string();
            let here = |msg: String| Diag::new(c.span, msg).in_module(&c.module);
            self.check_not_import(&c.module, &bare, c.span)
                .map_err(|d| d.in_module(&c.module))?;
            if self.consts.contains_key(&c.name) {
                return Err(here(format!("`{bare}` is already defined")));
            }
            // One namespace for every name a module declares, as for
            // functions and types: `LIMIT` the constant and `LIMIT()` the
            // function would make a bare `LIMIT` mean two things.
            if self.sigs.contains_key(&c.name) || BUILTIN_FNS.contains(&bare.as_str()) {
                return Err(here(format!("`{bare}` is already a function")));
            }
            if self.typedefs.iter().any(|d| d.name == c.name) {
                return Err(here(format!("`{bare}` is already a type")));
            }
            self.consts.insert(
                c.name.clone(),
                ConstInfo {
                    decl: c.clone(),
                    val: None,
                },
            );
            self.const_order.push(c.name.clone());
        }
        for key in self.const_order.clone() {
            let span = self.consts[&key].decl.span;
            self.const_value_of(&key, span)?;
        }
        Ok(())
    }

    /// The key of the constant a bare name means in the module being
    /// lowered, if it names one.
    pub(super) fn resolve_const(&self, name: &str) -> Option<String> {
        let key = if self.cur_module.is_empty() {
            name.to_string()
        } else {
            format!("{}#{name}", self.cur_module)
        };
        self.consts.contains_key(&key).then_some(key)
    }

    /// `lib.NAME`, if `lib` is a module and not a local: the constant's key,
    /// once its privacy has been checked. `Ok(None)` when this is not a
    /// module qualifier at all and the caller should carry on as before.
    pub(super) fn qualified_const(
        &self,
        obj: &Expr,
        name: &str,
        span: Span,
    ) -> Result<Option<String>, Diag> {
        let Expr::Var(m, _) = obj else {
            return Ok(None);
        };
        if !self.modules.contains(m) || self.lookup(m).is_some() {
            return Ok(None);
        }
        let key = format!("{m}#{name}");
        let Some(info) = self.consts.get(&key) else {
            return Err(Diag::new(span, format!("`{m}` has no constant `{name}`")));
        };
        if !info.decl.is_pub && info.decl.module != self.cur_module {
            return Err(Diag::new(
                span,
                format!("`{name}` is private to `{m}`; mark it `pub` to export it"),
            ));
        }
        Ok(Some(key))
    }

    /// A bare name that is some OTHER module's constant: say where it lives
    /// rather than calling it unknown, the way a bare call does.
    pub(super) fn foreign_const_hint(&self, name: &str, span: Span) -> Option<Diag> {
        let suffix = format!("#{name}");
        let mut keys: Vec<&String> = self
            .consts
            .keys()
            .filter(|k| k.ends_with(&suffix))
            .collect();
        keys.sort();
        let info = &self.consts[*keys.first()?];
        let owner = &info.decl.module;
        if *owner == self.cur_module {
            return None;
        }
        Some(Diag::new(
            span,
            if info.decl.is_pub {
                format!("`{name}` is declared in `{owner}`; write `{owner}.{name}`")
            } else {
                format!("`{name}` is private to `{owner}`")
            },
        ))
    }

    /// A use of a constant in lowered code: the folded value for a scalar,
    /// the literal for a `str`, the static object for a collection. Borrowed
    /// and never owned, like a string literal -- it is immortal.
    pub(super) fn lower_const_use(&mut self, key: &str) -> Val {
        let info = &self.consts[key];
        let ty = info.decl.ty;
        let val = info.val.clone().expect("every constant is computed first");
        let v = match val {
            CVal::Int(n) => {
                let v = self.new_val(IrTy::I64);
                self.push(Inst::IConst { dst: v, val: n });
                v
            }
            CVal::Float(x) => {
                let v = self.new_val(IrTy::F64);
                self.push(Inst::FConst { dst: v, val: x });
                v
            }
            CVal::Bool(b) => {
                let v = self.new_val(IrTy::I1);
                self.push(Inst::BConst { dst: v, val: b });
                v
            }
            CVal::Str(s) => {
                let idx = self.intern_str(&s);
                let v = self.new_val(IrTy::Ref);
                self.push(Inst::SConst { dst: v, idx });
                v
            }
            CVal::Obj(idx) => {
                let v = self.new_val(IrTy::Ref);
                self.push(Inst::KConst { dst: v, idx });
                v
            }
        };
        Val::new(v, ty, false)
    }

    /// If `e` reaches its object through a constant -- a module constant, or
    /// a local declared `const` -- the name as written, for the diagnostic.
    ///
    /// Followed through fields and indexes, because `T[0][1] = 5` changes
    /// the constant `T` as surely as `T[0] = 5` does.
    pub(super) fn const_root(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::Var(n, _) => {
                if let Some((_, _, is_const)) = self.binding(n) {
                    return is_const.then(|| n.clone());
                }
                if self.recv_field(n).is_some() {
                    return None;
                }
                self.resolve_const(n).map(|_| n.clone())
            }
            Expr::Field(o, f, span) => {
                if let Ok(Some(_)) = self.qualified_const(o, f, *span) {
                    if let Expr::Var(m, _) = &**o {
                        return Some(format!("{m}.{f}"));
                    }
                }
                self.const_root(o)
            }
            Expr::Index(o, _, _) => self.const_root(o),
            _ => None,
        }
    }

    /// Refuse a change made through a constant, when the compiler can see
    /// it. The runtime traps on the ones it cannot (runtime/rt.c,
    /// `rt_check_mutable`).
    pub(super) fn refuse_const_write(&self, target: &Expr, span: Span) -> Result<(), Diag> {
        match self.const_root(target) {
            Some(n) => Err(Diag::new(
                span,
                format!("`{n}` is const and cannot be changed; clone({n}) is a copy that can"),
            )),
            None => Ok(()),
        }
    }

    /// Whether method `m` changes a built-in collection of type `t` -- the
    /// calls `refuse_const_write` applies to. A user type's methods are not
    /// here: whether one changes its receiver is not written in its
    /// signature, so the compiler cannot say.
    pub(super) fn is_mutating_method(&self, t: Ty, m: &str) -> bool {
        if self.underlying(t) == Ty::Bytes {
            return matches!(m, "push" | "pop" | "clear" | "extend");
        }
        if self.seq_elem(t).is_some() {
            if self.is_list(t) {
                return matches!(
                    m,
                    "push" | "pop" | "insert" | "remove_at" | "clear" | "reverse" | "sort"
                );
            }
            return matches!(m, "reverse" | "sort");
        }
        if self.map_kv(t).is_some() {
            return matches!(m, "set" | "remove" | "clear");
        }
        false
    }

    /// Whether `e` denotes a module constant, whose value is immortal and so
    /// may cross a thread boundary without being moved.
    pub(super) fn is_module_const(&self, e: &Expr) -> bool {
        match e {
            Expr::Var(n, _) => {
                self.binding(n).is_none()
                    && self.recv_field(n).is_none()
                    && self.resolve_const(n).is_some()
            }
            Expr::Field(o, f, span) => matches!(self.qualified_const(o, f, *span), Ok(Some(_))),
            _ => false,
        }
    }

    /// Trap at run time if `obj` is frozen -- emitted before a store into an
    /// object that already exists. `rt_check_mutable` is a static inline in
    /// rt.h, so this is a load and a branch, not a call.
    pub(super) fn check_mutable(&mut self, obj: crate::ir::Value) {
        self.push(Inst::Call {
            dst: None,
            func: "rt_check_mutable".to_string(),
            args: vec![obj],
        });
    }

    /// Whether binding a `const` local of type `ty` takes a snapshot
    /// (docs/const-decision.md). A scalar and a `str` are immutable already;
    /// every other reference is handed to `rt_snapshot`, which freezes it in
    /// place when nothing else holds it and freezes a deep copy when
    /// something does. Nothing is refused: `const T a = b;` always compiles,
    /// and costs a copy exactly when `b` is shared.
    pub(super) fn const_snapshots(&self, ty: Ty) -> bool {
        self.is_ref(ty) && self.underlying(ty) != Ty::Str
    }

    fn intern_str(&mut self, s: &str) -> u32 {
        match self.strings.iter().position(|x| x == s) {
            Some(i) => i as u32,
            None => {
                self.strings.push(s.to_string());
                (self.strings.len() - 1) as u32
            }
        }
    }

    // ---- evaluation -------------------------------------------------------

    /// The value of the constant `key`, computing it the first time. `span`
    /// is the use that asked, which is where a cycle is reported.
    fn const_value_of(&mut self, key: &str, span: Span) -> Result<CVal, Diag> {
        if let Some(v) = &self.consts[key].val {
            return Ok(v.clone());
        }
        // A cycle is refused with the whole chain, head repeated to close
        // the loop -- the same shape as an import cycle, and for the same
        // reason: the shortest description of a cycle is all of it.
        if let Some(at) = self.const_stack.iter().position(|k| k == key) {
            let mut chain = String::new();
            for k in &self.const_stack[at..] {
                chain.push_str(&self.const_name(k));
                chain.push_str("\n    uses ");
            }
            chain.push_str(&self.const_name(key));
            return Err(Diag::new(span, format!("constant cycle: {chain}")));
        }
        let decl = self.consts[key].decl.clone();
        self.const_stack.push(key.to_string());
        // Names in the initialiser mean what they mean in the declaring
        // module, whichever module's use got here first.
        let saved = std::mem::replace(&mut self.cur_module, decl.module.clone());
        let r = self
            .check_const_ty(decl.ty, decl.span)
            .and_then(|_| self.eval_as(&decl.init, decl.ty));
        self.cur_module = saved;
        self.const_stack.pop();
        let v = r.map_err(|d| d.in_module(&decl.module))?;
        self.consts.get_mut(key).expect("registered").val = Some(v.clone());
        Ok(v)
    }

    /// A constant's name as a reader of the current module writes it.
    fn const_name(&self, key: &str) -> String {
        match key.split_once('#') {
            Some((m, n)) if m == self.cur_module => n.to_string(),
            Some((m, n)) => format!("{m}.{n}"),
            None => key.to_string(),
        }
    }

    /// What a module constant may be: a value the compiler can compute and
    /// the emitted C can hold as static data.
    ///
    /// Every built-in collection, `List` and `bytes` included, because a
    /// `const` means the same thing wherever it is written: a `const
    /// List<int>` local is legal and frozen, so one at module level is too.
    /// No user type yet: a struct constant is straightforward, but an enum
    /// constant raises what an immortal payload means for `match`, and
    /// neither has a use that is waiting on it.
    fn check_const_ty(&self, t: Ty, span: Span) -> Result<(), Diag> {
        match t {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes => return Ok(()),
            Ty::User(_) if self.base_of(t).is_none() => {
                if let Some(e) = self.seq_elem(t) {
                    return self.check_const_ty(e, span);
                }
                if let Some((k, v)) = self.map_kv(t) {
                    if k != Ty::Int && k != Ty::Str {
                        return Err(Diag::new(
                            span,
                            format!("a map key must be int or str, found {}", self.tyname(k)),
                        ));
                    }
                    return self.check_const_ty(v, span);
                }
            }
            _ => {}
        }
        Err(Diag::new(
            span,
            format!(
                "a constant must be an int, float, bool, str or bytes, or an Array, List \
                 or Map of those; `{}` is not",
                self.tyname(t)
            ),
        ))
    }

    /// Evaluate `e` where a value of type `want` is expected. A collection
    /// literal takes its type from here, as it does everywhere (§3.9).
    fn eval_as(&mut self, e: &Expr, want: Ty) -> Result<CVal, Diag> {
        match e {
            Expr::SeqLit(items, span) => {
                let elem = self.seq_elem_of(want, *span)?;
                let mut slots = Vec::new();
                for it in items {
                    let v = self.eval_as(it, elem)?;
                    slots.push(self.static_slot(v));
                }
                Ok(self.add_seq(want, elem, slots, e.span())?)
            }
            Expr::RepeatLit(fill, count, span) => {
                let elem = self.seq_elem_of(want, *span)?;
                let n = match self.eval_any(count)? {
                    (CVal::Int(n), _) => n,
                    (_, t) => {
                        return Err(Diag::new(count.span(), self.mismatch(Ty::Int, t)));
                    }
                };
                if n < 0 {
                    return Err(Diag::new(count.span(), "array length cannot be negative"));
                }
                if n > MAX_SLOTS {
                    return Err(Diag::new(
                        count.span(),
                        format!(
                            "a constant collection may hold at most {MAX_SLOTS} elements: \
                             it is written out in full in the compiled program"
                        ),
                    ));
                }
                let v = self.eval_as(fill, elem)?;
                let s = self.static_slot(v);
                Ok(self.add_seq(want, elem, vec![s; n as usize], fill.span())?)
            }
            Expr::MapLit(items, span) => {
                let Some((kt, vt)) = self.map_kv(want) else {
                    return Err(Diag::new(
                        *span,
                        format!("a map literal cannot be {}", self.tyname(want)),
                    ));
                };
                let mut entries: Vec<(CVal, CVal)> = Vec::new();
                for (k, v) in items {
                    let kv = self.eval_as(k, kt)?;
                    // Refused rather than last-one-wins: in a table written
                    // by hand, a key written twice is a mistake, and nothing
                    // at run time would ever show it.
                    if entries.iter().any(|(x, _)| *x == kv) {
                        return Err(Diag::new(
                            k.span(),
                            "this key is already in the map; a constant map may not \
                             repeat one",
                        ));
                    }
                    let vv = self.eval_as(v, vt)?;
                    entries.push((kv, vv));
                }
                if entries.len() as i64 > MAX_SLOTS {
                    return Err(Diag::new(
                        *span,
                        format!("a constant collection may hold at most {MAX_SLOTS} elements"),
                    ));
                }
                Ok(self.add_map(kt, vt, entries))
            }
            _ => {
                let (v, t) = self.eval_any(e)?;
                if t != want {
                    return Err(Diag::new(e.span(), self.mismatch(want, t)));
                }
                Ok(v)
            }
        }
    }

    /// The element type a sequence literal is written in: an Array's or a
    /// List's element, or for `bytes` an int, since a byte is an int from 0
    /// to 255 (§3.10).
    fn seq_elem_of(&self, want: Ty, span: Span) -> Result<Ty, Diag> {
        if want == Ty::Bytes {
            return Ok(Ty::Int);
        }
        self.seq_elem(want).ok_or_else(|| {
            Diag::new(
                span,
                format!("a sequence literal cannot be {}", self.tyname(want)),
            )
        })
    }

    /// Evaluate an expression that has a type of its own.
    fn eval_any(&mut self, e: &Expr) -> Result<(CVal, Ty), Diag> {
        match e {
            Expr::Int(n, _) => Ok((CVal::Int(*n), Ty::Int)),
            Expr::Float(x, _) => Ok((CVal::Float(*x), Ty::Float)),
            Expr::Bool(b, _) => Ok((CVal::Bool(*b), Ty::Bool)),
            Expr::Str(s, _) => Ok((CVal::Str(s.clone()), Ty::Str)),
            Expr::Var(n, span) => {
                let Some(key) = self.resolve_const(n) else {
                    if let Some(d) = self.foreign_const_hint(n, *span) {
                        return Err(d);
                    }
                    return Err(Diag::new(
                        *span,
                        format!(
                            "`{n}` is not a constant; a constant's value is computed by \
                             the compiler, from literals, operators and other constants"
                        ),
                    ));
                };
                let v = self.const_value_of(&key, *span)?;
                Ok((v, self.consts[&key].decl.ty))
            }
            Expr::Field(o, f, span) => match self.qualified_const(o, f, *span)? {
                Some(key) => {
                    let v = self.const_value_of(&key, *span)?;
                    Ok((v, self.consts[&key].decl.ty))
                }
                None => Err(self.not_constant(e)),
            },
            Expr::Un(op, x, span) => {
                let (v, t) = self.eval_any(x)?;
                match (op, v) {
                    (UnOp::Neg, CVal::Int(n)) => {
                        n.checked_neg().map(|r| (CVal::Int(r), t)).ok_or_else(|| {
                            Diag::new(
                                *span,
                                "integer overflow in -: at run time this would trap, so as \
                                 a constant it is refused",
                            )
                        })
                    }
                    // A sign flip, exactly as the run-time `-x` is: zero
                    // included, so `-0.0` is negative zero.
                    (UnOp::Neg, CVal::Float(x)) => Ok((CVal::Float(-x), t)),
                    (UnOp::BitNot, CVal::Int(n)) => Ok((CVal::Int(!n), t)),
                    (UnOp::Not, CVal::Bool(b)) => Ok((CVal::Bool(!b), t)),
                    _ => {
                        let o = match op {
                            UnOp::Neg => "-",
                            UnOp::Not => "!",
                            UnOp::BitNot => "~",
                        };
                        Err(Diag::new(
                            *span,
                            format!("cannot apply `{o}` to a value of type {}", self.tyname(t)),
                        ))
                    }
                }
            }
            Expr::Bin(op, l, r, span) => {
                let (a, at) = self.eval_any(l)?;
                let (b, bt) = self.eval_any(r)?;
                if at != bt {
                    return Err(Diag::new(
                        *span,
                        format!(
                            "cannot apply `{}` to {} and {}; convert one of them",
                            op.spelling(),
                            self.tyname(at),
                            self.tyname(bt)
                        ),
                    ));
                }
                self.eval_bin(*op, a, b, at, *span)
            }
            _ => Err(self.not_constant(e)),
        }
    }

    fn not_constant(&self, e: &Expr) -> Diag {
        Diag::new(
            e.span(),
            "not a constant expression: a constant's value is computed by the \
             compiler, so it may use literals, operators and other constants, \
             but not calls, fields or indexing",
        )
    }

    /// One binary operator on two computed values of one type, by the same
    /// rules the run time applies (docs/reference.md §6.1).
    fn eval_bin(&self, op: BinOp, a: CVal, b: CVal, t: Ty, span: Span) -> Result<(CVal, Ty), Diag> {
        use BinOp::*;
        let err = |msg: String| Err(Diag::new(span, msg));
        let bad = || {
            Err(Diag::new(
                span,
                format!(
                    "cannot apply `{}` to {} and {}",
                    op.spelling(),
                    self.tyname(t),
                    self.tyname(t)
                ),
            ))
        };
        match (a, b) {
            (CVal::Int(x), CVal::Int(y)) => {
                let over = || {
                    Diag::new(
                        span,
                        format!(
                            "integer overflow in {}: at run time this would trap, so as a \
                             constant it is refused",
                            op.spelling()
                        ),
                    )
                };
                let v = match op {
                    Add => x.checked_add(y).ok_or_else(over)?,
                    Sub => x.checked_sub(y).ok_or_else(over)?,
                    Mul => x.checked_mul(y).ok_or_else(over)?,
                    Div | Rem if y == 0 => {
                        let what = if op == Div { "division" } else { "remainder" };
                        return err(format!("{what} by zero"));
                    }
                    Div => x.checked_div(y).ok_or_else(over)?,
                    Rem => x.checked_rem(y).ok_or_else(over)?,
                    BitAnd => x & y,
                    BitOr => x | y,
                    BitXor => x ^ y,
                    // On the bit pattern, as rt_ishl and rt_ishr do: `<<`
                    // discards what it shifts out, `>>` copies the sign.
                    Shl | Shr if !(0..=63).contains(&y) => {
                        return err(format!("shift count out of range in {}", op.spelling()));
                    }
                    Shl => ((x as u64) << y) as i64,
                    Shr => x >> y,
                    Eq => return Ok((CVal::Bool(x == y), Ty::Bool)),
                    Ne => return Ok((CVal::Bool(x != y), Ty::Bool)),
                    Lt => return Ok((CVal::Bool(x < y), Ty::Bool)),
                    Le => return Ok((CVal::Bool(x <= y), Ty::Bool)),
                    Gt => return Ok((CVal::Bool(x > y), Ty::Bool)),
                    Ge => return Ok((CVal::Bool(x >= y), Ty::Bool)),
                    And | Or => return bad(),
                };
                Ok((CVal::Int(v), Ty::Int))
            }
            (CVal::Float(x), CVal::Float(y)) => {
                let v = match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    Div => x / y,
                    Rem => {
                        return err("`%` is integer remainder; it does not apply to float".into())
                    }
                    Eq => return Ok((CVal::Bool(x == y), Ty::Bool)),
                    Ne => return Ok((CVal::Bool(x != y), Ty::Bool)),
                    Lt => return Ok((CVal::Bool(x < y), Ty::Bool)),
                    Le => return Ok((CVal::Bool(x <= y), Ty::Bool)),
                    Gt => return Ok((CVal::Bool(x > y), Ty::Bool)),
                    Ge => return Ok((CVal::Bool(x >= y), Ty::Bool)),
                    _ => return bad(),
                };
                // The rule a float literal already follows (§1.5): a value
                // the source cannot write is refused rather than kept. At run
                // time the same arithmetic gives an infinity or a NaN, which
                // is IEEE's answer; in a constant it is always a mistake.
                if !v.is_finite() {
                    let what = if v.is_nan() { "nan" } else { "an infinity" };
                    return err(format!(
                        "this `{}` gives {what}; a float constant must be finite, as a \
                         float literal must",
                        op.spelling()
                    ));
                }
                Ok((CVal::Float(v), Ty::Float))
            }
            (CVal::Bool(x), CVal::Bool(y)) => match op {
                And => Ok((CVal::Bool(x && y), Ty::Bool)),
                Or => Ok((CVal::Bool(x || y), Ty::Bool)),
                Eq => Ok((CVal::Bool(x == y), Ty::Bool)),
                Ne => Ok((CVal::Bool(x != y), Ty::Bool)),
                _ => bad(),
            },
            (CVal::Str(x), CVal::Str(y)) => match op {
                Add => Ok((CVal::Str(x + &y), Ty::Str)),
                Eq => Ok((CVal::Bool(x == y), Ty::Bool)),
                Ne => Ok((CVal::Bool(x != y), Ty::Bool)),
                _ => bad(),
            },
            // A collection has no operators.
            _ => bad(),
        }
    }

    // ---- static objects ---------------------------------------------------

    fn static_slot(&mut self, v: CVal) -> StaticSlot {
        match v {
            CVal::Int(n) => StaticSlot::Word(n),
            // The bit pattern, which is what rt_f2i would have stored.
            CVal::Float(x) => StaticSlot::Word(x.to_bits() as i64),
            CVal::Bool(b) => StaticSlot::Word(b as i64),
            CVal::Str(s) => StaticSlot::Str(self.intern_str(&s)),
            CVal::Obj(i) => StaticSlot::Obj(i),
        }
    }

    /// An Array, a List or a bytes of these slots, whichever `want` is. A
    /// byte outside 0..255 is refused, as it is in any bytes literal.
    fn add_seq(
        &mut self,
        want: Ty,
        elem: Ty,
        slots: Vec<StaticSlot>,
        span: Span,
    ) -> Result<CVal, Diag> {
        let refs = self.is_ref(elem);
        let obj = if want == Ty::Bytes {
            let mut bs = Vec::new();
            for s in &slots {
                match s {
                    StaticSlot::Word(v) if (0..=255).contains(v) => bs.push(*v as u8),
                    StaticSlot::Word(v) => {
                        return Err(Diag::new(
                            span,
                            format!("{v} is not a byte; a bytes element is 0 to 255"),
                        ));
                    }
                    _ => unreachable!("a bytes literal's elements were checked as int"),
                }
            }
            StaticObj::Bytes(bs)
        } else if self.is_list(want) {
            StaticObj::List { refs, slots }
        } else {
            StaticObj::Array { refs, slots }
        };
        self.static_objs.push(obj);
        Ok(CVal::Obj((self.static_objs.len() - 1) as u32))
    }

    /// Lay a map out the way `rt_map_set` would have left it, but at a size
    /// chosen once: the smallest power of two from 8 that keeps the load
    /// under the runtime's 70%, so a probe for an absent key always meets an
    /// empty slot and stops. Keys go in in source order, with the runtime's
    /// hash and linear probe.
    fn add_map(&mut self, kt: Ty, vt: Ty, entries: Vec<(CVal, CVal)>) -> CVal {
        let len = entries.len();
        let mut cap = 0usize;
        if len > 0 {
            cap = 8;
            while (len + 1) * 10 >= cap * 7 {
                cap *= 2;
            }
        }
        let mut table: Vec<Option<(StaticSlot, StaticSlot)>> = vec![None; cap];
        for (k, v) in entries {
            let mut i = (map_hash(&k) & (cap as u64 - 1)) as usize;
            while table[i].is_some() {
                i = (i + 1) & (cap - 1);
            }
            let (ks, vs) = (self.static_slot(k), self.static_slot(v));
            table[i] = Some((ks, vs));
        }
        self.static_objs.push(StaticObj::Map {
            key_is_str: kt == Ty::Str,
            val_is_ref: self.is_ref(vt),
            len,
            table,
        });
        CVal::Obj((self.static_objs.len() - 1) as u32)
    }
}
