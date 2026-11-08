# Firepanda: Rust MCP stdio service with a managed Lightpanda browser.
# `build`/`gates` exercise the core binary (4 tools); `release` adds search.

CARGO ?= cargo
BINARY ?= target/release/firepanda
SEARCH_TARGET ?= target-search
SEARCH_BINARY ?= $(SEARCH_TARGET)/release/firepanda
LIGHTPANDA ?= .local/bin/lightpanda
PREFIX ?= $(HOME)/.local

.PHONY: build release test check gates search-gates install package clean

build:
	$(CARGO) build --release

release:
	$(CARGO) build --release --features search

test:
	$(CARGO) test --release
	$(CARGO) test --release --features search

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --release --all-targets -- -D warnings
	$(CARGO) clippy --release --all-targets --features search -- -D warnings

gates: build
	python3 tests/protocol.py $(BINARY)
	python3 tests/transport.py $(BINARY)
	python3 tests/fetch.py $(BINARY) $(LIGHTPANDA)
	python3 tests/integration.py $(BINARY) $(LIGHTPANDA)
	python3 tests/memory.py $(BINARY) $(LIGHTPANDA)

search-gates:
	$(CARGO) build --release --features search --target-dir $(SEARCH_TARGET)
	python3 tests/search.py $(SEARCH_BINARY) $(LIGHTPANDA)

install: release
	install -d "$(PREFIX)/bin" "$(PREFIX)/share/licenses/firepanda"
	install -m 0755 $(BINARY) "$(PREFIX)/bin/firepanda"
	install -m 0644 LICENSE "$(PREFIX)/share/licenses/firepanda/LICENSE"

package: release
	python3 scripts/package-release.py --binary $(BINARY)

clean:
	$(CARGO) clean
	rm -rf target-search dist
