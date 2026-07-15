# Deleted / replaced binary & library detection — goal

Status: **goal** — why and invariants only, no design and no actions.
The current implementation lives in `ARCHITECTURE.md`; this doc is the target that design serves and must not regress.
A future plan (the *how*) belongs in `plans/`.

## What it answers for the user

A running process can be executing code that no longer exists on disk.
Its executable, or a shared library it mapped, was unlinked or replaced out from under it — the everyday cause is a package upgrade while the process keeps running.
That process is now running stale code: unpatched, possibly carrying a fixed vulnerability, and not what a fresh launch would produce.
The goal is to surface, at a glance, that a process is running outdated on-disk code and should be restarted.
An operator doing post-upgrade hygiene, or chasing a security advisory, should see which processes still need bouncing without reaching for separate tooling.

## Why it is a first-class signal

It is a security- and correctness-relevant fact about a process, not decoration.
It belongs to the "richer monitoring over UI fanciness" goal: a signal the reader cannot easily get elsewhere, made visible in the one place they are already looking at processes.
It earns display space because acting on it (a restart) is cheap and the cost of ignoring it (running vulnerable or inconsistent code indefinitely) is high.

## The two transitions differ in kind

This asymmetry is imposed by the operating system, not chosen by us, and any solution must honor it.

An **executable** deletion is *absorbing*: once the kernel marks the running image gone, that incarnation can never return to a good state, so the fact is permanent for the life of the process.
The truth, once established, never needs re-checking.

A **library** deletion is *transient*: a process can unmap a replaced library and map its successor, so the condition can appear and later clear.
The signal must therefore be able to both appear and retract over a process's life; it is not something that can be latched once and forgotten.

## Cost and scale invariants

These are hard constraints, inherited from the project's first goal (minimal CPU; work tracks change, not population).

Steady state must cost approximately nothing.
On a system where nothing has been replaced, the feature must do no meaningful ongoing work, and its idle cost must not scale with the number of processes.

The check must not impose per-cycle work proportional to the process count or to the size of processes' address spaces.
Whatever observes a transient library replacement must be bounded so that total cost stays independent of population, however large.

Detection latency is generous.
This is hygiene and security signalling on a human timescale — an operator deciding what to restart after an upgrade — not a real-time metric.
Seconds, even tens of seconds, between a replacement and the flag appearing is acceptable.
Correctness matters more than promptness: a briefly missed-then-corrected state is fine, a persistently wrong one is not.

Value must justify cost.
It is a visual flag, not a number read continuously, so its budget is small and it must never crowd out the core gather.

## Coverage

Both the executable and mapped libraries count, wherever their backing files live — system paths, per-application bundles, container and flatpak roots, content-addressed stores, anywhere a mapping's file can be unlinked.
The goal is not tied to a fixed set of locations.

Unprivileged operation is the baseline.
A privileged mode may detect the same condition more cheaply or more precisely, but the feature must exist — possibly at higher cost or coarser latency — with no elevated privilege.

## Success criteria

A process running a deleted or replaced executable or library is visibly flagged within the latency budget.
The flag clears when a transient library condition resolves — no false permanence.
On a system with no recent replacements, ongoing cost is negligible and independent of process count.

## Non-goals

Not a real-time notification of the unlink or replace event; catching the instant of replacement is unnecessary.
Not an inventory of which file changed, nor a diff of old against new — only that the running process is stale.
Not a replacement for the OS or package manager's own restart-advisory tooling; this is a lightweight in-context hint, not a report.
Not tied to any one detection mechanism — the goal fixes cost and correctness and leaves mechanism to the design.
