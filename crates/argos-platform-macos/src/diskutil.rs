//! Pure parsing of the XML property lists `diskutil list -plist` and
//! `diskutil info -plist <id>` print to stdout, plus the whole-disk id
//! arithmetic built on top of it. Kept free of any process/filesystem access
//! (mirrors `argos-platform-linux`'s `sysfs.rs`/`mounts.rs` split) so the
//! parsing itself is unit-testable with plain byte strings -- no `diskutil`,
//! no macOS, required to run these tests. Running `diskutil` and turning its
//! output into a [`Device`](argos_core::device::Device) happens in
//! `enumerate.rs`.

use plist::Dictionary;

/// Fields pulled out of one `diskutil info -plist <id>` dictionary. Every
/// field is read defensively (missing key -> a documented default) rather
/// than required, since Apple has changed this schema across macOS releases
/// before (backlog E3 calls this out explicitly) and a missing key here must
/// degrade gracefully rather than panic or refuse the whole listing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskInfo {
    pub device_identifier: String,
    pub device_node: String,
    pub size_bytes: u64,
    /// Defaults to `true` (the conservative reading) when the key is
    /// missing: a disk `diskutil` doesn't clearly report as external must
    /// never be silently treated as one.
    pub internal: bool,
    pub removable_media: bool,
    pub bus_protocol: Option<String>,
    pub virtual_or_physical: Option<String>,
    pub parent_whole_disk: Option<String>,
    pub whole_disk: bool,
    pub media_name: Option<String>,
    pub volume_name: Option<String>,
    pub serial_number: Option<String>,
    /// First-level physical-store device identifiers backing this disk, when
    /// it's a synthesized APFS container (e.g. `["disk0s2"]`). Empty for a
    /// disk that isn't a container.
    pub physical_stores: Vec<String>,
}

/// Parses one `diskutil info -plist <id>` document (the top-level dict
/// describing a single disk or volume). Returns `None` if `xml` isn't a
/// parseable plist dict, or is the `Error` dict `diskutil` prints for an
/// identifier it doesn't recognize -- callers treat that as "this disk is
/// gone", the same as any other lookup miss.
pub fn parse_disk_info(xml: &[u8]) -> Option<DiskInfo> {
    let value = plist::Value::from_reader_xml(xml).ok()?;
    let dict = value.as_dictionary()?;
    if boolean(dict, "Error") == Some(true) {
        return None;
    }
    Some(DiskInfo {
        device_identifier: string(dict, "DeviceIdentifier").unwrap_or_default(),
        device_node: string(dict, "DeviceNode").unwrap_or_default(),
        size_bytes: dict
            .get("Size")
            .and_then(|v| v.as_unsigned_integer())
            .unwrap_or(0),
        internal: boolean(dict, "Internal").unwrap_or(true),
        removable_media: boolean(dict, "RemovableMedia")
            .or_else(|| boolean(dict, "Removable"))
            .unwrap_or(false),
        bus_protocol: string(dict, "BusProtocol"),
        virtual_or_physical: string(dict, "VirtualOrPhysical"),
        parent_whole_disk: string(dict, "ParentWholeDisk"),
        whole_disk: boolean(dict, "WholeDisk").unwrap_or(false),
        media_name: non_empty_string(dict, "MediaName"),
        volume_name: non_empty_string(dict, "VolumeName"),
        serial_number: non_empty_string(dict, "SerialNumber"),
        physical_stores: physical_stores(dict),
    })
}

/// Parses the top-level `diskutil list -plist` document (no target given),
/// returning its `WholeDisks` array: every disk identifier `diskutil`
/// considers a top-level device, in the order it reports them. This
/// includes both real physical disks and synthesized APFS-container
/// pseudo-disks -- callers tell the two apart via
/// [`DiskInfo::virtual_or_physical`]. Returns `None` if `xml` doesn't parse
/// or has no `WholeDisks` array.
pub fn parse_whole_disks(xml: &[u8]) -> Option<Vec<String>> {
    let value = plist::Value::from_reader_xml(xml).ok()?;
    let dict = value.as_dictionary()?;
    let array = dict.get("WholeDisks")?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(|v| v.as_string().map(str::to_owned))
            .collect(),
    )
}

/// Strips a partition/snapshot suffix from a macOS disk identifier, e.g.
/// `disk0s2` -> `disk0`, `disk3s1s1` -> `disk3`. An identifier that's
/// already a whole disk is returned unchanged. Unlike Linux device names,
/// macOS identifiers always start with the literal `disk` followed by
/// digits, so the whole-disk id is exactly that prefix -- no per-bus-family
/// special-casing needed the way Linux's `nvme0n1p2`/`mmcblk0p1` naming
/// requires.
pub fn whole_disk_of(identifier: &str) -> String {
    match identifier.strip_prefix("disk") {
        Some(rest) => {
            let digit_len = rest.chars().take_while(char::is_ascii_digit).count();
            format!("disk{}", &rest[..digit_len])
        }
        None => identifier.to_string(),
    }
}

/// Resolves the physical whole disk actually holding `container` (the
/// `DiskInfo` of a disk identified by [`DiskInfo::parent_whole_disk`]),
/// walking through a synthesized APFS container to the physical partition
/// backing it when there is one. On Apple Silicon the volume mounted at `/`
/// sits inside such a container, so without this step its parent whole disk
/// would itself be a virtual pseudo-disk rather than the real internal SSD --
/// exactly the mistake backlog E3 calls out as the thing to avoid.
pub fn resolve_physical_system_disk(container: &DiskInfo) -> String {
    match container.physical_stores.first() {
        Some(store) => whole_disk_of(store),
        None => container.device_identifier.clone(),
    }
}

fn string(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key).and_then(|v| v.as_string()).map(str::to_owned)
}

fn non_empty_string(dict: &Dictionary, key: &str) -> Option<String> {
    string(dict, key).filter(|s| !s.is_empty())
}

fn boolean(dict: &Dictionary, key: &str) -> Option<bool> {
    dict.get(key).and_then(|v| v.as_boolean())
}

/// `APFSPhysicalStores` shows up under two different inner key names
/// depending on which `diskutil` subcommand produced the plist: `diskutil
/// info -plist <container>` nests `{"APFSPhysicalStore": "disk0s2"}`, while
/// `diskutil list -plist`'s `AllDisksAndPartitions` nests
/// `{"DeviceIdentifier": "disk0s2"}` for the same information. Both are
/// checked so this crate only needs `info -plist` calls, never `list
/// -plist`'s much larger nested form, to resolve a container's physical
/// store.
fn physical_stores(dict: &Dictionary) -> Vec<String> {
    dict.get("APFSPhysicalStores")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.as_dictionary())
                .filter_map(|d| {
                    string(d, "APFSPhysicalStore").or_else(|| string(d, "DeviceIdentifier"))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured verbatim from `diskutil info -plist disk0` on a real Apple
    // Silicon Mac (macOS 26 "Tahoe"), trimmed to the keys this module reads.
    const INTERNAL_WHOLE_DISK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>BusProtocol</key>
	<string>Apple Fabric</string>
	<key>DeviceIdentifier</key>
	<string>disk0</string>
	<key>DeviceNode</key>
	<string>/dev/disk0</string>
	<key>Internal</key>
	<true/>
	<key>MediaName</key>
	<string>APPLE SSD AP1024Z</string>
	<key>ParentWholeDisk</key>
	<string>disk0</string>
	<key>Removable</key>
	<false/>
	<key>RemovableMedia</key>
	<false/>
	<key>Size</key>
	<integer>1000555581440</integer>
	<key>VolumeName</key>
	<string></string>
	<key>WholeDisk</key>
	<true/>
</dict>
</plist>
"#;

    // Captured verbatim from `diskutil info -plist disk3` (the APFS
    // container holding the boot volume) on the same machine.
    const APFS_CONTAINER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>APFSPhysicalStores</key>
	<array>
		<dict>
			<key>APFSPhysicalStore</key>
			<string>disk0s2</string>
		</dict>
	</array>
	<key>BusProtocol</key>
	<string>Apple Fabric</string>
	<key>DeviceIdentifier</key>
	<string>disk3</string>
	<key>DeviceNode</key>
	<string>/dev/disk3</string>
	<key>Internal</key>
	<true/>
	<key>ParentWholeDisk</key>
	<string>disk3</string>
	<key>RemovableMedia</key>
	<false/>
	<key>Size</key>
	<integer>994610155520</integer>
	<key>VirtualOrPhysical</key>
	<string>Virtual</string>
	<key>WholeDisk</key>
	<true/>
</dict>
</plist>
"#;

    // Captured verbatim from `diskutil info -plist disk4` on the same
    // machine, with a real USB stick (a SanDisk 3.2Gen1 flash drive)
    // plugged in -- confirming the schema this module assumed before a
    // drive was available to test against (backlog E3's "Status" note).
    // `SerialNumber` is genuinely absent here, confirming that field's
    // `Option` handling isn't just a defensive guess.
    const EXTERNAL_USB_WHOLE_DISK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>BusProtocol</key>
	<string>USB</string>
	<key>DeviceIdentifier</key>
	<string>disk4</string>
	<key>DeviceNode</key>
	<string>/dev/disk4</string>
	<key>Internal</key>
	<false/>
	<key>IORegistryEntryName</key>
	<string>USB SanDisk 3.2Gen1 Media</string>
	<key>MediaName</key>
	<string>SanDisk 3.2Gen1</string>
	<key>ParentWholeDisk</key>
	<string>disk4</string>
	<key>Removable</key>
	<true/>
	<key>RemovableMedia</key>
	<true/>
	<key>Size</key>
	<integer>30784094208</integer>
	<key>VirtualOrPhysical</key>
	<string>Physical</string>
	<key>WholeDisk</key>
	<true/>
</dict>
</plist>
"#;

    const LIST_WHOLE_DISKS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>WholeDisks</key>
	<array>
		<string>disk0</string>
		<string>disk1</string>
		<string>disk4</string>
	</array>
</dict>
</plist>
"#;

    const ERROR_DOCUMENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Error</key>
	<true/>
	<key>ErrorMessage</key>
	<string>Could not find disk: diskZZ</string>
	<key>ExitCode</key>
	<integer>1</integer>
</dict>
</plist>
"#;

    #[test]
    fn parses_internal_whole_disk() {
        let info = parse_disk_info(INTERNAL_WHOLE_DISK.as_bytes()).unwrap();
        assert_eq!(info.device_identifier, "disk0");
        assert_eq!(info.device_node, "/dev/disk0");
        assert!(info.internal);
        assert!(!info.removable_media);
        assert_eq!(info.bus_protocol.as_deref(), Some("Apple Fabric"));
        assert_eq!(info.size_bytes, 1_000_555_581_440);
        // An empty VolumeName string is treated as absent, not "".
        assert_eq!(info.volume_name, None);
    }

    #[test]
    fn parses_apfs_container_and_its_physical_store() {
        let info = parse_disk_info(APFS_CONTAINER.as_bytes()).unwrap();
        assert_eq!(info.virtual_or_physical.as_deref(), Some("Virtual"));
        assert_eq!(info.physical_stores, vec!["disk0s2".to_string()]);
    }

    #[test]
    fn parses_external_usb_whole_disk() {
        let info = parse_disk_info(EXTERNAL_USB_WHOLE_DISK.as_bytes()).unwrap();
        assert!(!info.internal);
        assert!(info.removable_media);
        assert_eq!(info.bus_protocol.as_deref(), Some("USB"));
        assert_eq!(info.virtual_or_physical.as_deref(), Some("Physical"));
    }

    #[test]
    fn error_document_parses_to_none() {
        assert_eq!(parse_disk_info(ERROR_DOCUMENT.as_bytes()), None);
    }

    #[test]
    fn garbage_input_parses_to_none_instead_of_panicking() {
        assert_eq!(parse_disk_info(b"not a plist at all"), None);
    }

    #[test]
    fn parses_whole_disks_list() {
        let ids = parse_whole_disks(LIST_WHOLE_DISKS.as_bytes()).unwrap();
        assert_eq!(ids, vec!["disk0", "disk1", "disk4"]);
    }

    #[test]
    fn whole_disk_of_strips_partition_suffix() {
        assert_eq!(whole_disk_of("disk0s2"), "disk0");
        assert_eq!(whole_disk_of("disk0"), "disk0");
    }

    #[test]
    fn whole_disk_of_strips_snapshot_suffix_too() {
        assert_eq!(whole_disk_of("disk3s1s1"), "disk3");
    }

    #[test]
    fn whole_disk_of_handles_multi_digit_disk_numbers() {
        assert_eq!(whole_disk_of("disk10s3"), "disk10");
    }

    #[test]
    fn resolve_physical_system_disk_walks_through_apfs_container() {
        let container = parse_disk_info(APFS_CONTAINER.as_bytes()).unwrap();
        assert_eq!(resolve_physical_system_disk(&container), "disk0");
    }

    #[test]
    fn resolve_physical_system_disk_falls_back_when_not_a_container() {
        let plain = parse_disk_info(INTERNAL_WHOLE_DISK.as_bytes()).unwrap();
        assert_eq!(resolve_physical_system_disk(&plain), "disk0");
    }
}
