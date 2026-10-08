//! `m31c lint` -- naming report. docs/naming-decision.md is the rulebook.
//!
//! Parse-only, like `fmt`: it has to work on a program that does not
//! typecheck, and on a program whose imports are not on disk (an extracted
//! repo). Report mode only -- it prints findings and an exit code, it never
//! edits a file and nothing in the gates calls it yet.

use crate::ast::{bare, Args, Expr, Func, MatchArm, Param, Program, Stmt, Ty};
use crate::diag::Span;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Local,
    Param,
    Field,
    MatchBind,
    LoopVar,
    LambdaParam,
    Function,
    Type,
    Variant,
    Const,
    TypeParam,
    Module,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Local => "local",
            Kind::Param => "parameter",
            Kind::Field => "field",
            Kind::MatchBind => "match binding",
            Kind::LoopVar => "loop variable",
            Kind::LambdaParam => "lambda parameter",
            Kind::Function => "function",
            Kind::Type => "type",
            Kind::Variant => "enum variant",
            Kind::Const => "constant",
            Kind::TypeParam => "type parameter",
            Kind::Module => "module",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub line: u32,
    pub col: u32,
    pub kind: Kind,
    pub name: String,
    pub reason: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Default)]
pub struct Opts {
    /// Extra names accepted everywhere, on top of the built-in exceptions.
    pub allow: Vec<String>,
    /// Drop the built-in exceptions (`i`/`j` in a counting loop, `id`,
    /// numeric `x`/`y`): every name must clear the length rule.
    pub strict: bool,
}

/// Shortest acceptable name for a value: three characters.
const MIN_LEN: usize = 3;

/// Two-letter English words that read as words at a call site -- `a.eq(b)`,
/// `Point.of(1, 2)`, `grid.at(row, col)` -- and so may name a function.
const SHORT_WORDS: &[&str] = &["at", "by", "eq", "of", "ok", "up"];

/// Predeclared generic containers: `array` for an `Array<int>` says nothing
/// the type does not, so it is never offered as a rename.
const CONTAINERS: &[&str] = &["Array", "List", "Map", "Set", "Option", "Result"];

/// Abbreviations that clear the length rule and still say less than the
/// word. Closed and small on purpose -- the length rule does the real work.
const ABBREVIATIONS: &[(&str, &str)] = &[
    ("idx", "index"),
    ("cnt", "count"),
    ("tmp", "temp"),
    ("res", "result"),
    ("ret", "result"),
    ("val", "value"),
    ("msg", "message"),
    ("cfg", "config"),
    ("ctx", "context"),
    ("num", "number"),
    ("pos", "position"),
    ("arr", "array"),
    ("cur", "current"),
    ("prev", "previous"),
    ("src", "source"),
    ("dst", "destination"),
    ("buf", "buffer"),
    ("ptr", "pointer"),
    ("elem", "element"),
    ("args", "arguments"),
    ("param", "parameter"),
    ("params", "parameters"),
    ("tok", "token"),
    ("toks", "tokens"),
    ("err", "error"),
    ("dir", "directory"),
    ("len", "length"),
    ("conn", "connection"),
    ("req", "request"),
    ("resp", "response"),
];

pub struct Linter<'a> {
    prog: &'a Program,
    lines: Vec<&'a str>,
    opts: &'a Opts,
    out: Vec<Finding>,
}

pub fn lint(prog: &Program, src: &str, module: &str, opts: &Opts) -> Vec<Finding> {
    let mut l = Linter {
        prog,
        lines: src.lines().collect(),
        opts,
        out: Vec::new(),
    };
    l.run(module);
    l.out.sort_by_key(|f| (f.line, f.col));
    l.out
}

/// Parse without typechecking, as `fmt` does.
pub fn parse(src: &str, module: &str) -> Result<Program, crate::diag::Diag> {
    let lexed = crate::lexer::Lexer::tokenize_for_fmt(src)?;
    let mut p = crate::parser::Parser::new(lexed.toks);
    if crate::stdlib::embedded(module).is_some() {
        p = p.stdlib();
    }
    p.parse_program(module)
}

impl Linter<'_> {
    fn run(&mut self, module: &str) {
        if !module.is_empty() {
            self.check_module(module);
        }
        let prog = self.prog;
        for t in &prog.types {
            let name = bare(&t.name);
            self.check_case(Kind::Type, name, t.span);
            for tp in &t.tparams {
                self.check_case(Kind::TypeParam, tp, t.span);
            }
            for f in &t.fields {
                if !f.embedded {
                    self.value(Kind::Field, f);
                }
            }
            for v in &t.variants {
                self.check_case(Kind::Variant, &v.name, v.span);
            }
            for m in &t.methods {
                self.func(m);
            }
        }
        for f in &prog.funcs {
            self.func(f);
        }
        for c in &prog.consts {
            self.check_case(Kind::Const, bare(&c.name), c.span);
            self.expr(&c.init);
        }
        self.block(&prog.toplevel);
    }

    fn func(&mut self, f: &Func) {
        self.check_case(Kind::Function, bare(&f.name), f.span);
        for tp in f.tparams.iter().chain(&f.recv_tparams) {
            self.check_case(Kind::TypeParam, tp, f.span);
        }
        for p in &f.params {
            self.value(Kind::Param, p);
        }
        self.block(&f.body);
    }

    fn block(&mut self, body: &[Stmt]) {
        for s in body {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Decl {
                ty,
                name,
                init,
                span,
                ..
            } => {
                self.name(Kind::Local, name, *ty, *span, false);
                self.expr(init);
            }
            Stmt::Assign { value, .. } => self.expr(value),
            Stmt::SetIndex {
                obj, index, value, ..
            } => {
                self.expr(obj);
                self.expr(index);
                self.expr(value);
            }
            Stmt::SetField { obj, value, .. } => {
                self.expr(obj);
                self.expr(value);
            }
            Stmt::Return { value, .. } => {
                if let Some(v) = value {
                    self.expr(v);
                }
            }
            Stmt::Eval { expr, .. } => self.expr(expr),
            Stmt::Spawn { args, .. } => self.args(args),
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::ForRange {
                ty,
                name,
                from,
                to,
                body,
                span,
            } => {
                self.name(Kind::LoopVar, name, *ty, *span, true);
                self.expr(from);
                self.expr(to);
                self.block(body);
            }
            Stmt::ForIn {
                ty,
                name,
                iter,
                body,
                span,
            } => {
                self.name(Kind::LoopVar, name, *ty, *span, false);
                self.expr(iter);
                self.block(body);
            }
            Stmt::While { cond, body, .. } => {
                self.expr(cond);
                self.block(body);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                self.expr(scrutinee);
                for a in arms {
                    self.arm(a);
                }
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                self.expr(cond);
                self.block(then);
                if let Some(e) = els {
                    self.block(e);
                }
            }
            Stmt::ConstBlock { body, .. } => self.block(body),
        }
    }

    fn arm(&mut self, a: &MatchArm) {
        for b in &a.binds {
            self.bind(Kind::MatchBind, b, Some(&a.variant));
        }
        self.block(&a.body);
    }

    fn args(&mut self, a: &Args) {
        for e in &a.pos {
            self.expr(e);
        }
        for (_, e) in &a.named {
            self.expr(e);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Int(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::Str(..)
            | Expr::Var(..)
            | Expr::This(_) => {}
            Expr::Bin(_, a, b, _) | Expr::Index(a, b, _) | Expr::RepeatLit(a, b, _) => {
                self.expr(a);
                self.expr(b);
            }
            Expr::Un(_, a, _) | Expr::Field(a, _, _) | Expr::Try(a, _) => self.expr(a),
            Expr::Call(_, args, _) | Expr::EnumNew(_, _, args, _) | Expr::New(_, args, _) => {
                self.args(args)
            }
            Expr::MethodCall(recv, _, args, _) => {
                self.expr(recv);
                self.args(args);
            }
            Expr::SeqLit(items, _) => {
                for i in items {
                    self.expr(i);
                }
            }
            Expr::MapLit(pairs, _) => {
                for (k, v) in pairs {
                    self.expr(k);
                    self.expr(v);
                }
            }
            Expr::Lambda(params, body, _) => {
                for p in params {
                    self.bind(Kind::LambdaParam, p, None);
                }
                self.expr(body);
            }
        }
    }

    fn value(&mut self, kind: Kind, p: &Param) {
        self.bind(kind, p, None);
    }

    fn bind(&mut self, kind: Kind, p: &Param, variant: Option<&str>) {
        if let Some(def) = &p.default {
            self.expr(def);
        }
        self.name_in(kind, &p.name, p.ty, p.span, false, variant);
    }

    fn name(&mut self, kind: Kind, name: &str, ty: Ty, span: Span, counting: bool) {
        self.name_in(kind, name, ty, span, counting, None);
    }

    /// The checks every value name gets: casing, then length, then the
    /// abbreviation table. At most one finding per name, because three
    /// findings on `e` is noise and the first one already says "rename it".
    fn name_in(
        &mut self,
        kind: Kind,
        name: &str,
        ty: Ty,
        span: Span,
        counting: bool,
        variant: Option<&str>,
    ) {
        if name.starts_with("__") || self.opts.allow.iter().any(|a| a == name) {
            return;
        }
        let suggestion = self.suggest(name, ty, variant);
        if !is_snake(name) {
            let fix = to_snake(name);
            let reason = format!("{} `{name}` is not snake_case", kind.label());
            return self.report(kind, name, span, reason, Some(fix));
        }
        if name.len() < MIN_LEN && !self.builtin_ok(name, ty, counting) {
            let reason = format!(
                "{} `{name}` is too short; a name needs {MIN_LEN} or more characters",
                kind.label()
            );
            return self.report(kind, name, span, reason, suggestion);
        }
        if let Some((_, full)) = ABBREVIATIONS.iter().find(|(a, _)| *a == name) {
            let reason = format!("{} `{name}` is an abbreviation", kind.label());
            self.report(kind, name, span, reason, Some((*full).to_string()));
        }
    }

    fn builtin_ok(&self, name: &str, ty: Ty, counting: bool) -> bool {
        if self.opts.strict {
            return false;
        }
        match name {
            "i" | "j" => counting,
            "id" => true,
            "x" | "y" => matches!(ty, Ty::Int | Ty::Float),
            _ => false,
        }
    }

    /// A mechanical suggestion where one exists: an `Err` payload is an
    /// `error`; any other named type is its own name in snake_case.
    fn suggest(&self, name: &str, ty: Ty, variant: Option<&str>) -> Option<String> {
        if variant == Some("Err") {
            return Some("error".to_string());
        }
        let Ty::User(i) = ty else { return None };
        let full = bare(&self.prog.ty_exprs.get(i as usize)?.name);
        let tname = full.rsplit('.').next().unwrap_or(full).to_string();
        if CONTAINERS.contains(&tname.as_str()) {
            return None;
        }
        let snake = to_snake(&tname);
        (snake.len() >= MIN_LEN && snake != name && is_snake(&snake)).then_some(snake)
    }

    fn check_case(&mut self, kind: Kind, name: &str, span: Span) {
        if name.starts_with("__") || self.opts.allow.iter().any(|a| a == name) {
            return;
        }
        let (ok, want, fixed) = match kind {
            Kind::Type | Kind::Variant | Kind::TypeParam => {
                (is_pascal(name), "PascalCase", to_pascal(name))
            }
            Kind::Const => (
                is_screaming(name),
                "SCREAMING_SNAKE_CASE",
                to_snake(name).to_uppercase(),
            ),
            _ => (is_snake(name), "snake_case", to_snake(name)),
        };
        if !ok {
            let reason = format!("{} `{name}` is not {want}", kind.label());
            self.report(kind, name, span, reason, Some(fixed));
        } else if matches!(kind, Kind::Function | Kind::Const)
            && name.len() < MIN_LEN
            && !(kind == Kind::Function && !self.opts.strict && SHORT_WORDS.contains(&name))
        {
            let reason = format!(
                "{} `{name}` is too short; a name needs {MIN_LEN} or more characters",
                kind.label()
            );
            self.report(kind, name, span, reason, None);
        }
    }

    fn check_module(&mut self, module: &str) {
        if module.starts_with("__") || is_snake(module) {
            return;
        }
        let reason = format!("module `{module}` is not snake_case");
        self.out.push(Finding {
            line: 1,
            col: 1,
            kind: Kind::Module,
            name: module.to_string(),
            reason,
            suggestion: Some(to_snake(module)),
        });
    }

    /// Spans point at the start of the declaration, not the name, so find
    /// the name as a whole word on that line, after the span's column.
    fn locate(&self, span: Span, name: &str) -> (u32, u32) {
        let Some(text) = self.lines.get(span.line as usize - 1) else {
            return (span.line, span.col);
        };
        let chars: Vec<char> = text.chars().collect();
        let want: Vec<char> = name.chars().collect();
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let mut at = (span.col as usize).saturating_sub(1);
        while at + want.len() <= chars.len() {
            if chars[at..at + want.len()] == want[..]
                && (at == 0 || !word(chars[at - 1]))
                && chars.get(at + want.len()).is_none_or(|c| !word(*c))
            {
                return (span.line, at as u32 + 1);
            }
            at += 1;
        }
        (span.line, span.col)
    }

    fn report(
        &mut self,
        kind: Kind,
        name: &str,
        span: Span,
        reason: String,
        suggestion: Option<String>,
    ) {
        let (line, col) = self.locate(span, name);
        self.out.push(Finding {
            line,
            col,
            kind,
            name: name.to_string(),
            reason,
            suggestion,
        });
    }
}

fn is_snake(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.contains("__")
        && !name.ends_with('_')
}

fn is_pascal(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_screaming(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !name.contains("__")
        && !name.ends_with('_')
}

/// `ParseError` -> `parse_error`, `HTTPServer` -> `http_server`.
pub fn to_snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let acronym_end = i > 0
                && chars[i - 1].is_ascii_uppercase()
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if (prev_lower || acronym_end) && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c == '_' && (out.is_empty() || out.ends_with('_')) {
            continue;
        } else {
            out.push(c);
        }
    }
    out.trim_end_matches('_').to_string()
}

fn to_pascal(name: &str) -> String {
    name.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Per-kind tallies, for `--summary`.
pub fn tally(findings: &[Finding]) -> BTreeMap<Kind, usize> {
    let mut m = BTreeMap::new();
    for f in findings {
        *m.entry(f.kind).or_insert(0) += 1;
    }
    m
}

pub const USAGE: &str =
    "usage: m31c lint [--allow NAME[,NAME..]] [--strict] [--summary] <file|dir>...";

/// The `lint` subcommand. Exit 0 clean, 1 findings or a file that does not
/// parse, 2 bad usage.
pub fn main(args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let mut opts = Opts::default();
    let mut summary = false;
    let mut inputs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow" => {
                i += 1;
                let Some(list) = args.get(i) else {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                };
                opts.allow
                    .extend(list.split(',').filter(|s| !s.is_empty()).map(String::from));
            }
            "--strict" => opts.strict = true,
            "--summary" => summary = true,
            "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option `{other}`");
                return ExitCode::from(2);
            }
            other => inputs.push(other.to_string()),
        }
        i += 1;
    }
    if inputs.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }

    let mut files = Vec::new();
    for input in &inputs {
        if let Err(e) = collect(std::path::Path::new(input), &mut files) {
            eprintln!("cannot read {input}: {e}");
            return ExitCode::from(2);
        }
    }

    let mut all: Vec<Finding> = Vec::new();
    let mut unparsed = 0;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            eprintln!("cannot read {path}");
            unparsed += 1;
            continue;
        };
        let module = crate::modules::module_name(path);
        let prog = match parse(&src, &module) {
            Ok(p) => p,
            Err(d) => {
                eprintln!("{}", d.render(path));
                unparsed += 1;
                continue;
            }
        };
        for f in lint(&prog, &src, &module, &opts) {
            let hint = f
                .suggestion
                .as_ref()
                .map(|s| format!(" (try `{s}`)"))
                .unwrap_or_default();
            println!("{path}:{}:{}: {}{hint}", f.line, f.col, f.reason);
            all.push(f);
        }
    }
    if summary {
        for (kind, n) in tally(&all) {
            eprintln!("  {:<18}{n}", kind.label());
        }
        let mut names: BTreeMap<&str, usize> = BTreeMap::new();
        for f in &all {
            *names.entry(f.name.as_str()).or_insert(0) += 1;
        }
        let mut top: Vec<_> = names.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let shown: Vec<String> = top
            .iter()
            .take(12)
            .map(|(n, c)| format!("{n}:{c}"))
            .collect();
        eprintln!("  most common: {}", shown.join(" "));
    }
    eprintln!(
        "{} finding(s) in {} file(s); {unparsed} file(s) did not parse",
        all.len(),
        files.len()
    );
    if all.is_empty() && unparsed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn collect(path: &std::path::Path, out: &mut Vec<String>) -> std::io::Result<()> {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        for e in entries {
            let hidden = e
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if !hidden && (e.is_dir() || e.extension().is_some_and(|x| x == "m31")) {
                collect(&e, out)?;
            }
        }
    } else {
        out.push(path.to_string_lossy().into_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str, opts: &Opts) -> Vec<String> {
        let prog = parse(src, "sample").expect("parses");
        lint(&prog, src, "sample", opts)
            .into_iter()
            .map(|f| format!("{}:{} {}", f.line, f.col, f.name))
            .collect()
    }

    #[test]
    fn short_locals_and_params_are_reported_with_the_name_column() {
        let src = "int add(int a, int count) {\n    int s = a + count;\n    return s;\n}\n";
        assert_eq!(run(src, &Opts::default()), ["1:13 a", "2:9 s"]);
    }

    #[test]
    fn counting_loops_may_use_i_and_j_but_for_in_may_not() {
        let src = "for (int i in 0 .. 3) {\n    for (int j in 0 .. 3) {\n    }\n}\nList<int> items = [1];\nfor (int i in items) {\n}\n";
        assert_eq!(run(src, &Opts::default()), ["6:10 i"]);
        assert_eq!(
            run(
                src,
                &Opts {
                    strict: true,
                    ..Opts::default()
                }
            )
            .len(),
            3
        );
    }

    #[test]
    fn numeric_x_and_y_and_id_are_allowed_but_strict_drops_them() {
        let src = "type Point {\n    int x;\n    int y;\n    int id;\n}\n";
        assert!(run(src, &Opts::default()).is_empty());
        assert_eq!(
            run(
                src,
                &Opts {
                    strict: true,
                    ..Opts::default()
                }
            )
            .len(),
            3
        );
    }

    #[test]
    fn match_payloads_get_a_type_derived_suggestion() {
        let src = "type Config {\n    int size;\n}\nResult<Config, str> load() {\n    return Result<Config, str>.Ok(Config(size: 1));\n}\nmatch (load()) {\n    case Ok(Config c): {\n    }\n    case Err(str e): {\n    }\n}\n";
        let prog = parse(src, "sample").expect("parses");
        let found = lint(&prog, src, "sample", &Opts::default());
        let hints: Vec<_> = found.iter().map(|f| f.suggestion.clone()).collect();
        assert_eq!(
            hints,
            [Some("config".to_string()), Some("error".to_string())]
        );
    }

    #[test]
    fn allow_list_and_abbreviation_table() {
        let src = "int f(int idx, int n) {\n    return idx + n;\n}\n";
        let found = run(src, &Opts::default());
        assert!(found.iter().any(|f| f.ends_with(" idx")));
        let allowed = Opts {
            allow: vec!["n".into(), "idx".into(), "f".into()],
            ..Opts::default()
        };
        assert!(run(src, &allowed).is_empty());
    }

    #[test]
    fn casing_is_checked_per_kind() {
        assert!(is_snake("parse_error") && !is_snake("parseError") && !is_snake("a__b"));
        assert!(is_pascal("ParseError") && !is_pascal("parse_error"));
        assert!(is_screaming("MAX_SIZE") && !is_screaming("MaxSize"));
        assert_eq!(to_snake("HTTPServer"), "http_server");
        assert_eq!(to_snake("parseError"), "parse_error");
        assert_eq!(to_pascal("parse_error"), "ParseError");
    }

    #[test]
    fn seam_names_with_a_double_underscore_prefix_are_exempt() {
        let src = "int __o(int q) {\n    return q;\n}\n";
        let prog = parse(src, "text").expect("parses as stdlib");
        let found = lint(&prog, src, "text", &Opts::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "q");
    }
}
