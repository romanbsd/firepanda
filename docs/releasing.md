# Contributing and release maintenance

## Before opening a pull request

Run the formatting/lint/test targets, then the full gates with a native browser:

```sh
make check
make test
python3 scripts/install-lightpanda.py --release 1.0.2
python3 scripts/install-obscura.py --release v0.2.6
make gates           # protocol, transport, installer, fetch, search, integration, memory
make gates-obscura   # the browser suites against Obscura
```

`make gates` uses `.local/bin/lightpanda` by default; override with
`make gates LIGHTPANDA=/path/to/lightpanda`. `make gates-obscura` uses
`.local/bin/obscura`; override with `OBSCURA=/path/to/obscura`. `make gates-all`
runs both. Without a browser, run the offline suites directly
(`tests/protocol.py`, `tests/transport.py`, `tests/installer.py`,
`tests/search.py`) and say which browser-dependent checks could not run.

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
`.sha256`. The archive includes `bin/firepanda`, README, docs, both browser
installers, and Firepanda's MIT license. It does not bundle Lightpanda or Obscura,
local configuration, build caches, or review artifacts. Architecture is detected
from the native runner; these are not universal macOS binaries or cross-compiled
Linux binaries. Ubuntu-built binaries depend on the runner's libc/toolchain and
may need rebuilding for older distributions.

After extracting an archive, install Lightpanda or Obscura using the included
installer and point an MCP client at `bin/firepanda`. Build/test/benchmark
instructions in the README require a source checkout.
