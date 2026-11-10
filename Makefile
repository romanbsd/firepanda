# Firepanda: Rust MCP stdio service with a managed Lightpanda or Obscura browser.
# `build` produces the five-tool binary; search is always enabled.

CARGO ?= cargo
BINARY ?= target/release/firepanda
LIGHTPANDA ?= .local/bin/lightpanda
OBSCURA ?= .local/bin/obscura
PREFIX ?= $(HOME)/.local
BENCH_ITERATIONS ?= 30
BENCH_HEAVY ?= 5

.PHONY: build release test check gates gates-obscura gates-all bench install package clean

build:
	$(CARGO) build --release

release: build

test:
	$(CARGO) test --release

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --release --all-targets -- -D warnings

gates: build
	python3 tests/protocol.py $(BINARY)
	python3 tests/transport.py $(BINARY)
	python3 tests/installer.py
	python3 tests/fetch.py $(BINARY) $(LIGHTPANDA)
	python3 tests/search.py $(BINARY) $(LIGHTPANDA)
	python3 tests/integration.py $(BINARY) $(LIGHTPANDA)
	python3 tests/map_sitemap.py $(BINARY) $(LIGHTPANDA)
	python3 tests/memory.py $(BINARY) $(LIGHTPANDA)

gates-obscura: build
	python3 tests/protocol.py $(BINARY)
	python3 tests/integration.py $(BINARY) $(OBSCURA) obscura
	python3 tests/map_sitemap.py $(BINARY) $(OBSCURA) obscura
	python3 tests/memory.py $(BINARY) $(OBSCURA) --backend obscura

gates-all: gates gates-obscura

bench: build
	python3 scripts/benchmark-backends.py --binary $(BINARY) --lightpanda $(LIGHTPANDA) \
		--obscura $(OBSCURA) --iterations $(BENCH_ITERATIONS) --heavy-iterations $(BENCH_HEAVY)

install: release
	install -d "$(PREFIX)/bin" "$(PREFIX)/share/licenses/firepanda"
	install -m 0755 $(BINARY) "$(PREFIX)/bin/firepanda"
	install -m 0644 LICENSE "$(PREFIX)/share/licenses/firepanda/LICENSE"

package: release
	python3 scripts/package-release.py --binary $(BINARY)

clean:
	$(CARGO) clean
	rm -rf dist
