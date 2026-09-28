//! Type-level helpers shared across the lowering: naming, IR-type
//! conversion, and the small predicates (`is_ref`, `is_list`, ...) the
//! other seams build on.

use super::{ir_ty, Lowerer};
use crate::ast::*;
use crate::ir::IrTy;

impl Lowerer {
    /// Find the `Ty` for an already-declared type, by name.
    ///
    /// The arena is populated by monomorphisation from types the source
    /// spells. A container the source only implies -- `List<K>` behind
    /// `Map<K, V>.keys()` -- has a declaration but no arena entry until
    /// something asks for one.
    pub(super) fn ty_named(&mut self, name: &str) -> Option<Ty> {
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
    pub(super) fn list_of(&mut self, elem: Ty) -> Option<Ty> {
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

    /// A type's name, for diagnostics. `Ty::name()` cannot do this because it
    /// has no access to the interning arena.
    /// A type's name as a reader wrote it.
    ///
    /// Declared types are interned module-qualified (`lib#Point`) so that two
    /// modules may each declare a `Point`. Nobody should ever see that
    /// spelling: inside its own module it is `Point`, and from outside it is
    /// `lib.Point`, which is how it would be written.
    /// A type as the source spells it: `List<int>`, `Map<str, lib.Point>`.
    /// A type with its article, for a sentence that reads: "this lambda's
    /// body is an int". Only the vowel matters, and only the spelling the
    /// reader sees is looked at.
    pub(super) fn a_ty(&self, t: Ty) -> String {
        let n = self.tyname(t);
        let a = if n.starts_with(['a', 'e', 'i', 'o', 'u']) {
            "an"
        } else {
            "a"
        };
        format!("{a} {n}")
    }

    pub(super) fn tyname(&self, t: Ty) -> String {
        match t {
            Ty::User(i) => self.show_name(&self.ty_exprs[i as usize].name),
            other => other.name().to_string(),
        }
    }

    /// A type's name for a diagnostic: module-qualified unless it is this
    /// module's own, and an instantiation spelled with its type arguments
    /// rather than its mangled C name. Every diagnostic that names a type
    /// goes through here or `tyname`, so none of them leaks a `$`.
    pub(super) fn show_name(&self, raw: &str) -> String {
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
    pub(super) fn bare_name(&self, raw: &str) -> String {
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
            // The method may be an instantiation too: `Picker.pick<int>`
            // for `Picker.pick$int`, never the mangled name.
            Some(m) => match self.shown.get(m) {
                Some((base, args)) => {
                    let args: Vec<String> = args.iter().map(|a| self.tyname(*a)).collect();
                    format!("{head}.{}<{}>", crate::ast::bare(base), args.join(", "))
                }
                None => format!("{head}.{m}"),
            },
            None => head,
        }
    }

    /// The representation of a surface type. A distinct type is represented
    /// exactly as its base -- that is the whole point: `distinct int Price`
    /// is an `i64` at runtime, with no object, no header and no refcount.
    pub(super) fn irty(&self, t: Ty) -> IrTy {
        match self.base_of(t) {
            Some(b) => self.irty(b),
            None => match self.value_enum_tid(t) {
                Some(tid) => IrTy::Val(tid),
                None => ir_ty(t),
            },
        }
    }

    /// This type's index in the type table if it is a value enum -- an enum
    /// laid out as a tag and a union and passed by copy, rather than a heap
    /// object (`value_enums`, and docs/value-enums.md).
    ///
    /// Distinctness is NOT resolved here: `irty` does that first, so that
    /// `distinct Option<int> Maybe` is represented exactly as its base.
    fn value_enum_tid(&self, t: Ty) -> Option<u32> {
        match t {
            Ty::User(i) => self
                .value_enum
                .get(&self.ty_exprs[i as usize].name)
                .copied(),
            _ => None,
        }
    }

    /// Whether a value of this type is a value enum, distinctness resolved.
    pub(super) fn is_value_enum(&self, t: Ty) -> bool {
        self.value_enum_tid(self.underlying(t)).is_some()
    }

    /// The base type of a distinct type, if it is one.
    pub(super) fn base_of(&self, t: Ty) -> Option<Ty> {
        let tid = self.tdef_of(t)?;
        self.distinct_base[tid as usize]
    }

    /// Strip distinctness down to the underlying ordinary type.
    pub(super) fn underlying(&self, t: Ty) -> Ty {
        match self.base_of(t) {
            Some(b) => self.underlying(b),
            None => t,
        }
    }

    /// The element type of a channel type, if it is one.
    /// Does this type need refcounting? A distinct type follows its base --
    /// `distinct int Price` is not a reference, however it is spelled.
    pub(super) fn is_ref(&self, t: Ty) -> bool {
        self.underlying(t).is_ref()
    }

    pub(super) fn chan_elem(&self, t: Ty) -> Option<Ty> {
        self.builtin_elem(t, "Chan$")
    }

    /// The element type of an `Array<T>` or `List<T>`, if it is one.
    pub(super) fn seq_elem(&self, t: Ty) -> Option<Ty> {
        self.builtin_elem(t, "Array$")
            .or_else(|| self.builtin_elem(t, "List$"))
    }

    pub(super) fn is_list(&self, t: Ty) -> bool {
        self.builtin_elem(t, "List$").is_some()
    }

    /// The key and value types of a `Map<K, V>`, if it is one.
    ///
    /// Both this and `builtin_elem` look through a distinct type, because
    /// `distinct List<int> Bag` IS a list: indexing it, iterating it and its
    /// built-in methods all work exactly as on the base. Identity is kept
    /// where it matters -- `assignable` still refuses a `Bag` for a
    /// `List<int>` -- and that check never asks this question.
    pub(super) fn map_kv(&self, t: Ty) -> Option<(Ty, Ty)> {
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

    /// Is `from` usable where `to` is expected?
    ///
    /// Identical types always. Beyond that, a concrete type is assignable to
    /// an interface when it has every required method with a matching
    /// signature -- structurally, with no `implements` clause, so a type
    /// written before the interface existed can satisfy it.
    pub(super) fn assignable(&self, from: Ty, to: Ty) -> bool {
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
    pub(super) fn mismatch(&self, want: Ty, got: Ty) -> String {
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
    pub(super) fn tdef_of(&self, t: Ty) -> Option<u32> {
        let Ty::User(i) = t else { return None };
        let name = &self.ty_exprs[i as usize].name;
        self.typedefs
            .iter()
            .position(|d| d.name == *name)
            .map(|x| x as u32)
    }

    /// The surface type of a field, recovered from the declaration.
    pub(super) fn field_ty(&self, tid: u32, idx: u32) -> Ty {
        self.field_surface[tid as usize][idx as usize]
    }

    pub(super) fn field_tyname(&self, tid: u32, idx: u32) -> String {
        self.tyname(self.field_ty(tid, idx))
    }
}
