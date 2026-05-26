#define _GNU_SOURCE

#include <errno.h>
#include <grp.h>
#include <pwd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <termios.h>
#include <unistd.h>
#include <fcntl.h>

static void die(const char *message)
{
    fprintf(stderr, "agent-sandbox-login-tty: %s: %s\n", message, strerror(errno));
    exit(127);
}

int main(int argc, char **argv)
{
    const char *tty_path;
    const char *user;
    const char *command;
    struct passwd *pw;
    int fd;

    if (argc < 4) {
        fprintf(stderr, "usage: %s TTY USER COMMAND [ARG...]\n", argv[0]);
        return 64;
    }

    tty_path = argv[1];
    user = argv[2];
    command = argv[3];

    errno = 0;
    pw = getpwnam(user);
    if (pw == NULL) {
        if (errno != 0) {
            die("getpwnam");
        }
        fprintf(stderr, "agent-sandbox-login-tty: unknown user: %s\n", user);
        return 67;
    }

    if (setsid() < 0) {
        die("setsid");
    }

    fd = open(tty_path, O_RDWR);
    if (fd < 0) {
        die("open tty");
    }

    if (ioctl(fd, TIOCSCTTY, 1) < 0) {
        die("TIOCSCTTY");
    }

    if (tcsetpgrp(fd, getpgrp()) < 0) {
        die("tcsetpgrp");
    }

    if (dup2(fd, STDIN_FILENO) < 0 ||
        dup2(fd, STDOUT_FILENO) < 0 ||
        dup2(fd, STDERR_FILENO) < 0) {
        die("dup2");
    }
    if (fd > STDERR_FILENO) {
        close(fd);
    }

    if (initgroups(user, pw->pw_gid) < 0) {
        die("initgroups");
    }
    if (setgid(pw->pw_gid) < 0) {
        die("setgid");
    }
    if (setuid(pw->pw_uid) < 0) {
        die("setuid");
    }

    if (setenv("HOME", pw->pw_dir, 1) < 0 ||
        setenv("USER", pw->pw_name, 1) < 0 ||
        setenv("LOGNAME", pw->pw_name, 1) < 0 ||
        setenv("SHELL", pw->pw_shell, 1) < 0) {
        die("setenv");
    }

    if (chdir(pw->pw_dir) < 0) {
        die("chdir");
    }

    execv(command, &argv[3]);
    die("execv");
}
