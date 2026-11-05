# Contributing and release maintenance

## Before opening a pull request

Build from a clean directory with the selected dependency tags, run the full
CTest suite with native Lightpanda, and check C++ formatting. Browser-dependent
tests require `FIREPANDA_TEST_LIGHTPANDA`; without it only offline/protocol tests run.
For ownership, transport, or memory changes, also run the sanitizer configuration
from the README. Describe any validation that could not be run.

## Dependency updates

The weekly Monday 06:00 UTC/manual `Bump FetchContent tags` workflow follows the
same mechanism as mnemon-cpp: a Python standard-library script finds the latest
GitHub release tag (or falls back to a version tag), updates `GIT_TAG` declarations,
and `peter-evans/create-pull-request` opens/updates `bump-fetchcontent-tags`.
Only cpp-httplib and nlohmann/json are managed. No automatic merge is enabled.

```sh
python3 tests/dependency_updates.py
python3 scripts/bump_fetchcontent_tags.py
```

The second command contacts GitHub and writes CMakeLists.txt when tags change.
Set `GITHUB_TOKEN` to avoid anonymous API limits. Malformed tags or missing
releases fail before any file is written. Tags are readable upstream version
references, not immutable hashes. Review API changes and license notices.

In repository Settings → Actions → General, enable GitHub Actions to create pull
requests and allow the workflow's contents/pull-request write permissions.
GitHub documents that PR workflows created using GITHUB_TOKEN enter an
approval-required state, while token-created push events do not trigger new runs.
This workflow therefore builds and runs all tests before creating the PR. See
[GitHub workflow-trigger rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).
Maintainers should still run required macOS/sanitizer checks before merging.
Workflow changes from pull requests run with read-only repository permissions.

## Prepare a release

1. Set the version in `project(firepanda VERSION ...)` in CMakeLists.txt. CLI and
   MCP identity derive from it. Update the README/dependency notices if needed.
2. Run a clean Release build, CTest with Lightpanda 1.0.2, sanitizer tests, and
   formatting. Run the Linux/macOS workflows; local macOS results do not establish
   remote Linux success.
3. Inspect the files to publish. Local MCP configuration, build outputs,
   downloaded browsers, and review evidence should stay outside source/release archives.
4. Commit the reviewed source, create a matching `vMAJOR.MINOR.PATCH` tag, and push
   it when ready. The `Release` workflow builds/tests on Ubuntu 24.04 and macOS 15,
   rejects a tag/version mismatch, and creates a **draft** GitHub release.
5. Inspect the archive contents, checksums, platform architectures, and workflow
   results, then publish the draft when ready. Archives are unsigned; checksums
   detect transfer corruption but do not provide independent publisher identity.

The workflows are prepared configuration; they do not create a remote repository,
set repository visibility, or publish anything until explicitly run/tagged.
There is no hardcoded GitHub owner or repository URL to change before first push.

## Package locally

From a tested source build:

```sh
python3 scripts/package-release.py --binary build/firepanda --tag v0.2.0
```

The script writes `dist/firepanda-VERSION-OS-ARCH.tar.gz` and an accompanying
`.sha256`. The archive includes `bin/firepanda`, README, docs, the Lightpanda
installer, Firepanda's MIT license, and both C++ dependency MIT notices. It does
not bundle Lightpanda, local configuration, build caches, or review artifacts.
Architecture is detected from the native runner; these are not universal macOS
binaries or cross-compiled Linux binaries. Ubuntu-built binaries depend on the
runner's libc/toolchain and may need rebuilding for older distributions.

After extracting an archive, install Lightpanda using the included installer and
point an MCP client at `bin/firepanda`. Build/test/benchmark instructions in the
README that refer to CMake or tests require a source checkout.
