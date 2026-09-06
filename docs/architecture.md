# Architecture

## Push path

```text
developer
   │ git push
   ▼
sshd → git-shell
   ▼
/srv/git/<project>.git
   ▼ post-receive
enqueue
   ├─ classifies ci/release
   ├─ resolves exact commit SHA
   ├─ creates refs/kilnr/jobs/<job-id>
   └─ atomically publishes job.json
          ▼
/var/lib/kilnr/queue/incoming
          ▼ systemd.path
kilnr-controller.service
          ▼
controller
   ├─ claims job into queue/running
   ├─ archives exact SHA into build/src
   ├─ reads .kilnr/pipeline.json from that SHA
   ├─ validates the pipeline
   ├─ generates trusted pipeline.mk
   └─ launches GNU Make
          ▼
make -jN -k
          ▼
execute <build> <step>
          ▼
ephemeral Docker container
```

## Why Make

Kilnr does not implement a general-purpose scheduler. Each pipeline step becomes a Make target. `needs` becomes target prerequisites. GNU Make handles readiness, concurrency and dependency failure propagation.

After Make finishes, Kilnr converts remaining `pending` steps into `skipped`.

## Exact SHA

The hook never schedules `main` or a tag name as a build source. It stores the resolved commit SHA. An internal `refs/kilnr/jobs/<id>` ref keeps the object reachable until the build has been prepared.

The workspace is produced with `git archive <sha>`, so it contains no `.git` directory.

## Project lifecycle locking

Every validated project name has a lock below
`/var/lib/kilnr/locks/projects/`. That directory and both of its mutable
ancestors below `/var/lib` are root-owned and are not writable by submitter
identities; root lifecycle paths pre-provision stable
`root:kilnr-submit` mode `0660` lock files, and enqueue only opens existing
entries. Thus a submitter cannot unlink and replace the pathname while another
process holds an inode lock.

The post-receive path passes repository context rather than freezing a project
basename. Enqueue resolves the current repository path, takes that project's
shared lock, and resolves it again; if a rename changed the identity, enqueue
releases and retries under the final name. It then holds the stable shared lock
until the job and pin ref are either published or cleaned up. Webhook and
release-secret set/delete operations use the same shared span from project
validation through durable publication. Project creation and deletion hold an
exclusive lock for their entire mutation.

Project rename validates both names and acquires both exclusive locks in sorted
order. It then repeats preflight while locked. This prevents a concurrent push,
create, delete, or overlapping rename from observing or creating half-renamed
state; unrelated projects can continue independently.

## Project rename transaction

Rename handles only Kilnr-owned identity state and has four phases:

1. **Inventory and preflight:** reject inconsistent source state, any occupied
   destination, source jobs in incoming/running, unsafe managed paths, build-ID
   ambiguity, and cross-filesystem moves. Production roots require the
   installer-created owners, groups, modes and ACL writer policy; the repository
   must be `git:git`, and `kilnr` must not be able to write `refs/heads`.
2. **Prepare:** write allowlisted configuration and generated build metadata to
   sibling temporary files, validate them, and fsync them without changing the
   active project.
3. **Commit:** atomically move the repository and managed state, install the
   prepared metadata, and rename matching `refs/kilnr/jobs/*` refs while
   recording an inverse for every completed mutation.
4. **Verify:** reload the renamed project through normal validators, check the
   mapped completed builds and refs, and reject remaining active Kilnr-owned
references to the old identity.

Controller-valid terminal preparation failures are migrated even when they
have no `runtime.json`, `pipeline.mk`, or populated `status.pipeline`. Rename
rewrites only the managed files present for that modeled state. It validates
the expected build top-level entries and their types while leaving payloads
inside `src`, `work`, `logs`, `artifacts`, `commands`, and `runtime` opaque.

Commit or verification failure runs the recorded inverses in reverse order
while both locks remain held. Sources, user command output, historical logs,
artifacts, repository objects, cache payloads, and secret values are outside
the rewrite allowlist.

## Trust boundaries

### `git`

Owns the bare repositories and accepts restricted SSH Git traffic. It can submit jobs but cannot read Kilnr secrets.

### `kilnr`

Runs the controller. It can read bare repositories and write only the `refs/kilnr/jobs` namespace. The systemd controller receives Docker group access through `SupplementaryGroups=docker`; the account is not permanently added to the Docker group.

### build containers

Repository-controlled commands run only inside Docker. They do not receive the Docker socket or Kilnr secrets.

### `kilnr-web`

The optional web interface only reads build output. It runs in a separate Docker container behind Caddy and receives no host port.

## Queue and crashes

Job publication and status writes use temporary files plus atomic renames.

At controller startup:

- a job claimed into `running/` before its build directory existed is returned to `incoming/`;
- an interrupted build with an existing build directory is marked `aborted`;
- labeled Docker containers from an interrupted build are force-removed;
- the job pin is cleaned up.

## Releases

Only the initial creation of a tag matching `^v[0-9]+\.[0-9]+\.[0-9]+$` becomes a `release` job.

Pipeline steps with `"when": "release"` are excluded entirely from normal CI runtime data.

## Completed-build retention

The Rust administrator cleanup command and daily systemd timer share the same
retention implementation. Cleanup holds the controller lock, then an exclusive
project lock, validates terminal build identities, and retires selected builds
through durable `.cleanup-<id>` transactions under the builds root. Rerun holds
a shared project lock through enqueue. Git pins remain preparation-scoped;
cleanup retries stale loose pins without granting repository-wide write access.

See [retention](retention.md) for configuration, migration behavior, deletion
invariants, reader behavior, and recovery.

## Current limitation

Kilnr 0.1 executes project steps in Linux Docker containers. Native macOS workers are intentionally not part of this initial package.
