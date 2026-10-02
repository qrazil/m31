# Single source of truth for which hand-written context-switch file this
# build links (docs/concurrency-decision.md, "Phase 4" -- portability).
#
# runtime/greenthread.h's `struct rt_ctx` and exactly one of
# runtime/ctx_switch_x86_64.s / runtime/ctx_switch_aarch64.s are one
# contract (see either .s file's own header comment); which one applies is
# a property of the machine actually running the build, not something a
# per-script `if` should decide ad hoc in a dozen different places. Every
# build/test script that used to hard-code "runtime/ctx_switch_x86_64.s"
# sources this instead and uses $RT_CTX_ASM, so there is exactly one place
# that knows how to map `uname -m` to a filename.
#
# Not folded into config.sh on purpose: config.sh is the language's
# *identity* (name/binary/extension), rewritten wholesale by rename.sh --
# this is the *host's* architecture, an orthogonal axis that rename.sh has
# no business touching.
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
