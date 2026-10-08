# Contributing

[Project overview](README.md) | [Operations guide](docs/startup-and-debugging.md)

Crosslane uses the Rust toolchain selected by `rust-toolchain.toml`. Run commands
from the repository root. Keep local credentials and state out of commits.

## Build and run locally

```bash
cp .env.example .env
cargo build --workspace --locked
cargo run -p crosslaned
```

In another terminal:

```bash
cargo run -p crosslane -- health
cargo run -p crosslane -- help --all
```

The example environment uses `.crosslane-dev`. Set an absolute
`CROSSLANE_HOME` for work across directories, and never point development
commands at a production data directory.

## Validate changes

Select checks relevant to the change. Before a release, run the complete set:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo test --workspace --no-default-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
docker compose config --quiet
docker compose -f docker-compose.yml -f docker-compose.build.yml config --quiet
sh -n deploy/entrypoint.sh deploy/docker-setup.sh deploy/docker-upgrade.sh deploy/crosslane-docker deploy/install-crosslane-wrapper.sh
git diff --check
```

Use `cargo test -p <PACKAGE>` or `--test <TARGET>` to narrow a local test run.
Automated tests use simulated upstream responses and do not establish that a
real account, model, or client version works. Check those separately when a
change depends on them.

## Documentation and review

Keep README files focused on setup and navigation. Put operational instructions
in the paired English and Chinese guides. Document user-visible commands,
configuration, API behavior, and limitations; keep investigations and
implementation notes in the code review or Git history. Update paired guides
together, and check local links, examples, and commands before submitting.

Do not commit credentials, raw requests, or unsanitized test captures. Account
exports are redacted by default; only include secrets in a protected backup.

Use concise English Angular-style commit messages, such as
`docs(usage): clarify service setup`. Describe the user-visible result, checks
run, and remaining limitations in the review. Mark incompatible behavior with
`!` or a `BREAKING CHANGE` footer and include the required user action.
