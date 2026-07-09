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

During the settle window, each rule first prepares a bounded, cycle-wide index and then detects
individual roots from that index. Preparation is O(processes) per rule; per-root detection may
not walk a subtree. If a rule resolves, the best display fact and strongest trusted collapse
rank are cached separately. If the settle window expires without resolution, the PID becomes
`NoGroup`. Steady-state grouping does not re-run; later child births update tree metrics and
descendant counts only, not group identity.

Grouping runs after `tree::build` and `tree::aggregate`, because rules need tree links,
subtree sizes, and resolved cmdlines.

## Rule Model

Rules are behavior, not an enum. The classifier stores facts as plain data and dispatches
through a trait registry:

```rust
trait GroupRule {
    fn prepare(&mut self, tree: &TreeView<'_>, meta: &GroupMetaView<'_>);
    fn detect(&self, tree: &TreeView<'_>, meta: &GroupMetaView<'_>, pid_idx: usize)
        -> Option<GroupFact>;
}
```

`GroupFact` is plain render-facing data: rule id, `GroupLabel`, rank, and evidence provenance.
The stored fact is not a trait object. Cgroup rules normalize identities and compute subtree
coherence bottom-up once; Chromium propagates its type marker once; runtime rules index
normalized entrypoints by preorder interval.

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
- Only facts backed by a coherent kernel cgroup boundary may start collapsed. Process-controlled
  `comm`, argv, and root-filesystem metadata may improve labels but never authorize hiding rows.
- Canonical auto roots are selected in one preorder pass. The strongest trusted ancestor rank is
  carried down the traversal, avoiding a separate ancestor walk per classified process.
- User expansion records PID suppression so the same visible PID does not re-collapse.
- State is evicted as soon as a PID is absent from a gathered frame. No incarnation keying is
  needed: the PID reuse model (ARCHITECTURE.md) guarantees that reuse arrives ≥2 cycles after
  death, and eviction runs every cycle, so no stale collapse state survives for a reused PID to
  inherit.

Both the classifier and collapse state use the same generation-eviction model — neither needs
`start_time`. Signal safety (`kill_verified`) is the sole consumer of incarnation identity.

Render-facing accessors on `Gatherer`/`App` expose group facts by PID. The command cell uses
`GroupLabel` for collapsed group roots. Expanded rows keep the existing cmdline/comm behavior.

## Implemented Detectors

All current detectors use only data already available to the classifier: tree links, subtree
size, `comm`, and resolved cmdline. Cgroup paths are the next major metadata source because
they carry service, scope, container, session, and Flatpak provenance that process names alone
cannot distinguish.

### Chromium/Electron

The first rule is intentionally narrow:

- Nominate roots with child fan-out and shared `comm`.
- Resolve only when descendants include Chromium-style `--type=` cmdline arguments.
- Label fallback is the current row display name (`cmdline` when present, else `comm`).

This covers Chrome, Chromium, Brave, Edge, and Electron applications such as VS Code, Slack,
Spotify, and Discord. Richer label derivation is deferred.

### Runtime Worker Pools

This rule covers interpreter-launched process pools where a parent fans out multiple same-runtime
workers for the same entrypoint:

- Nominate Python, Node, JVM, and dotnet roots with enough descendants and repeated same-runtime
  immediate children.
- Resolve only when multiple descendants share the root entrypoint:
  - Python script path or `-m` module.
  - Node/dotnet script or app argument.
  - JVM `-jar` target or main class.
- Label fallback is the root cmdline.

The detector is intentionally conservative: it should catch common `multiprocessing`, clustered
Node, Java worker, and dotnet worker layouts without collapsing unrelated interpreters that only
happen to sit near each other in the process tree.

### Container Shims

This rule catches the small supervisor process that owns a container workload subtree:

- Nominate known shim/supervisor comm names such as `containerd-shim`, `conmon`, and `docker-init`
  when they have descendants.
- Resolve only when the shim cmdline includes a container identity flag (`-id`, `--id`,
  `-container-id`, or `--container-id`).
- Label fallback is the root cmdline, preserving the runtime-provided container id until richer
  name lookup exists.

This is not a full container detector. It does not yet infer namespace membership, cgroup paths,
pod identity, image name, or orchestrator metadata.

## Next Detector Surface: Cgroups

Plain `bwrap` is not a useful semantic app detector on its own. It appears in ad hoc harnesses,
developer sandboxes, and Flatpak runtime support; the process name says "sandbox mechanism",
not "application identity". Treat it as supporting evidence only after stronger provenance has
been read.

Cgroup paths should become first-class metadata in `GroupMetaView`:

- Read `/proc/<pid>/cgroup` with the same slow-changing, generation-evicted discipline as
  cmdlines.
- Parse systemd units and scopes from paths such as `system.slice/*.service`,
  `user.slice/*/*.service`, `user@UID.service`, `session-*.scope`, and `app-*.scope`.
- Parse container/runtime identity from Docker, containerd, CRI-O, libpod, and Kubernetes cgroup
  path fragments.
- Use common cgroup ancestry to nominate roots even when the process tree is reparented or the
  semantic owner is a small shim.
- Prefer cgroup-derived labels over cmdline fallbacks when the cgroup path contains a stable
  service/scope/container name.

Flatpak should be a specialization on top of this cgroup-aware layer:

- Nominate processes in Flatpak-looking user scopes or sandbox subtrees.
- Resolve by probing `/proc/<pid>/root/.flatpak-info`, not by trusting `bwrap`.
- Label with the Flatpak app id and branch/runtime details where available.
- Fall back to the cgroup scope label before falling back to cmdline.

This preserves plain `bwrap` as useful context while avoiding false app groups for one-off
sandbox harnesses.

## Deferred Work

- Exe basename, cmdline argument, and `.desktop` label tiers.
- `/proc/<pid>/cgroup` storage and parsing in `GroupMetaView`.
- systemd service/scope detectors.
- Flatpak precision from cgroups plus `.flatpak-info`.
- Full container and Kubernetes pod identity from cgroups/namespaces/runtime metadata.
- Richer interpreter/runtime labels.
- Jail/chroot/user wrapper rules that need filesystem or namespace metadata.
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
- Additional detector synthetic trees:
  - shared-entrypoint runtime pool becomes a group;
  - runtime fan with divergent entrypoints settles to `NoGroup`;
  - container shim with a container id flag becomes a group.
- Future cgroup detector synthetic trees:
  - systemd service/scope cgroup path becomes a group;
  - shared Kubernetes pod cgroup descendants group under a pod label;
  - Flatpak cgroup plus `.flatpak-info` resolves to the Flatpak app id;
  - plain ad hoc `bwrap` without cgroup/Flatpak provenance does not group by itself.
- App behavior:
  - classified auto-group starts collapsed;
  - user expansion suppresses re-collapse while that PID remains visible;
  - absent PIDs clear stale collapse/suppression state.
- Render assertion:
  - collapsed group root uses group label when present;
  - expanded tree remains structurally naive and unchanged.
