//! `prim "c"`: the checked call of an assembly or C-ABI function.
//!
//! This file is the whole lowering of docs/ffi-decision.md, and the only place
//! a pointer into an m31 object is made. For each call the emitted C
//!
//!   1. evaluates every span bound with overflow-checked `+` and `*`,
//!   2. checks each `int ... in A | B` against its literals,
//!   3. checks each span against the object's SIZE (never its capacity):
//!      `0 <= lo <= hi <= size`,
//!   4. refuses a frozen object as an `out`/`inout` span (`rt_check_mutable`),
//!   5. calls the callee with the derived pointers and the scalars,
//!
//! and every failure is an `rt_trap` naming the callee and the parameter. A
//! target without kernels (`RT_HAVE_KERNELS` is 0) calls nothing: an `int`
//! callee answers -1, a `void` one traps.

use std::fmt::Write;

use super::*;
use crate::middle::ir::{NativeExpr, NativeParam, NativeSig};

/// `extern` declarations for every callee, so a symbol nobody defined is a
/// link error that names it.
pub(super) fn emit_native_decls(o: &mut String, m: &Module) {
    if m.natives.is_empty() {
        return;
    }
    writeln!(o, "#if RT_HAVE_KERNELS").unwrap();
    for (sym, n) in &m.natives {
        let ret = if n.returns_int { "int64_t" } else { "void" };
        let params: Vec<&str> = n
            .params
            .iter()
            .filter_map(|p| match p {
                NativeParam::Scalar { .. } => Some("int64_t"),
                NativeParam::Range { .. } => None,
                NativeParam::Span {
                    ints: false,
                    writes: false,
                    ..
                } => Some("const uint8_t *"),
                NativeParam::Span {
                    ints: false,
                    writes: true,
                    ..
                } => Some("uint8_t *"),
                NativeParam::Span {
                    ints: true,
                    writes: false,
                    ..
                } => Some("const int64_t *"),
                NativeParam::Span {
                    ints: true,
                    writes: true,
                    ..
                } => Some("int64_t *"),
            })
            .collect();
        let plist = if params.is_empty() {
            "void".to_string()
        } else {
            params.join(", ")
        };
        writeln!(o, "extern {ret} {sym}({plist});").unwrap();
    }
    writeln!(o, "#endif\n").unwrap();
}

struct Gen<'a> {
    o: &'a mut String,
    sig: &'a NativeSig,
    args: Vec<String>,
    temps: usize,
}

impl Gen<'_> {
    fn trap(&self, what: &str) -> String {
        format!("rt_trap(\"prim {}: {what}\")", self.sig.name)
    }

    /// Emit the evaluation of a bound; returns a C expression for it.
    fn expr(&mut self, e: &NativeExpr, what: &str) -> String {
        match e {
            NativeExpr::Lit(n) if *n == i64::MIN => "INT64_MIN".to_string(),
            NativeExpr::Lit(n) => format!("INT64_C({n})"),
            NativeExpr::Param(i) => self.args[*i].clone(),
            NativeExpr::Add(a, b) | NativeExpr::Mul(a, b) => {
                let (builtin, op) = if matches!(e, NativeExpr::Add(..)) {
                    ("__builtin_add_overflow", "+")
                } else {
                    ("__builtin_mul_overflow", "*")
                };
                let x = self.expr(a, what);
                let y = self.expr(b, what);
                let t = format!("t{}", self.temps);
                self.temps += 1;
                let trap = self.trap(&format!("{what}: a bound overflows (`{op}`)"));
                writeln!(
                    self.o,
                    "        int64_t {t}; if ({builtin}({x}, {y}, &{t})) {trap};"
                )
                .unwrap();
                t
            }
        }
    }
}

/// The checked call. `callee` is the C name of the callee, `dst` the result
/// variable if the IR keeps one, `blocking` whether the call is wrapped in
/// the blocking handoff (it is unless the declaration said `nonblocking`).
pub(super) fn emit_native_call(
    o: &mut String,
    sig: &NativeSig,
    callee: &str,
    dst: Option<&Value>,
    args: &[Value],
    blocking: bool,
) {
    writeln!(o, "    {{ /* prim \"c\" {} */", sig.name).unwrap();
    writeln!(o, "#if RT_HAVE_KERNELS").unwrap();
    let mut g = Gen {
        o,
        sig,
        args: args.iter().map(|v| v.to_string()).collect(),
        temps: 0,
    };
    // 1 and 2: scalars first, so a bad `rounds` is reported as that and not
    // as the span it would have made too small.
    for (i, p) in sig.params.iter().enumerate() {
        if let NativeParam::Scalar { name, allowed } = p {
            if !allowed.is_empty() {
                let v = g.args[i].clone();
                let cond: Vec<String> = allowed
                    .iter()
                    .map(|n| format!("{v} != INT64_C({n})"))
                    .collect();
                let trap = g.trap(&format!(
                    "`{name}` is not one of the values the declaration allows"
                ));
                writeln!(g.o, "        if ({}) {trap};", cond.join(" && ")).unwrap();
            }
        }
    }
    // 1 and 3 and 4, span by span; the pointers are named p<i>.
    let mut ptrs: Vec<Option<String>> = vec![None; sig.params.len()];
    for (i, p) in sig.params.iter().enumerate() {
        let NativeParam::Span {
            name,
            ints,
            writes,
            lo,
            hi,
        } = p
        else {
            continue;
        };
        let lo_c = g.expr(lo, name);
        let hi_c = g.expr(hi, name);
        let obj = g.args[i].clone();
        let (len, data, cty) = if *ints {
            (
                format!("((Arr *){obj})->len"),
                format!("((Arr *){obj})->data"),
                "int64_t",
            )
        } else {
            (
                format!("((Bytes *){obj})->len"),
                format!("((Bytes *){obj})->data"),
                "uint8_t",
            )
        };
        let trap = g.trap(&format!("span `{name}` is outside its buffer"));
        // `0 <= lo <= hi <= size`; a literal-zero `lo` needs no test of its own.
        let cond = if matches!(lo, NativeExpr::Lit(0)) {
            format!("{hi_c} < 0 || {hi_c} > {len}")
        } else {
            format!("{lo_c} < 0 || {hi_c} < {lo_c} || {hi_c} > {len}")
        };
        writeln!(g.o, "        if ({cond}) {trap};").unwrap();
        if *writes {
            writeln!(g.o, "        rt_check_mutable({obj});").unwrap();
        }
        let q = if *writes { "" } else { "const " };
        writeln!(g.o, "        {q}{cty} *p{i} = {data} + {lo_c};").unwrap();
        ptrs[i] = Some(format!("p{i}"));
    }
    // 5.
    let call_args: Vec<String> = sig
        .params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match p {
            NativeParam::Scalar { .. } => Some(g.args[i].clone()),
            NativeParam::Range { .. } => None,
            NativeParam::Span { .. } => ptrs[i].clone(),
        })
        .collect();
    let call = format!("{callee}({})", call_args.join(", "));
    if blocking {
        writeln!(g.o, "        rt_enter_blocking();").unwrap();
    }
    match (dst, sig.returns_int) {
        (Some(d), true) => writeln!(g.o, "        {d} = {call};").unwrap(),
        _ => writeln!(g.o, "        {call};").unwrap(),
    }
    if blocking {
        writeln!(g.o, "        rt_exit_blocking();").unwrap();
    }
    writeln!(g.o, "#else").unwrap();
    if sig.returns_int {
        if let Some(d) = dst {
            writeln!(g.o, "        {d} = -1;").unwrap();
        }
    } else {
        let trap = g.trap("no native kernels on this target; the library must ask first");
        writeln!(g.o, "        {trap};").unwrap();
    }
    writeln!(g.o, "#endif").unwrap();
    writeln!(g.o, "    }}").unwrap();
}
