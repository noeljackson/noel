CARGO ?= cargo
DOCKER ?= docker
E2E_IMAGE ?= noel-e2e
TS_PARITY_IMAGE ?= pi-ts-parity
RUST_CI_IMAGE ?= noel-rust-ci:1.99.0
TS_REFERENCE_REPO ?= https://github.com/earendil-works/pi.git
TS_REFERENCE_REF ?= main
TS_PARITY_TRACKING_REF ?= main
# chord-delta.json, chord-link.json, local-env.json, pi-env.json and
# pi-env-ssh.json are pinned to this upstream commit, whatever TS_REFERENCE_REF
# is. ts-parity-drift reports commits to packages/chord, to Durable's env
# (packages/durable/src/env and its conformance cases) and to pi-env
# (packages/env) on the tracking ref that came after it; move the pin here and
# regenerate.
CHORD_REFERENCE_REF ?= 1692bba6347a287ab0f3cb554a4430fafffc474b
TS_PARITY_FIXTURES_DIR ?= tests/fixtures/ts-parity
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
SYSTEM_PREFIX ?= /usr/local
SYSTEM_BINDIR ?= $(SYSTEM_PREFIX)/bin
SUDO ?= sudo
INSTALL_BUILD_SCRIPT := scripts/install-build.sh

RUN_ARGS ?=
DOGFOOD_ARGS ?= --continue --model faux/echo

.PHONY: help build release install install-system run dogfood dogfood-release fmt lint test check check-ci ci e2e dogfood-long dogfood-real dogfood-real-print test-smoke docker-build docker-e2e parity-check ts-parity-build ts-parity-fixtures ts-parity-chord ts-parity-env ts-parity-pi-env ts-parity-pi-env-ssh ts-parity-update ts-parity-drift ts-parity-agent chord-interop env-interop env-daemon smoke-real smoke-real-openai smoke-real-anthropic smoke-real-gemini smoke-real-mistral smoke-real-openrouter smoke-real-profiles smoke-claude-opus-oauth clean

help:
	@printf '%s\n' \
		'Targets:' \
		'  build        Build the workspace' \
		'  release      Build the noel CLI release binary' \
		'  env-daemon   Build the noel-env daemon for upload (size-optimized profile)' \
		'  install      Install the noel CLI release binary to $$(PREFIX)/bin' \
		'  install-system Install the noel CLI release binary to /usr/local/bin' \
		'  run          Run the noel CLI; pass RUN_ARGS="..." for CLI args' \
		'  dogfood     Run dev TUI with rebuild/restart watcher' \
		'  fmt          Check Rust formatting' \
		'  lint         Run clippy with warnings denied' \
		'  test         Run all Rust tests' \
		'  check        Run fmt, lint, and test' \
		'  check-ci     Run check with the pinned CI toolchain inside Docker' \
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
		'  ts-parity-chord     Regenerate only the Chord goldens inside Docker' \
		'  ts-parity-env       Regenerate only the LocalEnv goldens inside Docker' \
		'  ts-parity-pi-env    Regenerate only the pi-env daemon goldens inside Docker' \
		'  ts-parity-pi-env-ssh Regenerate only the pi-env SSH deployment goldens inside Docker' \
		'  chord-interop       Run noel-chord against upstream Chord over WebSockets' \
		'  env-interop         Run upstream pi-env client tests against noel-env serve, and the noel-env client tests against upstream pi-env' \
		'  ts-parity-update    Refresh fixtures from moving TS reference inside Docker' \
		'  ts-parity-drift     Check moving TS reference for fixture drift' \
		'  ts-parity-agent     Check drift and optionally dispatch NOEL_PARITY_AGENT_COMMAND' \
		'  smoke-claude-opus-oauth  Run manual Claude Opus OAuth smoke' \
		'  clean        Remove Cargo build output'

build:
	$(CARGO) build --all

release:
	$(CARGO) build --release -p noel-cli

# The daemon is uploaded to remote machines, so its profile trades build time
# for size (profile.env-daemon in Cargo.toml).
env-daemon:
	$(CARGO) build --profile env-daemon -p noel-env-daemon

install:
	CARGO="$(CARGO)" $(INSTALL_BUILD_SCRIPT)
	install -d "$(BINDIR)"
	install -m 0755 target/release/noel "$(BINDIR)/noel"

install-system: release
	$(SUDO) install -d "$(SYSTEM_BINDIR)"
	$(SUDO) install -m 0755 target/release/noel "$(SYSTEM_BINDIR)/noel"

run:
	$(CARGO) run -p noel-cli -- $(RUN_ARGS)

dogfood:
	CARGO="$(CARGO)" scripts/dogfood-dev.sh $(DOGFOOD_ARGS)

fmt:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --all-targets --all-features -- -D warnings

# NOEL_TEST_ISOLATION: no test may fall back to ~/.noel or refresh models.
test:
	NOEL_TEST_ISOLATION=1 $(CARGO) test --all

check: fmt lint test

check-ci:
	$(DOCKER) build -q -f Dockerfile.ci -t $(RUST_CI_IMAGE) .
	mkdir -p "$(HOME)/.cache/noel-ci-cargo"
	$(DOCKER) run --rm --user "$$(id -u):$$(id -g)" \
		-v "$(CURDIR):/src" -w /src \
		-v "$(HOME)/.cache/noel-ci-cargo:/cargo" -e CARGO_HOME=/cargo -e HOME=/tmp \
		-e CARGO_TARGET_DIR=/src/target/ci \
		$(RUST_CI_IMAGE) make check

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
	$(CARGO) test -p noel-parity
	$(CARGO) test -p noel-ai --lib matches_ts
	$(CARGO) test -p noel-ai --lib catalog
	$(CARGO) test -p noel-config --lib model
	$(CARGO) test -p noel-cli --test model_parity
	$(CARGO) test -p noel-core --lib upstream_agent_tool_loop_fixture_documents_model_callable_tools
	$(CARGO) test -p noel-core --lib system_prompt_matches_ts_builder
	$(CARGO) test -p noel-cli --bin noel prompt_and_context_files_match_ts_discovery
	$(CARGO) test -p noel-core --lib tool_parity_tests
	$(CARGO) test -p noel-config --lib _ts
	$(CARGO) test -p noel-core --lib input_expansion_matches_ts_session
	$(CARGO) test -p noel-cli --test rpc_parity get_commands_matches_upstream
	$(CARGO) test -p noel-cli --test rpc_parity golden_
	$(CARGO) test -p noel-chord
	$(CARGO) test -p noel-env
	$(CARGO) test -p noel-env-daemon
	$(MAKE) ts-parity-drift

ts-parity-build:
	$(DOCKER) build -f Dockerfile.ts-parity \
		--build-arg TS_REFERENCE_REPO="$(TS_REFERENCE_REPO)" \
		--build-arg TS_REFERENCE_REF="$(TS_REFERENCE_REF)" \
		--build-arg CHORD_REFERENCE_REF="$(CHORD_REFERENCE_REF)" \
		-t $(TS_PARITY_IMAGE) .

ts-parity-fixtures: ts-parity-build
	mkdir -p "$(TS_PARITY_FIXTURES_DIR)"
	$(DOCKER) run --rm -v "$(CURDIR)/$(TS_PARITY_FIXTURES_DIR):/fixtures" $(TS_PARITY_IMAGE)

ts-parity-chord: ts-parity-build
	mkdir -p "$(TS_PARITY_FIXTURES_DIR)"
	$(DOCKER) run --rm -v "$(CURDIR)/$(TS_PARITY_FIXTURES_DIR):/fixtures" $(TS_PARITY_IMAGE) \
		sh -c 'for fixture in /ts-parity/chord-*-fixture.ts; do npx tsx "$$fixture" /fixtures || exit 1; done'

# Durable's env conformance cases against NodeExecutionEnv, from the pinned
# checkout, for noel-env's LocalEnv.
ts-parity-env: ts-parity-build
	mkdir -p "$(TS_PARITY_FIXTURES_DIR)"
	$(DOCKER) run --rm -v "$(CURDIR)/$(TS_PARITY_FIXTURES_DIR):/fixtures" $(TS_PARITY_IMAGE) \
		npx tsx /ts-parity/local-env-fixture.ts /fixtures

# Upstream pi-env's client driving its own daemon (both from the pinned
# checkout) through a tap, for noel-env serve's goldens. Runs as the caller, so
# the fixture is the caller's.
ts-parity-pi-env: ts-parity-build
	mkdir -p "$(TS_PARITY_FIXTURES_DIR)"
	$(DOCKER) run --rm --user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR)/$(TS_PARITY_FIXTURES_DIR):/fixtures" $(TS_PARITY_IMAGE) \
		node --conditions=source /ts-parity/pi-env-fixture.ts /fixtures

# Upstream pi-env's SSH deployment (ssh.ts) through a stand-in for ssh
# (fake-ssh.sh), for noel-env's SSH goldens. No network.
ts-parity-pi-env-ssh: ts-parity-build
	mkdir -p "$(TS_PARITY_FIXTURES_DIR)"
	$(DOCKER) run --rm --network none --user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR)/$(TS_PARITY_FIXTURES_DIR):/fixtures" $(TS_PARITY_IMAGE) \
		node --conditions=source /ts-parity/pi-env-ssh-fixture.ts /fixtures

ts-parity-update:
	$(MAKE) ts-parity-fixtures TS_REFERENCE_REF="$(TS_PARITY_TRACKING_REF)"

# Upstream's client and server hosts (in the ts-parity image) against
# noel-chord's examples, over WebSockets on Unix sockets.
chord-interop: ts-parity-build
	CARGO="$(CARGO)" DOCKER="$(DOCKER)" TS_PARITY_IMAGE="$(TS_PARITY_IMAGE)" scripts/chord-interop.sh

# Upstream pi-env's client tests (in the ts-parity image) against noel-env
# serve, which they start directly; upstream's daemon runs them first. Then
# noel's client tests against upstream's pi-env, after noel-env serve.
env-interop: ts-parity-build
	CARGO="$(CARGO)" DOCKER="$(DOCKER)" TS_PARITY_IMAGE="$(TS_PARITY_IMAGE)" scripts/env-interop.sh

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
