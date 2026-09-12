//! Keeping a selection honest while the device list changes underneath it.
//!
//! None of this *is* a safety guarantee. `prepare_write` re-resolves the
//! device, and `argos-helper` re-validates it under a TOCTOU check that
//! refuses a changed serial or size outright. What this does is turn a
//! refusal that would arrive after the user has committed into one they can
//! see before they do.

use argos_core::device::Device;

/// What the user picked, remembered by identity rather than by index -- a
/// position in a list means nothing once the list is rebuilt.
#[derive(Clone, PartialEq, Eq)]
pub struct Selection {
    pub platform_id: String,
    pub serial: Option<String>,
    pub size_bytes: u64,
}

impl Selection {
    pub fn of(device: &Device) -> Self {
        Selection {
            platform_id: device.platform_id.clone(),
            serial: device.serial.clone(),
            size_bytes: device.size_bytes,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelectionOutcome {
    /// Same drive, still there.
    Unchanged,
    /// The path is gone from the list -- unplugged, most likely.
    Gone,
    /// Something is still at that path, but it is not the same drive: the
    /// serial or the size differs. Exactly the two things
    /// `validate_refreshed_device` refuses on, checked here so the user finds
    /// out before confirming rather than after.
    Replaced,
}

pub fn reconcile(selection: &Selection, devices: &[Device]) -> SelectionOutcome {
    let Some(device) = devices
        .iter()
        .find(|d| d.platform_id == selection.platform_id)
    else {
        return SelectionOutcome::Gone;
    };
    if device.serial != selection.serial || device.size_bytes != selection.size_bytes {
        return SelectionOutcome::Replaced;
    }
    SelectionOutcome::Unchanged
}

/// The devices a front end may offer.
///
/// A system disk is never in this list, in either mode. It is refused by
/// `check_device_is_offerable` regardless, so this is not what makes it safe
/// -- but a device that cannot be selected is a device that cannot be
/// mis-clicked, and the CLI's equivalent (`argos list`) has the space to
/// explain a refusal where a dropdown does not.
pub fn offerable(devices: &[Device], show_all: bool) -> Vec<&Device> {
    devices
        .iter()
        .filter(|d| !d.is_system_disk && (show_all || d.is_safe_to_write()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use argos_core::device::Bus;

    fn device(id: &str, serial: Option<&str>, size: u64) -> Device {
        Device {
            platform_id: id.into(),
            display_name: "Example".into(),
            size_bytes: size,
            bus: Bus::Usb,
            os_reports_removable: true,
            is_system_disk: false,
            serial: serial.map(str::to_string),
        }
    }

    #[test]
    fn the_same_drive_still_present_is_unchanged() {
        let d = device("/dev/sdz", Some("ABC"), 8_000);
        let selection = Selection::of(&d);
        assert_eq!(reconcile(&selection, &[d]), SelectionOutcome::Unchanged);
    }

    #[test]
    fn a_path_that_left_the_list_is_gone() {
        let d = device("/dev/sdz", Some("ABC"), 8_000);
        let selection = Selection::of(&d);
        assert_eq!(
            reconcile(&selection, &[device("/dev/sdy", Some("XYZ"), 8_000)]),
            SelectionOutcome::Gone
        );
        assert_eq!(reconcile(&selection, &[]), SelectionOutcome::Gone);
    }

    /// The dangerous one: the path a user is looking at now belongs to a
    /// different drive. Unplug one stick, plug another into the same port.
    #[test]
    fn a_different_serial_at_the_same_path_is_a_different_drive() {
        let selection = Selection::of(&device("/dev/sdz", Some("ABC"), 8_000));
        assert_eq!(
            reconcile(&selection, &[device("/dev/sdz", Some("DIFFERENT"), 8_000)]),
            SelectionOutcome::Replaced
        );
    }

    #[test]
    fn a_different_size_at_the_same_path_is_a_different_drive() {
        let selection = Selection::of(&device("/dev/sdz", Some("ABC"), 8_000));
        assert_eq!(
            reconcile(&selection, &[device("/dev/sdz", Some("ABC"), 64_000)]),
            SelectionOutcome::Replaced
        );
    }

    /// A drive that never reported a serial must not become "replaced" just
    /// because it still does not report one.
    #[test]
    fn a_drive_with_no_serial_stays_unchanged_when_it_still_has_none() {
        let d = device("/dev/sdz", None, 8_000);
        let selection = Selection::of(&d);
        assert_eq!(reconcile(&selection, &[d]), SelectionOutcome::Unchanged);
    }

    /// ...but gaining or losing one is a change worth noticing, since that is
    /// what the helper will refuse on.
    #[test]
    fn gaining_a_serial_counts_as_replaced() {
        let selection = Selection::of(&device("/dev/sdz", None, 8_000));
        assert_eq!(
            reconcile(&selection, &[device("/dev/sdz", Some("ABC"), 8_000)]),
            SelectionOutcome::Replaced
        );
    }

    // ---- what may be offered --------------------------------------------

    /// The one that must never regress: a system disk is absent from the
    /// dropdown in both modes.
    #[test]
    fn a_system_disk_is_never_offered_in_either_mode() {
        let mut system = device("/dev/sda", Some("SYS"), 1_000_000);
        system.is_system_disk = true;
        for show_all in [false, true] {
            assert!(
                offerable(&[system.clone()], show_all).is_empty(),
                "show_all={show_all}"
            );
        }
    }

    #[test]
    fn a_plain_usb_stick_is_offered_by_default() {
        let d = device("/dev/sdz", Some("ABC"), 8_000);
        assert_eq!(offerable(&[d], false).len(), 1);
    }

    /// Mirrors `--i-know-what-im-doing`: a non-removable, non-system disk is
    /// hidden until the user asks for it.
    #[test]
    fn a_non_removable_disk_appears_only_in_show_all() {
        let mut d = device("/dev/sdb", Some("ABC"), 8_000);
        d.os_reports_removable = false;
        assert!(offerable(&[d.clone()], false).is_empty());
        assert_eq!(offerable(&[d], true).len(), 1);
    }
}
