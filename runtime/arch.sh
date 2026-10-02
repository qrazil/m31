# Single source of truth for which host-specific files this build links --
# two independent axes, each with its own case statement below:
#
#   $RT_CTX_ASM    which hand-written context-switch file (CPU architecture)
#   $RT_REACTOR_C  which reactor backend (OS)
#
# runtime/greenthread.h's `struct rt_ctx` and exactly one of
# runtime/ctx_switch_x86_64.s / runtime/ctx_switch_aarch64.s are one
# contract (see either .s file's own header comment; docs/
# concurrency-decision.md, "Phase 4" -- portability). runtime/reactor.h's
# contract (rt_reactor_create/rt_reactor_wait/rt_reactor_destroy) is
# implemented twice -- runtime/reactor_epoll.c (Linux: epoll_wait, eventfd)
# and runtime/reactor_kqueue.c (macOS/BSD: kevent, EVFILT_USER) -- and
# exactly one of those is ever compiled into a given build (docs/
# concurrency-decision.md, "Phase 3.5" -- the kqueue port). Neither choice
# is something a per-script `if` should decide ad hoc in a dozen different
# places. Every build/test script that used to hard-code
# "runtime/ctx_switch_x86_64.s" or "runtime/reactor.c" sources this instead
# and uses $RT_CTX_ASM / $RT_REACTOR_C, so there is exactly one place that
# knows how to map `uname -m`/`uname -s` to a filename for each axis.
#
# Not folded into config.sh on purpose: config.sh is the language's
# *identity* (name/binary/extension), rewritten wholesale by rename.sh --
# this is the *host's* architecture and OS, an orthogonal pair of axes that
# rename.sh has no business touching.
case "$(uname -m)" in
    x86_64 | amd64)
        RT_CTX_ASM=runtime/ctx_switch_x86_64.s
        ;;
    aarch64 | arm64)
        RT_CTX_ASM=runtime/ctx_switch_aarch64.s
        ;;
    *)
        echo "unsupported architecture: $(uname -m) -- this runtime has a" \
             "hand-written context switch only for x86-64 and aarch64" \
             "(docs/concurrency-decision.md, Phase 4)" >&2
        exit 1
        ;;
esac

# `spawn`/`Chan`/`net` always route through the reactor now
# (docs/concurrency-decision.md "Phase 3.5"), so this is not an opt-in: every
# single program build links one of these two files, unconditionally.
case "$(uname -s)" in
    Linux)
        RT_REACTOR_C=runtime/reactor_epoll.c
        ;;
    Darwin | FreeBSD | OpenBSD | NetBSD | DragonFly)
        RT_REACTOR_C=runtime/reactor_kqueue.c
        ;;
    *)
        echo "unsupported OS: $(uname -s) -- this runtime has a reactor" \
             "backend only for Linux (epoll) and macOS/BSD (kqueue)" \
             "(docs/concurrency-decision.md, Phase 3.5)" >&2
        exit 1
        ;;
esac
