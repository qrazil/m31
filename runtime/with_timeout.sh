#!/usr/bin/env bash
# Run a command with a hard time limit, on Linux AND on macOS.
#
#     bash runtime/with_timeout.sh SECONDS command [args...]
#
# macOS has no timeout(1) (coreutils' is installed as `gtimeout`, if at all),
# and a test that cannot be bounded turns a hang into hours of silence. perl is
# on every macOS and Linux box and its alarm() needs nothing else, so this is
# a few lines of it: the command runs in its own process group, and when the
# limit passes the whole group gets SIGTERM, then SIGKILL two seconds later.
#
# Exit status: the command's own, or 124 (as timeout(1)) after a kill. On a
# kill it first prints a stack sample of the hung process where the host has a
# tool for that (`sample` on macOS), because a hang with no stack is just a
# mystery; set WITH_TIMEOUT_NO_DIAG=1 to skip it.
set -uo pipefail
if [ $# -lt 2 ]; then
    echo "usage: with_timeout.sh SECONDS command [args...]" >&2
    exit 2
fi
exec perl -e '
    use POSIX ":sys_wait_h";
    my $secs = shift @ARGV;
    my $pid = fork();
    die "fork: $!" unless defined $pid;
    if ($pid == 0) {
        setpgrp(0, 0);
        exec { $ARGV[0] } @ARGV;
        print STDERR "with_timeout: cannot run $ARGV[0]: $!\n";
        POSIX::_exit(127);
    }
    setpgrp($pid, $pid);
    $SIG{INT} = $SIG{TERM} = sub { kill "TERM", -$pid; exit 130; };
    my $timed_out = 0;
    $SIG{ALRM} = sub {
        $timed_out = 1;
        print STDERR "with_timeout: $secs s limit hit by: @ARGV\n";
        if (!$ENV{WITH_TIMEOUT_NO_DIAG}) {
            if (-x "/usr/bin/sample") {
                my @kids = grep { /^\s*\d+\s*$/ } `pgrep -g $pid 2>/dev/null`;
                for my $k (@kids) { $k =~ s/\s+//g; print STDERR `/usr/bin/sample $k 1 2>&1 | head -80`; }
            }
        }
        kill "TERM", -$pid;
        $SIG{ALRM} = sub { kill "KILL", -$pid; };
        alarm 2;
    };
    alarm $secs;
    my $rc;
    while (1) {
        my $w = waitpid($pid, 0);
        if ($w == $pid) { $rc = $?; last; }
        last if $w < 0 && !$!{EINTR};
    }
    kill "KILL", -$pid;
    exit 124 if $timed_out;
    exit(($rc & 127) ? 128 + ($rc & 127) : ($rc >> 8));
' -- "$@"
