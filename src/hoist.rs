//! Post-lowering IR pass: hoist an `Array`/`List`/`bytes`' data pointer and
//! length out of a loop that does nothing opaque to it -- docs/perf-board.md
//! item 1, "hoist the metadata".
//!
//! Once `a[i]` is emitted inline (runtime/rt.h, `rt_index_get`/
//! `rt_index_set` and the `bytes` pair), item 1 expected gcc/clang to hoist
//! the rest themselves: "a simple loop contains no opaque call and the
//! compiler can see that for itself." Measured against that claim, on
//! `apps/git/sha1.src`'s message-schedule loop (the one FRICTION.md §5
//! names) at `cc -O2` and `-O3`, gcc and clang: they do not, even with the
//! bounds check hoisted out of a runtime call and `restrict` on the element
//! pointer. A from-scratch reduced C reproduction (no runtime, no frozen
//! check, just a header with `len` and an inline `data[]`, indexed in a
//! read-then-write loop) confirms why: `len` and an element are both
//! `int64_t`, so nothing rules out a STORE through the element pointer being
//! the write that changes `len`, and the load is repeated every iteration --
//! confirmed present even after `-O3`'s loop unswitching separates the
//! `Array`/`List` cases into their own copies of the loop. Hand-hoisting the
//! pointer and length into real C locals ahead of the loop (bypassing
//! `rt_index_get`/`rt_index_set` entirely for that object) measured ~13%
//! faster, best of 9, than the inlined-but-not-hoisted version -- smaller
//! than item 1's inlining half, but real, so this pass exists to do it
//! automatically wherever it is sound.
//!
//! Soundness rests entirely on this IR being SSA: a `Value` is written by
//! exactly one instruction (or is a block parameter) for its whole life, so
//! "the object is the same on every iteration" reduces to "the `Value`
//! naming it is not itself (re)defined inside the loop" -- checkable
//! exactly, without alias analysis, because the IR has already thrown away
//! everything an aliasing name could mean.
//!
//! What makes a loop eligible:
//!
//!   - a single back edge (one `continue`-shaped latch; a loop with more
//!     than one is left alone rather than reasoned about further, which
//!     costs a missed loop now and then and risks nothing);
//!   - nothing in it whose effect on the object is unknown: an interface
//!     call (dynamic dispatch can run anything), a spawn, a refcount
//!     decrement (can run an arbitrary destructor), or a call to any
//!     runtime function other than the handful this pass already knows only
//!     ever touch the one object named in their first argument. Any of
//!     these disqualifies the WHOLE loop, conservatively -- reasoning about
//!     a call this pass cannot see into is `const`'s job (item 2), not this
//!     pass's, and out of scope here by design (docs/perf-board.md item 1).
//!
//! When eligible, every `Array`/`List` and every `bytes` accessed by index
//! with an object defined outside the loop gets its data pointer and length
//! read once, in a new block spliced in before the loop, and every access
//! inside the loop is rewritten to use those two values directly.

use crate::ir::{Block, BlockId, Func, Inst, IrTy, Module, Term, Value};
use std::collections::{HashMap, HashSet};

/// Runtime functions this pass already knows are safe inside a loop it is
/// considering hoisting something out of: each touches only the ONE object
/// named in its first argument, and none of them can call back into the
/// program. Includes the `_at`/`_data_addr` helpers this pass itself
/// introduces, so a loop nested inside another one this pass already
/// rewrote still sees a safe loop rather than an unrecognised call.
const SAFE_CALLS: &[&str] = &[
    "rt_index_get",
    "rt_index_set",
    "rt_bytes_get",
    "rt_bytes_set",
    "rt_len_of",
    "rt_bytes_len",
    "rt_seq_data_addr",
    "rt_bytes_data_addr",
    "rt_index_get_at",
    "rt_index_set_at",
    "rt_bytes_get_at",
    "rt_bytes_set_at",
];

pub fn hoist_seq_metadata(m: &mut Module) {
    for f in &mut m.funcs {
        hoist_in_func(f);
    }
}

fn hoist_in_func(f: &mut Func) {
    // Applying one loop's hoist only ever APPENDS a block and edits existing
    // `Call` instructions in place -- it never removes or renumbers a block
    // -- so every OTHER loop's back edge, discovered from the structure
    // before this one was touched, is still exactly where it was. But an
    // inner loop's freshly-inserted preheader can become part of an outer
    // loop's body, which changes what the outer loop's safety scan sees, so
    // recompute from scratch and restart after every successful hoist,
    // rather than trying to patch the analysis up in place.
    loop {
        let preds = predecessors(f);
        let defs = def_blocks(f);
        let mut headers: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
        for b in &f.blocks {
            for t in successors(&b.term) {
                if t.0 <= b.id.0 {
                    headers.entry(t).or_default().push(b.id);
                }
            }
        }
        let mut order: Vec<BlockId> = headers.keys().copied().collect();
        order.sort_by_key(|h| h.0);

        let mut applied = false;
        for h in order {
            let latches = &headers[&h];
            // More than one back edge into the same header: a loop with an
            // extra `continue`-shaped latch, most likely. Left alone -- see
            // the file comment.
            if latches.len() != 1 {
                continue;
            }
            let set = natural_loop(&preds, h, latches[0]);
            if try_hoist_loop(f, h, &set, &preds, &defs) {
                applied = true;
                break;
            }
        }
        if !applied {
            break;
        }
    }
}

fn successors(t: &Term) -> Vec<BlockId> {
    match t {
        Term::Jump { to, .. } => vec![*to],
        Term::Brif { then, els, .. } => vec![*then, *els],
        Term::Ret { .. } => vec![],
    }
}

fn predecessors(f: &Func) -> HashMap<BlockId, Vec<BlockId>> {
    let mut m: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for b in &f.blocks {
        for t in successors(&b.term) {
            m.entry(t).or_default().push(b.id);
        }
    }
    m
}

/// Every block that can reach `latch` without going through `header` --
/// the standard natural-loop definition for a single back edge
/// `latch -> header` -- plus `header` and `latch` themselves.
fn natural_loop(
    preds: &HashMap<BlockId, Vec<BlockId>>,
    header: BlockId,
    latch: BlockId,
) -> HashSet<BlockId> {
    let mut set = HashSet::new();
    set.insert(header);
    set.insert(latch);
    let mut stack = vec![latch];
    while let Some(n) = stack.pop() {
        if n == header {
            continue;
        }
        if let Some(ps) = preds.get(&n) {
            for &p in ps {
                if set.insert(p) {
                    stack.push(p);
                }
            }
        }
    }
    set
}

/// The block that defines every `Value` in `f`: an instruction's result, or
/// a block parameter. A function parameter lives on the entry block's
/// parameter list, so it falls out of the same map with no special case.
fn def_blocks(f: &Func) -> HashMap<Value, BlockId> {
    let mut m = HashMap::new();
    for b in &f.blocks {
        for &p in &b.params {
            m.insert(p, b.id);
        }
        for i in &b.insts {
            for d in inst_dsts(i) {
                m.insert(d, b.id);
            }
        }
    }
    m
}

fn inst_dsts(i: &Inst) -> Vec<Value> {
    use Inst::*;
    match i {
        IConst { dst, .. }
        | FConst { dst, .. }
        | BConst { dst, .. }
        | SConst { dst, .. }
        | KConst { dst, .. }
        | Arith { dst, .. }
        | ICmp { dst, .. }
        | Not { dst, .. }
        | Alloc { dst, .. }
        | EnumPack { dst, .. }
        | EnumTag { dst, .. }
        | EnumPayload { dst, .. }
        | EnumClone { dst, .. }
        | LoadField { dst, .. } => vec![*dst],
        Call { dst, .. } | CallIface { dst, .. } => dst.iter().copied().collect(),
        ParseInto { ok, dst, .. } => vec![*ok, *dst],
        EnumTake { .. } | StoreField { .. } | Spawn { .. } | RcInc { .. } | RcDec { .. } => {
            vec![]
        }
    }
}

/// Whether this instruction's effect on some OTHER object might not be
/// nothing -- specifically, whether it could reallocate or replace an
/// `Array`/`List`/`bytes`' storage in a way this pass has not accounted for.
/// `true` disqualifies the whole loop it is found in.
fn is_opaque(i: &Inst) -> bool {
    match i {
        Inst::CallIface { .. }
        | Inst::Spawn { .. }
        | Inst::RcDec { .. }
        | Inst::ParseInto { .. } => true,
        Inst::Call { func, .. } => !SAFE_CALLS.contains(&func.as_str()),
        _ => false,
    }
}

fn try_hoist_loop(
    f: &mut Func,
    header: BlockId,
    set: &HashSet<BlockId>,
    preds: &HashMap<BlockId, Vec<BlockId>>,
    defs: &HashMap<Value, BlockId>,
) -> bool {
    for b in &f.blocks {
        if set.contains(&b.id) && b.insts.iter().any(is_opaque) {
            return false;
        }
    }

    // Loop-invariant: this IR is SSA, so a `Value` is written exactly once,
    // ever. If that one write is outside the loop (or it is a function
    // parameter, which lives on the entry block and is never "inside" a
    // loop unless the whole function is one), the value cannot change
    // between iterations, full stop -- no aliasing question to ask.
    let invariant = |o: Value| defs.get(&o).is_none_or(|bl| !set.contains(bl));

    let mut seq: Vec<Value> = Vec::new();
    let mut bytes: Vec<Value> = Vec::new();
    for b in &f.blocks {
        if !set.contains(&b.id) {
            continue;
        }
        for i in &b.insts {
            let Inst::Call { func, args, .. } = i else {
                continue;
            };
            match func.as_str() {
                "rt_index_get" | "rt_index_set"
                    if invariant(args[0]) && !seq.contains(&args[0]) =>
                {
                    seq.push(args[0]);
                }
                "rt_bytes_get" | "rt_bytes_set"
                    if invariant(args[0]) && !bytes.contains(&args[0]) =>
                {
                    bytes.push(args[0]);
                }
                _ => {}
            }
        }
    }
    if seq.is_empty() && bytes.is_empty() {
        return false;
    }

    // The header's parameter TYPES, read before anything below starts
    // mutating `f.types` -- `f.block(header)` and `f.types.push` cannot be
    // live at once.
    let header_param_types: Vec<IrTy> = f.blocks[header.0 as usize]
        .params
        .iter()
        .map(|p| f.ty_of(*p))
        .collect();
    let ph_params: Vec<Value> = header_param_types
        .into_iter()
        .map(|t| {
            let v = Value(f.types.len() as u32);
            f.types.push(t);
            v
        })
        .collect();

    let mut ph_insts = Vec::new();
    let mut seq_map: HashMap<Value, (Value, Value)> = HashMap::new();
    for &o in &seq {
        let ptr = Value(f.types.len() as u32);
        f.types.push(IrTy::I64);
        ph_insts.push(Inst::Call {
            dst: Some(ptr),
            func: "rt_seq_data_addr".to_string(),
            args: vec![o],
        });
        let len = Value(f.types.len() as u32);
        f.types.push(IrTy::I64);
        ph_insts.push(Inst::Call {
            dst: Some(len),
            func: "rt_len_of".to_string(),
            args: vec![o],
        });
        seq_map.insert(o, (ptr, len));
    }
    let mut bytes_map: HashMap<Value, (Value, Value)> = HashMap::new();
    for &o in &bytes {
        let ptr = Value(f.types.len() as u32);
        f.types.push(IrTy::I64);
        ph_insts.push(Inst::Call {
            dst: Some(ptr),
            func: "rt_bytes_data_addr".to_string(),
            args: vec![o],
        });
        let len = Value(f.types.len() as u32);
        f.types.push(IrTy::I64);
        ph_insts.push(Inst::Call {
            dst: Some(len),
            func: "rt_bytes_len".to_string(),
            args: vec![o],
        });
        bytes_map.insert(o, (ptr, len));
    }

    let ph_id = BlockId(f.blocks.len() as u32);
    f.blocks.push(Block {
        id: ph_id,
        params: ph_params.clone(),
        insts: ph_insts,
        term: Term::Jump {
            to: header,
            args: ph_params,
        },
    });

    // Redirect every entry into the loop from OUTSIDE it through the new
    // preheader instead; the latch keeps jumping straight to the header.
    if let Some(ext) = preds.get(&header) {
        for &p in ext {
            if !set.contains(&p) {
                redirect(&mut f.blocks[p.0 as usize].term, header, ph_id);
            }
        }
    }

    // Rewrite every access inside the loop to use the hoisted pointer and
    // length. A store's frozen check still names the object -- only the
    // bounds check and the dereference move.
    for b in &mut f.blocks {
        if !set.contains(&b.id) {
            continue;
        }
        for i in &mut b.insts {
            let Inst::Call { func, args, .. } = i else {
                continue;
            };
            match func.as_str() {
                "rt_index_get" => {
                    if let Some(&(ptr, len)) = seq_map.get(&args[0]) {
                        let idx = args[1];
                        *func = "rt_index_get_at".to_string();
                        *args = vec![ptr, len, idx];
                    }
                }
                "rt_index_set" => {
                    if let Some(&(ptr, len)) = seq_map.get(&args[0]) {
                        let (o, idx, v) = (args[0], args[1], args[2]);
                        *func = "rt_index_set_at".to_string();
                        *args = vec![o, ptr, len, idx, v];
                    }
                }
                "rt_bytes_get" => {
                    if let Some(&(ptr, len)) = bytes_map.get(&args[0]) {
                        let idx = args[1];
                        *func = "rt_bytes_get_at".to_string();
                        *args = vec![ptr, len, idx];
                    }
                }
                "rt_bytes_set" => {
                    if let Some(&(ptr, len)) = bytes_map.get(&args[0]) {
                        let (o, idx, v) = (args[0], args[1], args[2]);
                        *func = "rt_bytes_set_at".to_string();
                        *args = vec![o, ptr, len, idx, v];
                    }
                }
                _ => {}
            }
        }
    }

    true
}

fn redirect(t: &mut Term, from: BlockId, to: BlockId) {
    match t {
        Term::Jump { to: tgt, .. } if *tgt == from => *tgt = to,
        Term::Brif { then, els, .. } => {
            if *then == from {
                *then = to;
            }
            if *els == from {
                *els = to;
            }
        }
        _ => {}
    }
}
