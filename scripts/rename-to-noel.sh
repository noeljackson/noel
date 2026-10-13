#!/usr/bin/env bash
# Renames this Rust port of pi to noel: binary, crates, state directory,
# environment variables, project directory, user-agents and product prose.
#
# The rename is generated, never edited by hand, so it can be redone after a
# rebase: drop the generated commit and the compatibility commit, run this
# script on the clean rebased tree, commit what it staged, and cherry-pick the
# compatibility commit. The rules are patterns, so pi-named code that landed in
# the meantime is renamed too. Every place that keeps the name pi is listed in
# EXCLUDED, DATA or KEEP below with its reason, and the script ends by
# printing what is left so a reviewer can scan it.
#
# The generated tree builds and is lint-clean by itself. Its tests pass except
# the ones that replay what upstream's fixtures name: a project's .pi/ tree and
# the PI_* rule of the system prompt. Those pass from the compatibility commit
# on, which reads a project's .pi/ after .noel/ and compares upstream's text
# through noel_config::with_noel_names.
#
# Needs a clean tree, bash, perl and git, plus the cargo of rust-toolchain.toml
# for the final `cargo fmt` (set CARGO to choose another cargo binary).
set -euo pipefail
export LC_ALL=C

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
cargo_bin="${CARGO:-cargo}"

die() {
  printf 'rename-to-noel: %s\n' "$*" >&2
  exit 1
}

[ -z "$(git status --porcelain)" ] || die "the working tree is not clean"
[ -d crates/pi-cli ] && [ ! -e crates/noel-cli ] ||
  die "crates/pi-cli is missing or crates/noel-cli exists: the tree is already renamed"
toolchain="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
case "$("${cargo_bin}" --version)" in
  "cargo ${toolchain} "*) ;;
  *) die "cargo fmt must come from toolchain ${toolchain}; put rustup's cargo first in PATH or set CARGO" ;;
esac

old_version="$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\([0-9]*\.[0-9]*\.[0-9]*\)"$/\1/p' Cargo.toml)"
[ -n "${old_version}" ] || die "Cargo.toml has no [workspace.package] version"
IFS=. read -r major minor _ <<<"${old_version}"
# Pre-1.0, the minor version is the compatibility boundary, and the rename
# breaks every script, config and dependency that names pi.
new_version="${major}.$((minor + 1)).0"

read -r -d '' worker_source <<'PERL' || true
use strict;
use warnings;
use re '/a';

# Whole files that keep pi: never rewritten or moved.
my @EXCLUDED = (
    [qr{^tests/fixtures/ts-parity/}, "upstream pi's captured output; parity tests compare it as is"],
    [qr{^scripts/ts-parity/}, "runs upstream pi's TypeScript inside Docker"],
    [qr{^Dockerfile\.ts-parity$}, "builds the upstream pi TypeScript reference image"],
    [qr{^\.pi/}, "this repository's tracked .pi/ (upstream pi extensions and prompts), which noel reads as a fallback"],
    [qr{^\.gitignore$}, "ignores files upstream pi writes (.pi_config/, pi-*.html, .pi/hf-sessions/)"],
    [qr{^scripts/rename-to-noel\.sh$}, "this script"],
);

# Data that moves with its crate and is never rewritten: a model or provider
# named pi there would be an id, not this product's name. The script checks at
# the end that these files are byte for byte what they were.
my @DATA = (
    [qr{^crates/(?:pi|noel)-ai/src/catalog_port_models\.json$},
        "the built-in models of this port's own providers, written by hand"],
);

# Spans that keep pi wherever they occur. They are set aside before any rule
# but the PRE ones.
# Two upstream contracts need no entry because no rule can touch them: the
# Claude Code identity (claude-cli/<version>) and the RPC/JSON wire names
# (record types, commands, fields), none of which contain pi.
my @KEEP = (
    ['upstream repository', qr{earendil-works/pi[\w.-]*},
        "upstream's repositories and packages (earendil-works/pi, pi-mono, \@earendil-works/pi-*)"],
    ['upstream prose',
        qr{\b[Uu]pstream(?:'s)?(?:\s|//[/!]?)+`?pi\b|\bquestions about\s+pi\b|\bupstream leaves at pi's},
        "text about upstream pi (\"upstream pi\", \"Upstream's `pi mcp`\", what its docs prompt section is for, its tools' working directory)"],
    ['upstream command', qr{\bpi update --models\b},
        "upstream's command that refreshes its hosted model catalog; noel has no such command"],
    ['upstream tool directory', qr{~/\.pi/agent/bin\b}, "upstream's managed tool directory, which noel does not have"],
    ['upstream API name', qr{\bpi-messages\b},
        "the API of upstream's Radius gateway, as model-catalog.json names it"],
    ['upstream sources', qr{\bpackages/[\w./-]+}, "paths in upstream's TypeScript monorepo"],
    ['upstream entry type', qr{\bpi\.bug-report\b}, "upstream's /bug session entry type"],
    ['upstream export name', qr{\b[Uu]pstream's default: pi-session-<session>\.html},
        "the comment naming upstream's default /export file; noel's own default is noel-session-<session id>.html"],
    ['upstream site', qr{\bpi\.dev\b}, "upstream's website"],
    ['upstream reference image', qr{\bpi-ts-parity\b}, "Docker image that runs the upstream pi TypeScript reference"],
    ["this repository's URLs", qr{(?<![\w.-])noeljackson/noel(?:\.git)?[/?\#][^\s<>"'`)\]]*},
        "what follows this repository's slug in a URL (an issue number, a tag, a release asset) names something that exists; only the slug moves"],
    ['another repository', qr{(?<![\w.-])noeljackson/pi[\w.-]+(?:[/?\#][^\s<>"'`)\]]*)?},
        "a repository of this owner whose name only starts with pi, with the rest of its URL"],
    ['Codex identity',
        qr{"originator",\s*HeaderValue::from_static\("pi"\)\);\s*headers\.insert\("User-Agent",\s*HeaderValue::from_static\("pi"\)},
        "Codex originator and User-Agent headers, pinned by the Codex parity fixture"],
    ['pi user-agent', qr{\b(?:PI_USER_AGENT|pi_user_agent|format_pi_user_agent)\b|\bpi \((?:\{platform\}|<platform>)},
        "upstream getPiUserAgent() identity, pinned by the Mistral parity fixtures"],
    ['parity prompt', qr{system_prompt: Some\("pi rust cli"}, "system prompt of requests compared with upstream fixtures"],
    ['other meanings', qr{\bRaspberry Pi\b|\b3\.14 pi\b|\bomicron pi rho\b}, "the computer, the number and the Greek letter"],
);

# Renames, in order: [name, pattern, replacement, required number of matches].
# PRE rules run before the KEEP spans are set aside.
my @PRE = (
    # The web tools' user-agent names its own homepage, not upstream's.
    ['web tools homepage', qr{\(\+https://github\.com/earendil-works/pi\)}, '(+https://github.com/noeljackson/noel)', 1],
    # This repository is renamed on GitHub with the product: its slug in every
    # form (an https, ssh, API, raw or release URL, a badge, a bare slug). A
    # longer name (noeljackson/pi-tools, noeljackson/pi.rs) is another
    # repository; noeljackson/pi.git is this one.
    ['this repository', qr{(?<![\w.-])noeljackson/pi(?![\w-])(?!\.(?!git(?![\w.-]))[\w-])}, 'noeljackson/noel', undef],
);
my @RULES = (
    # State directory: ~/.pi/agent becomes ~/.noel with the same layout inside.
    ['state dir', qr{(?<![\w.-])\.pi/agent(?![\w-])}, '.noel', undef],
    ['state dir default', qr{\.join\(CONFIG_DIR_NAME\)\.join\("agent"\)}, '.join(CONFIG_DIR_NAME)', 1],
    ['NOEL_DIR', qr{\bPI_CODING_AGENT_DIR\b}, 'NOEL_DIR', undef],
    ['NOEL_SESSION_DIR', qr{\bPI_CODING_AGENT_SESSION_DIR\b}, 'NOEL_SESSION_DIR', undef],
    # Environment variables (and the PI_* filter of /bug reports).
    ['PI_ prefix', qr{(?<![A-Za-z0-9_])PI_}, 'NOEL_', undef],
    # Crate paths (pi_core::) and identifiers that start with pi_.
    ['pi_ prefix', qr{(?<![A-Za-z0-9_])pi_}, 'noel_', undef],
    # Identifier segments: isolated_pi, is_pi, end_pi_like_upstream.
    ['_pi segment', qr{(?<=[A-Za-z0-9]_)pi(?![A-Za-z0-9])}, 'noel', undef],
    # The word: binary, crates (pi-cli), .pi/, .pi-extension.json, prose.
    ['pi', qr{\bpi\b}, 'noel', undef],
    ['Pi', qr{\bPi\b}, 'Noel', undef],
);

my $mode = shift @ARGV // die "usage: worker rewrite|moves|version|verify|report\n";

sub tracked {
    open my $git, '-|', 'git', 'ls-files', '-z' or die "git ls-files: $!\n";
    local $/ = "\0";
    my @files = <$git>;
    close $git or die "git ls-files failed\n";
    chomp @files;
    return @files;
}

sub excluded {
    my ($path) = @_;
    for my $rule (@EXCLUDED) {
        return $rule->[1] if $path =~ $rule->[0];
    }
    return undef;
}

sub data {
    my ($path) = @_;
    for my $rule (@DATA) {
        return $rule->[1] if $path =~ $rule->[0];
    }
    return undef;
}

sub read_text {
    my ($path) = @_;
    return undef if -l $path || !-f $path;
    open my $fh, '<:raw', $path or die "$path: $!\n";
    local $/;
    my $text = <$fh> // '';
    return index($text, "\0") >= 0 ? undef : $text;
}

sub write_text {
    my ($path, $text) = @_;
    open my $fh, '>:raw', $path or die "$path: $!\n";
    print {$fh} $text;
    close $fh or die "$path: $!\n";
}

sub rename_text {
    my ($text, $hits) = @_;
    for my $rule (@PRE) {
        $hits->{ $rule->[0] } += ($text =~ s{$rule->[1]}{$rule->[2]}g) || 0;
    }
    my @kept;
    for my $keep (@KEEP) {
        $text =~ s{$keep->[1]}{push @kept, $&; "\0$#kept\0"}ge;
    }
    for my $rule (@RULES) {
        $hits->{ $rule->[0] } += ($text =~ s{$rule->[1]}{$rule->[2]}g) || 0;
    }
    $text =~ s{\0(\d+)\0}{$kept[$1]}g;
    return $text;
}

if ($mode eq 'rewrite') {
    my (%hits, %renamed);
    for my $path (tracked()) {
        next if defined excluded($path) || defined data($path);
        my $text = read_text($path) // next;
        my $new = rename_text($text, \%hits);
        $renamed{$path} = $new if $new ne $text;
    }
    for my $rule (@PRE, @RULES) {
        my ($name, $required) = @$rule[0, 3];
        my $count = $hits{$name} // 0;
        die "rule '$name' matched $count times instead of $required; the code it targets changed\n"
            if defined $required && $count != $required;
    }
    write_text($_, $renamed{$_}) for sort keys %renamed;
    printf "rewrote %d files; matches per rule:\n", scalar keys %renamed;
    printf "  %-20s %5d\n", $_->[0], $hits{ $_->[0] } // 0 for @PRE, @RULES;
}
elsif ($mode eq 'moves') {
    # One round of moves: the first path component that the rules rename.
    my %seen;
    for my $path (tracked()) {
        next if defined excluded($path);
        my @parts = split m{/}, $path;
        for my $i (0 .. $#parts) {
            my $name = rename_text($parts[$i], {});
            next if $name eq $parts[$i];
            my $old = join '/', @parts[0 .. $i];
            print $old, "\0", join('/', @parts[0 .. $i - 1], $name), "\0" unless $seen{$old}++;
            last;
        }
    }
}
elsif ($mode eq 'version') {
    my ($old, $new) = @ARGV;
    my $toml = read_text('Cargo.toml');
    $toml =~ s{^(\[workspace\.package\]\n(?:[^\[\n].*\n|\n)*?version = ")\Q$old\E"}{$1$new"}m
        or die "Cargo.toml: workspace version $old not found\n";
    write_text('Cargo.toml', $toml);

    # Cargo.lock: bump the workspace crates (no source) and keep cargo's order,
    # packages and dependency lists sorted by name, which the rename changed.
    my $lock = read_text('Cargo.lock');
    my ($head, @blocks) = split /\n\n(?=\[\[package\]\]\n)/, $lock;
    my $name = sub { $_[0] =~ /^name = "([^"]+)"$/m ? $1 : die "Cargo.lock: package without name\n" };
    my $dep_name = sub { $_[0] =~ /^ "([^" ]+)/ ? $1 : die "Cargo.lock: bad dependency $_[0]\n" };
    for my $block (@blocks) {
        $block =~ s/\n+\z//;
        $block =~ s/^version = "\Q$old\E"$/version = "$new"/m unless $block =~ /^source = /m;
        $block =~ s{^(dependencies = \[\n)((?: "[^"\n]*",\n)+)}{
            my ($open, @deps) = ($1, split /(?<=\n)/, $2);
            my @order = sort { $dep_name->($deps[$a]) cmp $dep_name->($deps[$b]) or $a <=> $b } 0 .. $#deps;
            $open . join('', @deps[@order]);
        }me;
    }
    my @order = sort { $name->($blocks[$a]) cmp $name->($blocks[$b]) or $a <=> $b } 0 .. $#blocks;
    write_text('Cargo.lock', join("\n\n", $head, @blocks[@order]) . "\n");
}
elsif ($mode eq 'verify') {
    # What is staged, with a move paired only when the content is identical:
    # no excluded file may be among it, and a data file may only have moved.
    open my $git, '-|', 'git', 'diff', '--cached', '--raw', '-z', '-M100%', '--no-abbrev'
        or die "git diff: $!\n";
    local $/ = "\0";
    my @problems;
    while (defined(my $entry = <$git>)) {
        chomp $entry;
        my ($status) = $entry =~ /^:\d+ \d+ [0-9a-f]+ [0-9a-f]+ ([A-Z])\d*$/
            or die "git diff: unexpected entry $entry\n";
        for (1 .. ($status eq 'R' ? 2 : 1)) {
            my $path = <$git> // die "git diff: entry without a path\n";
            chomp $path;
            push @problems, "$path is excluded but staged" if defined excluded($path);
            push @problems, "$path is data but its content changed" if defined data($path) && $status ne 'R';
        }
    }
    close $git or die "git diff failed\n";
    die join('', map { "rename-to-noel: $_\n" } @problems) if @problems;
    printf "verified: no excluded file is staged, and data only moved: %s\n",
        join(' ', grep { defined data($_) } tracked());
}
elsif ($mode eq 'report') {
    my $left = qr{\b[Pp][Ii]\b|(?<![A-Za-z0-9_])(?:PI|pi)_|(?<=[A-Za-z0-9]_)pi(?![A-Za-z0-9])};
    my (%kept, %excluded, @unexpected, $inside_words);
    for my $path (tracked()) {
        my $text = read_text($path) // next;
        $inside_words += () = $text =~ /^(?!.*$left).*PI_.*$/mg;
        if (defined(my $reason = excluded($path) // data($path))) {
            my $lines = () = $text =~ /^.*$left.*$/mg;
            push @{ $excluded{$reason} }, "$path ($lines)" if $lines || defined data($path);
            next;
        }
        my @spans;
        for my $k (0 .. $#KEEP) {
            while ($text =~ /$KEEP[$k][1]/g) {
                push @spans, [$-[0], $+[0], $k];
            }
        }
        my %line_seen;
        while ($text =~ /$left/g) {
            my $at = $-[0];
            my $line = 1 + (() = substr($text, 0, $at) =~ /\n/g);
            my ($span) = grep { $_->[0] <= $at && $at < $_->[1] } @spans;
            my ($content) = substr($text, rindex($text, "\n", $at) + 1) =~ /^\s*(.*)/;
            my $entry = sprintf '%s:%d: %s', $path, $line, substr($content, 0, 110);
            next if $line_seen{ ($span ? $span->[2] : -1) . ":$line" }++;
            if ($span) { push @{ $kept{ $span->[2] } }, $entry } else { push @unexpected, $entry }
        }
    }
    print "pi left in the tree, per exception:\n";
    for my $k (sort { $a <=> $b } keys %kept) {
        printf "  %s: %s\n", @{ $KEEP[$k] }[0, 2];
        print "    $_\n" for @{ $kept{$k} };
    }
    for my $rule (@EXCLUDED, @DATA) {
        my $files = $excluded{ $rule->[1] } or next;
        printf "  files not rewritten, %s:\n", $rule->[1];
        my $line = '   ';
        for my $file (@$files) {
            if (length($line) + length($file) > 108) { print "$line\n"; $line = '   ' }
            $line .= " $file";
        }
        print "$line\n";
    }
    printf "  %d more lines contain PI_ only inside longer words (API_KEY and the like)\n", $inside_words // 0;
    if (@unexpected) {
        print "  UNEXPECTED, not covered by any rule or exception:\n";
        print "    $_\n" for @unexpected;
        exit 1;
    }
    print "  nothing unexpected\n";
}
else {
    die "unknown mode $mode\n";
}
PERL

worker() {
  perl -e "${worker_source}" -- "$@"
}

next_moves() {
  moves=()
  while IFS= read -r -d '' from && IFS= read -r -d '' to; do
    [ ! -e "${to}" ] || die "cannot move ${from}: ${to} exists (ignored files of an earlier build?)"
    moves+=("${from}" "${to}")
  done < <(worker moves)
}

next_moves
printf 'rename-to-noel: pi %s -> noel %s\n' "${old_version}" "${new_version}"
worker rewrite

moved=0
while :; do
  next_moves
  [ "${#moves[@]}" -gt 0 ] || break
  for ((i = 0; i < ${#moves[@]}; i += 2)); do
    git mv -- "${moves[i]}" "${moves[i + 1]}"
    printf 'moved %s -> %s\n' "${moves[i]}" "${moves[i + 1]}"
    moved=$((moved + 1))
  done
done

worker version "${old_version}" "${new_version}"
"${cargo_bin}" fmt --all
git diff --name-only -z | xargs -0 -r git add --
printf 'moved %d paths, staged %d files (git diff --cached --stat)\n' \
  "${moved}" "$(git diff --cached --name-only | wc -l)"

worker verify
worker report
