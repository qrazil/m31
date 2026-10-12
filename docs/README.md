# Documentation index

Start with `reference.md` (the language) and `roadmap.md` (what exists and
what is next). Every file here keeps its path: source comments, the README and
the CHANGELOG cite them by name.

## Reference

| File | What it covers |
| --- | --- |
| `reference.md` | The language reference |
| `types.md` | The type system |
| `value-enums.md` | Value enums |
| `ir-v0.md` | IR v0, the walking skeleton |
| `sys-layer.md` | The sys layer: everything below the language |
| `stdlib-seam.md` | Where the language stops and C begins |
| `concurrency.md` | Concurrency and refcounting, with evidence |
| `roadmap.md` | Features and roadmap |

## Decisions

| File | Subject |
| --- | --- |
| `naming-decision.md` | Naming rules and the 0.4.0 renames |
| `project-layout-decision.md` | Project layout, prefixes, deps files |
| `stdlib-layout-decision.md` | Standard library groups (0.4.0) |
| `stdlib-decision.md` | The standard library, and what an error is |
| `errors-decision.md` | Errors |
| `modules-decision.md` | Modules |
| `module-state-decision.md` | Module state |
| `remote-imports-decision.md` | Remote imports |
| `concurrency-decision.md` | Green threads |
| `net-timeouts-decision.md` | Bounded waits that park |
| `reentrancy-decision.md` | The runtime calling the program back |
| `closures-decision.md` | Callbacks, function references, lambdas |
| `destructors-decision.md` | Destructors |
| `const-decision.md` | `const` |
| `text-decision.md` | Text |
| `regex-decision.md` | Regex |
| `ssh-decision.md` | The SSH client |
| `proc-decision.md` | Child processes: pipes, run, start |
| `apps-repo-split-decision.md` | Per-app repository split |

## History and working notes

| File | Note |
| --- | --- |
| `release-notes-v0.3.3.md` | Release notes for v0.3.3 |
| `dip-investigation.md` | Diagnosis of a throughput dip on a branch never meant for master |
| `perf-board.md` | The performance board; may be stale |
| `readability-plan.md` | Plan for making the repository readable; its source-tree part is being carried out |
