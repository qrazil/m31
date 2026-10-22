//! Whole functions: prototypes, spawn trampolines, bodies and terminators.

use super::inst::{emit_inst, emit_jump};
use super::*;

pub(super) fn check_unique_c_names(m: &Module) {
    // Escaping `$` could collide two distinct IR names, and a collision here
    // is a silent miscompile. `lower` rejects it with a proper diagnostic
    // before we get this far -- see `check_c_name_collisions` -- so reaching
    // this assertion means that check has a hole, not that the user did
    // something wrong.
    {
        let mut seen = std::collections::BTreeSet::new();
        for f in &m.funcs {
            assert!(
                seen.insert(c_name(&f.name)),
                "two functions map to the same C name: {} \
                 (lower::check_c_name_collisions should have caught this)",
                c_name(&f.name)
            );
        }
    }
}

pub(super) fn emit_prototypes(o: &mut String, m: &Module) {
    // Prototypes first, so definition order does not constrain call order.
    for f in &m.funcs {
        writeln!(o, "{};", signature(f)).unwrap();
    }
    o.push('\n');
}

pub(super) fn emit_spawn_trampolines(o: &mut String, m: &Module) {
    // One argument struct and one trampoline per spawned function. pthreads
    // takes a single void*, so the arguments are packed on the heap and the
    // trampoline unpacks them.
    let mut spawned: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for f in &m.funcs {
        for b in &f.blocks {
            for i in &b.insts {
                if let Inst::Spawn { func, .. } = i {
                    spawned.insert(func.as_str());
                }
            }
        }
    }
    for name in &spawned {
        let target = m
            .funcs
            .iter()
            .find(|f| f.name == **name)
            .expect("spawned function must exist");
        let cn = c_name(name);
        writeln!(o, "typedef struct {{").unwrap();
        for (i, p) in target.params.iter().enumerate() {
            let t = target.ty_of(*p).c_name();
            let sep = if t.ends_with('*') { "" } else { " " };
            writeln!(o, "    {t}{sep}a{i};").unwrap();
        }
        if target.params.is_empty() {
            writeln!(o, "    int unused;").unwrap();
        }
        writeln!(o, "}} spawn_{cn};").unwrap();
        // `void`, not `void *`: rt_spawn (runtime/rt.h) now takes
        // `rt_sched_spawn`'s own entry signature (runtime/scheduler.h),
        // `void (*)(void *)`, because every spawn is a green thread on the
        // process-wide scheduler, never a pthread -- there is no return
        // value for a pthread join to collect any more.
        writeln!(o, "static void trampoline_{cn}(void *p) {{").unwrap();
        writeln!(o, "    spawn_{cn} *a = (spawn_{cn} *)p;").unwrap();
        let passed: Vec<String> = (0..target.params.len())
            .map(|i| format!("a->a{i}"))
            .collect();
        writeln!(o, "    {cn}({});", passed.join(", ")).unwrap();
        // The spawning side handed this thread a reference per argument --
        // moved if it owned one, retained if it only borrowed. Parameters are
        // borrowed inside the callee (docs/ir-v0.md §5.1), so nobody there
        // will release them: the trampoline is where the transfer ends.
        for (i, p) in target.params.iter().enumerate() {
            if target.ty_of(*p) == IrTy::Ref {
                writeln!(o, "    rc_dec(a->a{i});").unwrap();
            }
        }
        writeln!(o, "    free(a);").unwrap();
        writeln!(o, "}}").unwrap();
    }
    if !spawned.is_empty() {
        o.push('\n');
    }
}

pub(super) fn emit_functions(o: &mut String, m: &Module) {
    for f in &m.funcs {
        emit_func(
            o,
            f,
            &m.types,
            &Ffi {
                prim_targets: &m.prim_targets,
                natives: &m.natives,
            },
        );
        o.push('\n');
    }
}

pub(super) fn emit_func(o: &mut String, f: &crate::middle::ir::Func, types: &[TypeDef], ffi: &Ffi) {
    // Blocks are emitted in order and control falls into the first one, so
    // the entry block must be first or the function starts in the wrong place.
    assert_eq!(
        f.blocks.first().map(|b| b.id),
        Some(f.entry),
        "entry block must be emitted first"
    );
    writeln!(o, "{} {{", signature(f)).unwrap();

    // The compiler-emitted stack probe (docs/concurrency-decision.md,
    // "Stacks: fixed, but not limited"; runtime/rt.h for the full contract,
    // including a real, reproduced bug: this MUST be a real out-of-line
    // call, never the probe's own comparison text inlined directly here.
    // Inlining it lets a cross-carrier park/resume inside THIS function
    // leave a later, separately-inlined copy (e.g. from a small callee like
    // `Conn.close` that `-O2` inlines back in) reading a stale, wrong-
    // carrier's cached TLS address -- see rt_stack_limit's own comment in
    // rt.h for the full account. `rt_stack_check` (runtime/rt.h/.c) is
    // `noinline` specifically so every call site gets its own fresh read.
    //
    // Always correct, always emitted -- Phase 1 deliberately skips the
    // "primitives/leaf functions don't need it" optimisation the design
    // allows, to get the simple version right first. rt_stack_limit is 0 on
    // any thread that has never run a green thread (runtime/rt.c), and an
    // address is never 0, so this is a dead compare-and-branch for every
    // program until something starts switching into green threads.
    writeln!(o, "    rt_stack_check();").unwrap();

    // A block parameter is allowed to be dead: a merge point carries every
    // variable the arms disagree about, and nothing is obliged to read them
    // afterwards. A loop that mutates `i` but only uses `found` after the
    // loop produces exactly this. Mark those declarations so -Wunused-but-set
    // stays meaningful for ordinary instruction results, where a dead value
    // really would indicate a lowering bug.
    let block_params: std::collections::BTreeSet<Value> = f
        .blocks
        .iter()
        .flat_map(|b| b.params.iter().copied())
        .collect();

    // A value nothing reads is legitimate in emitted code: a loop whose body
    // always returns still computes the index increment its back edge would
    // have used, and that edge does not exist. gcc warns, run.sh treats a
    // warning as failure, so such a declaration says it may be unused. A dead
    // value is not a defect the way a dead *label* was.
    let mut read: std::collections::BTreeSet<Value> = std::collections::BTreeSet::new();
    for b in &f.blocks {
        for i in &b.insts {
            match i {
                Inst::Arith { lhs, rhs, .. } => {
                    read.insert(*lhs);
                    read.insert(*rhs);
                }
                // A comparison of a value with itself is emitted as the
                // constant it must be, so it reads neither operand. This has
                // to agree with the emission below or the operand is declared
                // used and then never used, which is its own warning.
                Inst::ICmp { lhs, rhs, .. } => {
                    if lhs != rhs || f.ty_of(*lhs) == IrTy::F64 {
                        read.insert(*lhs);
                        read.insert(*rhs);
                    }
                }
                Inst::Not { src, .. } => {
                    read.insert(*src);
                }
                Inst::Call { args, .. }
                | Inst::CallIface { args, .. }
                | Inst::Spawn { args, .. } => read.extend(args.iter().copied()),
                Inst::LoadField { obj, .. } => {
                    read.insert(*obj);
                }
                Inst::FConst { .. } => {}
                Inst::ParseInto { src, .. } => {
                    read.insert(*src);
                }
                Inst::EnumPack { args, .. } => read.extend(args.iter().copied()),
                Inst::EnumTag { obj, .. }
                | Inst::EnumPayload { obj, .. }
                | Inst::EnumTake { obj, .. } => {
                    read.insert(*obj);
                }
                Inst::EnumClone { src, .. } => {
                    read.insert(*src);
                }
                Inst::StoreField { obj, val, .. } => {
                    read.insert(*obj);
                    read.insert(*val);
                }
                Inst::RcInc { val } | Inst::RcDec { val } => {
                    read.insert(*val);
                }
                Inst::IConst { .. }
                | Inst::BConst { .. }
                | Inst::SConst { .. }
                | Inst::KConst { .. }
                | Inst::Alloc { .. } => {}
            }
        }
        match &b.term {
            Term::Jump { args, .. } => read.extend(args.iter().copied()),
            Term::Brif {
                cond,
                then_args,
                els_args,
                ..
            } => {
                read.insert(*cond);
                read.extend(then_args.iter().copied());
                read.extend(els_args.iter().copied());
            }
            Term::Ret { val: Some(v) } => {
                read.insert(*v);
            }
            Term::Ret { val: None } => {}
        }
    }

    // Declare every value up front. Function parameters are already declared
    // by the signature, so skip those.
    let is_param = |v: Value| f.params.contains(&v);
    for (i, t) in f.types.iter().enumerate() {
        let v = Value(i as u32);
        if is_param(v) {
            continue;
        }
        let name = t.c_name();
        let sep = if name.ends_with('*') { "" } else { " " };
        // Initialised to silence -Wmaybe-uninitialized: a value defined in one
        // arm of a branch is only read on paths that define it, but the C
        // compiler cannot always see that, and run.sh treats warnings as
        // failures.
        let zero = match t {
            IrTy::I64 => "0",
            IrTy::F64 => "0.0",
            IrTy::I1 => "false",
            IrTy::Ref => "NULL",
            // A struct, so a brace initialiser. `{0}` is the one form both
            // gcc and clang accept without -Wmissing-field-initializers,
            // which -Wextra turns on and run.sh reads as a failure.
            IrTy::Val(_) => "{0}",
        };
        let attr = if block_params.contains(&v) || !read.contains(&v) {
            "__attribute__((unused)) "
        } else {
            ""
        };
        writeln!(o, "    {attr}{name}{sep}{v} = {zero};").unwrap();
    }
    if f.types.len() > f.params.len() {
        o.push('\n');
    }

    // Only blocks that are actually jumped to get a label. C warns about
    // unused labels, run.sh treats warnings as failures, and the entry block
    // is fallen into rather than branched to.
    let mut targeted = std::collections::BTreeSet::new();
    for b in &f.blocks {
        match &b.term {
            Term::Jump { to, .. } => {
                targeted.insert(*to);
            }
            Term::Brif { then, els, .. } => {
                targeted.insert(*then);
                targeted.insert(*els);
            }
            Term::Ret { .. } => {}
        }
    }

    for b in &f.blocks {
        if targeted.contains(&b.id) {
            writeln!(o, "b{}:; /* {} */", b.id.0, b.id).unwrap();
        } else if b.id != f.entry {
            writeln!(o, "    /* {} (unreachable) */", b.id).unwrap();
        }
        for i in &b.insts {
            emit_inst(o, f, types, i, ffi);
        }
        emit_term(o, f, &b.term);
    }

    writeln!(o, "}}").unwrap();
}

pub(super) fn emit_term(o: &mut String, f: &crate::middle::ir::Func, t: &Term) {
    match t {
        Term::Jump { to, args } => emit_jump(o, f, *to, args),
        Term::Brif {
            cond,
            then,
            then_args,
            els,
            els_args,
        } => {
            writeln!(o, "    if ({cond}) {{").unwrap();
            emit_jump(o, f, *then, then_args);
            writeln!(o, "    }} else {{").unwrap();
            emit_jump(o, f, *els, els_args);
            writeln!(o, "    }}").unwrap();
        }
        Term::Ret { val: Some(v) } => writeln!(o, "    return {v};").unwrap(),
        // A valueless return in a function that returns something is the
        // lowering's filler for an UNREACHABLE block -- every arm of an `if`
        // or a `match` already returned, so the join has no predecessor. C
        // rejects a bare `return` there, so say what it means instead:
        // rt_trap is _Noreturn, which also satisfies C that the function
        // cannot fall off its end.
        Term::Ret { val: None } if f.ret.is_some() => {
            writeln!(o, "    rt_trap(\"unreachable\");").unwrap()
        }
        Term::Ret { val: None } => writeln!(o, "    return;").unwrap(),
    }
}
