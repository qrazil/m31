//! The command line of `m31c`, in the grammar every m31 program shares: Oro's
//! `args` (oro/docs/reference.md, "`args` -- written in Oro"; `lib/args.m31` is
//! the m31 port of it, and this file is its twin for the one program that is not
//! written in m31).
//!
//!   -word    a COMMAND          -fmt, -lint        (short: one dash, one letter)
//!   --word   an OPTION          --out PATH, --out=PATH   (short: --o)
//!   word     DATA               an operand of the option or command on its left
//!
//! Several commands may share a line and run in the order typed; options attach
//! to the command on their left; a list option is greedy; `--help` is the only
//! help; `-h`, `-help`, `help`, `--h`, a lone `--`, clustered shorts and
//! abbreviations are refused with a message that says what to write. A
//! mistyped name is *suggested* (a unique prefix, else the closest by edit
//! distance), never accepted. Std only.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// A switch.
    Flag,
    /// One value.
    Value,
    /// Every operand up to the next dash.
    Values,
    /// One whole number.
    Int,
}

pub struct Opt {
    pub long: &'static str,
    /// `--q` for `Some('q')`.
    pub short: Option<char>,
    pub kind: Kind,
    pub placeholder: &'static str,
    pub help: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Need {
    Required,
    Optional,
    Tail,
}

pub struct Operand {
    pub name: &'static str,
    pub need: Need,
    pub help: &'static str,
}

pub struct Command {
    pub word: &'static str,
    /// `-f` for `Some('f')`.
    pub short: Option<char>,
    pub spec: &'static Spec,
}

pub struct Spec {
    /// `m31c`, or `m31c -fmt` for a command.
    pub prog: &'static str,
    pub summary: &'static str,
    pub opts: &'static [Opt],
    pub operands: &'static [Operand],
    pub commands: &'static [Command],
    /// `Some(v)` makes `--version` an answer like `--help`.
    pub version: Option<&'static str>,
    /// Adds `--completions SHELL`, answered like `--help`.
    pub completions: bool,
    pub epilog: &'static str,
}

#[derive(Default, Debug)]
pub struct Parsed {
    given: HashSet<&'static str>,
    values: HashMap<&'static str, Vec<String>>,
    pub operands: Vec<String>,
}

impl Parsed {
    pub fn flag(&self, long: &str) -> bool {
        self.given.contains(long)
    }
    pub fn value(&self, long: &str) -> Option<&str> {
        self.values
            .get(long)
            .and_then(|v| v.first())
            .map(String::as_str)
    }
    pub fn values(&self, long: &str) -> &[String] {
        self.values.get(long).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// What a whole line answered.
#[derive(Debug)]
pub enum Line {
    /// `--help`: the help of the parser it was typed under.
    Help(String),
    /// `--version`.
    Version(String),
    /// `--completions SHELL`: the script.
    Script(String),
    /// Each command typed, in order, with what it was given. (The program's
    /// own clause is validated and, having no options of its own here, dropped.)
    Run(Vec<(String, Parsed)>),
}

fn find_opt<'a>(spec: &'a Spec, long: &str) -> Option<&'a Opt> {
    spec.opts.iter().find(|o| o.long == long)
}

fn find_short(spec: &Spec, letter: &str) -> Option<&'static str> {
    let mut chars = letter.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    spec.opts
        .iter()
        .find(|o| o.short == Some(c))
        .map(|o| o.long)
}

/// What a token is, by its first characters and nothing else.
#[derive(PartialEq)]
enum Token {
    Data,
    Bare,
    Option,
    Command,
}

fn token_kind(token: &str) -> Token {
    if !token.starts_with('-') || token == "-" {
        return Token::Data;
    }
    if token == "--" {
        return Token::Bare;
    }
    if token.starts_with("--") {
        return Token::Option;
    }
    match token.as_bytes().get(1) {
        Some(b) if b.is_ascii_digit() || *b == b'.' => Token::Data,
        _ => Token::Command,
    }
}

pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut grid = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in grid.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in grid[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (grid[i - 1][j] + 1)
                .min(grid[i][j - 1] + 1)
                .min(grid[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(grid[i - 2][j - 2] + 1);
            }
            grid[i][j] = best;
        }
    }
    grid[a.len()][b.len()]
}

fn flatten(name: &str) -> String {
    name.replace(['-', '_'], "")
}

/// The one candidate `given` was plainly meant to be: separators ignored and
/// one a prefix of the other, when exactly one matches; else the closest by
/// edit distance when close enough to be a typo.
pub fn nearest<'a>(given: &str, names: &[&'a str]) -> Option<&'a str> {
    let flat = flatten(given);
    let hits: Vec<&&str> = names
        .iter()
        .filter(|n| {
            let other = flatten(n);
            other == flat || other.starts_with(&flat) || flat.starts_with(&other)
        })
        .collect();
    if hits.len() == 1 {
        return Some(hits[0]);
    }
    if hits.len() > 1 {
        return None;
    }
    let size = given.chars().count();
    let limit = if size > 8 {
        3
    } else if size > 4 {
        2
    } else {
        1
    };
    let mut best: Option<(&str, usize)> = None;
    for name in names {
        let far = distance(given, name);
        if far <= limit && best.is_none_or(|(_, b)| far < b) {
            best = Some((name, far));
        }
    }
    best.map(|(n, _)| n)
}

fn command_list(spec: &Spec) -> String {
    spec.commands
        .iter()
        .map(|c| format!("-{}", c.word))
        .collect::<Vec<_>>()
        .join(", ")
}

fn unknown_option(spec: &Spec, spelled: &str) -> String {
    let mut names: Vec<&str> = vec!["help"];
    if spec.version.is_some() {
        names.push("version");
    }
    names.extend(spec.opts.iter().map(|o| o.long));
    match nearest(spelled.trim_start_matches('-'), &names) {
        Some(near) => format!("unknown option '{spelled}' -- did you mean '--{near}'?"),
        None => format!("unknown option '{spelled}'"),
    }
}

struct Clause {
    spec: &'static Spec,
    word: String,
    tokens: Vec<String>,
}

fn command_for(spec: &'static Spec, token: &str) -> Result<&'static Command, String> {
    let body = &token[1..];
    if (body == "h" || body == "help") && !spec.commands.iter().any(|c| c.word == body) {
        return Err(format!(
            "'{token}' would be a command here, and there is none called '{body}' -- help is an option, so write '--help'"
        ));
    }
    if spec.commands.is_empty() {
        return Err(format!(
            "{} takes no commands, and got '{token}'",
            spec.prog
        ));
    }
    if body.chars().count() == 1 {
        let c = body.chars().next().unwrap();
        return spec
            .commands
            .iter()
            .find(|cmd| cmd.short == Some(c))
            .ok_or_else(|| format!("unknown command '{token}' -- one of {}", command_list(spec)));
    }
    if let Some(cmd) = spec.commands.iter().find(|c| c.word == body) {
        return Ok(cmd);
    }
    let words: Vec<&str> = spec.commands.iter().map(|c| c.word).collect();
    Err(match nearest(body, &words) {
        Some(near) => format!("unknown command '{token}' -- did you mean '-{near}'?"),
        None => format!("unknown command '{token}' -- one of {}", command_list(spec)),
    })
}

fn usage_of(spec: &Spec) -> String {
    usage(spec)
}

/// Parse a whole line. The whole line is validated before anything is
/// returned, so a typo in the third command is found before the first runs.
pub fn parse(spec: &'static Spec, args: &[String]) -> Result<Line, String> {
    let mut clauses = vec![Clause {
        spec,
        word: String::new(),
        tokens: Vec::new(),
    }];
    for token in args {
        match token_kind(token) {
            Token::Bare => {
                return Err("'--' has no meaning here -- data is data because it does not start with a dash, so nothing has to be marked off".into());
            }
            Token::Command => {
                let cmd = command_for(spec, token)?;
                clauses.push(Clause {
                    spec: cmd.spec,
                    word: cmd.word.to_string(),
                    tokens: Vec::new(),
                });
            }
            _ => {
                if clauses.len() == 1
                    && !spec.commands.is_empty()
                    && spec.commands.iter().any(|c| c.word == token)
                {
                    return Err(format!(
                        "'{token}' is a command, so it is written with a dash: '-{token}'"
                    ));
                }
                clauses.last_mut().unwrap().tokens.push(token.clone());
            }
        }
    }
    for clause in &clauses {
        if clause.tokens.iter().any(|t| t == "--help") {
            return Ok(Line::Help(help(clause.spec)));
        }
    }
    if let Some(v) = spec.version {
        if clauses
            .iter()
            .any(|c| c.tokens.iter().any(|t| t == "--version"))
        {
            return Ok(Line::Version(format!("{} {v}", spec.prog)));
        }
    }
    if spec.completions {
        let t = &clauses[0].tokens;
        for (i, token) in t.iter().enumerate() {
            let shell = if token == "--completions" {
                match t.get(i + 1) {
                    Some(s) => s.clone(),
                    None => {
                        return Err("'--completions' needs a value and is the last argument".into())
                    }
                }
            } else if let Some(s) = token.strip_prefix("--completions=") {
                s.to_string()
            } else {
                continue;
            };
            return match completion(&shell, spec) {
                Ok(script) => Ok(Line::Script(script)),
                Err(_) => Err(format!(
                    "'--completions' needs bash, zsh or fish, and got '{shell}'"
                )),
            };
        }
    }
    parse_clause(clauses[0].spec, &clauses[0].tokens)?;
    let mut commands = Vec::new();
    for clause in &clauses[1..] {
        commands.push((
            clause.word.clone(),
            parse_clause(clause.spec, &clause.tokens)?,
        ));
    }
    if !spec.commands.is_empty() && commands.is_empty() {
        return Err(format!(
            "{} needs a command -- one of {}",
            spec.prog,
            command_list(spec)
        ));
    }
    Ok(Line::Run(commands))
}

fn parse_clause(spec: &'static Spec, tokens: &[String]) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut operands: Vec<String> = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        if token_kind(token) != Token::Option {
            operands.push(token.clone());
            continue;
        }
        let mut name = token[2..].to_string();
        let mut inline: Option<String> = None;
        if let Some((n, v)) = name.clone().split_once('=') {
            name = n.to_string();
            inline = Some(v.to_string());
        }
        let spelled = format!("--{name}");
        if name.is_empty() {
            return Err(format!(
                "'{token}' is not an option -- a long option needs a name after the dashes"
            ));
        }
        let long: &'static str;
        if name.chars().count() == 1 {
            match find_short(spec, &name) {
                Some(l) => long = l,
                None => {
                    if name == "h" {
                        return Err(format!(
                            "'{token}' is not an option -- help has one spelling, and it is '--help'"
                        ));
                    }
                    return Err(format!("unknown option '{token}'"));
                }
            }
        } else if let Some(o) = find_opt(spec, &name) {
            long = o.long;
        } else {
            let letters: Vec<String> = name.chars().map(|c| c.to_string()).collect();
            if letters.len() > 1 && letters.iter().all(|l| find_short(spec, l).is_some()) {
                let spread: Vec<String> = letters.iter().map(|l| format!("--{l}")).collect();
                return Err(format!(
                    "'{token}': short options do not cluster -- write '{}'",
                    spread.join(" ")
                ));
            }
            return Err(unknown_option(spec, &spelled));
        }
        let opt = find_opt(spec, long).unwrap();
        let again = parsed.given.contains(long);
        match opt.kind {
            Kind::Flag => {
                if inline.is_some() {
                    return Err(format!(
                        "'{spelled}' is a flag and takes no value -- you wrote '{token}'"
                    ));
                }
                if again {
                    return Err(format!("'{spelled}' was given more than once"));
                }
                parsed.given.insert(long);
            }
            Kind::Values => {
                if again {
                    return Err(format!(
                        "'{spelled}' was given more than once -- it already takes every value up to the next dash, so write them together: '{spelled} a b c'"
                    ));
                }
                let mut collected: Vec<String> = inline.into_iter().collect();
                while index < tokens.len() && token_kind(&tokens[index]) == Token::Data {
                    collected.push(tokens[index].clone());
                    index += 1;
                }
                if collected.is_empty() {
                    return Err(format!(
                        "'{spelled}' needs at least one value -- leaving it out is how an empty list is spelled"
                    ));
                }
                parsed.values.insert(long, collected);
                parsed.given.insert(long);
            }
            Kind::Value | Kind::Int => {
                let raw = match inline {
                    Some(v) => v,
                    None => {
                        let Some(next) = tokens.get(index) else {
                            return Err(format!(
                                "'{spelled}' needs a value and is the last argument"
                            ));
                        };
                        if token_kind(next) != Token::Data {
                            return Err(format!(
                                "'{spelled}' needs a value, and '{next}' starts with a dash -- write '{spelled}={next}' if that is the value you meant"
                            ));
                        }
                        index += 1;
                        next.clone()
                    }
                };
                if again {
                    return Err(format!(
                        "'{spelled}' was given more than once and takes a single value"
                    ));
                }
                if opt.kind == Kind::Int && raw.parse::<i64>().is_err() {
                    return Err(format!("'{spelled}' needs a whole number, and got '{raw}'"));
                }
                parsed.values.insert(long, vec![raw]);
                parsed.given.insert(long);
            }
        }
    }
    // hand the operands to the declared ones
    let mut next = 0;
    for operand in spec.operands {
        if operand.need == Need::Tail {
            parsed
                .operands
                .extend(operands[next.min(operands.len())..].iter().cloned());
            next = operands.len();
            continue;
        }
        if next < operands.len() {
            parsed.operands.push(operands[next].clone());
            next += 1;
            continue;
        }
        if operand.need == Need::Required {
            return Err(format!("missing <{}> -- {}", operand.name, usage_of(spec)));
        }
    }
    if next < operands.len() {
        let extra = &operands[next];
        if spec.operands.is_empty() {
            return Err(format!(
                "{} takes no arguments of its own, and got '{extra}'",
                spec.prog
            ));
        }
        return Err(format!(
            "unexpected argument '{extra}' -- {}",
            usage_of(spec)
        ));
    }
    Ok(parsed)
}

pub fn usage(spec: &Spec) -> String {
    let mut parts = vec![format!("usage: {}", spec.prog)];
    if !spec.opts.is_empty() {
        parts.push("[options]".into());
    }
    for o in spec.operands {
        parts.push(match o.need {
            Need::Required => format!("<{}>", o.name),
            Need::Optional => format!("[<{}>]", o.name),
            Need::Tail => format!("[<{}>...]", o.name),
        });
    }
    if !spec.commands.is_empty() {
        parts.push("-<command>".into());
        parts.push("[...]".into());
    }
    parts.join(" ")
}

fn opt_column(o: &Opt) -> String {
    let left = match o.short {
        Some(c) => format!("--{c}, "),
        None => "     ".to_string(),
    };
    match o.kind {
        Kind::Flag => format!("{left}--{}", o.long),
        Kind::Values => format!("{left}--{} {}...", o.long, o.placeholder),
        _ => format!("{left}--{} {}", o.long, o.placeholder),
    }
}

pub fn help(spec: &Spec) -> String {
    let mut lefts: Vec<(String, String)> = Vec::new();
    for c in spec.commands {
        let left = match c.short {
            Some(s) => format!("-{s}, -{}", c.word),
            None => format!("    -{}", c.word),
        };
        lefts.push((left, c.spec.summary.to_string()));
    }
    let command_rows = lefts.len();
    let mut operand_rows: Vec<(String, String)> = Vec::new();
    for o in spec.operands {
        let left = match o.need {
            Need::Required => format!("<{}>", o.name),
            Need::Optional => format!("[<{}>]", o.name),
            Need::Tail => format!("[<{}>...]", o.name),
        };
        operand_rows.push((left, o.help.to_string()));
    }
    let mut option_rows: Vec<(String, String)> =
        vec![("     --help".into(), "show this help".into())];
    if spec.version.is_some() {
        option_rows.push(("     --version".into(), "print the version and exit".into()));
    }
    for o in spec.opts {
        option_rows.push((opt_column(o), o.help.to_string()));
    }
    if spec.completions {
        option_rows.push((
            "     --completions SHELL".into(),
            "print a shell completion script (bash, zsh or fish) and exit".into(),
        ));
    }
    let width = lefts
        .iter()
        .chain(&operand_rows)
        .chain(&option_rows)
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    let row = |l: &str, t: &str| -> String {
        if t.is_empty() {
            format!("  {l}")
        } else {
            format!("  {l:<width$}  {t}")
        }
    };
    let mut out = vec![usage(spec)];
    if !spec.summary.is_empty() {
        out.push(String::new());
        out.push(spec.summary.to_string());
    }
    if command_rows > 0 {
        out.push(String::new());
        out.push("commands:".into());
        out.extend(lefts.iter().map(|(l, t)| row(l, t)));
    }
    if !operand_rows.is_empty() {
        out.push(String::new());
        out.push("arguments:".into());
        out.extend(operand_rows.iter().map(|(l, t)| row(l, t)));
    }
    out.push(String::new());
    out.push("options:".into());
    out.extend(option_rows.iter().map(|(l, t)| row(l, t)));
    if !spec.epilog.is_empty() {
        out.push(String::new());
        out.push(spec.epilog.to_string());
    }
    out.join("\n")
}

/// A usage error: `prog: message`, the usage line, a pointer to `--help`, on
/// stderr, exit status 2.
pub fn fail(spec: &Spec, message: &str) -> std::process::ExitCode {
    eprintln!("{}: {message}", spec.prog);
    eprintln!("{}", usage(spec));
    eprintln!("try '{} --help' for more information", spec.prog);
    std::process::ExitCode::from(2)
}

/// Old spellings of a released program, accepted for one release: each token
/// that is `old` (or, for `--old`, `--old=value`) becomes `new`, with one line
/// on standard error. A bare first word (`fmt`) is rewritten only in first
/// position, so a file called `fmt` elsewhere stays data.
pub fn migrate(
    prog: &str,
    args: &[String],
    pairs: &[(&str, &str)],
    first_words: &[&str],
) -> Vec<String> {
    let mut out = Vec::new();
    for (i, token) in args.iter().enumerate() {
        let mut done = false;
        if i == 0 && first_words.contains(&token.as_str()) {
            eprintln!("{prog}: warning: '{token}' is deprecated, use '-{token}'");
            out.push(format!("-{token}"));
            continue;
        }
        for (old, new) in pairs {
            if token == old {
                eprintln!("{prog}: warning: '{old}' is deprecated, use '{new}'");
                out.push((*new).to_string());
                done = true;
                break;
            }
            if old.starts_with("--") && token.starts_with(&format!("{old}=")) {
                eprintln!("{prog}: warning: '{old}' is deprecated, use '{new}'");
                out.push(format!("{new}{}", &token[old.len()..]));
                done = true;
                break;
            }
        }
        if !done {
            out.push(token.clone());
        }
    }
    out
}

fn option_words(spec: &Spec, root: Option<&Spec>) -> Vec<String> {
    let mut words = Vec::new();
    for c in spec.commands {
        words.push(format!("-{}", c.word));
        if let Some(s) = c.short {
            words.push(format!("-{s}"));
        }
    }
    for o in spec.opts {
        words.push(format!("--{}", o.long));
        if let Some(s) = o.short {
            words.push(format!("--{s}"));
        }
    }
    words.push("--help".into());
    if spec.version.is_some() && root.is_none() {
        words.push("--version".into());
    }
    words
}

fn is_path(o: &Opt) -> bool {
    o.placeholder.ends_with("PATH")
        || o.placeholder.ends_with("FILE")
        || o.placeholder.ends_with("DIR")
}

/// A shell completion script for the spec and its commands, from the same
/// tables that parse. `shell` is `bash`, `zsh` or `fish`.
pub fn completion(shell: &str, root: &'static Spec) -> Result<String, String> {
    let prog = root.prog;
    let ident = prog.replace('-', "_");
    let mut out: Vec<String> = Vec::new();
    match shell {
        "bash" => {
            out.push(format!(
                "# bash completion for {prog} (generated by `{prog} --completions bash`)"
            ));
            out.push(format!("_{ident}_complete() {{"));
            out.push("    local cur word command".into());
            out.push("    cur=\"${COMP_WORDS[COMP_CWORD]}\"".into());
            out.push("    command=\"\"".into());
            out.push("    for word in \"${COMP_WORDS[@]:1:COMP_CWORD-1}\"; do".into());
            out.push("        case \"$word\" in".into());
            for c in root.commands {
                let mut alt = format!("-{}", c.word);
                if let Some(s) = c.short {
                    alt.push_str(&format!("|-{s}"));
                }
                out.push(format!("            {alt}) command={} ;;", c.word));
            }
            out.push("        esac".into());
            out.push("    done".into());
            out.push("    if [[ \"$cur\" == -* ]]; then".into());
            out.push("        case \"$command\" in".into());
            for c in root.commands {
                let mut words = option_words(c.spec, Some(root));
                for w in option_words(root, None) {
                    if w.starts_with('-') && !w.starts_with("--") && !words.contains(&w) {
                        words.push(w);
                    }
                }
                out.push(format!(
                    "            {}) COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")) ;;",
                    c.word,
                    words.join(" ")
                ));
            }
            out.push(format!(
                "            *) COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")) ;;",
                option_words(root, None).join(" ")
            ));
            out.push("        esac".into());
            out.push("    else".into());
            out.push("        COMPREPLY=($(compgen -f -- \"$cur\"))".into());
            out.push("    fi".into());
            out.push("}".into());
            out.push(format!("complete -o filenames -F _{ident}_complete {prog}"));
        }
        "fish" => {
            out.push(format!(
                "# fish completion for {prog} (generated by `{prog} --completions fish`)"
            ));
            let q = |t: &str| t.replace('\\', "\\\\").replace('\'', "\\'");
            for c in root.commands {
                out.push(format!(
                    "complete -c {prog} -o {} -d '{}'",
                    c.word,
                    q(c.spec.summary)
                ));
                if let Some(s) = c.short {
                    out.push(format!(
                        "complete -c {prog} -o {s} -d '{}'",
                        q(c.spec.summary)
                    ));
                }
            }
            let emit = |spec: &Spec, cond: String| -> Vec<String> {
                let mut out: Vec<String> = Vec::new();
                for o in spec.opts {
                    let arg = match o.kind {
                        Kind::Flag => "",
                        _ if is_path(o) => " -r -F",
                        _ => " -x",
                    };
                    out.push(format!(
                        "complete -c {prog}{cond} -l {}{arg} -d '{}'",
                        o.long,
                        q(o.help)
                    ));
                    if let Some(s) = o.short {
                        let arg = if o.kind == Kind::Flag { "" } else { " -x" };
                        out.push(format!(
                            "complete -c {prog}{cond} -l {s}{arg} -d '{}'",
                            q(o.help)
                        ));
                    }
                }
                out
            };
            out.extend(emit(root, String::new()));
            out.push(format!("complete -c {prog} -l help -d 'show this help'"));
            if root.version.is_some() {
                out.push(format!(
                    "complete -c {prog} -l version -d 'print the version and exit'"
                ));
            }
            for c in root.commands {
                out.extend(emit(
                    c.spec,
                    format!(" -n '__fish_seen_argument -o {}'", c.word),
                ));
            }
        }
        "zsh" => {
            let esc = |t: &str| t.replace('\'', "'\\''").replace(':', "\\:");
            out.push(format!("#compdef {prog}"));
            out.push(format!(
                "# zsh completion for {prog} (generated by `{prog} --completions zsh`)"
            ));
            out.push(format!("_{ident}() {{"));
            out.push("    local -a choices".into());
            out.push("    if [[ $PREFIX == -* ]]; then".into());
            let mut items: Vec<String> = Vec::new();
            for c in root.commands {
                items.push(format!("'-{}:{}'", c.word, esc(c.spec.summary)));
            }
            let mut all = root.opts.iter().collect::<Vec<_>>();
            for c in root.commands {
                all.extend(c.spec.opts.iter());
            }
            for o in all {
                items.push(format!("'--{}:{}'", o.long, esc(o.help)));
                if let Some(s) = o.short {
                    items.push(format!("'--{s}:{}'", esc(o.help)));
                }
            }
            items.push("'--help:show this help'".into());
            if root.version.is_some() {
                items.push("'--version:print the version and exit'".into());
            }
            out.push(format!("        choices=({})", items.join(" ")));
            out.push("        _describe 'option' choices".into());
            out.push("    else".into());
            out.push("        _files".into());
            out.push("    fi".into());
            out.push("}".into());
            out.push(format!("compdef _{ident} {prog}"));
        }
        other => return Err(format!("unknown shell '{other}': one of bash, zsh, fish")),
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    static OPTS: [Opt; 4] = [
        Opt {
            long: "out",
            short: Some('o'),
            kind: Kind::Value,
            placeholder: "PATH",
            help: "where to write",
        },
        Opt {
            long: "quiet",
            short: Some('q'),
            kind: Kind::Flag,
            placeholder: "",
            help: "say nothing",
        },
        Opt {
            long: "count",
            short: Some('n'),
            kind: Kind::Int,
            placeholder: "N",
            help: "how many",
        },
        Opt {
            long: "tag",
            short: None,
            kind: Kind::Values,
            placeholder: "TAG",
            help: "tags",
        },
    ];
    static OPERANDS: [Operand; 2] = [
        Operand {
            name: "src",
            need: Need::Required,
            help: "the file",
        },
        Operand {
            name: "more",
            need: Need::Tail,
            help: "",
        },
    ];
    static SUB: Spec = Spec {
        prog: "tool -serve",
        summary: "run it",
        opts: &OPTS,
        operands: &OPERANDS,
        commands: &[],
        version: None,
        completions: false,
        epilog: "",
    };
    static NONE: [Operand; 0] = [];
    static INIT: Spec = Spec {
        prog: "tool -init",
        summary: "create files",
        opts: &[],
        operands: &NONE,
        commands: &[],
        version: None,
        completions: false,
        epilog: "",
    };
    static ROOT: Spec = Spec {
        prog: "tool",
        summary: "do a thing",
        opts: &[],
        operands: &NONE,
        commands: &[
            Command {
                word: "serve",
                short: Some('s'),
                spec: &SUB,
            },
            Command {
                word: "init",
                short: None,
                spec: &INIT,
            },
        ],
        version: Some("1.0"),
        completions: true,
        epilog: "",
    };

    fn run(args: &[&str]) -> Result<Line, String> {
        parse(
            &ROOT,
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }
    fn serve(args: &[&str]) -> Result<Parsed, String> {
        let mut line = vec!["-serve"];
        line.extend_from_slice(args);
        match run(&line)? {
            Line::Run(mut cmds) => Ok(cmds.remove(0).1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_accepted_forms() {
        let p = serve(&["--out", "a", "src"]).unwrap();
        assert_eq!(p.value("out"), Some("a"));
        assert_eq!(serve(&["--out=a", "src"]).unwrap().value("out"), Some("a"));
        assert_eq!(serve(&["--o", "a", "src"]).unwrap().value("out"), Some("a"));
        assert!(serve(&["--q", "src"]).unwrap().flag("quiet"));
        assert_eq!(
            serve(&["--n", "7", "src"]).unwrap().value("count"),
            Some("7")
        );
        let p = serve(&["src", "x", "--tag", "a", "b", "c"]).unwrap();
        assert_eq!(p.values("tag"), ["a", "b", "c"]);
        assert_eq!(p.operands, ["src", "x"]);
        assert_eq!(
            serve(&["-", "-5", "-1.5"]).unwrap().operands,
            ["-", "-5", "-1.5"]
        );
        assert_eq!(
            serve(&["--out=-x", "src"]).unwrap().value("out"),
            Some("-x")
        );
    }

    #[test]
    fn several_commands_on_one_line_run_in_order() {
        let Line::Run(cmds) = run(&["-s", "a", "-init", "-serve", "b"]).unwrap() else {
            panic!()
        };
        let words: Vec<&str> = cmds.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(words, ["serve", "init", "serve"]);
    }

    #[test]
    fn the_whole_line_is_validated_before_anything_runs() {
        assert!(run(&["-init", "-serve"]).is_err());
    }

    #[test]
    fn oros_refusals_exactly() {
        let msg = |a: &[&str]| run(a).unwrap_err();
        assert_eq!(
            serve(&["--qn", "src"]).unwrap_err(),
            "'--qn': short options do not cluster -- write '--q --n'"
        );
        assert_eq!(
            serve(&["--out"]).unwrap_err(),
            "'--out' needs a value and is the last argument"
        );
        assert_eq!(
            serve(&["--out", "--q", "src"]).unwrap_err(),
            "'--out' needs a value, and '--q' starts with a dash -- write '--out=--q' if that is the value you meant"
        );
        assert_eq!(
            serve(&["--count", "abc", "src"]).unwrap_err(),
            "'--count' needs a whole number, and got 'abc'"
        );
        assert_eq!(
            serve(&["--q=yes", "src"]).unwrap_err(),
            "'--q' is a flag and takes no value -- you wrote '--q=yes'"
        );
        assert_eq!(
            serve(&["--out", "a", "--out", "b", "src"]).unwrap_err(),
            "'--out' was given more than once and takes a single value"
        );
        assert_eq!(
            serve(&["--tag", "a", "--tag", "b", "src"]).unwrap_err(),
            "'--tag' was given more than once -- it already takes every value up to the next dash, so write them together: '--tag a b c'"
        );
        assert_eq!(
            serve(&["--tag"]).unwrap_err(),
            "'--tag' needs at least one value -- leaving it out is how an empty list is spelled"
        );
        assert!(serve(&[])
            .unwrap_err()
            .starts_with("missing <src> -- usage: tool -serve [options] <src>"));
        assert_eq!(msg(&["-h"]), "'-h' would be a command here, and there is none called 'h' -- help is an option, so write '--help'");
        assert_eq!(msg(&["-help"]), "'-help' would be a command here, and there is none called 'help' -- help is an option, so write '--help'");
        assert_eq!(msg(&["--"]), "'--' has no meaning here -- data is data because it does not start with a dash, so nothing has to be marked off");
        assert_eq!(
            msg(&["serve"]),
            "'serve' is a command, so it is written with a dash: '-serve'"
        );
        assert_eq!(
            msg(&["-srve"]),
            "unknown command '-srve' -- did you mean '-serve'?"
        );
        assert_eq!(
            msg(&["-zzzzzz"]),
            "unknown command '-zzzzzz' -- one of -serve, -init"
        );
        assert_eq!(msg(&[]), "tool needs a command -- one of -serve, -init");
        assert_eq!(
            serve(&["--h", "src"]).unwrap_err(),
            "'--h' is not an option -- help has one spelling, and it is '--help'"
        );
        assert_eq!(
            serve(&["--cuont", "3", "src"]).unwrap_err(),
            "unknown option '--cuont' -- did you mean '--count'?"
        );
        assert_eq!(
            serve(&["--prot", "src"]).unwrap_err(),
            "unknown option '--prot'"
        );
        assert_eq!(
            serve(&["--=x"]).unwrap_err(),
            "'--=x' is not an option -- a long option needs a name after the dashes"
        );
    }

    #[test]
    fn help_and_version_win_before_validation() {
        assert!(
            matches!(run(&["-serve", "--help"]), Ok(Line::Help(h)) if h.starts_with("usage: tool -serve [options] <src> [<more>...]"))
        );
        assert!(matches!(run(&["--version"]), Ok(Line::Version(v)) if v == "tool 1.0"));
        assert!(matches!(run(&["--bogus", "--help"]), Ok(Line::Help(_))));
    }

    #[test]
    fn the_help_is_generated() {
        let text = help(&ROOT);
        assert!(text.starts_with("usage: tool -<command> [...]\n\ndo a thing\n\ncommands:\n"));
        assert!(text
            .lines()
            .any(|l| l.starts_with("  -s, -serve") && l.ends_with("  run it")));
        assert!(text
            .lines()
            .any(|l| l.starts_with("      -init") && l.ends_with("  create files")));
        assert!(text.contains("     --help               show this help\n"));
        assert!(text.contains("     --version            print the version and exit\n"));
        assert!(text.contains("     --completions SHELL  print a shell completion script (bash, zsh or fish) and exit"));
        let sub = help(&SUB);
        assert!(sub.contains("  --o, --out PATH    where to write\n"));
        assert!(sub.contains("       --tag TAG...  tags"));
    }

    #[test]
    fn migrate_rewrites_old_spellings() {
        let args: Vec<String> = ["fmt", "--check", "-r", "x", "--output=y"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = migrate(
            "tool",
            &args,
            &[("-r", "--recursive"), ("--output", "--out")],
            &["fmt", "lint"],
        );
        assert_eq!(out, ["-fmt", "--check", "--recursive", "x", "--out=y"]);
    }

    #[test]
    fn completion_scripts() {
        let bash = completion("bash", &ROOT).unwrap();
        assert!(bash.contains("-serve|-s) command=serve ;;"));
        assert!(bash.contains("serve) COMPREPLY=($(compgen -W \"--out --o --quiet --q --count --n --tag --help -serve -s -init\""));
        assert!(completion("fish", &ROOT)
            .unwrap()
            .contains("complete -c tool -o serve -d 'run it'"));
        assert!(completion("zsh", &ROOT)
            .unwrap()
            .starts_with("#compdef tool\n"));
        assert!(completion("csh", &ROOT).is_err());
    }

    #[test]
    fn edit_distance() {
        assert_eq!(distance("prot", "port"), 1);
        assert_eq!(nearest("coun", &["count", "tag"]), Some("count"));
        assert_eq!(nearest("zzz", &["count", "tag"]), None);
    }
}
