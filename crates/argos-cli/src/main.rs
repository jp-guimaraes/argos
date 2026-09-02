mod commands;
mod platform_select;

use argos_privileged::protocol::WindowsLayout;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// CLI-facing mirror of [`WindowsLayout`] -- mapped rather than derived so
/// `argos-privileged` (which runs elevated) never links against `clap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LayoutArg {
    /// Two partitions: UEFI:NTFS boot + NTFS data (backlog #27). Needs
    /// mkfs.ntfs and ntfs-3g, so Linux-only. Still the default until the
    /// FAT32 layout is validated on real hardware (phase 3 M5).
    Ntfs,
    /// One pure-Rust FAT32 partition (phase 3 M3/M2, backlog #43/#42): no
    /// external programs, no mounting. An install.wim over FAT32's 4GiB
    /// file limit is split into install.swm parts automatically. Works on
    /// Linux and macOS.
    Fat32,
}

impl From<LayoutArg> for WindowsLayout {
    fn from(arg: LayoutArg) -> Self {
        match arg {
            LayoutArg::Ntfs => WindowsLayout::Ntfs,
            LayoutArg::Fat32 => WindowsLayout::Fat32,
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
        #[arg(long, value_enum, default_value_t = LayoutArg::Ntfs)]
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
        #[arg(long, value_enum, default_value_t = LayoutArg::Ntfs)]
        layout: LayoutArg,
    },
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
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(err.exit_code());
    }
}
