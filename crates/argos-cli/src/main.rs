mod commands;
mod platform_select;

use argos_privileged::protocol::WindowsLayout;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// CLI-facing mirror of [`WindowsLayout`] -- mapped rather than derived so
/// `argos-privileged` (which runs elevated) never links against `clap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LayoutArg {
    /// One pure-Rust FAT32 partition (phase 3 M3/M2, backlog #43/#42): no
    /// external programs, no mounting. An install.wim over FAT32's 4GiB
    /// file limit is split into install.swm parts automatically. Works on
    /// Linux and macOS. GPT-partitioned, so it boots UEFI firmware only.
    /// The only layout Argos produces -- an earlier two-partition
    /// UEFI:NTFS scheme (backlog #27) was retired once this one was
    /// validated on real hardware from both hosts, on both firmwares
    /// (decision point M4.3, see docs/architecture.md).
    Fat32,
    /// The same FAT32 media, but MBR-partitioned and carrying Argos's own
    /// boot records, so it boots on **legacy BIOS machines too** (phase 3
    /// M6, backlog #45). Windows 10 only -- Windows 11 requires UEFI.
    #[value(name = "fat32-bios")]
    Fat32Bios,
}

impl From<LayoutArg> for WindowsLayout {
    fn from(arg: LayoutArg) -> Self {
        match arg {
            LayoutArg::Fat32 => WindowsLayout::Fat32,
            LayoutArg::Fat32Bios => WindowsLayout::Fat32Bios,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "argos",
    version,
    about = "Create bootable installer USB drives, safely.",
    long_about = "Create bootable Windows and Linux installer USB drives, in \
the spirit of Rufus, for both legacy BIOS/MBR and modern UEFI/GPT machines, \
from Linux or macOS.\n\n\
Start with `list` to find the device, then `write` to it. Writing always \
prints exactly what it's about to overwrite and asks for the device path \
to be typed back before touching anything -- there is no way to trigger a \
write by accident. `argos` re-elevates itself (`pkexec` where available on \
Linux, `sudo` otherwise) to actually perform it.",
    after_help = "EXAMPLES:\n    \
argos list\n        \
Find the device -- e.g. /dev/sdb on Linux, /dev/diskN on macOS.\n\n    \
argos write some.iso --device /dev/sdb\n        \
Write a Linux ISO byte-for-byte, or a Windows installer ISO as a FAT32 \
volume (add --layout fat32-bios for legacy BIOS/MBR).\n\n    \
argos verify /dev/sdb --iso some.iso\n        \
Re-check a device against the image it was written from, without writing \
anything. Note the argument order flips relative to `write`: the device is \
positional here, the ISO is --iso.\n\n\
Run `argos <command> --help` for that command's full set of flags."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List disks Argos can see, and whether each looks safe to write to.
    ///
    /// "Safe" means: not a system disk, reported removable by the OS, and on
    /// a USB bus. The device path this prints (e.g. /dev/sdb, /dev/diskN)
    /// is what `write` and `verify` below take.
    List,
    /// Write a Linux or Windows installer ISO image to a device.
    ///
    /// The ISO's kind is detected automatically. A Linux ISO is written
    /// byte-for-byte ("DD mode"); a Windows 10/11 installer ISO is written
    /// as a FAT32 volume, with install.wim split into install.swm parts
    /// automatically if it's over FAT32's 4GiB file limit. Before anything
    /// is touched, the exact target is printed and the device path has to
    /// be typed back to confirm -- there is no way to trigger this by
    /// accident, and no undo once confirmed.
    ///
    /// The ISO can be given either way: positionally, or as --iso. Accepting
    /// both exists because `verify` only ever took --iso (its device is
    /// positional instead), and typing --iso here out of habit used to be a
    /// hard error -- reported from real use within hours of --iso being
    /// documented for `verify`. Give exactly one; the flag exists to be
    /// forgiving, not to make positional the deprecated form.
    #[command(group(
        clap::ArgGroup::new("write_iso_source")
            .args(["iso", "iso_flag"])
            .required(true)
    ))]
    Write {
        /// Path to the ISO image.
        #[arg(value_name = "ISO")]
        iso: Option<PathBuf>,
        /// Path to the ISO image (equivalent to the positional argument
        /// above).
        #[arg(long = "iso", value_name = "ISO")]
        iso_flag: Option<PathBuf>,
        /// Target device, e.g. /dev/sdb.
        #[arg(long)]
        device: String,
        /// Skip the post-write read-back verification. Linux ISOs only --
        /// a Windows installer write is never verified inline; run `argos
        /// verify` afterward for that.
        #[arg(long)]
        no_verify: bool,
        /// Don't eject the device after a successful write.
        #[arg(long)]
        no_eject: bool,
        /// Allow writing to a disk the OS doesn't report as removable.
        /// Still refuses disks Argos detects as holding a system mount.
        #[arg(long)]
        i_know_what_im_doing: bool,
        /// On-disk layout for a Windows installer ISO (ignored for Linux
        /// ISOs, which are always written in DD mode).
        #[arg(long, value_enum, default_value_t = LayoutArg::Fat32)]
        layout: LayoutArg,
    },
    /// Re-run post-write verification against a device without writing again.
    ///
    /// Useful after the fact, or if `write` ran with --no-verify. Read-only
    /// -- nothing on the device is touched. Note the argument shape flips
    /// relative to `write`: the device is positional here, and the ISO is
    /// --iso.
    Verify {
        /// Target device, e.g. /dev/sdb.
        device: String,
        /// Path to the ISO image to compare against.
        #[arg(long)]
        iso: PathBuf,
        /// The layout the device was written with, for a Windows installer
        /// ISO (ignored for Linux ISOs).
        #[arg(long, value_enum, default_value_t = LayoutArg::Fat32)]
        layout: LayoutArg,
    },
    /// Print a shell completion script to stdout.
    ///
    /// Generated from the same definition the CLI itself uses, so it cannot
    /// drift out of step with the real flags:
    ///
    ///     argos completions zsh > ~/.zfunc/_argos
    Completions {
        /// Shell to generate for.
        shell: clap_complete::Shell,
    },
    /// Print this program's man page, in roff, to stdout.
    ///
    /// Hidden because its audience is distribution packaging, not people at
    /// a terminal: a formula or PKGBUILD runs `argos man > argos.1` rather
    /// than carrying a copy that has to be kept in step by hand.
    #[command(hide = true)]
    Man,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::List => commands::list::run(),
        Command::Write {
            iso,
            iso_flag,
            device,
            no_verify,
            no_eject,
            i_know_what_im_doing,
            layout,
        } => commands::write::run(commands::write::Args {
            // The ArgGroup on Command::Write guarantees exactly one of
            // these is Some.
            iso: iso.or(iso_flag).expect("the iso ArgGroup requires one"),
            device,
            no_verify,
            no_eject,
            i_know_what_im_doing,
            layout: layout.into(),
        }),
        Command::Verify {
            device,
            iso,
            layout,
        } => commands::verify::run(commands::verify::Args {
            device,
            iso,
            layout: layout.into(),
        }),
        Command::Completions { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }
        Command::Man => {
            write_man_page(&mut std::io::stdout()).map_err(argos_core::error::ArgosError::Io)
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(err.exit_code());
    }
}

/// Assembles one comprehensive `argos(1)` page, rather than
/// `clap_mangen::Man::new(cmd).render()`'s default: a thin top-level page
/// whose SUBCOMMANDS section cross-references `argos-list(1)`,
/// `argos-write(1)` and so on as if they were separate manual pages --
/// which this project doesn't generate or install, so `man argos-write`
/// would report nothing found. Every real subcommand's own synopsis,
/// description and options are inlined here instead, one `.SH` block each,
/// the way `dd(1)`/`rsync(1)` document every option on a single page.
///
/// `help` (clap's own auto-added dispatcher) and `man` (hidden on purpose,
/// see its own doc comment) are skipped -- neither is a subcommand worth a
/// section of its own.
fn write_man_page(w: &mut dyn std::io::Write) -> std::io::Result<()> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd.clone());
    man.render_title(w)?;
    man.render_name_section(w)?;
    man.render_synopsis_section(w)?;
    man.render_description_section(w)?;
    man.render_options_section(w)?;

    for sub in cmd.get_subcommands() {
        if sub.get_name() == "help" || sub.is_hide_set() {
            continue;
        }
        writeln!(w, ".SH {}", sub.get_name().to_uppercase())?;
        let sub_man = clap_mangen::Man::new(sub.clone());
        sub_man.render_synopsis_section(w)?;
        sub_man.render_description_section(w)?;
        sub_man.render_options_section(w)?;
    }

    man.render_extra_section(w)?;
    man.render_version_section(w)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap's own consistency check over the whole definition -- duplicate
    /// flags, conflicting short options, malformed value parsers. Cheap, and
    /// it fails at test time rather than the first time a user types the
    /// subcommand that happens to be broken.
    #[test]
    fn the_cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// `write` accepts its ISO either way -- positionally, or as `--iso` --
    /// unlike `verify`, which only ever takes `--iso`. Reported from real
    /// use: `--iso` documented for `verify` got typed for `write` too,
    /// within hours, and used to be a hard error.
    #[test]
    fn write_accepts_the_iso_either_positionally_or_as_a_flag() {
        let cases: [&[&str]; 2] = [
            &["argos", "write", "some.iso", "--device", "/dev/sdb"],
            &[
                "argos", "write", "--iso", "some.iso", "--device", "/dev/sdb",
            ],
        ];
        for argv in cases {
            let Command::Write {
                iso,
                iso_flag,
                device,
                ..
            } = Cli::try_parse_from(argv).unwrap().command
            else {
                panic!("parsed a Write out of {argv:?}");
            };
            assert_eq!(iso.or(iso_flag).unwrap().to_str().unwrap(), "some.iso");
            assert_eq!(device, "/dev/sdb");
        }
    }

    /// Giving neither, or both, has to fail -- silently preferring one
    /// would hide a typo (`--iso` given but ignored because a stray
    /// positional also matched) rather than reporting it.
    #[test]
    fn write_refuses_neither_or_both_iso_forms() {
        assert!(Cli::try_parse_from(["argos", "write", "--device", "/dev/sdb"]).is_err());
        assert!(Cli::try_parse_from([
            "argos", "write", "a.iso", "--iso", "b.iso", "--device", "/dev/sdb",
        ])
        .is_err());
    }

    /// The man page exists for packaging, so what matters is that it renders
    /// at all and documents every real subcommand's own options inline.
    #[test]
    fn the_man_page_renders_and_documents_every_subcommand() {
        let mut out = Vec::new();
        write_man_page(&mut out).expect("the man page should render");
        let roff = String::from_utf8(out).expect("roff is utf-8");

        assert!(roff.contains(".TH argos 1"), "missing the man page header");
        for heading in [".SH LIST", ".SH WRITE", ".SH VERIFY", ".SH COMPLETIONS"] {
            assert!(roff.contains(heading), "missing the {heading} section");
        }
        // The point of write_man_page over clap_mangen's default: each
        // subcommand's own flags are inlined here, not left as a
        // cross-reference to an argos-write(1) page nothing installs.
        // (roff escapes "--" as "\-\-", hence the odd-looking needles.)
        for flag in ["\\-\\-device", "\\-\\-iso", "\\-\\-layout"] {
            assert!(
                roff.contains(flag),
                "{flag} is missing from the inlined subcommand options"
            );
        }
        // "help" and "man" are neither hidden by clap nor worth a page.
        assert!(
            !roff.contains(".SH HELP"),
            "`help` should not get a section"
        );
        assert!(
            !roff.contains(".SH MAN"),
            "`man` is hidden; must not appear"
        );

        // Renders under a real man-page formatter, not just "is valid
        // UTF-8" -- but mandoc isn't guaranteed present on every CI runner
        // (it ships on macOS by default; Ubuntu needs it installed), so
        // this is a bonus check where available rather than a hard
        // dependency of the test.
        if std::process::Command::new("mandoc")
            .arg("--version")
            .output()
            .is_ok()
        {
            let output = std::process::Command::new("mandoc")
                .arg("-Tutf8")
                .arg("-Wfatal") // fail only on FATAL-severity mandoc diagnostics
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    child
                        .stdin
                        .take()
                        .expect("stdin was piped")
                        .write_all(roff.as_bytes())?;
                    child.wait_with_output()
                })
                .expect("mandoc is on PATH, so spawning and writing to it should succeed");
            assert!(
                output.status.success(),
                "mandoc reported a fatal error: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rendered = String::from_utf8(output.stdout).expect("mandoc's output is utf-8");
            // Plain body prose, not a bolded heading/flag: mandoc -Tutf8
            // simulates bold outside a real terminal by doubling letters
            // (WRITE -> WWRRIITTEE), so this has to be text that isn't
            // stylized to survive as a literal substring.
            assert!(
                rendered.contains("byte-for-byte"),
                "rendered page is missing write's own description text"
            );
        }
    }

    /// Every shell a package might generate for has to produce something. A
    /// silently empty completion file is worse than none, because it installs
    /// cleanly and then does nothing.
    #[test]
    fn completions_generate_for_every_supported_shell() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
            clap_complete::Shell::Elvish,
        ] {
            let mut out = Vec::new();
            let mut command = Cli::command();
            clap_complete::generate(shell, &mut command, "argos", &mut out);
            let script = String::from_utf8(out).expect("completion scripts are utf-8");
            assert!(!script.is_empty(), "{shell} produced an empty script");
            assert!(
                script.contains("argos"),
                "{shell} script does not mention the binary"
            );
        }
    }
}
