#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <unistd.h>
int main(void) {
    struct rlimit rl = {48, 48};
    setrlimit(RLIMIT_NOFILE, &rl);
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_addr.s_addr = htonl(0x7f000001);
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 128);
    socklen_t al = sizeof a; getsockname(l, (struct sockaddr *)&a, &al);
    fcntl(l, F_SETFL, O_NONBLOCK);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (struct sockaddr *)&a, sizeof a);
    usleep(100000);
    int held[64], n = 0;
    for (;;) { int f = open("/dev/null", O_RDONLY); if (f < 0) break; held[n++] = f; }
    errno = 0; int r = accept(l, NULL, NULL);
    printf("accept with full table: r=%d errno=%d (%s)\n", r, errno, strerror(errno));
    struct pollfd p = {l, POLLIN, 0};
    printf("poll after EMFILE: %d\n", poll(&p, 1, 500));
    close(held[--n]);
    printf("poll after freeing one: %d\n", poll(&p, 1, 500));
    errno = 0; r = accept(l, NULL, NULL);
    printf("accept after freeing one: r=%d errno=%d (%s)\n", r, errno, strerror(errno));
    return 0;
}
