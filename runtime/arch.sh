# Single source of truth for which OS-specific reactor backend this build
# links (docs/concurrency-decision.md, "Phase 3.5" -- the kqueue port).
#
# runtime/reactor.h's contract (rt_reactor_create/rt_reactor_wait/
# rt_reactor_destroy) is implemented twice -- runtime/reactor_epoll.c
# (Linux: epoll_wait, eventfd) and runtime/reactor_kqueue.c (macOS/BSD:
# kevent, EVFILT_USER) -- and exactly one is ever compiled into a given
# build. Which one applies is a property of the OS actually running the
# build, not something a per-script `if` should decide ad hoc in a dozen
# different places. Every build/test script that used to hard-code
# "runtime/reactor.c" sources this instead and uses $RT_REACTOR_C, so there
# is exactly one place that knows how to map `uname -s` to a filename --
# the same shape this file already uses for $RT_CTX_ASM (the per-CPU-
# architecture context-switch file; an orthogonal axis, see that variable's
# own assignment below if this file has been merged with that port's).
#
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
