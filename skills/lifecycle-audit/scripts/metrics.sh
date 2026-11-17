#!/usr/bin/env bash
# Code quality metrics for a Rust workspace (with optional Swift/TS).
# Usage: metrics.sh <repo-root> [size|all]
# Read-only. Every section is a cheap grep; interpret the numbers, do not
# treat any threshold here as a rule.
set -u
root="${1:-.}"
mode="${2:-all}"
cd "$root" || exit 1

src_rs() { find . -name '*.rs' -not -path '*/target/*' -not -path '*/tests/*' -not -name '*_tests.rs' "$@"; }
all_rs() { find . -name '*.rs' -not -path '*/target/*' "$@"; }

section() { printf '\n== %s\n' "$1"; }

section "production lines per crate"
for c in crates/*; do
  [ -d "$c/src" ] || continue
  n=$(find "$c/src" -name '*.rs' -not -name '*_tests.rs' | xargs cat 2>/dev/null | wc -l)
  printf '%8d %s\n' "$n" "$c"
done | sort -n

section "largest source files (>3000 lines) and where their test module starts"
all_rs | xargs wc -l 2>/dev/null | awk '$1>3000 && $2!="total"' | while read -r n f; do
  t=$(grep -n '^mod tests' "$f" | head -1 | cut -d: -f1)
  printf '%8d %s  (prod ~%s)\n' "$n" "$f" "${t:-$n}"
done

section "other languages"
find . \( -name '*.swift' -o -name '*.ts' -o -name '*.kt' \) -not -path '*/node_modules/*' -not -path '*/Generated/*' -not -path '*/target/*' | xargs wc -l 2>/dev/null | tail -1

[ "$mode" = "size" ] && exit 0

section "longest functions (production region, rough)"
src_rs | while read -r f; do
  end=$(grep -n '^mod tests' "$f" | head -1 | cut -d: -f1); end=${end:-999999}
  awk -v F="$f" -v E="$end" 'NR>E{exit} /^(pub(\([a-z]+\))? )?(async )?fn |^    (pub(\([a-z]+\))? )?(async )?fn /{if(name)print NR-start, F":"start, name; start=NR; name=$0}' "$f"
done | sort -rn | head -15 | cut -c1-140

section "unwrap/expect in production region of the largest files"
all_rs | xargs wc -l 2>/dev/null | awk '$1>3000 && $2!="total"{print $2}' | while read -r f; do
  end=$(grep -n '^mod tests' "$f" | head -1 | cut -d: -f1); end=${end:-$(wc -l <"$f")}
  printf '%4d %s\n' "$(head -n "$end" "$f" | grep -cE '\.unwrap\(\)|\.expect\(')" "$f"
done

section "allow attributes"
grep -rhoE '#!?\[allow\([a-z_:, ]+\)\]' --include='*.rs' . | sort | uniq -c | sort -rn | head

section "TODO/FIXME/HACK (excluding generated)"
grep -rnE '\b(TODO|FIXME|HACK)\b' --include='*.rs' --include='*.swift' --include='*.ts' . 2>/dev/null | grep -v Generated | grep -v node_modules | grep -v target | cut -c1-120 | head

section "string-literal comparisons per file (== \"x\")"
src_rs | xargs grep -cE '(==|!=) "[a-zA-Z_]+"' 2>/dev/null | grep -v ':0$' | sort -t: -k2 -n | tail -8

section "untyped JSON (serde_json::Value) per file"
src_rs | xargs grep -c 'serde_json::Value\|: Value\b\|json!(' 2>/dev/null | grep -v ':0$' | sort -t: -k2 -n | tail -6

section "Result<_, String> per crate (prod)"
for c in crates/*; do
  [ -d "$c/src" ] || continue
  n=$(grep -rE 'Result<[^>]*, *String>' "$c/src" --include='*.rs' | grep -v _tests | wc -l)
  [ "$n" -gt 0 ] && printf '%4d %s\n' "$n" "$c"
done

section "process::exit sites"
grep -rn 'process::exit' --include='*.rs' . | grep -v tests | grep -v target | cut -c1-110

section "unsafe blocks vs SAFETY comments"
printf 'unsafe: %s  SAFETY: %s\n' "$(grep -rn 'unsafe ' --include='*.rs' . | grep -v _tests | grep -v target | wc -l)" "$(grep -rn 'SAFETY' --include='*.rs' . | grep -v target | wc -l)"

section "duplicate dependency versions"
cargo tree -d -e normal 2>/dev/null | grep -E '^[a-z]' | sort -u | head -30

section "test lines per crate"
for c in crates/*; do
  t=$(find "$c" -path '*/tests/*' -name '*.rs' -o -path "$c/*" -name '*_tests.rs' | xargs cat 2>/dev/null | wc -l)
  it=$(grep -rhc '#\[test\]\|#\[tokio::test\]' "$c/src" 2>/dev/null | paste -sd+ | bc 2>/dev/null)
  printf '%-28s test_lines=%-6s inline_tests=%s\n' "$c" "$t" "${it:-0}"
done

section "sleep-based waits in tests"
grep -rn 'sleep(' --include='*.rs' . | grep -E '_tests.rs|/tests/' | grep -v target | wc -l
grep -rn '#\[ignore' --include='*.rs' . | grep -v target | wc -l | sed 's/^/ignored tests: /'

section "env var reads"
grep -rhoE 'env::var(_os)?\("[A-Z_]+"' --include='*.rs' crates 2>/dev/null | sed 's/.*("//' | sort | uniq -c | sort -rn | head -8
prefix=$(grep -rhoE 'env::var(_os)?\("[A-Z]+_' --include='*.rs' crates 2>/dev/null | sed 's/.*("//' | sort | uniq -c | sort -rn | head -1 | awk '{print $2}')
if [ -n "$prefix" ] && [ -f README.md ]; then
  for v in $(grep -rhoE "env::var(_os)?\(\"${prefix}[A-Z_]+\"" --include='*.rs' crates | sed 's/.*("//;s/"//' | sort -u); do
    grep -rq "$v" README.md docs 2>/dev/null || echo "undocumented: $v"
  done
fi

section "diagnostics: eprintln! per crate vs crates using tracing/log"
for c in crates/*; do
  n=$(grep -rn 'eprintln!' "$c/src" --include='*.rs' 2>/dev/null | grep -v _tests | wc -l)
  [ "$n" -gt 0 ] && printf '%4d %s\n' "$n" "$c"
done
grep -rl 'tracing::\|log::' crates/*/src 2>/dev/null | xargs -n1 dirname 2>/dev/null | sort -u | sed 's/^/uses tracing: /'

section "secrets and persistence"
grep -rn 'mode(0o6\|mode(0o7' --include='*.rs' crates | grep -v _tests | wc -l | sed 's/^/restricted-mode writes: /'
grep -rn 'getrandom\|OsRng' --include='*.rs' crates | grep -v _tests | wc -l | sed 's/^/system RNG uses: /'
grep -rn 'api_key\|access_token\|refresh_token' --include='*.rs' crates | grep -iE 'debug!|info!|warn!|println!|eprintln!' | grep -v _tests | cut -c1-120 | sed 's/^/possible secret in log: /' | head -5

section "lint config"
grep -n 'workspace.lints' Cargo.toml || echo "no [workspace.lints]"
grep -n 'edition' Cargo.toml | head -1
ls deny.toml 2>/dev/null || echo "no deny.toml"
grep -ln 'cargo deny\|cargo audit' .github/workflows/*.yml 2>/dev/null || echo "no deny/audit in CI"
