# Application memory accounting gotchas

An application's memory does not have one kernel-defined total. Different counters answer
different questions, and substituting one for another can produce errors larger than the
application itself.

## Resident-set totals double-count sharing

Adding each process's resident set counts a shared physical page once per process that maps it.
Multi-process browsers and Electron applications share large code and data mappings, so their
sum can be more than twice the application's resident physical footprint. This total is useful
for inspecting individual processes, but not for presenting one application total.

## Proportional set size is the useful application total

Proportional set size divides every shared resident page among all processes mapping it. Adding
it across an application's members produces an additive attribution: private pages count fully,
intra-application sharing recombines toward one page, and pages shared outside the application
remain proportionally charged.

A physical-page union is a useful validation baseline, but it is not suitable for interactive
sampling. It requires elevated access, scanning every resident mapping, and fully charges a page
to each application that maps it. Proportional accounting tracks that baseline closely while
remaining meaningful when comparing or summing applications.

If one live member cannot provide proportional accounting, the entire application total must
fall back to resident-set addition; mixing the two units would make the result incoherent. A
member with no resident memory is the exception: a zombie or concurrently disappearing process
has no address-space contribution, so its unreadable proportional record safely contributes
zero.

## Cgroup memory is charged memory, not resident footprint

Cgroup memory includes more than pages currently mapped by the application's processes. It can
include filesystem cache retained after mappings close, kernel allocations, sockets, and
descendant charges. Conversely, a shared page may be charged to the cgroup that faulted it even
while processes in another cgroup map it.

The difference is workload-dependent rather than a small correction. A downloader or game
launcher can retain tens of gigabytes of charged file cache while its processes map only a few
gigabytes. Other applications can report less charged memory than their mapped physical-page
union. Cgroup memory is valuable as a separate resource-control diagnostic, but it must never be
presented as application resident memory.

## Sampling policy

Collapsed application rows use proportional-set-size addition, with whole-group resident-set
addition only when a live non-empty member cannot be read. Sampling is limited to collapsed rows
in the visible viewport, and stays outside rendering.

Reading proportional data for every member of every visible folded group every cycle is the
dominant interactive cost on a real desktop — a browser is exactly the large, many-mapping
process this walk is most expensive on. Application memory changes slowly, so it is change-gated
rather than recomputed: each member's proportional value is cached and reused until its resident
set (already known every cycle for free) moves enough to matter, where "enough" is measured
absolutely against host memory, not as a fraction of the process. Candidates are refreshed
highest-change-first under a fixed per-cycle read budget, so newly folding a large application
converges over a few cycles instead of spiking one, and a settled desktop reads essentially
nothing. A slow periodic refresh backstops the one thing the resident-set gate cannot see: a
member's proportional share shifting when a *shared* page is mapped or unmapped elsewhere, with
no change to its own resident set.
