//! `argos format`: return a stick to ordinary use, as a terminal presents it.
//! Same shape as `write`: `argos-session` prepares and elevates, this file
//! asks for confirmation and says what happened.

use super::progress::Presenter;
use super::write::require_retyped_device_path;
use argos_core::error::{ArgosError, Result};
use argos_session::{self as session, human_size, ElevationUi, FormatRequest, Outcome};

pub struct Args {
    pub device: String,
    pub label: String,
    pub no_eject: bool,
    pub i_know_what_im_doing: bool,
    pub elevation: ElevationUi,
}

pub fn run(args: Args) -> Result<()> {
    let platform = session::current_platform();

    let prepared = session::prepare_format(
        &platform,
        &FormatRequest {
            device_id: args.device,
            label: args.label,
            eject: !args.no_eject,
            allow_non_removable: args.i_know_what_im_doing,
        },
    )?;

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
    println!();
    println!("Argos will replace the partition table with:");
    println!(
        "  partition 1 (FAT32, MBR, the whole device): {} labelled {}",
        human_size(prepared.partition_bytes),
        prepared.label
    );
    println!("  note: FAT32 cannot hold a single file larger than 4GiB");
    println!();
    require_retyped_device_path(&device.platform_id)?;

    // No Ctrl-C handler: a format takes seconds and the helper never reads
    // the cancel byte for one -- stopping between the table and the
    // filesystem would only leave less usable media than finishing.
    let running = session::spawn(prepared.plan(), args.elevation)?;
    let mut presenter = Presenter::new();
    match running.stream(&mut presenter)? {
        Outcome::Format {
            partition_bytes,
            label,
        } => {
            println!(
                "Done. One {} FAT32 volume labelled {label}.",
                human_size(partition_bytes)
            );
            println!(
                "Remove and reinsert the drive for the system to pick up the new partition table."
            );
            Ok(())
        }
        other => Err(ArgosError::Io(std::io::Error::other(format!(
            "argos-helper answered a format with {other:?}"
        )))),
    }
}
