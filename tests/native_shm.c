// Exercise POSIX named shared memory on the disposable native system.
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "shm check failed at line %d: %s (errno %d)\n", __LINE__, #condition, errno); \
        exit(1); \
    } \
} while (0)

static const char private_name[] = "/psios-root-private";
static const char shared_name[] = "/psios-operator-exchange";

static int create(const char *name) {
    int fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(fd >= 0);
    return fd;
}

static void exchange(void) {
    int fd = create(shared_name);
    CHECK(ftruncate(fd, 4096) == 0);
    char *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(page != MAP_FAILED);
    CHECK(close(fd) == 0);
    page[0] = 'A';
    pid_t reader = fork();
    CHECK(reader >= 0);
    if (reader == 0) {
        CHECK(munmap(page, 4096) == 0);
        fd = shm_open(shared_name, O_RDWR, 0);
        CHECK(fd >= 0);
        struct stat owner;
        CHECK(fstat(fd, &owner) == 0 && owner.st_uid == getuid() && (owner.st_mode & 0777) == 0600);
        page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        CHECK(page != MAP_FAILED && page[0] == 'A');
        CHECK(close(fd) == 0 && shm_unlink(shared_name) == 0);
        page[0] = 'B';
        CHECK(munmap(page, 4096) == 0);
        _exit(0);
    }
    int status;
    CHECK(waitpid(reader, &status, 0) == reader && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(page[0] == 'B');
    CHECK(shm_open(shared_name, O_RDONLY, 0) == -1 && errno == ENOENT);
    CHECK(munmap(page, 4096) == 0);
}

static void check_capacity(void) {
    struct statvfs space;
    CHECK(statvfs("/dev/shm", &space) == 0);
    CHECK(space.f_blocks * space.f_frsize == 16 * 1024 * 1024 && space.f_files == 1024);
    int fd = create(shared_name);
    off_t bytes = (off_t)space.f_bavail * space.f_frsize;
    CHECK(bytes > 0 && posix_fallocate(fd, 0, bytes) == 0);
    CHECK(posix_fallocate(fd, bytes, space.f_frsize) == ENOSPC);
    CHECK(pwrite(fd, "X", 1, 0) == 1);
    CHECK(shm_unlink(shared_name) == 0 && close(fd) == 0);

    size_t created = 0;
    char name[64];
    for (; created < space.f_files; created++) {
        snprintf(name, sizeof(name), "/psios-slot-%zu", created);
        fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fd < 0) {
            CHECK(errno == ENOSPC);
            break;
        }
        CHECK(close(fd) == 0);
    }
    CHECK(created == space.f_favail && created > 0);
    // Exhaustion is confined to this mount. Ordinary scratch writes still work.
    char temp[] = "/tmp/shm-independent-XXXXXX";
    fd = mkstemp(temp);
    CHECK(fd >= 0 && write(fd, "x", 1) == 1 && close(fd) == 0 && unlink(temp) == 0);
    for (size_t i = 0; i < created; i++) {
        snprintf(name, sizeof(name), "/psios-slot-%zu", i);
        CHECK(shm_unlink(name) == 0);
    }
    fd = create(shared_name);
    CHECK(posix_fallocate(fd, 0, 4096) == 0 && close(fd) == 0 && shm_unlink(shared_name) == 0);
}

int main(void) {
    umask(077);
    int private_fd = create(private_name);
    CHECK(close(private_fd) == 0);
    pid_t operator = fork();
    CHECK(operator >= 0);
    if (operator == 0) {
        alarm(30);
        CHECK(setgroups(0, NULL) == 0 && setgid(1000) == 0 && setuid(1000) == 0);
        CHECK(shm_open(private_name, O_RDWR, 0) == -1 && errno == EACCES);
        CHECK(shm_unlink(private_name) == -1 && (errno == EPERM || errno == EACCES));
        exchange();
        check_capacity();
        _exit(0);
    }
    int status;
    CHECK(waitpid(operator, &status, 0) == operator && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(shm_unlink(private_name) == 0);
    puts("POSIX shared memory exchange, permissions, byte and inode limits and recovery passed");
    return 0;
}
