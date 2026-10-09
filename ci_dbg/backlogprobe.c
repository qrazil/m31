#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <unistd.h>
int main(int argc, char **argv) {
    int n = atoi(argv[1]), backlog = atoi(argv[2]);
    struct rlimit rl = {4096, 4096};
    setrlimit(RLIMIT_NOFILE, &rl);
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_addr.s_addr = htonl(0x7f000001);
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, backlog);
    socklen_t al = sizeof a; getsockname(l, (struct sockaddr *)&a, &al);
    int *cs = malloc(sizeof(int) * n);
    int immediate_err = 0;
    for (int i = 0; i < n; i++) {
        cs[i] = socket(AF_INET, SOCK_STREAM, 0);
        fcntl(cs[i], F_SETFL, O_NONBLOCK);
        int r = connect(cs[i], (struct sockaddr *)&a, sizeof a);
        if (r < 0 && errno != EINPROGRESS) immediate_err++;
    }
    usleep(500000);
    int ok = 0, err = 0, pending = 0; int errs[200] = {0};
    for (int i = 0; i < n; i++) {
        struct pollfd p = {cs[i], POLLOUT, 0};
        int pr = poll(&p, 1, 0);
        int e = 0; socklen_t el = sizeof e;
        getsockopt(cs[i], SOL_SOCKET, SO_ERROR, &e, &el);
        if (pr == 0) pending++; else if (e) { err++; errs[e]++; } else ok++;
    }
    printf("n=%d backlog=%d: immediate_err=%d connected=%d pending=%d failed=%d", n, backlog, immediate_err, ok, pending, err);
    for (int e = 0; e < 200; e++) if (errs[e]) printf(" errno%d x%d", e, errs[e]);
    printf("\n");
    return 0;
}
