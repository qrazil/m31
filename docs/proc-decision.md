# Child processes: the decision

Decided **2026-10-12**, implemented in the same change: `lib/proc.m31`, four
sys-layer functions (`docs/sys-layer.md`, "Child stdio, pipes, signals and
links"), tests `corpus/modules/proc-*`, `tests/proc/test_proc.sh`.

---

## The problem

`os.run(argv)` starts a program and waits for it with its three standard
streams untouched. That is right for an editor and useless for `git`'s
plumbing, a compiler driver or anything that wants what a program printed:
the output went to the terminal and nowhere else. The ways out are all
wrong in a different way. A shell redirect (`["sh", "-c", "cmd > file"]`)
puts the program's arguments back through a parser; a temporary file is a
race and a cleanup; and "give the child a pipe" done naively deadlocks the
first time the child writes more than the pipe holds while the parent is
still writing to it.

## Lineage: Oro's `proc`, with the reactor instead of threads

This is Oro's design (`github.com/qrazil/oro`, reference "proc", and
`src/process.rs`), which is itself Python's `subprocess` with the sharp
edges taken off:

| | Oro | here |
|---|---|---|
| whole answer | `proc.run(args, input=, cwd=, env=, timeout=, check=true, quiet=false)` | `proc.run(argv, input:, cwd:, env:, timeout_ms:, should_check:, is_quiet:)` |
| live child | `proc.spawn(args, cwd=, env=)` | `proc.start(argv, cwd:, env:)` |
| streams | three pipe streams, `io.copy`/`read_until` unchanged | the same: `proc.Pipe` is an `io.Stream` |
| argv | always a list, no shell | the same |
| output cap | 64 MiB per stream, `.truncated` | the same, `is_truncated` |
| env | replaces, does not merge | the same |
| drop | kills and reaps | the same, in `Proc.drop` |
| how it waits | one helper OS thread per active pipe | **the reactor**: a pipe read or write parks only the green thread |

Four renames, each forced:

  - **`start`, not `spawn`**: `spawn` is a keyword here (`spawn f(x);`
    starts a green thread), so it cannot name a function. Go's
    `exec.Cmd.Start` is the other spelling.
  - **`timeout_ms`**: durations are milliseconds in `int` everywhere
    (`timer.sleep_ms`, `Conn.set_read_timeout`), and a name that carries
    its unit is the library's habit.
  - **`should_check`, `is_quiet`**: the naming lint (`naming-decision.md`)
    requires a `bool` to read as a question. `check: false` is
    `should_check: false`.
  - **`is_success()` for `ok`**, and `is_truncated`: a field `ok` is two
    letters, and a `bool` field is a question.

## What is where

| | |
|---|---|
| `proc.run` | start, feed `input` on a green thread, drain both outputs on two more, wait; `Completed` |
| `proc.start` | `Proc { pid, stdin, stdout, stderr }`, returning at once |
| `Proc.wait()` | `os.ExitStatus`, the type `os.run` answers with; the encoding is unchanged (0..255, 256 + signal) |
| `Proc.kill(sig)` | `Ok(false)` once the child has been waited for, so the pid is never signalled after it could belong to someone else |
| `Proc.take_stdin()` etc. | hand a stream out as a local, which is the only thing a value can be moved to a thread from |
| `Pipe` | an `io.Stream`: `read`, `read_all`, `read_until`, `write`, `close`, plus `write_some` and `set_deadline_ns` |
| `proc.Error` | payload-free, `to_str()`: `NotFound`, `PermissionDenied`, `NotExecutable`, `NotADirectory`, `InvalidArgument`, `ResourceLimit`, `BrokenPipe`, `TimedOut`, `NonZeroExit`, `Io` |
| `os.run` | unchanged |

## How it holds together

**One pipe per stream, a copy of the child's end on 0/1/2, and nothing else.**
`sys_pipe` makes close-on-exec pipes; `sys_proc_start_fds` puts the child's
ends on 0, 1 and 2 and every other descriptor closes on exec. The parent's
end is made non-blocking (`SYS_SO_NONBLOCK`) and every wait goes through
`__wait_io`, the same primitive `net` uses, which parks the green thread on
the reactor (epoll on Linux, kqueue on macOS). The child's ends are closed in
the parent right after the start; keeping them is what stops an
end-of-file arriving, and is the classic bug.

**Backpressure is the kernel's pipe buffer and nothing else.** A `Pipe`
holds at most one 64 KiB read. A write that finds the pipe full parks until
the child reads. There is no hidden queue to grow: `io.copy` of 200 MiB
through `cat` ran in an 8.5 MB resident set (`tests/proc/test_proc.sh`
asserts under 48 MiB and the same for 24 MiB as for 200).

**`run` cannot deadlock on large input.** The input is written by one green
thread and each output drained by its own, all three at once; the main
thread only collects their reports. 8 MiB of input beside the output of a
`cat` is a test.

**`wait` is the blocking-call handoff.** `wait4` is a plain blocking call
and the compiler wraps every foreign call in `enter_blocking` /
`exit_blocking` (`concurrency-decision.md`), so the runtime moves the
carrier's other green threads away while it lasts. That is the answer for
"parks only the calling task" at a cost: a carrier is held for the length of
the wait, which is short when the streams are drained first (a child whose
output has reached its end is already exiting) and is the caller's business
when it waits on a child that runs for an hour. Waiting by `pidfd` was not
used: it is Linux-only, the raw backend would need a new system call and the
libc one a feature test, and the handoff already exists. `wait` for a child
that closes its streams and keeps running holds a carrier until it ends.

**A `Proc` dropped without `wait` kills (SIGKILL) and reaps its child**, in a
destructor, as `io.File` closes its descriptor
(`destructors-decision.md`). A `Proc` that was waited for does nothing, and
`kill` never signals a reaped pid.

**Timeouts are three bounds and one watchdog.** `timeout_ms` fixes a
deadline on the monotonic clock (`timer.monotonic_ns`). Each pipe carries it
and its waits end at it (`__wait_io_timeout`), so no stream thread can be
held by a grandchild that kept the pipe open. A watchdog thread kills the
child at the deadline, waking every 50 ms rather than sleeping the whole
timeout (the program waits for its spawned threads before exiting, so a
long sleep would hold a quick run up for the full timeout). The watchdog and
the reaper share a one-slot channel as a lock, and the reaper records the
reaping in it before the watchdog may signal again.

## Limits, said plainly

  - **No shell.** Not in this module, and not as an option. `["sh", "-c",
    text]` is the spelling, and owning it is the point.
  - **No pseudo-terminal.** A child sees pipes, so `isatty` is false and
    programs that colour or page behave as in a script. A pty is a separate
    piece of the sys layer (open, session, controlling terminal, window size).
  - **No process groups or sessions.** `kill` signals one process. A
    shell's children outlive the shell it is killed under; `setsid` and
    `kill(-pgid)` are a later seam.
  - **No extra descriptors.** Only 0, 1 and 2 reach the child, and only as
    pipes or inherited.
  - **`wait` holds a carrier** (above), and a timeout's watchdog polls at
    50 ms. Both would go with a `pidfd`/`WNOWAIT` wait.
  - **The window between a reap and the watchdog's next look** is as long as
    it takes the reaper to take the one-slot token after `wait4` returns;
    a pid could in theory be recycled in it. Closing it takes a wait that
    does not reap (`waitid` with `WNOWAIT`), which is a new sys function.
  - **`check` keeps neither output nor code** in `NonZeroExit`: errors are
    payload-free. `should_check: false` gives the `Completed` and a `match`
    on its `status`.
  - **Grandchildren holding the pipes** keep a `run` waiting to the end of
    its `timeout_ms`, as with `subprocess.run`.
  - **The child's PATH.** A program with no `/` is looked up on the `PATH` in
    `env`, or `/bin:/usr/bin`, never on this process's (a difference from
    `posix_spawnp`, and the same as Python).
  - **macOS.** Pipes go through the kqueue reactor like sockets; this was
    written and tested on Linux only. CI runs `tests/proc/test_proc.sh` on
    macOS and aarch64 (`.github/workflows/`), which is the first time it
    will have run there.
