// LD_PRELOAD shim: times every fsync/fdatasync/sync_file_range and prints one line per call to stderr:
// SPY <call> <monotonic ms at end> <duration ms> <file path>
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
static double now_ms(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static void report(const char *name, int fd, double started) {
    double end = now_ms();
    char link[64], path[512];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, path, sizeof path - 1);
    path[n < 0 ? 0 : n] = 0;
    fprintf(stderr, "SPY %s %.3f %.3f %s\n", name, end, end - started, path);
}
int fsync(int fd) {
    static int (*real)(int);
    if (!real) real = dlsym(RTLD_NEXT, "fsync");
    double t = now_ms(); int r = real(fd); report("fsync", fd, t); return r;
}
int fdatasync(int fd) {
    static int (*real)(int);
    if (!real) real = dlsym(RTLD_NEXT, "fdatasync");
    double t = now_ms(); int r = real(fd); report("fdatasync", fd, t); return r;
}
