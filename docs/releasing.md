# Contributing and release maintenance

## Before opening a pull request

Run the formatting/lint/test targets, then the full gates
with a native Lightpanda build:

```sh
make check
make test
python3 scripts/install-lightpanda.py --release 1.0.2
make gates           # protocol, transport, fetch, search, integration, memory
```

`make gates` uses `.local/bin/lightpanda` by default; override with
`make gates LIGHTPANDA=/path/to/lightpanda`. Without Lightpanda, run the offline
suites directly (`tests/protocol.py`, `tests/transport.py`, `tests/search.py`) and
say which browser-dependent checks could not run.

## Dependency updates

Dependabot (`.github/dependabot.yml`) opens weekly pull requests against the
root Cargo project and the GitHub Actions workflows. Minor/patch updates are
grouped; major updates arrive separately for review. To update locally:

```sh
cargo update
cargo upgrade --incompatible
cargo test --release
```

Review the lockfile diff and upstream license/API changes before merging. New
runtime dependencies should keep the binary small.

## Prepare a release

1. Bump `version` in `Cargo.toml` (and refresh `Cargo.lock`).
2. Run `make check`, `make test`, and `make gates`.
3. Confirm the release build reports the new version:
   `cargo build --release && target/release/firepanda --version`.
4. Tag `vVERSION` and push. The Release workflow builds and tests the tagged
   tree on macOS and Ubuntu, packages archives, and creates a draft release.

## Package locally

From a tested source build:

```sh
python3 scripts/package-release.py --binary target/release/firepanda --tag v0.2.0
```

The script writes `dist/firepanda-VERSION-OS-ARCH.tar.gz` and an accompanying
`.sha256`. The archive includes `bin/firepanda`, README, docs, the Lightpanda
installer, and Firepanda's MIT license. It does not bundle Lightpanda, local
configuration, build caches, or review artifacts. Architecture is detected from
the native runner; these are not universal macOS binaries or cross-compiled Linux
binaries. Ubuntu-built binaries depend on the runner's libc/toolchain and may need
rebuilding for older distributions.

After extracting an archive, install Lightpanda using the included installer and
point an MCP client at `bin/firepanda`. Build/test/benchmark instructions in the
README require a source checkout.
