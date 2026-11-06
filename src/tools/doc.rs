//! `m31c -doc <file|dir>...` -- markdown for the public items of a module,
//! with their docstrings (docs/docstrings-decision.md).
//!
//! Parse-only, like `fmt` and `lint`: it works on a program that does not
//! typecheck and on one whose imports are not on disk. One section per
//! file; inside it the public constants, types and functions in source
//! order, each with its signature and its docstring. A docstring is printed
//! as written (after normalisation); it is not interpreted.
//!
//! NOT covered, on purpose: private items (nothing outside the module can
//! use them), module-level `let`s (they cannot be `pub`), the members of a
//! type that is not `pub`, a module-level docstring (a doc comment before
//! `import` is refused as detached), links between items, and any output
//! but markdown. A file that does not parse is reported and skipped.

use crate::front::ast::{bare, Func, Param, Program, TypeDecl};
use crate::tools::{cli, fmt, lint};

/// The markdown for one parsed module.
pub fn render(prog: &Program, module: &str) -> String {
    fmt::set_type_names(prog);
    // (line, text): sections come out in source order, not grouped by kind.
    let mut items: Vec<(u32, String)> = Vec::new();
    for c in prog.consts.iter().filter(|c| c.is_pub) {
        let sig = format!(
            "pub const {} {}",
            fmt::type_text(c.ty),
            fmt::bare_name(&c.name)
        );
        items.push((
            c.span.line,
            section("constant", &fmt::bare_name(&c.name), &sig, &c.doc, &[]),
        ));
    }
    for t in prog.types.iter().filter(|t| t.is_pub) {
        items.push((t.span.line, type_section(t)));
    }
    for f in prog.funcs.iter().filter(|f| f.is_pub) {
        let kind = if f.recv.is_some() {
            "method"
        } else {
            "function"
        };
        items.push((
            f.span.line,
            section(kind, &func_key(f), &func_sig(f), &f.doc, &[]),
        ));
    }
    items.sort_by_key(|(line, _)| *line);
    let mut out = format!("# Module `{module}`\n");
    if items.is_empty() {
        out.push_str("\nNo public items.\n");
    }
    for (_, text) in items {
        out.push('\n');
        out.push_str(&text);
    }
    out
}

fn section(kind: &str, name: &str, sig: &str, doc: &Option<String>, members: &[String]) -> String {
    let mut s = format!("## {kind} `{name}`\n\n```m31\n{sig}\n```\n");
    if let Some(d) = doc {
        s.push('\n');
        s.push_str(d);
        s.push('\n');
    }
    if !members.is_empty() {
        s.push('\n');
        for m in members {
            s.push_str(m);
            s.push('\n');
        }
    }
    s
}

fn type_section(t: &TypeDecl) -> String {
    let kw = if t.distinct_base.is_some() {
        "distinct"
    } else if t.is_interface {
        "interface"
    } else if t.is_enum {
        "enum"
    } else if t.is_const {
        "const type"
    } else {
        "type"
    };
    let tp = if t.tparams.is_empty() {
        String::new()
    } else {
        format!("<{}>", t.tparams.join(", "))
    };
    let name = fmt::bare_name(&t.name);
    let sig = match t.distinct_base {
        Some(b) => format!("pub distinct {} {name}", fmt::type_text(b)),
        None => format!("pub {kw} {name}{tp}"),
    };
    let mut members: Vec<String> = Vec::new();
    for v in &t.variants {
        let payload: Vec<String> = v.payload.iter().map(|p| fmt::type_text(*p)).collect();
        let head = if payload.is_empty() {
            v.name.clone()
        } else {
            format!("{}({})", v.name, payload.join(", "))
        };
        members.push(member("variant", &head, &v.doc));
    }
    for f in t.fields.iter().filter(|f| f.is_pub) {
        let head = if f.embedded {
            fmt::type_text(f.ty)
        } else {
            format!("{} {}", fmt::type_text(f.ty), f.name)
        };
        members.push(member("field", &head, &f.doc));
    }
    for m in &t.methods {
        let head = format!(
            "{} {}({})",
            fmt::type_text(m.ret),
            m.name,
            params(&m.params)
        );
        members.push(member("method", &head, &m.doc));
    }
    section(kw, &name, &sig, &t.doc, &members)
}

/// One list entry; a multi-line doc continues indented under it.
fn member(kind: &str, head: &str, doc: &Option<String>) -> String {
    let mut s = format!("- {kind} `{head}`");
    if let Some(d) = doc {
        let mut lines = d.lines();
        if let Some(first) = lines.next() {
            s.push_str(": ");
            s.push_str(first);
        }
        for l in lines {
            s.push('\n');
            if !l.is_empty() {
                s.push_str("  ");
                s.push_str(l);
            }
        }
    }
    s
}

fn params(ps: &[Param]) -> String {
    ps.iter()
        .map(|p| {
            let c = if p.is_const { "const " } else { "" };
            let d = if p.default.is_some() { " = ..." } else { "" };
            format!("{c}{} {}{d}", fmt::type_text(p.ty), p.name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `area` or `Rect.area`: what a reader calls it.
fn func_key(f: &Func) -> String {
    match &f.recv {
        Some(r) => format!("{}.{}", bare(r), f.name),
        None => bare(&f.name).to_string(),
    }
}

fn func_sig(f: &Func) -> String {
    let tp = if f.tparams.is_empty() {
        String::new()
    } else {
        format!("<{}>", f.tparams.join(", "))
    };
    let name = match &f.recv {
        Some(r) if !f.recv_tparams.is_empty() => {
            format!("{}<{}>.{}", bare(r), f.recv_tparams.join(", "), f.name)
        }
        _ => func_key(f),
    };
    let kw = if f.is_prim {
        "pub prim "
    } else if f.is_static {
        "pub static "
    } else {
        "pub "
    };
    format!(
        "{kw}{} {name}{tp}({})",
        fmt::type_text(f.ret),
        params(&f.params)
    )
}

static DOC_OPERANDS: [cli::Operand; 2] = [
    cli::Operand {
        name: "path",
        need: cli::Need::Required,
        help: "a source file, or a directory searched for .m31 files",
    },
    cli::Operand {
        name: "more",
        need: cli::Need::Tail,
        help: "",
    },
];

pub static SPEC: cli::Spec = cli::Spec {
    prog: "m31c -doc",
    summary: "print markdown for the public items of m31 source, with their docstrings",
    opts: &[],
    operands: &DOC_OPERANDS,
    commands: &[],
    version: None,
    completions: false,
    epilog: "One section per file, public items in source order: signature, then the\n\
             docstring (`///` or `/** */` directly above the item). Private items,\n\
             module-level lets and module-level docs are not covered.",
};

/// Exit 0 when every file parsed, 1 otherwise, 2 when a path cannot be read.
pub fn run(parsed: &cli::Parsed) -> std::process::ExitCode {
    use std::process::ExitCode;
    let mut files = Vec::new();
    for input in &parsed.operands {
        if let Err(e) = lint::collect(std::path::Path::new(input), &mut files) {
            eprintln!("cannot read {input}: {e}");
            return ExitCode::from(2);
        }
    }
    let mut failed = false;
    let mut first = true;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            eprintln!("cannot read {path}");
            failed = true;
            continue;
        };
        let module = crate::resolve::modules::module_name(path);
        match lint::parse(&src, &module) {
            Ok(prog) => {
                if !first {
                    println!();
                }
                first = false;
                print!("{}", render(&prog, &module));
            }
            Err(d) => {
                eprintln!("{}", d.render_with_source(path, &src));
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
