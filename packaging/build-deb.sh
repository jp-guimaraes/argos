#!/usr/bin/env bash
# Builds a .deb for argos + argos-helper, from a clean release build of the
# whole workspace.
#
# Two things `cargo deb` cannot do on its own, which is why this script
# exists rather than a bare `cargo deb -p argos-cli` in CI:
#
# 1. The package needs two binaries from two different crates (`argos` from
#    argos-cli, `argos-helper` from argos-privileged) landing side by side --
#    `argos` looks for its helper as a sibling of its own path
#    (locate_helper_binary in crates/argos-cli/src/commands/helper.rs). A
#    single `cargo deb -p argos-cli` only knows about argos-cli's own
#    binary, so both crates are built first with a plain `cargo build
#    --release --workspace`, and cargo-deb is then told `--no-build` and
#    picks up argos-helper as a plain asset.
# 2. Shell completions and the man page are generated *from the built
#    binary* (`argos completions <shell>`, `argos man`) rather than kept as
#    hand-maintained files, so they can never drift from the real CLI (#46).
#    `cargo deb` has no hook to run a command mid-build, so this script runs
#    that step itself before invoking it.
#
# Usage:
#   packaging/build-deb.sh
#
# Produces target/debian/argos_<version>-1_<arch>.deb. Requires cargo-deb
# (`cargo install cargo-deb`, installed automatically if missing).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! cargo deb --version >/dev/null 2>&1; then
    echo "==> installing cargo-deb"
    cargo install cargo-deb --locked
fi

echo "==> building the workspace"
cargo build --release --workspace

echo "==> generating completions and the man page from the built binary"
mkdir -p target/deb-assets
target/release/argos completions bash >target/deb-assets/argos.bash
target/release/argos completions zsh >target/deb-assets/_argos
target/release/argos completions fish >target/deb-assets/argos.fish
target/release/argos man >target/deb-assets/argos.1

echo "==> packaging"
cargo deb --no-build -p argos-cli "$@"
