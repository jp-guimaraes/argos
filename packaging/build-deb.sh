#!/usr/bin/env bash
# Builds a .deb for argos + argos-helper + argos-gui, from a clean release
# build of the whole workspace.
#
# Three things `cargo deb` cannot do on its own, which is why this script
# exists rather than a bare `cargo deb -p argos-cli` in CI:
#
# 1. The package needs binaries from three different crates (`argos` from
#    argos-cli, `argos-helper` from argos-privileged, `argos-gui` from
#    argos-gui) landing side by side -- `argos` looks for its helper as a
#    sibling of its own path (locate_helper_binary, in argos-session). A
#    single `cargo deb -p argos-cli` only knows about argos-cli's own
#    binary, so all three are built first with a plain `cargo build
#    --release --workspace`, and cargo-deb is then told `--no-build` and
#    picks up the other two as plain assets.
# 2. Shell completions and the man page are generated *from the built
#    binary* (`argos completions <shell>`, `argos man`) rather than kept as
#    hand-maintained files, so they can never drift from the real CLI (#46).
#    `cargo deb` has no hook to run a command mid-build, so this script runs
#    that step itself before invoking it.
# 3. The GUI's `.desktop` file is validated against the real spec
#    (`desktop-file-validate`) before packaging -- a syntax error there
#    would otherwise only surface as a launcher silently failing to appear,
#    on whichever desktop a user happens to be running (#94/G7.6).
#
# Usage:
#   packaging/build-deb.sh
#
# Produces target/debian/argos_<version>-1_<arch>.deb. Requires cargo-deb
# (`cargo install cargo-deb`, installed automatically if missing) and
# desktop-file-validate (`apt install desktop-file-utils`).

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

echo "==> validating the .desktop file"
desktop-file-validate packaging/linux/argos.desktop

echo "==> packaging"
cargo deb --no-build -p argos-cli "$@"
