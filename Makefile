CARGO ?= cargo
DOCKER ?= docker
E2E_IMAGE ?= pi-e2e
TS_PARITY_IMAGE ?= pi-ts-parity
TS_REFERENCE_REPO ?= https://github.com/earendil-works/pi.git
TS_REFERENCE_REF ?= main
TS_PARITY_TRACKING_REF ?= main
TS_PARITY_FIXTURES_DIR ?= tests/fixtures/ts-parity
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
INSTALL_BUILD_SCRIPT := scripts/install-build.sh

.PHONY: help build release install run fmt lint test check ci e2e dogfood dogfood-real dogfood-real-print test-smoke docker-build docker-e2e ts-parity-build ts-parity-fixtures ts-parity-update ts-parity-drift ts-parity-agent smoke-claude-opus-oauth clean

help:
	@printf '%s\n' \
		'Targets:' \
		'  build        Build the workspace' \
		'  release      Build the pi CLI release binary' \
		'  install      Install the pi CLI release binary to $$(PREFIX)/bin' \
		'  run          Run the pi CLI' \
		'  fmt          Check Rust formatting' \
		'  lint         Run clippy with warnings denied' \
		'  test         Run all Rust tests' \
		'  check        Run fmt, lint, and test' \
		'  ci           Run check and local tmux e2e' \
		'  e2e          Run tmux TTY e2e' \
		'  dogfood      Run release-binary tmux dogfood smoke' \
		'  dogfood-real Run optional real-provider TTY dogfood smoke' \
		'  dogfood-real-print Run optional real-provider print smoke' \
		'  test-smoke   Run local TTY smoke plus manual real-provider smoke' \
		'  docker-e2e   Build and run Dockerized tmux TTY e2e' \
		'  ts-parity-fixtures  Generate TS reference fixtures inside Docker' \
		'  ts-parity-update    Refresh fixtures from moving TS reference inside Docker' \
		'  ts-parity-drift     Check moving TS reference for fixture drift' \
		'  ts-parity-agent     Check drift and optionally dispatch PI_PARITY_AGENT_COMMAND' \
		'  smoke-claude-opus-oauth  Run manual Claude Opus OAuth smoke' \
		'  clean        Remove Cargo build output'

build:
	$(CARGO) build --all

release:
	$(CARGO) build --release -p pi-cli

install:
	CARGO="$(CARGO)" $(INSTALL_BUILD_SCRIPT)
	install -d "$(BINDIR)"
	install -m 0755 target/release/pi "$(BINDIR)/pi"

run:
	$(CARGO) run -p pi-cli

fmt:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --all-targets --all-features -- -D warnings

test:
	$(CARGO) test --all

check: fmt lint test

ci: check e2e

e2e:
	scripts/e2e-tmux.sh

dogfood: release
	scripts/dogfood-release.sh

dogfood-real: release
	scripts/dogfood-real-tty.sh

dogfood-real-print: smoke-claude-opus-oauth

test-smoke: e2e smoke-claude-opus-oauth

docker-build:
	$(DOCKER) build -f Dockerfile.e2e -t $(E2E_IMAGE) .

docker-e2e: docker-build
	$(DOCKER) run --rm $(E2E_IMAGE)

ts-parity-build:
	$(DOCKER) build -f Dockerfile.ts-parity \
		--build-arg TS_REFERENCE_REPO="$(TS_REFERENCE_REPO)" \
		--build-arg TS_REFERENCE_REF="$(TS_REFERENCE_REF)" \
		-t $(TS_PARITY_IMAGE) .

ts-parity-fixtures: ts-parity-build
	mkdir -p "$(TS_PARITY_FIXTURES_DIR)"
	$(DOCKER) run --rm -v "$(CURDIR)/$(TS_PARITY_FIXTURES_DIR):/fixtures" $(TS_PARITY_IMAGE)

ts-parity-update:
	$(MAKE) ts-parity-fixtures TS_REFERENCE_REF="$(TS_PARITY_TRACKING_REF)"

ts-parity-drift:
	scripts/ts-parity-drift.sh

ts-parity-agent:
	scripts/ts-parity-drift.sh

smoke-claude-opus-oauth:
	CARGO="$(CARGO)" scripts/smoke-claude-opus-oauth.sh

clean:
	$(CARGO) clean
