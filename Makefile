CARGO ?= cargo
DOCKER ?= docker
E2E_IMAGE ?= pi-e2e

.PHONY: help build release run fmt lint test check ci e2e docker-build docker-e2e clean

help:
	@printf '%s\n' \
		'Targets:' \
		'  build        Build the workspace' \
		'  release      Build the pi CLI release binary' \
		'  run          Run the pi CLI' \
		'  fmt          Check Rust formatting' \
		'  lint         Run clippy with warnings denied' \
		'  test         Run all Rust tests' \
		'  check        Run fmt, lint, and test' \
		'  ci           Run check and local tmux e2e' \
		'  e2e          Run tmux TTY e2e' \
		'  docker-e2e   Build and run Dockerized tmux TTY e2e' \
		'  clean        Remove Cargo build output'

build:
	$(CARGO) build --all

release:
	$(CARGO) build --release -p pi-cli

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

clean:
	$(CARGO) clean
