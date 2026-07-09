# Semantic Process Grouping

## Problem

The process tree is purely structural (`ppid` linkage). Multi-process applications and
runtime scopes can produce large subtrees where the individual worker rows are less useful
than the semantic unit they form: Chromium/Electron apps, Flatpak sandboxes, systemd/cgroup
services, containers, pods, interpreter launchers, and sandbox/jail scopes.

The expanded tree remains naive and structural. Grouping affects only collapsed roots:

1. **Auto-collapse**: classify a process once (per PID lifetime), then start its semantic root
   collapsed.
2. **Smarter collapsed display**: show a group label when the collapsed row has a group fact.

## Detection Lifecycle

Classification is per PID, generation-evicted. It does **not** need incarnation keying
(`start_time`): per the PID reuse model (ARCHITECTURE.md), death eviction is ≤1 cycle and
reuse requires a full `pid_max` wrap, so any reused PID arrives at a clean slate with no stale
classification to collide with.

Startup is one large first-seen event: every process visible on the first sample enters the
same settling lifecycle as a process born later.

Grouping is not recalled every cycle. Each visible PID has sidecar state:

```rust
enum GroupState {
    Settling { first_seen_gen: u64 },
    NoGroup,
    Group { fact: GroupFact },
}
```

The sidecar is a `PidMap<GroupState>` owned by `Gatherer`, generation-evicted when a PID is
absent for a cycle (the same `seen_gen` / `last_seen_gen` pattern used by `PidSlot`).

During the settle window, rules may nominate and resolve candidates. If a rule resolves, the
fact is cached as `Group`. If the settle window expires without resolution, the PID becomes
`NoGroup`. Steady-state grouping does not re-run; later child births update tree metrics and
descendant counts only, not group identity.

Grouping runs after `tree::build` and `tree::aggregate`, because rules need tree links,
subtree sizes, and resolved cmdlines.

## Rule Model

Rules are behavior, not an enum. The classifier stores facts as plain data and dispatches
through a trait registry:

```rust
trait GroupRule {
    fn id(&self) -> &'static str;
    fn nominate(&self, tree: &TreeView<'_>, pid_idx: usize, out: &mut Vec<GroupCandidate>);
    fn resolve(&self, meta: &GroupMetaView<'_>, candidate: &GroupCandidate) -> Option<GroupFact>;
}
```

`GroupCandidate` is a plain nominated root plus the rule id. `GroupFact` is plain render-facing
data: rule id and `GroupLabel`. The stored fact is not a trait object.

This keeps v1 focused while leaving room for future rule families:

- Chromium/Electron shared-process fans.
- Flatpak sandboxes via `/proc/<pid>/root/.flatpak-info`.
- systemd/cgroup service scopes.
- containers and Kubernetes pods.
- interpreter launchers such as Python, Node, JVM, and dotnet.
- sandbox/jail scopes and user-defined wrappers.

## UI State

Collapse state is transient UI state keyed by plain PID, generation-evicted the same way as
classification:

- Manual collapsed roots stay user-controlled.
- Auto-group roots are inserted once when first classified.
- User expansion records PID suppression so the same visible PID does not re-collapse.
- State is evicted as soon as a PID is absent from a gathered frame. No incarnation keying is
  needed: the PID reuse model (ARCHITECTURE.md) guarantees that reuse arrives ≥2 cycles after
  death, and eviction runs every cycle, so no stale collapse state survives for a reused PID to
  inherit.

Both the classifier and collapse state use the same generation-eviction model — neither needs
`start_time`. Signal safety (`kill_verified`) is the sole consumer of incarnation identity.

Render-facing accessors on `Gatherer`/`App` expose group facts by PID. The command cell uses
`GroupLabel` for collapsed group roots. Expanded rows keep the existing cmdline/comm behavior.

## Phase 1 Rule: Chromium/Electron

The first rule is intentionally narrow:

- Nominate roots with child fan-out and shared `comm`.
- Resolve only when descendants include Chromium-style `--type=` cmdline arguments.
- Label fallback is the current row display name (`cmdline` when present, else `comm`).

This covers Chrome, Chromium, Brave, Edge, and Electron applications such as VS Code, Slack,
Spotify, and Discord. Richer label derivation is deferred.

## Deferred Work

- Exe basename, cmdline argument, and `.desktop` label tiers.
- Flatpak precision from `.flatpak-info`.
- systemd/cgroup, container, pod, interpreter, and sandbox/jail rules.
- PSS memory accounting from `smaps_rollup` for collapsed groups.
- Persistent user grouping preferences and overrides.

## Test Plan

- Doc check: this file reflects generation-evicted lifecycle, startup-as-first-seen behavior,
  trait-rule design, `PidMap` sidecar storage, and future rule families.
- Classifier lifecycle: startup first-seen classification, settle-to-`NoGroup`, vanished PID
  generation-eviction, reused PID (after eviction) starts fresh with no stale state.
- Chromium rule synthetic trees:
  - shared-`comm` fan with `--type=` descendants becomes a group;
  - fan without `--type=` does not auto-group;
  - late child after classification does not re-run classification.
- App behavior:
  - classified auto-group starts collapsed;
  - user expansion suppresses re-collapse while that PID remains visible;
  - absent PIDs clear stale collapse/suppression state.
- Render assertion:
  - collapsed group root uses group label when present;
  - expanded tree remains structurally naive and unchanged.
