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
    about = "Create bootable installer USB drives, safely."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List disks Argos can see, and whether each looks safe to write to.
    List,
    /// Write a Linux or Windows installer ISO image to a device.
    Write {
        /// Path to the ISO image.
        iso: PathBuf,
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
            device,
            no_verify,
            no_eject,
            i_know_what_im_doing,
            layout,
        } => commands::write::run(commands::write::Args {
            iso,
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
        Command::Man => clap_mangen::Man::new(Cli::command())
            .render(&mut std::io::stdout())
            .map_err(argos_core::error::ArgosError::Io),
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(err.exit_code());
    }
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

    /// The man page exists for packaging, so what matters is that it renders
    /// at all and names the subcommands a package's users will look up.
    #[test]
    fn the_man_page_renders_and_documents_the_subcommands() {
        let mut out = Vec::new();
        clap_mangen::Man::new(Cli::command())
            .render(&mut out)
            .expect("the man page should render");
        let roff = String::from_utf8(out).expect("roff is utf-8");

        assert!(roff.contains(".TH argos 1"), "missing the man page header");
        for subcommand in ["list", "write", "verify"] {
            assert!(
                roff.contains(subcommand),
                "man page does not mention `{subcommand}`"
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
