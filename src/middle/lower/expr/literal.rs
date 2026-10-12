//! Literals, sequence construction, and `new`.

use super::*;

impl Lowerer {
    /// `[]`, `[a, b, c]`, `[x; n]`, `{}`, `{k: v}`.
    pub(super) fn lower_literal(&mut self, e: &Expr, want: Option<Ty>) -> Result<Val, Diag> {
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
                // The map retains the key only once the value is lowered.
                let kv = self.hold(ke, kv, self.may_run_code(ve, true));
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
            // The runtime retains the fill only once the length is lowered.
            let v = self.hold(ve, v, self.may_run_code(ne, true));
            let n = self.lower_expr(ne)?;
            if self.underlying(n.ty) != Ty::Int {
                return Err(Diag::new(ne.span(), self.mismatch(Ty::Int, n.ty)));
            }
            return Ok(self.build_repeat(want, elem, v, n.val(), is_list));
        }

        let Expr::SeqLit(items, _) = e else {
            unreachable!("only the three literal forms reach here")
        };
        // Every element is lowered before any is stored.
        let later = self.later_in(items);
        let mut vals = Vec::new();
        for (it, later) in items.iter().zip(later) {
            let v = self.lower_expr_as(it, elem)?;
            if !self.assignable(v.ty, elem) {
                return Err(Diag::new(it.span(), self.mismatch(elem, v.ty)));
            }
            vals.push(self.hold(it, v, later));
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
    pub(super) fn bytes_literal(&mut self, e: &Expr) -> Result<Val, Diag> {
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
    pub(super) fn build_repeat(
        &mut self,
        ty: Ty,
        elem: Ty,
        fill: Val,
        n: Value,
        is_list: bool,
    ) -> Val {
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
    pub(super) fn build_seq(
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
                    self.rc_inc(v.val());
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

    pub(super) fn lower_new(&mut self, ty: Ty, args: &Args, span: Span) -> Result<Val, Diag> {
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
        if tname.starts_with("Map$") {
            let (k, v) = self.map_kv(ty).expect("a map has a key and a value");
            // The key type is checked BEFORE the spelling, and on the `{}`
            // path too: a bad key type must be caught either way, and it is
            // the more useful of the two things to be told.
            let kind = self.map_key_kind(k, span)?;
            if !self.building_literal {
                return Err(Diag::new(
                    span,
                    format!(
                        "write {} as a literal: `{{k: v}}` or `{{}}`",
                        a_or_an(&self.tyname(ty))
                    ),
                ));
            }
            if !args.pos.is_empty() || !args.named.is_empty() {
                return Err(Diag::new(span, "a map takes no arguments"));
            }
            // `MapKey`, then the two refcount flags (runtime/rt.h).
            let kv = self.new_val(IrTy::I64);
            self.push(Inst::IConst { dst: kv, val: kind });
            let mut flags = vec![kv];
            for b in [self.is_ref(k), self.is_ref(v)] {
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
        // Construction writes every field, so from outside the module it is
        // allowed only when every field it would write is one the caller
        // could write anyway. A private field with a default is written by
        // its own module's default and blocks nothing unless it is named; a
        // private field without one would have to be passed, which is
        // exactly what privacy forbids. That is the point: a type with an
        // invariant hides a field, and then its module's own function is
        // the only way to make one. Checked before binding, so the refusal
        // is about privacy and not about how many arguments there are.
        for (i, f) in fields.iter().enumerate() {
            if self.field_visible(tid, i as u32) {
                continue;
            }
            let shown = self.show_name(name);
            if f.default.is_none() {
                return Err(Diag::new(
                    span,
                    format!(
                        "`{shown}` cannot be constructed outside `{module}`: its field `{}` \
                         is private and has no default. `{module}` must provide a function \
                         that builds one",
                        f.name
                    ),
                ));
            }
            if let Some((_, e)) = args.named.iter().find(|(n, _)| *n == f.name) {
                return Err(Diag::new(
                    e.span(),
                    format!(
                        "field `{}` of `{shown}` is private to `{module}`; only a `pub` \
                         field can be set from another module",
                        f.name
                    ),
                ));
            }
        }
        let slots = self.bind_args(name, &fields, args, span)?;
        // Allocating and storing run no user code, so only a later argument
        // can disturb an earlier one before it is stored.
        let later = self.later_flags(&slots, &fields, false);

        let mut given: Vec<Option<Val>> = Vec::new();
        for (i, (e, f)) in slots.iter().zip(fields.iter()).enumerate() {
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
            let v = self.hold(e, v, later[i]);
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
                    self.rc_inc(v.val());
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
}
