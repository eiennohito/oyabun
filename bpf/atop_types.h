/*
 * atop_types.h — the single source of layout truth for the BPF<->Rust interface.
 *
 * Every struct here is written by a BPF program and read by Rust as raw bytes
 * (zerocopy). The Rust mirror in `crates/atop/src/gather/bpf/types.rs` must match
 * field-for-field; both sides assert sizeof() so a drift is a compile error, not a
 * silent misread. Same machine, same endianness — no byte-swapping, ever.
 *
 * Layout rule: 8-byte fields first, then 4, then 2, then bytes, with explicit tail
 * padding so the size is a multiple of 8 and has no implicit holes the two languages
 * could disagree about.
 *
 * Unit rule: raw kernel values cross the boundary unconverted (nanoseconds, pages,
 * raw task state bits). Conversion to atop's display units (clock ticks, bytes, the
 * state char) happens in Rust, where CLK_TCK / page size / the state table live — one
 * place, testable, identical to the /proc path's semantics.
 */
#ifndef ATOP_TYPES_H
#define ATOP_TYPES_H

#ifndef ATOP_TYPES_NO_VMLINUX
/* When compiled into the BPF object, the integer typedefs come from vmlinux.h.
   The Rust side never includes this file — it mirrors the layout by hand. */
typedef unsigned char __u8;
typedef unsigned short __u16;
typedef unsigned int __u32;
typedef unsigned long long __u64;
typedef signed short __s16;
typedef signed long long __s64;
#endif

#define ATOP_COMM_LEN 16 /* TASK_COMM_LEN */

/* proc_event.event values. FREE is the reap (release_task), not exit: a zombie stays a live
   /proc entry until reaped, so removal — and the emit-on-change hash cleanup — keys off free. */
#define ATOP_EVENT_FORK 0
#define ATOP_EVENT_FREE 1

/*
 * One row of the task iterator's output, written once per thread-group leader via
 * bpf_seq_write. Read back in Rust as &[task_info] (a pointer cast over the read
 * buffer). Mirrors /proc/<pid>/stat's fields, but as raw kernel values.
 */
struct atop_task_info {
	__u64 utime_ns;          /* task->utime (nanoseconds) */
	__u64 stime_ns;          /* task->stime (nanoseconds) */
	__u64 start_boottime_ns; /* task->start_boottime — matches /proc field 22 after nsec_to_clock_t */
	__s64 rss_pages;         /* sum of FILE+ANON+SHMEM rss counters (pages; may be transiently <0) */
	__u32 pid;               /* task->tgid (userspace PID) */
	__u32 tgid;              /* task->tgid (== pid for the leaders we emit) */
	__u32 ppid;              /* real_parent->tgid */
	__u32 uid;               /* cred->uid.val (init-userns uid) */
	__u32 flags;             /* task->flags (PF_KTHREAD etc.) */
	__u32 state;             /* task->__state (raw run-state bits) */
	__u32 exit_state;        /* task->exit_state (EXIT_ZOMBIE/EXIT_DEAD) */
	__u32 nr_threads;        /* signal->nr_threads */
	__s16 prio;              /* task->prio (priority col = prio - 100) */
	__s16 static_prio;       /* task->static_prio (nice = static_prio - 120) */
	__u8  comm[ATOP_COMM_LEN];
	__u8  _pad[4];           /* explicit tail pad: size is a multiple of 8 */
};

_Static_assert(sizeof(struct atop_task_info) == 88, "atop_task_info layout drift");

/*
 * A birth/reap record pushed to the ringbuf by the fork/free tp_btf programs.
 * Only thread-group-leader events are emitted (a new process / a process reap),
 * never bare thread create/free.
 */
struct atop_proc_event {
	__u32 pid;        /* the process (tgid) that was born / reaped */
	__u8  event;      /* ATOP_EVENT_FORK | ATOP_EVENT_FREE */
	__u8  _pad[3];
};

_Static_assert(sizeof(struct atop_proc_event) == 8, "atop_proc_event layout drift");

#endif /* ATOP_TYPES_H */
