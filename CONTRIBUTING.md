# Contributing to Kilnr

Kilnr is intentionally small infrastructure software built around standard Unix tools.

Contributions should preserve that philosophy:

- simplicity over abstraction;
- standard Unix tools over large dependencies;
- explicit behavior over magic;
- security boundaries over convenience;
- reproducibility over implicit host state.

## Development Requirements

The supported targets are Ubuntu 24.04 LTS and Ubuntu 26.04 LTS.

Kilnr currently relies on:

- Rust 1.85 or newer;
- Bash;
- Git;
- GNU Make;
- systemd;
- Docker Engine;
- Linux ACLs;
- iptables.

Avoid adding runtime dependencies unless there is a clear benefit.

## Running Checks

Run:

    cargo test --all-targets
    cargo clippy --all-targets -- -D warnings

Frontend behavior tests and the production build (after installing its npm dependencies):

    npm --prefix web/frontend test
    npm --prefix web/frontend run build

Before submitting a change, also verify:

    bash -n install.sh
    bash -n update.sh
    bash -n uninstall.sh
    bash -n install-web.sh
    bash -n uninstall-web.sh

Rust code must pass `cargo fmt --check` and Clippy without warnings.

## Security-Sensitive Changes

Extra care is required when modifying:

- `libexec/controller`;
- `libexec/execute`;
- Git hooks;
- repository permissions or ACLs;
- Docker arguments;
- CI networking or firewall rules;
- secret handling;
- project creation or deletion;
- web exposure or authentication.

Repository-controlled values must never become arbitrary host shell commands.

Build containers must never receive the Docker socket or Kilnr secrets.

## Project Configuration

Do not add project-specific configuration to the Kilnr repository.

Kilnr itself must remain generic.

Project runtime state belongs under:

    /etc/kilnr/
    /var/lib/kilnr/
    /srv/git/

These directories contain installation-specific state and must not be copied into the Kilnr source repository.

## Pull Requests

Keep changes focused.

For behavioral changes:

1. explain the problem being solved;
2. describe security implications when relevant;
3. include tests or reproducible verification;
4. update documentation if user-facing behavior changes.

## Releases

Kilnr uses semantic-style version tags such as:

    v0.1.0
    v0.1.1
    v0.2.0

Update `VERSION` and `CHANGELOG.md` when preparing a release.
