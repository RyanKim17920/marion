/*
 * Does `setsid()` alone claim the pty as the child's controlling terminal when the
 * slave is on fd 0?  Six cells: {slave on fd 0, slave on fd 1 with a pipe on fd 0}
 * x {no setsid, setsid only, setsid + ioctl(slave_fd, TIOCSCTTY, 0)}.
 *
 * The parent answers with tcgetsid(master), which is answerable only if this pty is
 * some session's controlling terminal.  Printed per cell: the child's pid, what
 * tcgetsid said, and errno when it refused.
 *
 *   cc -o ctty_probe ctty_probe.c && ./ctty_probe
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <signal.h>
#include <termios.h>
#include <unistd.h>

static void cell2(const char *name, int slave_on_zero, int do_setsid, int do_ioctl, int parent_opens);
static void cell(const char *name, int slave_on_zero, int do_setsid, int do_ioctl) {
    cell2(name, slave_on_zero, do_setsid, do_ioctl, 0);
}
static void cell2(const char *name, int slave_on_zero, int do_setsid, int do_ioctl, int parent_opens) {
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    if (master < 0) { perror("posix_openpt"); exit(1); }
    if (grantpt(master) || unlockpt(master)) { perror("grantpt/unlockpt"); exit(1); }
    char *sname = ptsname(master);
    int pfd[2];
    if (pipe(pfd)) { perror("pipe"); exit(1); }

    int pre = parent_opens ? open(sname, O_RDWR | O_NOCTTY) : -1;
    pid_t pid = fork();
    if (pid == 0) {
        int slave = parent_opens ? pre : open(sname, O_RDWR | O_NOCTTY);
        if (slave < 0) _exit(101);
        int ctty_fd;
        if (slave_on_zero) {
            dup2(slave, 0); dup2(slave, 1); dup2(slave, 2);
            ctty_fd = 0;
        } else {
            dup2(pfd[0], 0); dup2(slave, 1); dup2(slave, 2);
            ctty_fd = 1;
        }
        close(pfd[0]); close(pfd[1]);
        if (slave > 2) close(slave);
        close(master);
        if (do_setsid && setsid() < 0) _exit(102);
        if (do_ioctl && ioctl(ctty_fd, TIOCSCTTY, 0) != 0) _exit(103);
        if (getenv("PROBE_EXEC")) {
            execl("/bin/sh", "sh", "-c", "tty; sleep 3", (char *)NULL);
            _exit(104);
        }
        sleep(3);
        _exit(0);
    }
    close(pfd[0]); close(pfd[1]);
    if (pre >= 0) close(pre);
    usleep(300000);
    errno = 0;
    pid_t sid = tcgetsid(master);
    printf("%-28s child=%d tcgetsid=%d %s%s\n", name, pid, (int)sid,
           sid < 0 ? strerror(errno) : "",
           (sid == pid) ? "  <-- this pty IS the child's controlling terminal" : "");
    kill(pid, 9);
    waitpid(pid, NULL, 0);
    close(master);
}

int main(void) {
    cell("slave-fd0 / bare",        1, 0, 0);
    cell("slave-fd0 / setsid",      1, 1, 0);
    cell("slave-fd0 / setsid+ioctl",1, 1, 1);
    cell("pipe-fd0 / bare",         0, 0, 0);
    cell("pipe-fd0 / setsid",       0, 1, 0);
    cell("pipe-fd0 / setsid+ioctl", 0, 1, 1);
    cell2("PARENT-opens fd0 / setsid",       1, 1, 0, 1);
    cell2("PARENT-opens fd0 / setsid+ioctl", 1, 1, 1, 1);
    cell2("PARENT-opens pipe / setsid",      0, 1, 0, 1);
    return 0;
}
