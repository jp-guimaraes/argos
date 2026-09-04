# Packaging

## Debian / Ubuntu (`.deb`)

```sh
packaging/build-deb.sh
```

Builds the whole workspace, generates shell completions and the man page
from the built `argos` binary (`argos completions <shell>` / `argos man`,
backlog #46) rather than keeping hand-maintained copies that can drift, and
packages `argos` + `argos-helper` together with [`cargo-deb`][cargo-deb]
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

## Homebrew (macOS)

Not this repository -- see [`jp-guimaraes/homebrew-argos`](https://github.com/jp-guimaraes/homebrew-argos).

## Arch / pacman (AUR)

`packaging/aur/PKGBUILD` builds `argos` and `argos-helper` from a tagged
release tarball, same shape as the `.deb` and the Homebrew formula: both
binaries side by side, and the man page and shell completions generated
from the built binary rather than kept as separate files.

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
