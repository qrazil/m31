//! The advisory blocking-FFI check for spawned functions.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::middle::ir::{Inst, Module};

/// Phase 3 of docs/concurrency-decision.md, "Blocking FFI": "unannotated
/// foreign calls from a green thread's call path produce a compile-time
/// warning." Run once per successful compile (src/main.rs's `finish`),
/// after lowering, printing to stderr -- advisory only, never fails the
/// build, the same way a C compiler's own `-Wall` warnings do not.
///
/// This is a best-effort, HONESTLY APPROXIMATE static check, not a sound
/// one, and says so out loud rather than overclaiming precision:
///
///   - **"a green thread's call path"** is approximated here as "reachable
///     from a function passed to `spawn`" -- the only concurrency entry
///     point the compiled language has today. Green threads are not wired
///     to `spawn` yet (`spawn` still means an OS thread, unchanged -- see
///     runtime/scheduler.h's own header for why that boundary is
///     deliberately untouched by this phase), so this is a stand-in for
///     the real thing, not the real thing; once a future phase routes
///     `spawn` through the green-thread scheduler, this same check becomes
///     exact rather than approximate, with no change needed here.
///   - **"unannotated"** means a call that is not marked `nonblocking`.
///     A `prim "c"` may carry that marker (docs/ffi-decision.md): the
///     author's promise that the callee makes no system call and waits
///     for nothing, and the lowering takes such a callee out of
///     `prim_targets`, so it is neither wrapped nor warned about. Every
///     other `prim` -- the ordinary runtime ones, which have no way to
///     say it -- stays "unannotated" and is flagged.
///   - **Reachability only follows `Inst::Call` edges** (direct calls,
///     resolved after monomorphisation) -- it does NOT follow
///     `Inst::CallIface` (interface/virtual dispatch), whose concrete
///     target is not statically known at this point in the pipeline. A
///     `prim` call reached only through an interface method is therefore
///     MISSED by this check. Closing that gap soundly would need either a
///     closed-world call-graph analysis over every possible implementor of
///     the interface (a real, separate pass this project does not have) or
///     a maximally-conservative "any CallIface might reach anything"
///     fallback (which would make nearly every spawn warn, defeating the
///     point of a signal). Neither was judged worth building against this
///     phase's own timeline, so this check ships as a real but incomplete
///     signal rather than a silently-incomplete one pretending otherwise.
pub(super) fn warn_unannotated_foreign_calls_from_spawn(m: &Module) {
    let by_name: HashMap<&str, &crate::middle::ir::Func> =
        m.funcs.iter().map(|f| (f.name.as_str(), f)).collect();

    let mut roots: Vec<&str> = Vec::new();
    for f in &m.funcs {
        for b in &f.blocks {
            for i in &b.insts {
                if let Inst::Spawn { func, .. } = i {
                    roots.push(func.as_str());
                }
            }
        }
    }

    for root in roots {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        seen.insert(root);
        queue.push_back(root);
        // Dedupe per root: the same `prim` reachable through two different
        // call paths from the same spawn should warn once, not once per
        // path -- the useful information is "reachable at all", not a full
        // enumeration of every path that gets there.
        let mut warned: HashSet<&str> = HashSet::new();

        while let Some(name) = queue.pop_front() {
            if m.prim_targets.contains(name) && warned.insert(name) {
                eprintln!(
                    "warning: `{root}` is spawned and may reach the foreign \
                     call `{name}` -- every FFI call site is already wrapped \
                     in enter_blocking/exit_blocking automatically; only a \
                     `prim \"c\"` declared `nonblocking` is exempt, so every \
                     other foreign call reachable from a spawned function \
                     is flagged for awareness \
                     (docs/concurrency-decision.md, \"Blocking FFI\"; this \
                     check does not see through interface/virtual calls --\
                     see src/back/emit_c/blocking.rs's own comment on this function for \
                     exactly what it can and cannot detect)"
                );
                continue; // a prim target has no Func/body to expand into
            }
            if let Some(f) = by_name.get(name) {
                for b in &f.blocks {
                    for i in &b.insts {
                        if let Inst::Call { func, .. } = i {
                            if seen.insert(func.as_str()) {
                                queue.push_back(func.as_str());
                            }
                        }
                    }
                }
            }
        }
    }
}
