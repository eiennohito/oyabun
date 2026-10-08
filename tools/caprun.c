/*
 * caprun — run a command with specific Linux capabilities.
 *
 * Setuid-root wrapper. Drops to the real user, retains only the
 * capabilities oyabun needs for its privileged mode, passes them as
 * ambient so the exec'd child inherits them. The binary is the
 * policy — exactly these caps, nothing more.
 *
 * Granted:
 *   CAP_BPF         (39) — BPF program loading (task iterator, kprobes)
 *   CAP_PERFMON     (38) — BPF tracing attachment
 *   CAP_NET_ADMIN   (12) — netlink (planned: I/O monitoring); unused until then
 *   CAP_SYS_PTRACE  (19) — /proc/<pid>/io for other users' processes
 *
 * Build:   cc -static -o tools/caprun tools/caprun.c
 * Install: sudo chown root tools/caprun && sudo chmod 4755 tools/caprun
 * Use:     tools/caprun target/debug/oya [args...]
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <linux/capability.h>

/* Capability numbers — from <linux/capability.h>, repeated here so the
   build doesn't need kernel headers newer than the running kernel. */
#ifndef CAP_NET_ADMIN
#define CAP_NET_ADMIN   12
#endif
#ifndef CAP_SYS_PTRACE
#define CAP_SYS_PTRACE  19
#endif
#ifndef CAP_PERFMON
#define CAP_PERFMON     38
#endif
#ifndef CAP_BPF
#define CAP_BPF         39
#endif

#define CAP_WORD(c) ((c) >> 5)
#define CAP_BIT(c)  (1U << ((c) & 31))

static const int CAPS[] = { CAP_NET_ADMIN, CAP_SYS_PTRACE, CAP_PERFMON, CAP_BPF };
#define NCAPS ((int)(sizeof(CAPS) / sizeof(CAPS[0])))

static void die(const char *msg) {
    fprintf(stderr, "caprun: %s: %s\n", msg, strerror(errno));
    _exit(1);
}

static void scrub_env(void) {
    static const char *const names[] = {
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "LD_DEBUG",
        "LD_DEBUG_OUTPUT",
        "LD_ORIGIN_PATH",
        "LD_PROFILE",
        "LD_SHOW_AUXV",
        "GCONV_PATH",
        "GETCONF_DIR",
        "HOSTALIASES",
        "LOCALDOMAIN",
        "LOCPATH",
        "MALLOC_TRACE",
        "NLSPATH",
        "RESOLV_HOST_CONF",
        "RES_OPTIONS",
        "TMPDIR",
        "TZDIR",
    };

    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
        unsetenv(names[i]);
    }
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
            "Usage: caprun <command> [args...]\n"
            "\n"
            "Run <command> with BPF/tracing/ptrace capabilities.\n"
            "Caps: CAP_BPF, CAP_PERFMON, CAP_NET_ADMIN, CAP_SYS_PTRACE\n");
        return 1;
    }

    /* 1. Keep capabilities across the uid drop. */
    if (prctl(PR_SET_KEEPCAPS, 1) < 0)
        die("PR_SET_KEEPCAPS");

    /* 2. Drop to the real user/group — we're setuid root only to get
          the caps; running the child as root is not the intent. */
    if (setgid(getgid()) < 0) die("setgid");
    if (setuid(getuid()) < 0) die("setuid");

    /* 3. After setuid, effective set is cleared. Rebuild permitted +
          effective + inheritable to exactly our four caps.
          Uses the raw capset syscall (v3 = two 32-bit words). */
    struct __user_cap_header_struct hdr = {
        .version = _LINUX_CAPABILITY_VERSION_3,
        .pid = 0,
    };
    struct __user_cap_data_struct data[2] = {};

    for (int i = 0; i < NCAPS; i++) {
        int w = CAP_WORD(CAPS[i]);
        unsigned bit = CAP_BIT(CAPS[i]);
        data[w].permitted   |= bit;
        data[w].effective   |= bit;
        data[w].inheritable |= bit;
    }

    if (syscall(SYS_capset, &hdr, data) < 0)
        die("capset");

    /* 4. Raise ambient caps so the exec'd child inherits them.
          Requires each cap in both permitted and inheritable (set above). */
    for (int i = 0; i < NCAPS; i++) {
        if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, CAPS[i], 0, 0) < 0) {
            fprintf(stderr, "caprun: PR_CAP_AMBIENT_RAISE(%d): %s\n",
                    CAPS[i], strerror(errno));
            return 1;
        }
    }

    /* 5. Ambient-cap exec does not trigger the dynamic linker's secure mode. Scrub the
          environment entries that would otherwise steer loader/NSS/locale behavior. */
    scrub_env();

    /* 6. Exec the real command. */
    execvp(argv[1], argv + 1);
    fprintf(stderr, "caprun: exec %s: %s\n", argv[1], strerror(errno));
    return 1;
}
