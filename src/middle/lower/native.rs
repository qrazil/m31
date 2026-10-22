//! `prim "c"`: handing the emitter what it needs (docs/ffi-decision.md).
//!
//! The call site itself needs nothing special: a `prim "c"` is a `prim` whose
//! arguments are borrowed and whose callee is named by the seam's one
//! transform, so `lower_call` emits an ordinary `Inst::Call`. What makes it a
//! checked native call is the entry in `Module::natives` this builds, which
//! `src/back/emit_c/native.rs` looks the callee up in.

use super::*;
use crate::middle::ir::{NativeExpr, NativeParam as IrParam, NativeSig};

impl Lowerer {
    /// One entry per `prim "c"`, keyed by the callee symbol.
    pub(super) fn native_sigs(&self) -> std::collections::BTreeMap<String, NativeSig> {
        fn expr(e: &SpanExpr) -> NativeExpr {
            match e {
                SpanExpr::Lit(n) => NativeExpr::Lit(*n),
                SpanExpr::Param(i) => NativeExpr::Param(*i),
                SpanExpr::Add(a, b) => NativeExpr::Add(Box::new(expr(a)), Box::new(expr(b))),
                SpanExpr::Mul(a, b) => NativeExpr::Mul(Box::new(expr(a)), Box::new(expr(b))),
            }
        }
        let mut out = std::collections::BTreeMap::new();
        for (key, sig) in &self.sigs {
            let Some(n) = &sig.native else { continue };
            let bare = crate::front::ast::bare(key);
            let sym = format!("rt_{}", bare.trim_start_matches('_'));
            let params = sig
                .params
                .iter()
                .zip(&n.params)
                .map(|(p, np)| match np {
                    NativeParam::Scalar { allowed } => IrParam::Scalar {
                        name: p.name.clone(),
                        allowed: allowed.clone(),
                    },
                    NativeParam::Range => IrParam::Range {
                        name: p.name.clone(),
                    },
                    NativeParam::Span { mode, lo, hi } => IrParam::Span {
                        name: p.name.clone(),
                        ints: p.ty != Ty::Bytes,
                        writes: mode.writes(),
                        lo: expr(lo),
                        hi: expr(hi),
                    },
                })
                .collect();
            out.insert(
                sym,
                NativeSig {
                    name: bare.trim_start_matches("__k_").to_string(),
                    nonblocking: n.nonblocking,
                    returns_int: sig.ret == Ty::Int,
                    params,
                },
            );
        }
        out
    }
}
