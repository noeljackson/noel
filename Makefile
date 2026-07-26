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
SYSTEM_PREFIX ?= /usr/local
SYSTEM_BINDIR ?= $(SYSTEM_PREFIX)/bin
SUDO ?= sudo
INSTALL_BUILD_SCRIPT := scripts/install-build.sh

RUN_ARGS ?=
DOGFOOD_ARGS ?= --continue --model faux/echo

.PHONY: help build release install install-system run dogfood dogfood-release fmt lint test check ci e2e dogfood-long dogfood-real dogfood-real-print test-smoke docker-build docker-e2e parity-check ts-parity-build ts-parity-fixtures ts-parity-update ts-parity-drift ts-parity-agent smoke-real smoke-real-openai smoke-real-anthropic smoke-real-gemini smoke-real-mistral smoke-real-openrouter smoke-real-profiles smoke-claude-opus-oauth clean

help:
	@printf '%s\n' \
		'Targets:' \
		'  build        Build the workspace' \
		'  release      Build the pi CLI release binary' \
		'  install      Install the pi CLI release binary to $$(PREFIX)/bin' \
		'  install-system Install the pi CLI release binary to /usr/local/bin' \
		'  run          Run the pi CLI; pass RUN_ARGS="..." for CLI args' \
		'  dogfood     Run dev TUI with rebuild/restart watcher' \
		'  fmt          Check Rust formatting' \
		'  lint         Run clippy with warnings denied' \
		'  test         Run all Rust tests' \
		'  check        Run fmt, lint, and test' \
		'  ci           Run check and local tmux e2e' \
		'  e2e          Run tmux TTY e2e' \
		'  dogfood-release Run release-binary tmux dogfood smoke' \
		'  dogfood-long Run long release-binary TTY paint/scroll smoke' \
		'  dogfood-real Run optional real-provider TTY dogfood smoke' \
		'  dogfood-real-print Run optional real-provider print smoke' \
		'  test-smoke   Run local TTY smoke plus manual real-provider smoke' \
		'  smoke-real   Run opt-in generic real-provider print smoke' \
		'  smoke-real-profiles Run opt-in OpenAI/Anthropic/Gemini/Mistral/OpenRouter smokes' \
		'  docker-e2e   Build and run Dockerized tmux TTY e2e' \
		'  parity-check Run committed TS parity checks and drift detection' \
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

install-system: release
	$(SUDO) install -d "$(SYSTEM_BINDIR)"
	$(SUDO) install -m 0755 target/release/pi "$(SYSTEM_BINDIR)/pi"

run:
	$(CARGO) run -p pi-cli -- $(RUN_ARGS)

dogfood:
	CARGO="$(CARGO)" scripts/dogfood-dev.sh $(DOGFOOD_ARGS)

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

dogfood-release: release
	scripts/dogfood-release.sh

dogfood-long: release
	scripts/dogfood-long-tty.sh

dogfood-real: release
	scripts/dogfood-real-tty.sh

dogfood-real-print: smoke-claude-opus-oauth

test-smoke: e2e smoke-real smoke-claude-opus-oauth

docker-build:
	$(DOCKER) build -f Dockerfile.e2e -t $(E2E_IMAGE) .

docker-e2e: docker-build
	$(DOCKER) run --rm $(E2E_IMAGE)

parity-check:
	$(CARGO) test -p pi-parity
	$(CARGO) test -p pi-ai --lib matches_ts
	$(CARGO) test -p pi-core --lib upstream_agent_tool_loop_fixture_documents_model_callable_tools
	$(MAKE) ts-parity-drift

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

smoke-real:
	CARGO="$(CARGO)" scripts/smoke-real-provider.sh

smoke-real-openai:
	CARGO="$(CARGO)" scripts/smoke-real-provider.sh openai

smoke-real-anthropic:
	CARGO="$(CARGO)" scripts/smoke-real-provider.sh anthropic

smoke-real-gemini:
	CARGO="$(CARGO)" scripts/smoke-real-provider.sh gemini

smoke-real-mistral:
	CARGO="$(CARGO)" scripts/smoke-real-provider.sh mistral

smoke-real-openrouter:
	CARGO="$(CARGO)" scripts/smoke-real-provider.sh openrouter

smoke-real-profiles: smoke-real-openai smoke-real-anthropic smoke-real-gemini smoke-real-mistral smoke-real-openrouter

smoke-claude-opus-oauth:
	CARGO="$(CARGO)" scripts/smoke-claude-opus-oauth.sh

clean:
	$(CARGO) clean
