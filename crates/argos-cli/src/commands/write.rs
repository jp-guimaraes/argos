//! `argos write`: the full destructive flow, as a terminal presents it.
//!
//! The flow itself -- resolve, refuse, classify, preflight, build the plan,
//! elevate, stream -- lives in `argos-session`, so this file is now only the
//! two things that are genuinely a terminal's job: asking for confirmation on
//! stdin, and saying what happened on stdout.

use super::progress::Presenter;
use argos_core::error::{ArgosError, Result};
use argos_privileged::protocol::WindowsLayout;
use argos_session::{
    self as session, human_size, ElevationUi, Outcome, PreparedWrite, WritePreview, WriteRequest,
};
use std::io::Write;
use std::path::PathBuf;

pub struct Args {
    pub iso: PathBuf,
    pub device: String,
    pub no_verify: bool,
    pub no_eject: bool,
    pub i_know_what_im_doing: bool,
    pub layout: WindowsLayout,
    pub elevation: ElevationUi,
}

pub fn run(args: Args) -> Result<()> {
    let platform = session::current_platform();

    let prepared = session::prepare_write(
        &platform,
        &WriteRequest {
            iso: args.iso,
            device_id: args.device,
            no_verify: args.no_verify,
            eject: !args.no_eject,
            allow_non_removable: args.i_know_what_im_doing,
            layout: args.layout,
        },
    )?;

    confirm_or_abort(&prepared)?;

    let running = session::spawn(prepared.plan(), args.elevation)?;
    // Cancellation (backlog #35): the helper's plan channel is not closed
    // right after the `Plan` line -- it stays open, held by the `Canceller`,
    // for the rest of the run. This handler writes the cancel byte into it
    // and drops it, which both delivers the byte `argos-helper`'s watcher
    // thread is reading for and closes the pipe as a second, redundant
    // signal in case the byte doesn't make it.
    //
    // Best-effort: a second run in the same process (doesn't happen today)
    // would find the handler already registered and get an `Err` here, which
    // is fine to ignore -- the first registration already covers this
    // process's whole lifetime.
    let canceller = running.canceller();
    let _ = ctrlc::set_handler(move || canceller.cancel());

    let mut presenter = Presenter::new();
    let outcome = running.stream(&mut presenter)?;

    // Ejecting is not done here: it needs the same privilege writing does
    // (on a stock Ubuntu `/dev/sdX` is `root:disk`), so the plan carries
    // `eject` and the helper does it while it still holds that privilege.
    // Its result reaches us as `SessionEvent::Ejected`, which `Presenter`
    // prints.
    match outcome {
        Outcome::DdWrite { hash } => println!("Done. SHA-256: {hash}"),
        Outcome::WindowsWrite {
            files_copied,
            bytes_copied,
        } => {
            println!(
                "Done. {files_copied} files copied ({}).",
                human_size(bytes_copied)
            );
            println!(
                "Run `argos verify {} {}` to confirm it.",
                prepared.device.platform_id,
                prepared.iso.display()
            );
        }
        // `argos write` only ever sends a write plan, so the helper can only
        // ever answer with a write outcome.
        other => {
            return Err(ArgosError::Io(std::io::Error::other(format!(
                "argos-helper answered a write with {other:?}"
            ))))
        }
    }

    Ok(())
}

/// Dispatches to the right prompt for what is about to be written. Both of
/// them end the same way, with the retyped-path guard.
fn confirm_or_abort(prepared: &PreparedWrite) -> Result<()> {
    let device = &prepared.device;
    println!("About to overwrite:");
    println!(
        "  device:  {} ({})",
        device.platform_id, device.display_name
    );
    println!("  size:    {}", human_size(device.size_bytes));
    println!(
        "  serial:  {}",
        device.serial.as_deref().unwrap_or("unknown")
    );

    match &prepared.preview {
        WritePreview::Dd { image_size_bytes } => {
            println!("  image:   {}", prepared.iso.display());
            println!("  image size: {}", human_size(*image_size_bytes));
            println!();
        }
        WritePreview::Windows {
            layout, firmware, ..
        } => {
            println!("  image:   {} (Windows installer)", prepared.iso.display());
            println!();
            println!("Argos will create a new partition table with:");
            println!(
                "  partition 1 (Windows files, FAT32): {} at offset {}",
                human_size(layout.windows_partition.size_bytes),
                human_size(layout.windows_partition.start_offset_bytes)
            );
            // Which firmware the result will boot is the single most
            // consequential thing about this choice, and the partition sizes
            // above do not show it.
            match firmware {
                WindowsLayout::Fat32Bios => println!(
                    "  boot: legacy BIOS (MBR + Argos boot records), and UEFI firmware \
                     that accepts MBR-partitioned removable media"
                ),
                _ => println!(
                    "  boot: UEFI only (GPT); use --layout fat32-bios for legacy BIOS machines"
                ),
            }
            // Splitting is invisible in the layout above but very visible on
            // the resulting media (install.wim becomes install.swm +
            // install2.swm ...), so say so before the user commits.
            for note in prepared.preview.split_notes() {
                println!(
                    "  note: {} is over FAT32's 4GiB file limit and will be split into {} parts ({})",
                    note.source_path,
                    note.part_paths.len(),
                    note.part_paths.join(", ")
                );
            }
            println!();
        }
    }

    require_retyped_device_path(&device.platform_id)
}

/// Requires the user to retype the exact device path -- not just "y/N" -- so a
/// hasty Enter can't confirm the wrong drive.
fn require_retyped_device_path(platform_id: &str) -> Result<()> {
    println!("This will PERMANENTLY ERASE all data on {platform_id}.");
    print!("Type the device path ({platform_id}) to confirm: ");
    std::io::stdout().flush().ok();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if input.trim() != platform_id {
        println!("Confirmation did not match; aborting. Nothing was written.");
        return Err(ArgosError::NotConfirmed);
    }
    Ok(())
}
