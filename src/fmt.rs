//! The formatter.
//!
//! One canonical layout, no options. That is the whole point: the argument
//! for braces over significant indentation was that a formatter gives you
//! "one correct layout and nobody argues about it" without putting whitespace
//! in the grammar. This is the half of that bargain the language owes.
//!
//! **Declarations stay in the order they were written, except that a method
//! joins its type.** Declaring methods by qualified name lets them scatter
//! through a file, which was accepted deliberately on the grounds that a
//! formatter would gather them (docs/types.md). So it does, and that is the
//! only thing it moves: a method separated from its type by some other
//! declaration or statement is moved to follow the type's last member, and a
//! method written above its type moves to just below it. Safe, because
//! declarations are order-independent and statements keep their order.
//!
//! It used to do more -- every type first, then the free functions, then the
//! statements -- and that was worth less than it cost. Section-divider
//! comments (`// --- decoding ---`) belong to the file's layout, not to the
//! declaration under them, and there is no right place to put one when the
//! things around it are shuffled into a different order: the formatter
//! hoisted them along with whichever type they happened to sit above. Keeping
//! the written order is what gofmt does, and it keeps every comment in the
//! place its author chose.
//!
//! Comments are trivia to the parser and absent from the AST, so a formatter
//! that only walked the AST would silently delete them. The lexer keeps them
//! aside with their positions and they are re-emitted by line. A comment
//! block TOUCHING the declaration below it -- no blank line between -- is
//! that declaration's and travels with it. One with a blank line after it is
//! free: it stays exactly where it was among the declarations.
//!
//! **Lines are not wrapped.** gofmt does not wrap and rustfmt does; this
//! follows gofmt. Wrapping needs a width, a layout search and a rule for
//! every construct that can break, and it turns a one-token edit into a
//! reflowed paragraph in the diff. A line too long to read is better fixed by
//! its author with a name for a subexpression, which no formatter can invent.

use crate::ast::*;
use crate::diag::Span;
use crate::lexer::{Comment, Tok, Token};
use std::collections::HashMap;

pub struct Fmt {
    out: String,
    depth: usize,
    comments: Vec<Comment>,
    /// Which comments have been printed. A flag per comment rather than a
    /// cursor, because a method can be printed out of source order: a cursor
    /// advanced past one item's body would swallow the comments of every
    /// item before it.
    done: Vec<bool>,
    /// The first line of the item being printed. Body comments are only
    /// taken from its own range, never from an item printed later.
    lo: u32,
    /// The token stream, for what the AST does not record: where each block
    /// closes. A comment after a block's last statement is still inside the
    /// block, and only the closing brace's line says so.
    toks: Vec<Token>,
    /// For each `{` token, the index of the `}` that closes it.
    closer: HashMap<usize, usize>,
    /// How each string and integer literal was written, by position. The
    /// formatter keeps the spelling: decoding is many-to-one, and
    /// `"\u{feff}"`, `0o755` and `1_000` are written that way to be read.
    spellings: HashMap<(u32, u32), String>,
}

const INDENT: &str = "    ";

/// One thing at the top level, in the order the printer emits them.
#[derive(Clone, Copy, PartialEq)]
enum Unit {
    /// Entry `i` of the source-ordered entries, with its own comments.
    Entry(usize),
    /// A free comment block: run `i`.
    Free(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Import,
    Prim,
    /// A type or a function with a body.
    Item,
    Stmt,
}

/// A top-level declaration or statement.
struct Entry<'p> {
    what: What<'p>,
    kind: Kind,
    start: Span,
    /// The line of its last token: the closing brace, or the `;`.
    end: u32,
    /// The comment block touching it from above.
    comments: Vec<usize>,
}

enum What<'p> {
    Import(&'p Import),
    Type(&'p TypeDecl),
    Func(&'p Func),
    Stmt(&'p Stmt),
}

/// A run of own-line comments on consecutive lines, as comment indices.
struct Run {
    comments: Vec<usize>,
    first: u32,
    last: u32,
}

pub fn format(p: &Program, lexed: crate::lexer::Lexed) -> String {
    let n = lexed.comments.len();
    let mut closer = HashMap::new();
    let mut open: Vec<usize> = Vec::new();
    for (i, t) in lexed.toks.iter().enumerate() {
        match t.tok {
            Tok::LBrace => open.push(i),
            Tok::RBrace => {
                if let Some(o) = open.pop() {
                    closer.insert(o, i);
                }
            }
            _ => {}
        }
    }
    let mut f = Fmt {
        out: String::new(),
        depth: 0,
        comments: lexed.comments,
        done: vec![false; n],
        lo: 0,
        toks: lexed.toks,
        closer,
        spellings: lexed
            .spellings
            .into_iter()
            .map(|(s, text)| ((s.line, s.col), text))
            .collect(),
    };
    f.program(p);
    f.trailing();
    // Exactly one trailing newline, no blank lines before it.
    let mut s = f.out.trim_end().to_string();
    s.push('\n');
    s
}

impl Fmt {
    fn line(&mut self, s: &str) {
        for _ in 0..self.depth {
            self.out.push_str(INDENT);
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn blank(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    // ---- the token stream -------------------------------------------------

    /// The first token at or after `s`.
    fn tok_at(&self, s: Span) -> usize {
        self.toks
            .partition_point(|t| (t.span.line, t.span.col) < (s.line, s.col))
    }

    /// The first `{` from token `i` on that is not inside parentheses or
    /// brackets -- the body of whatever construct starts at `i`. A map
    /// literal in a condition or a default argument is inside `(...)`.
    fn body_open(&self, mut i: usize) -> Option<usize> {
        let mut depth = 0i32;
        while let Some(t) = self.toks.get(i) {
            match t.tok {
                Tok::LParen | Tok::LBracket => depth += 1,
                Tok::RParen | Tok::RBracket => depth -= 1,
                Tok::LBrace if depth <= 0 => return Some(i),
                // A bodiless declaration: `prim`, `distinct`.
                Tok::Semi if depth <= 0 => return None,
                Tok::Eof => return None,
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// The line of the brace closing the block that opens at token `open`.
    fn close_line(&self, open: Option<usize>) -> Option<u32> {
        open.and_then(|o| self.closer.get(&o))
            .map(|&c| self.toks[c].span.line)
    }

    /// The line of the brace closing the body of the construct at `s`.
    fn body_close(&self, s: Span) -> Option<u32> {
        self.close_line(self.body_open(self.tok_at(s)))
    }

    /// The last line of the top-level declaration or statement at `s`: its
    /// `;`, or the brace that closes it with no `else` after.
    fn extent_end(&self, s: Span) -> u32 {
        let mut i = self.tok_at(s);
        let mut depth = 0i32;
        while let Some(t) = self.toks.get(i) {
            match t.tok {
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket => depth -= 1,
                Tok::RBrace => {
                    depth -= 1;
                    let more = matches!(self.toks.get(i + 1).map(|t| &t.tok), Some(Tok::KwElse));
                    if depth <= 0 && !more {
                        // A declaration may still end in `;` on this line,
                        // as `Map<str, int> m = {};` does; same line either
                        // way.
                        return t.span.line;
                    }
                }
                Tok::Semi if depth <= 0 => return t.span.line,
                Tok::Eof => return t.span.line,
                _ => {}
            }
            i += 1;
        }
        s.line
    }

    // ---- comments ---------------------------------------------------------

    /// The last line a comment occupies; a block comment can span several.
    fn last_line(&self, i: usize) -> u32 {
        let c = &self.comments[i];
        c.line + c.text.lines().count().saturating_sub(1) as u32
    }

    fn print_comment(&mut self, i: usize) {
        self.done[i] = true;
        let text = self.comments[i].text.clone();
        for part in text.lines() {
            self.line(part.trim());
        }
    }

    /// Emit every not-yet-printed comment in the current item's range that
    /// appeared before `line`.
    fn comments_before(&mut self, line: u32) {
        for i in 0..self.comments.len() {
            let c = &self.comments[i];
            if self.done[i] || c.line < self.lo || c.line >= line {
                continue;
            }
            if c.own_line {
                self.print_comment(i);
            } else {
                // Trailing: put it back on the line it followed.
                self.done[i] = true;
                let text = c.text.trim().to_string();
                let t = self.out.trim_end().to_string();
                self.out = format!("{t}  {text}\n");
            }
        }
    }

    /// Before a block's closing brace: a comment after its last statement is
    /// still inside it. Printing it before the `}` is the whole fix for the
    /// formatter that moved `return 1;  // why` below the brace.
    fn close_block(&mut self, close: Option<u32>) {
        if let Some(c) = close {
            self.comments_before(c);
        }
    }

    fn trailing(&mut self) {
        self.lo = 0;
        for i in 0..self.comments.len() {
            if !self.done[i] {
                self.print_comment(i);
            }
        }
    }

    // ---- items --------------------------------------------------------

    /// The top level, in source order but for methods joining their types.
    fn program(&mut self, p: &Program) {
        // Every declaration and statement, in source order.
        let mut entries: Vec<Entry> = Vec::new();
        let mut raw: Vec<(What, Kind, Span)> = Vec::new();
        for i in &p.imports {
            raw.push((What::Import(i), Kind::Import, i.span));
        }
        for t in &p.types {
            raw.push((What::Type(t), Kind::Item, t.span));
        }
        for f in &p.funcs {
            let k = if f.is_prim { Kind::Prim } else { Kind::Item };
            raw.push((What::Func(f), k, f.span));
        }
        for s in &p.toplevel {
            raw.push((What::Stmt(s), Kind::Stmt, stmt_span(s)));
        }
        raw.sort_by_key(|(_, _, s)| (s.line, s.col));
        for (what, kind, start) in raw {
            let end = self.extent_end(start);
            entries.push(Entry {
                what,
                kind,
                start,
                end,
                comments: Vec::new(),
            });
        }

        // Own-line comments outside every entry, in runs of consecutive
        // lines. A run touching the entry below it is that entry's; any other
        // run is free and keeps its place. A comment opening an entry's own
        // first line (`/* x */ int y = 1;`) is the entry's too.
        let inside =
            |line: u32, es: &[Entry]| es.iter().any(|e| e.start.line <= line && line <= e.end);
        let mut runs: Vec<Run> = Vec::new();
        for i in 0..self.comments.len() {
            let c = &self.comments[i];
            if !c.own_line {
                continue;
            }
            if let Some(e) = entries.iter_mut().find(|e| e.start.line == c.line) {
                e.comments.push(i);
                continue;
            }
            if inside(c.line, &entries) {
                continue;
            }
            let (first, last) = (c.line, self.last_line(i));
            match runs.last_mut() {
                Some(r) if r.last + 1 == first => {
                    r.comments.push(i);
                    r.last = last;
                }
                _ => runs.push(Run {
                    comments: vec![i],
                    first,
                    last,
                }),
            }
        }
        let mut free: Vec<Run> = Vec::new();
        for r in runs {
            match entries.iter_mut().find(|e| e.start.line == r.last + 1) {
                Some(e) => {
                    // Above any comment on the entry's own first line.
                    let mut cs = r.comments;
                    cs.append(&mut e.comments);
                    e.comments = cs;
                }
                None => free.push(r),
            }
        }

        let order = self.order(&entries, &free);

        let mut prev: Option<Unit> = None;
        for u in order {
            let blank = match (prev, u) {
                (None, _) => false,
                (Some(Unit::Free(_)), _) => true,
                (Some(Unit::Entry(e)), Unit::Free(r)) => {
                    // The source's own spacing, except that an item is always
                    // set apart from what follows it.
                    entries[e].kind == Kind::Item || self.gap_before(&free[r], &entries, &free)
                }
                (Some(Unit::Entry(a)), Unit::Entry(b)) => {
                    let (a, b) = (entries[a].kind, entries[b].kind);
                    // Runs of imports, of `prim`s and of statements are
                    // lists and stay together; a definition stands apart.
                    !(a == b && a != Kind::Item)
                }
            };
            if blank {
                self.blank();
            }
            match u {
                Unit::Free(r) => {
                    for &i in &free[r].comments {
                        self.print_comment(i);
                    }
                }
                Unit::Entry(e) => self.entry(&entries[e]),
            }
            prev = Some(u);
        }
    }

    /// Whether the source had a blank line above free run `r`.
    fn gap_before(&self, r: &Run, entries: &[Entry], free: &[Run]) -> bool {
        let above = entries
            .iter()
            .map(|e| e.end)
            .chain(free.iter().map(|f| f.last))
            .filter(|&l| l < r.first)
            .max();
        above.is_none_or(|l| r.first > l + 1)
    }

    /// The order to print in: source order, except that a method of a type
    /// declared here follows that type's group -- the type and the methods
    /// already placed after it. A method already with its group, with at most
    /// comments between, stays put, so a divider inside a type's run of
    /// methods is not disturbed; one separated from its group by anything
    /// else is moved to the end of the group, taking only the comments that
    /// touch it.
    fn order(&self, entries: &[Entry], free: &[Run]) -> Vec<Unit> {
        let mut src: Vec<(u32, Unit)> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            src.push((e.start.line, Unit::Entry(i)));
        }
        for (i, r) in free.iter().enumerate() {
            src.push((r.first, Unit::Free(i)));
        }
        src.sort_by_key(|(l, _)| *l);

        let types: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e.what {
                What::Type(t) => Some(t.name.as_str()),
                _ => None,
            })
            .collect();
        let recv_of = |u: Unit| match u {
            Unit::Entry(i) => match entries[i].what {
                What::Func(f) => f.recv.as_deref().filter(|r| types.contains(r)),
                _ => None,
            },
            Unit::Free(_) => None,
        };

        let mut out: Vec<Unit> = Vec::new();
        // Where each type's group currently ends in `out`.
        let mut last: HashMap<&str, usize> = HashMap::new();
        // Methods met before their type, waiting for it.
        let mut waiting: HashMap<&str, Vec<Unit>> = HashMap::new();
        for (_, u) in src {
            if let Unit::Entry(i) = u {
                if let What::Type(t) = entries[i].what {
                    out.push(u);
                    let mut at = out.len() - 1;
                    for m in waiting.remove(t.name.as_str()).unwrap_or_default() {
                        out.push(m);
                        at = out.len() - 1;
                    }
                    last.insert(t.name.as_str(), at);
                    continue;
                }
            }
            let Some(r) = recv_of(u) else {
                out.push(u);
                continue;
            };
            let Some(&at) = last.get(r) else {
                waiting.entry(r).or_default().push(u);
                continue;
            };
            let settled = out[at + 1..].iter().all(|x| matches!(x, Unit::Free(_)));
            if settled {
                out.push(u);
                last.insert(r, out.len() - 1);
            } else {
                out.insert(at + 1, u);
                for v in last.values_mut() {
                    if *v > at {
                        *v += 1;
                    }
                }
                last.insert(r, at + 1);
            }
        }
        // A method whose type never came: not possible for a type declared
        // here, but losing a declaration is the one thing a formatter must
        // never do.
        let mut rest: Vec<Unit> = waiting.into_values().flatten().collect();
        rest.sort_by_key(|u| match u {
            Unit::Entry(i) => (entries[*i].start.line, entries[*i].start.col),
            Unit::Free(_) => (0, 0),
        });
        out.extend(rest);
        out
    }

    fn entry(&mut self, e: &Entry) {
        for &i in &e.comments {
            self.print_comment(i);
        }
        self.lo = e.start.line;
        match e.what {
            What::Import(i) => self.line(&format!("import {};", i.name)),
            What::Type(t) => self.type_decl(t),
            What::Func(f) => self.func(f),
            What::Stmt(s) => self.stmt(s),
        }
        // Whatever is left in the entry's lines: a comment trailing its last
        // line (`}  // end`), or one inside an expression that spans lines.
        // Printed here, it cannot end up attached to some later item.
        self.lo = e.start.line;
        self.comments_before(e.end + 1);
        self.lo = 0;
    }

    fn type_decl(&mut self, t: &TypeDecl) {
        let vis = if t.is_pub { "pub " } else { "" };
        if let Some(b) = t.distinct_base {
            self.line(&format!("{vis}distinct {} {};", self.ty(b), shown(&t.name)));
            return;
        }
        let kw = if t.is_interface {
            "interface"
        } else if t.is_enum {
            "enum"
        } else {
            "type"
        };
        let tp = if t.tparams.is_empty() {
            String::new()
        } else {
            format!("<{}>", t.tparams.join(", "))
        };
        self.line(&format!("{vis}{kw} {}{tp} {{", shown(&t.name)));
        self.depth += 1;
        for v in &t.variants {
            self.comments_before(v.span.line);
            if v.payload.is_empty() {
                self.line(&format!("{};", v.name));
            } else {
                let ps: Vec<String> = v.payload.iter().map(|p| self.ty(*p)).collect();
                self.line(&format!("{}({});", v.name, ps.join(", ")));
            }
        }
        for f in &t.fields {
            self.comments_before(f.span.line);
            if f.embedded {
                self.line(&format!("{};", self.ty(f.ty)));
            } else {
                let d = match &f.default {
                    Some(e) => format!(" = {}", self.expr(e)),
                    None => String::new(),
                };
                self.line(&format!("{} {}{d};", self.ty(f.ty), f.name));
            }
        }
        for m in &t.methods {
            self.comments_before(m.span.line);
            self.line(&format!(
                "{} {}({});",
                self.ty(m.ret),
                m.name,
                self.params(&m.params)
            ));
        }
        self.close_block(self.body_close(t.span));
        self.depth -= 1;
        self.line("}");
    }

    fn func(&mut self, f: &Func) {
        let tp = if f.tparams.is_empty() {
            String::new()
        } else {
            format!("<{}>", f.tparams.join(", "))
        };
        let name = match &f.recv {
            Some(r) if !f.recv_tparams.is_empty() => {
                format!("{}<{}>.{}", shown(r), f.recv_tparams.join(", "), f.name)
            }
            Some(r) => format!("{}.{}", shown(r), f.name),
            None => shown(&f.name),
        };
        let vis = if f.is_pub { "pub " } else { "" };
        let kw = if f.is_static { "static " } else { "" };
        // A `prim` has no body: signature, semicolon, done.
        if f.is_prim {
            self.line(&format!(
                "prim {} {name}({});",
                self.ty(f.ret),
                self.params(&f.params)
            ));
            return;
        }
        self.line(&format!(
            "{vis}{kw}{} {name}{tp}({}) {{",
            self.ty(f.ret),
            self.params(&f.params)
        ));
        self.depth += 1;
        self.block(&f.body, self.body_close(f.span));
        self.depth -= 1;
        self.line("}");
    }

    fn params(&self, ps: &[Param]) -> String {
        ps.iter()
            .map(|p| {
                let d = match &p.default {
                    Some(e) => format!(" = {}", self.expr(e)),
                    None => String::new(),
                };
                format!("{} {}{d}", self.ty(p.ty), p.name)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    // ---- statements ---------------------------------------------------

    /// A block's statements, then any comment before its closing brace,
    /// which is on line `close`.
    fn block(&mut self, stmts: &[Stmt], close: Option<u32>) {
        for s in stmts {
            self.comments_before(stmt_span(s).line);
            self.stmt(s);
        }
        self.close_block(close);
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Decl {
                ty,
                name,
                init,
                is_const,
                ..
            } => {
                let c = if *is_const { "const " } else { "" };
                self.line(&format!(
                    "{c}{} {name} = {};",
                    self.ty(*ty),
                    self.expr(init)
                ));
            }
            Stmt::Assign { name, value, .. } => {
                self.line(&format!("{name} = {};", self.expr(value)));
            }
            Stmt::SetField {
                obj, field, value, ..
            } => {
                self.line(&format!(
                    "{}.{field} = {};",
                    self.expr(obj),
                    self.expr(value)
                ));
            }
            Stmt::SetIndex {
                obj, index, value, ..
            } => {
                self.line(&format!(
                    "{}[{}] = {};",
                    self.expr(obj),
                    self.expr(index),
                    self.expr(value)
                ));
            }
            Stmt::Return { value: Some(e), .. } => {
                self.line(&format!("return {};", self.expr(e)));
            }
            Stmt::Return { value: None, .. } => self.line("return;"),
            Stmt::Eval { expr, .. } => {
                let e = self.expr(expr);
                self.line(&format!("{e};"));
            }
            Stmt::Break { .. } => self.line("break;"),
            Stmt::Continue { .. } => self.line("continue;"),
            Stmt::Spawn { name, args, .. } => {
                self.line(&format!("spawn {name}({});", self.args(args)));
            }
            Stmt::While {
                cond, body, span, ..
            } => {
                self.line(&format!("while ({}) {{", self.expr(cond)));
                self.depth += 1;
                self.block(body, self.body_close(*span));
                self.depth -= 1;
                self.line("}");
            }
            Stmt::ForIn {
                ty,
                name,
                iter,
                body,
                span,
            } => {
                self.line(&format!(
                    "for ({} {name} in {}) {{",
                    self.ty(*ty),
                    self.expr(iter)
                ));
                self.depth += 1;
                self.block(body, self.body_close(*span));
                self.depth -= 1;
                self.line("}");
            }
            Stmt::Match {
                scrutinee,
                arms,
                span,
            } => {
                self.line(&format!("match ({}) {{", self.expr(scrutinee)));
                self.depth += 1;
                for a in arms {
                    self.comments_before(a.span.line);
                    let binds: Vec<String> = a
                        .binds
                        .iter()
                        .map(|b| format!("{} {}", self.ty(b.ty), b.name))
                        .collect();
                    let head = if binds.is_empty() {
                        a.variant.clone()
                    } else {
                        format!("{}({})", a.variant, binds.join(", "))
                    };
                    self.line(&format!("case {head}: {{"));
                    self.depth += 1;
                    self.block(&a.body, self.body_close(a.span));
                    self.depth -= 1;
                    self.line("}");
                }
                // After the last arm, still inside the `match`.
                self.close_block(self.body_close(*span));
                self.depth -= 1;
                self.line("}");
            }
            Stmt::If {
                cond,
                then,
                els,
                span,
            } => {
                // The then-block's braces, and the else-block's: the `{`
                // two tokens after the `}` that closes the then-block, past
                // the `else`.
                let open = self.body_open(self.tok_at(*span));
                let else_open = open
                    .and_then(|o| self.closer.get(&o))
                    .map(|&c| c + 2)
                    .filter(|&i| matches!(self.toks.get(i).map(|t| &t.tok), Some(Tok::LBrace)));
                self.line(&format!("if ({}) {{", self.expr(cond)));
                self.depth += 1;
                self.block(then, self.close_line(open));
                self.depth -= 1;
                match els {
                    // `else if` stays on one line rather than nesting.
                    Some(e) if e.len() == 1 && matches!(e[0], Stmt::If { .. }) => {
                        let tail = self.rendered(&e[0]);
                        let mut it = tail.lines();
                        let first = it.next().unwrap_or("").trim_start();
                        self.line(&format!("}} else {first}"));
                        for rest in it {
                            self.out.push_str(rest);
                            self.out.push('\n');
                        }
                    }
                    Some(e) => {
                        self.line("} else {");
                        self.depth += 1;
                        self.block(e, self.close_line(else_open));
                        self.depth -= 1;
                        self.line("}");
                    }
                    None => self.line("}"),
                }
            }
        }
    }

    /// Render one statement in isolation, for the `else if` join.
    fn rendered(&mut self, s: &Stmt) -> String {
        let saved = std::mem::take(&mut self.out);
        self.stmt(s);
        std::mem::replace(&mut self.out, saved)
    }

    // ---- expressions --------------------------------------------------

    fn ty(&self, t: Ty) -> String {
        // The formatter runs before monomorphisation, so a `Ty::User` still
        // names what the source wrote. Printing it needs the arena, which
        // the AST does not carry -- so the parser's spelling is recovered
        // from the interned name by the caller of `format`.
        match t {
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Void => "void".into(),
            Ty::User(i) => TYPE_NAMES.with(|n| {
                n.borrow()
                    .get(i as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("T{i}"))
            }),
        }
    }

    fn args(&self, a: &Args) -> String {
        let mut parts: Vec<String> = a.pos.iter().map(|e| self.expr(e)).collect();
        for (n, e) in &a.named {
            parts.push(format!("{n}: {}", self.expr(e)));
        }
        parts.join(", ")
    }

    fn expr(&self, e: &Expr) -> String {
        match e {
            Expr::EnumNew(ty, variant, args, _) => {
                let a = self.args(args);
                if a.is_empty() {
                    format!("{}.{variant}", self.ty(*ty))
                } else {
                    format!("{}.{variant}({a})", self.ty(*ty))
                }
            }
            Expr::Try(e, _) => format!("{}?", self.expr(e)),
            Expr::SeqLit(items, _) => {
                let xs: Vec<String> = items.iter().map(|e| self.expr(e)).collect();
                format!("[{}]", xs.join(", "))
            }
            Expr::RepeatLit(v, n, _) => format!("[{}; {}]", self.expr(v), self.expr(n)),
            Expr::MapLit(items, _) => {
                let xs: Vec<String> = items
                    .iter()
                    .map(|(k, v)| format!("{}: {}", self.expr(k), self.expr(v)))
                    .collect();
                format!("{{{}}}", xs.join(", "))
            }
            // The first byte tells the two kinds of spelling apart, so a
            // synthesised node that happened to share a string's position
            // could not print as that string.
            Expr::Int(n, span) => match self.spellings.get(&(span.line, span.col)) {
                Some(text) if text.starts_with(|c: char| c.is_ascii_digit()) => text.clone(),
                _ => n.to_string(),
            },
            Expr::Float(x, _) => fmt_float(*x),
            Expr::Bool(b, _) => b.to_string(),
            Expr::Str(s, span) => match self.spellings.get(&(span.line, span.col)) {
                Some(text) => text.clone(),
                None => crate::lexer::quote(s),
            },
            Expr::Var(n, _) => n.clone(),
            Expr::This(_) => "this".to_string(),
            Expr::Bin(op, l, r, _) => {
                let p = prec(*op);
                format!(
                    "{} {} {}",
                    self.operand(l, true, p),
                    op.spelling(),
                    self.operand(r, false, p)
                )
            }
            Expr::Un(op, x, _) => {
                let o = match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "!",
                    UnOp::BitNot => "~",
                };
                format!("{o}{}", self.operand(x, true, UNARY))
            }
            Expr::Call(n, a, _) => format!("{n}({})", self.args(a)),
            Expr::MethodCall(o, m, a, _) => {
                format!("{}.{m}({})", self.operand(o, true, POSTFIX), self.args(a))
            }
            Expr::Field(o, f, _) => format!("{}.{f}", self.operand(o, true, POSTFIX)),
            Expr::Index(o, i, _) => format!("{}[{}]", self.operand(o, true, POSTFIX), self.expr(i)),
            Expr::New(t, a, _) => format!("{}({})", self.ty(*t), self.args(a)),
        }
    }

    /// Parenthesise a subexpression exactly when dropping the parentheses
    /// would change how it reparses, the way gofmt does: a looser operator
    /// under a tighter one, or an equal one on the right, since every binary
    /// operator associates to the left. Anything more is noise -- the old
    /// rule of parenthesising every nested operator turned `a * b + c` into
    /// `(a * b) + c` in every file it touched.
    ///
    /// The "formatter preserves meaning" gate reformats the whole corpus and
    /// compares the emitted C, so a mistake here, or drift from the parser's
    /// table, is caught there rather than trusted.
    fn operand(&self, e: &Expr, left: bool, parent: u8) -> String {
        match e {
            Expr::Bin(op, ..) => {
                let p = prec(*op);
                if p < parent || (p == parent && !left) {
                    format!("({})", self.expr(e))
                } else {
                    self.expr(e)
                }
            }
            // A unary operator binds tighter than any binary one, so it
            // never needs parentheses as an operand of one. Under another
            // unary it keeps them: `-(-x)`, never `--x`.
            Expr::Un(..) if parent < UNARY => self.expr(e),
            Expr::Un(..) => format!("({})", self.expr(e)),
            _ => self.expr(e),
        }
    }
}

/// Binding power of a binary operator: higher binds tighter. The same
/// numbers as the parser's `infix_bp`, which is what makes printing without
/// parentheses reparse to the same tree.
fn prec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Ne => 3,
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
        BinOp::BitOr => 5,
        BinOp::BitXor => 6,
        BinOp::BitAnd => 7,
        BinOp::Shl | BinOp::Shr => 8,
        BinOp::Add | BinOp::Sub => 9,
        BinOp::Mul | BinOp::Div | BinOp::Rem => 10,
    }
}

/// Binding power of a unary operator -- the parser's `UNARY_BP`.
const UNARY: u8 = 11;
/// What a postfix operand (`.f`, `[i]`, `.m()`) binds with: tighter than
/// anything, so any operator expression under it needs parentheses.
const POSTFIX: u8 = 12;

thread_local! {
    /// Source spellings for `Ty::User`, set by the driver before formatting.
    static TYPE_NAMES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The module being formatted. A type declared HERE is written bare; one
    /// from elsewhere keeps its `mod.` qualifier, or the formatted file no
    /// longer compiles.
    static THIS_MODULE: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
}

/// Record how each interned type was spelled, so the formatter can print it.
///
/// Rendered recursively from the arena rather than by reading the table being
/// built -- the first version did the latter and printed `Pair<T0, T1>`,
/// because every argument was looked up in a table that was still empty.
pub fn set_type_names(p: &Program) {
    THIS_MODULE.with(|m| *m.borrow_mut() = p.module.clone());
    let rendered: Vec<String> = (0..p.ty_exprs.len())
        .map(|i| render_ty(&p.ty_exprs, &Ty::User(i as u32)))
        .collect();
    TYPE_NAMES.with(|n| *n.borrow_mut() = rendered);
}

/// Names are interned module-qualified; source has to come back out the way
/// it went in -- bare for this module's own types, qualified for anyone
/// else's.
fn shown(name: &str) -> String {
    match name.split_once('#') {
        None => name.to_string(),
        Some((m, n)) => {
            let here = THIS_MODULE.with(|t| t.borrow().clone());
            if m == here {
                n.to_string()
            } else {
                format!("{m}.{n}")
            }
        }
    }
}

fn render_ty(exprs: &[TyExpr], t: &Ty) -> String {
    match t {
        Ty::Int => "int".into(),
        Ty::Float => "float".into(),
        Ty::Bool => "bool".into(),
        Ty::Str => "str".into(),
        Ty::Bytes => "bytes".into(),
        Ty::Void => "void".into(),
        Ty::User(i) => match exprs.get(*i as usize) {
            None => format!("T{i}"),
            Some(e) if e.args.is_empty() => shown(&e.name),
            Some(e) => {
                let args: Vec<String> = e.args.iter().map(|a| render_ty(exprs, a)).collect();
                format!("{}<{}>", shown(&e.name), args.join(", "))
            }
        },
    }
}

fn stmt_span(s: &Stmt) -> Span {
    match s {
        Stmt::Decl { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::SetField { span, .. }
        | Stmt::SetIndex { span, .. }
        | Stmt::Match { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Eval { span, .. }
        | Stmt::Break { span }
        | Stmt::Continue { span }
        | Stmt::Spawn { span, .. }
        | Stmt::While { span, .. }
        | Stmt::ForIn { span, .. }
        | Stmt::If { span, .. } => *span,
    }
}

/// A float literal, printed so it reads like what was written and parses back
/// to the same value. An integral value keeps its `.0`, or it would come back
/// as an `int` and change the program's meaning -- which the formatter gate
/// would catch, but only after the fact.
fn fmt_float(x: f64) -> String {
    for p in 1..=17 {
        let s = format!("{x:.p$}", p = p);
        if s.parse::<f64>() == Ok(x) {
            return s;
        }
    }
    // Too small for 17 places: Rust's shortest form, which has an exponent.
    // It writes `1e-300`, which is not a literal here -- the dot is
    // mandatory before an exponent (§1.5) -- so put the `.0` back.
    let s = format!("{x:e}");
    match s.split_once('e') {
        Some((m, e)) if !m.contains('.') => format!("{m}.0e{e}"),
        _ => s,
    }
}
