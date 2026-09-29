#!/usr/bin/env python3
"""Drive the compiled `ourgitui` binary under a real pty, against disposable
git fixtures this script builds and destroys itself -- the same discipline
`apps/git/test.sh`/`test_write.sh` already use for the write-path work, and
`apps/git/design.md`'s own "Safety" section names for this client
specifically: never a real repository, real `git` as the oracle throughout.

Run from anywhere:

    python3 apps/git/pty_e2e.py <ourgitui-binary> <scratch-dir>

`<scratch-dir>` is created fresh (and removed first if it already exists) --
`test_gitui.sh` passes it a directory under its own `mktemp -d`, never
anything resembling a real repository.

Every `ok`/`FAIL` line this prints is one check; the exit code is the number
of failures, so `test_gitui.sh` can both grep for `FAIL` and trust `$?`. This
is the "drive it under a pty against a disposable fixture, oracle-checked"
half of the client's test obligations (`apps/git/design.md`, "how you'll know
you're done and correct"); `t_gitclient.src` and `t_gitclient_ops.src` cover
the unit- and oracle-level checks a pty adds nothing to.
"""
import os
import pty
import select
import shutil
import subprocess
import sys
import time

failures = 0


def ok(name):
    global failures
    print("ok   " + name)


def fail(name, detail):
    global failures
    failures += 1
    print("FAIL " + name + ": " + detail)


def git(cwd, *args, env=None):
    r = subprocess.run(["git", "-C", cwd] + list(args), capture_output=True, text=True, env=env)
    # `.rstrip("\n")` only -- `git status --short`'s leading column can be a
    # space (an unstaged-only row starts " M path"), and a blanket `.strip()`
    # would eat that space off the first line and make a clean status look
    # identical to a staged one.
    return r.stdout.rstrip("\n"), r.stderr.rstrip("\n"), r.returncode


GIT_ENV = dict(os.environ)
GIT_ENV["GIT_AUTHOR_NAME"] = "Fixture"
GIT_ENV["GIT_AUTHOR_EMAIL"] = "fixture@example.com"
GIT_ENV["GIT_COMMITTER_NAME"] = "Fixture"
GIT_ENV["GIT_COMMITTER_EMAIL"] = "fixture@example.com"
GIT_ENV["GIT_AUTHOR_DATE"] = "1700000000 +0000"
GIT_ENV["GIT_COMMITTER_DATE"] = "1700000000 +0000"

CLIENT_ENV = dict(os.environ)
CLIENT_ENV["TERM"] = "xterm"
CLIENT_ENV["GIT_AUTHOR_NAME"] = "Client"
CLIENT_ENV["GIT_AUTHOR_EMAIL"] = "client@example.com"
CLIENT_ENV["GIT_COMMITTER_NAME"] = "Client"
CLIENT_ENV["GIT_COMMITTER_EMAIL"] = "client@example.com"


class Session:
    """One `ourgitui` process on a pty, in one fixture directory."""

    def __init__(self, binpath, fixture):
        self.master, slave = pty.openpty()
        self.proc = subprocess.Popen(
            [binpath, fixture],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            cwd=fixture,
            env=CLIENT_ENV,
        )
        os.close(slave)
        time.sleep(0.3)
        self.drain()

    def drain(self, timeout=0.35):
        out = b""
        while True:
            r, _, _ = select.select([self.master], [], [], timeout)
            if not r:
                break
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
        return out

    def send(self, keys):
        os.write(self.master, keys.encode())
        time.sleep(0.2)
        return self.drain()

    def quit(self):
        self.send("q")
        try:
            self.proc.wait(timeout=2)
        except Exception:
            self.proc.kill()
        try:
            os.close(self.master)
        except OSError:
            pass
        return self.proc.returncode


def make_fixture(root, name):
    fx = os.path.join(root, name)
    os.makedirs(fx)
    subprocess.run(["git", "init", "-q", "-b", "main", "."], cwd=fx, check=True)
    return fx


def main():
    if len(sys.argv) != 3:
        print("usage: pty_e2e.py <ourgitui-binary> <scratch-dir>", file=sys.stderr)
        return 2
    binpath = os.path.abspath(sys.argv[1])
    root = os.path.abspath(sys.argv[2])
    if os.path.exists(root):
        shutil.rmtree(root)
    os.makedirs(root)

    # --- staging an untracked file and an unstaged (modified) file ----------
    fx = make_fixture(root, "stage")
    with open(os.path.join(fx, "a.txt"), "w") as f:
        f.write("one\n")
    with open(os.path.join(fx, "b.txt"), "w") as f:
        f.write("two\n")
    git(fx, "add", "-A", env=GIT_ENV)
    git(fx, "commit", "-q", "-m", "first", env=GIT_ENV)
    with open(os.path.join(fx, "a.txt"), "a") as f:
        f.write("changed\n")
    with open(os.path.join(fx, "c.txt"), "w") as f:
        f.write("new\n")

    s = Session(binpath, fx)
    # rows: 0 untracked section, 1 c.txt, 2 unstaged section, 3 M a.txt
    s.send("j")  # -> c.txt
    s.send("s")  # stage the untracked file
    s.quit()

    got, _, _ = git(fx, "status", "--short")
    if got == " M a.txt\nA  c.txt":
        ok("stage: untracked file staged, unstaged modification untouched")
    else:
        fail("stage: untracked file staged, unstaged modification untouched", "git status --short = %r" % got)

    # now stage the remaining unstaged file (a.txt) in a fresh session, where
    # its row position is unambiguous: untracked(0), unstaged(1): M a.txt,
    # staged(1): A c.txt, commits(1).
    s2 = Session(binpath, fx)
    s2.send("j")  # row1: unstaged section -> its child "M a.txt" is row... section is row1, child row2
    s2.send("j")  # row2: M a.txt
    s2.send("s")  # stage it
    s2.quit()
    got, _, _ = git(fx, "status", "--short")
    if got == "M  a.txt\nA  c.txt":
        ok("stage: second (previously unstaged) file also staged")
    else:
        fail("stage: second (previously unstaged) file also staged", "git status --short = %r" % got)

    fsck_out, _, _ = git(fx, "fsck", "--full")
    if fsck_out == "":
        ok("stage: fsck reports nothing after two whole-file stages")
    else:
        fail("stage: fsck reports nothing after two whole-file stages", fsck_out)

    # --- unstaging: a path tracked in HEAD, and a newly-added path ----------
    fx2 = make_fixture(root, "unstage")
    with open(os.path.join(fx2, "a.txt"), "w") as f:
        f.write("one\n")
    git(fx2, "add", "-A", env=GIT_ENV)
    git(fx2, "commit", "-q", "-m", "first", env=GIT_ENV)
    with open(os.path.join(fx2, "a.txt"), "a") as f:
        f.write("changed\n")
    with open(os.path.join(fx2, "new.txt"), "w") as f:
        f.write("brand new\n")
    git(fx2, "add", "-A", env=GIT_ENV)
    # rows: untracked(0), unstaged(0), staged(2): M a.txt, A new.txt, commits(1)
    s3 = Session(binpath, fx2)
    s3.send("j")
    s3.send("j")
    s3.send("j")  # row3: M a.txt (staged section is row2, first child row3)
    s3.send("u")  # unstage a.txt -- tracked in HEAD
    s3.quit()
    got, _, _ = git(fx2, "status", "--short")
    if got == " M a.txt\nA  new.txt":
        ok("unstage: a path tracked in HEAD goes back to HEAD's version, not deleted")
    else:
        fail("unstage: a path tracked in HEAD goes back to HEAD's version, not deleted", "git status --short = %r" % got)

    # cursor reset to row0 after the reload (its old id no longer exists);
    # new.txt is now the sole staged row: untracked(0), unstaged(1): a.txt,
    # staged(1): new.txt -- row0 untracked, row1 unstaged section, row2
    # a.txt, row3 staged section, row4 new.txt.
    s4 = Session(binpath, fx2)
    s4.send("j")
    s4.send("j")
    s4.send("j")
    s4.send("j")  # row4: A new.txt
    s4.send("u")  # unstage new.txt -- never in HEAD, so dropped entirely
    s4.quit()
    got, _, _ = git(fx2, "status", "--short")
    if got == " M a.txt\n?? new.txt":
        ok("unstage: a path never in HEAD is dropped from the index, not committed empty")
    else:
        fail("unstage: a path never in HEAD is dropped from the index, not committed empty", "git status --short = %r" % got)

    fsck_out, _, _ = git(fx2, "fsck", "--full")
    dangling_only = all(line.startswith("dangling blob") for line in fsck_out.splitlines() if line)
    if dangling_only:
        ok("unstage: fsck reports nothing but expected dangling blobs")
    else:
        fail("unstage: fsck reports nothing but expected dangling blobs", fsck_out)

    # --- committing: write the message externally, finish, check with git ---
    fx3 = make_fixture(root, "commit")
    with open(os.path.join(fx3, "a.txt"), "w") as f:
        f.write("one\n")
    git(fx3, "add", "-A", env=GIT_ENV)
    # no commit yet -- unborn branch, exactly the case `finish_commit` must
    # also handle (no parent).
    s5 = Session(binpath, fx3)
    s5.send("c")  # open the commit which-key overlay
    s5.send("e")  # write the template at COMMIT_EDITMSG
    time.sleep(0.1)
    msgpath = os.path.join(fx3, ".git", "COMMIT_EDITMSG")
    with open(msgpath) as f:
        template = f.read()
    with open(msgpath, "w") as f:
        f.write("first commit via the interactive client\n" + template)
    s5.send("f")  # finish
    s5.quit()

    log_out, _, log_rc = git(fx3, "log", "--oneline")
    if log_rc == 0 and log_out.endswith("first commit via the interactive client"):
        ok("commit: the client's first commit (unborn branch, no parent) appears in git log")
    else:
        fail("commit: the client's first commit (unborn branch, no parent) appears in git log", "log=%r rc=%d" % (log_out, log_rc))

    status_out, _, _ = git(fx3, "status", "--short")
    if status_out == "":
        ok("commit: working tree clean after the commit (the staged file was committed)")
    else:
        fail("commit: working tree clean after the commit (the staged file was committed)", status_out)

    author_out, _, _ = git(fx3, "log", "-1", "--format=%an <%ae>")
    if author_out == "Client <client@example.com>":
        ok("commit: author identity came from GIT_AUTHOR_NAME/EMAIL")
    else:
        fail("commit: author identity came from GIT_AUTHOR_NAME/EMAIL", author_out)

    fsck_out, _, _ = git(fx3, "fsck", "--full")
    if fsck_out == "":
        ok("commit: fsck reports nothing after the client's own commit")
    else:
        fail("commit: fsck reports nothing after the client's own commit", fsck_out)

    # a second commit, so the tree-builder is exercised with a real parent
    # and an existing history to extend.
    with open(os.path.join(fx3, "b.txt"), "w") as f:
        f.write("two\n")
    s6 = Session(binpath, fx3)
    s6.send("j")  # onto the untracked b.txt
    s6.send("s")  # stage it
    s6.send("c")
    s6.send("e")
    time.sleep(0.1)
    with open(msgpath) as f:
        template2 = f.read()
    with open(msgpath, "w") as f:
        f.write("second commit\n" + template2)
    s6.send("f")
    s6.quit()
    log_out, _, _ = git(fx3, "log", "--oneline")
    lines = log_out.splitlines()
    if len(lines) == 2 and lines[0].endswith("second commit") and lines[1].endswith("first commit via the interactive client"):
        ok("commit: a second commit has the first as its parent")
    else:
        fail("commit: a second commit has the first as its parent", log_out)

    parents, _, _ = git(fx3, "log", "-1", "--format=%P")
    if len(parents.split()) == 1:
        ok("commit: the second commit has exactly one parent")
    else:
        fail("commit: the second commit has exactly one parent", repr(parents))

    # --- the empty-message refusal -------------------------------------------
    fx4 = make_fixture(root, "empty-message")
    with open(os.path.join(fx4, "a.txt"), "w") as f:
        f.write("one\n")
    git(fx4, "add", "-A", env=GIT_ENV)
    s7 = Session(binpath, fx4)
    s7.send("c")
    s7.send("e")  # a fresh template: comment lines only
    out = s7.send("f")  # finish with nothing but comments -- must refuse
    s7.quit()
    log_out, _, log_rc = git(fx4, "log", "--oneline")
    status_out, _, _ = git(fx4, "status", "--short")
    if log_rc != 0 and status_out == "A  a.txt" and b"aborting commit due to empty commit message" in out:
        ok("commit: an all-comment message is refused, exactly like real git")
    else:
        fail(
            "commit: an all-comment message is refused, exactly like real git",
            "log_rc=%d status=%r saw_refusal=%s" % (log_rc, status_out, b"aborting commit due to empty commit message" in out),
        )

    return failures


if __name__ == "__main__":
    sys.exit(1 if main() > 0 else 0)
