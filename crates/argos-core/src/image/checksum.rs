//! SHA-256 checksumming of a readable stream. Used both to fingerprint the source
//! ISO before writing and to verify what actually landed on the device afterwards.
//! Streams in fixed-size chunks so multi-GB images never need to fit in memory.

use sha2::{Digest, Sha256};
use std::io::{self, Read};

const CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB

/// Computes the SHA-256 of everything `reader` yields, calling `on_chunk` after
/// each chunk with the number of bytes hashed so far (for progress reporting).
/// Does not know or care whether `reader` is a file, a device, or a cursor.
pub fn sha256_stream<R: Read>(mut reader: R, mut on_chunk: impl FnMut(u64)) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        on_chunk(total);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn matches_known_sha256_of_empty_input() {
        let hash = sha256_stream(Cursor::new(Vec::<u8>::new()), |_| {}).unwrap();
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn matches_known_sha256_of_abc() {
        let hash = sha256_stream(Cursor::new(b"abc".to_vec()), |_| {}).unwrap();
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn progress_callback_reports_final_total() {
        let data = vec![0u8; CHUNK_SIZE * 2 + 10];
        let mut last_seen = 0u64;
        sha256_stream(Cursor::new(data.clone()), |done| last_seen = done).unwrap();
        assert_eq!(last_seen, data.len() as u64);
    }
}
