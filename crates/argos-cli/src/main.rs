mod commands;
mod platform_select;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    },
    /// Re-run post-write verification against a device without writing again.
    Verify {
        /// Target device, e.g. /dev/sdb.
        device: String,
        /// Path to the ISO image to compare against.
        #[arg(long)]
        iso: PathBuf,
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
        } => commands::write::run(commands::write::Args {
            iso,
            device,
            no_verify,
            no_eject,
            i_know_what_im_doing,
        }),
        Command::Verify { device, iso } => {
            commands::verify::run(commands::verify::Args { device, iso })
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(err.exit_code());
    }
}
