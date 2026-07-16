// atop BPF programs — privileged observation.
//
// Three programs, one object:
//   atop_task_iter   iter/task   — per-process stat snapshot, emit-on-change (delta stream)
//   atop_sched_fork  tp_btf      — process-birth events (short-lived pairing)
//   atop_sched_free  tp_btf      — process-reap events (row removal + hash-map cleanup)
//
// CO-RE: all kernel reads go through BPF_CORE_READ, relocated at load time against the
// running kernel's BTF. The committed atop.bpf.o therefore loads on any kernel whose
// task_struct keeps these (stable-ABI) fields, without recompilation.
//
// Emit-on-change: the iterator still walks every task each cycle (there is no kernel signal
// for "a sleeping process's rss/utime moved", so polling is the only way), but it writes a row
// only when the process's observable state *changed* since last cycle. So the seq_file stream
// userspace drains is O(changed), not O(all) — the walk cost stays, the transfer + parse cost
// collapses on an idle box.
//
// The change key is a 64-bit hash of the **hot** fields — the ones that move without notice:
// thread-group CPU time, run state, resident pages (reclaim/swap edit while sleeping), and the
// parent (reparenting). An unchanged hot hash bails *before* reading cold fields (uid, nice,
// thread count, start time, comm). Cold fields refresh only on an emit or a forced resync.
//
// CPU time is the thread-group total (`signal->{utime,stime}` + every live thread's), matching
// `/proc/<pid>/stat`'s `thread_group_cputime()`. The leader's per-thread `task->utime` alone
// would miss non-leader-thread activity in both the hash and the emitted value. The group walk
// via `bpf_loop` costs O(nr_threads) per leader; single-threaded processes take a fast path.
//
// Leader filter: the iterator and both tracepoints fire per *thread*; we act only on
// thread-group leaders (kernel pid == tgid) so the output is processes, matching the /proc
// enumeration the unprivileged path produces.

#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_tracing.h>

#define ATOP_TYPES_NO_VMLINUX
#include "atop_types.h"

char LICENSE[] SEC("license") = "GPL";

// rss counter indices — enum { MM_FILEPAGES, MM_ANONPAGES, MM_SWAPENTS, MM_SHMEMPAGES }
// in the kernel; pinned here as stable ABI. /proc's "rss" excludes SWAPENTS.
#define ATOP_MM_FILEPAGES  0
#define ATOP_MM_ANONPAGES  1
#define ATOP_MM_SHMEMPAGES 3

// Live-leader hash table capacity — also a deliberate cap on this map's locked kernel memory.
// Beyond it an update fails and that PID can no longer dedup: it re-emits every cycle
// (degraded, never wrong). 32768 leaders is far above any realistic live count (and unrelated
// to the like-valued 32-bit pid_max — coincidence, not derivation). Unlike the fd-pool
// overflow, this silent cap is intentionally left unsurfaced for now; revisit if real
// workloads approach it.
#define ATOP_MAX_PIDS 32768

// `ctrl` array indices (mirrored in the Rust source).
#define ATOP_CTRL_EMIT_ALL 0 // userspace→bpf: force a full snapshot this cycle (resync)
#define ATOP_CTRL_DROPS    1 // bpf→userspace: count of dropped event records (ringbuf full)
#define ATOP_CTRL_LEN      2

// FNV-1a 64-bit — fast, good mixing, straight-line (the verifier accepts it without loops).
#define ATOP_FNV_OFFSET 0xcbf29ce484222325ULL
#define ATOP_FNV_PRIME  0x00000100000001b3ULL

// Birth/reap events. 256 KiB holds a large fork storm between cycle drains; an overflow bumps
// the drop counter (which arms a resync) and is otherwise dropped — never corrupts.
struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, 256 * 1024);
} events SEC(".maps");

// Per-leader hot-field hash — the emit-on-change key, keyed by tgid. A map (not iterator
// state) so cycle N+1 sees cycle N's hashes across the per-cycle iterator re-creation. Deleted
// on reap, so a reused PID always re-emits.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, ATOP_MAX_PIDS);
	__type(key, __u32);
	__type(value, __u64);
} last_hash SEC(".maps");

// Control/stats: [EMIT_ALL] forces a full snapshot (userspace writes); [DROPS] counts dropped
// event records (this program increments, userspace reads to detect overflow → arm a resync).
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, ATOP_CTRL_LEN);
	__type(key, __u32);
	__type(value, __u32);
} ctrl SEC(".maps");

static __always_inline __u64 hmix(__u64 h, __u64 x)
{
	return (h ^ x) * ATOP_FNV_PRIME;
}

// Verifier bound for the thread-group walk; beyond it, tail threads' time is omitted
// (bounded-stale, self-healing on the next resync).
#define ATOP_MAX_THREADS 4096

struct group_walk_ctx {
	__u64 utime;
	__u64 stime;
	struct list_head *pos;
	struct list_head *sentinel; // &sig->thread_head — stop when we loop back here
	unsigned long off;          // CO-RE byte offset of thread_node in task_struct
};

// bpf_loop callback: stop at the sentinel (a thread exiting mid-walk shortens the list
// below the snapshotted nr_threads), then container_of → accumulate → advance.
// `list_head.next` is at offset 0 — a plain probe_read avoids the CO-RE relocation the
// verifier rejects on a stack-held pointer.
static int sum_thread_cputime(unsigned int idx, void *data)
{
	struct group_walk_ctx *ctx = data;
	if (ctx->pos == ctx->sentinel)
		return 1; // looped back to head — list shorter than nr_threads
	struct task_struct *t =
		(struct task_struct *)((unsigned long)ctx->pos - ctx->off);
	ctx->utime += BPF_CORE_READ(t, utime);
	ctx->stime += BPF_CORE_READ(t, stime);
	struct list_head *next;
	bpf_probe_read_kernel(&next, sizeof(next), ctx->pos);
	ctx->pos = next;
	return 0;
}

// Thread-group CPU time matching `thread_group_cputime()`: dead-thread accumulation from
// signal + live-thread walk via signal->thread_head.  The leader is a member of the
// thread_head list (linked at fork by copy_signal), so nr_threads iterations from
// thread_head.next cover every live thread including the leader.  sig->{utime,stime}
// (accumulated unconditionally in __exit_signal for every exiting thread) must always be
// added, even for single-threaded processes.
static __always_inline void group_cputime(struct task_struct *leader,
					  __u64 *out_ut, __u64 *out_st)
{
	struct signal_struct *sig = BPF_CORE_READ(leader, signal);

	// Dead-thread accumulation — unconditional (a process that was multi-threaded and
	// dropped back to nr_threads=1 still has exited threads' time here).
	*out_ut = BPF_CORE_READ(sig, utime);
	*out_st = BPF_CORE_READ(sig, stime);

	__u32 nr = BPF_CORE_READ(sig, nr_threads);
	if (nr <= 1) {
		// Single-threaded fast path: only the leader's own time to add.
		*out_ut += BPF_CORE_READ(leader, utime);
		*out_st += BPF_CORE_READ(leader, stime);
		return;
	}

	// Compute the sentinel address: sig + CO-RE offset of thread_head.
	// CO-RE relocation through __CORE_RELO on signal_struct would need a typed
	// expression; use the same raw-offset trick as the thread_node container_of.
	struct list_head *sentinel = (struct list_head *)(
		(unsigned long)sig + __CORE_RELO(sig, thread_head, BYTE_OFFSET));

	struct group_walk_ctx ctx = {
		.utime    = *out_ut,
		.stime    = *out_st,
		.pos      = BPF_CORE_READ(sig, thread_head.next),
		.sentinel = sentinel,
		.off      = __CORE_RELO(leader, thread_node, BYTE_OFFSET),
	};
	if (nr > ATOP_MAX_THREADS)
		nr = ATOP_MAX_THREADS;
	bpf_loop(nr, sum_thread_cputime, &ctx, 0);
	*out_ut = ctx.utime;
	*out_st = ctx.stime;
}

static __always_inline __s64 read_rss(struct mm_struct *mm)
{
	if (!mm)
		return 0; // kernel thread / reaped leader — no address space
	// percpu_counter.count is the batched approximate sum — adequate for a TUI, and the only
	// value reachable without summing per-CPU deltas.
	__s64 fp = BPF_CORE_READ(mm, rss_stat[ATOP_MM_FILEPAGES].count);
	__s64 an = BPF_CORE_READ(mm, rss_stat[ATOP_MM_ANONPAGES].count);
	__s64 sh = BPF_CORE_READ(mm, rss_stat[ATOP_MM_SHMEMPAGES].count);
	return fp + an + sh;
}

static __always_inline int emit_all_set(void)
{
	__u32 k = ATOP_CTRL_EMIT_ALL;
	__u32 *v = bpf_map_lookup_elem(&ctrl, &k);
	return v && *v;
}

SEC("iter/task")
int atop_task_iter(struct bpf_iter__task *ctx)
{
	struct task_struct *task = ctx->task;
	if (!task)
		return 0; // terminal call — the iterator is done

	__u32 pid = BPF_CORE_READ(task, pid);
	__u32 tgid = BPF_CORE_READ(task, tgid);
	if (pid != tgid)
		return 0; // non-leader thread — emit processes only

	// Hot fields (thread-group CPU time, state, rss, ppid): hash for emit-on-change.
	__u64 utime, stime;
	group_cputime(task, &utime, &stime);
	__u32 state = BPF_CORE_READ(task, __state);
	__u32 exit_state = BPF_CORE_READ(task, exit_state);
	__u32 ppid = BPF_CORE_READ(task, real_parent, tgid);
	__s64 rss = read_rss(BPF_CORE_READ(task, mm));

	__u64 h = ATOP_FNV_OFFSET;
	h = hmix(h, utime);
	h = hmix(h, stime);
	h = hmix(h, (__u64)rss);
	h = hmix(h, ((__u64)state << 32) | exit_state);
	h = hmix(h, (__u64)ppid);

	__u64 *prev = bpf_map_lookup_elem(&last_hash, &tgid);
	if (!emit_all_set() && prev && *prev == h)
		return 0; // unchanged — bail before the cold reads / seq_write

	// Changed, new, or forced: read the cold fields and emit the full row.
	struct atop_task_info info = {};
	info.pid = tgid;
	info.tgid = tgid;
	info.ppid = ppid;
	info.utime_ns = utime;
	info.stime_ns = stime;
	info.state = state;
	info.exit_state = exit_state;
	info.rss_pages = rss;
	info.uid = BPF_CORE_READ(task, cred, uid.val);
	info.start_boottime_ns = BPF_CORE_READ(task, start_boottime);
	info.flags = BPF_CORE_READ(task, flags);
	info.nr_threads = BPF_CORE_READ(task, signal, nr_threads);
	info.prio = BPF_CORE_READ(task, prio);
	info.static_prio = BPF_CORE_READ(task, static_prio);
	BPF_CORE_READ_STR_INTO(&info.comm, task, comm);

	bpf_seq_write(ctx->meta->seq, &info, sizeof(info));
	bpf_map_update_elem(&last_hash, &tgid, &h, BPF_ANY);
	return 0;
}

static __always_inline void bump_drops(void)
{
	__u32 k = ATOP_CTRL_DROPS;
	__u32 *d = bpf_map_lookup_elem(&ctrl, &k);
	if (d)
		__sync_fetch_and_add(d, 1);
}

static __always_inline void emit_event(__u32 pid, __u8 event)
{
	struct atop_proc_event *e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
	if (!e) {
		bump_drops(); // ringbuf full — drop, but arm a resync via the counter
		return;
	}
	__builtin_memset(e, 0, sizeof(*e));
	e->pid = pid;
	e->event = event;
	bpf_ringbuf_submit(e, 0);
}

SEC("tp_btf/sched_process_fork")
int BPF_PROG(atop_sched_fork, struct task_struct *parent, struct task_struct *child)
{
	__u32 pid = BPF_CORE_READ(child, pid);
	__u32 tgid = BPF_CORE_READ(child, tgid);
	if (pid == tgid) // a new process leader, not a new thread within one
		emit_event(tgid, ATOP_EVENT_FORK);
	return 0;
}

// Reap (release_task), not exit: a zombie is still a live /proc entry the walk keeps showing as
// `Z`, so removal must wait for the actual free. Deleting the hash entry here also makes a
// reused PID re-emit, and frees the entry exactly when the PID becomes reusable.
SEC("tp_btf/sched_process_free")
int BPF_PROG(atop_sched_free, struct task_struct *p)
{
	__u32 pid = BPF_CORE_READ(p, pid);
	__u32 tgid = BPF_CORE_READ(p, tgid);
	if (pid == tgid) {
		emit_event(tgid, ATOP_EVENT_FREE);
		bpf_map_delete_elem(&last_hash, &tgid);
	}
	return 0;
}
