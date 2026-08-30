//! Best-effort cross-check against UDisks2 over D-Bus.
//!
//! This is deliberately *not* the primary enumeration source (that's still
//! sysfs + the udev database in [`crate::enumerate`] and [`crate::sysfs`],
//! which need no running daemon and work on any udev-based system). Instead,
//! this module implements the "two sources, cross-referenced" defense in
//! depth from the original design notes: when `udisksd` is reachable, its
//! answer for "is this removable, and what bus is it on" is combined with
//! sysfs/udev's answer so that **either source disagreeing pushes the result
//! towards refusing to write**, never towards allowing it. When UDisks2 isn't
//! running (headless servers, containers, minimal installs) or the D-Bus call
//! fails for any reason, this returns `None` and callers fall back to
//! sysfs/udev alone, exactly as before this module existed.
//!
//! Talks to the system bus directly via `zbus`'s blocking API (no manual XML
//! introspection, no generated proxy trait -- `GetManagedObjects` is called
//! as a plain untyped method call and its deeply nested `a{oa{sa{sv}}}` reply
//! is walked by hand, which is simpler than it sounds because we only ever
//! read a handful of well-known property names).

use std::collections::HashMap;
use zbus::blocking::Connection;
use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue};

const SERVICE: &str = "org.freedesktop.UDisks2";
const MANAGER_PATH: &str = "/org/freedesktop/UDisks2";
const BLOCK_INTERFACE: &str = "org.freedesktop.UDisks2.Block";
const DRIVE_INTERFACE: &str = "org.freedesktop.UDisks2.Drive";
/// Present on a block device's object only when it's a *partition* of some
/// other block device -- its absence is how we tell "this is the whole-disk
/// block device" apart from "this is one of its partitions". A single drive
/// backs several block device objects (the whole disk plus each partition),
/// all pointing at the same `Drive` object path, so this distinction is
/// needed to build a map keyed uniquely by device path.
const PARTITION_INTERFACE: &str = "org.freedesktop.UDisks2.Partition";

/// What UDisks2 reports about one drive (physical device), keyed by the
/// kernel device path (e.g. `/dev/sda`) of its block device in
/// [`Udisks2Snapshot::by_device_path`].
#[derive(Debug, Clone, Default)]
pub struct DriveInfo {
    pub connection_bus: String,
    pub removable: bool,
    pub media_removable: bool,
    pub serial: Option<String>,
}

impl DriveInfo {
    /// UDisks2's own opinion on "is this safe to treat as a removable USB
    /// disk", combining the two independent flags it exposes for exactly the
    /// reason `argos-core`'s `Device::is_safe_to_write` combines bus +
    /// removable: neither alone is fully trustworthy.
    pub fn looks_removable_usb(&self) -> bool {
        self.connection_bus == "usb" && (self.removable || self.media_removable)
    }
}

pub struct Udisks2Snapshot {
    by_device_path: HashMap<String, DriveInfo>,
}

impl Udisks2Snapshot {
    /// Connects to the system bus and asks `udisksd` for everything it
    /// knows, in one round trip. Returns `None` on any failure (no D-Bus, no
    /// `udisksd`, an unexpected reply shape, ...) -- there is no partial or
    /// "best guess" result, callers should fall back entirely to sysfs/udev.
    pub fn fetch() -> Option<Self> {
        let connection = Connection::system().ok()?;
        let reply = connection
            .call_method(
                Some(SERVICE),
                MANAGER_PATH,
                Some("org.freedesktop.DBus.ObjectManager"),
                "GetManagedObjects",
                &(),
            )
            .ok()?;

        // a{oa{sa{sv}}}: object path -> interface name -> property name -> value.
        type ManagedObjects =
            HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
        let objects: ManagedObjects = reply.body().deserialize().ok()?;

        let mut drives_by_object_path: HashMap<String, DriveInfo> = HashMap::new();
        for (path, interfaces) in &objects {
            if let Some(props) = interfaces.get(DRIVE_INTERFACE) {
                drives_by_object_path.insert(path.as_str().to_string(), drive_info_from(props));
            }
        }

        // Keyed by device path this time (unique per block device), not by
        // drive object path (which is shared by a disk and all its
        // partitions, and would silently overwrite entries in a HashMap).
        let mut by_device_path: HashMap<String, DriveInfo> = HashMap::new();
        for interfaces in objects.values() {
            if interfaces.contains_key(PARTITION_INTERFACE) {
                continue; // a partition, not the whole-disk block device
            }
            let Some(props) = interfaces.get(BLOCK_INTERFACE) else {
                continue;
            };
            let Some(device_path) = byte_array_property_as_path(props, "Device") else {
                continue;
            };
            let Some(drive_object_path) = object_path_property(props, "Drive") else {
                continue;
            };
            if drive_object_path == "/" {
                continue; // no associated drive (e.g. a loop/dm/md block device)
            }
            if let Some(info) = drives_by_object_path.get(&drive_object_path) {
                by_device_path.insert(device_path, info.clone());
            }
        }

        Some(Self { by_device_path })
    }

    /// Looks up what UDisks2 reported for `platform_id` (e.g. `/dev/sda`).
    /// `None` means UDisks2 doesn't know about this device at all (not that
    /// it's known and non-removable) -- callers should treat that as "no
    /// opinion", not as a refusal signal.
    pub fn get(&self, platform_id: &str) -> Option<&DriveInfo> {
        self.by_device_path.get(platform_id)
    }
}

fn drive_info_from(props: &HashMap<String, OwnedValue>) -> DriveInfo {
    DriveInfo {
        connection_bus: string_property(props, "ConnectionBus").unwrap_or_default(),
        removable: bool_property(props, "Removable").unwrap_or(false),
        media_removable: bool_property(props, "MediaRemovable").unwrap_or(false),
        serial: string_property(props, "Serial").filter(|s| !s.is_empty()),
    }
}

fn string_property(props: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    props.get(name)?.downcast_ref::<String>().ok()
}

fn bool_property(props: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    props.get(name)?.downcast_ref::<bool>().ok()
}

fn object_path_property(props: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    props
        .get(name)?
        .downcast_ref::<ObjectPath>()
        .ok()
        .map(|p| p.as_str().to_string())
}

/// UDisks2 exposes device paths as a NUL-terminated byte array (`ay`) rather
/// than a string, e.g. `Device` on `org.freedesktop.UDisks2.Block`.
fn byte_array_property_as_path(props: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    let array = props.get(name)?.downcast_ref::<Array>().ok()?;
    let bytes: Vec<u8> = array
        .iter()
        .filter_map(|v| v.downcast_ref::<u8>().ok())
        .collect();
    let without_nul = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
    String::from_utf8(without_nul.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_removable_usb_requires_both_usb_bus_and_a_removable_flag() {
        let info = DriveInfo {
            connection_bus: "usb".into(),
            removable: true,
            media_removable: false,
            serial: None,
        };
        assert!(info.looks_removable_usb());
    }

    #[test]
    fn non_usb_bus_is_never_removable_regardless_of_flags() {
        let info = DriveInfo {
            connection_bus: "ata".into(),
            removable: true,
            media_removable: true,
            serial: None,
        };
        assert!(!info.looks_removable_usb());
    }

    #[test]
    fn usb_bus_without_either_removable_flag_is_not_removable() {
        let info = DriveInfo {
            connection_bus: "usb".into(),
            removable: false,
            media_removable: false,
            serial: None,
        };
        assert!(!info.looks_removable_usb());
    }
}
