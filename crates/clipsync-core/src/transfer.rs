//! Codec and bounded state for large clipboard/file transfers.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const BINARY_VERSION: u8 = 1;
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_TRANSFER_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_CHUNKS: u32 = (MAX_TRANSFER_BYTES as usize).div_ceil(MAX_CHUNK_BYTES) as u32;
const HEADER_BYTES: usize = 38;
const MAGIC: &[u8; 4] = b"CSB1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryChunk {
    pub transfer_id: [u8; 16],
    pub index: u32,
    pub total_chunks: u32,
    pub total_size: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransferError {
    #[error("binary frame is malformed")]
    Malformed,
    #[error("binary chunk exceeds limit")]
    ChunkTooLarge,
    #[error("transfer exceeds limit")]
    TransferTooLarge,
    #[error("unexpected chunk order")]
    OutOfOrder,
    #[error("transfer hash mismatch")]
    HashMismatch,
    #[error("I/O error: {0}")]
    Io(String),
}

impl BinaryChunk {
    pub fn encode(&self) -> Result<Vec<u8>, TransferError> {
        if self.bytes.len() > MAX_CHUNK_BYTES || self.total_size > MAX_TRANSFER_BYTES {
            return Err(if self.bytes.len() > MAX_CHUNK_BYTES {
                TransferError::ChunkTooLarge
            } else {
                TransferError::TransferTooLarge
            });
        }
        if self.total_chunks == 0
            || self.total_chunks > MAX_CHUNKS
            || self.index >= self.total_chunks
        {
            return Err(TransferError::Malformed);
        }
        let mut out = Vec::with_capacity(HEADER_BYTES + self.bytes.len());
        out.extend_from_slice(MAGIC);
        out.push(BINARY_VERSION);
        out.push(0);
        out.extend_from_slice(&self.transfer_id);
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.total_chunks.to_be_bytes());
        out.extend_from_slice(&self.total_size.to_be_bytes());
        out.extend_from_slice(&self.bytes);
        Ok(out)
    }

    pub fn decode(frame: &[u8]) -> Result<Self, TransferError> {
        if frame.len() < HEADER_BYTES || &frame[..4] != MAGIC || frame[4] != BINARY_VERSION {
            return Err(TransferError::Malformed);
        }
        let mut transfer_id = [0; 16];
        transfer_id.copy_from_slice(&frame[6..22]);
        let index = u32::from_be_bytes(frame[22..26].try_into().unwrap());
        let total_chunks = u32::from_be_bytes(frame[26..30].try_into().unwrap());
        let total_size = u64::from_be_bytes(frame[30..38].try_into().unwrap());
        let chunk = frame[HEADER_BYTES..].to_vec();
        let value = Self {
            transfer_id,
            index,
            total_chunks,
            total_size,
            bytes: chunk,
        };
        value.encode()?;
        if value.total_size < value.bytes.len() as u64 {
            return Err(TransferError::Malformed);
        }
        Ok(value)
    }
}

pub fn chunks(data: &[u8], transfer_id: [u8; 16]) -> impl Iterator<Item = BinaryChunk> + '_ {
    let total_chunks = data.len().div_ceil(MAX_CHUNK_BYTES) as u32;
    data.chunks(MAX_CHUNK_BYTES)
        .enumerate()
        .map(move |(index, bytes)| BinaryChunk {
            transfer_id,
            index: index as u32,
            total_chunks,
            total_size: data.len() as u64,
            bytes: bytes.to_vec(),
        })
}

/// Writes one chunk at a time. It never buffers the complete transfer.
#[derive(Debug)]
pub struct Receiver {
    file: File,
    path: PathBuf,
    expected_id: [u8; 16],
    expected_hash: [u8; 32],
    expected_size: u64,
    total_chunks: u32,
    next_index: u32,
    received: u64,
    hasher: Sha256,
}

impl Receiver {
    pub fn create(
        dir: impl AsRef<Path>,
        id: [u8; 16],
        hash: [u8; 32],
        size: u64,
        total_chunks: u32,
    ) -> Result<Self, TransferError> {
        if size > MAX_TRANSFER_BYTES || total_chunks == 0 || total_chunks > MAX_CHUNKS {
            return Err(TransferError::TransferTooLarge);
        }
        let path = dir.as_ref().join(format!("clipsync-{:x?}.part", id));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| TransferError::Io(e.to_string()))?;
        Ok(Self {
            file,
            path,
            expected_id: id,
            expected_hash: hash,
            expected_size: size,
            total_chunks,
            next_index: 0,
            received: 0,
            hasher: Sha256::new(),
        })
    }

    pub fn push(&mut self, chunk: BinaryChunk) -> Result<bool, TransferError> {
        if chunk.transfer_id != self.expected_id
            || chunk.total_chunks != self.total_chunks
            || chunk.total_size != self.expected_size
            || chunk.index != self.next_index
        {
            return Err(TransferError::OutOfOrder);
        }
        if chunk.bytes.len() > MAX_CHUNK_BYTES
            || self.received + chunk.bytes.len() as u64 > self.expected_size
        {
            return Err(TransferError::TransferTooLarge);
        }
        self.file
            .write_all(&chunk.bytes)
            .map_err(|e| TransferError::Io(e.to_string()))?;
        self.hasher.update(&chunk.bytes);
        self.received += chunk.bytes.len() as u64;
        self.next_index += 1;
        if self.next_index == self.total_chunks {
            if self.received != self.expected_size
                || self.hasher.clone().finalize().as_slice() != self.expected_hash
            {
                return Err(TransferError::HashMismatch);
            }
            self.file
                .flush()
                .map_err(|e| TransferError::Io(e.to_string()))?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn hash(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

impl From<io::Error> for TransferError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_and_chunking_round_trip() {
        let data = vec![7; MAX_CHUNK_BYTES * 2 + 3];
        let decoded: Vec<_> = chunks(&data, [4; 16])
            .map(|c| BinaryChunk::decode(&c.encode().unwrap()).unwrap())
            .collect();
        assert_eq!(
            decoded
                .iter()
                .flat_map(|c| c.bytes.iter())
                .copied()
                .collect::<Vec<_>>(),
            data
        );
        assert_eq!(decoded.len(), 3);
    }

    #[test]
    fn tamper_and_limits_are_rejected() {
        let mut frame = BinaryChunk {
            transfer_id: [0; 16],
            index: 0,
            total_chunks: 1,
            total_size: 1,
            bytes: vec![1],
        }
        .encode()
        .unwrap();
        frame[3] = b'X';
        assert_eq!(BinaryChunk::decode(&frame), Err(TransferError::Malformed));
        assert_eq!(
            BinaryChunk {
                transfer_id: [0; 16],
                index: 0,
                total_chunks: 1,
                total_size: 1,
                bytes: vec![0; MAX_CHUNK_BYTES + 1]
            }
            .encode(),
            Err(TransferError::ChunkTooLarge)
        );
    }

    #[test]
    fn receiver_detects_tampering() {
        let dir = std::env::temp_dir();
        let id = [9; 16];
        let data = b"streamed";
        let mut receiver = Receiver::create(&dir, id, hash(data), data.len() as u64, 1).unwrap();
        let mut chunk = chunks(data, id).next().unwrap();
        chunk.bytes[0] ^= 1;
        assert_eq!(receiver.push(chunk), Err(TransferError::HashMismatch));
        let _ = std::fs::remove_file(receiver.path());
    }
}
