# Changelog

## [0.1.1] - 2026-11-15

### Fixed

- Apply `--no-private-network` to raw HTTP requests made by `fetch` and `map`
  robots/sitemap discovery, as well as the Obscura browser.
- Reject non-public IP-literal destinations before sending requests and at
  every redirect hop. Filter DNS results so hostnames connect only to public
  addresses.
- Include underlying request errors so callers can see why a destination was
  refused.

## [0.1.0] - 2026-11-15

### Added

- Initial Rust MCP release with `scrape`, `crawl`, `map`, `fetch`, and `search`
  tools and support for Lightpanda and Obscura browser backends.
- Prebuilt binaries for Linux amd64, Linux arm64, and macOS arm64, with SHA-256
  checksums and browser installers.

[0.1.1]: https://github.com/romanbsd/firepanda/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/romanbsd/firepanda/releases/tag/v0.1.0
