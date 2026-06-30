/*
 * Spike: multi-round fill/reap with read-slot reuse, matching atop's
 * collect() loop. The question: when a read_slot is freed in reap round N
 * and reused in fill round N+1, could the old data in the pad slot be
 * misattributed to the new chain?
 *
 * Scenario:
 *   - Small pad (4 slots) forces many fill/reap rounds per "collect."
 *   - Real PIDs and dead probe candidates are interleaved.
 *   - Each round reuses slots from the previous round.
 *   - PID cross-check on every read: parsed PID vs expected PID.
 *
 * Build: cc -O2 -o stale_fd_link stale_fd_link.c -luring
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <liburing.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define POOL_CAP    128
#define PAD_SLOTS   4       /* tiny: forces multi-round fill/reap */
#define SLOT_SIZE   1024
#define PAD_SIZE    (PAD_SLOTS * SLOT_SIZE)
#define RING_SZ     256

#define OP_OPEN 0
#define OP_READ 1
static inline __u64 pk(int fixed, int op) {
    return ((__u64)(unsigned)fixed << 1) | (unsigned)op;
}

struct ctx {
    __u32 pid;
    int   pid_idx;
    int   read_slot;
    int   pending;
    int   open_failed;
    int   read_ok;
    int   open_res;
    int   read_res;
    __u32 stat_len;
};

static char pad[PAD_SIZE] __attribute__((aligned(4096)));
static struct ctx ctxs[POOL_CAP];
static char paths[POOL_CAP][64];

/* Held map: PID → fixed_idx. */
struct held { __u32 pid; int fixed; };
static struct held hmap[POOL_CAP];
static int hn;
static int held_find(__u32 pid) {
    for (int i = 0; i < hn; i++) if (hmap[i].pid == pid) return i;
    return -1;
}
static void held_add(__u32 pid, int fixed) {
    hmap[hn++] = (struct held){pid, fixed};
}
static void held_remove(int i) { hmap[i] = hmap[--hn]; }

static int ff[POOL_CAP], ff_top;   /* free fixed-idx stack */
static int rs[PAD_SLOTS], rs_top;  /* free read-slot stack */

/* Parse PID from stat line: "<pid> (..." */
static __u32 parse_stat_pid(const char *d, int len) {
    __u32 p = 0;
    for (int i = 0; i < len && d[i] != ' '; i++) {
        if (d[i] < '0' || d[i] > '9') return 0;
        p = p * 10 + (d[i] - '0');
    }
    return p;
}

/*
 * One collect cycle: iterate over pids[], fill SQ in rounds,
 * reap+process between rounds. Exactly atop's collect() loop.
 * Returns phantom count.
 */
static int collect(struct io_uring *ring, __u32 *pids, int npids,
                   int *out_reads) {
    int next = 0;
    int outstanding = 0;
    int phantoms = 0;

    while (next < npids || outstanding > 0) {
        /* --- fill --- */
        int filled = 0;
        struct io_uring_sqe *sqe;
        while (next < npids) {
            /* Need room for 2 SQEs (new chain). */
            if (io_uring_sq_space_left(ring) < 2) break;
            /* Need a read slot. */
            if (rs_top == 0) break;

            __u32 pid = pids[next];
            int hi = held_find(pid);

            if (hi >= 0) {
                /* Cached: single ReadFixed. */
                int slot = rs[--rs_top];
                int fixed = hmap[hi].fixed;

                sqe = io_uring_get_sqe(ring);
                io_uring_prep_read_fixed(sqe, fixed,
                    pad + slot * SLOT_SIZE, SLOT_SIZE, 0, 0);
                sqe->flags |= IOSQE_FIXED_FILE;
                sqe->user_data = pk(fixed, OP_READ);

                ctxs[fixed] = (struct ctx){
                    .pid = pid, .pid_idx = next, .read_slot = slot,
                    .pending = 1
                };
                filled += 1;
            } else if (ff_top > 0) {
                /* New: OpenAt → ReadFixed chain. */
                int slot = rs[--rs_top];
                int fixed = ff[--ff_top];

                snprintf(paths[fixed], 64, "/proc/%u/stat", pid);

                sqe = io_uring_get_sqe(ring);
                io_uring_prep_openat_direct(sqe, AT_FDCWD, paths[fixed],
                                            O_RDONLY, 0, fixed);
                sqe->user_data = pk(fixed, OP_OPEN);
                sqe->flags |= IOSQE_IO_LINK;

                sqe = io_uring_get_sqe(ring);
                io_uring_prep_read_fixed(sqe, fixed,
                    pad + slot * SLOT_SIZE, SLOT_SIZE, 0, 0);
                sqe->flags |= IOSQE_FIXED_FILE;
                sqe->user_data = pk(fixed, OP_READ);

                held_add(pid, fixed);
                ctxs[fixed] = (struct ctx){
                    .pid = pid, .pid_idx = next, .read_slot = slot,
                    .pending = 2
                };
                filled += 2;
            } else {
                /* Pool exhausted — skip (overflow). */
            }
            next++;
        }

        outstanding += filled;
        if (outstanding == 0) break;

        /* --- submit + wait for all --- */
        int ret;
        do {
            ret = io_uring_submit_and_wait(ring, outstanding);
        } while (ret == -EINTR);
        if (ret < 0) {
            fprintf(stderr, "submit_and_wait: %d\n", ret);
            break;
        }

        /* --- reap --- */
        int done[POOL_CAP], nd = 0;
        int reaped = 0;
        struct io_uring_cqe *cqe;
        unsigned head;
        io_uring_for_each_cqe(ring, head, cqe) {
            reaped++;
            int fixed = (int)(cqe->user_data >> 1);
            int op = (int)(cqe->user_data & 1);
            if (fixed < 0 || fixed >= POOL_CAP) continue;
            if (op == OP_OPEN) {
                ctxs[fixed].open_res = cqe->res;
                if (cqe->res < 0) ctxs[fixed].open_failed = 1;
            } else {
                ctxs[fixed].read_res = cqe->res;
                if (cqe->res > 0) {
                    ctxs[fixed].read_ok = 1;
                    ctxs[fixed].stat_len = (unsigned)cqe->res;
                }
            }
            if (--ctxs[fixed].pending == 0)
                done[nd++] = fixed;
        }
        io_uring_cq_advance(ring, reaped);
        outstanding -= reaped;

        /* --- process completed chains --- */
        for (int d = 0; d < nd; d++) {
            int fixed = done[d];
            struct ctx *c = &ctxs[fixed];

            if (c->read_ok) {
                (*out_reads)++;
                char *data = pad + c->read_slot * SLOT_SIZE;
                __u32 sp = parse_stat_pid(data, (int)c->stat_len);

                if (sp != 0 && sp != c->pid) {
                    phantoms++;
                    fprintf(stderr,
                        "!! PHANTOM expected=%u parsed=%u fixed=%d "
                        "slot=%d open=%d(%d) read=%d\n"
                        "   data: %.80s\n",
                        c->pid, sp, fixed, c->read_slot,
                        c->open_res, c->open_failed, c->read_res, data);
                }
                /* Free read slot. Keep held entry. */
                rs[rs_top++] = c->read_slot;
            } else if (c->open_failed) {
                /* Open failed: free both. No register_files_update. */
                int hi = held_find(c->pid);
                if (hi >= 0) held_remove(hi);
                ff[ff_top++] = fixed;
                rs[rs_top++] = c->read_slot;
            } else {
                /* Read failed (ESRCH). Clear slot. */
                int fds[1] = {-1};
                io_uring_register_files_update(ring, fixed, fds, 1);
                int hi = held_find(c->pid);
                if (hi >= 0) held_remove(hi);
                ff[ff_top++] = fixed;
                rs[rs_top++] = c->read_slot;
            }
        }
    }

    return phantoms;
}

int main(void) {
    struct io_uring ring;
    struct io_uring_params params = {0};
    params.flags = IORING_SETUP_SINGLE_ISSUER |
                   IORING_SETUP_DEFER_TASKRUN |
                   IORING_SETUP_COOP_TASKRUN;
    if (io_uring_queue_init_params(RING_SZ, &ring, &params) < 0) {
        params.flags = IORING_SETUP_COOP_TASKRUN;
        if (io_uring_queue_init_params(RING_SZ, &ring, &params) < 0) {
            if (io_uring_queue_init(RING_SZ, &ring, 0) < 0) {
                fprintf(stderr, "ring init failed\n"); return 1;
            }
        }
    }
    fprintf(stderr, "ring flags: 0x%x\n", params.flags);

    if (io_uring_register_files_sparse(&ring, POOL_CAP) < 0) return 1;
    struct iovec iov = { .iov_base = pad, .iov_len = PAD_SIZE };
    if (io_uring_register_buffers(&ring, &iov, 1) < 0) return 1;

    /* Collect real PIDs. */
    __u32 real_pids[40];
    int n_real = 0;
    real_pids[n_real++] = getpid();
    for (__u32 p = 1; p <= 50 && n_real < 40; p++) {
        char chk[64];
        snprintf(chk, 64, "/proc/%u/stat", p);
        if (access(chk, F_OK) == 0) real_pids[n_real++] = p;
    }
    fprintf(stderr, "real pids: %d, pad slots: %d (forces %d+ fill/reap rounds)\n",
            n_real, PAD_SLOTS, (n_real + PAD_SLOTS - 1) / PAD_SLOTS);

    int total_phantoms = 0, total_reads = 0;

    for (int cycle = 0; cycle < 2000; cycle++) {
        /* Reset pools. */
        ff_top = 0;
        for (int i = POOL_CAP - 1; i >= 0; i--) ff[ff_top++] = i;
        rs_top = 0;
        for (int i = PAD_SLOTS - 1; i >= 0; i--) rs[rs_top++] = i;
        hn = 0;

        /* Build the PID list: real PIDs + dead probe candidates. */
        __u32 pids[80];
        int np = 0;

        /* Spawn children, kill them — creates dead PIDs that will be
         * interleaved with real PIDs in the fill/reap rounds. */
        pid_t kids[16];
        int nk = 0;
        for (int i = 0; i < 16; i++) {
            pid_t p = fork();
            if (p == 0) _exit(0);
            if (p > 0) kids[nk++] = p;
        }
        /* Also spawn a few that stay alive briefly. */
        pid_t alive[4];
        int na = 0;
        for (int i = 0; i < 4; i++) {
            pid_t p = fork();
            if (p == 0) { usleep(5000); _exit(0); }
            if (p > 0) alive[na++] = p;
        }

        /* Reap the dead ones. */
        for (int i = 0; i < nk; i++) waitpid(kids[i], NULL, 0);

        /* Interleave real PIDs and probe candidates. */
        for (int i = 0; i < n_real && np < 80; i++)
            pids[np++] = real_pids[i];
        for (int i = 0; i < nk && np < 80; i++)
            pids[np++] = (__u32)kids[i];       /* dead */
        for (int i = 0; i < na && np < 80; i++)
            pids[np++] = (__u32)alive[i];      /* alive (briefly) */

        /* Sort (atop requires sorted). */
        for (int i = 0; i < np - 1; i++)
            for (int j = i + 1; j < np; j++)
                if (pids[i] > pids[j]) {
                    __u32 t = pids[i]; pids[i] = pids[j]; pids[j] = t;
                }

        /* Collect with tiny pad — many fill/reap rounds per cycle. */
        int ph = collect(&ring, pids, np, &total_reads);
        total_phantoms += ph;

        /* Reap the alive ones. */
        for (int i = 0; i < na; i++) waitpid(alive[i], NULL, 0);

        if (cycle < 5 || cycle % 200 == 0 || ph > 0) {
            fprintf(stderr, "  cycle=%4d pids=%d reads=%d phantoms=%d\n",
                    cycle, np, total_reads, total_phantoms);
        }
    }

    fprintf(stderr, "\n=== %d phantoms / %d reads in 2000 cycles ===\n",
            total_phantoms, total_reads);
    io_uring_queue_exit(&ring);
    return total_phantoms > 0 ? 1 : 0;
}
