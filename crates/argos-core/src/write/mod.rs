pub mod dd_mode;

/// How an image gets onto a device. Only [`WriteStrategy::Dd`] is implemented
/// in v1 (see `image::isohybrid` -- only `Hybrid` images are accepted, and
/// they carry their own MBR/GPT and bootloaders already). `PartitionCopy` is
/// reserved for a future phase that handles non-hybrid ISOs and persistence
/// partitions by actually creating partition tables (via `gptman`/`mbrman`)
/// instead of copying bytes verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStrategy {
    Dd,
    PartitionCopy,
}
