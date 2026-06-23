use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use flate2::Decompress;
use flate2::FlushDecompress;
use flate2::Status;
use sha1::{Digest, Sha1};

pub use gix_hash::ObjectId;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const FETCH_THROTTLE: Duration = Duration::from_secs(3);
const MAX_RETRIES: u32 = 3;

// ---------------------------------------------------------------------------
// URL parsing for ssh:// URLs
// ---------------------------------------------------------------------------

struct SshTarget {
    user: Option<String>,
    host: String,
    port: Option<u16>,
    path: String,
}

type ObjectList = Vec<(u8, Vec<u8>)>;

fn parse_ssh_url(url: &str) -> Result<SshTarget> {
    if let Some(rest) = url.strip_prefix("ssh://") {
        // ssh://[user@]host[:port]/path
        let (user_host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let path = format!("/{}", path);

        let (userinfo, host_with_port) = user_host.split_once('@').unwrap_or(("", user_host));
        let (host, port_str) = host_with_port.split_once(':').unwrap_or((host_with_port, ""));
        let port = if port_str.is_empty() {
            None
        } else {
            Some(port_str.parse::<u16>().context("invalid port")?)
        };
        let user = if userinfo.is_empty() {
            None
        } else {
            Some(userinfo.to_owned())
        };

        Ok(SshTarget { user, host: host.to_owned(), port, path })
    } else if let Some(at_pos) = url.rfind('@') {
        // SCP-style: [user@]host:path  (no ssh:// prefix)
        let user = if at_pos > 0 {
            Some(url[..at_pos].to_owned())
        } else {
            None
        };
        let rest = &url[at_pos + 1..];
        if let Some(colon_pos) = rest.find(':') {
            let host = rest[..colon_pos].to_owned();
            let path = rest[colon_pos + 1..].to_owned();
            let path = if path.starts_with('/') { path } else { format!("/{}", path) };
            Ok(SshTarget { user, host, port: None, path })
        } else {
            bail!("invalid SCP-style URL: no colon after host in '{}'", url)
        }
    } else if let Some(colon_pos) = url.find(':') {
        // SCP-style without user: host:path
        let host = url[..colon_pos].to_owned();
        let path = url[colon_pos + 1..].to_owned();
        let path = if path.starts_with('/') { path } else { format!("/{}", path) };
        Ok(SshTarget { user: None, host, port: None, path })
    } else {
        bail!("URL must start with ssh:// or be in SCP-style [user@]host:path");
    }
}

// ---------------------------------------------------------------------------
// SSH process management
// ---------------------------------------------------------------------------

fn build_ssh_cmd(
    target: &SshTarget,
    service: &str,
    ssh_key: Option<&str>,
    _password: Option<&str>,
) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(port) = target.port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(key) = ssh_key {
        cmd.arg("-i").arg(key);
    }

    let destination = match &target.user {
        Some(u) => format!("{}@{}", u, target.host),
        None => target.host.clone(),
    };
    cmd.arg(destination);
    cmd.arg(format!("{} '{}'", service, target.path));
    cmd
}

/// Spawn an SSH child for the given git service, return (child, stdout_bufreader).
fn spawn_ssh(
    remote_url: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
    service: &str,
) -> Result<(Child, BufReader<std::process::ChildStdout>)> {
    let target = parse_ssh_url(remote_url)?;
    let mut child = build_ssh_cmd(&target, service, ssh_key, password)
        .spawn()
        .context("failed to spawn ssh")?;

    let stdout = BufReader::new(child.stdout.take().context("no stdout from ssh")?);
    Ok((child, stdout))
}

// ---------------------------------------------------------------------------
// pkt-line helpers
// ---------------------------------------------------------------------------

fn read_pkt_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len_str = std::str::from_utf8(&len_buf)?;
    let len = usize::from_str_radix(len_str, 16).context("invalid pkt-line length")?;
    if len == 0 {
        return Ok(None); // flush
    }
    if len == 1 {
        // Half-duplex close – treat as flush
        return Ok(None);
    }
    let mut data = vec![0u8; len - 4];
    reader.read_exact(&mut data)?;
    Ok(Some(data))
}

fn write_pkt_line(writer: &mut impl Write, data: &[u8]) -> Result<()> {
    let len = data.len() + 4;
    if len > 0xffff {
        bail!("pkt-line too long: {} bytes", len);
    }
    write!(writer, "{:04x}", len)?;
    writer.write_all(data)?;
    Ok(())
}

fn write_flush(writer: &mut impl Write) -> Result<()> {
    writer.write_all(b"0000")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Varint helpers (type + size encoding used in packfiles)
// ---------------------------------------------------------------------------

const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

fn decode_size_type(data: &[u8], pos: &mut usize) -> Result<(u8, usize)> {
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

fn encode_size_type(kind: u8, size: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut remaining = size;
    // First byte: continuation flag (bit 7), type (bits 6-4), size low 4 bits (bits 3-0)
    let mut byte = ((kind & 7) << 4) | (remaining as u8 & 0x0f);
    remaining >>= 4;
    if remaining > 0 {
        byte |= 0x80;
    }
    buf.push(byte);
    // Subsequent bytes: continuation flag (bit 7), size (bits 6-0)
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
// Object hashing (sha1 of "type size\0content")
// ---------------------------------------------------------------------------

fn hash_object(kind: u8, content: &[u8]) -> ObjectId {
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
// Packfile parser – parse a complete packfile into (ObjectId, content) map
// ---------------------------------------------------------------------------

fn parse_packfile(data: &[u8]) -> Result<HashMap<ObjectId, Vec<u8>>> {
    if data.len() < 12 || &data[..4] != b"PACK" {
        bail!("not a valid packfile (bad magic)");
    }
    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if !(2..=3).contains(&version) {
        bail!("unsupported pack version {}", version);
    }
    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;

    // Verify trailing SHA1 checksum
    let actual: [u8; 20] = Sha1::digest(&data[..data.len() - 20]).into();
    let expected: [u8; 20] = data[data.len() - 20..].try_into().unwrap();
    if actual != expected {
        bail!("packfile checksum mismatch");
    }

    let mut pos: usize = 12;
    let mut objects: HashMap<ObjectId, Vec<u8>> = HashMap::with_capacity(count);
    let mut obj_types: HashMap<ObjectId, u8> = HashMap::with_capacity(count);
    let mut by_offset: HashMap<usize, ObjectId> = HashMap::new(); // packfile pos → OID (for OFS_DELTA)
    let mut deferred: Vec<(ObjectId, Vec<u8>, usize)> = Vec::new(); // (base_oid, delta_data, obj_start)
    let mut decompress = Decompress::new(true);

    for i in 0..count {
        let object_start = pos;
        let (kind, size) = decode_size_type(data, &mut pos)?;

        let mut delta_base_oid: Option<ObjectId> = None;
        let mut delta_base_pos: Option<usize> = None;

        match kind {
            OBJ_OFS_DELTA => {
                // Parse the delta offset (big-endian varint with +1 per continuation byte)
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

        // Decompress the zlib stream
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

    // Second pass: resolve deferred REF_DELTA objects
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
// Packfile builder – build a packfile from (kind, content) entries
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Delta resolution – apply a Git delta to a base object
// ---------------------------------------------------------------------------

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
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
            // Copy from base
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
            // Insert literal data
            let size = cmd as usize;
            result.extend_from_slice(&delta[pos..pos + size]);
            pos += size;
        }
        // cmd == 0 is reserved / no-op
    }

    Ok(result)
}

fn build_packfile(entries: &[(u8, &[u8])]) -> Result<Vec<u8>> {
    let count = entries.len();
    let mut pack = Vec::new();

    // Header
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes()); // version 2
    pack.extend_from_slice(&(count as u32).to_be_bytes());

    for &(kind, content) in entries {
        let header = encode_size_type(kind, content.len());
        pack.extend_from_slice(&header);

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content)?;
        let compressed = encoder.finish()?;
        pack.extend_from_slice(&compressed);
    }

    // SHA1 checksum of everything above
    let checksum: [u8; 20] = Sha1::digest(&pack).into();
    pack.extend_from_slice(&checksum);

    Ok(pack)
}

// ---------------------------------------------------------------------------
// Tree walking – recursively walk tree objects to build path→content map
// ---------------------------------------------------------------------------

use gix_object::TreeRefIter;

fn walk_tree(
    objects: &HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
    base: &Path,
    map: &mut HashMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    let tree_data = objects
        .get(tree_oid)
        .with_context(|| format!("missing tree object {}", tree_oid))?;

    let iter = TreeRefIter::from_bytes(tree_data, gix_hash::Kind::Sha1);
    for entry in iter {
        let entry = entry?;
        let name = std::str::from_utf8(entry.filename)?;
        let path = base.join(name);

        let entry_oid: ObjectId = entry.oid.to_owned();
        if entry.mode.is_tree() {
            walk_tree(objects, &entry_oid, &path, map)?;
        } else if let Some(blob_data) = objects.get(&entry_oid) {
            map.insert(path, blob_data.to_vec());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tree building from a file path→data map
// ---------------------------------------------------------------------------

/// Build a tree object from a set of file entries (all at this level).
/// `entries` is a map from file name to (mode, oid) for immediate children.
fn build_tree_object(
    entries: &BTreeMap<String, (u32, ObjectId)>,
) -> Result<Vec<u8>> {
    let mut tree = Vec::new();
    for (name, (mode, oid)) in entries {
        // Git tree format: "<octal-mode> <filename>\0<20-byte-oid>"
        write!(tree, "{:o} {}\0", mode, name)?;
        tree.extend_from_slice(oid.as_bytes());
    }
    Ok(tree)
}

/// Given a set of full paths with their blob data, construct tree + blob objects.
/// Returns (root_tree_oid, all_objects).
fn build_trees(
    files: &HashMap<PathBuf, Vec<u8>>,
) -> Result<(ObjectId, ObjectList)> {
    // Collect all objects (blobs and trees).
    // Strategy: for each file, split into path components, then build trees
    // bottom-up.

    // First, create all blob objects and record their oids.
    let mut blob_oids: HashMap<PathBuf, ObjectId> = HashMap::new();
    let mut all_objects: Vec<(u8, Vec<u8>)> = Vec::new();

    for (path, data) in files {
        let oid = hash_object(OBJ_BLOB, data);
        blob_oids.insert(path.clone(), oid);
        all_objects.push((OBJ_BLOB, data.clone()));
    }

    // Group files by directory level and build trees bottom-up.
    // Collect all directories referenced by file paths.
    let mut dir_files: HashMap<PathBuf, BTreeMap<String, (u32, ObjectId)>> = HashMap::new();

    for (path, oid) in &blob_oids {
        let parent = path.parent().unwrap_or(Path::new(""));
        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        dir_files
            .entry(parent.to_path_buf())
            .or_default()
            .insert(filename, (0o100644, *oid));
    }

    // Collect subdirectory entries for each directory.
    // Process paths shortest first (root first? actually we need deepest first).
    let all_dirs: Vec<PathBuf> = dir_files.keys().cloned().collect();
    // Sort by depth (deepest first) so we build subtrees before their parents.
    let mut dirs_by_depth: Vec<PathBuf> = all_dirs;
    dirs_by_depth.sort_by_key(|b| std::cmp::Reverse(b.components().count()));

    let mut tree_oids: HashMap<PathBuf, ObjectId> = HashMap::new();

    for dir in &dirs_by_depth {
        let mut entries = dir_files.remove(dir).unwrap_or_default();

        // Add subdirectory entries
        for (subdir, sub_oid) in &tree_oids {
            if let Ok(rel) = subdir.strip_prefix(dir)
                && rel.components().count() == 1
            {
                let name = rel.file_name().unwrap().to_string_lossy().to_string();
                entries.insert(name, (0o40000, *sub_oid));
            }
        }

        let tree_bytes = build_tree_object(&entries)?;
        let oid = hash_object(OBJ_TREE, &tree_bytes);
        tree_oids.insert(dir.clone(), oid);
        all_objects.push((OBJ_TREE, tree_bytes));
    }

    let root_oid = tree_oids
        .get(Path::new(""))
        .or_else(|| tree_oids.get(Path::new(".")))
        .or_else(|| {
            // If no root entry, check if there's just one tree that IS the root
            tree_oids.values().last()
        })
        .copied()
        .context("no root tree generated")?;

    Ok((root_oid, all_objects))
}

// ---------------------------------------------------------------------------
// Git commit object creation
// ---------------------------------------------------------------------------

fn build_commit(
    tree_oid: &ObjectId,
    parent_oid: Option<&ObjectId>,
    author_name: &str,
    author_email: &str,
    message: &str,
) -> Vec<u8> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let tz = "+0000"; // UTC

    let mut commit = Vec::new();
    writeln!(commit, "tree {}", tree_oid).unwrap();
    if let Some(parent) = parent_oid {
        writeln!(commit, "parent {}", parent).unwrap();
    }
    writeln!(commit, "author {} <{}> {} {}", author_name, author_email, timestamp, tz).unwrap();
    writeln!(commit, "committer {} <{}> {} {}", author_name, author_email, timestamp, tz).unwrap();
    writeln!(commit).unwrap();
    writeln!(commit, "{}", message).unwrap();

    commit
}

// ---------------------------------------------------------------------------
// Ref advertisement reader
// ---------------------------------------------------------------------------

struct RefAdvertisement {
    refs: HashMap<String, ObjectId>,
    capabilities: String,
}

fn read_refs(reader: &mut impl BufRead) -> Result<RefAdvertisement> {
    let mut refs = HashMap::new();
    let mut capabilities = String::new();

    loop {
        let line = read_pkt_line(reader)?;
        match line {
            None => break, // flush
            Some(data) => {
                let line_str = std::str::from_utf8(&data)?;
                // Format: "<hex-oid> <refname>\0<capabilities>" or "<hex-oid> <refname>"
                let hex_len = 40; // SHA1 hex
                if line_str.len() < hex_len + 1 {
                    continue;
                }
                let hex = &line_str[..hex_len];
                let rest = &line_str[hex_len + 1..]; // skip space

                let oid_bytes = hex::decode(hex)?;
                let oid = ObjectId::from(<[u8; 20]>::try_from(&oid_bytes[..]).unwrap());

                if let Some(null_pos) = rest.find('\0') {
                    // First ref carries capabilities
                    let refname = &rest[..null_pos];
                    capabilities = rest[null_pos + 1..].to_string();
                    refs.insert(refname.to_owned(), oid);
                } else {
                    let refname = rest.trim_end_matches('\n');
                    refs.insert(refname.to_owned(), oid);
                }
            }
        }
    }

    Ok(RefAdvertisement { refs, capabilities })
}

// ---------------------------------------------------------------------------
// Fetch protocol
// ---------------------------------------------------------------------------

struct FetchResult {
    head_commit_oid: ObjectId,
    objects: HashMap<ObjectId, Vec<u8>>,
}

fn do_fetch(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
) -> Result<FetchResult> {
    for attempt in 1..=MAX_RETRIES {
        match try_fetch(remote_url, branch, ssh_key, password) {
            Ok(result) => return Ok(result),
            Err(e) if attempt < MAX_RETRIES => {
                tracing::warn!("fetch attempt {} failed (will retry): {}", attempt, e);
            }
            Err(e) => {
                tracing::warn!("fetch failed after {} attempts: {}", MAX_RETRIES, e);
                return Err(e);
            }
        }
    }
    // unreachable
    bail!("fetch failed");
}

fn try_fetch(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
) -> Result<FetchResult> {
    let (mut child, mut stdout) =
        spawn_ssh(remote_url, ssh_key, password, "git-upload-pack")?;
    let mut stdin = child.stdin.take().context("no stdin from ssh")?;

    // Read ref advertisement
    let adv = read_refs(&mut stdout)?;

    let head_ref = format!("refs/heads/{}", branch);
    let head_oid = adv
        .refs
        .get(&head_ref)
        .copied()
        .with_context(|| format!("branch '{}' not found on remote", branch))?;

    // Send want with selected capabilities (exclude side-band for raw packfile)
    // NOTE: thin-pack is intentionally omitted – we do not handle deltas
    // whose base is outside the received packfile.
    let supported_caps = [
        "multi_ack", "multi_ack_detailed", "no-progress", "ofs-delta",
    ];
    tracing::debug!(
        "server capabilities: {:?}, our caps: {:?}",
        adv.capabilities,
        supported_caps
    );
    let want_caps = adv.capabilities
        .split_ascii_whitespace()
        .filter(|c| supported_caps.contains(c))
        .collect::<Vec<_>>()
        .join(" ");
    let want_line = if want_caps.is_empty() {
        tracing::warn!("no supported capabilities from server, sending bare want");
        format!("want {}\n", head_oid)
    } else {
        format!("want {} {}\n", head_oid, want_caps)
    };
    write_pkt_line(&mut stdin, want_line.as_bytes())?;
    write_flush(&mut stdin)?;
    write_pkt_line(&mut stdin, b"done\n")?;
    stdin.flush()?;

    // Close stdin to signal we're done sending
    drop(stdin);

    // Read all remaining data: may start with NAK/ACK pkt-line then raw packfile
    let mut raw = Vec::new();
    stdout.read_to_end(&mut raw)?;

    // Skip leading pkt-line(s) that are NAK/ACK (4 hex len + data)
    let mut offset = 0;
    while offset + 8 <= raw.len() {
        let len_str = std::str::from_utf8(&raw[offset..offset + 4]).unwrap_or("xxxx");
        if let Ok(len) = usize::from_str_radix(len_str, 16) && len >= 4 {
            let content = &raw[offset + 4..offset + len];
            if content.starts_with(b"NAK") || content.starts_with(b"ACK") {
                offset += len;
                continue;
            }
        }
        break;
    }
    let pack_data = &raw[offset..];

    child.wait()?;

    let objects = parse_packfile(pack_data)?;

    Ok(FetchResult { head_commit_oid: head_oid, objects })
}

// ---------------------------------------------------------------------------
// Push protocol
// ---------------------------------------------------------------------------

fn do_push(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
    new_commit_oid: ObjectId,
    packfile: &[u8],
) -> Result<bool> {
    for attempt in 1..=MAX_RETRIES {
        match try_push(remote_url, branch, ssh_key, password, new_commit_oid, packfile) {
            Ok(true) => return Ok(true),
            Ok(false) => {
                tracing::warn!("push attempt {} returned non-fast-forward", attempt);
                return Ok(false);
            }
            Err(e) if attempt < MAX_RETRIES => {
                tracing::warn!("push attempt {} failed (will retry): {}", attempt, e);
            }
            Err(e) => {
                tracing::warn!("push failed after {} attempts: {}", MAX_RETRIES, e);
                return Err(e);
            }
        }
    }
    bail!("push failed after {} attempts", MAX_RETRIES);
}

fn try_push(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
    new_commit_oid: ObjectId,
    packfile: &[u8],
) -> Result<bool> {
    let (mut child, mut stdout) =
        spawn_ssh(remote_url, ssh_key, password, "git-receive-pack")?;
    let mut stdin = child.stdin.take().context("no stdin from ssh")?;

    // Read ref advertisement
    let adv = read_refs(&mut stdout)?;

    // Find old oid for our branch
    let head_ref = format!("refs/heads/{}", branch);
    let old_oid = adv
        .refs
        .get(&head_ref)
        .copied()
        .unwrap_or(ObjectId::from([0u8; 20])); // zero oid for new branch

    // Pick push capabilities we support
    let push_caps = ["report-status", "side-band-64k", "quiet", "agent"];
    let caps_str = adv.capabilities
        .split_ascii_whitespace()
        .filter(|c| {
            let name = c.split('=').next().unwrap_or(c);
            push_caps.contains(&name)
        })
        .collect::<Vec<_>>()
        .join(" ");

    // Send update command
    let cmd_line = format!(
        "{} {} {}\0{}\n",
        old_oid, new_commit_oid, head_ref, caps_str
    );
    write_pkt_line(&mut stdin, cmd_line.as_bytes())?;
    write_flush(&mut stdin)?;

    // Send packfile
    stdin.write_all(packfile)?;
    stdin.flush()?;

    // Close stdin
    drop(stdin);

    // Read report-status (pkt-line encoded)
    let mut report = String::new();
    while let Some(data) = read_pkt_line(&mut stdout)? {
        let line = String::from_utf8_lossy(&data);
        report.push_str(&line);
    }

    child.wait()?;

    // Parse report
    if report.contains("unpack ok") && report.contains("ok") {
        Ok(true)
    } else if report.contains("non-fast-forward")
        || report.contains("fetch first")
        || report.contains("NG")
    {
        let detail = report.lines().find(|l| l.contains("NG"))
            .or_else(|| report.lines().find(|l| l.contains("non-fast")))
            .or_else(|| report.lines().find(|l| l.contains("fetch first")))
            .unwrap_or(&report);
        tracing::warn!("push rejected by server: {}", detail.trim());
        Ok(false)
    } else {
        bail!("push rejected: {}", report.trim());
    }
}

/// Returns (ObjectId of new root tree, list of all objects for the packfile).
fn build_change_commit(
    files: &HashMap<PathBuf, Vec<u8>>,
    parent_oid: Option<ObjectId>,
    author_name: &str,
    author_email: &str,
    message: &str,
) -> Result<(ObjectId, ObjectList)> {
    let (tree_oid, mut objects) = build_trees(files)?;
    let commit_data = build_commit(&tree_oid, parent_oid.as_ref(), author_name, author_email, message);
    let commit_oid = hash_object(OBJ_COMMIT, &commit_data);
    objects.push((OBJ_COMMIT, commit_data));
    Ok((commit_oid, objects))
}

// ---------------------------------------------------------------------------
// GitRepo – public API
// ---------------------------------------------------------------------------

pub struct GitRepo {
    remote_url: String,
    branch: String,
    author_name: String,
    author_email: String,
    ssh_key: Option<String>,
    password: Option<String>,
    head_oid: Mutex<Option<ObjectId>>,
    tree: Mutex<HashMap<PathBuf, Vec<u8>>>,
    last_fetch: Mutex<Instant>,
}

impl GitRepo {
    /// Initialize, fetch from remote, and build in-memory file tree.
    pub fn init_and_fetch(
        remote_url: &str,
        branch: &str,
        ssh_key: Option<&str>,
        password: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<Self> {
        let author_name = if author_name.is_empty() {
            "davgit".to_string()
        } else {
            author_name.to_owned()
        };
        let author_email = if author_email.is_empty() {
            "davgit@localhost".to_string()
        } else {
            author_email.to_owned()
        };

        let repo = GitRepo {
            remote_url: remote_url.to_owned(),
            branch: branch.to_owned(),
            author_name,
            author_email,
            ssh_key: ssh_key.map(|s| s.to_owned()),
            password: password.map(|s| s.to_owned()),
            head_oid: Mutex::new(None),
            tree: Mutex::new(HashMap::new()),
            last_fetch: Mutex::new(Instant::now()),
        };

        // Initial fetch — gracefully handle empty repos
        match do_fetch(remote_url, branch, ssh_key, password) {
            Ok(result) => {
                let tree = match parse_fetch_to_map(&result) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("parse_fetch_to_map failed: {}", e);
                        HashMap::new()
                    }
                };
                *repo.head_oid.lock().unwrap() = Some(result.head_commit_oid);
                *repo.tree.lock().unwrap() = tree;
            }
            Err(e) => {
                tracing::warn!("initial fetch failed (starting empty): {}", e);
            }
        }
        *repo.last_fetch.lock().unwrap() = Instant::now();

        Ok(repo)
    }

    /// Re-fetch from remote and return updated tree if changed.
    /// Returns `None` if throttled or remote hasn't changed.
    pub fn refresh_tree(&self) -> Result<Option<HashMap<PathBuf, Vec<u8>>>> {
        // Throttle check
        {
            let last = self.last_fetch.lock().unwrap();
            if last.elapsed() < FETCH_THROTTLE {
                return Ok(None);
            }
        }

        // Attempt fetch (non-fatal on failure)
        let result = match do_fetch(&self.remote_url, &self.branch,
                                   self.ssh_key.as_deref(), self.password.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("refresh fetch failed: {}", e);
                *self.last_fetch.lock().unwrap() = Instant::now();
                return Ok(None);
            }
        };

        let new_tree = parse_fetch_to_map(&result)?;

        // Check if head changed
        {
            let head = self.head_oid.lock().unwrap();
            if *head == Some(result.head_commit_oid) {
                *self.last_fetch.lock().unwrap() = Instant::now();
                return Ok(None);
            }
        }

        *self.head_oid.lock().unwrap() = Some(result.head_commit_oid);
        *self.tree.lock().unwrap() = new_tree.clone();
        *self.last_fetch.lock().unwrap() = Instant::now();
        Ok(Some(new_tree))
    }

    /// Returns `Some(tree_oid)` if we have a cached tree, `None` if empty.
    pub fn resolve_head_tree(&self) -> Result<Option<ObjectId>> {
        let tree = self.tree.lock().unwrap();
        // Return a dummy oid if we have files, else None
        if tree.is_empty() {
            Ok(None)
        } else {
            // We don't actually store the tree oid separately for empty check.
            // Just return any non-null oid to signal "has content".
            Ok(Some(ObjectId::from([1u8; 20])))
        }
    }

    /// Read the cached tree into a HashMap (ignores the provided tree_id).
    pub fn read_tree_to_memory(&self, _tree_id: ObjectId) -> Result<HashMap<PathBuf, Vec<u8>>> {
        let tree = self.tree.lock().unwrap();
        Ok(tree.clone())
    }

    /// Write a single file, commit, and push.
    pub fn write_path(&self, path: &Path, data: &[u8]) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let msg = format!("update {}", path_str);

        for _ in 0..MAX_RETRIES {
            if self.commit_and_push(
                &[(path.to_path_buf(), data.to_vec())],
                &[],
                &msg,
            )? {
                return Ok(());
            }
        }
        bail!("write failed after {} attempts", MAX_RETRIES);
    }

    /// Delete a single file, commit, and push.
    pub fn delete_path(&self, path: &Path) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let msg = format!("delete {}", path_str);

        for _ in 0..MAX_RETRIES {
            if self.commit_and_push(&[], &[path.to_path_buf()], &msg)? {
                return Ok(());
            }
        }
        bail!("delete failed after {} attempts", MAX_RETRIES);
    }

    /// Apply multiple writes and deletes in a single commit+push cycle.
    pub fn batch_paths(
        &self,
        writes: &[(PathBuf, Vec<u8>)],
        deletes: &[PathBuf],
    ) -> Result<()> {
        for _ in 0..MAX_RETRIES {
            if self.commit_and_push(writes, deletes, "batch update")? {
                return Ok(());
            }
        }
        bail!("batch commit failed after {} attempts", MAX_RETRIES);
    }

    // -----------------------------------------------------------------------
    // Internal: fetch → build tree → commit → push (with non-fast-forward retry)
    // -----------------------------------------------------------------------

    fn commit_and_push(
        &self,
        writes: &[(PathBuf, Vec<u8>)],
        deletes: &[PathBuf],
        message: &str,
    ) -> Result<bool> {
        // 1. Fetch latest from remote
        let result = do_fetch(
            &self.remote_url,
            &self.branch,
            self.ssh_key.as_deref(),
            self.password.as_deref(),
        )?;
        let parent_oid = Some(result.head_commit_oid);
        let mut files = parse_fetch_to_map(&result)?;

        // 2. Apply changes to in-memory file map
        for (path, data) in writes {
            files.insert(path.clone(), data.clone());
        }
        for path in deletes {
            files.remove(path);
        }

        // 3. Build tree + commit objects
        let (commit_oid, objects) = build_change_commit(
            &files,
            parent_oid,
            &self.author_name,
            &self.author_email,
            message,
        )?;

        // 4. Build packfile
        let entries: Vec<(u8, &[u8])> = objects.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        let packfile = build_packfile(&entries)?;

        // 5. Push
        // Drop the tree lock before push to avoid deadlock
        let pushed = do_push(
            &self.remote_url,
            &self.branch,
            self.ssh_key.as_deref(),
            self.password.as_deref(),
            commit_oid,
            &packfile,
        )?;

        if pushed {
            // Update cached state
            *self.head_oid.lock().unwrap() = Some(commit_oid);
            *self.tree.lock().unwrap() = files;
            *self.last_fetch.lock().unwrap() = Instant::now();
        }

        Ok(pushed)
    }
}

// ---------------------------------------------------------------------------
// Helpers to convert fetch result into file map
// ---------------------------------------------------------------------------

fn parse_fetch_to_map(result: &FetchResult) -> Result<HashMap<PathBuf, Vec<u8>>> {
    // Parse HEAD commit to get tree OID
    let commit_data = result
        .objects
        .get(&result.head_commit_oid)
        .context("HEAD commit not found in fetched packfile")?;

    let commit = gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1)?;
    let tree_oid: ObjectId = commit.tree().to_owned();

    let mut map = HashMap::new();
    walk_tree(&result.objects, &tree_oid, Path::new(""), &mut map)?;
    Ok(map)
}
