# Changelog

## [0.1.2] - 2026-11-15

### Added

- Use the Brave Search API first for automatic search when `BRAVE_API_KEY`
  is set, with keyless DuckDuckGo and Brave HTML search as fallbacks. Select
  `engine: "brave_api"` to use the API explicitly.
- Convert HTML highlights and entities in Brave API results to plain text.

### Fixed

- Apply `--no-private-network` to managed Lightpanda 1.0.4 or newer using its
  private-network guard, which checks destinations after DNS resolution.
- Reject `--no-private-network` with external browsers (`--socket` or
  `--cdp-url`), whose network policies Firepanda does not control.

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

[0.1.2]: https://github.com/romanbsd/firepanda/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/romanbsd/firepanda/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/romanbsd/firepanda/releases/tag/v0.1.0
