use std::collections::HashMap;
use std::io::Write;

use anyhow::{bail, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use flate2::Decompress;
use flate2::FlushDecompress;
use flate2::Status;
use sha1::{Digest, Sha1};

pub use gix_hash::ObjectId;

// ---------------------------------------------------------------------------
// Object types
// ---------------------------------------------------------------------------

pub const OBJ_COMMIT: u8 = 1;
pub const OBJ_TREE: u8 = 2;
pub const OBJ_BLOB: u8 = 3;
pub const OBJ_OFS_DELTA: u8 = 6;
pub const OBJ_REF_DELTA: u8 = 7;

// ---------------------------------------------------------------------------
// Varint helpers
// ---------------------------------------------------------------------------

pub fn decode_size_type(data: &[u8], pos: &mut usize) -> Result<(u8, usize)> {
    let byte = data[*pos];
    *pos += 1;
    let kind = (byte >> 4) & 7;
    let mut size = (byte & 0x0f) as usize;
    let mut c = byte;
    let mut shift = 4;
    while c & 0x80 != 0 {
        c = data[*pos];
        *pos += 1;
        size |= ((c & 0x7f) as usize) << shift;
        shift += 7;
    }
    Ok((kind, size))
}

pub fn encode_size_type(kind: u8, size: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut remaining = size;
    let mut byte = ((kind & 7) << 4) | (remaining as u8 & 0x0f);
    remaining >>= 4;
    if remaining > 0 {
        byte |= 0x80;
    }
    buf.push(byte);
    while remaining > 0 {
        let mut c = (remaining & 0x7f) as u8;
        remaining >>= 7;
        if remaining > 0 {
            c |= 0x80;
        }
        buf.push(c);
    }
    buf
}

// ---------------------------------------------------------------------------
// Object hashing
// ---------------------------------------------------------------------------

pub fn hash_object(kind: u8, content: &[u8]) -> ObjectId {
    let prefix: &[u8] = match kind {
        OBJ_COMMIT => b"commit",
        OBJ_TREE => b"tree",
        OBJ_BLOB => b"blob",
        _ => panic!("unknown object type {}", kind),
    };
    let mut hasher = Sha1::new();
    hasher.update(prefix);
    hasher.update(b" ");
    hasher.update(content.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(content);
    let raw: [u8; 20] = hasher.finalize().into();
    raw.into()
}

// ---------------------------------------------------------------------------
// Delta resolution
// ---------------------------------------------------------------------------

pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0usize;

    let read_varint = |p: &mut usize| -> Result<usize> {
        let mut val = 0usize;
        let mut shift = 0u32;
        loop {
            let b = delta[*p];
            *p += 1;
            val |= ((b & 0x7f) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok(val)
    };

    let _source_size = read_varint(&mut pos)?;
    let target_size = read_varint(&mut pos)?;

    let mut result = Vec::with_capacity(target_size);

    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;

        if cmd & 0x80 != 0 {
            let mut copy_offset = 0u32;
            let mut copy_size: u32 = 0;

            if cmd & 0x01 != 0 { copy_offset |= delta[pos] as u32; pos += 1; }
            if cmd & 0x02 != 0 { copy_offset |= (delta[pos] as u32) << 8; pos += 1; }
            if cmd & 0x04 != 0 { copy_offset |= (delta[pos] as u32) << 16; pos += 1; }
            if cmd & 0x08 != 0 { copy_offset |= (delta[pos] as u32) << 24; pos += 1; }
            if cmd & 0x10 != 0 { copy_size |= delta[pos] as u32; pos += 1; }
            if cmd & 0x20 != 0 { copy_size |= (delta[pos] as u32) << 8; pos += 1; }
            if cmd & 0x40 != 0 { copy_size |= (delta[pos] as u32) << 16; pos += 1; }

            if copy_size == 0 {
                copy_size = 0x10000;
            }

            let offset = copy_offset as usize;
            let size = copy_size as usize;
            let end = offset.saturating_add(size).min(base.len());
            result.extend_from_slice(&base[offset..end]);
        } else if cmd > 0 {
            let size = cmd as usize;
            result.extend_from_slice(&delta[pos..pos + size]);
            pos += size;
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Packfile parser
// ---------------------------------------------------------------------------

pub fn parse_packfile(data: &[u8]) -> Result<HashMap<ObjectId, Vec<u8>>> {
    if data.len() < 12 || &data[..4] != b"PACK" {
        bail!("not a valid packfile (bad magic)");
    }
    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if !(2..=3).contains(&version) {
        bail!("unsupported pack version {}", version);
    }
    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;

    let actual: [u8; 20] = Sha1::digest(&data[..data.len() - 20]).into();
    let expected: [u8; 20] = data[data.len() - 20..].try_into().unwrap();
    if actual != expected {
        bail!("packfile checksum mismatch");
    }

    let mut pos: usize = 12;
    let mut objects: HashMap<ObjectId, Vec<u8>> = HashMap::with_capacity(count);
    let mut obj_types: HashMap<ObjectId, u8> = HashMap::with_capacity(count);
    let mut by_offset: HashMap<usize, ObjectId> = HashMap::new();
    let mut deferred: Vec<(ObjectId, Vec<u8>, usize)> = Vec::new();
    let mut decompress = Decompress::new(true);

    for i in 0..count {
        let object_start = pos;
        let (kind, size) = decode_size_type(data, &mut pos)?;

        let mut delta_base_oid: Option<ObjectId> = None;
        let mut delta_base_pos: Option<usize> = None;

        match kind {
            OBJ_OFS_DELTA => {
                let mut off = 0usize;
                loop {
                    let b = data[pos];
                    pos += 1;
                    off = (off << 7) | ((b & 0x7f) as usize);
                    if b & 0x80 == 0 {
                        break;
                    }
                    off += 1;
                }
                delta_base_pos = Some(object_start - off);
            }
            OBJ_REF_DELTA => {
                let base: [u8; 20] = data[pos..pos + 20].try_into().unwrap();
                pos += 20;
                delta_base_oid = Some(ObjectId::from(base));
            }
            _ => {}
        }

        let mut content = Vec::with_capacity(size);
        loop {
            let before = decompress.total_in();
            let input = &data[pos..];
            content.reserve(8192);
            let result = decompress
                .decompress_vec(input, &mut content, FlushDecompress::None)
                .map_err(|e| anyhow::anyhow!("zlib error at byte {}: {}", pos, e))?;
            let consumed = (decompress.total_in() - before) as usize;
            pos += consumed;
            if result == Status::StreamEnd {
                break;
            }
            if consumed == 0 {
                bail!(
                    "zlib decompression stalled at byte {} (object {}, data_len={})",
                    pos, i, data.len()
                );
            }
        }
        decompress.reset(true);

        match kind {
            OBJ_COMMIT | OBJ_TREE | OBJ_BLOB => {
                let oid = hash_object(kind, &content);
                objects.insert(oid, content);
                obj_types.insert(oid, kind);
                by_offset.insert(object_start, oid);
            }
            OBJ_OFS_DELTA => {
                let base_pos = delta_base_pos.unwrap();
                let base_oid = by_offset
                    .get(&base_pos)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("OFS_DELTA base not found at offset 0x{:x}", base_pos))?;
                let base_type = obj_types[&base_oid];
                let resolved = apply_delta(&objects[&base_oid], &content)?;
                let oid = hash_object(base_type, &resolved);
                objects.insert(oid, resolved);
                obj_types.insert(oid, base_type);
                by_offset.insert(object_start, oid);
            }
            OBJ_REF_DELTA => {
                let base_oid = delta_base_oid.unwrap();
                match obj_types.get(&base_oid) {
                    Some(&base_type) => {
                        let resolved = apply_delta(&objects[&base_oid], &content)?;
                        let oid = hash_object(base_type, &resolved);
                        objects.insert(oid, resolved);
                        obj_types.insert(oid, base_type);
                        by_offset.insert(object_start, oid);
                    }
                    None => {
                        deferred.push((base_oid, content, object_start));
                    }
                }
            }
            _ => {}
        }
    }

    for (base_oid, delta_data, object_start) in deferred {
        match obj_types.get(&base_oid) {
            Some(&base_type) => {
                let resolved = apply_delta(&objects[&base_oid], &delta_data)?;
                let oid = hash_object(base_type, &resolved);
                objects.insert(oid, resolved);
                obj_types.insert(oid, base_type);
                by_offset.insert(object_start, oid);
            }
            None => {
                tracing::warn!("REF_DELTA base {} not found – skipping", base_oid);
            }
        }
    }

    Ok(objects)
}

// ---------------------------------------------------------------------------
// Packfile builder
// ---------------------------------------------------------------------------

pub fn build_packfile(entries: &[(u8, &[u8])]) -> Result<Vec<u8>> {
    let count = entries.len();
    let mut pack = Vec::new();

    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(count as u32).to_be_bytes());

    for &(kind, content) in entries {
        let header = encode_size_type(kind, content.len());
        pack.extend_from_slice(&header);

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content)?;
        let compressed = encoder.finish()?;
        pack.extend_from_slice(&compressed);
    }

    let checksum: [u8; 20] = Sha1::digest(&pack).into();
    pack.extend_from_slice(&checksum);

    Ok(pack)
}
