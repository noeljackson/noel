#!/usr/bin/env sh
set -eu

go test ./internal/benchmarks -run '^$' -bench . -benchmem
