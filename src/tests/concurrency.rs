use super::*;

#[test]
fn a_send_moves_and_a_recv_acquires() {
    // No retain and no release in between: the sender gives its reference up
    // and the receiver takes it. That is what keeps refcounts non-atomic.
    let out =
        ir("Chan<str> c = Chan<str>(2);\nstr s = \"a\" + \"b\";\nsend(c, s);\nprint(recv(c));");
    let main = out.split("func $main").nth(1).unwrap();
    assert_eq!(
        main.matches("rc_inc").count(),
        0,
        "a move must not retain:\n{main}"
    );
    // Two: the received string, and the channel local at scope end. The
    // channel's is a no-op, because a channel is immortal -- see
    // rt_chan_new -- but the lowering does not special-case it.
    assert_eq!(
        main.matches("rc_dec").count(),
        2,
        "the received value once, plus the channel local:\n{main}"
    );
}

#[test]
fn using_a_moved_local_is_refused() {
    assert!(
        err("Chan<str> c = Chan<str>(2);\nstr s = \"x\";\nsend(c, s);\nprint(s);")
            .contains("was moved and cannot be used again")
    );
    assert!(
        err("Chan<str> c = Chan<str>(2);\nstr s = \"x\";\nsend(c, s);\nsend(c, s);")
            .contains("was moved")
    );
    assert!(
        err("void w(str s) { print(s); }\nstr s = \"x\";\nspawn w(s);\nprint(s);")
            .contains("was moved")
    );
}

#[test]
fn an_int_needs_no_move() {
    // Only references are moved; an int is copied, so it stays usable.
    let out = ir("Chan<int> c = Chan<int>(2);\nint n = 5;\nsend(c, n);\nprint(n);");
    assert!(out.contains("rt_chan_send"), "expected a send:\n{out}");
}

#[test]
fn a_channel_is_shared_rather_than_moved() {
    // The one exemption: a channel is how threads share, so passing it to a
    // spawn aliases it. Without this the ordinary worker pattern could not
    // be written at all.
    let out = ir("void w(Chan<int> c) { send(c, 1); }\nChan<int> c = Chan<int>(2);\nspawn w(c);\nprint(recv(c));");
    assert!(out.contains("spawn w"), "expected the spawn:\n{out}");
}

#[test]
fn channel_misuse_is_rejected() {
    assert!(err("int x = 1;\nsend(x, 2);").contains("needs a channel"));
    assert!(err("int f(int a) { return a; }\nspawn f(1);").contains("needs a void function"));
    assert!(err("Chan<int> c = Chan<int>(2);\nsend(c, \"s\");").contains("expected int"));
}
