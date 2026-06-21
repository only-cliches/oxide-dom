#!/bin/bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/solite-size-XXXXXX")"
trap 'rm -rf "$tmpdir"' EXIT

write_probe() {
    local dir="$1"
    local name="$2"
    local dep_line="$3"

    mkdir -p "$dir/src"
    cat >"$dir/Cargo.toml" <<EOF
[package]
name = "$name"
version = "0.1.0"
edition = "2024"

[dependencies]
$dep_line
EOF

    cat >"$dir/src/main.rs" <<'EOF'
use solite::{Instance, InstanceConfig};

fn main() {
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 64,
            height: 64,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        "import { render } from \"solite-runtime\"; render(() => __sol_createElement(\"div\"), __SOL_ROOT__);",
    )
    .expect("create instance");

    let _ = instance.tick();
    let _ = instance.render();
}
EOF
}

mkdir -p "$tmpdir/baseline/src"
cat >"$tmpdir/baseline/Cargo.toml" <<'EOF'
[package]
name = "baseline"
version = "0.1.0"
edition = "2024"

[dependencies]
EOF
cat >"$tmpdir/baseline/src/main.rs" <<'EOF'
fn main() {}
EOF

write_probe \
    "$tmpdir/solite-core" \
    "solite-core" \
    "solite = { path = \"$repo_root\", default-features = false }"

write_probe \
    "$tmpdir/solite-default" \
    "solite-default" \
    "solite = { path = \"$repo_root\" }"

build_release() {
    local dir="$1"
    cargo build --release --manifest-path "$dir/Cargo.toml"
}

build_release "$tmpdir/baseline"
build_release "$tmpdir/solite-core"
build_release "$tmpdir/solite-default"

bytes() {
    wc -c <"$1" | tr -d '[:space:]'
}

strip_copy_size() {
    local src="$1"
    local tmp="$src.stripped"
    cp "$src" "$tmp"
    if strip "$tmp" >/dev/null 2>&1; then
        bytes "$tmp"
    else
        rm -f "$tmp"
        printf 'n/a'
        return
    fi
    rm -f "$tmp"
}

to_mib() {
    awk -v bytes="$1" 'BEGIN { printf "%.2f", bytes / 1024 / 1024 }'
}

report() {
    local label="$1"
    local path="$2"
    local size="$3"
    local stripped="$4"

    printf "%-16s %10s bytes  %6s MiB" "$label" "$size" "$(to_mib "$size")"
    if [[ "$stripped" != "n/a" ]]; then
        printf "  stripped: %10s bytes  %6s MiB" "$stripped" "$(to_mib "$stripped")"
    else
        printf "  stripped: unavailable"
    fi
    printf "\n"
    printf "  %s\n" "$path"
}

baseline_bin="$tmpdir/baseline/target/release/baseline"
core_bin="$tmpdir/solite-core/target/release/solite-core"
default_bin="$tmpdir/solite-default/target/release/solite-default"

baseline_size="$(bytes "$baseline_bin")"
core_size="$(bytes "$core_bin")"
default_size="$(bytes "$default_bin")"

baseline_stripped="$(strip_copy_size "$baseline_bin")"
core_stripped="$(strip_copy_size "$core_bin")"
default_stripped="$(strip_copy_size "$default_bin")"

printf "Release binary sizes\n"
report "baseline" "$baseline_bin" "$baseline_size" "$baseline_stripped"
report "solite-core" "$core_bin" "$core_size" "$core_stripped"
report "solite-default" "$default_bin" "$default_size" "$default_stripped"

printf "\nBinary growth vs baseline\n"
awk \
    -v b="$baseline_size" \
    -v c="$core_size" \
    -v d="$default_size" \
    'BEGIN {
        printf "  core delta:    %10d bytes  %6.2f MiB\n", c - b, (c - b) / 1024 / 1024;
        printf "  default delta: %10d bytes  %6.2f MiB\n", d - b, (d - b) / 1024 / 1024;
        printf "  default-core:  %10d bytes  %6.2f MiB\n", d - c, (d - c) / 1024 / 1024;
    }'

if [[ "$baseline_stripped" != "n/a" && "$core_stripped" != "n/a" && "$default_stripped" != "n/a" ]]; then
    printf "\nStripped growth vs baseline\n"
    awk \
        -v b="$baseline_stripped" \
        -v c="$core_stripped" \
        -v d="$default_stripped" \
        'BEGIN {
            printf "  core delta:    %10d bytes  %6.2f MiB\n", c - b, (c - b) / 1024 / 1024;
            printf "  default delta: %10d bytes  %6.2f MiB\n", d - b, (d - b) / 1024 / 1024;
            printf "  default-core:  %10d bytes  %6.2f MiB\n", d - c, (d - c) / 1024 / 1024;
        }'
fi
