//! Partition-region I/O (phase 3 M3.1, backlog #43): a `Read + Write + Seek`
//! window over an already-open whole-device handle, bounded to one
//! partition's byte range. This is what lets the FAT32 Windows write path
//! format and populate a partition through `fatfs` *without* re-reading the
//! partition table, waiting for per-partition device nodes to appear, or
//! mounting anything -- the three moving parts the NTFS path (W3) needs the
//! OS for.
//!
//! Also home to [`SizedDevice`], the wrapper that makes a raw device handle
//! answer "how big are you?" on platforms where the OS won't.
//!
//! Generic over the handle rather than hardcoding [`std::fs::File`] so unit
//! tests can run against an in-memory `Cursor` and integration tests against
//! a plain temp file -- no privilege, no loop device.

use argos_core::partition::windows::PartitionRegion;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Flushes a raw device's writes to the medium, tolerating platforms where
/// the strongest barrier isn't available on a device node.
///
/// [`std::fs::File::sync_all`] maps to `fcntl(F_FULLFSYNC)` on macOS, and
/// **device special files reject that with `ENOTTY`** ("Inappropriate ioctl
/// for device") -- confirmed against a real `/dev/diskN`, where the GPT had
/// already been written correctly and only the final sync failed, failing
/// the whole write. Plain `fsync(2)` on the same descriptor succeeds and is
/// what every other tool uses there, so fall back to it rather than
/// reporting a write that actually landed as failed.
///
/// The fallback is deliberately narrow: any error other than `ENOTTY`
/// propagates, so a genuine I/O failure is never swallowed.
pub fn sync_device(file: &std::fs::File) -> io::Result<()> {
    match file.sync_all() {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::ENOTTY) => {
            use std::os::fd::AsRawFd;
            // SAFETY: fsync takes a single valid file descriptor, which
            // `file` owns for the duration of this call.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        Err(err) => Err(err),
    }
}

/// Wraps a device handle so `Seek(SeekFrom::End(_))` is answered from a
/// known `total_size_bytes` instead of from the OS. Every other seek, read
/// and write passes straight through.
///
/// This exists because **macOS device special files don't support
/// `SEEK_END`**: `/dev/diskN` *and* `/dev/rdiskN` (both confirmed against
/// real hardware) report 0 rather than the device size, unlike Linux block
/// devices. `gptman` uses exactly that seek to size a brand-new GPT, so
/// without this it underflows and panics ("attempt to subtract with
/// overflow") before a single byte is written -- on any real macOS disk,
/// not just in tests.
///
/// `total_size_bytes` should be the platform-refreshed
/// [`argos_core::device::Device::size_bytes`], already re-validated against
/// the real device by the write path's TOCTOU check moments earlier --
/// trusted here for the same reason the capacity preflight trusts it.
pub struct SizedDevice<H> {
    inner: H,
    total_size_bytes: u64,
}

impl<H: Read + Write + Seek> SizedDevice<H> {
    pub fn new(inner: H, total_size_bytes: u64) -> Self {
        Self {
            inner,
            total_size_bytes,
        }
    }
}

impl<H: Read> Read for SizedDevice<H> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<H: Write> Write for SizedDevice<H> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<H: Seek> Seek for SizedDevice<H> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let SeekFrom::End(offset) = pos else {
            return self.inner.seek(pos);
        };
        let target = self
            .total_size_bytes
            .checked_add_signed(offset)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "seek relative to the device end is out of range",
                )
            })?;
        // Seek the underlying handle too, so the position the caller gets
        // back is the position it will actually read or write at.
        self.inner.seek(SeekFrom::Start(target))
    }
}

/// A bounded view of `inner` covering exactly `region`'s byte range.
/// Position 0 of the window is `region.start_offset_bytes` of the device;
/// reads past the window's end return EOF, writes past it fail (a filesystem
/// writer escaping its partition is always a bug, never something to clamp
/// silently).
///
/// Every operation re-seeks `inner` to the window's absolute position first,
/// so the window owns the handle's file position while it exists -- callers
/// must not interleave their own I/O on `inner` mid-use (taking `inner` by
/// value, `&mut File` included, enforces that through the borrow checker).
pub struct PartitionWindow<H> {
    inner: H,
    start_offset_bytes: u64,
    size_bytes: u64,
    position: u64,
}

impl<H: Read + Write + Seek> PartitionWindow<H> {
    pub fn new(inner: H, region: PartitionRegion) -> Self {
        Self {
            inner,
            start_offset_bytes: region.start_offset_bytes,
            size_bytes: region.size_bytes,
            position: 0,
        }
    }

    fn remaining(&self) -> u64 {
        self.size_bytes.saturating_sub(self.position)
    }

    fn seek_inner_to_position(&mut self) -> io::Result<()> {
        self.inner
            .seek(SeekFrom::Start(self.start_offset_bytes + self.position))?;
        Ok(())
    }
}

impl<H: Read + Write + Seek> Read for PartitionWindow<H> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let allowed = self.remaining().min(buf.len() as u64) as usize;
        if allowed == 0 {
            return Ok(0);
        }
        self.seek_inner_to_position()?;
        let n = self.inner.read(&mut buf[..allowed])?;
        self.position += n as u64;
        Ok(n)
    }
}

impl<H: Read + Write + Seek> Write for PartitionWindow<H> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let allowed = self.remaining().min(buf.len() as u64) as usize;
        if allowed == 0 {
            return Err(io::Error::other(format!(
                "write at offset {} would escape the {}-byte partition window",
                self.position, self.size_bytes
            )));
        }
        self.seek_inner_to_position()?;
        let n = self.inner.write(&buf[..allowed])?;
        self.position += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<H: Read + Write + Seek> Seek for PartitionWindow<H> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Same contract as `File`: seeking anywhere >= 0 succeeds (even past
        // the end -- reads there hit EOF, writes there fail), seeking below
        // 0 is an error.
        let new_position = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.size_bytes.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        match new_position {
            Some(p) => {
                self.position = p;
                Ok(p)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the partition window",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn region(start: u64, size: u64) -> PartitionRegion {
        PartitionRegion {
            start_offset_bytes: start,
            size_bytes: size,
        }
    }

    /// A 64-byte backing "device" where every byte holds its own index, so
    /// tests can tell exactly which device bytes a window operation touched.
    fn device() -> Cursor<Vec<u8>> {
        Cursor::new((0u8..64).collect())
    }

    #[test]
    fn reads_start_at_the_region_offset() {
        let mut window = PartitionWindow::new(device(), region(16, 32));
        let mut buf = [0u8; 4];
        window.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [16, 17, 18, 19]);
    }

    #[test]
    fn reads_hit_eof_at_the_region_end_not_the_device_end() {
        let mut window = PartitionWindow::new(device(), region(16, 8));
        let mut all = Vec::new();
        window.read_to_end(&mut all).unwrap();
        assert_eq!(all, (16u8..24).collect::<Vec<_>>());
    }

    #[test]
    fn writes_land_inside_the_region_and_nowhere_else() {
        let mut cursor = device();
        {
            let mut window = PartitionWindow::new(&mut cursor, region(16, 8));
            window.write_all(&[0xAA; 8]).unwrap();
        }
        let bytes = cursor.into_inner();
        assert_eq!(bytes[15], 15, "byte before the region must be untouched");
        assert_eq!(&bytes[16..24], &[0xAA; 8]);
        assert_eq!(bytes[24], 24, "byte after the region must be untouched");
    }

    #[test]
    fn writing_past_the_region_end_fails_instead_of_spilling() {
        let mut cursor = device();
        {
            let mut window = PartitionWindow::new(&mut cursor, region(16, 8));
            let err = window.write_all(&[0xBB; 9]).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::Other);
        }
        // The in-bounds prefix was written (normal `Write` short-write
        // semantics); the byte past the region was not.
        let bytes = cursor.into_inner();
        assert_eq!(&bytes[16..24], &[0xBB; 8]);
        assert_eq!(bytes[24], 24);
    }

    #[test]
    fn seek_from_end_reports_the_region_size_not_the_device_size() {
        let mut window = PartitionWindow::new(device(), region(16, 32));
        assert_eq!(window.seek(SeekFrom::End(0)).unwrap(), 32);
    }

    #[test]
    fn seek_before_the_start_is_an_error() {
        let mut window = PartitionWindow::new(device(), region(16, 32));
        assert!(window.seek(SeekFrom::Current(-1)).is_err());
    }

    /// A handle that reports 0 for `SeekFrom::End` -- what a macOS
    /// `/dev/diskN` actually does, and what `SizedDevice` exists to paper
    /// over. Everything else behaves like the `Cursor` underneath.
    struct EndBlindDevice(Cursor<Vec<u8>>);

    impl Read for EndBlindDevice {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Write for EndBlindDevice {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
    impl Seek for EndBlindDevice {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            match pos {
                SeekFrom::End(_) => Ok(0),
                other => self.0.seek(other),
            }
        }
    }

    #[test]
    fn sync_device_succeeds_on_an_ordinary_file() {
        // The fallback path only triggers on ENOTTY (a device node); on a
        // regular file sync_all works and must be what's used. The real
        // device-node case is covered by the hdiutil integration tests,
        // which is where the ENOTTY was found in the first place.
        let file = tempfile::tempfile().unwrap();
        sync_device(&file).unwrap();
    }

    #[test]
    fn sized_device_answers_seek_from_end_when_the_os_will_not() {
        let raw = EndBlindDevice(Cursor::new(vec![0u8; 64]));
        let mut device = SizedDevice::new(raw, 64);
        assert_eq!(device.seek(SeekFrom::End(0)).unwrap(), 64);
        assert_eq!(device.seek(SeekFrom::End(-8)).unwrap(), 56);
    }

    #[test]
    fn sized_device_leaves_the_handle_positioned_where_it_reported() {
        let mut backing = (0u8..64).collect::<Vec<_>>();
        backing[56] = 0xC0;
        let mut device = SizedDevice::new(EndBlindDevice(Cursor::new(backing)), 64);
        device.seek(SeekFrom::End(-8)).unwrap();
        let mut byte = [0u8; 1];
        device.read_exact(&mut byte).unwrap();
        assert_eq!(
            byte[0], 0xC0,
            "the read must happen where the seek reported"
        );
    }

    #[test]
    fn sized_device_passes_other_seeks_straight_through() {
        let mut device = SizedDevice::new(EndBlindDevice(Cursor::new(vec![0u8; 64])), 64);
        assert_eq!(device.seek(SeekFrom::Start(10)).unwrap(), 10);
        assert_eq!(device.seek(SeekFrom::Current(5)).unwrap(), 15);
    }

    #[test]
    fn sized_device_refuses_a_seek_before_the_device_start() {
        let mut device = SizedDevice::new(EndBlindDevice(Cursor::new(vec![0u8; 64])), 64);
        assert!(device.seek(SeekFrom::End(-65)).is_err());
    }

    #[test]
    fn interleaved_seeks_and_reads_stay_window_relative() {
        let mut window = PartitionWindow::new(device(), region(16, 32));
        window.seek(SeekFrom::Start(4)).unwrap();
        let mut buf = [0u8; 2];
        window.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [20, 21]);
        window.seek(SeekFrom::Current(2)).unwrap();
        window.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [24, 25]);
    }
}
