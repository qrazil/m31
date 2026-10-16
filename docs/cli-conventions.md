# Command-line conventions

Every m31 program's command line is **Oro's `args` grammar, exactly**: the
specification is oro/docs/reference.md, "`args` -- written in Oro" (lines
2180-2443), pinned by oro's `corpus/divergence/110_args.oro`; `lib/args.m31` is its
port and the one parser every m31 program uses; `src/tools/cli.rs` is its twin for
`m31c`, the one program that is not written in m31. The audit that found what had
to change is `docs/CLI-INVENTORY.md`.

(History: the first pass of this audit applied a POSIX grammar -- `-n 5`,
`--number 5`, `-h`, `--`, `--no-x`, clustering. The owner then decided the
standard is Oro's, for every application including the ones that mimic OpenSSH
or git. That pass is gone; the POSIX spellings are refused, see below.)

## The grammar

One dash is a **command**, two dashes are an **option**, anything else is **data**.

    orogit -addrepo site --public -grant site alice write
           └ command ┘    └ opt ┘ └ command ┘└─ data ──┘

| rule | |
|---|---|
| command | `-serve`; its short form is one dash and one letter, `-s` |
| option | `--out PATH`, `--out=PATH`; its short form is **two dashes and one letter**, `--o PATH` |
| data | anything that does not start with a dash; a lone `-` (stdin/stdout) and `-5`, `-1.5` (a sign) are data too |
| several commands | one line may carry several; they run in the order typed, the same one may appear twice |
| attachment | options attach to the command on their left; before any command they are the program's |
| depth | two levels: the program, and its commands. A command has no commands |
| validation | the whole line is validated before anything runs: a typo in the third command means the first never ran |
| list option | greedy: `--tag a b c` takes every operand up to the next dash; written twice it is refused. A program that has both a list option and operands takes the operands **first** |
| value option | takes one value; the value is never something that starts with a dash (`--out=-x` carries one); given twice it is refused |
| flag | takes no value (`--quiet=yes` is refused); given twice it is refused; **there is no `--no-x`**: a switch whose default is on is declared as its opposite |
| positional | `positional(name)` must be given; `optional(name, default)` may be left out; `tail(name)` is zero or more. **There are no required options**: what a command cannot run without is a positional |
| `--help` | the only help, on every parser; `prog -serve --help` is `-serve`'s. It wins before any validation. Reserved: it cannot be declared |
| refused | `-h`, `-help`, `help`, `--h`; a lone `--`; a clustered `--abc` (write `--a --b --c`); an abbreviation (`--verb`: *suggested*, never accepted); `-x` for an option |
| hidden options | there are none: a flag that works but is not in `--help` is a trap |

The messages are Oro's, word for word (ASCII ` -- ` for its dash), because they are
the user interface of every program built on the parser: `unknown option '--coun' --
did you mean '--count'?`, `'--out' needs a value and is the last argument`,
`'--count' needs a whole number, and got 'abc'`, `missing <src> -- usage: ...`,
`'init' is a command, so it is written with a dash: '-init'`, and the rest of the
table in the reference.

Errors go to **stderr**, exit status **2**: `prog: message`, the usage line, and
`try 'prog --help' for more information`. A program that ran and failed exits 1;
success is 0.

## What the m31 port adds

All additive; no form and no message of the grammar changes.

| | |
|---|---|
| `p.version = "1.2.3"` | `--version` is answered like `--help` (anywhere, before validation): `prog 1.2.3`. `version` is a reserved option name. `-V` is not a thing |
| `p.completions()` | adds `--completions SHELL` (bash, zsh, fish), generated from the table; answered like `--help` |
| `p.integer(..., min:, max:)` | `'--port' needs a whole number from 1 to 65535, and got '70000'` |
| edit distance | a mistyped option or command names the closest one when no unique prefix matches (swapped neighbours count as one). Only ever a suggestion |
| `p.epilog` | text after the options table |
| `p.parse_or_exit(argv, status)` | parse, then do what every program does the same way: print `--help`/`--version`/`--completions` and exit 0, or print the usage error and exit `status`. Outside the core contract (which prints nothing and never exits) |
| `p.fail(message, status)` | the same for a mistake only the program can see (`--index needs --root`) |
| `args.migrate(prog, argv, pairs)` | old spellings of a released program, rewritten with a one-line warning, for one release |

`lib/args.m31`'s header lists the whole API. `corpus/modules/stdlib-args` pins the
grammar (it is Oro's `110_args` case by case); `corpus/modules/stdlib-args-extras`
pins the additions.

## Vocabulary

One name for one concept, across programs. Long names are lowercase words joined by
`-`; the short form (`--o`) is only for options used constantly.

| concept | option | short |
|---|---|---|
| output file | `--out PATH` | `--o` |
| configuration file | `--config FILE` | `--c` |
| port | `--port PORT` | `--p` |
| host | `--host HOST` | |
| user | `--user NAME` | `--u` |
| quiet / verbose | `--quiet`, `--verbose` | `--q` |
| colour | `--color auto always never` (value) | |
| theme | `--theme NAME` | |
| recursive | `--recursive` | `--r` |
| check only | `--check` | `--c` (in `-fmt`) |
| version | `--version` | none |
| completions | `--completions SHELL` | |

## Deprecated spellings

A released program changes its spelling the polite way: the old spelling keeps
working for **one minor release** and prints one line on stderr,
`prog: warning: '-p' is deprecated, use '--port'`; after that release the alias is
deleted. `args.migrate` does the rewrite for single tokens. A program whose old
grammar cannot be rewritten token by token (OpenSSH's `-p22`, `-tt`, a remote command
with dashes in it) reads such a line with its old parser, behind one warning, for the
same single release. An alias that is simply a bug is not kept. `m31c` keeps
`--emit-c`, `--emit-ir`, `fmt`, `lint`, `init`, `-o`, `-r` this way.

## Exemptions

There are none. The programs that used to mimic OpenSSH (`m31ssh`, `m31scp`,
`m31sftp`, `m31ssh-keygen`) and git (`1git`, `mgit`) are on this grammar, on the
owner's decision: a drop-in clone gives nobody a reason to use ours. (1git's git
command surface is being redesigned by its own migration; until it lands, git's own
commands keep git's argument grammar.) Developer scripts that are not
user-facing (`gates.sh`, `scripts/*.sh`, `build.sh X -o OUT`) are not covered.

Consequences worth knowing: a word that starts with a dash is never data, so the
remote command of `m31ssh` is one operand (`m31ssh host "ls -l"`), and a file called
`-x` is passed as `./-x`.

## For program authors

    import args;
    import os;

    args.Parser p = args.Parser("tool", summary: "do a thing");
    p.version = "1.2.0";
    p.integer("port", short: "p", default: "8080", min: "1", max: "65535", help: "port to listen on");
    p.value("out", short: "o", placeholder: "PATH", help: "where to write");
    p.positional("input", placeholder: "FILE", help: "what to read");
    p.completions();
    args.Parsed given = p.parse_or_exit(os.arguments().slice(1, os.arguments().size()));
    int port = given.integer("port").or(8080);

A mistake in the program's own declarations (a repeated name, `--help`, a default
the option would refuse) traps at the line that wrote it.
