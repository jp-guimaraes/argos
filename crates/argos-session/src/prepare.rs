//! Everything that happens before a byte is written: resolving the image and
//! the device, refusing the ones Argos must not touch, classifying the image,
//! running the preflight checks, and building the `Plan` that crosses the
//! privilege boundary.
//!
//! Nothing here prints or reads from the user. Confirmation belongs to each
//! front end, which is why [`prepare_write`] hands back a [`WritePreview`]
//! carrying the facts a prompt needs *as data* rather than a formatted block.

use argos_core::device::Device;
use argos_core::error::{ArgosError, Result};
use argos_core::partition::windows::WindowsFat32Plan;
use argos_core::{image, preflight};
use argos_platform::PlatformOps;
use argos_privileged::protocol::{
    Plan, VerifyPlan, VerifyWindowsPlan, WindowsLayout, WritePlan, WriteWindowsPlan,
};
use argos_privileged::windows_fat32::{fat32_layout_for, plan_copy_actions, CopyAction};
use std::path::{Path, PathBuf};

/// What a front end asks for. Field names say what they *do* rather than how
/// the CLI spells them -- `allow_non_removable` is `--i-know-what-im-doing`
/// on the command line and "show all devices" in a window.
pub struct WriteRequest {
    pub iso: PathBuf,
    pub device_id: String,
    pub no_verify: bool,
    /// Whether to eject once the write and its verify are done. Travels in
    /// the plan and is carried out by the privileged helper, because
    /// ejecting needs the privilege writing does -- see
    /// `protocol::WritePlan::eject`.
    pub eject: bool,
    pub allow_non_removable: bool,
    pub layout: WindowsLayout,
}

pub struct VerifyRequest {
    pub device_id: String,
    pub iso: PathBuf,
    pub layout: WindowsLayout,
}

/// A write that has passed every unprivileged check and is ready to be
/// confirmed and run.
pub struct PreparedWrite {
    pub device: Device,
    /// Absolute -- see [`canonicalize_iso_path`].
    pub iso: PathBuf,
    pub preview: WritePreview,
    plan: Plan,
}

impl PreparedWrite {
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
}

pub struct PreparedVerify {
    pub iso: PathBuf,
    plan: Plan,
}

impl PreparedVerify {
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
}

/// Everything a confirmation prompt needs, as data.
pub enum WritePreview {
    /// A hybrid ISO copied byte for byte; it carries its own partition table,
    /// so there is no layout to describe.
    Dd { image_size_bytes: u64 },
    Windows {
        layout: WindowsFat32Plan,
        actions: Vec<CopyAction>,
        firmware: WindowsLayout,
    },
}

/// A file too large for FAT32 that will reach the media as several `.swm`
/// parts. Invisible in the layout numbers and very visible on the resulting
/// drive, so every front end says so before the user commits.
pub struct SplitNote {
    pub source_path: String,
    pub part_paths: Vec<String>,
}

impl WritePreview {
    pub fn split_notes(&self) -> Vec<SplitNote> {
        let WritePreview::Windows { actions, .. } = self else {
            return Vec::new();
        };
        actions
            .iter()
            .filter_map(|action| match action {
                CopyAction::SplitWim {
                    source_path,
                    part_paths,
                    ..
                } => Some(SplitNote {
                    source_path: source_path.clone(),
                    part_paths: part_paths.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// Resolves an ISO path to absolute before it's put in a `Plan` and sent
/// across the privilege boundary to `argos-helper`.
///
/// Load-bearing, not defensive: `argos-helper` opens `plan.image_path` (or
/// `plan.iso_path`) relative to *its own* working directory, not the shell's
/// the user typed a relative path in. `sudo` commonly preserves the caller's
/// cwd, which is why this went unnoticed for a long time -- `pkexec`
/// deliberately does not (the same cwd-reset hardening every setuid-style
/// launcher does, to stop a relative path from resolving somewhere the
/// caller didn't intend), so it surfaces exactly there: confirmed on real
/// hardware, running `argos write some.iso --device /dev/sdg` from the
/// directory holding the ISO -- unmounting succeeds, then `File::open` on
/// the (still-relative) path fails with a bare, pathless "No such file or
/// directory" from deep inside the elevated helper, well after the
/// destructive confirmation prompt.
///
/// Resolving here instead means a bad path fails immediately, with the path
/// named in the message, before any confirmation prompt, unmount, or
/// elevation -- not moments after the user has confirmed a write.
pub fn canonicalize_iso_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|err| ArgosError::Io(std::io::Error::other(format!("{}: {err}", path.display()))))
}

/// The non-negotiable part of the safety gate: a system disk is refused
/// unconditionally, no flag overrides it. A disk the OS doesn't report as
/// removable can only proceed with the caller's explicit override -- and still
/// has to survive the front end's confirmation.
pub fn check_device_is_offerable(device: &Device, allow_non_removable: bool) -> Result<()> {
    if device.is_system_disk {
        return Err(ArgosError::DeviceIsSystemDisk(device.platform_id.clone()));
    }
    if !device.is_safe_to_write() && !allow_non_removable {
        return Err(ArgosError::DeviceNotRemovable(device.platform_id.clone()));
    }
    Ok(())
}

/// What kind of image this is, judged on its own -- no device involved.
///
/// A front end needs this the moment a file is picked, to say what it found
/// and to decide whether a layout choice even applies, which is well before
/// a target has been chosen. [`prepare_write`] makes the same judgement
/// again, in the same order, when it has both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// A hybrid ISO, written byte for byte. It carries its own partition
    /// table, so `--layout` has nothing to decide.
    LinuxDd,
    WindowsInstaller,
}

/// Classifies `iso` the way [`prepare_write`] will: DD-mode first, then the
/// Windows-installer shape. `Ok(None)` means neither matched -- a plain data
/// ISO, or a corrupt one.
///
/// Cheap for the DD case (a few sectors); the Windows case reads the image's
/// directory tree, so a front end should call it off its UI thread.
pub fn classify_image(iso: &Path) -> Result<Option<ImageKind>> {
    if image::classify(iso)?.is_writable_as_dd_image() {
        return Ok(Some(ImageKind::LinuxDd));
    }
    if image::windows::classify(iso)?.is_windows_installer_iso() {
        return Ok(Some(ImageKind::WindowsInstaller));
    }
    Ok(None)
}

/// Resolve, refuse, classify, preflight, and build the `Plan`. Nothing here
/// is destructive and nothing elevates.
pub fn prepare_write(platform: &dyn PlatformOps, req: &WriteRequest) -> Result<PreparedWrite> {
    // Before anything else: see `canonicalize_iso_path` for why a relative
    // ISO path cannot survive to the Plan, and why failing here turns a
    // confusing, late, pathless error into an immediate, specific one.
    let iso = canonicalize_iso_path(&req.iso)?;

    let device = platform
        .refresh(&req.device_id, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(req.device_id.clone()))?;

    check_device_is_offerable(&device, req.allow_non_removable)?;

    // Linux-hybrid ISOs (DD mode) first, since that's the overwhelmingly
    // common case; only try the Windows-installer shape (UDF/ISO9660, see
    // `image::windows`) once that's ruled out. A plain-data or corrupt image
    // matches neither and falls through to `UnsupportedIso` below.
    if image::classify(&iso)?.is_writable_as_dd_image() {
        return prepare_dd_write(platform, device, iso, req);
    }
    if image::windows::classify(&iso)?.is_windows_installer_iso() {
        return prepare_windows_write(platform, device, iso, req);
    }
    Err(ArgosError::UnsupportedIso(iso))
}

/// Check order is load-bearing and differs from the Windows path below:
/// size, then capacity, then collision. Which check fires first is which
/// error the user sees, so a tidy-up that unifies the two paths is a
/// behaviour change.
fn prepare_dd_write(
    platform: &dyn PlatformOps,
    device: Device,
    iso: PathBuf,
    req: &WriteRequest,
) -> Result<PreparedWrite> {
    let image_size_bytes = std::fs::metadata(&iso)?.len();
    preflight::check_capacity(
        &device.platform_id,
        device.size_bytes,
        &iso,
        image_size_bytes,
    )?;
    check_source_collision(platform, &device, &iso)?;

    let plan = Plan::Write(WritePlan {
        device_path: device.platform_id.clone(),
        expected_serial: device.serial.clone(),
        expected_size_bytes: device.size_bytes,
        image_path: iso.clone(),
        image_size_bytes,
        verify: !req.no_verify,
        eject: req.eject,
    });

    Ok(PreparedWrite {
        device,
        iso,
        preview: WritePreview::Dd { image_size_bytes },
        plan,
    })
}

/// The FAT32 Windows path: plan, then capacity, then collision -- not the
/// order [`prepare_dd_write`] uses, and preserved as-is.
///
/// The layout is computed here purely for the preflight and the front end's
/// preview; the privileged side independently recomputes it either way (see
/// [`windows_fat32_plan_for`]).
fn prepare_windows_write(
    platform: &dyn PlatformOps,
    device: Device,
    iso: PathBuf,
    req: &WriteRequest,
) -> Result<PreparedWrite> {
    let (layout, actions) = windows_fat32_plan_for(&iso)?;
    preflight::check_windows_fat32_capacity(&device.platform_id, device.size_bytes, &iso, &layout)?;
    check_source_collision(platform, &device, &iso)?;

    let plan = Plan::WriteWindowsImage(WriteWindowsPlan {
        device_path: device.platform_id.clone(),
        expected_serial: device.serial.clone(),
        expected_size_bytes: device.size_bytes,
        iso_path: iso.clone(),
        layout: req.layout,
        eject: req.eject,
    });

    Ok(PreparedWrite {
        device,
        iso,
        preview: WritePreview::Windows {
            layout,
            actions,
            firmware: req.layout,
        },
        plan,
    })
}

/// `argos verify`'s counterpart. Read-only, so there is no capacity check, no
/// collision check and no confirmation -- and no `expected_serial`/
/// `expected_size_bytes` on the plans, since a read has no destructive TOCTOU
/// window to guard (see the plan types' own doc comments).
pub fn prepare_verify(platform: &dyn PlatformOps, req: &VerifyRequest) -> Result<PreparedVerify> {
    let iso = canonicalize_iso_path(&req.iso)?;

    // Resolved unprivileged, before elevating, purely so a typo'd device path
    // fails fast with a clear error instead of only after a sudo/pkexec
    // prompt. The result itself isn't otherwise used -- but the *ordering*
    // is observable: a bad device outranks an unsupported image.
    platform
        .refresh(&req.device_id, None)?
        .ok_or_else(|| ArgosError::DeviceNotFound(req.device_id.clone()))?;

    if image::classify(&iso)?.is_writable_as_dd_image() {
        let iso_size_bytes = std::fs::metadata(&iso)?.len();
        let plan = Plan::Verify(VerifyPlan {
            device_path: req.device_id.clone(),
            iso_path: iso.clone(),
            iso_size_bytes,
        });
        return Ok(PreparedVerify { iso, plan });
    }
    if image::windows::classify(&iso)?.is_windows_installer_iso() {
        let plan = Plan::VerifyWindowsImage(VerifyWindowsPlan {
            device_path: req.device_id.clone(),
            iso_path: iso.clone(),
            layout: req.layout,
        });
        return Ok(PreparedVerify { iso, plan });
    }
    Err(ArgosError::UnsupportedIso(iso))
}

/// The FAT32 counterpart of a per-write layout plan (phase 3 M3.5, backlog
/// #43) -- display-only, and it front-runs the helper's own refusals so an
/// ISO the FAT32 layout genuinely cannot hold fails here, before any
/// sudo/pkexec prompt or destructive confirmation.
///
/// Calls the helper's own [`plan_copy_actions`], deliberately, rather than
/// reimplementing the "will this fit?" rules: the first version of this
/// function had its own copy of the pre-splitter check, which went stale the
/// moment the WIM splitter landed and made `argos write --layout fat32`
/// refuse real Windows media the helper handled fine. Sharing the function is
/// what keeps the prompt's numbers and the helper's behaviour from ever
/// disagreeing again.
fn windows_fat32_plan_for(iso: &Path) -> Result<(WindowsFat32Plan, Vec<CopyAction>)> {
    let image = image::windows::WindowsIso::open(iso)?;
    let files = image.list_files()?;
    let actions = plan_copy_actions(&image, &files)?;
    let layout = fat32_layout_for(&actions);
    Ok((layout, actions))
}

/// The source-on-target-device guard, shared by both write paths.
fn check_source_collision(platform: &dyn PlatformOps, device: &Device, iso: &Path) -> Result<()> {
    if let Some(backing_device_id) = platform.backing_device_of(iso)? {
        preflight::check_no_source_target_collision(iso, &backing_device_id, &device.platform_id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argos_core::device::Bus;

    fn usb_stick() -> Device {
        Device {
            platform_id: "/dev/sdz".into(),
            display_name: "Example USB Stick".into(),
            size_bytes: 8_000_000_000,
            bus: Bus::Usb,
            os_reports_removable: true,
            is_system_disk: false,
            serial: Some("ABC123".into()),
        }
    }

    // The single most important test in this project: no flag, typo, or code
    // path may talk `check_device_is_offerable` into accepting a system disk.
    #[test]
    fn never_offers_a_system_disk_even_with_i_know_what_im_doing() {
        let mut device = usb_stick();
        device.is_system_disk = true;
        let err = check_device_is_offerable(&device, true).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceIsSystemDisk(_)));
    }

    #[test]
    fn offers_a_plain_usb_stick_by_default() {
        assert!(check_device_is_offerable(&usb_stick(), false).is_ok());
    }

    #[test]
    fn refuses_a_non_removable_disk_without_the_override_flag() {
        let mut device = usb_stick();
        device.os_reports_removable = false;
        let err = check_device_is_offerable(&device, false).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceNotRemovable(_)));
    }

    #[test]
    fn accepts_a_non_removable_non_system_disk_with_the_override_flag() {
        let mut device = usb_stick();
        device.os_reports_removable = false;
        assert!(check_device_is_offerable(&device, true).is_ok());
    }

    /// This is the actual bug: a relative path resolves fine right here
    /// (same process, same cwd as the shell), which is exactly what let it
    /// through code review and every existing test (all of which use
    /// tempfile's always-absolute paths) -- and then fails deep inside
    /// argos-helper, elevated via a mechanism that may not preserve cwd,
    /// with an error naming no path at all. Pins the fix at the type that
    /// actually crosses the privilege boundary: what canonicalize_iso_path
    /// returns must be absolute, not merely "resolved without error".
    #[test]
    fn a_relative_path_comes_back_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("some.iso");
        std::fs::write(&iso, b"x").unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = canonicalize_iso_path(Path::new("some.iso"));
        std::env::set_current_dir(original_cwd).unwrap();

        let resolved = result.expect("a file that exists should resolve");
        assert!(resolved.is_absolute(), "got {resolved:?}, not absolute");
        assert_eq!(resolved.file_name().unwrap(), "some.iso");
    }

    /// The failure case has to name the path -- a bare "No such file or
    /// directory" is what the user actually saw in the field, and it names
    /// nothing to act on.
    #[test]
    fn a_missing_path_is_named_in_the_error() {
        let err = canonicalize_iso_path(Path::new("/definitely/does/not/exist.iso"))
            .expect_err("a nonexistent path must not resolve");
        let message = err.to_string();
        assert!(
            message.contains("does/not/exist.iso"),
            "error {message:?} does not name the path"
        );
    }

    /// Regression guard for a bug found by a user running `argos write
    /// --layout fat32` against a real Windows 11 ISO: this function used to
    /// carry its own pre-splitter "every file must fit FAT32" check, so it
    /// rejected media the helper could write perfectly well, before even
    /// prompting. The planning logic is now shared with the helper --
    /// asserting on a real oversized WIM here is what proves the two agree.
    ///
    /// Uses the synthetic UDF fixture (small files only), so it pins the
    /// non-refusal for ordinary media; the oversized-WIM half is covered by
    /// `argos-privileged`'s own `plan_copy_actions` tests, which run against
    /// a WIM too large for FAT32.
    #[test]
    fn fat32_planning_accepts_ordinary_windows_media() {
        let iso = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            iso.path(),
            argos_core::image::windows::fixtures::udf_windows_installer_iso(true, true),
        )
        .unwrap();

        let (layout, actions) = windows_fat32_plan_for(iso.path()).expect(
            "a Windows installer ISO must plan cleanly for fat32 -- a refusal here means the \
             CLI and the helper disagree about what the layout can hold",
        );
        assert!(!actions.is_empty());
        assert!(layout.total_bytes_required() > 0);
    }

    #[test]
    fn a_windows_installer_iso_is_classified_as_one() {
        let iso = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            iso.path(),
            argos_core::image::windows::fixtures::udf_windows_installer_iso(true, true),
        )
        .unwrap();
        assert_eq!(
            classify_image(iso.path()).unwrap(),
            Some(ImageKind::WindowsInstaller)
        );
    }

    /// Anything Argos does not recognize classifies as nothing, rather than
    /// being guessed into one of the two write paths.
    #[test]
    fn an_unrecognized_image_classifies_as_neither() {
        let iso = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(iso.path(), vec![0u8; 128 * 1024]).unwrap();
        assert_eq!(classify_image(iso.path()).unwrap(), None);
    }

    #[test]
    fn a_dd_preview_never_reports_split_parts() {
        let preview = WritePreview::Dd {
            image_size_bytes: 1234,
        };
        assert!(preview.split_notes().is_empty());
    }
}
