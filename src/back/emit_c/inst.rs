//! Instructions: one IR instruction to C, and the jumps between blocks.

use super::types::{emit_enum_slot_switch_on, from_slot, to_slot};
use super::*;

pub(super) fn emit_inst(
    o: &mut String,
    f: &crate::middle::ir::Func,
    types: &[TypeDef],
    i: &Inst,
    ffi: &Ffi,
) {
    match i {
        Inst::FConst { dst, val } => {
            // 17 significant digits round-trips any double exactly, and the
            // hex form would be exact but unreadable in emitted source.
            // `inf` and `nan` cannot appear: a literal that does not fit is
            // rejected by the lexer, and arithmetic producing them happens at
            // run time, not here.
            writeln!(o, "    {dst} = {val:.17e};").unwrap();
        }
        Inst::IConst { dst, val } => {
            // INT64_MIN has no positive literal form in C; write it as an
            // expression the preprocessor already defines correctly.
            if *val == i64::MIN {
                writeln!(o, "    {dst} = INT64_MIN;").unwrap();
            } else {
                writeln!(o, "    {dst} = INT64_C({val});").unwrap();
            }
        }
        Inst::BConst { dst, val } => {
            writeln!(o, "    {dst} = {};", if *val { "true" } else { "false" }).unwrap();
        }
        Inst::SConst { dst, idx } => {
            writeln!(o, "    {dst} = (Obj *)&str{idx};").unwrap();
        }
        // The static is `const` -- it lives in read-only memory -- and an
        // `Obj *` is not. Casting the qualifier away is defined as long as
        // nothing writes through the pointer, and nothing does: the refcount
        // operations return early on RC_IMMORTAL, and every runtime path that
        // would change a collection traps on one first (runtime/rt.c,
        // `rt_check_mutable`).
        Inst::KConst { dst, idx } => {
            writeln!(o, "    {dst} = (Obj *)&k{idx};").unwrap();
        }
        Inst::Arith { dst, op, lhs, rhs } => {
            if f.ty_of(*dst) == IrTy::F64 {
                // IEEE: no trap, no checked helper. Overflow is an infinity
                // and 0.0/0.0 is a NaN, which is the defined answer rather
                // than a fault -- unlike the integer case, where wrapping
                // would be the silent wrong answer.
                writeln!(o, "    {dst} = {lhs} {} {rhs};", op.c_op()).unwrap();
            } else {
                writeln!(o, "    {dst} = {}({lhs}, {rhs});", op.rt_fn()).unwrap();
            }
        }
        Inst::ICmp { dst, cmp, lhs, rhs } => {
            // NOT for a float: `x == x` is FALSE when x is a NaN, so the
            // answer does depend on the value and the fold would be wrong.
            // The C compilers do not warn about a self-comparison on a
            // double for exactly the same reason.
            if lhs == rhs && f.ty_of(*lhs) != IrTy::F64 {
                // `x == x` is a warning under -Wall (-Wtautological-compare)
                // on both gcc and clang, and the warning gate reads any
                // warning as a failure. The answer does not depend on the
                // value, so emit the answer. Comparing a name with itself is
                // odd but legal, and the emitter is not the place to say so.
                let val = cmp.holds_for_equal_operands();
                writeln!(o, "    {dst} = {};", if val { "true" } else { "false" }).unwrap();
            } else {
                writeln!(o, "    {dst} = ({lhs} {} {rhs});", cmp.c_op()).unwrap();
            }
        }
        Inst::Not { dst, src } => {
            writeln!(o, "    {dst} = !{src};").unwrap();
        }
        Inst::Call { dst, func, args } => {
            // Blocking FFI (docs/concurrency-decision.md, "Blocking FFI",
            // Phase 3): a genuine `prim` call -- the ONLY way emitted code
            // reaches outside the runtime's own control, where `O_NONBLOCK`
            // is a no-op on a regular file and `getaddrinfo` is synchronous
            // by specification -- is wrapped in rt_enter_blocking() and
            // rt_exit_blocking(), unconditionally, every single call site,
            // the same "always correct, always emitted" discipline the
            // stack probe (emit_func, above) uses. `prim_targets` is exactly
            // the set of runtime symbols a `prim` declaration's call sites
            // produce (see `ir::Module::prim_targets`'s doc comment for why
            // an ordinary internal runtime helper call like `rt_concat` or
            // `rt_chan_send` -- same `rt_`-prefixed shape at this point --
            // is NOT in this set and so is correctly left unwrapped: it
            // never leaves this runtime's own control and so can never
            // block on an external syscall the way a `prim` can).
            let is_ffi = ffi.prim_targets.contains(func.as_str());
            if let Some(n) = ffi.natives.get(func.as_str()) {
                // `prim "c"` (docs/ffi-decision.md): the checked native call,
                // wrapped in the blocking handoff unless `nonblocking`. The
                // wrap goes outside the checks' block, around the call only,
                // by being part of the same text; see native.rs.
                native::emit_native_call(o, n, func, dst.as_ref(), args, is_ffi);
                return;
            }
            if is_ffi {
                writeln!(o, "    rt_enter_blocking();").unwrap();
            }
            // Runtime helpers are already C names; user functions are IR
            // names and need escaping and the prefix.
            let callee = if func.starts_with("rt_") {
                func.clone()
            } else {
                c_name(func)
            };
            // The channel runtime is typed in terms of Chan *, and a slot is
            // 64 bits: the compiler knows the element type statically, so a
            // reference rides as its pointer with no tagging.
            let a: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(i, v)| match (func.as_str(), i) {
                    ("rt_chan_send", 0) | ("rt_chan_recv", 0) | ("rt_chan_close", 0) => {
                        format!("(Chan *){v}")
                    }
                    // Everything below takes a generic slot: one machine
                    // word whatever the element type is.
                    ("rt_chan_send", 1)
                    | ("rt_index_set", 2)
                    // The hoisted-pointer accessor (src/middle/hoist.rs,
                    // "rt_index_set_at(o, ptr, len, i, v)"): same value,
                    // shifted two slots by the extra `ptr`/`len` arguments.
                    | ("rt_index_set_at", 4)
                    | ("rt_list_push", 1)
                    | ("rt_list_insert", 2)
                    | ("rt_seq_contains", 1)
                    | ("rt_seq_index_of", 1)
                    | ("rt_array_new", 1)
                    | ("rt_list_repeat", 1)
                    | ("rt_array_put", 2)
                    | ("rt_map_set", 1)
                    | ("rt_map_set", 2)
                    | ("rt_map_get", 1)
                    | ("rt_map_has", 1)
                    | ("rt_map_remove", 1)
                        if matches!(f.ty_of(*v), IrTy::Ref | IrTy::F64) =>
                    {
                        to_slot(&v.to_string(), f.ty_of(*v))
                    }
                    _ => v.to_string(),
                })
                .collect();
            match dst {
                Some(d)
                    if matches!(
                        func.as_str(),
                        "rt_chan_recv"
                            | "rt_index_get"
                            | "rt_index_get_at"
                            | "rt_list_pop"
                            | "rt_list_remove_at"
                            | "rt_map_get"
                    ) && matches!(f.ty_of(*d), IrTy::Ref | IrTy::F64) =>
                {
                    let call = format!("{callee}({})", a.join(", "));
                    writeln!(o, "    {d} = {};", from_slot(&call, f.ty_of(*d))).unwrap()
                }
                Some(d)
                    if matches!(
                        func.as_str(),
                        "rt_chan_new"
                            | "rt_array_new"
                            | "rt_array_blank"
                            | "rt_list_new"
                            | "rt_list_repeat"
                            | "rt_map_keys"
                            | "rt_map_values"
                    ) =>
                {
                    writeln!(o, "    {d} = (Obj *){callee}({});", a.join(", ")).unwrap()
                }
                Some(d) => writeln!(o, "    {d} = {callee}({});", a.join(", ")).unwrap(),
                None => writeln!(o, "    {callee}({});", a.join(", ")).unwrap(),
            }
            if is_ffi {
                writeln!(o, "    rt_exit_blocking();").unwrap();
            }
        }
        Inst::Alloc { dst, tid } => {
            // A field-less type is built once, statically, and every
            // construction hands back that one immortal object -- no malloc,
            // no refcount traffic, nothing to free. The lowering does not
            // need to know: it still emits an Alloc and the rc_inc/rc_dec
            // pairs around it, and both are a compare-and-return on an
            // immortal. See TypeDef::is_immortal_singleton.
            if types[*tid as usize].is_immortal_singleton() {
                writeln!(o, "    {dst} = &imm_T{tid}.hdr;").unwrap();
            } else {
                writeln!(o, "    {dst} = rt_alloc(sizeof(T{tid}), &ti_T{tid});").unwrap();
            }
        }
        Inst::EnumPack {
            dst,
            tid,
            tag,
            args,
        } => {
            // A value enum is built in place: no allocation, no refcount, no
            // slot packing -- each payload goes into its own typed member of
            // the variant's union arm (docs/value-enums.md §3).
            if types[*tid as usize].is_value {
                // The OS-errno variant has no union member at all
                // (`emit_enum_body`) -- its one `int` argument is the errno,
                // and it goes straight into the tag's low 32 bits instead,
                // behind the reserved high bit. See `TypeDef::os_errno_variant`.
                if types[*tid as usize].os_errno_variant(*tag as usize) {
                    writeln!(
                        o,
                        "    {dst}.tag = INT64_C({base}) | (int64_t)(uint32_t){arg};",
                        base = crate::middle::ir::OS_ERRNO_TAG_BASE,
                        arg = args[0],
                    )
                    .unwrap();
                    return;
                }
                writeln!(o, "    {dst}.tag = {tag};").unwrap();
                for (k, a) in args.iter().enumerate() {
                    writeln!(o, "    {dst}.u.v{tag}.p{k} = {a};").unwrap();
                }
                return;
            }
            // A payload-free tag of a boxed enum has nothing to distinguish
            // one construction from another, so every one of them yields the
            // same shared, immortal object instead of a fresh `rt_alloc` --
            // see `TypeDef::is_immortal_variant` (docs/perf-board.md item 5).
            // `Option.None` is the case this exists for.
            if types[*tid as usize].is_immortal_variant(*tag as usize) {
                writeln!(o, "    {dst} = &imm_T{tid}_v{tag}.hdr;").unwrap();
                return;
            }
            writeln!(o, "    {dst} = rt_alloc(sizeof(T{tid}), &ti_T{tid});").unwrap();
            writeln!(o, "    ((T{tid} *){dst})->tag = {tag};").unwrap();
            for (k, a) in args.iter().enumerate() {
                writeln!(o, "    ((T{tid} *){dst})->u.v{tag}.p{k} = {a};").unwrap();
            }
        }
        Inst::ParseInto { ok, dst, func, src } => {
            writeln!(o, "    {ok} = {func}({src}, &{dst});").unwrap();
        }
        Inst::EnumTag { dst, obj, tid } => {
            if types[*tid as usize].is_value {
                writeln!(o, "    {dst} = {obj}.tag;").unwrap();
                return;
            }
            writeln!(o, "    {dst} = ((T{tid} *){obj})->tag;").unwrap();
        }
        Inst::EnumPayload {
            dst,
            obj,
            tid,
            tag,
            idx,
        } => {
            // For a value enum the member is already the right type, so there
            // is nothing to unpack: the tag the lowering had in hand picks the
            // union arm.
            if types[*tid as usize].is_value {
                // The OS-errno variant's payload has no union member to read
                // (`emit_enum_body`) -- it lives in the tag's low 32 bits,
                // behind the reserved high bit `Inst::EnumPack` set.
                if types[*tid as usize].os_errno_variant(*tag as usize) {
                    writeln!(o, "    {dst} = {obj}.tag & INT64_C(0xFFFFFFFF);").unwrap();
                    return;
                }
                writeln!(o, "    {dst} = {obj}.u.v{tag}.p{idx};").unwrap();
                return;
            }
            writeln!(o, "    {dst} = ((T{tid} *){obj})->u.v{tag}.p{idx};").unwrap();
        }
        // A second object with the same tag and the same payload, each
        // reference in it retained once more -- `clone` on an enum. Which
        // slots hold references depends on the tag, so the retains switch on
        // it, the same way the drop and walk functions do.
        Inst::EnumClone { dst, src, tid } => {
            let t = &types[*tid as usize];
            writeln!(o, "    {dst} = rt_alloc(sizeof(T{tid}), &ti_T{tid});").unwrap();
            writeln!(o, "    ((T{tid} *){dst})->tag = ((T{tid} *){src})->tag;").unwrap();
            if t.variants.iter().any(|v| !v.payload.is_empty()) {
                // One assignment: the union is whatever the tag says it is,
                // and copying the whole of it copies that.
                writeln!(o, "    ((T{tid} *){dst})->u = ((T{tid} *){src})->u;").unwrap();
            }
            emit_enum_slot_switch_on(o, t, dst, *tid, "rc_inc({p});");
        }
        Inst::EnumTake { obj, tid, tag, idx } => {
            // The slot's +1 has gone to whoever read it; clearing it is what
            // stops the enum releasing the value a second time. The drop
            // function generated above skips a null slot.
            writeln!(o, "    ((T{tid} *){obj})->u.v{tag}.p{idx} = NULL;").unwrap();
        }
        Inst::LoadField { dst, obj, tid, idx } => {
            let name = c_ident(&types[*tid as usize].fields[*idx as usize].0);
            writeln!(o, "    {dst} = ((T{tid} *){obj})->f_{name};").unwrap();
        }
        Inst::StoreField { obj, tid, idx, val } => {
            let name = c_ident(&types[*tid as usize].fields[*idx as usize].0);
            writeln!(o, "    ((T{tid} *){obj})->f_{name} = {val};").unwrap();
        }
        Inst::CallIface {
            dst,
            slot,
            name,
            args,
            ret,
        } => {
            // The slot is cast back to its real signature here; only the
            // indirection is dynamic, and the compiler checked the signature
            // structurally before emitting this.
            let rty = match ret {
                Some(t) => t.c_name(),
                None => "void".to_string(),
            };
            let ptypes: Vec<String> = args.iter().map(|a| f.ty_of(*a).c_name()).collect();
            let a: Vec<String> = args.iter().map(|v| v.to_string()).collect();
            let recv = &a[0];
            let cast = format!("({rty} (*)({}))", ptypes.join(", "));
            let call = format!("({cast}{recv}->ty->vtable[{slot}])({})", a.join(", "));
            match dst {
                Some(d) => writeln!(o, "    {d} = {call}; /* .{name} */").unwrap(),
                None => writeln!(o, "    {call}; /* .{name} */").unwrap(),
            }
        }
        Inst::Spawn { func, args } => {
            let a: Vec<String> = args.iter().map(|v| v.to_string()).collect();
            writeln!(o, "    {{").unwrap();
            writeln!(
                o,
                "        spawn_{0} *sa = malloc(sizeof(spawn_{0}));",
                c_name(func)
            )
            .unwrap();
            writeln!(o, "        if (sa == NULL) rt_trap(\"out of memory\");").unwrap();
            for (i, x) in a.iter().enumerate() {
                writeln!(o, "        sa->a{i} = {x};").unwrap();
            }
            writeln!(o, "        rt_spawn(trampoline_{}, sa);", c_name(func)).unwrap();
            writeln!(o, "    }}").unwrap();
        }
        Inst::RcInc { val } => writeln!(o, "    rc_inc({val});").unwrap(),
        Inst::RcDec { val } => writeln!(o, "    rc_dec({val});").unwrap(),
    }
    let _ = f;
}

/// Emit a jump, moving block arguments into the target's parameters.
///
/// Every argument goes through a temporary first. That is the general fix for
/// the parallel-copy problem -- `jump b(p1, p0)` would corrupt both values if
/// assigned in sequence -- and it costs nothing, because the C compiler
/// removes the redundant copies. docs/ir-v0.md §7.2 flags this as a trap worth
/// not being clever about.
pub(super) fn emit_jump(
    o: &mut String,
    f: &crate::middle::ir::Func,
    to: crate::middle::ir::BlockId,
    args: &[Value],
) {
    let target = f.block(to);
    debug_assert_eq!(
        target.params.len(),
        args.len(),
        "block argument arity mismatch"
    );
    if args.is_empty() {
        writeln!(o, "    goto b{};", to.0).unwrap();
        return;
    }
    writeln!(o, "    {{").unwrap();
    for (n, a) in args.iter().enumerate() {
        let t = f.ty_of(*a);
        let name = t.c_name();
        let sep = if name.ends_with('*') { "" } else { " " };
        writeln!(o, "        {name}{sep}tmp{n} = {a};").unwrap();
    }
    for (n, p) in target.params.iter().enumerate() {
        writeln!(o, "        {p} = tmp{n};").unwrap();
    }
    writeln!(o, "        goto b{};", to.0).unwrap();
    writeln!(o, "    }}").unwrap();
}

impl ArithOp {
    #[allow(dead_code)]
    fn is_checked(self) -> bool {
        true
    }
}
