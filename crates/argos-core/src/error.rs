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

    #[error("'{path}' is {size_bytes} bytes, over FAT32's 4GiB-1 file limit, and cannot be split to fit (only a WIM can be, and a solid .esd cannot)")]
    WindowsFileTooLargeForFat32 { path: String, size_bytes: u64 },

    #[error("operation cancelled by user; the device is left in an inconsistent state and must be rewritten before use")]
    Cancelled,

    #[error("confirmation did not match; nothing was written and the device is untouched")]
    NotConfirmed,

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
            // Exit code 24 (WindowsImageRequiresLinux) was retired along
            // with the NTFS write path itself (phase 3 M4.3): the FAT32
            // layout it gated has run on both hosts since M4 (#34), so
            // nothing produces this error anymore. Exit code 25
            // (WindowsFileTooLargeForAvailableMemory) was retired earlier,
            // with the memory guard itself, when image::udf's streaming
            // reader removed the whole-file-in-memory cost (phase 3 M1,
            // #40). Both stay reserved rather than reused.
            ArgosError::WindowsFileTooLargeForFat32 { .. } => 26,
            // Distinct from Cancelled (18) on purpose: that one means a
            // write was interrupted partway and the media is unusable,
            // while this one means the user declined before anything was
            // touched. Printing Cancelled's "device is left in an
            // inconsistent state" right after "Nothing was written" read as
            // a contradiction, and could scare someone who simply typed the
            // device path wrong.
            ArgosError::NotConfirmed => 27,
        }
    }
}

pub type Result<T> = std::result::Result<T, ArgosError>;
