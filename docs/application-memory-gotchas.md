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

If one live process cannot provide proportional accounting, only that process falls back to its
resident set, and every row whose total includes it is marked so. The marker is what keeps the
mixed sum honest: the alternative, dropping a whole row to resident addition, makes one
unreadable helper distort an entire application. A process with no resident memory contributes
zero and needs no marker: a zombie or concurrently disappearing process has no address-space
contribution.

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

Every row uses proportional set size (a collapsed row sums it over what it hides), with a
per-process resident-set fallback when a live non-empty process cannot be read. One column must
not mix measures under one header, and a collapsed process row and a folded group over the same
set must agree.

Reading proportional data for every process shown or hidden under the viewport every cycle is the
dominant interactive cost on a real desktop — a browser is exactly the large, many-mapping
process this walk is most expensive on. The value is therefore estimated, not recomputed, and the
enabling observation is that a member's shared-vs-private *split* changes far more slowly than its
resident *size*. Resident growth is overwhelmingly private (heap), and resident size is read for
free every cycle. So each member's split is walked once and cached; every cycle its proportional
set size is re-derived by attributing the resident change since that walk to private pages and
recombining with the cached shared share. This is exact for private growth — the common case — and
inaccurate only while the *shared mapping set* itself has changed since the last walk.

The expensive walk then runs only in two situations: to bootstrap a member with no cached split
yet, and, on a slow staggered refresh, to re-establish the split and so catch shared-set drift —
the one thing the free resident reading cannot reveal. Resident movement is deliberately not a
re-walk trigger: a moving resident set is exactly what the estimate handles for free, and
triggering on it would make a busy application re-walk every cycle. Walks are bounded by a fixed
per-cycle budget and prioritized largest-member-first, so newly folding a large application
settles over a few cycles instead of spiking one, and a settled desktop walks essentially
nothing. The staggered refresh is spread by PID rather than by a fixed age, so the members of a
group folded together do not all come due on the same later cycle and walk as one herd.

Which processes get walked is a view concern (viewport-only candidates, gated on change, charged
to the UI's frame budget); see *Memory: proportional set size everywhere* in `ARCHITECTURE.md`.
