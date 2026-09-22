//! The formatter.
//!
//! One canonical layout, no options. That is the whole point: the argument
//! for braces over significant indentation was that a formatter gives you
//! "one correct layout and nobody argues about it" without putting whitespace
//! in the grammar. This is the half of that bargain the language owes.
//!
//! **Methods are grouped under their type.** Declaring them by qualified name
//! lets them scatter through a file, which was accepted deliberately on the
//! grounds that a formatter would gather them. So it does: every declaration
//! is emitted in source order, each type immediately followed by its own
//! methods, then free functions, then the top-level statements. That is safe
//! because declarations are order-independent and statements keep their
//! relative order.
//!
//! Comments are trivia to the parser and absent from the AST, so a formatter
//! that only walked the AST would silently delete them. The lexer keeps them
//! aside with their positions and they are re-emitted by line.

use crate::ast::*;
use crate::lexer::Comment;

pub struct Fmt {
    out: String,
    depth: usize,
    comments: Vec<Comment>,
    /// Which comments have been printed. A flag per comment rather than a
    /// cursor, because items are printed out of source order: a cursor
    /// advanced past one item's body swallowed the comments of every item
    /// before it.
    done: Vec<bool>,
    /// Each top-level item's first line mapped to its last -- the closing
    /// brace, or the `;` of a bodiless declaration. A comment between the two
    /// is in the item's body and stays there.
    ends: std::collections::HashMap<u32, u32>,
    /// The first line of the item being printed. Body comments are only
    /// taken from its own range, never from an item printed later.
    lo: u32,
    /// Comments claimed by a top-level item, keyed by the item's source
    /// line. Assignment happens in SOURCE order before anything is printed,
    /// because the printer reorders items -- a comment written above a free
    /// function must travel with it, not stay where the line numbers put it.
    owned: std::collections::BTreeMap<u32, Vec<Comment>>,
    /// The file's header: the comment lines at the very top, when a blank
    /// line separates them from what follows. They describe the file rather
    /// than the first declaration, so they stay first when the printer
    /// reorders -- without this, a header rode along with whichever item
    /// happened to be declared first and ended up in the middle of the file.
    header: Vec<Comment>,
}

const INDENT: &str = "    ";

pub fn format(
    p: &Program,
    comments: Vec<Comment>,
    ends: std::collections::HashMap<u32, u32>,
) -> String {
    let n = comments.len();
    let mut f = Fmt {
        out: String::new(),
        depth: 0,
        comments,
        done: vec![false; n],
        ends,
        lo: 0,
        owned: std::collections::BTreeMap::new(),
        header: Vec::new(),
    };
    f.claim_item_comments(p);
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

    /// Assign each comment to the top-level item it was written above.
    ///
    /// Done in source order, before printing, because the printer reorders
    /// items: a comment above a free function must travel with that function
    /// rather than stay where its line number happens to fall.
    fn claim_item_comments(&mut self, p: &Program) {
        // The header is a run of own-line comments on consecutive lines,
        // starting before anything else in the file, and followed by a gap.
        // A comment block touching the declaration below it documents that
        // declaration instead, and stays with it.
        let first_item = p
            .imports
            .iter()
            .map(|i| i.span.line)
            .chain(p.types.iter().map(|t| t.span.line))
            .chain(p.funcs.iter().map(|f| f.span.line))
            .chain(p.toplevel.iter().map(stmt_line))
            .min()
            .unwrap_or(u32::MAX);
        let mut end = 0usize;
        while end < self.comments.len()
            && self.comments[end].own_line
            && self.comments[end].line < first_item
            && (end == 0 || self.comments[end].line == self.comments[end - 1].line + 1)
        {
            end += 1;
        }
        let gap_after = end > 0 && {
            let last = self.comments[end - 1].line;
            let next = self
                .comments
                .get(end)
                .map_or(first_item, |c| c.line.min(first_item));
            next > last + 1
        };
        if gap_after {
            self.header = self.comments[..end].to_vec();
            for d in &mut self.done[..end] {
                *d = true;
            }
        }

        // Every place a comment can attach, in source order: items, which
        // are reordered and so take their comments with them, and top-level
        // statements, which are not. A comment belongs to an item only when
        // the item is the very next anchor below it and the comment is not
        // inside some earlier item's body.
        let mut anchors: Vec<(u32, bool)> = Vec::new();
        for t in &p.types {
            anchors.push((t.span.line, true));
        }
        for f in &p.funcs {
            anchors.push((f.span.line, true));
        }
        for st in &p.toplevel {
            anchors.push((stmt_line(st), false));
        }
        anchors.sort_unstable();

        for i in 0..self.comments.len() {
            if self.done[i] || !self.comments[i].own_line {
                continue;
            }
            let line = self.comments[i].line;
            let inside = self
                .ends
                .iter()
                .any(|(&from, &to)| from <= line && line <= to);
            if inside {
                continue;
            }
            if let Some(&(at, is_item)) = anchors.iter().find(|(a, _)| *a > line) {
                if is_item {
                    self.owned
                        .entry(at)
                        .or_default()
                        .push(self.comments[i].clone());
                    self.done[i] = true;
                }
            }
        }
    }

    /// Emit the comments claimed by the item declared at `line`.
    fn item_comments(&mut self, line: u32) {
        if let Some(cs) = self.owned.remove(&line) {
            for c in cs {
                for part in c.text.lines() {
                    self.line(part.trim());
                }
            }
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
            self.done[i] = true;
            let c = c.clone();
            if c.own_line {
                for part in c.text.lines() {
                    self.line(part.trim());
                }
            } else {
                // Trailing: put it back on the line it followed.
                let t = self.out.trim_end().to_string();
                self.out = format!("{t}  {}\n", c.text.trim());
            }
        }
    }

    /// Before an item's closing brace: a comment after its last statement
    /// is still inside it.
    fn end_item(&mut self, line: u32) {
        if let Some(&end) = self.ends.get(&line) {
            self.comments_before(end);
        }
        self.lo = 0;
    }

    fn trailing(&mut self) {
        self.lo = 0;
        for i in 0..self.comments.len() {
            if self.done[i] {
                continue;
            }
            self.done[i] = true;
            let c = self.comments[i].clone();
            for part in c.text.lines() {
                self.line(part.trim());
            }
        }
    }

    // ---- items --------------------------------------------------------

    fn program(&mut self, p: &Program) {
        if !self.header.is_empty() {
            for c in std::mem::take(&mut self.header) {
                self.line(c.text.trim());
            }
            self.blank();
        }
        // Imports first and in source order, which is where the parser
        // demands them: a reader learns a file's dependencies without
        // reading the file.
        if !p.imports.is_empty() {
            for i in &p.imports {
                self.item_comments(i.span.line);
                self.line(&format!("import {};", i.name));
            }
            self.blank();
        }
        // `prim` declarations come right after the imports and stay
        // together. A run of them is a list -- the module's whole seam to C,
        // readable in one glance -- and spacing them apart like definitions
        // would hide that they are one thing. The same reason `import` lines
        // are not separated.
        let mut any_prim = false;
        for f in p.funcs.iter().filter(|f| f.recv.is_none() && f.is_prim) {
            self.item_comments(f.span.line);
            self.func(f);
            any_prim = true;
        }
        if any_prim {
            self.blank();
        }

        // Types first, each followed by its own methods. Declarations are
        // order-independent, so this cannot change what the program means.
        for t in &p.types {
            self.item_comments(t.span.line);
            self.type_decl(t);
            for f in p
                .funcs
                .iter()
                .filter(|f| f.recv.as_deref() == Some(&t.name))
            {
                self.blank();
                self.item_comments(f.span.line);
                self.func(f);
            }
            self.blank();
        }

        for f in p.funcs.iter().filter(|f| f.recv.is_none() && !f.is_prim) {
            self.item_comments(f.span.line);
            self.func(f);
            self.blank();
        }

        // A method on a builtin or on a type that is not declared here would
        // otherwise be dropped. Nothing should hit this, but silently losing
        // a declaration is the one thing a formatter must never do.
        let declared: Vec<&str> = p.types.iter().map(|t| t.name.as_str()).collect();
        for f in p
            .funcs
            .iter()
            .filter(|f| f.recv.as_deref().is_some_and(|r| !declared.contains(&r)))
        {
            self.item_comments(f.span.line);
            self.func(f);
            self.blank();
        }

        for s in &p.toplevel {
            self.comments_before(stmt_line(s));
            self.stmt(s);
        }
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
        self.lo = t.span.line;
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
        self.end_item(t.span.line);
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
        self.lo = f.span.line;
        self.depth += 1;
        self.block(&f.body);
        self.end_item(f.span.line);
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

    fn block(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            self.comments_before(stmt_line(s));
            self.stmt(s);
        }
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
            Stmt::While { cond, body, .. } => {
                self.line(&format!("while ({}) {{", self.expr(cond)));
                self.depth += 1;
                self.block(body);
                self.depth -= 1;
                self.line("}");
            }
            Stmt::ForIn {
                ty,
                name,
                iter,
                body,
                ..
            } => {
                self.line(&format!(
                    "for ({} {name} in {}) {{",
                    self.ty(*ty),
                    self.expr(iter)
                ));
                self.depth += 1;
                self.block(body);
                self.depth -= 1;
                self.line("}");
            }
            Stmt::Match {
                scrutinee, arms, ..
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
                    self.block(&a.body);
                    self.depth -= 1;
                    self.line("}");
                }
                self.depth -= 1;
                self.line("}");
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                self.line(&format!("if ({}) {{", self.expr(cond)));
                self.depth += 1;
                self.block(then);
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
                        self.block(e);
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
            Expr::Int(n, _) => n.to_string(),
            Expr::Float(x, _) => fmt_float(*x),
            Expr::Bool(b, _) => b.to_string(),
            Expr::Str(s, _) => format!("{s:?}"),
            Expr::Var(n, _) => n.clone(),
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
        BinOp::Add | BinOp::Sub => 5,
        BinOp::Mul | BinOp::Div | BinOp::Rem => 6,
    }
}

/// Binding power of a unary operator -- the parser's `UNARY_BP`.
const UNARY: u8 = 7;
/// What a postfix operand (`.f`, `[i]`, `.m()`) binds with: tighter than
/// anything, so any operator expression under it needs parentheses.
const POSTFIX: u8 = 8;

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

fn stmt_line(s: &Stmt) -> u32 {
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
        | Stmt::If { span, .. } => span.line,
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
    format!("{x:?}")
}
