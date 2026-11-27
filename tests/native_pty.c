// Exercise the installed Linux guest's Unix98 PTYs with real file descriptors.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "PTY check failed at line %d: %s (errno %d)\n", __LINE__, #condition, errno); \
        exit(1); \
    } \
} while (0)

static int open_master(char *name, size_t size) {
    int master = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
    CHECK(master >= 0);
    CHECK(grantpt(master) == 0);
    CHECK(unlockpt(master) == 0);
    CHECK(ptsname_r(master, name, size) == 0);
    return master;
}

static void check_operator(const char *private_slave) {
    CHECK(setgroups(0, NULL) == 0);
    CHECK(setgid(1000) == 0 && setuid(1000) == 0);
    CHECK(open(private_slave, O_RDWR | O_NOCTTY) == -1 && errno == EACCES);

    char name[128], byte;
    int master = open_master(name, sizeof(name));
    struct stat metadata;
    CHECK(stat(name, &metadata) == 0 && S_ISCHR(metadata.st_mode));
    CHECK(metadata.st_uid == getuid() && (metadata.st_mode & 0777) == 0600);
    int slave = open(name, O_RDWR | O_NOCTTY);
    CHECK(slave >= 0 && isatty(slave));
    struct termios settings;
    CHECK(tcgetattr(slave, &settings) == 0);
    cfmakeraw(&settings);
    CHECK(tcsetattr(slave, TCSANOW, &settings) == 0);
    CHECK(write(master, "a", 1) == 1 && read(slave, &byte, 1) == 1 && byte == 'a');
    CHECK(write(slave, "b", 1) == 1 && read(master, &byte, 1) == 1 && byte == 'b');
    struct winsize size = {.ws_row = 31, .ws_col = 97};
    CHECK(ioctl(master, TIOCSWINSZ, &size) == 0);
    size = (struct winsize){0};
    CHECK(ioctl(slave, TIOCGWINSZ, &size) == 0 && size.ws_row == 31 && size.ws_col == 97);

    // One pair belongs to the parent and one to this process.
    int held[64 - 2];
    for (size_t i = 0; i < sizeof(held) / sizeof(held[0]); i++) {
        held[i] = posix_openpt(O_RDWR | O_NOCTTY);
        CHECK(held[i] >= 0);
    }
    CHECK(posix_openpt(O_RDWR | O_NOCTTY) == -1 && errno == ENOSPC);
    CHECK(write(master, "c", 1) == 1 && read(slave, &byte, 1) == 1 && byte == 'c');
    CHECK(close(held[0]) == 0);
    held[0] = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(held[0] >= 0);
    for (size_t i = 0; i < sizeof(held) / sizeof(held[0]); i++) {
        CHECK(close(held[i]) == 0);
    }
    CHECK(close(slave) == 0 && close(master) == 0);
    CHECK(stat(name, &metadata) == -1 && errno == ENOENT);
}

int main(void) {
    char name[128];
    int master = open_master(name, sizeof(name));
    pid_t child = fork();
    CHECK(child >= 0);
    if (child == 0) {
        alarm(20);
        CHECK(close(master) == 0);
        check_operator(name);
        _exit(0);
    }
    int status;
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(close(master) == 0);
    puts("private PTY I/O, resize, 64-pair limit and reuse passed");
    return 0;
}
