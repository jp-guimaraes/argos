# Packaging

## Debian / Ubuntu (`.deb`)

```sh
packaging/build-deb.sh
```

Builds the whole workspace, generates shell completions and the man page
from the built `argos` binary (`argos completions <shell>` / `argos man`,
backlog #46) rather than keeping hand-maintained copies that can drift,
validates `argos.desktop` with `desktop-file-validate` (#94/G7.6), and
packages `argos` + `argos-helper` + `argos-gui` -- plus the GUI's launcher
entry, icon and polkit policy -- together with [`cargo-deb`][cargo-deb]
into `target/debian/argos_<version>-1_<arch>.deb`.

Install the result with:

```sh
sudo apt install ./target/debian/argos_*.deb
```

The two Debian-specific files this needs live in `packaging/debian/`:

- `copyright`, hand-written rather than `cargo-deb`'s auto-generated one.
  `cargo-deb` only knows how to embed a single license's full text under a
  `License:` header taken verbatim from the crate's `license` field --
  Argos's is `MIT OR Apache-2.0`, so its auto-generated copyright file
  named both but only ever included MIT's text, which is exactly what
  Debian policy (and `lintian`) flags as `copyright-not-using-common-license-for-apache2`.
  This file gives Apache-2.0 its own `License:` stanza pointing at
  `/usr/share/common-licenses/Apache-2.0`, which every Debian and Ubuntu
  system carries, instead of duplicating a few hundred lines of license
  text into the package.
- `changelog`, in the format Debian policy requires for non-native
  packages (an `-1` revision, as ours is). It deliberately doesn't
  duplicate `../../CHANGELOG.md` -- it just points at it.

### Why the asset paths in `crates/argos-cli/Cargo.toml` look inconsistent

They're not, but the rule is unintuitive enough to be worth writing down.
`cargo-deb` resolves an asset source path one of two ways:

1. **Exactly** `target/release/<name>` gets special handling: `cargo-deb`
   substitutes in the real target directory (accounting for
   `CARGO_TARGET_DIR`, cross-compilation, build profiles, workspaces) and,
   when it does the build itself, strips debug symbols from the result.
   [cargo-deb's own docs](https://github.com/kornelski/cargo-deb#readme)
   are explicit that trying to "fix" this into a relative path breaks it.
2. **Anything else** is a perfectly ordinary path, resolved relative to
   the `Cargo.toml` doing the packaging -- `crates/argos-cli/Cargo.toml`
   here, hence the `../../` on everything that isn't a binary.

Found the hard way: an earlier version of this config used `../../` on the
binaries too, which silently produced *unstripped* binaries (a real
`lintian` error, `unstripped-binary-or-object`) because `packaging/build-deb.sh`
builds the workspace ahead of time and hands `cargo deb` the result with
`--no-build` -- so `cargo-deb`'s own stripping never got a chance to run,
and case 2's plain-copy behavior doesn't strip anything. Fixed by both:
using the exact `target/release/` prefix (so `cargo-deb`'s own handling of
those two files is correct) *and* setting `[profile.release] strip = true`
in the workspace root `Cargo.toml`, which strips at build time regardless
of which tool does the building or how it's invoked afterward.

## macOS `.app` / `.dmg`

```sh
packaging/build-macos-app.sh
```

Builds the workspace (unless given an existing binary directory as its one
argument -- see below), then hand-assembles `target/macos/Argos.app` and
`target/macos/Argos-<version>.dmg`. No `cargo-bundle`: the bundle needs three
binaries from three different crates side by side (`argos` from `argos-cli`,
`argos-gui` from `argos-gui`, `argos-helper` from `argos-privileged`), plus a
`lipo` step for the universal release build and an `hdiutil` step, none of
which a bundler does -- the same reasoning that put `build-deb.sh` here
instead of `cargo deb -p argos-cli` alone.

All three binaries land in `Contents/MacOS/`, not the more conventional
`Contents/Resources/` or `Contents/Helpers/`. That placement is load-bearing,
not a style choice: `locate_helper_binary()` (in `argos-session`, shared by
the CLI and the GUI) looks for `argos-helper` as a sibling of
`current_exe()`, and `current_exe()` resolves symlinks -- so a
double-clicked `Argos.app` finds its helper with no code change at all, the
same way a plain `target/release/` checkout does. Moving the helper
elsewhere, which sounds tidier, breaks that lookup.

The icon is generated from `packaging/macos/icon-1024.png` via `sips` (to
produce every size an `.iconset` needs) and `iconutil -c icns` -- both ship
with the Xcode Command Line Tools, so building the icon costs no extra
dependency. The `.icns` itself is never committed, matching
`build-deb.sh`/`PKGBUILD`'s general posture of generating packaging
artifacts rather than hand-maintaining them; the PNG *is* committed, the
same way the Linux `.desktop` icon is, rather than being generated from the
`.svg` at build time -- that would need an SVG rasterizer as a build
dependency this project does not otherwise have a reason to carry. Both
icons are the same artwork (a USB stick, the one object this app actually
writes to), rasterized once from `packaging/linux/icons/hicolor/scalable/apps/argos.svg`.

`LSMinimumSystemVersion` in the generated `Info.plist` is `11.0`, read off a
real built binary (`otool -l target/release/argos-gui`, the `LC_BUILD_VERSION`
load command's `minos` field) rather than guessed -- it is whatever the
Rust/Xcode toolchain already targets by default on this project's supported
hosts.

### Universal binaries

`packaging/build-macos-app.sh <bin-dir>`, given an existing directory of
binaries, skips its own `cargo build` and packages whatever is there
instead. The release workflow's `macos-app` job uses this: it downloads both
Darwin targets' already-built tarballs (from the `build` job), `lipo`s
`argos`, `argos-gui` and `argos-helper` from each into one universal set,
and hands that directory to the script -- asking a lab user whether their
Mac is Intel or Apple Silicon is exactly the friction the GUI exists to
remove. A local, single-architecture build (`packaging/build-macos-app.sh`,
no argument) is for development and does not need this.

### Unsigned, on purpose -- for this phase

`spctl -a -vv target/macos/Argos.app` reports `rejected, source=no usable
signature`; a quarantined `Argos.app` downloaded from a browser will be
refused on first open by Gatekeeper, with `xattr -dr com.apple.quarantine`
as the documented way past it (see the main `README.md`). Signing and
notarizing were deliberately not done in this phase: a paid Developer ID
(US$99/yr) plus notarization on every release, for a project whose macOS
install story already has a better answer -- **Homebrew never sets the
quarantine bit**, sidestepping Gatekeeper entirely, and the tap
[`jp-guimaraes/homebrew-argos`](https://github.com/jp-guimaraes/homebrew-argos)
already exists for the CLI. The `.dmg` is the convenience download for
people who want the GUI without a Rust toolchain, not the primary
distribution path.

Confirmed live, on macOS 26.6.2: a hand-set quarantine attribute
(`xattr -w com.apple.quarantine`) followed by `open`-ing the app did *not*
trigger a block, but directly executing the quarantined binary (as a build
script's own verification step did, by accident, while this was being
written) did -- a real, on-screen dialog reading:

> Apple could not verify "Argos" is free of malware that may harm your Mac
> or compromise your privacy.

which is the current (post-Ventura) wording, not the older "is from an
unidentified developer" or "is damaged and can't be opened" phrasing some
older guides still describe. The dialog itself carries no "Open Anyway"
button in this version -- that lives in System Settings -> Privacy &
Security, as the main `README.md` already documents. Whether a plain Finder
double-click reaches the identical dialog (rather than the direct-execve
path that triggered it here) was not separately confirmed, but the
wording itself is not expected to depend on how the blocked launch was
attempted.

### Full Disk Access (TCC), not a signing problem

A write from `Argos.app` fails right after unmounting with a plain
`Operation not permitted (os error 1)` unless the app (or `argos-helper`
inside it) has **Full Disk Access** (System Settings -> Privacy & Security).
This is unrelated to code signing -- confirmed with `log stream --predicate
'subsystem == "com.apple.TCC" OR process == "argos-helper"'` while
reproducing it for real: the same permission macOS requires of Disk Utility
and similar tools for raw removable-device access, denied
(`authValue=0, authReason=5`) until granted and allowed
(`authValue=2, authReason=4`) after, confirmed end to end with a real
write-verify-eject against physical media. Full history and the log
evidence: issue #102 (closed).

**The grant does not survive a rebuild -- confirmed, twice, in #107
(closed).** The development binary carries an **ad-hoc** signature (a hash
of its own contents, changing on every rebuild), and TCC remembers a grant
by code identity, not by path. Rebuilding `packaging/build-macos-app.sh`
after an already-working grant reproduced `Operation not permitted` again,
with Full Disk Access now showing a *second*, unauthorized `argos-helper`
entry next to the old one; granting the new entry (dragging the binary from
Finder onto the list -- System Settings' own file picker cannot navigate
into a `.app`'s `Contents/MacOS` without "Show Package Contents", which is
not offered inside that specific dialog) restored the write. Repeated a
second time with the same result.

This is not only a development-iteration annoyance: **every tagged release
recompiles `argos-helper` with a new ad-hoc signature**, so a user
upgrading `Argos.app` (a new `.dmg` download, or a future Homebrew cask
bump) loses their Full Disk Access grant and has to re-add it after every
update, not only on first install. Worth surfacing prominently to users,
not buried as a troubleshooting footnote -- a stable signing identity (even
a free, non-Developer-ID one) is the real fix, tracked as a possible future
item rather than blocking this release.

One thing still not separately confirmed: whether a first-ever run of a
never-granted `argos-helper` shows an actual consent dialog, or fails
silently the way it did in testing (which had already had the permission
removed and re-added by hand, not a truly virgin binary).

### Homebrew (macOS)

Not this repository -- see [`jp-guimaraes/homebrew-argos`](https://github.com/jp-guimaraes/homebrew-argos).

The tap's existing formula builds the CLI from source, which is also why it
never hits Gatekeeper at all -- a local build has no quarantine bit to trip
over. The GUI is a different shape of artifact (a `.app` bundle, not a bare
binary in `PATH`) and needs a Homebrew **cask**, not a formula change; casks
are how Homebrew installs a pre-built `.app` into `/Applications`. Not done
here: like the AUR publish step below, it needs a commit to a separate
repository the maintainer controls. A starting point, once ready:

```ruby
cask "argos" do
  version "<version>"
  sha256 "<sha256 of Argos-<version>.dmg>"

  url "https://github.com/jp-guimaraes/argos/releases/download/v#{version}/Argos-#{version}.dmg"
  name "Argos"
  desc "Create bootable Windows and Linux installer USB drives"
  homepage "https://github.com/jp-guimaraes/argos"

  app "Argos.app"

  zap trash: [
    "~/Library/Application Support/argos",
  ]
end
```

## Arch / pacman (AUR)

`packaging/aur/PKGBUILD` builds `argos` and `argos-helper` from a tagged
release tarball, same shape as the `.deb` and the Homebrew formula: both
binaries side by side, and the man page and shell completions generated
from the built binary rather than kept as separate files.

**Not `argos-gui` yet.** `source=` pins a tagged release tarball on
purpose -- that is what proves the real `sha256sums` and build steps work,
not just this checkout -- and no tag published so far contains the
`argos-gui` crate or `packaging/linux/argos.desktop`; phase 4 has not
shipped a release. `build()`'s comment in the PKGBUILD spells out exactly
what to add once one has: `-p argos-gui`, the three GUI assets, and the
two extra runtime `depends`. The `.deb` (above) is not affected by this --
`packaging/build-deb.sh` always builds *this checkout*, never a tagged
tarball.

Validated the same way as the `.deb`: built for real with `makepkg` inside
a plain `archlinux:base-devel` container (CI does this on every push --
see `aur-package` in `.github/workflows/ci.yml`), then checked with
[`namcap`](https://wiki.archlinux.org/title/Namcap), Arch's own packaging
linter. One thing worth naming, since it isn't obvious from the diff: an
early version built `argos-debug`, an empty debug-symbol split package --
`[profile.release] strip = true` already strips the binaries before
`makepkg` sees them, so there's nothing left to split out.
`options=('!debug')` turns that off.

### This is not published to the AUR yet

A PKGBUILD living in this repository is not the same thing as an AUR
package -- the AUR is a *separate* git repository per package
(`ssh://aur@aur.archlinux.org/argos.git`), pushed to under the
maintainer's own AUR account and SSH key. That account/key setup is a
one-time, human step this repository's tooling can't do on anyone's
behalf. Once it exists:

```sh
git clone ssh://aur@aur.archlinux.org/argos.git
cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO argos/
cd argos && git add -A && git commit -m "1.5.1-1" && git push
```

### Keeping it in sync with releases

Unlike `packaging/build-deb.sh` (which always builds *this checkout*), the
PKGBUILD's `source=` pins a specific tagged release tarball and its
`sha256sums`. There is no way around updating both by hand for every new
Argos version -- same as the Homebrew formula's `url`/`sha256`:

```sh
# in packaging/aur/PKGBUILD: bump pkgver, then
curl -sL -o /tmp/argos.tar.gz \
  https://github.com/jp-guimaraes/argos/archive/refs/tags/vX.Y.Z.tar.gz
sha256sum /tmp/argos.tar.gz        # paste into sha256sums=(...)
makepkg --printsrcinfo > packaging/aur/.SRCINFO
```

[cargo-deb]: https://github.com/kornelski/cargo-deb

## The GUI's desktop integration (`linux/argos.desktop`, `linux/icons/`)

`argos.desktop` is what makes `argos-gui` show up in an application menu at
all, rather than being reachable only from a terminal. `StartupWMClass`
inside it must equal `APP_ID` in `crates/argos-gui/src/app.rs` (the value
passed to eframe's `with_app_id()`) or the taskbar shows a generic icon for
a running window instead of ours -- locked together by
`the_desktop_file_names_the_same_app_id_eframe_uses` in `argos-gui`'s own
tests, rather than trusted to stay in sync by hand.

The icon is a plain geometric USB stick in the design's own accent colour
(`theme.rs`'s `LIGHT.accent`, `#2B6E62`), not an illustrated mascot --
`draw_header` already reserves a 168x42 sprite for one, but that needs an
illustrator's frames this repository does not have, and a vendored binary
asset is exactly what decision M6.1 already declined for a boot record.
Both the scalable SVG and a rendered 256x256 PNG are installed under
`hicolor`, the icon theme every Linux desktop falls back to.

## The polkit policy (`linux/org.argos.helper.policy`)

Installed to `/usr/share/polkit-1/actions/` by both the `.deb` and the AUR
package -- the file itself has existed since #100 (G3), but nothing
installed it anywhere until this milestone (#94/G7), so the friendly
message below was never actually seen outside of a hand-copied test.
Without it `pkexec` still works and still authenticates -- it falls back to
its generic action and says "Authentication is required to run
/usr/bin/argos-helper as the super user", which tells a user nothing about
what is about to happen to their disk. With it, the dialog says so, in
English or Brazilian Portuguese depending on the session locale.

Two things worth knowing rather than working around:

- The policy pins an absolute path (`/usr/bin/argos-helper`), so a
  development build run out of `target/release/` matches no action and gets
  the generic message. That is fine; a second, dev-only policy file would not
  be worth its own maintenance.
- polkit picks the language from the **authentication agent's session
  locale**, not from Argos's own setting. Someone running Argos in Portuguese
  on an `en_US` desktop still sees the English polkit message.

`auth_admin`, not `auth_admin_keep`: `_keep` caches the authorization for
about five minutes, so a *second* destructive write in that window would
proceed with no prompt at all. Writing a USB stick is not a repeated
operation, so that caching buys nothing and quietly removes a confirmation.
