//! Central error type for Argos. Every fallible operation in `argos-core` and the
//! platform crates should eventually resolve into an [`ArgosError`], so the CLI (and,
//! later, a GUI) can map failures to distinct, scriptable exit codes instead of a
//! generic "something went wrong".

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ArgosError {
    #[error("no device matches '{0}'")]
    DeviceNotFound(String),

    #[error(
        "device '{0}' is not a removable disk; refusing to write without --i-know-what-im-doing"
    )]
    DeviceNotRemovable(String),

    #[error("device '{0}' looks like a system disk (it holds a mounted '/', '/boot', '/boot/efi' or '/home' partition); refusing to write")]
    DeviceIsSystemDisk(String),

    #[error("insufficient permissions to access '{0}'; try running with elevated privileges")]
    InsufficientPermissions(String),

    #[error("device '{0}' ({2} bytes) is smaller than the image '{1}' ({3} bytes)")]
    DeviceTooSmall(String, PathBuf, u64, u64),

    #[error("the image file '{0}' is stored on the very device it would be written to")]
    SourceTargetCollision(PathBuf),

    #[error("'{0}' is not a Linux ISO9660 image Argos recognizes")]
    UnsupportedIso(PathBuf),

    #[error("'{0}' is not a Windows installer ISO Argos recognizes (no 'bootmgr' + 'sources/boot.wim' at the root)")]
    NotWindowsInstallerIso(PathBuf),

    #[error("checksum mismatch after writing: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("partition table does not match the expected Windows write layout: {0}")]
    WindowsPartitionLayoutMismatch(String),

    #[error("'{path}' does not match after writing: expected {expected}, got {actual}")]
    WindowsFileMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("writing or verifying a Windows installer image requires a Linux host with ntfs-3g installed, for now")]
    WindowsImageRequiresLinux,

    #[error("operation cancelled by user; the device is left in an inconsistent state and must be rewritten before use")]
    Cancelled,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}

impl ArgosError {
    /// Stable exit code per error category, so scripts/CI wrapping the CLI can branch
    /// on failure kind instead of parsing human-readable text.
    pub fn exit_code(&self) -> i32 {
        match self {
            ArgosError::DeviceNotFound(_) => 10,
            ArgosError::DeviceNotRemovable(_) => 11,
            ArgosError::DeviceIsSystemDisk(_) => 12,
            ArgosError::InsufficientPermissions(_) => 13,
            ArgosError::DeviceTooSmall(..) => 14,
            ArgosError::SourceTargetCollision(_) => 15,
            ArgosError::UnsupportedIso(_) => 16,
            ArgosError::ChecksumMismatch { .. } => 17,
            ArgosError::Cancelled => 18,
            ArgosError::Io(_) => 19,
            ArgosError::NotImplemented(_) => 20,
            ArgosError::NotWindowsInstallerIso(_) => 21,
            ArgosError::WindowsPartitionLayoutMismatch(_) => 22,
            ArgosError::WindowsFileMismatch { .. } => 23,
            ArgosError::WindowsImageRequiresLinux => 24,
        }
    }
}

pub type Result<T> = std::result::Result<T, ArgosError>;
