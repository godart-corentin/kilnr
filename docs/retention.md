# Build retention

`kilnr cleanup [--dry-run] [--project <project>]` applies administrator-configured
retention. The CLI uses sudo when needed; its privileged helper drops to `kilnr`
before touching build data. The daily `kilnr-cleanup.timer` invokes the same
helper as an unprivileged oneshot service. It has no Docker group membership and
retains the controller's applicable systemd hardening. Filesystem ACLs still
restrict repository writes to `refs/kilnr/jobs`.

## Configuration and upgrades

Fresh installations ship this template in `/etc/kilnr/defaults.json`:

```json
{
  "retention": {
    "max_age_days": 30,
    "max_builds_per_ref": 10,
    "keep_releases": true
  }
}
```

The [retention example](../examples/retention.json) is a config fragment to merge
into the existing defaults/project object, not a replacement for its other fields.

Like runner settings, this object is **copied on project creation** into
`/etc/kilnr/projects/<project>.json`. Change the project file to change an existing
project's policy. There is no live inheritance from defaults and no
repository-controlled retention setting.

**Upgrades preserve existing defaults and project files. A project without a
retention object has no automatic age or count deletion.** Thus updating an
existing installation enables the timer but does not silently delete its
history. New projects on that installation also remain disabled until the
administrator adds retention to its existing defaults or project configuration.

Omitted or `null` limits are disabled. Specified limits must be positive integers
(up to 1,000,000); zero, booleans, negative values, and unknown keys are errors.
An omitted `keep_releases` means `true`. An empty object disables both limits.
Invalid configuration fails closed for that project and makes cleanup exit
nonzero. Project rename preserves the retention object.

To enable retention on an existing project, add the object above to its config,
then inspect it before the next scheduled run:

```bash
kilnr cleanup --dry-run --project my_app
kilnr cleanup --project my_app
```

The preview prints each eligible build's ID, project, full Git ref, age in days,
and all applicable reasons. It does not modify builds, pin refs, or transaction
records. Real cleanup uses identical selection and safety checks; results can
change if builds or configuration change between invocations. For a maintenance
window, stop `kilnr-cleanup.timer` before editing policy and start it afterwards.

## Selection

A completed build has matching validated `job.json` and `status.json`, a terminal
state (`success`, `failed`, or `aborted`), and a timezone-qualified `finished_at`
no earlier than receipt. Preparation failures are eligible even without a
runtime or pipeline. Running/preparing builds and any build identified by an
incoming/running queue entry are excluded. Invalid or incomplete metadata is
preserved, never treated as permission to delete.

Eligible builds are grouped independently by project and **full Git ref**, and
sorted newest completion first, with descending build ID as a deterministic tie
breaker. Count retention removes entries beyond the newest N. Age retention
removes entries strictly older than the limit, measured from completion.
**Either limit can select a build**: N is a maximum, not a minimum protected
history. Both reasons are shown when applicable.

Releases are excluded entirely by default. With `keep_releases: false`, the same
limits apply to releases, grouped by full tag ref. No GitHub API, PR state,
branch-deletion trigger, or remote-ref lookup participates in selection. Deleted
branches' builds follow normal age/count rules.

## Locks and deletion invariants

Cleanup takes the existing controller lock exclusively for its entire run,
including inventory and interrupted-deletion recovery. This excludes controller
claim, recovery, execution, finalization, and notification. It then takes each
project lock exclusively, excluding enqueue, rerun, rename, project deletion,
and other project lifecycle mutations. Locks are acquired nonblocking in that
order; busy controllers/projects are reported as deferred. Unrelated unlocked
projects can still be processed. Deferred work is retried on the next timer run
or manually; a continuously busy controller can delay retention.

Existing per-build status locks are also acquired nonblocking before deletion.
The global/project locks and metadata checks remain necessary even if status is
already terminal. Corrupt queue metadata prevents cleanup for the project;
queue filenames and embedded IDs both protect builds.

Before retiring a build, cleanup requires:

- a structured ID matching enqueue's timestamp/project/SHA/random-suffix format;
- matching project, IDs, SHA, type, ref, and receipt time in job and status;
- the exact `refs/kilnr/jobs/<build-id>` pin identity;
- regular, singly linked metadata with trusted ownership and no nonowner write
  permission; build roots/directories are also checked for ownership and modes;
- nonsymlink roots, ancestors, build directories, metadata, and repository/ref
  namespace directories;
- no nested mounts (including same-device Linux bind mounts) or filesystem
  crossings in payloads.

Only direct children of the fixed `/var/lib/kilnr/builds` directory can be
retired. No CLI/environment override changes that root. Recursive removal does
not follow payload symlinks: it unlinks those links themselves.
Repository-provided path fields are not deletion targets.
Administrative processes that bypass Kilnr locks or change mounts/ownership
while cleanup runs are outside the supported concurrency model.

## Crash recovery and readers

Before removal, cleanup creates a `.cleanup-<build-id>` directory beneath the
builds root, writes and fsyncs a `record.json` snapshot, then atomically moves the
build into its `build` child. It fsyncs both directories before deleting payloads.
Once published, that record represents committed deletion intent. Subsequent
runs finish its removal even if retention is later disabled. Dry-run reports
pending recovery without performing it. This keeps partial filesystem deletion
recoverable without a database. Empty transactions left before/after publication
are removed safely as well. Malformed transactions are preserved for review.

The build disappears from CLI/web discovery at the move. Existing open file
handles may remain readable until closed; new requests return missing-build/log
responses. Listing tolerates concurrent disappearance, and live streams close
with a `deleted` end state. Hidden cleanup records do not consume the web list
limit.

Finish interrupted cleanup before deleting a project or renaming projects;
those operations refuse pending cleanup rather than orphan its identity/config.
Build history belonging to an already deleted project is intentionally not
cleaned: no project configuration remains to authorize a policy.

## Git pins and rerun

Job pins protect objects during preparation/processing, **not for the lifetime
of historical builds**. The controller normally removes them at completion or
recovery. Retention retries leftover loose pins for its selected builds before
retiring their directories, verifying the SHA and taking the corresponding Git
ref lock. Missing pins are fine. A mismatch, symlink, or existing ref lock blocks
deletion and produces a nonzero exit. A pin removed before a later filesystem
failure is safe: completed history did not promise a pin, and retry is idempotent.
A crash while holding a Git ref lock may require an administrator to inspect and
remove that stale `.lock` once all relevant processes are stopped.

Installed repositories set `gc.packRefs=false`. If an administrator has packed a
historical job pin, cleanup refuses that build and reports the exact ref for
administrator repair. Removing a packed ref requires repository-wide write
access that `kilnr` intentionally does not have; retention does not grant it or
silently leave an undeletable pin behind while deleting the build. Inspect and
remove only that stale internal ref using Git as the repository owner, then
retry cleanup. Ordinary branches/tags and unrelated job refs are never deleted.
No Git object garbage collection is performed.

Rerun still creates a new CI build at the original SHA, provided Git still has
that object. It holds the project's shared lock from reading history through
enqueue publication. Cleanup either finishes first (rerun reports missing
history) or defers until rerun has queued its new, independently protected job.
Once history is deleted, its ID cannot be rerun through the history command.

## Scheduling and scope

The timer runs daily with up to one hour randomized delay and `Persistent=true`
so a missed run is scheduled after startup. Installation/update installs the
oneshot and timer and enables the timer idempotently. Updates stop the cleanup
units before replacing helpers. Uninstall disables the timer and stops its
service. Inspect runs with:

```bash
systemctl status kilnr-cleanup.timer
journalctl -u kilnr-cleanup.service
```

Retention removes the selected build's source snapshot, workspaces, logs,
artifacts, commands, and generated runtime/metadata. It does **not** clean
`/var/lib/kilnr/cache`, repositories/objects, queue jobs, secrets, or unrelated
state. Cache eviction and build cleanup are separate lifecycle concerns; cache
retention is future work.
