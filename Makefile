CARGO ?= cargo
DOCKER ?= docker
E2E_IMAGE ?= pi-e2e
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
INSTALL_BUILD_SCRIPT := scripts/install-build.sh

.PHONY: help build release install run fmt lint test check ci e2e docker-build docker-e2e smoke-claude-opus-oauth clean

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
		'  docker-e2e   Build and run Dockerized tmux TTY e2e' \
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

docker-build:
	$(DOCKER) build -f Dockerfile.e2e -t $(E2E_IMAGE) .

docker-e2e: docker-build
	$(DOCKER) run --rm $(E2E_IMAGE)

smoke-claude-opus-oauth:
	CARGO="$(CARGO)" scripts/smoke-claude-opus-oauth.sh

clean:
	$(CARGO) clean
