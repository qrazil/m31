//! The command line of `m31c` itself: Oro's grammar (docs/cli-conventions.md),
//! every command and option, the refusals, the exit codes, and the pre-0.6
//! spellings that still work for one release. These run the built binary, so
//! they see exactly what a user sees (stdout, stderr, status).

use std::path::PathBuf;
use std::process::{Command, Output};

fn m31c(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_m31c"))
        .args(args)
        .output()
        .expect("run m31c")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().expect("exit status")
}

/// A scratch directory under the build's own, unique per test.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const HELLO: &str = "print(\"hi\");\n";

fn hello(dir: &str) -> String {
    let d = scratch(dir);
    let src = d.join("hello.m31");
    std::fs::write(&src, HELLO).unwrap();
    src.to_str().unwrap().to_string()
}

#[test]
fn help_is_generated_and_is_only_spelled_one_way() {
    let o = m31c(&["--help"]);
    assert_eq!(code(&o), 0);
    let text = out(&o);
    assert!(text.starts_with("usage: m31c -<command> [...]\n\nthe m31 compiler\n\ncommands:\n"));
    for want in [
        "-emit-c  ",
        "-emit-ir  ",
        "-fmt  ",
        "-lint  ",
        "-init  ",
        "--help  ",
        "--version  ",
        "--completions SHELL  print a shell completion script (bash, zsh or fish) and exit",
        "Commands share a line and run in the order typed",
    ] {
        assert!(
            text.lines().any(|l| l.trim_start().starts_with(want)),
            "help lacks {want:?}:\n{text}"
        );
    }
    assert_eq!(err(&o), "");
    // `-h`, `-help`, `help` and `--h` are each refused with a pointer to --help
    for (arg, message) in [
        ("-h", "'-h' would be a command here, and there is none called 'h' -- help is an option, so write '--help'"),
        ("-help", "'-help' would be a command here, and there is none called 'help' -- help is an option, so write '--help'"),
    ] {
        let o = m31c(&[arg]);
        assert_eq!(code(&o), 2);
        assert!(err(&o).starts_with(&format!("m31c: {message}\n")), "{}", err(&o));
    }
    let o = m31c(&["-fmt", "--help"]);
    assert_eq!(code(&o), 0);
    assert!(out(&o).starts_with("usage: m31c -fmt [options] <path> [<more>...]\n"));
    assert!(out(&o).contains("--c, --check"));
    assert!(out(&o).contains("--r, --recursive"));
}

#[test]
fn version_is_an_option() {
    let o = m31c(&["--version"]);
    assert_eq!(code(&o), 0);
    assert_eq!(out(&o), format!("m31c {}\n", env!("CARGO_PKG_VERSION")));
    // like --help it wins before validation
    let o = m31c(&["-emit-c", "--version"]);
    assert_eq!(code(&o), 0);
    let o = m31c(&["-V"]);
    assert_eq!(code(&o), 2);
}

#[test]
fn usage_errors_exit_2_on_stderr_with_the_usage_line() {
    let o = m31c(&[]);
    assert_eq!(code(&o), 2);
    assert_eq!(out(&o), "");
    assert_eq!(
        err(&o),
        "m31c: m31c needs a command -- one of -emit-c, -emit-ir, -fmt, -lint, -init\n\
         usage: m31c -<command> [...]\n\
         try 'm31c --help' for more information\n"
    );
    let cases: [(&[&str], &str); 8] = [
        (
            &["-emit-cc", "x"],
            "unknown command '-emit-cc' -- did you mean '-emit-c'?",
        ),
        (
            &["-emitc"],
            "unknown command '-emitc' -- did you mean '-emit-c'?",
        ),
        (&["--"], "'--' has no meaning here"),
        (&["-emit-c"], "missing <source>"),
        (
            &["-emit-c", "a.m31", "b.m31"],
            "unexpected argument 'b.m31'",
        ),
        (
            &["-emit-c", "a.m31", "--out"],
            "'--out' needs a value and is the last argument",
        ),
        (
            &["-emit-c", "a.m31", "--ou", "x"],
            "unknown option '--ou' -- did you mean '--out'?",
        ),
        (
            &["emit-c", "a.m31"],
            "'emit-c' is a command, so it is written with a dash: '-emit-c'",
        ),
    ];
    for (args, want) in cases {
        let o = m31c(args);
        assert_eq!(code(&o), 2, "{args:?}");
        assert!(err(&o).contains(want), "{args:?}: {}", err(&o));
    }
    let o = m31c(&["-fmt", "x", "--qc"]);
    assert_eq!(code(&o), 2);
    assert!(
        err(&o).contains("short options do not cluster -- write '--q --c'")
            || err(&o).contains("unknown option '--qc'")
    );
    let o = m31c(&["-fmt", "x", "--check=yes"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("'--check' is a flag and takes no value -- you wrote '--check=yes'"));
    let o = m31c(&["-fmt", "x", "--check", "--check"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("'--check' was given more than once"));
}

#[test]
fn out_has_a_long_and_an_equals_and_a_short_form() {
    let src = hello("out");
    let dir = PathBuf::from(&src).parent().unwrap().to_path_buf();
    let mut texts = Vec::new();
    for (name, spelling) in [
        (
            "long.c",
            vec![
                "--out".to_string(),
                dir.join("long.c").to_str().unwrap().to_string(),
            ],
        ),
        (
            "equals.c",
            vec![format!("--out={}", dir.join("equals.c").display())],
        ),
        (
            "short.c",
            vec![
                "--o".to_string(),
                dir.join("short.c").to_str().unwrap().to_string(),
            ],
        ),
    ] {
        let mut args = vec!["-emit-c".to_string(), src.clone()];
        args.extend(spelling);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let o = m31c(&args);
        assert_eq!(code(&o), 0, "{name}: {}", err(&o));
        assert_eq!(err(&o), "", "no warning for the new spellings");
        texts.push(std::fs::read_to_string(dir.join(name)).unwrap());
    }
    assert!(texts.iter().all(|t| t == &texts[0] && !t.is_empty()));
    // without --out the C goes to standard output; options may come before the operand
    let o = m31c(&["-emit-c", &src]);
    assert_eq!(out(&o), texts[0]);
    let o = m31c(&["-emit-ir", &src]);
    assert_eq!(code(&o), 0);
    assert!(!out(&o).is_empty());
}

#[test]
fn runtime_failures_exit_1() {
    let o = m31c(&["-emit-c", "/definitely/not/here.m31"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).starts_with("cannot read /definitely/not/here.m31"));
}

#[test]
fn several_commands_run_in_order_and_the_line_is_validated_first() {
    let d = scratch("several");
    let good = d.join("good.m31");
    let bad = d.join("bad.m31");
    std::fs::write(&good, HELLO).unwrap();
    std::fs::write(&bad, "print(  \"hi\"  );\n").unwrap();
    let (g, b) = (good.to_str().unwrap(), bad.to_str().unwrap());
    // two commands, one line: the first passes, the second fails
    let o = m31c(&["-fmt", "--check", g, "-fmt", "--check", b]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("not formatted"));
    // a typo in the second command stops the first from running at all
    let o = m31c(&["-fmt", b, "-lnt", g]);
    assert_eq!(code(&o), 2);
    assert_eq!(
        std::fs::read_to_string(&bad).unwrap(),
        "print(  \"hi\"  );\n"
    );
    // and they really do run in order: format, then check
    let o = m31c(&["-fmt", b, "-fmt", "--check", b]);
    assert_eq!(code(&o), 0, "{}", err(&o));
}

#[test]
fn fmt_options() {
    let d = scratch("fmt");
    let good = d.join("good.m31");
    let bad = d.join("bad.m31");
    std::fs::write(&good, HELLO).unwrap();
    std::fs::write(&bad, "print(  \"hi\"  );\n").unwrap();
    let (g, b, dir) = (
        good.to_str().unwrap(),
        bad.to_str().unwrap(),
        d.to_str().unwrap(),
    );
    for flag in ["--check", "--c"] {
        assert_eq!(code(&m31c(&["-fmt", flag, g])), 0, "{flag}");
        let o = m31c(&["-fmt", flag, b]);
        assert_eq!(code(&o), 1, "{flag}");
        assert!(err(&o).contains("not formatted"));
    }
    for flag in ["--recursive", "--r"] {
        assert_eq!(code(&m31c(&["-fmt", "--check", flag, dir])), 1, "{flag}");
    }
    assert_eq!(code(&m31c(&["-fmt", "--recursive", dir])), 0);
    assert_eq!(code(&m31c(&["-fmt", "--check", "--recursive", dir])), 0);
    let o = m31c(&["-fmt"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("missing <path>"));
}

#[test]
fn lint_options() {
    let d = scratch("lint");
    let src = d.join("sample.m31");
    std::fs::write(&src, "int idx = 3;\nprint(idx);\n").unwrap();
    let path = src.to_str().unwrap();
    let o = m31c(&["-lint", path]);
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("idx"));
    // the allow list is greedy: the paths come first
    for args in [
        vec!["-lint", path, "--allow", "idx"],
        vec!["-lint", path, "--allow", "other", "idx"],
        vec!["-lint", path, "--allow=idx"],
        vec!["-lint", path, "--allow", "other,idx"],
    ] {
        let o = m31c(&args);
        assert_eq!(code(&o), 0, "{args:?}: {}", out(&o));
    }
    let o = m31c(&["-lint", path, "--allow", "idx", "--allow", "x"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains(
        "'--allow' was given more than once -- it already takes every value up to the next dash"
    ));
    let o = m31c(&["-lint", path, "--allow"]);
    assert_eq!(code(&o), 2);
    let o = m31c(&["-lint", path, "--summary"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("most common"));
    let o = m31c(&["-lint"]);
    assert_eq!(code(&o), 2);
}

#[test]
fn init_options() {
    let d = scratch("init");
    let long = d.join("long");
    let short = d.join("short");
    let o = m31c(&[
        "-init",
        long.to_str().unwrap(),
        "--lib",
        "--name",
        "my-lib",
        "--prefix",
        "ML",
    ]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let o = m31c(&[
        "-init",
        short.to_str().unwrap(),
        "--l",
        "--n",
        "my-lib",
        "--p",
        "ML",
    ]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(
        std::fs::read_to_string(long.join("deps")).unwrap(),
        std::fs::read_to_string(short.join("deps")).unwrap()
    );
    let o = m31c(&["-init", long.to_str().unwrap(), "--lib", "--name", "my-lib"]);
    assert_eq!(code(&o), 1);
    let o = m31c(&["-init", "a", "b"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("unexpected argument 'b'"));
}

#[test]
fn completions_come_from_the_option_tables() {
    let o = m31c(&["--completions", "bash"]);
    assert_eq!(code(&o), 0);
    let script = out(&o);
    assert!(script.contains("complete -o filenames -F _m31c_complete m31c"));
    assert!(script.contains("-emit-c -emit-ir -fmt -lint -init --help --version"));
    assert!(script.contains("fmt) COMPREPLY=($(compgen -W \"--check --c --recursive --r --help"));
    assert_eq!(code(&m31c(&["--completions=fish"])), 0);
    let o = m31c(&["--completions", "zsh"]);
    assert!(out(&o).starts_with("#compdef m31c\n"));
    let o = m31c(&["--completions", "csh"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("'--completions' needs bash, zsh or fish, and got 'csh'"));
}

// --- the pre-0.6 spellings: they work for one release, and say so ---------------

#[test]
fn the_old_spellings_still_work_and_warn() {
    let src = hello("old");
    let dir = PathBuf::from(&src).parent().unwrap().to_path_buf();
    let target = dir.join("old.c");
    let o = m31c(&["--emit-c", &src, "-o", target.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let new = dir.join("new.c");
    assert_eq!(
        code(&m31c(&["-emit-c", &src, "--out", new.to_str().unwrap()])),
        0
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        std::fs::read_to_string(&new).unwrap()
    );
    let e = err(&o);
    assert!(
        e.contains("m31c: warning: '--emit-c' is deprecated, use '-emit-c'\n"),
        "{e}"
    );
    assert!(
        e.contains("m31c: warning: '-o' is deprecated, use '--out'\n"),
        "{e}"
    );
    // fmt / lint / init as the first word, and -r
    let d = scratch("old-fmt");
    std::fs::write(d.join("x.m31"), "print(  \"hi\"  );\n").unwrap();
    let o = m31c(&["fmt", "--check", "-r", d.to_str().unwrap()]);
    assert_eq!(code(&o), 1);
    let e = err(&o);
    assert!(e.contains("'fmt' is deprecated, use '-fmt'"), "{e}");
    assert!(e.contains("'-r' is deprecated, use '--recursive'"), "{e}");
    assert!(e.contains("not formatted"), "{e}");
    let o = m31c(&["lint", "/nonexistent"]);
    assert!(err(&o).contains("'lint' is deprecated, use '-lint'"));
    // a file that happens to be called fmt, elsewhere on the line, stays data
    let o = m31c(&["-emit-c", "fmt"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("cannot read fmt"));
    assert!(!err(&o).contains("deprecated"));
}
