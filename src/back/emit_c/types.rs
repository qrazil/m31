//! Type layouts and the per-type support code: structs, drop, walk and copy
//! functions, TypeInfos and vtables.

use super::*;

pub(super) fn emit_type_structs(o: &mut String, m: &Module) {
    // Value enums first, and in dependency order: they are plain C structs
    // with no header, they may appear as a FIELD of an ordinary type or as a
    // payload of another value enum, and C needs a complete type for both.
    // The order exists because the qualifying rule forbids a cycle -- an enum
    // that reaches itself through a payload would have no finite size, so it
    // is never a value enum (docs/value-enums.md).
    emit_value_enums(o, m);

    // One C struct per user type. Every object starts with the Obj header, so
    // a `ref` is always an `Obj *` and the field access casts.
    for (i, t) in m.types.iter().enumerate() {
        if t.is_value {
            continue; // emitted above, and it has no object
        }
        if t.is_interface {
            writeln!(
                o,
                "/* interface {} -- no layout; values are Obj * */",
                t.name
            )
            .unwrap();
            continue;
        }
        writeln!(o, "/* {} */", t.name).unwrap();
        writeln!(o, "typedef struct {{").unwrap();
        writeln!(o, "    Obj hdr;").unwrap();
        if t.is_enum {
            // The same tag and union a value enum has -- the ONLY difference
            // between the two is the header above and the heap below.
            emit_enum_body(o, t);
        }
        for (fname, fty) in &t.fields {
            let c = fty.c_name();
            let sep = if c.ends_with('*') { "" } else { " " };
            let fname = c_ident(fname);
            writeln!(o, "    {c}{sep}f_{fname};").unwrap();
        }
        writeln!(o, "}} T{i};").unwrap();
    }
    if !m.types.is_empty() {
        o.push('\n');
    }
}

pub(super) fn emit_type_support(o: &mut String, m: &Module) {
    // Function prototypes before the drop functions, which call a type's
    // destructor, and before the TypeInfos, whose vtables name methods.
    if !m.types.is_empty() {
        for f in &m.funcs {
            writeln!(o, "{};", signature(f)).unwrap();
        }
        o.push('\n');
    }

    // A drop function only exists for types that hold references or declare
    // a destructor; rc_dec checks for NULL, so types with neither pay a
    // branch instead of a call.
    for (i, t) in m.types.iter().enumerate() {
        if t.has_no_object() || !t.has_drop_fn() {
            continue;
        }
        writeln!(o, "static void drop_T{i}(Obj *o) {{").unwrap();
        if let Some(d) = &t.destructor {
            // The destructor runs FIRST, on a whole object: every field is
            // still alive, so it can use them -- close a descriptor, flush a
            // buffer -- and a field's own destructor runs only afterwards,
            // when the field is released below.
            //
            // The count is 0 here. The destructor borrows `this` like any
            // method, and its body may retain and release it (a local, an
            // argument); at 0 that pair would reach zero a second time and
            // free the object inside its own destructor. So it runs at 1 --
            // the borrow it is given -- and must leave it at exactly 1. More
            // means `this` was stored somewhere that outlives the call: a
            // dead object brought back, whose memory is about to be freed
            // under whoever holds it. That is a bug with no safe
            // continuation, so it traps (docs/destructors-decision.md).
            //
            // The plain store would also clear the frozen bit, but it never
            // has one to clear: a value that owns a resource can never be
            // frozen for good -- the compiler refuses the `const` and
            // rt_snapshot traps on what it cannot see -- and a `const` block
            // that froze one keeps it reachable until it thaws it, so no
            // frozen object reaches here.
            //
            // rt_drop_enter/rt_drop_leave bracket the call and nothing else.
            // This is the ONE place the runtime's iterative release reaches
            // the program, and inside it the program must behave exactly as
            // it does at the top level: what the body drops is released
            // there and then, not queued behind the walk that is releasing
            // this object. See runtime/rt.c, "a destructor's body is
            // top-level code", and docs/destructors-decision.md.
            writeln!(o, "    RcDrain rcd;").unwrap();
            writeln!(o, "    o->rc = 1;").unwrap();
            writeln!(o, "    rt_drop_enter(&rcd);").unwrap();
            writeln!(o, "    {}(o);", c_name(d)).unwrap();
            writeln!(o, "    rt_drop_leave(&rcd);").unwrap();
            writeln!(
                o,
                "    if (o->rc != 1) rt_trap(\"object resurrected in its destructor: \
                 `drop` stored `this` somewhere that outlives it\");"
            )
            .unwrap();
            writeln!(o, "    o->rc = 0;").unwrap();
        }
        if !t.needs_drop() {
            writeln!(o, "}}").unwrap();
            continue;
        }
        writeln!(o, "    T{i} *p = (T{i} *)o;").unwrap();
        if t.is_enum {
            // A slot may be null: a `case V(T x):` binding that crossed a
            // thread boundary was TAKEN out of the enum (ir::Inst::EnumTake),
            // which clears the slot so that this release does not run on a
            // value the enum no longer owns.
            emit_enum_slot_switch(o, t, "if ({p} != NULL) rc_dec({p});");
        }
        for (fname, fty) in &t.fields {
            if *fty == IrTy::Ref {
                let fname = c_ident(fname);
                writeln!(o, "    rc_dec(p->f_{fname});").unwrap();
            }
        }
        writeln!(o, "}}").unwrap();
    }
    if m.types.iter().any(TypeDef::has_drop_fn) {
        o.push('\n');
    }

    // A walk function reports the references an object holds, without
    // releasing them. rt_check_unique uses it to decide whether the whole
    // graph reachable from a value crossing a thread boundary is reachable
    // from anywhere else -- see runtime/rt.c. Same condition as the drop
    // function: a type with no reference fields needs neither.
    for (i, t) in m.types.iter().enumerate() {
        if t.has_no_object() || !t.needs_drop() {
            continue;
        }
        writeln!(
            o,
            "static void walk_T{i}(Obj *o, VisitFn visit, void *ctx) {{"
        )
        .unwrap();
        writeln!(o, "    T{i} *p = (T{i} *)o;").unwrap();
        if t.is_enum {
            emit_enum_slot_switch(o, t, "visit(ctx, {p});");
        }
        for (fname, fty) in &t.fields {
            if *fty == IrTy::Ref {
                let fname = c_ident(fname);
                writeln!(o, "    visit(ctx, p->f_{fname});").unwrap();
            }
        }
        writeln!(o, "}}").unwrap();
    }
    if m.types.iter().any(TypeDef::needs_drop) {
        o.push('\n');
    }

    // A copy function per concrete type, for a `const` binding that has to
    // snapshot a shared value (runtime/rt.c, rt_snapshot). Every type gets
    // one, reference fields or not: the runtime does not know a user type's
    // size, so even a struct of ints is copied here. The body is a struct
    // assignment -- the scalars -- then a fresh count, registration with the
    // runtime BEFORE any child is copied (so a cycle closes onto this copy),
    // and each reference replaced by its child's copy.
    for (i, t) in m.types.iter().enumerate() {
        if t.has_no_object() {
            continue;
        }
        writeln!(o, "static Obj *copy_T{i}(Obj *o, void *ctx) {{").unwrap();
        writeln!(o, "    T{i} *p = (T{i} *)o;").unwrap();
        writeln!(
            o,
            "    T{i} *n = (T{i} *)rt_alloc(sizeof(T{i}), p->hdr.ty);"
        )
        .unwrap();
        writeln!(o, "    Obj hdr = n->hdr;").unwrap();
        writeln!(o, "    *n = *p;").unwrap();
        writeln!(o, "    n->hdr = hdr;").unwrap();
        writeln!(o, "    rt_copy_register(ctx, o, (Obj *)n);").unwrap();
        if t.is_enum {
            emit_enum_slot_switch(o, t, "{n} = rt_copy_child(ctx, {p});");
        }
        for (fname, fty) in &t.fields {
            if *fty == IrTy::Ref {
                let fname = c_ident(fname);
                writeln!(o, "    n->f_{fname} = rt_copy_child(ctx, p->f_{fname});").unwrap();
            }
        }
        writeln!(o, "    return (Obj *)n;").unwrap();
        writeln!(o, "}}").unwrap();
    }

    // One TypeInfo per concrete type: its drop function and its vtable. The
    // vtable has one slot per distinct interface method name in the program,
    // so a dispatch index is a compile-time constant.
    for (i, t) in m.types.iter().enumerate() {
        if t.has_no_object() {
            continue;
        }
        let drop = if t.has_drop_fn() {
            format!("drop_T{i}")
        } else {
            "NULL".to_string()
        };
        let walk = if t.needs_drop() {
            format!("walk_T{i}")
        } else {
            "NULL".to_string()
        };
        // A name made of identifiers, dots, `<`, `>`, `,` and spaces, so it
        // needs no escaping as a C string.
        let resource = match &t.resource {
            Some(n) => format!("\"{n}\""),
            None => "NULL".to_string(),
        };
        // The reserved methods the runtime calls on this type (rt.h). No
        // cast: the emitted definition of `int P.cmp(P other)` already has
        // the prototype `int64_t (Obj *, Obj *)`, so a mismatch is a C
        // compile error here rather than a wrong call at run time -- which
        // is why these are TypeInfo fields and not vtable slots.
        let named = |f: &Option<String>| match f {
            Some(n) => c_name(n),
            None => "NULL".to_string(),
        };
        let reserved = format!("{}, {}, {}", named(&t.cmp), named(&t.hash), named(&t.eq));
        if m.iface_slots.is_empty() {
            writeln!(
                o,
                "static const TypeInfo ti_T{i} __attribute__((unused)) = \
                 {{ {drop}, NULL, {walk}, copy_T{i}, {resource}, {reserved} }};"
            )
            .unwrap();
        } else {
            let entries: Vec<String> = t
                .vtable
                .iter()
                .map(|e| match e {
                    Some(fname) => format!("(AnyFn){}", c_name(fname)),
                    None => "NULL".to_string(),
                })
                .collect();
            writeln!(
                o,
                "static const AnyFn vt_T{i}[] __attribute__((unused)) = {{ {} }};",
                entries.join(", ")
            )
            .unwrap();
            writeln!(
                o,
                "static const TypeInfo ti_T{i} __attribute__((unused)) = \
                 {{ {drop}, vt_T{i}, {walk}, copy_T{i}, {resource}, {reserved} }};"
            )
            .unwrap();
        }
    }
    if !m.types.iter().all(TypeDef::has_no_object) {
        o.push('\n');
    }
}

/// The C layout of every value enum: a tag and a union of the variants that
/// carry a payload (docs/value-enums.md §3).
///
/// ```c
/// typedef struct {
///     int64_t tag;
///     union { struct { int64_t p0; } v1; } u;
/// } T7v;
/// ```
///
/// The payload members are TYPED -- `int64_t`, `double`, `bool`, or another
/// value enum's struct -- rather than the boxed layout's machine-word slots.
/// That is what lets a payload be another value enum, and it also spares a
/// float payload the `rt_f2i` round trip a slot would cost.
///
/// A variant with no payload contributes no union member, and an enum where
/// no variant has one gets no union at all, because a union with no members
/// is not C.
///
/// Emitted in dependency order: a value enum may hold another, and C needs
/// the inner type complete. The order exists because the qualifying rule
/// admits no cycle.
pub(super) fn emit_value_enums(o: &mut String, m: &Module) {
    // Depth-first, so an enum is written after everything it contains. The
    // recursion terminates because the type graph among value enums is
    // acyclic by construction; `seen` also makes it linear.
    fn visit(o: &mut String, m: &Module, i: usize, seen: &mut Vec<bool>) {
        if seen[i] {
            return;
        }
        seen[i] = true;
        let t = &m.types[i];
        for v in &t.variants {
            for p in &v.payload {
                if let IrTy::Val(j) = p {
                    visit(o, m, *j as usize, seen);
                }
            }
        }
        writeln!(o, "/* {} -- a value: a tag and a union, by copy */", t.name).unwrap();
        writeln!(o, "typedef struct {{").unwrap();
        emit_enum_body(o, t);
        writeln!(o, "}} T{i}v;").unwrap();
    }

    let mut seen = vec![false; m.types.len()];
    for i in 0..m.types.len() {
        if m.types[i].is_value {
            visit(o, m, i, &mut seen);
        }
    }
    if m.types.iter().any(|t| t.is_value) {
        o.push('\n');
    }
}

/// An enum's tag and payload union -- the same for a value enum and a boxed
/// one, which is the point: the only difference between the two is the `Obj`
/// header a boxed one carries and the heap it lives on.
///
/// The payload members are TYPED and per variant, rather than the
/// machine-word slots shared between variants that boxed enums used to have.
/// That is what lets any enum's payload be a value enum -- it sits inline at
/// its own size -- so `Result<bytes, Error>` being a heap object no longer
/// drags `Error` back onto the heap with it. It also spares a `float` payload
/// the `rt_f2i` round trip a shared slot cost.
pub(super) fn emit_enum_body(o: &mut String, t: &TypeDef) {
    writeln!(o, "    int64_t tag;").unwrap();
    // The OS-errno variant (`TypeDef::os_errno_variant`) contributes no
    // union member at all: its payload lives inside the tag word itself,
    // packed there by `Inst::EnumPack` and read back by `Inst::EnumPayload`.
    // For `io.Error`/`net.Error`/`term.Error` it is the type's only
    // payload-carrying variant, so excluding it here is what takes the
    // whole type from a tag-plus-union to a bare 8-byte tag.
    let has_union = t
        .variants
        .iter()
        .enumerate()
        .any(|(tag, v)| !v.payload.is_empty() && !t.os_errno_variant(tag));
    if !has_union {
        return; // a union with no members is not C
    }
    writeln!(o, "    union {{").unwrap();
    for (tag, v) in t.variants.iter().enumerate() {
        if v.payload.is_empty() || t.os_errno_variant(tag) {
            continue;
        }
        let members: Vec<String> = v
            .payload
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let c = p.c_name();
                let sep = if c.ends_with('*') { "" } else { " " };
                format!("{c}{sep}p{k};")
            })
            .collect();
        writeln!(o, "        struct {{ {} }} v{tag};", members.join(" ")).unwrap();
    }
    writeln!(o, "    }} u;").unwrap();
}

/// Turn an IR function name into a C identifier.
///
/// Monomorphisation mangles with `$` (`unwrap$int`), methods are qualified
/// with `.` (`Rect.area`), and the synthesised entry point is `$main`. None
/// of those are valid in standard C -- gcc and clang accept `$` as a GNU
/// extension, but the whole point of the C backend is reaching compilers we
/// have not tested.
///
/// The two escapes are different lengths on purpose, so `a$b` and `a.b`
/// cannot both become the same C name. `emit` asserts no collision anyway.
/// The body of an enum's drop or walk: which payload slots hold references
/// depends on the tag, so both have to switch on it.
///
/// `action` is a template in which `{p}` stands for the payload member being
/// read out of `p` and `{n}` for the same member of `n`. Only the variants
/// that actually carry a reference get a case; the rest fall to the default
/// and do nothing.
pub(super) fn emit_enum_slot_switch(o: &mut String, t: &TypeDef, action: &str) {
    let any = t.variants.iter().any(|v| v.payload.contains(&IrTy::Ref));
    if !any {
        return;
    }
    writeln!(o, "    switch (p->tag) {{").unwrap();
    for (tag, v) in t.variants.iter().enumerate() {
        if !v.payload.contains(&IrTy::Ref) {
            continue;
        }
        writeln!(o, "    case {tag}:").unwrap();
        for (k, pty) in v.payload.iter().enumerate() {
            if *pty == IrTy::Ref {
                let act = action
                    .replace("{p}", &format!("p->u.v{tag}.p{k}"))
                    .replace("{n}", &format!("n->u.v{tag}.p{k}"));
                writeln!(o, "        {act}").unwrap();
            }
        }
        writeln!(o, "        break;").unwrap();
    }
    // Every other tag carries no reference. Named so the C compiler does not
    // warn about the enumeration being incompletely handled.
    writeln!(o, "    default: break;").unwrap();
    writeln!(o, "    }}").unwrap();
}

/// The same switch, inside an ordinary function body rather than a drop or
/// walk function: `p` is bound to the object first, in a block of its own so
/// the name cannot collide with a second use in the same function.
pub(super) fn emit_enum_slot_switch_on(
    o: &mut String,
    t: &TypeDef,
    obj: &Value,
    tid: u32,
    action: &str,
) {
    if !t.variants.iter().any(|v| v.payload.contains(&IrTy::Ref)) {
        return;
    }
    writeln!(o, "    {{").unwrap();
    writeln!(o, "    T{tid} *p = (T{tid} *){obj};").unwrap();
    emit_enum_slot_switch(o, t, action);
    writeln!(o, "    }}").unwrap();
}

/// Put a value into a generic slot -- a collection's element, an enum's
/// payload -- which is always one machine word.
///
/// A reference rides as its pointer. A double does NOT fit by conversion,
/// only by bit pattern, so it goes through rt_f2i; `(int64_t)3.5` would
/// store 3.
pub(super) fn to_slot(expr: &str, t: IrTy) -> String {
    match t {
        IrTy::Ref => format!("(int64_t)(intptr_t){expr}"),
        IrTy::F64 => format!("rt_f2i({expr})"),
        // A value enum never reaches a slot: the lowering takes its type back
        // to a boxed enum first, and `Module::verify` says so. Spelled out
        // rather than left to the catch-all, which would have written a cast
        // from a struct and left it to the C compiler to complain.
        IrTy::Val(_) => unreachable!("a value enum has no machine-word slot"),
        IrTy::I64 | IrTy::I1 => format!("(int64_t){expr}"),
    }
}

/// Read a value back out of a generic slot, undoing `to_slot`.
pub(super) fn from_slot(expr: &str, t: IrTy) -> String {
    match t {
        IrTy::Ref => format!("(Obj *)(intptr_t){expr}"),
        IrTy::F64 => format!("rt_i2f({expr})"),
        IrTy::I1 => format!("(bool){expr}"),
        IrTy::I64 => expr.to_string(),
        // A value enum never reaches a slot: the lowering demotes its type
        // back to a boxed enum first, and `Module::verify` says so.
        IrTy::Val(_) => unreachable!("a value enum has no machine-word slot"),
    }
}
