# Firepanda: Rust MCP stdio service with a managed Lightpanda browser.
# `build`/`gates` exercise the parity binary (3 tools); `release` adds search.

CARGO ?= cargo
MANIFEST ?= rust/Cargo.toml
BINARY ?= rust/target/release/firepanda
SEARCH_TARGET ?= rust/target-search
SEARCH_BINARY ?= $(SEARCH_TARGET)/release/firepanda
LIGHTPANDA ?= .local/bin/lightpanda
PREFIX ?= $(HOME)/.local

.PHONY: build release test check gates search-gates install package clean

build:
	$(CARGO) build --manifest-path $(MANIFEST) --release

release:
	$(CARGO) build --manifest-path $(MANIFEST) --release --features search

test:
	$(CARGO) test --manifest-path $(MANIFEST) --release
	$(CARGO) test --manifest-path $(MANIFEST) --release --features search

check:
	$(CARGO) fmt --manifest-path $(MANIFEST) --check
	$(CARGO) clippy --manifest-path $(MANIFEST) --release --all-targets -- -D warnings
	$(CARGO) clippy --manifest-path $(MANIFEST) --release --all-targets --features search -- -D warnings

gates: build
	python3 tests/protocol.py $(BINARY)
	python3 tests/transport.py $(BINARY)
	python3 tests/integration.py $(BINARY) $(LIGHTPANDA)
	python3 tests/memory.py $(BINARY) $(LIGHTPANDA)

search-gates:
	$(CARGO) build --manifest-path $(MANIFEST) --release --features search --target-dir $(SEARCH_TARGET)
	python3 tests/search.py $(SEARCH_BINARY) $(LIGHTPANDA)

install: release
	install -d "$(PREFIX)/bin" "$(PREFIX)/share/licenses/firepanda"
	install -m 0755 $(BINARY) "$(PREFIX)/bin/firepanda"
	install -m 0644 LICENSE "$(PREFIX)/share/licenses/firepanda/LICENSE"

package: release
	python3 scripts/package-release.py --binary $(BINARY)

clean:
	$(CARGO) clean --manifest-path $(MANIFEST)
	rm -rf rust/target-search dist
