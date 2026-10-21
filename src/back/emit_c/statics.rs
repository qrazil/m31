//! Static data: immortal singletons, string literals and the constant tables.

use super::*;

pub(super) fn emit_immortals(o: &mut String, m: &Module) {
    // One static instance per field-less type, on the same terms as a string
    // literal: static storage, RC_IMMORTAL, never freed, never counted by
    // RC_TRACK_ALLOC. `Inst::Alloc` on such a type yields this object instead
    // of calling rt_alloc -- see TypeDef::is_immortal_singleton for why every
    // value of the type may share one, and which types are excluded.
    //
    // Deliberately NOT `const`: rc_inc and rc_dec take a non-const `Obj *`
    // and read the count before they see RC_IMMORTAL and return. They never
    // write to it, but a const object passed to them would be a const-cast,
    // and a write the compiler thought impossible would be undefined rather
    // than merely wrong. The TypeInfo it points at IS const; that is the part
    // nothing ever writes through.
    //
    // It must come after the TypeInfos, because it names one.
    let singletons: Vec<usize> = m
        .types
        .iter()
        .enumerate()
        .filter(|(_, t)| t.is_immortal_singleton())
        .map(|(i, _)| i)
        .collect();
    for i in &singletons {
        writeln!(
            o,
            "static T{i} imm_T{i} __attribute__((unused)) = \
             {{ {{ RC_IMMORTAL, &ti_T{i} }} }};"
        )
        .unwrap();
    }
    if !singletons.is_empty() {
        o.push('\n');
    }

    // One static instance per payload-free variant of a boxed enum, on the
    // same terms as the field-less singletons above -- see
    // `TypeDef::is_immortal_variant` (docs/perf-board.md item 5). `Inst::
    // EnumPack` yields this object instead of calling `rt_alloc` whenever it
    // constructs one of these tags. The union member, when the type has one
    // at all (another variant of the same enum may carry a payload even
    // though this one does not), is zeroed explicitly rather than left for
    // C's own elision: gcc's `-Wmissing-field-initializers` (on under
    // `-Wextra`, which `run.sh` treats as a failure) warns on the elided
    // form, and nothing ever reads this member anyway, because a
    // payload-free tag has no union arm to read.
    let immortal_variants: Vec<(usize, usize)> = m
        .types
        .iter()
        .enumerate()
        .flat_map(|(i, t)| {
            (0..t.variants.len())
                .filter(move |&tag| t.is_immortal_variant(tag))
                .map(move |tag| (i, tag))
        })
        .collect();
    for (i, tag) in &immortal_variants {
        let has_union = m.types[*i].variants.iter().any(|v| !v.payload.is_empty());
        // Two brace levels: one for the union, one for its first member,
        // which is itself a per-variant struct -- gcc's `-Wmissing-braces`
        // (also under `-Wextra`) wants the nesting spelled out explicitly.
        let u = if has_union { ", { { 0 } }" } else { "" };
        writeln!(
            o,
            "static T{i} imm_T{i}_v{tag} __attribute__((unused)) = \
             {{ {{ RC_IMMORTAL, &ti_T{i} }}, {tag}{u} }};"
        )
        .unwrap();
    }
    if !immortal_variants.is_empty() {
        o.push('\n');
    }

    // String literals are immortal: static storage, RC_IMMORTAL, never freed.
    for (i, s) in m.strings.iter().enumerate() {
        let bytes = s.as_bytes();
        writeln!(
            o,
            "static Str str{i} = {{ {{ RC_IMMORTAL, &rt_str_type }}, {}, {} }};",
            bytes.len(),
            c_string_literal(bytes)
        )
        .unwrap();
    }
    if !m.strings.is_empty() {
        o.push('\n');
    }
}

/// A finite double as a C literal that reads back as the same bits: 17
/// significant digits always round-trip, and C's conversion of a decimal
/// literal is correctly rounded. A constant is never a NaN or an infinity
/// (the compiler refuses one).
fn c_double(x: f64) -> String {
    format!("{x:.16e}")
}

/// Module constants' collections, as static data -- docs/reference.md §4.4.
///
/// Each is laid out exactly as the runtime lays out the same collection
/// built at run time, with a count of RC_IMMORTAL, so every read path is the
/// ordinary one and no initialisation code runs: there is no order in which
/// constants come into existence, because they all exist before `main`.
///
/// `static const`, so they land in read-only memory: a write the runtime's
/// trap somehow missed faults instead of corrupting a table every thread
/// reads. `__attribute__((unused))` because a constant nobody reads is still
/// checked and still emitted, and gcc warns about an unused static const in
/// C.
///
/// An array is its own anonymous struct with the element count spelled out,
/// rather than the runtime's `Arr`: `Arr` ends in a flexible array member,
/// and initialising one statically is a GNU extension. The layout is the
/// same -- header, length, then the slots -- and the runtime only ever sees
/// the object through an `Obj *` in its own translation unit.
pub(super) fn emit_statics(o: &mut String, m: &Module) {
    use crate::middle::ir::{StaticObj, StaticSlot};
    let slot = |s: &StaticSlot| match s {
        StaticSlot::Word(v) if *v == i64::MIN => "INT64_MIN".to_string(),
        StaticSlot::Word(v) => format!("INT64_C({v})"),
        StaticSlot::Str(i) => format!("(int64_t)(intptr_t)&str{i}"),
        StaticSlot::Obj(i) => format!("(int64_t)(intptr_t)&k{i}"),
    };
    // Eight to a line: the float-formatting table has 684 entries, and one
    // line of it would be unreadable in a debugger.
    let rows = |items: Vec<String>| -> String {
        items
            .chunks(8)
            .map(|c| format!("        {}", c.join(", ")))
            .collect::<Vec<_>>()
            .join(",\n")
    };
    for (i, k) in m.statics.iter().enumerate() {
        match k {
            StaticObj::Array { refs, slots } => {
                let ti = if *refs {
                    "rt_arr_ref_type"
                } else {
                    "rt_arr_val_type"
                };
                // A zero-length array member is not standard C either, so an
                // empty constant carries one slot it never reads.
                let cap = slots.len().max(1);
                let body = if slots.is_empty() {
                    "        0".to_string()
                } else {
                    rows(slots.iter().map(slot).collect())
                };
                writeln!(
                    o,
                    "static const struct {{ Obj hdr; int64_t len; int64_t data[{cap}]; }} \
                     k{i} __attribute__((unused)) = {{\n    {{ RC_IMMORTAL, &{ti} }}, {},\n    {{\n{body}\n    }}\n}};",
                    slots.len()
                )
                .unwrap();
            }
            // The slots in a buffer of their own, as `rt_list_new` keeps
            // them, with `cap` equal to `len`: nothing will ever push.
            StaticObj::List { refs, slots } => {
                let ti = if *refs {
                    "rt_lst_ref_type"
                } else {
                    "rt_lst_val_type"
                };
                let data = if slots.is_empty() {
                    "NULL".to_string()
                } else {
                    writeln!(
                        o,
                        "static const int64_t k{i}_data[{}] = {{\n{}\n}};",
                        slots.len(),
                        rows(slots.iter().map(slot).collect())
                    )
                    .unwrap();
                    format!("(int64_t *)k{i}_data")
                };
                writeln!(
                    o,
                    "static const Lst k{i} __attribute__((unused)) = \
                     {{ {{ RC_IMMORTAL, &{ti} }}, {n}, {n}, {data} }};",
                    n = slots.len()
                )
                .unwrap();
            }
            // `data` is never NULL for a bytes (rt.h), so an empty one still
            // points at a byte it never reads.
            StaticObj::Bytes(bs) => {
                let body: Vec<String> = if bs.is_empty() {
                    vec!["0".to_string()]
                } else {
                    bs.iter().map(|b| b.to_string()).collect()
                };
                writeln!(
                    o,
                    "static const uint8_t k{i}_data[{}] = {{\n{}\n}};",
                    body.len(),
                    rows(body)
                )
                .unwrap();
                writeln!(
                    o,
                    "static const Bytes k{i} __attribute__((unused)) = \
                     {{ {{ RC_IMMORTAL, &rt_bytes_type }}, {n}, {n}, (uint8_t *)k{i}_data }};",
                    n = bs.len()
                )
                .unwrap();
            }
            StaticObj::Map {
                key_is_str,
                val_is_ref,
                len,
                table,
            } => {
                // An empty map has no table at all, which is also how
                // `rt_map_new` leaves one until its first `set`.
                let slots_ref = if table.is_empty() {
                    "NULL".to_string()
                } else {
                    let entries: Vec<String> = table
                        .iter()
                        .map(|e| match e {
                            Some((k, v)) => format!("{{ {}, {}, SLOT_FULL }}", slot(k), slot(v)),
                            None => "{ 0, 0, SLOT_EMPTY }".to_string(),
                        })
                        .collect();
                    writeln!(
                        o,
                        "static const MapSlot k{i}_slots[{}] = {{\n{}\n}};",
                        table.len(),
                        rows(entries)
                    )
                    .unwrap();
                    format!("(MapSlot *)k{i}_slots")
                };
                // `used` is `len`: nothing was ever removed, so there are no
                // tombstones. The key is a reference exactly when it is a str.
                // A user-typed key (MK_OBJ) cannot appear: the compiler
                // would have to run the program's own `hash` to lay the
                // table out, so `check_const_ty` refuses it.
                writeln!(
                    o,
                    "static const Map k{i} __attribute__((unused)) = \
                     {{ {{ RC_IMMORTAL, &rt_map_type }}, {slots_ref}, {cap}, {len}, {len}, \
                     {kind}, {ks}, {vr} }};",
                    cap = table.len(),
                    kind = if *key_is_str { "MK_STR" } else { "MK_INT" },
                    ks = key_is_str,
                    vr = val_is_ref,
                )
                .unwrap();
            }
            // A user struct, laid out as `Inst::Alloc` would leave it: the
            // header, then each field as the struct declares it. A field
            // pointing at another static is `(Obj *)&k{j}`, which comes
            // first, because a constant's parts are laid out before it.
            StaticObj::Struct { tid, slots } => {
                let t = &m.types[*tid as usize];
                assert_eq!(t.fields.len(), slots.len(), "one slot per field");
                let mut parts = vec![format!("{{ RC_IMMORTAL, &ti_T{tid} }}")];
                for ((_, fty), s) in t.fields.iter().zip(slots) {
                    parts.push(match (fty, s) {
                        (IrTy::F64, StaticSlot::Word(v)) => c_double(f64::from_bits(*v as u64)),
                        (IrTy::I1, StaticSlot::Word(v)) => {
                            (if *v != 0 { "true" } else { "false" }).to_string()
                        }
                        (IrTy::Ref, StaticSlot::Str(i)) => format!("(Obj *)&str{i}"),
                        (IrTy::Ref, StaticSlot::Obj(i)) => format!("(Obj *)&k{i}"),
                        (_, s) => slot(s),
                    });
                }
                writeln!(
                    o,
                    "static const T{tid} k{i} __attribute__((unused)) = {{\n    {}\n}};",
                    parts.join(",\n    ")
                )
                .unwrap();
            }
        }
    }
    if !m.statics.is_empty() {
        o.push('\n');
    }
}
