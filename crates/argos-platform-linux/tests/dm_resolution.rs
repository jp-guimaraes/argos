//! Backlog E2: validates the two filesystem-touching halves of the
//! device-mapper resolver (`dm::sysfs_slaves_of`, which reads
//! `/sys/block/<name>/slaves`, and `dm::block_device_name_of_mount_source`,
//! which resolves a `/dev/mapper/*` symlink to its `dm-N` target) against a
//! **real** device-mapper stack -- a throwaway `dmsetup` linear target on top
//! of a loop device, standing in for what LVM/RAID/dm-crypt would set up in
//! practice. The recursive resolution logic itself (`dm::physical_disks_of`)
//! is pure and already covered by unit tests in `src/dm.rs` with fake
//! multi-level stacks (LVM, dm-crypt-under-LVM, striped volumes); this test
//! exists to confirm those two real I/O primitives read what the kernel
//! actually reports, not a synthetic stand-in.
//!
//! Needs root (`dmsetup create`/`remove`, `losetup`) and the `dmsetup`
//! binary. Every test skips itself (never fails) when the prerequisites
//! aren't met, mirroring `argos-privileged`'s `loop_device_write` tests. Run
//! explicitly:
//!
//! ```sh
//! sudo -E env "PATH=$PATH" cargo test -p argos-platform-linux \
//!     --test dm_resolution -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use argos_platform_linux::dm;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// `cargo test` runs these in parallel threads of the *same* process, so
/// `std::process::id()` alone isn't a unique name -- this counter makes each
/// rig's dmsetup target name unique within the run.
static RIG_COUNTER: AtomicU32 = AtomicU32::new(0);

struct DmTestRig {
    dm_name: String,
    loop_path: String,
    _backing_file: tempfile::NamedTempFile,
}

impl DmTestRig {
    /// Sets up a loop device over a small throwaway file, then a `dmsetup`
    /// linear target spanning it entirely -- the minimal real device-mapper
    /// stack. Returns `None` (never panics) if any prerequisite is missing,
    /// so callers can skip cleanly.
    fn set_up() -> Option<Self> {
        const SIZE_BYTES: u64 = 8 * 1024 * 1024;
        const SECTORS: u64 = SIZE_BYTES / 512;

        let mut backing_file = tempfile::NamedTempFile::new().ok()?;
        backing_file.as_file_mut().set_len(SIZE_BYTES).ok()?;

        let output = Command::new("losetup")
            .args(["--find", "--show"])
            .arg(backing_file.path())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let loop_path = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if loop_path.is_empty() {
            return None;
        }

        let dm_name = format!(
            "argos-test-dm-{}-{}",
            std::process::id(),
            RIG_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let table = format!("0 {SECTORS} linear {loop_path} 0");
        let status = Command::new("dmsetup")
            .args(["create", &dm_name, "--table", &table])
            .status()
            .ok()?;
        if !status.success() {
            let _ = Command::new("losetup")
                .args(["--detach", &loop_path])
                .status();
            return None;
        }

        Some(Self {
            dm_name,
            loop_path,
            _backing_file: backing_file,
        })
    }

    fn mapper_path(&self) -> String {
        format!("/dev/mapper/{}", self.dm_name)
    }

    /// The bare `loopN` name (without `/dev/`), as it would appear in
    /// `/sys/block/dm-M/slaves/`.
    fn loop_device_name(&self) -> String {
        self.loop_path.trim_start_matches("/dev/").to_string()
    }
}

impl Drop for DmTestRig {
    fn drop(&mut self) {
        let _ = Command::new("dmsetup")
            .args(["remove", &self.dm_name])
            .status();
        let _ = Command::new("losetup")
            .args(["--detach", &self.loop_path])
            .status();
    }
}

fn running_as_root() -> bool {
    // SAFETY: geteuid() takes no arguments and has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[test]
#[ignore = "needs root, losetup, and dmsetup; see module docs"]
fn resolves_a_dev_mapper_symlink_to_its_real_dm_device() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    let Some(rig) = DmTestRig::set_up() else {
        eprintln!("skipping: could not set up a real dm-mapper test stack (needs dmsetup + losetup + root)");
        return;
    };

    let resolved_name = dm::block_device_name_of_mount_source(&rig.mapper_path());
    assert!(
        resolved_name.starts_with("dm-"),
        "expected /dev/mapper/{} to resolve to a dm-N device, got {resolved_name:?}",
        rig.dm_name
    );
}

#[test]
#[ignore = "needs root, losetup, and dmsetup; see module docs"]
fn reads_the_real_slave_relationship_from_sysfs() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    let Some(rig) = DmTestRig::set_up() else {
        eprintln!("skipping: could not set up a real dm-mapper test stack (needs dmsetup + losetup + root)");
        return;
    };

    let dm_device_name = dm::block_device_name_of_mount_source(&rig.mapper_path());
    let slaves = dm::sysfs_slaves_of(&dm_device_name);
    assert_eq!(
        slaves,
        vec![rig.loop_device_name()],
        "the dm target's only slave should be the loop device it was built on"
    );
}

#[test]
#[ignore = "needs root, losetup, and dmsetup; see module docs"]
fn end_to_end_resolve_mount_source_finds_the_loop_device() {
    if !running_as_root() {
        eprintln!("skipping: not running as root");
        return;
    }
    let Some(rig) = DmTestRig::set_up() else {
        eprintln!("skipping: could not set up a real dm-mapper test stack (needs dmsetup + losetup + root)");
        return;
    };

    let resolved = dm::resolve_mount_source(&rig.mapper_path());
    assert_eq!(
        resolved,
        vec![format!("/dev/{}", rig.loop_device_name())],
        "expected resolution to reach the real loop device backing the dm target"
    );
}
