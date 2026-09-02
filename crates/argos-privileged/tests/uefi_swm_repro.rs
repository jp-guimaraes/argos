//! Builds real media from a real Windows ISO into a plain file, so QEMU can
//! boot Windows Setup off it. Diagnostic scaffolding, not an assertion: it
//! asserts nothing about the media beyond the write succeeding.
//!
//! The BIOS boot-chain test already covers the MBR/SeaBIOS path. What this
//! exists for is the case that fails on real hardware and has never been
//! emulated: GPT/UEFI media carrying a *split* `install.swm`.
//!
//!     ARGOS_REPRO_ISO=.testdata/Win10_22H2_English_x64v1.iso \
//!     ARGOS_REPRO_OUT=/tmp/winrepro/uefi-22h2.img \
//!     ARGOS_REPRO_LAYOUT=fat32 \
//!     cargo test -p argos-privileged --test uefi_swm_repro -- --ignored --nocapture

use argos_privileged::protocol::WindowsLayout;
use argos_privileged::windows_fat32::{
    plan_copy_actions, total_bytes_on_target, write_fat32_media_for_test, TargetLayout,
};

#[test]
#[ignore = "diagnostic aid; needs a real Windows ISO named by ARGOS_REPRO_ISO"]
fn build_media_from_a_real_iso() {
    let iso_path = match std::env::var("ARGOS_REPRO_ISO") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("skipping: set ARGOS_REPRO_ISO");
            return;
        }
    };
    let out_path = std::env::var("ARGOS_REPRO_OUT").expect("set ARGOS_REPRO_OUT");
    let layout_name = std::env::var("ARGOS_REPRO_LAYOUT").unwrap_or_else(|_| "fat32".into());
    let layout_kind = match layout_name.as_str() {
        "fat32" => WindowsLayout::Fat32,
        "fat32-bios" => WindowsLayout::Fat32Bios,
        other => panic!("unknown layout {other}"),
    };

    let started = std::time::Instant::now();
    let iso =
        argos_core::image::windows::WindowsIso::open(std::path::Path::new(&iso_path)).unwrap();
    let files = iso.list_files().unwrap();
    let actions = plan_copy_actions(&iso, &files).unwrap();
    let layout = TargetLayout::for_layout(layout_kind, total_bytes_on_target(&actions));

    eprintln!(
        "{} files -> {} layout, partition {} bytes at LBA {}",
        actions.len(),
        layout_name,
        layout.region().size_bytes,
        layout.start_lba().unwrap(),
    );
    for action in &actions {
        if action.bytes_on_target() > 1 << 30 {
            eprintln!("  large: {action:?}");
        }
    }

    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut disk = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&out_path)
        .unwrap();
    disk.set_len(layout.total_bytes_required()).unwrap();

    let outcome = write_fat32_media_for_test(
        &mut disk,
        &layout,
        &iso,
        &actions,
        &argos_core::progress::NoopProgress,
    )
    .expect("the real write path should produce media");

    eprintln!(
        "wrote {} files, {} bytes to {out_path} in {:?}",
        outcome.files_copied,
        outcome.bytes_copied,
        started.elapsed()
    );
}
