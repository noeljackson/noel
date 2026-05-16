.PHONY: help build test vet bench check run demo clean tidy install sessions

help:
	@awk 'BEGIN{FS=":.*##"; printf "Targets:\n"} /^[a-zA-Z0-9_-]+:.*##/ {printf "  %-12s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build pi and pi-tui-demo binaries at repo root
	go build -o pi ./cmd/pi
	go build -o pi-tui-demo ./cmd/pi-tui-demo

test: ## Run the full test suite
	go test ./...

vet: ## go vet ./...
	go vet ./...

bench: ## Run performance benchmarks
	./scripts/check-perf.sh

check: vet test bench ## Vet + test + performance benchmarks (CI-shaped local check)

run: ## Launch the interactive TUI against a fresh session
	go run ./cmd/pi

demo: ## Run the TUI against scripted mock events (no API call)
	go run ./cmd/pi-tui-demo

clean: ## Remove built binaries
	rm -f pi pi-tui-demo

tidy: ## go mod tidy
	go mod tidy

install: ## Install pi to $GOBIN (or $GOPATH/bin)
	go install ./cmd/pi

sessions: ## List existing session files
	@ls -lt ~/.pi/sessions/ 2>/dev/null | head -20 || echo "no sessions yet"
