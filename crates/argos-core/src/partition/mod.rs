//! Partition-table planning: pure arithmetic, no disk I/O. This is the piece
//! `write::WriteStrategy::PartitionCopy` has been reserved for since v1 --
//! anything that needs to actually *create* a partition table (GPT via
//! `gptman`, formatting, mounting) lives in `argos-privileged` instead (W3),
//! which turns a plan computed here into real bytes on a real device.

pub mod windows;

pub use windows::WindowsPartitionPlan;
