# Contributing

[Documentation](README.md#documentation) · [Architecture](CLAUDE.md) · [Changelog](CHANGELOG.md)

Use the pinned Rust 1.97.1 toolchain and commit `Cargo.lock` changes when dependencies
or workspace versions change. Dependencies are pinned in the root workspace;
crate manifests inherit them. This workspace builds Unix-socket applications,
not a native Windows service.

## Local development

```bash
cp .env.example .env  # First setup only
cargo build --workspace --locked
cargo run -p crosslaned
```

In another terminal at the same repository root, use `cargo run -p crosslane -- health`
and `cargo run -p crosslane -- help --all`. The example isolates development files in
`.crosslane-dev`; use an absolute `CROSSLANE_HOME` if working from multiple directories.
Do not run development commands against a production data directory.

The default daemon enables the `sso` feature. Code requiring browser login must
remain feature-gated, and the no-default-features build must keep working.
The full Docker image pairs `chromiumoxide 0.9.1` with Chromium revision `1566079`;
update and validate the pair together. Native SSO additionally needs a browser.

## Validation

Run checks appropriate to the change, and run the full set before a release:

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

Workspace tests include integration targets automatically; use `--test` to select
one target when narrowing the scope:

```bash
cargo test -p crosslane --test cli_help --locked
cargo test -p crosslane-translate --test compatibility_controls --locked
cargo test -p crosslaned --test end_to_end responses:: --locked
```

Tests use temporary directories, Unix sockets, and loopback HTTP listeners.
The standard suite uses simulated upstream responses and does not need real
credentials. Compaction regressions can be selected separately:

```bash
cargo test -p crosslane-translate tokenizer --locked
cargo test -p crosslaned --test end_to_end manual_compaction:: --locked
cargo test -p crosslaned --test end_to_end warning_regressions:: --locked
```

One real Claude Code checkpoint test is ignored by default. After installing a
Claude Code version supporting `--bare`, run it with isolated configuration and
the test's local simulated upstream:

```bash
cargo test -p crosslaned --test end_to_end real_claude_code_retains_the_compaction_checkpoint --locked -- --ignored
```

Passing mocks does not establish production SSO, upstream availability, latency,
or usage accuracy. Record those checks separately during release qualification.

The repository currently has a tag-triggered Docker publishing workflow, not a
PR test gate. The commands above describe required validation, not existing CI
coverage. Record validation results for the exact source being released.

## Change boundaries

- Keep CLI definitions, help and completion on the same Clap command tree. New
  RPC behavior must update the shared protocol, daemon dispatch and CLI together.
- For protocol changes, read the [compatibility rules](docs/protocol-compatibility.md#compatibility-maintenance-rules)
  before adding rejection conditions. Test translated payloads and client-visible
  behavior, including streaming and non-streaming errors where relevant.
- Add configuration defaults, validation and reload handling together. Invalid
  reloads must leave the last valid state active. Reuse existing transaction locks.
- Preserve system instructions and tool pairing during compaction; verify the
  next client turn as well as the first response. Keep summary accounting separate.
- Keep credentials out of fixtures and diagnostics. Account export includes
  credentials by default; use `--redact` before sharing it.

## Documentation changes

Keep the main READMEs focused on setup and navigation. Detailed behavior belongs
in the six references linked from the READMEs. Update paired English/Chinese commands,
defaults, limits and authentication requirements together. Merge overlapping topics
and remove obsolete design notes; update all links when deleting a document.
Keep the complete CLI command tree in generated help instead of duplicating it in Markdown.

Verify commands against the current CLI, local links and heading anchors, fenced
JSON/TOML examples, and the actual Compose build/pull paths. Help invocation is
enough to inspect destructive commands; do not execute deletion, upgrades, real
diagnosis or webhook delivery merely to validate documentation.

Separate current implementation, historical observations, and proposed work.
Record audit date, commit, platform, features and skipped checks with test results.
Use explicit release examples; a source version string alone does not prove that
a matching image contains untagged commits. Update [CHANGELOG.md](CHANGELOG.md)
for externally visible changes and migrations.

## Commits and reviews

Use concise English Angular-style commit messages, such as
`docs(cli): clarify command migration` or `fix(proxy): preserve tool replay`.
Keep one logical change per commit. Call out incompatible behavior with `!` or a
`BREAKING CHANGE` footer and a migration note.

Describe the user-visible problem, resulting behavior, validation and remaining
limitations in the review. Update the workspace version, lockfile and changelog
together before releasing; pushing a matching `v*` tag triggers image publication.
