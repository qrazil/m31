//! Heap watch's static data: a TypeInfo per allocation site, the program's
//! source lines, and the call that hands both to the runtime
//! (docs/heap-watch-design.md, "The ledger"; runtime/heapwatch.c).
//!
//! An allocation site of a user type is a (type, line) pair, found from the
//! `Inst::Site` the lowering puts before every allocation. Each gets a copy
//! of the type's TypeInfo with its own ledger slot, numbered 1, 2, ... in
//! the order the functions are emitted, so the numbering is the same on
//! every run (the reproducibility gate). The runtime's own objects -- a str,
//! a List -- are counted against the line the program last passed to
//! `rt_hw_at`, when they are sampled.
//!
//! All of it is for a heap watch build (`-DRT_HEAPWATCH`): the records and
//! tables are inside `#if RT_HEAPWATCH`, an allocation names its record as
//! `RT_HW_TI(site, type)`, and every hook is a macro rt.h makes nothing in
//! any other build.
//!
//! Every line written here that depends on line numbers or site numbers
//! carries the comment `/*hwsite*/`, or a `_s<N>` name, so a gate that
//! compares emitted C across a reformatting can blank those numbers.

use std::collections::HashMap;

use super::*;

/// The allocation sites of user types, in emission order.
pub(super) struct Sites {
    /// (type id, loc) -> slot.
    pub slot: HashMap<(u32, u32), u32>,
    /// Slot `n` is `order[n - 1]`.
    pub order: Vec<(u32, u32)>,
}

impl Sites {
    pub fn find(m: &Module) -> Sites {
        let mut s = Sites {
            slot: HashMap::new(),
            order: Vec::new(),
        };
        for f in &m.funcs {
            for b in &f.blocks {
                let mut loc = 0;
                for i in &b.insts {
                    let tid = match i {
                        Inst::Site { loc: l } => {
                            loc = *l;
                            continue;
                        }
                        Inst::Alloc { tid, .. }
                            if !m.types[*tid as usize].is_immortal_singleton() =>
                        {
                            *tid
                        }
                        Inst::EnumPack { tid, tag, .. }
                            if !m.types[*tid as usize].is_value
                                && !m.types[*tid as usize].is_immortal_variant(*tag as usize) =>
                        {
                            *tid
                        }
                        Inst::EnumClone { tid, .. } => *tid,
                        _ => continue,
                    };
                    s.add(tid, loc);
                }
            }
        }
        s
    }

    fn add(&mut self, tid: u32, loc: u32) {
        if self.slot.contains_key(&(tid, loc)) {
            return;
        }
        self.order.push((tid, loc));
        self.slot.insert((tid, loc), self.order.len() as u32);
    }

    /// The TypeInfo an allocation of `tid` at `loc` passes: the site's own
    /// record in a heap watch build, the type's otherwise (rt.h, RT_HW_TI).
    pub fn ti(&self, tid: u32, loc: u32) -> String {
        match self.slot.get(&(tid, loc)) {
            Some(n) => format!("RT_HW_TI(ti_T{tid}_s{n}, ti_T{tid})"),
            // Not reached: `find` saw every allocation the emitter emits.
            None => format!("&ti_T{tid}"),
        }
    }
}

/// What an instruction's code needs from heap watch: the site records, the
/// line it is at, and whether the object it stores into was just made
/// (emit_func).
pub(super) struct At<'a> {
    pub sites: &'a Sites,
    pub loc: u32,
    pub into_fresh: bool,
}

/// The reference fields of type `i`, for trace to name a holder's slot by:
/// `hwf_T<i>`. Returns the number of entries (0: no table emitted).
pub(super) fn emit_field_table(o: &mut String, i: usize, t: &TypeDef) -> usize {
    let mut entries = Vec::new();
    if t.is_enum {
        for (tag, v) in t.variants.iter().enumerate() {
            for (k, p) in v.payload.iter().enumerate() {
                if *p == IrTy::Ref {
                    entries.push(format!(
                        "{{ \"{}.{k}\", offsetof(T{i}, u.v{tag}.p{k}), {tag} }}",
                        v.name
                    ));
                }
            }
        }
    }
    for (fname, fty) in &t.fields {
        if *fty == IrTy::Ref {
            entries.push(format!(
                "{{ \"{}\", offsetof(T{i}, f_{}), -1 }}",
                fname.replace('$', "_"),
                c_ident(fname)
            ));
        }
    }
    if entries.is_empty() {
        return 0;
    }
    writeln!(
        o,
        "static const RtField hwf_T{i}[] __attribute__((unused)) = {{ {} }};",
        entries.join(", ")
    )
    .unwrap();
    entries.len()
}

/// A type's name as a C string literal. Names are identifiers, dots, `<`,
/// `>`, `,` and spaces, but a quote or a backslash costs nothing to rule out.
pub(super) fn name_literal(t: &TypeDef) -> String {
    let name: String = t
        .display
        .chars()
        .filter(|c| *c != '"' && *c != '\\')
        .collect();
    format!("\"{name}\"")
}

/// The per-site TypeInfos: the type's own initialiser, with the site's slot
/// and line. `body` is the part every record of the type shares.
/// Only a heap watch build has them (`#if RT_HEAPWATCH`).
pub(super) fn emit_site_infos(o: &mut String, sites: &Sites, tid: usize, tail: &str, body: &str) {
    let mut any = false;
    for (n, (t, loc)) in sites.order.iter().enumerate() {
        if *t as usize != tid {
            continue;
        }
        if !any {
            any = true;
            writeln!(o, "#if RT_HEAPWATCH").unwrap();
        }
        let slot = n + 1;
        writeln!(
            o,
            "static const TypeInfo ti_T{tid}_s{slot} __attribute__((unused)) = \
             {{ {body}, &ti_T{tid}, {slot}, {loc}, {tail} }}; /*hwsite*/"
        )
        .unwrap();
    }
    if any {
        writeln!(o, "#endif").unwrap();
    }
}

/// The program's source lines and its site records, and the call that hands
/// them to the runtime from `main`; in a heap watch build only.
pub(super) fn emit_tables(o: &mut String, m: &Module, sites: &Sites) {
    writeln!(o, "#if RT_HEAPWATCH").unwrap();
    writeln!(o, "static const RtHwLoc hw_locs[] = {{").unwrap();
    for (file, line) in &m.locs {
        writeln!(
            o,
            "    {{ {}, {line} }}, /*hwsite*/",
            c_string_literal(file.as_bytes())
        )
        .unwrap();
    }
    writeln!(o, "}};").unwrap();
    if sites.order.is_empty() {
        // A zero-length array is not C; the runtime is told there are none.
        writeln!(o, "static const TypeInfo *const hw_slots[1] = {{ NULL }};").unwrap();
    } else {
        writeln!(o, "static const TypeInfo *const hw_slots[] = {{").unwrap();
        for (n, (tid, _)) in sites.order.iter().enumerate() {
            writeln!(o, "    &ti_T{tid}_s{}, /*hwsite*/", n + 1).unwrap();
        }
        writeln!(o, "}};").unwrap();
    }
    writeln!(o, "#endif").unwrap();
    o.push('\n');
}

/// The lines in `main` that start heap watch, in a heap watch build.
pub(super) fn init_call(m: &Module, sites: &Sites) -> String {
    format!(
        "#if RT_HEAPWATCH\n    rt_hw_program(hw_locs, {}, hw_slots, {}); /*hwsite*/\n#endif",
        m.locs.len(),
        sites.order.len()
    )
}
