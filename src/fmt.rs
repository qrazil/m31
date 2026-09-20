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
    /// Index of the next comment not yet emitted.
    next: usize,
    /// Comments claimed by a top-level item, keyed by the item's source
    /// line. Assignment happens in SOURCE order before anything is printed,
    /// because the printer reorders items -- a comment written above a free
    /// function must travel with it, not stay where the line numbers put it.
    owned: std::collections::BTreeMap<u32, Vec<Comment>>,
}

const INDENT: &str = "    ";

pub fn format(p: &Program, comments: Vec<Comment>) -> String {
    let mut f = Fmt {
        out: String::new(),
        depth: 0,
        comments,
        next: 0,
        owned: std::collections::BTreeMap::new(),
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
        let mut lines: Vec<u32> = Vec::new();
        for t in &p.types {
            lines.push(t.span.line);
        }
        for f in &p.funcs {
            lines.push(f.span.line);
        }
        lines.sort_unstable();

        let mut i = 0usize;
        for &l in &lines {
            let mut mine = Vec::new();
            while i < self.comments.len() && self.comments[i].line < l {
                // Only a comment on its own line belongs to the item below;
                // one trailing code stays with the line it was written on.
                if self.comments[i].own_line {
                    mine.push(self.comments[i].clone());
                }
                i += 1;
            }
            if !mine.is_empty() {
                self.owned.entry(l).or_default().extend(mine);
            }
        }
        self.next = i;
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

    /// Emit every not-yet-claimed comment that appeared before `line`.
    /// Used inside bodies, where nothing is reordered.
    fn comments_before(&mut self, line: u32) {
        while self.next < self.comments.len() && self.comments[self.next].line < line {
            let c = self.comments[self.next].clone();
            self.next += 1;
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

    fn trailing(&mut self) {
        while self.next < self.comments.len() {
            let c = self.comments[self.next].clone();
            self.next += 1;
            for part in c.text.lines() {
                self.line(part.trim());
            }
        }
    }

    // ---- items --------------------------------------------------------

    fn program(&mut self, p: &Program) {
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

        for f in p.funcs.iter().filter(|f| f.recv.is_none()) {
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
        if let Some(b) = t.distinct_base {
            self.line(&format!("distinct {} {};", self.ty(b), t.name));
            return;
        }
        let kw = if t.is_interface { "interface" } else { "type" };
        let tp = if t.tparams.is_empty() {
            String::new()
        } else {
            format!("<{}>", t.tparams.join(", "))
        };
        self.line(&format!("{kw} {}{tp} {{", t.name));
        self.depth += 1;
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
            Some(r) => format!("{r}.{}", f.name),
            None => f.name.clone(),
        };
        self.line(&format!(
            "{} {name}{tp}({}) {{",
            self.ty(f.ret),
            self.params(&f.params)
        ));
        self.depth += 1;
        self.block(&f.body);
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
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
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
            Expr::Int(n, _) => n.to_string(),
            Expr::Bool(b, _) => b.to_string(),
            Expr::Str(s, _) => format!("{s:?}"),
            Expr::Var(n, _) => n.clone(),
            Expr::Bin(op, l, r, _) => {
                format!(
                    "{} {} {}",
                    self.operand(l, true),
                    op.spelling(),
                    self.operand(r, false)
                )
            }
            Expr::Un(op, x, _) => {
                let o = match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "!",
                };
                format!("{o}{}", self.operand(x, false))
            }
            Expr::Call(n, a, _) => format!("{n}({})", self.args(a)),
            Expr::MethodCall(o, m, a, _) => {
                format!("{}.{m}({})", self.operand(o, false), self.args(a))
            }
            Expr::Field(o, f, _) => format!("{}.{f}", self.operand(o, false)),
            Expr::Index(o, i, _) => format!("{}[{}]", self.operand(o, false), self.expr(i)),
            Expr::New(t, a, _) => format!("{}({})", self.ty(*t), self.args(a)),
        }
    }

    /// Parenthesise a subexpression when dropping the parentheses would
    /// change how it reparses. Conservative: any nested binary or unary
    /// expression keeps its parentheses rather than reasoning about
    /// precedence, because a formatter that changes meaning is worse than one
    /// that is slightly verbose.
    fn operand(&self, e: &Expr, _left: bool) -> String {
        match e {
            Expr::Bin(..) | Expr::Un(..) => format!("({})", self.expr(e)),
            _ => self.expr(e),
        }
    }
}

thread_local! {
    /// Source spellings for `Ty::User`, set by the driver before formatting.
    static TYPE_NAMES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Record how each interned type was spelled, so the formatter can print it.
///
/// Rendered recursively from the arena rather than by reading the table being
/// built -- the first version did the latter and printed `Pair<T0, T1>`,
/// because every argument was looked up in a table that was still empty.
pub fn set_type_names(p: &Program) {
    let rendered: Vec<String> = (0..p.ty_exprs.len())
        .map(|i| render_ty(&p.ty_exprs, &Ty::User(i as u32)))
        .collect();
    TYPE_NAMES.with(|n| *n.borrow_mut() = rendered);
}

fn render_ty(exprs: &[TyExpr], t: &Ty) -> String {
    match t {
        Ty::Int => "int".into(),
        Ty::Bool => "bool".into(),
        Ty::Str => "str".into(),
        Ty::Void => "void".into(),
        Ty::User(i) => match exprs.get(*i as usize) {
            None => format!("T{i}"),
            Some(e) if e.args.is_empty() => e.name.clone(),
            Some(e) => {
                let args: Vec<String> = e.args.iter().map(|a| render_ty(exprs, a)).collect();
                format!("{}<{}>", e.name, args.join(", "))
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
