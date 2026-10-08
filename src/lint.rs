//! `m31c lint` -- naming report. docs/naming-decision.md is the rulebook.
//!
//! Parse-only, like `fmt`: it has to work on a program that does not
//! typecheck, and on a program whose imports are not on disk (an extracted
//! repo). Report mode only -- it prints findings and an exit code, it never
//! edits a file and nothing in the gates calls it yet.
//!
//! Module names (a finding of kind "module name", on line 1) are checked
//! from the file's path, since the rule depends on where the file lives:
//!
//! * under a `lib/` directory (the stdlib): lowercase, one word,
//!   `^[a-z][a-z0-9]*$` -- `io`, `net`, `sha256`. The compiler-internal
//!   seam files `__floatfmt` and `__text` keep their `__` prefix and are
//!   skipped, like every other `__` name;
//! * any other module: `^[A-Z][A-Z0-9]*_[a-z][a-z0-9_]*$` -- `TUI_text`,
//!   `GIT_refs`, `MD_blocks`;
//! * not modules, so exempt: an entry point (a file with top-level
//!   statements, or named `main.m31` -- an imported file may not have
//!   statements, so they identify the program) and anything under a
//!   `tests/`, `corpus/`, `scripts/` or `docs/` directory.

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
    ModuleName,
    BooleanName,
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
            Kind::ModuleName => "module name",
            Kind::BooleanName => "boolean name",
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
    ("cols", "columns"),
    ("vis", "visible"),
    ("sym", "symbol"),
    ("sty", "style"),
    ("wid", "width"),
    ("keyw", "keyword"),
    ("rel", "relative"),
    ("hdr", "header"),
    ("oid", "object_id"),
    ("nid", "node_id"),
    ("segs", "segments"),
    ("pat", "pattern"),
    ("rep", "replacement"),
    ("hay", "haystack"),
];

/// Prefixes that mark a name as a yes/no question (naming-decision section 8,
/// item 10): the locked `is_` and `has_`, plus the natural-language ones.
const BOOL_PREFIXES: &[&str] = &[
    "is_", "has_", "can_", "should_", "did_", "was_", "needs_", "will_",
];

/// The entry point, exempt from the boolean rule like it is from every
/// other "reads at the call site" rule.
const ENTRY_NAMES: &[&str] = &["main"];

/// The name with every `_`-separated segment that is a table abbreviation
/// expanded, or `None` when no segment is one. `upper` is for constants:
/// segments are matched lower-cased and the expansion is upper-cased again.
fn expand_abbreviations(name: &str, upper: bool) -> Option<String> {
    let mut hit = false;
    let parts: Vec<String> = name
        .split('_')
        .map(|seg| {
            let low = seg.to_ascii_lowercase();
            match ABBREVIATIONS.iter().find(|(a, _)| *a == low) {
                Some((_, full)) => {
                    hit = true;
                    if upper {
                        full.to_ascii_uppercase()
                    } else {
                        (*full).to_string()
                    }
                }
                None => seg.to_string(),
            }
        })
        .collect();
    hit.then(|| parts.join("_"))
}

fn has_bool_prefix(name: &str) -> bool {
    BOOL_PREFIXES.iter().any(|p| name.starts_with(p))
}

pub struct Linter<'a> {
    prog: &'a Program,
    lines: Vec<&'a str>,
    opts: &'a Opts,
    out: Vec<Finding>,
}

pub fn lint(prog: &Program, src: &str, opts: &Opts) -> Vec<Finding> {
    let mut l = Linter {
        prog,
        lines: src.lines().collect(),
        opts,
        out: Vec::new(),
    };
    l.run();
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
    fn run(&mut self) {
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
        let name = bare(&f.name);
        let flagged = self.check_case(Kind::Function, name, f.span);
        if !flagged && f.ret == Ty::Bool && !self.bool_function_ok(name) {
            self.report_boolean(Kind::Function, name, f.span);
        }
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
        for b in a.binds.iter().flatten() {
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
        if let Some(full) = expand_abbreviations(name, false) {
            return self.report_abbreviation(kind, name, span, full);
        }
        let is_value = matches!(
            kind,
            Kind::Local | Kind::Param | Kind::Field | Kind::LambdaParam
        );
        if is_value && ty == Ty::Bool && !has_bool_prefix(name) {
            self.report_boolean(kind, name, span);
        }
    }

    /// A whole-name abbreviation (`buf`) or one segment of a longer name
    /// (`msg_buf`); the suggestion is the full name either way.
    fn report_abbreviation(&mut self, kind: Kind, name: &str, span: Span, full: String) {
        let reason = if name.contains('_') {
            format!("{} `{name}` contains an abbreviation", kind.label())
        } else {
            format!("{} `{name}` is an abbreviation", kind.label())
        };
        self.report(kind, name, span, reason, Some(full));
    }

    /// A value or function that answers a yes/no question and does not say
    /// so. `kind` only words the reason; the finding is a "boolean name".
    fn report_boolean(&mut self, what: Kind, name: &str, span: Span) {
        let reason = format!(
            "{} `{name}` is a bool; a yes/no name starts with is_, has_, can_, should_, did_, was_, needs_ or will_",
            what.label()
        );
        let fix = format!("is_{name}");
        self.report(Kind::BooleanName, name, span, reason, Some(fix));
    }

    /// Functions the boolean rule leaves alone: the entry point, the
    /// two-letter words (`a.eq(b)`), the seam, and anything in `--allow`.
    fn bool_function_ok(&self, name: &str) -> bool {
        has_bool_prefix(name)
            || name.starts_with("__")
            || self.opts.allow.iter().any(|a| a == name)
            || ENTRY_NAMES.contains(&name)
            || (!self.opts.strict && SHORT_WORDS.contains(&name))
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

    /// True when it reported something, so the caller can skip its own
    /// checks: one finding per name.
    fn check_case(&mut self, kind: Kind, name: &str, span: Span) -> bool {
        if name.starts_with("__") || self.opts.allow.iter().any(|a| a == name) {
            return false;
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
            return true;
        }
        let valued = matches!(kind, Kind::Function | Kind::Const);
        if valued
            && name.len() < MIN_LEN
            && !(kind == Kind::Function && !self.opts.strict && SHORT_WORDS.contains(&name))
        {
            let reason = format!(
                "{} `{name}` is too short; a name needs {MIN_LEN} or more characters",
                kind.label()
            );
            self.report(kind, name, span, reason, None);
            return true;
        }
        if valued {
            if let Some(full) = expand_abbreviations(name, kind == Kind::Const) {
                self.report_abbreviation(kind, name, span, full);
                return true;
            }
        }
        false
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

/// Directories whose files are not imported modules.
const EXEMPT_DIRS: &[&str] = &["tests", "corpus", "scripts", "docs"];

/// `^[a-z][a-z0-9]*$`: the stdlib shape.
fn is_stdlib_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// `^[A-Z][A-Z0-9]*_[a-z][a-z0-9_]*$`: `PREFIX_name`.
fn is_prefixed_name(name: &str) -> bool {
    let Some((prefix, rest)) = name.split_once('_') else {
        return false;
    };
    prefix
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        && prefix
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && rest.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The module-name rule (docs/naming-decision.md section 8, item 6). Needs
/// the file's path because the rule depends on where the file lives, so it
/// is separate from `lint`, which sees only a parsed program.
pub fn check_module_name(path: &str, prog: &Program, opts: &Opts) -> Option<Finding> {
    let path = std::path::Path::new(path);
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    if stem.starts_with("__") || opts.allow.contains(&stem) {
        return None;
    }
    let dirs: Vec<String> = path
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if dirs.iter().any(|d| EXEMPT_DIRS.contains(&d.as_str())) {
        return None;
    }
    if !prog.toplevel.is_empty() || stem == "main" {
        return None;
    }
    let (reason, suggestion) = if dirs.iter().any(|d| d == "lib") {
        if is_stdlib_name(&stem) {
            return None;
        }
        let one_word: String = stem
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        (
            format!("module `{stem}` is not lowercase, one word (stdlib)"),
            is_stdlib_name(&one_word).then_some(one_word),
        )
    } else {
        if is_prefixed_name(&stem) {
            return None;
        }
        // Derivable only when the name already has a PREFIX_ shape in the
        // wrong case: `git_refs` -> `GIT_refs`.
        let fixed = stem.split_once('_').and_then(|(prefix, rest)| {
            let fixed = format!(
                "{}_{}",
                prefix.to_ascii_uppercase(),
                rest.to_ascii_lowercase()
            );
            is_prefixed_name(&fixed).then_some(fixed)
        });
        let reason = if fixed.is_some() {
            format!("module `{stem}` is not PREFIX_name")
        } else {
            format!("module `{stem}` is not PREFIX_name; needs a PREFIX_ and a descriptive part")
        };
        (reason, fixed)
    };
    Some(Finding {
        line: 1,
        col: 1,
        kind: Kind::ModuleName,
        name: stem,
        reason,
        suggestion,
    })
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

/// Printed after `USAGE` by `lint --help`.
const HELP: &str = "module names (finding kind \"module name\", reported on line 1):
  lib/ (stdlib)     lowercase, one word: io, net, sha256
  any other module  PREFIX_name: TUI_text, GIT_refs, MD_blocks
  exempt            entry points (top-level statements, or main.m31) and
                    files under tests/, corpus/, scripts/, docs/

abbreviations (reported under the kind of the name that holds them):
  a name is flagged when it is a table abbreviation or when any
  `_`-separated segment is: header_len -> header_length, MAX_LEN ->
  MAX_LENGTH. Types and variants are not checked; --allow NAME silences
  the whole name

boolean names (finding kind \"boolean name\"):
  a local, parameter, lambda parameter or field of type bool, and a function
  or method returning bool, starts with is_, has_, can_, should_, did_,
  was_, needs_ or will_; suggestion is_<name>. Exempt: main, the
  two-letter function words (at, by, eq, of, ok, up) and --allow names

--summary prints a count per kind (\"boolean name\" included) and the
twelve most common names";

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
                println!("{USAGE}\n{HELP}");
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
        let named = check_module_name(path, &prog, &opts);
        for f in named.into_iter().chain(lint(&prog, &src, &opts)) {
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
        lint(&prog, src, opts)
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
        let found = lint(&prog, src, &Opts::default());
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
        let found = lint(&prog, src, &Opts::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "q");
    }

    fn module_name(path: &str, src: &str, opts: &Opts) -> Option<(String, Option<String>)> {
        let prog = parse(src, "sample").expect("parses");
        check_module_name(path, &prog, opts).map(|f| (f.name, f.suggestion))
    }

    const DECLS: &str = "int double_it(int value) {\n    return value + value;\n}\n";

    #[test]
    fn stdlib_names_are_lowercase_one_word() {
        let o = Opts::default();
        for ok in ["lib/io.m31", "lib/sha256.m31", "./lib/chacha20poly1305.m31"] {
            assert_eq!(module_name(ok, DECLS, &o), None, "{ok}");
        }
        assert_eq!(
            module_name("lib/my_mod.m31", DECLS, &o),
            Some(("my_mod".into(), Some("mymod".into())))
        );
        assert_eq!(
            module_name("lib/Sha256.m31", DECLS, &o),
            Some(("Sha256".into(), Some("sha256".into())))
        );
        // the compiler-internal seam files keep their `__` prefix
        assert_eq!(module_name("lib/__floatfmt.m31", DECLS, &o), None);
    }

    #[test]
    fn other_modules_need_a_prefix_and_a_descriptive_part() {
        let o = Opts::default();
        for ok in [
            "TUI_text.m31",
            "app/GIT_refs.m31",
            "MD_blocks.m31",
            "A1_b2_c.m31",
        ] {
            assert_eq!(module_name(ok, DECLS, &o), None, "{ok}");
        }
        assert_eq!(
            module_name("git_refs.m31", DECLS, &o),
            Some(("git_refs".into(), Some("GIT_refs".into())))
        );
        assert_eq!(
            module_name("TUI_Text.m31", DECLS, &o),
            Some(("TUI_Text".into(), Some("TUI_text".into())))
        );
        // not derivable: no way to know the prefix or the descriptive part
        for bad in ["tuitext", "text", "Text", "TUI", "TUI_", "TuiText"] {
            let path = format!("{bad}.m31");
            assert_eq!(
                module_name(&path, DECLS, &o),
                Some((bad.into(), None)),
                "{bad}"
            );
        }
        let prog = parse(DECLS, "sample").expect("parses");
        let f = check_module_name("tuitext.m31", &prog, &o).expect("flagged");
        assert_eq!((f.line, f.col, f.kind), (1, 1, Kind::ModuleName));
        assert!(f.reason.contains("needs a PREFIX_ and a descriptive part"));
    }

    #[test]
    fn entry_points_and_non_module_directories_are_exempt() {
        let o = Opts::default();
        // statements at the top level make a file the program
        assert_eq!(module_name("server.m31", "int a = 1;\n", &o), None);
        assert_eq!(module_name("main.m31", DECLS, &o), None);
        for dir in ["tests", "corpus", "scripts", "docs"] {
            let path = format!("repo/{dir}/helper.m31");
            assert_eq!(module_name(&path, DECLS, &o), None, "{dir}");
        }
        assert!(module_name("repo/helper.m31", DECLS, &o).is_some());
    }

    #[test]
    fn module_name_follows_allow_and_ignores_strict() {
        let allowed = Opts {
            allow: vec!["tuitext".into()],
            ..Opts::default()
        };
        assert_eq!(module_name("tuitext.m31", DECLS, &allowed), None);
        let strict = Opts {
            strict: true,
            ..Opts::default()
        };
        assert_eq!(module_name("TUI_text.m31", DECLS, &strict), None);
        assert!(module_name("tuitext.m31", DECLS, &strict).is_some());
    }

    fn kinds(src: &str, opts: &Opts) -> Vec<(String, Kind, Option<String>)> {
        let prog = parse(src, "sample").expect("parses");
        lint(&prog, src, opts)
            .into_iter()
            .map(|f| (f.name, f.kind, f.suggestion))
            .collect()
    }

    #[test]
    fn new_table_entries_are_abbreviations() {
        for (short, full) in [
            ("cols", "columns"),
            ("vis", "visible"),
            ("sym", "symbol"),
            ("sty", "style"),
            ("wid", "width"),
            ("keyw", "keyword"),
            ("rel", "relative"),
            ("hdr", "header"),
            ("oid", "object_id"),
            ("nid", "node_id"),
            ("segs", "segments"),
            ("pat", "pattern"),
            ("rep", "replacement"),
            ("hay", "haystack"),
        ] {
            let src = format!("int use_it(int {short}) {{\n    return {short};\n}}\n");
            assert_eq!(
                kinds(&src, &Opts::default()),
                [(short.to_string(), Kind::Param, Some(full.to_string()))]
            );
        }
    }

    #[test]
    fn any_segment_of_a_name_may_be_an_abbreviation() {
        let src = "type Frame {\n    int header_len;\n    int base_off;\n    int end;\n}\nint read_it(int msg_buf, int hex_val) {\n    int src_dst_len = msg_buf + hex_val;\n    return src_dst_len;\n}\n";
        let found = kinds(src, &Opts::default());
        let got: Vec<_> = found
            .iter()
            .map(|(n, k, s)| (n.as_str(), *k, s.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            got,
            [
                ("header_len", Kind::Field, "header_length"),
                ("msg_buf", Kind::Param, "message_buffer"),
                ("hex_val", Kind::Param, "hex_value"),
                ("src_dst_len", Kind::Local, "source_destination_length"),
            ]
        );
    }

    #[test]
    fn segment_rule_matches_whole_segments_and_honours_allow() {
        // `length`, `bufferize`, `plan` and `repeat` contain table entries
        // as substrings only; `off`, `end`, `on`, `of` and `by` are words.
        let src = "int plan_it(int bufferize, int repeat_by, int base_off, int on_end) {\n    return bufferize;\n}\n";
        assert!(kinds(src, &Opts::default()).is_empty());
        let src = "int use_it(int header_len) {\n    return header_len;\n}\n";
        assert_eq!(kinds(src, &Opts::default()).len(), 1);
        let allowed = Opts {
            allow: vec!["header_len".into()],
            ..Opts::default()
        };
        assert!(kinds(src, &allowed).is_empty());
    }

    #[test]
    fn functions_and_constants_get_the_segment_rule_but_types_do_not() {
        let src = "const int MAX_LEN = 4;\nconst int LIMIT = 4;\ntype Msg_Buf {\n    int size;\n}\ntype Args {\n    int size;\n}\nint Args.read_buf() {\n    return 1;\n}\nint parse_hdr() {\n    return 1;\n}\n";
        let got: Vec<_> = kinds(src, &Opts::default())
            .into_iter()
            .map(|(n, k, s)| (n, k, s.unwrap_or_default()))
            .collect();
        let want = |n: &str, k, s: &str| (n.to_string(), k, s.to_string());
        // `Msg_Buf` is not PascalCase, but that is the casing rule's finding
        // (a type) and not an abbreviation finding.
        assert!(got.contains(&want("MAX_LEN", Kind::Const, "MAX_LENGTH")));
        assert!(got.contains(&want("parse_hdr", Kind::Function, "parse_header")));
        assert!(got.contains(&want("read_buf", Kind::Function, "read_buffer")));
        assert!(!got.iter().any(|(n, _, _)| n == "LIMIT" || n == "Args"));
        let types: Vec<_> = got.iter().filter(|(_, k, _)| *k == Kind::Type).collect();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].2, "MsgBuf");
    }

    #[test]
    fn module_qualified_uses_of_args_are_not_declarations() {
        let src = "import args;\nint run_it() {\n    int count = args.count();\n    List<str> rest = args.rest();\n    return count + rest.size();\n}\n";
        assert!(kinds(src, &Opts::default()).is_empty());
    }

    #[test]
    fn bool_values_and_functions_need_a_question_prefix() {
        let src = "type Cell {\n    bool dirty;\n    bool is_empty;\n    bool can_wrap;\n    int count;\n}\nbool visible(bool done, bool has_more, bool should_stop, bool did_fail, bool was_hit, bool needs_ink, bool will_run, int amount) {\n    bool found = done;\n    bool is_last = found;\n    return is_last;\n}\n";
        let got: Vec<_> = kinds(src, &Opts::default())
            .into_iter()
            .map(|(n, k, s)| (n, k, s.unwrap_or_default()))
            .collect();
        let b = |n: &str| (n.to_string(), Kind::BooleanName, format!("is_{n}"));
        assert_eq!(got, [b("dirty"), b("visible"), b("done"), b("found")]);
    }

    #[test]
    fn boolean_rule_exemptions_and_non_bool_types() {
        // eq/ok are function words, main is the entry, --allow is honoured,
        // and only the exact type `bool` counts: a List<bool> or an int is
        // not a yes/no answer.
        let src = "bool eq(int left, int right) {\n    return left == right;\n}\nbool ok(int code) {\n    return code == 0;\n}\nbool main() {\n    return true;\n}\nbool enabled(List<bool> flags, int mask) {\n    return true;\n}\nint flag_count(int total) {\n    return total;\n}\n";
        let got = kinds(src, &Opts::default());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "enabled");
        let allowed = Opts {
            allow: vec!["enabled".into()],
            ..Opts::default()
        };
        assert!(kinds(src, &allowed).is_empty());
        // strict drops the function words, as it does for the length rule:
        // `eq` is then too short, which is the one finding it gets
        let strict = Opts {
            strict: true,
            ..Opts::default()
        };
        assert!(kinds(src, &strict).iter().any(|(n, _, _)| n == "eq"));
    }

    #[test]
    fn a_bad_name_gets_one_finding_not_two() {
        // `ok` as a bool parameter is too short; it is not also a boolean
        // name. `done_buf` is an abbreviation and not also a boolean name.
        let src = "int use_it(bool ok, bool done_buf) {\n    return 1;\n}\n";
        let got = kinds(src, &Opts::default());
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|(_, k, _)| *k == Kind::Param));
    }

    #[test]
    fn boolean_prefixes_need_the_underscore() {
        // `island` starts with `is` and `hash` with `has`, but neither says
        // `is_`/`has_`.
        let src = "int use_it(bool island, bool hash) {\n    return 1;\n}\n";
        assert_eq!(kinds(src, &Opts::default()).len(), 2);
    }
}
