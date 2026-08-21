// Bounded incremental reads of one agent's wire.jsonl. Every frame reads at
// most a slice of new bytes, advances the raw byte offset even when the last
// JSONL row is incomplete, and persists the trailing partial line back onto
// the bucket so the next process reassembles it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::state::AgentBucket;

pub const WIRE_READ_BUDGET_BYTES: u64 = 1024 * 1024;
pub const MAIN_WIRE_SLICE_BYTES: u64 = 256 * 1024;
pub const AGENT_WIRE_SLICE_BYTES: u64 = 128 * 1024;
pub const MAX_PARTIAL_LINE_BYTES: usize = 1024 * 1024;

const MARKER_BYTES: u64 = 32;

/// SHA-256 hex of the bytes immediately before `offset` (None at offset 0).
pub fn tail_marker_hex(path: &Path, offset: u64) -> Option<String> {
    if offset == 0 {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let len = offset.min(MARKER_BYTES);
    let mut buf = vec![0u8; len as usize];
    file.seek(SeekFrom::Start(offset - len)).ok()?;
    file.read_exact(&mut buf).ok()?;
    Some(format!("{:x}", Sha256::digest(&buf)))
}

/// Whether the file still carries the bytes our cursor was left after.
/// A missing marker (offset 0) always matches.
pub fn wire_tail_matches(path: &Path, bucket: &AgentBucket) -> bool {
    let Some(stored) = &bucket.tail else {
        return true;
    };
    if bucket.offset == 0 {
        return true;
    }
    match tail_marker_hex(path, bucket.offset) {
        Some(actual) => actual == *stored,
        None => false,
    }
}

/// Read at most `max_bytes` of new data and advance the bucket offset,
/// returning the complete JSONL text found. None signals an IO error, in
/// which case the bucket is left where it was as far as practical.
pub fn read_bounded_wire(
    path: &Path,
    bucket: &mut AgentBucket,
    file_size: u64,
    max_bytes: u64,
) -> Option<String> {
    let available = file_size.saturating_sub(bucket.offset);
    let len = available.min(max_bytes) as usize;
    if len == 0 {
        return Some(String::new());
    }

    let mut file = File::open(path).ok()?;
    let mut chunk = vec![0u8; len];
    file.seek(SeekFrom::Start(bucket.offset)).ok()?;
    file.read_exact(&mut chunk).ok()?;

    bucket.offset += chunk.len() as u64;
    bucket.tail = tail_marker_hex(path, bucket.offset);

    let mut data = chunk;
    if bucket.discarding {
        match data.iter().position(|&b| b == 0x0a) {
            None => return Some(String::new()),
            Some(nl) => {
                data = data.split_off(nl + 1);
            }
        }
    }

    let mut combined = std::mem::take(&mut bucket.pending);
    combined.extend_from_slice(&data);

    let mut text = String::new();
    let mut line_start = 0usize;
    while let Some(pos) = combined[line_start..].iter().position(|&b| b == 0x0a) {
        let newline = line_start + pos;
        if newline - line_start <= MAX_PARTIAL_LINE_BYTES {
            text.push_str(&String::from_utf8_lossy(&combined[line_start..=newline]));
        }
        line_start = newline + 1;
    }
    let trailing = &combined[line_start..];
    if trailing.len() > MAX_PARTIAL_LINE_BYTES {
        bucket.pending = Vec::new();
        bucket.discarding = true;
    } else {
        bucket.pending = trailing.to_vec();
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kimi-hud-rs-wire-{}-{}",
            name,
            std::process::id()
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        path
    }

    #[test]
    fn partial_line_reassembles_across_reads() {
        let path = temp_file("split", b"{\"a\":1}\n{\"b\":");
        let meta = std::fs::metadata(&path).unwrap();
        let mut bucket = AgentBucket::default();
        // First read stops mid-row.
        let text = read_bounded_wire(&path, &mut bucket, meta.len(), 9).unwrap();
        assert_eq!(text, "{\"a\":1}\n");
        assert_eq!(bucket.pending, b"{".to_vec());
        // Second read finishes it.
        let text = read_bounded_wire(&path, &mut bucket, meta.len(), 1024).unwrap();
        assert_eq!(text, "");
        assert_eq!(bucket.pending, b"{\"b\":".to_vec());
        // Grow the file with the row's tail, read again.
        {
            let mut f = File::options().append(true).open(&path).unwrap();
            f.write_all(b"2}\n").unwrap();
        }
        let meta = std::fs::metadata(&path).unwrap();
        let text = read_bounded_wire(&path, &mut bucket, meta.len(), 1024).unwrap();
        assert_eq!(text, "{\"b\":2}\n");
        assert!(bucket.pending.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_marker_detects_truncation() {
        let path = temp_file("trunc", b"0123456789abcdefghijklmnopqrstuvwxyz\n");
        let meta = std::fs::metadata(&path).unwrap();
        let mut bucket = AgentBucket::default();
        read_bounded_wire(&path, &mut bucket, meta.len(), 1024).unwrap();
        assert!(wire_tail_matches(&path, &bucket));
        // Shrink the file in place: size is still > 0 but the tail bytes changed.
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(b"short\n").unwrap();
        }
        assert!(!wire_tail_matches(&path, &bucket));
        std::fs::remove_file(&path).ok();
    }
}
