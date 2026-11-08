# Contributing and release maintenance

## Before opening a pull request

Run the formatting/lint/test targets for both feature sets, then the browser gates
with a native Lightpanda build:

```sh
make check
make test
python3 scripts/install-lightpanda.py --release 1.0.2
make gates           # parity binary: protocol, transport, integration, memory
make search-gates    # search-feature binary: offline engine fixtures
```

`make gates` uses `.local/bin/lightpanda` by default; override with
`make gates LIGHTPANDA=/path/to/lightpanda`. Without Lightpanda, run the offline
suites directly (`tests/protocol.py`, `tests/transport.py`, `tests/search.py`) and
say which browser-dependent checks could not run.

## Dependency updates

Dependencies are pinned in `rust/Cargo.lock`. Update deliberately:

```sh
cargo update --manifest-path rust/Cargo.toml
cargo test --manifest-path rust/Cargo.toml --release
cargo test --manifest-path rust/Cargo.toml --release --features search
```

Review the lockfile diff and upstream license/API changes. New runtime
dependencies should keep the binary small; search-only crates stay behind the
`search` feature. There is no automated dependency-bump workflow.

## Prepare a release

1. Bump `version` in `rust/Cargo.toml` (and refresh `Cargo.lock`).
2. Run `make check`, `make test`, `make gates`, and `make search-gates`.
3. Confirm the release build reports the new version:
   `cargo build --manifest-path rust/Cargo.toml --release --features search &&
   rust/target/release/firepanda --version`.
4. Tag `vVERSION` and push. The Release workflow builds and tests the tagged
   tree on macOS and Ubuntu, packages archives, and creates a draft release.

## Package locally

From a tested source build:

```sh
python3 scripts/package-release.py --binary rust/target/release/firepanda --tag v0.2.0
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
