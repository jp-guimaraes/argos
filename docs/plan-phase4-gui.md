# Phase 4 plan: a graphical interface (`argos-gui`)

Written 2026-09-06. This plan assumes [`docs/architecture.md`](architecture.md)
as the description of the current state and does not restate it. Section 4 is
not a proposal: it records a spike that has already been run.

## 1. Why now

The README has said from the start that Argos is "architected so a GUI can be
added later without reworking the core logic", and `argos-core/src/lib.rs`,
`argos-platform/src/lib.rs` and `argos-cli/src/commands/write.rs` each carry a
comment anticipating this phase. Phase 3 closed the use case that motivates the
project — Windows installer media for the maintainer's ageing lab machines,
produced from a Mac or from the lab's own Linux boxes — validated against real
hardware on both hosts and both firmwares.

What is left is reaching the people who do not open a terminal. That is the
whole of phase 4.

Goal: one window, in the spirit of Rufus. Light, no system dependencies beyond
what any desktop already ships, working on macOS and Linux, in English and
Brazilian Portuguese.

## 2. Decisions taken before planning

| Decision | Choice |
|---|---|
| Toolkit | **egui / eframe** — pure Rust, MIT/Apache, static binary, no cmake, no GTK, no webview |
| Localization | **One binary with a language selector** — detected from the OS, switchable in the window, persisted |
| Scope of v1 | **Write + progress + cancel.** `verify` and `list` stay CLI-only |
| Confirmation | **Retype the device path**, always — the same guard the CLI has |

Constraints inherited from the project (`CONTRIBUTING.md`, and decision M6.1 in
`architecture.md`): minimal dependencies; nothing copyleft or under a bespoke
non-permissive licence, since the project declined a GPL relicense specifically
to keep MIT/Apache; `argos-privileged` stays minimal and dumb by design; and the
safety identity — system disks refused unconditionally, TOCTOU re-validation in
the helper, retype-to-confirm — must not be weakened by the GUI.

### 2.1 Why eframe 0.33, specifically

Pinned, not merely "latest". Verified against the registry on 2026-09-06:

| eframe | `rust-version` | default renderer |
|---|---|---|
| 0.36.x | 1.95 | wgpu |
| 0.34.0 | 1.92 | wgpu |
| **0.33.3** | **1.88** | **glow (OpenGL)** |

0.33.3's MSRV is exactly this workspace's `rust-version = "1.88"`, and it still
defaults to glow/OpenGL rather than wgpu. Both matter. Taking 0.36 would force
the workspace MSRV to 1.95, contradicting `CONTRIBUTING.md`'s "1.88+"; and the
hosts this project serves are the same vintage as the machines it writes media
for, where plain OpenGL is the safer bet than a Vulkan/Metal/DX12 stack with a
GL fallback. Licence is `MIT OR Apache-2.0`, so M6.1 is honoured.

### 2.2 The dependency-budget concession, stated honestly

eframe pulls in on the order of 150–250 transitive crates. That is in real
tension with "keep it minimal", and the plan does not pretend otherwise. The
defensible scope is the one the workspace manifest already uses for `ctrlc`:
the rule targets `argos-privileged` specifically. `argos-gui` is a leaf —
nothing depends on it, the `argos` CLI binary is unaffected, and
**`argos-helper`'s dependency tree, the only tree that runs as root, gains
exactly zero crates.** That claim is checkable, and it is the argument to make.

## 3. Two defects found while planning

Both confirmed by reading the tree, and both load-bearing for what follows.

1. **The helper's exit code is discarded.**
   `crates/argos-cli/src/commands/helper.rs:170` matches
   `Event::Error { message, .. }` and wraps it as
   `ArgosError::Io(io::Error::other(message))`, whose `exit_code()` is 19. So
   every helper-side failure exits **19**, whatever the helper actually
   reported — 12 (system disk), 17 (checksum mismatch), 18 (cancelled). The GUI
   needs that code to localize errors, so this gets fixed; because it changes
   user-visible CLI behaviour it gets its own PR and CHANGELOG entry rather
   than riding along inside a refactor.

2. **A stale doc comment.** `crates/argos-privileged/src/main.rs:14` still says
   *"**Known gap**: cancellation is not wired end-to-end yet"*, while line 50 of
   the same file spawns `watch_for_cancel` and lines 100–122 describe the
   working mechanism in detail. Free cleanup.

## 4. The hard problem, and the spike that settled it

`helper.rs:57-61` picks `pkexec` on Linux and **`sudo` on macOS**. Under a
windowed app there is no TTY and `sudo` simply fails. This was the one item that
could invalidate the rest of the plan, so it was spiked before anything else was
designed in detail.

### 4.1 What was tested

**Route A:** `osascript -e 'do shell script "…" with administrator privileges'`
— the system's own authorization dialog, so **the app never sees the password**;
works unsigned, with no Developer ID. Its obstacle is that `do shell script`
returns stdout only when the command finishes, which would kill both the
progress bar and cancellation. The answer is a pair of **FIFOs** standing in for
the pipes `Command::spawn` would have given us:

```
mkfifo $RUNDIR/plan $RUNDIR/events        # $RUNDIR 0700, one per run
do shell script "'…/argos-helper' < '…/plan' > '…/events'" with administrator privileges
```

`do shell script` runs the string through `/bin/sh` as root, so the redirections
apply: the helper's stdin is the plan FIFO and its stdout is the events FIFO.
Everything downstream of that is unchanged.

### 4.2 Results — macOS 26.6.2, against a protocol-faithful stub

| Case | Result |
|---|---|
| Normal elevated run | 22 events streamed **live** (250 ms apart, matching the emitter — no buffering); exit 0 |
| Cancel an elevated run | `CANCEL_SIGNAL` written by the **unprivileged** parent reached the **root** helper through the FIFO; it stopped and emitted its error event |
| User dismisses the auth dialog | osascript exits 1 with `execution error: User canceled. (-128)` on stderr; parent gives up 274 ms later |
| Force-quit the parent mid-write | helper gone in **under 0.3 s**, with ~4 s of work left — the EOF safety net survives |
| Changes required to `argos-privileged` | **none** |

### 4.3 The one non-obvious finding

The first version of the spike opened each FIFO end blocking, on its own thread,
reasoning that `sh`'s two redirections and the parent's two opens form a
handshake. They do — right up until the user dismisses the authorization dialog.
Then osascript exits, `sh` never runs, no counterpart end is ever opened, and
**both threads block in `open()` forever**. Observed for real: the parent process
still alive and stuck with the child already gone.

The fix is to open both FIFOs **`O_RDWR`**, which never blocks, and to poll the
child with `try_wait()` so a failed elevation is detected rather than waited on.
`O_RDWR` also preserves the EOF-on-parent-death semantics `watch_for_cancel`
depends on: what makes the helper's `read()` return 0 is the last *write* end
closing, and the parent's `O_RDWR` handle is one of those. Row 4 of the table
above is the evidence.

This is worth recording because it is invisible on the happy path and only ever
shows up as a hung window in the hands of a user who changed their mind.

### 4.4 What this means for the design

- `Event::Error`'s `-128` case maps to `ArgosError::NotConfirmed` (exit 27),
  the variant whose own comment already reads "the user declined before
  anything was touched". Semantically exact.
- The held-open plan FIFO handle *is* the `ChildStdin` equivalent, so one
  `Canceller` type serves both the CLI's SIGINT handler and the GUI's button.
- **Never elevate the GUI process itself.** `sudo argos-gui` is not a fallback.
  The privilege separation is the safety architecture; running egui, a file
  dialog and a font stack as root would be a straight regression.

### 4.5 Rejected alternatives

- **Plain temp files** instead of FIFOs: a growing events *file* has no EOF
  semantics, no cancel channel, and needs polling. A cancel-sentinel file plus a
  pidfile plus SIGTERM would be three ad-hoc mechanisms replacing one that
  already exists and already has unit tests.
- **`sudo -A` with an osascript askpass**: much simpler — the pipe path stays
  byte-identical to today — but **the password would transit the Argos
  process**. Defensible (it is the `ssh-askpass` model) and strictly worse for a
  project whose centrepiece is privilege separation. Kept as the documented
  fallback.
- **A unix domain socket**: same semantics, but requires adding `--socket` to
  `argos-helper`. FIFOs need zero helper change. Kept as the fallback if
  sandboxing ever forbids FIFOs in the temp directory.
- **`SMJobBless` / `SMAppService` with a signed helper**: Apple's "correct"
  answer, and an architectural regression here — it makes the privileged side a
  persistent, launchd-managed root daemon, the opposite of `argos-helper`'s
  one-shot design, in exchange for a Developer ID at US$99/yr, notarization on
  every release, and an XPC service replacing the stdin/stdout JSON protocol.

Recorded as decision **M-GUI.1**, including that
`AuthorizationExecuteWithPrivileges` (what `do shell script … with
administrator privileges` uses underneath) has been deprecated since 10.7 and
still functions — a known contingency rather than a future surprise.

### 4.6 Linux

`pkexec` already renders the desktop's polkit agent, so the mechanism works
today. What is missing is `packaging/linux/org.argos.helper.policy`, so the
dialog says something useful instead of "Authenticate to run
/usr/bin/argos-helper as root".

Use **`auth_admin`, not `auth_admin_keep`**: `_keep` caches the authorization
for about five minutes, so a *second* destructive write inside that window would
proceed with **no prompt at all**. For a tool whose first priority is never
writing to the wrong disk, silently removing the prompt from the second write is
a safety regression, and it buys nothing — writing a USB stick is not a repeated
operation.

Polkit policies support `<message xml:lang="pt_BR">`, so the auth prompt is
translated for free. Caveat to document rather than fight: polkit picks the
language from the *authentication agent's session locale*, not from Argos's own
selector, so a user who sets Argos to PT-BR on an `en_US` desktop still sees the
English polkit message.

The policy pins an absolute path (`/usr/bin/argos-helper`), so a development
build run from `target/release/` matches no action and falls back to pkexec's
generic message. That **works**, it is merely less pretty; document it, do not
work around it.

One real failure mode to handle: launched from a `.desktop` file under a bare WM
with no polkit agent running, `pkexec` has no TTY to fall back to and fails.
Detect the exit code and say so — "no authentication agent is running; install
one, or run `argos write` from a terminal" — because otherwise it looks like a
crash.

## 5. Architecture

Two new crates. No behaviour change in the CLI.

```
argos-core ── argos-platform ── argos-platform-{linux,macos}
     │              │
     └──── argos-privileged (lib + bin argos-helper)
                    │
              argos-session   ← NEW: UI-agnostic orchestration + elevation
                 ╱      ╲
          argos-cli    argos-gui   ← NEW: eframe/egui
```

### 5.1 `argos-session`

Takes what is currently private inside `argos-cli`. The rule: it decides and
executes, and never prints or reads from the user. Confirmation belongs to each
frontend.

```rust
pub fn prepare_write(platform: &dyn PlatformOps, req: &WriteRequest) -> Result<PreparedWrite>;

pub struct PreparedWrite { pub device: Device, pub iso: PathBuf, pub preview: WritePreview, /* plan */ }

/// Everything a confirmation prompt needs, as data.
pub enum WritePreview {
    Dd { image_size_bytes: u64 },
    Windows { layout: WindowsFat32Plan, actions: Vec<CopyAction>, firmware: WindowsLayout },
}

/// Split so the caller holds the Canceller *before* it blocks on the stream --
/// which is why cancellation no longer has to be a signal handler closing over
/// a mutex.
pub fn spawn(plan: &Plan, ui: ElevationUi) -> Result<Running>;
impl Running { pub fn stream(self, sink: &mut dyn EventSink) -> Result<Outcome, SessionError>; }

#[derive(Clone)] pub struct Canceller(/* the held-open plan channel */);
pub enum ElevationUi { Terminal, Graphical }
```

`ElevationUi::Terminal` keeps today's behaviour byte-for-byte (`pkexec` on
Linux if present, else `sudo`); `Graphical` selects the routes in section 4.

Moving out of `argos-cli`: `canonicalize_iso_path`, `human_size`,
`locate_helper_binary`, `run_plan`/`stream_helper_events`,
`check_device_is_offerable`, `windows_fat32_plan_for`, the source-collision
check, the classify-then-preflight ordering, and the `Plan` construction — each
with its existing tests, names unchanged so the diff stays auditable.

Staying in `argos-cli`: `Presenter`/`PlainPresenter` and their six tests, both
`confirm_*_or_abort` functions verbatim (now formatting from `WritePreview`
rather than recomputing), every `println!`, the `ctrlc` registration, and
`main.rs`'s exit-code handling. `argos-session` must depend on none of `ctrlc`,
`indicatif`, `console` or `clap`.

`prepare_write` calls the helper's own `plan_copy_actions`/`fat32_layout_for`,
never a private copy — the lesson already recorded at `write.rs:149-163`, where
the CLI's own duplicate check went stale and started refusing media the helper
handled fine.

**Two ordering traps for the extraction.** The DD path is `metadata →
check_capacity → collision → confirm`; the Windows path is `plan → capacity →
collision → confirm`. They differ, and a tidy-up that unifies them changes which
error the user sees first. And `Presenter::new()` must still be constructed
before the first event, because it probes `is_attended()`.

### 5.2 `argos-gui`

A reducer-shaped app so the interesting parts are testable with no window:

```rust
pub enum AppState { Idle, Preparing, AwaitingConfirmation { .. }, Running(RunState),
                    Done { .. }, Failed { .. }, Cancelled }
pub fn reduce(state: AppState, msg: WorkerMsg) -> AppState;
```

`prepare_write` and `execute` run on a worker thread; events return over an
`mpsc` channel; the worker calls `ctx.request_repaint()`. **Throttle that to
~30 Hz**: the helper emits a progress event per block, so a 5 GB write produces
thousands of them, and a repaint per event makes the window unusable.

Device enumeration also runs off the UI thread — `diskutil -plist` plus a
`diskutil info -plist` per disk is a fork/exec storm, and Linux reads sysfs, the
udev database, `/proc/mounts` and possibly UDisks2 over D-Bus.

Refresh: a manual button plus a 2 s poll, single-flighted, and **never while
writing** — on macOS that would mean `diskutil` calls against a disk the helper
holds `O_EXCL`, and this project has already been bitten by Disk Arbitration
auto-mounting a freshly written partition. That is a correctness constraint, not
a performance one.

If the selected device changes underneath the selection, a pure
`reconcile()` distinguishes *gone* (keep it shown, greyed, START disabled) from
*replaced* — same path, different serial or size — which clears the selection and
warns. Neither is the safety guarantee; `prepare_write` re-refreshes and the
helper re-validates. They turn a late refusal into an early, comprehensible one.

**Window layout** (Rufus's order: source → target → options → action):

| Field | Behaviour |
|---|---|
| Image | path + Browse + drag-and-drop; below it the detected kind and size |
| Target | dropdown of `is_safe_to_write()` devices; a "show all devices" checkbox mirrors `--i-know-what-im-doing`. **System disks are never listed in either mode** — a device you cannot select is a device you cannot mis-click |
| Boot | **the layout checkbox**, always present — see below |
| Warnings | `.swm` split note, capacity, source-on-target collision |
| Write | opens the confirmation modal |
| Progress | bar + phase label + bytes/total + Cancel |

**The layout checkbox is a first-class control**: always visible, never hidden
behind an advanced menu. `--layout` is binary, so it maps directly.

| State | Flag | Produces |
|---|---|---|
| unchecked (default) | `--layout fat32` | GPT; boots UEFI only |
| checked | `--layout fat32-bios` | MBR + Argos's own boot records: boots legacy BIOS **and** most UEFI firmware, which accepts MBR-partitioned removable media |

A line underneath states the consequence in each state — it is the most
consequential choice in the window, and the layout's name alone does not convey
it, which is exactly why `confirm_windows_fat32_write_or_abort` already prints
that explanation today (`write.rs:281-287`). Since old lab machines are the use
case that motivates the project, this control does not get buried. For a DD-mode
Linux ISO the flag genuinely does not apply — the image carries its own
partition table — so the checkbox is shown **disabled with the reason beside
it**, not silently greyed and not removed.

**Confirmation** is the CLI's block rendered as a modal, ending in a text field:
Write stays disabled until the typed text equals `device.platform_id` exactly,
case-sensitively. Rufus only asks for OK; Argos stays stricter.

### 5.3 One protocol change: a typed `Phase`

`Event::Phase` currently carries a `String` produced by `format!("{phase:?}")`.
The GUI would have to key localized labels off a `Debug` representation, so
renaming `Phase::FormattingFat32` would silently produce an unlabelled progress
bar with no compile error. Make `Phase` `Serialize`/`Deserialize`
(`rename_all = "snake_case"`) and wrap it on the wire in an untagged
`PhaseWire { Known(Phase), Unknown(String) }`, which parses both the old
`"Writing"` and the new `"writing"` in either direction of version skew. The
CLI's printed output stays byte-identical.

## 6. Localization

**Detection:** `LC_ALL`/`LC_MESSAGES`/`LANG` on Linux; `defaults read -g
AppleLanguages` on macOS, because `LANG` is frequently unset for a `.app`
launched from Finder. Manual override in the window. Precedence: config file >
`ARGOS_LANG` (for tests) > OS detection > English.

**Config:** hand-rolled, ~25 lines, `key = "value"` lines, no `directories` and
no TOML parser. `$XDG_CONFIG_HOME/argos/config.toml` on Linux;
`~/Library/Application Support/argos/config.toml` on macOS.

**Catalogue:** hand-rolled and compile-time-checked, living in `argos-session`
so error localization sits next to `SessionError`. A `Strings` struct of
`&'static str` for fixed labels — a missing field is a compile error — and `fn`
pointers for parameterized messages, so placeholder arity is type-checked too.
That is a stronger guarantee than any file-based scheme, at two languages and
~80 strings.

Not gettext, despite `docs-site` already using PO: there is no well-maintained
pure-Rust `.mo` runtime, and `gettext-rs` links C `libintl`, a system dependency
that directly violates the "easy to install" constraint on macOS. The escape
hatch, if translator ergonomics ever matter, is a `build.rs` generating
`Strings` from a `.po` at compile time — so the decision is not a dead end.

**Errors.** No i18n dependency in `argos-core`; its `#[error(...)]` strings stay
English. Unprivileged-side failures — the large majority — are matched on the
typed `ArgosError` with its payload available. Helper-side failures arrive
pre-stringified, so they map from `exit_code` (10–27, already stable) to a
localized template, **after** the discard bug in section 3 is fixed. When a code
has no mapping, show the raw English message behind a localized "the privileged
helper reported an error:" — never a bare number. Always keep the raw English
text in a Details pane even when a translation exists; that is what a bug report
needs.

**The CLI stays English.** Its help text, man page and five completion scripts
are all generated from the same clap definitions, and `packaging/build-deb.sh`
runs `argos man` on the build machine — a locale-dependent `argos man` would
ship whatever language the CI runner happened to have. The door stays open
cheaply because `Strings` lives in `argos-session`, not `argos-gui`.

**`human_size` is not edited.** Its output feeds `argos list`'s `{:>10}` column;
changing the separator would silently change CLI alignment. Add
`human_size_localized(bytes, lang)` for the GUI instead.

## 7. Packaging

**macOS:** `packaging/build-macos-app.sh`, hand-rolled in the spirit of the
existing `build-deb.sh` rather than `cargo-bundle` — the bundle needs two
binaries from two different crates side by side, plus `lipo` and `hdiutil`
steps no bundler does. `argos-gui`, `argos-helper` and `argos` all go in
`Contents/MacOS/`; that placement is **load-bearing**, because it is what makes
`locate_helper_binary()`'s sibling-of-`current_exe()` lookup work with no code
change. `Info.plist` carries `CFBundleLocalizations = (en, pt-BR)`, which is
what makes `AppleLanguages` report `pt-BR` for this app.

Shipping unsigned means a downloaded `.dmg` is quarantined and Gatekeeper
refuses it on first open. Do not sign or notarize in phase 4 — US$99/yr plus
notarization on every release, for a project whose macOS install story already
has a better answer: **Homebrew never sets the quarantine bit**, and the tap
`jp-guimaraes/homebrew-argos` already exists. Extend the formula, keep the
`.dmg` as the convenience download, and document `xattr -dr
com.apple.quarantine` in the README and in `docs-site/po/pt-BR.po` — lab users
are exactly who will hit this.

**Linux:** extend what already exists; no AppImage, no Flatpak (whose sandbox
makes raw `/dev/sdX` writes and `pkexec` a fight not worth having). Add to
`[package.metadata.deb]` and the PKGBUILD: the `argos-gui` binary, a `.desktop`
file with `GenericName[pt_BR]`/`Comment[pt_BR]`, icons, and the polkit policy.
`build-deb.sh` already runs `cargo build --release --workspace`, so it barely
changes — which is a point in favour of this route. `StartupWMClass` in the
`.desktop` file must equal the `with_app_id()` passed to eframe or the taskbar
shows a generic icon; assert that agreement in a test, the same way completions
are generated from the binary rather than hand-maintained.

**CI:** eframe needs `libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev
libxcb-xfixes0-dev` to build on `ubuntu-latest` (egui's own CI list, minus the
parts Argos does not use). glow and winit `dlopen` libGL and the Wayland
libraries, so no `-dev` packages for those. Nothing opens a window at build or
unit-test time, so the whole suite stays headless. Adding eframe to the
workspace does mean every `--workspace` build now compiles it; cache it or
accept the wall-time.

**Release:** add `-p argos-gui`, ship it in the **same** tarball (one product,
one version, and `locate_helper_binary` makes the sibling relationship a hard
requirement), and add a `lipo` job producing a universal `.app`/`.dmg` — asking
a lab user whether their Mac is Intel or Apple Silicon is precisely the friction
the GUI exists to remove.

## 8. Testing

A `FakePlatform` behind a `test-fixtures` feature on `argos-platform` — the
first time `PlatformOps` is mocked in this project, which is worth a note in
`CONTRIBUTING.md` with its scope limit spelled out: it is for testing
*orchestration*, never device safety. A fake that says a disk is safe proves
nothing about the code that decides that.

Headless unit tests cover `reduce()` across every state × message, the
retype-to-confirm predicate (case-sensitive, trimmed, empty is false),
`reconcile()`, the localized formatter, and catalogue placeholder parity. A
helper-stub binary behind `test-overrides` (same posture as the existing
`ARGOS_TEST_FORCE_REMOVABLE`) covers the runner end-to-end without root.

No screenshot testing in phase 4: `egui_kittest`'s snapshots need the wgpu
feature — the very dependency the 0.33/glow choice avoids — and snapshots of a
UI still being designed churn on every tweak.

Manual and irreplaceable, in the evidence-first style the M5/M6 rows already
use: a real write to real USB hardware from the GUI on both hosts and both
layouts; cancel mid-write on both; unplugging the stick mid-write; force-quitting
the GUI mid-write; launching a quarantined `.app` from Finder; launching from
the `.desktop` file in GNOME and KDE with and without a polkit agent; and
language auto-detection on a `pt_BR` desktop and a `pt-BR` Mac.

## 9. Milestones

Each is its own branch and PR, with CI green (fmt, clippy `-D warnings`, tests
on ubuntu and macos). Ordered to retire risk early.

**G0 is already done** — section 4 is its report. The macOS elevation route is
proven against a protocol-faithful stub, so nothing downstream is speculative.

| # | Scope | Acceptance |
|---|---|---|
| **G1** | `argos-session` created; the extraction in 5.1; `argos-cli` refactored onto it | `cargo test --workspace` green with **no test deleted**; before/after transcripts of `argos list`, `argos write` (DD and Windows, aborted at the prompt), `argos verify`, `argos --help`, `argos man \| md5` prove byte-identical output |
| **G2** | Typed `Phase`/`PhaseWire`; `SessionError` carrying `exit_code`; CLI exits with the helper's real code; the stale doc comment removed | Round-trip tests for both wire forms; CLI's printed phases unchanged; CHANGELOG entry naming the exit-code fix as user-visible |
| **G3** | The `Elevator` abstraction with section 4's route; the polkit policy; the no-agent error; a hidden `--elevation graphical` flag to drive it from a terminal | **A real write to a real USB stick on macOS through the auth dialog**; cancel and force-quit confirmed against the real helper; **`argos-helper` has a zero-line diff** |
| **G4** | eframe window; device combo, refresh, poll, `reconcile`; file picker and drag-and-drop; detected kind; the layout checkbox; capacity; the confirmation modal. Write not yet wired | Reducer, predicate and `reconcile` tests; builds clippy-clean on both OSes with the new apt step |
| **G5** | Worker thread; throttled event stream; progress and ETA; cancel; result panels; eject | **Real hardware write from the GUI on both hosts**, with the checkbox producing GPT unchecked and MBR checked (verified with `fdisk -l` / `diskutil list`); cancel confirmed on both. README's "No GUI exists today" updated |
| **G6** | `Lang`/`Strings`; detection; config; language menu; error localization; the `xml:lang` and `[pt_BR]` strings | Placeholder-parity tests; manual check on a `pt_BR` desktop and a `pt-BR` Mac |
| **G7** | Linux packaging: `.deb` assets, `.desktop`, icons, policy, PKGBUILD, `desktop-file-validate` in CI | lintian with no `E:`; namcap green; CI installs the `.deb` and asserts the paths |
| **G8** | macOS packaging: `build-macos-app.sh`, icon, the `lipo`/`.dmg` release job, Gatekeeper docs, the Homebrew cask PR | A tagged pre-release producing 3 tarballs + `.deb` + universal `.dmg`; `lipo -info` shows both arches |
| **G9** | Real-hardware validation and docs: a "Guiding decisions (phase 4)" section recording M-GUI.1–5, a phase-4 status row, `docs-site` and `po/pt-BR.po` | A lab machine booting from media written by the GUI |

Once G1 and G2 land, **G3 and G4 can run in parallel** on separate branches —
G4 needs only the `Elevator` signature, not its macOS implementation. That
matters here: this project has already run a macOS agent and a Linux agent
against the same repository at once, which is why branch-and-PR is the rule.

## 10. Open uncertainties

Stated rather than papered over.

1. `rfd`'s off-main-thread contract on macOS is unverified; the plan calls the
   file dialog on the UI thread, with `AsyncFileDialog` as the fallback.
2. `rfd` 0.17's `xdg-portal` feature appears to reach libdbus via `dlopen` (its
   dependency list carries no `zbus`, so there is no conflict with
   `argos-platform-linux`'s zbus 5) — confirm at implementation time.
3. The apt list is egui's known-good superset, not a proven minimum.
4. `pkexec`'s exit-code split between "could not authorize" and "dismissed"
   needs re-reading in `pkexec(1)` before being relied on.
5. Whether `with prompt` is honoured on current macOS, and whether the auth
   dialog attributes the request to "osascript" rather than "Argos" while the
   app is unsigned. Neither blocks; both should be documented as warts.
6. Gatekeeper's exact wording for an unsigned app on the target macOS version —
   confirm on hardware before writing it into the README.
7. Arch runtime `depends` are a starting guess; namcap in the existing CI job is
   the source of truth.
8. Binary size and CI wall-time impact of eframe — measure in G4, do not assert.
