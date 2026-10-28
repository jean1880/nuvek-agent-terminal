#!/usr/bin/env bash
# Shared by the build-*.sh scripts (sourced, not run). Each runs as root in a clean container of
# its distribution: the release workflow's jobs, or `docker run` locally.
#
#   SRC  the source tree (read-only is fine): default, the repository this file is in
#   OUT  where the package lands: default $SRC/dist
#
# The tree is copied to a scratch directory first, so the checkout is never written to.

set -euo pipefail

SRC="${SRC:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
OUT="${OUT:-$SRC/dist}"
WORK="$(mktemp -d /tmp/agent-terminal-build.XXXXXX)"

# The package version: the [package] version in Cargo.toml (its first `version =` line).
version() {
    sed -n 's/^version = "\(.*\)"$/\1/p' "$SRC/Cargo.toml" | head -n 1
}

# Copies the tree to $WORK, without build output or git metadata, and enters it.
stage_source() {
    tar --exclude=./target --exclude=./dist --exclude=./.git -C "$SRC" -cf - . \
        | tar -C "$WORK" -xf -
    cd "$WORK"
}

# The rustup installer ensure_rust runs, pinned by version and checksum per architecture
# (static.rust-lang.org/rustup/archive/<version>/<target>/rustup-init.sha256).
RUSTUP_VERSION="1.29.2"
declare -A RUSTUP_SHA256=(
    [x86_64]="5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4"
    [aarch64]="a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697"
)

# A current stable Rust toolchain through rustup (distribution packages lag behind the crates'
# minimum Rust), unless one is already on PATH. The installer runs only if its checksum matches.
ensure_rust() {
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v cargo >/dev/null 2>&1; then
        local arch init
        arch="$(uname -m)"
        if [[ -z "${RUSTUP_SHA256[$arch]:-}" ]]; then
            echo "no pinned rustup-init for $arch" >&2
            return 1
        fi
        init="$WORK/rustup-init"
        curl --proto '=https' --tlsv1.2 -sSf -o "$init" \
            "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$arch-unknown-linux-gnu/rustup-init"
        echo "${RUSTUP_SHA256[$arch]}  $init" | sha256sum --check --strict -
        chmod +x "$init"
        "$init" -y --profile minimal --default-toolchain stable
        rm -f "$init"
    fi
    cargo --version
}

# Puts the built package $1 in $OUT and lists them.
collect() {
    mkdir -p "$OUT"
    cp -v "$1" "$OUT/"
    ls -l "$OUT"
}
