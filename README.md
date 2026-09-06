# Kilnr

Kilnr is a small self-hosted CI system for Ubuntu 24.04 LTS and Ubuntu 26.04 LTS built from standard Unix tools:

```text
git push
   ↓
bare Git repository
   ↓
post-receive
   ↓
filesystem queue
   ↓
systemd
   ↓
GNU Make DAG
   ↓
ephemeral Docker containers
```

Kilnr deliberately has no Jenkins/GitLab/GitHub Actions server, no database, and no Docker socket inside build containers.

> [!WARNING]
> Kilnr is experimental 0.x software. It executes repository-controlled code and integrates with Docker, systemd, Git, and host firewall rules. Review the security model before using it with untrusted repositories or exposing Kilnr services publicly.

## Current features

- Bare Git repositories over SSH using a restricted `git` account
- Exact-SHA builds; branch names are never used for checkout
- Atomic filesystem queue with pinned Git refs under `refs/kilnr/jobs/`
- Branch pipelines from `.kilnr/pipelines/*.json`
- Release pipelines from `.kilnr/release.json`
- GNU Make dependency graph with groups, `needs`, and parallel jobs
- `run`, `script`, and low-level `command` execution modes
- Managed `pnpm` via `tools`, resolved from the exact-SHA `package.json`
- Persistent pnpm store cache with `cache: ["pnpm"]`
- Persistent per-job logs and artifacts
- Artifact inputs between dependent jobs
- Project-scoped release secrets
- Ephemeral Docker jobs with CPU/RAM/PID limits and hardened flags
- Dedicated CI Docker network with Internet access but private/LAN destinations blocked
- Discord notifications
- Read-only web UI with JSON API and SSE updates
- CLI for status, logs, watch, rerun, projects, secrets, webhooks, Git keys, and diagnostics

## Requirements

- Ubuntu 24.04 LTS or Ubuntu 26.04 LTS
- Rust 1.85+ (the installer builds a locked release binary in a pinned Rust container)
- systemd
- Docker Engine already installed and running
- rootful Docker with a `docker` group
- Internet access during installation for Ubuntu packages
- For the optional web UI: an existing Caddy container managed with Docker Compose

Kilnr **does not install or globally reconfigure Docker**.

## Installation

```bash
git clone git@github.com:godart-corentin/kilnr.git kilnr
cd kilnr
sudo ./install.sh
```

The installer adds the user who invoked `sudo` to the `kilnr-readers` group. Log out and reconnect before using `kilnr status` so the new group membership takes effect. If the installer was run directly as `root`, add the intended CLI user manually with `sudo usermod -aG kilnr-readers <username>`.

If `172.30.0.0/24` conflicts with an existing Docker/LAN subnet, choose another subnet on first install:

```bash
sudo KILNR_CI_SUBNET=172.31.50.0/24 ./install.sh
```

Then check the installation:

```bash
kilnr doctor
```

## SSH key and project creation

Add a development key on the Kilnr server:

```bash
kilnr git-key add
```

Create a project:

```bash
kilnr project create my_app
```

This creates the bare repository, project configuration, secret directory, post-receive hook, and exact-SHA pin namespace.

On the development machine:

```bash
git remote add home git@kilnr-server:/srv/git/my_app.git
git push home main
```

Configure Discord if wanted:

```bash
kilnr project webhook set my_app
```

## Renaming a project

Rename Kilnr-owned project state on the server, then update each development
machine that pushes to it:

```bash
kilnr project rename old_name new_name
git remote set-url home git@SERVER:/srv/git/new_name.git
```

The rename is refused before mutation if the destination already exists or the
source has a job in the incoming or running queue. On success Kilnr moves the
bare repository, configuration, webhooks, release secrets, cache, and completed
builds. Completed build IDs and Kilnr-generated metadata are updated to the new
project name; matching internal job-pin refs move with them. This includes
terminal preparation failures, whose build directories can legitimately omit
`runtime.json` and `pipeline.mk`.

Checked-out sources, user command output, historical logs, artifacts,
repository objects, cache payloads, and secret values are left byte-for-byte
unchanged. The command uses one transaction: if commit or verification fails,
Kilnr rolls completed changes back in reverse order while both project names
remain locked. If an inverse operation also fails, the error identifies the
remaining paths that require administrator recovery.

Project lifecycle coordination uses pre-provisioned `root:kilnr-submit` mode
`0660` entries under `/var/lib/kilnr/locks/projects`. The lock namespace and
its state-directory ancestors are root-owned and non-writable by submitter
identities, so an active inode lock cannot be bypassed by replacing a pathname.

## Branch pipelines

Branch CI lives in `.kilnr/pipelines/*.json`. A branch push scans the pipeline files from the exact pushed SHA. Zero matching pipelines means no build; exactly one runs; multiple matches are a configuration error.

Example:

```json
{
  "schema": 1,
  "trigger": {
    "type": "branch",
    "branches": ["*"]
  },
  "max_parallel": 4,
  "jobs": {
    "tests": {
      "group": "quality",
      "image": "node:24-bookworm",
      "network": "kilnr-ci",
      "tools": ["pnpm"],
      "cache": ["pnpm"],
      "run": [
        "pnpm install --frozen-lockfile",
        "pnpm test"
      ]
    },
    "build": {
      "group": "quality",
      "image": "node:24-bookworm",
      "network": "kilnr-ci",
      "tools": ["pnpm"],
      "cache": ["pnpm"],
      "run": [
        "pnpm install --frozen-lockfile",
        "pnpm build"
      ],
      "artifacts": ["dist/**"]
    }
  }
}
```

`run` commands execute sequentially in the same `/bin/sh -eu` process. Jobs run in parallel when the DAG allows it, up to `max_parallel`.

`network` may be:

- `none`: no network
- `kilnr-ci`: public Internet egress while private/LAN/host ranges are blocked

The project cannot request arbitrary Docker flags.

## Dependencies and groups

`needs` is the sole source of DAG ordering. It can reference either a job or a group:

```json
{
  "jobs": {
    "lint": {
      "group": "quality",
      "image": "node:24-bookworm",
      "tools": ["pnpm"],
      "run": ["pnpm lint"]
    },
    "tests": {
      "group": "quality",
      "image": "node:24-bookworm",
      "tools": ["pnpm"],
      "run": ["pnpm test"]
    },
    "package": {
      "needs": ["quality"],
      "image": "node:24-bookworm",
      "tools": ["pnpm"],
      "run": ["pnpm build"]
    }
  }
}
```

A group is only an organizational and dependency shortcut; it does not execute itself.

## Managed tools

Kilnr currently supports `pnpm` as a managed tool.

Automatic version resolution:

```json
"tools": ["pnpm"]
```

requires the exact-SHA root `package.json` to contain, for example:

```json
"packageManager": "pnpm@11.15.1"
```

An explicit version is also supported:

```json
"tools": {
  "pnpm": "11.15.1"
}
```

Kilnr exposes the managed binary inside the container; pipeline commands simply use `pnpm`.

## Persistent pnpm cache

Enable the project-scoped persistent pnpm store for a job with:

```json
"tools": ["pnpm"],
"cache": ["pnpm"]
```

Each job still runs the normal deterministic install:

```bash
pnpm install --frozen-lockfile
```

but pnpm can reuse packages already present in the warm store instead of downloading them again.

Kilnr stores the cache under:

```text
/var/lib/kilnr/cache/<project>/<job-type>/pnpm/<version>/
```

and mounts only that store into the job at:

```text
/run/kilnr/cache/pnpm
```

Important properties:

- projects never share caches
- normal CI and release jobs never share caches
- pnpm versions never share caches
- the cache is an accelerator, not a source of build truth
- the exact lockfile and `pnpm install --frozen-lockfile` remain authoritative
- workspaces remain isolated; `node_modules` is not shared between jobs

## Artifacts and inputs

A job can persist selected workspace files:

```json
"artifacts": [
  "release/*.AppImage",
  "release/latest-linux.yml"
]
```

Declared artifact patterns that match nothing fail the job. Paths are validated so absolute paths, `..`, and symlink escapes cannot leave the workspace.

A dependent job can consume artifacts from producer jobs with `inputs`:

```json
"needs": ["package-linux"],
"inputs": ["package-linux"]
```

Producer artifacts are exposed read-only under `/run/kilnr/inputs/<producer>` with a matching `KILNR_INPUT_<PRODUCER>` environment variable.

## Release pipelines and secrets

Release jobs live only in `.kilnr/release.json`. Branch CI never loads that file.

A SemVer-style tag such as `v1.5.0` creates a release build:

```bash
git tag v1.5.0
git push home v1.5.0
```

Project-scoped release secrets can be managed with:

```bash
kilnr secret set my_app APPLE_ID
kilnr secret set-file my_app WIN_CSC_LINK ./certificate.pfx
kilnr secret list my_app
kilnr secret delete my_app APPLE_ID
```

Secrets are release-only by default, mounted read-only for the requesting job, omitted from Docker environment metadata, and known textual values are redacted from persisted logs.

## Automatic job environment

Kilnr provides non-secret metadata including:

```text
KILNR_BUILD_ID
KILNR_PROJECT
KILNR_SHA
KILNR_REF
KILNR_JOB_TYPE
KILNR_JOB
KILNR_BRANCH   # branch CI
KILNR_TAG      # release
```

User-defined environment variables can be added with `env`, but `KILNR_*` is reserved.

## CLI

Typical commands:

```bash
kilnr status latest
kilnr logs latest
kilnr logs latest tests
kilnr watch latest pipeline
kilnr rerun latest

kilnr project create foo
kilnr project webhook set foo
kilnr project rename foo bar
kilnr project delete foo

kilnr secret list foo
kilnr doctor
kilnr cleanup --dry-run
kilnr cleanup --project foo
```

`kilnr rerun` creates a new CI build for the same SHA. Release builds are not rerun by that command.

## Build retention

Fresh installations ship this policy in `/etc/kilnr/defaults.json`. Project
creation copies it into `/etc/kilnr/projects/<project>.json` alongside the runner
settings. Merge this fragment into the existing configuration object:

```json
"retention": {
  "max_age_days": 30,
  "max_builds_per_ref": 10,
  "keep_releases": true
}
```

`kilnr cleanup --dry-run` previews deletion with build IDs, projects, refs, ages,
and reasons. `kilnr cleanup` applies it; `--project <project>` limits the scope.
The daily `kilnr-cleanup.timer` runs the same implementation through
`kilnr-cleanup.service`, with up to one hour randomized delay and catch-up for
missed runs after startup. Inspect its schedule and output with:

```bash
systemctl status kilnr-cleanup.timer
journalctl -u kilnr-cleanup.service
```

Only validated terminal builds (`success`, `failed`, `aborted`) outside the
incoming/running queues qualify. Age is measured from completion; count keeps
the newest completions independently per project/full Git ref, breaking ties by
descending build ID. Either limit can delete a build: the count limit does not
protect builds older than the age limit. Releases stay indefinitely unless
`keep_releases` is explicitly set to `false`.
Controller, project, and status locks protect lifecycle operations and reruns.
Busy controllers or projects defer cleanup until a later run. Deletion rejects
unsafe metadata, symlinked managed paths, and nested mounts. Interrupted deletion
is recovered from hidden transactions beneath the builds directory.

**Upgrades preserve existing configuration: projects without retention remain
disabled.** Limits omitted or set to `null` are disabled. Defaults are copied,
not inherited dynamically. Changing defaults affects only subsequently created
projects. To enable retention for an existing project, stop the timer, add the
object to that project's config, preview the results, then apply the policy and
restart the timer:

```bash
sudo systemctl stop kilnr-cleanup.timer
# Edit /etc/kilnr/projects/my_app.json to add retention.
kilnr cleanup --dry-run --project my_app
kilnr cleanup --project my_app
sudo systemctl start kilnr-cleanup.timer
```

`kilnr rerun` holds a shared project lock through enqueue so cleanup cannot
remove its source history during that operation. Deleted history cannot be
rerun by build ID. The `refs/kilnr/jobs/` pins protect pending processing, not
historical builds; cleanup removes leftover loose pins only after checking the
expected SHA. Packed pins or conflicting pin state block deletion and require
administrator review. Ordinary Git branches and tags are never removed.

Selected builds lose their snapshots, workspaces, logs, and artifacts. Deleted
builds disappear from the web UI, whose readers tolerate concurrent removal.
`/var/lib/kilnr/cache`, Git objects, secrets, and queue jobs are not cleaned.
Branch deletion does not trigger cleanup, and no GitHub or PR-state API is used.

See [retention operations and safety](docs/retention.md) for ordering, crash
recovery, pin repair, scheduling, and upgrade details.

## Filesystem layout

```text
/srv/git/
  <project>.git

/etc/kilnr/
  defaults.json
  network.env
  projects/
  secrets/
  web.json

/var/lib/kilnr/
  queue/
    tmp/
    incoming/
    running/
  builds/
    <build-id>/
      job.json
      status.json
      runtime.json        # present after runtime resolution
      pipeline.mk         # present after Makefile generation
      src/
      work/
      logs/
      artifacts/
      commands/
      runtime/            # optional generated tool runtime
  cache/
    <project>/
      ci/
        pnpm/
          <version>/
      release/
        pnpm/
          <version>/
  controller-home/
  job-runtime/
  secret-staging/
  locks/
    projects/
      <project>.lock
```

## Security model

### Git

`git` owns bare repositories and is reachable via SSH, but its shell is `git-shell`.

### Controller

`kilnr` has no login shell. It reads repositories through ACLs and can write only Kilnr's pinned job refs inside each bare repository.

### Build containers

Every job gets a fresh exact-SHA workspace. Docker jobs run with resource limits, all Linux capabilities dropped, `no-new-privileges`, and a non-root UID/GID.

Build code does **not** receive:

- `/var/run/docker.sock`
- the host root filesystem
- bare Git repositories or `.git`
- arbitrary host devices
- privileged mode
- unrelated project secrets or caches

`/tmp` is a no-exec tmpfs. Executable temporary work uses `/run/kilnr/tmp`, while `HOME=/run/kilnr/home` is an ephemeral disk-backed per-job directory removed after the job.

### Network

`kilnr-ci` uses a dedicated Docker bridge. Host firewall rules block private, loopback, link-local, carrier-grade NAT, multicast, and reserved IPv4 destinations while permitting public Internet package access.

## Read-only web UI

Kilnr Web is optional and runs separately behind an existing Dockerized Caddy deployment:

```bash
sudo ./install-web.sh kilnr.example.com
```

It publishes no Kilnr Web host port, uses the shared internal `kilnr-proxy` network, and keeps Basic Auth at Caddy. The web process is read-only against Kilnr build state.

Remove only the web layer with:

```bash
sudo ./uninstall-web.sh
```

## Updating Kilnr

```bash
git pull --ff-only
sudo ./update.sh
./tests/run.sh
kilnr doctor
```

The update path preserves repositories, project configuration, secrets, caches,
and build history during installation. It installs and enables the daily cleanup
timer idempotently. Existing projects without a retention setting remain exempt
from automatic deletion; projects with retention enabled follow their configured
policy on subsequent cleanup runs. Existing `defaults.json` is also preserved,
so add retention there explicitly if future projects should receive it.

## Uninstalling

```bash
sudo ./uninstall.sh
```

The uninstaller removes the installed programs, systemd units, and CI network/firewall setup while deliberately preserving persistent Kilnr data under `/srv/git`, `/var/lib/kilnr`, and `/etc/kilnr`.

To permanently remove all repositories, build history, configuration, secrets, optional web data, and Kilnr system identities, use the explicit purge mode:

```bash
sudo ./uninstall.sh --purge
```

The command displays the destructive paths and requires typing `PURGE`. For unattended environments, `--purge --yes` skips that confirmation. The shared `git` system account is preserved because it may be used independently of Kilnr.

## Development

Run the repository test suite with:

```bash
./tests/run.sh
```

Kilnr intentionally contains no GitHub Actions workflow. GitHub is used for source hosting and versioning only; Kilnr itself performs CI.
