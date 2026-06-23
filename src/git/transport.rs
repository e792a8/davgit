use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::git::packfile::ObjectId;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
}

pub struct RefAdvertisement {
    pub refs: HashMap<String, ObjectId>,
    pub capabilities: String,
}

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

pub fn parse_ssh_url(url: &str) -> Result<SshTarget> {
    if let Some(rest) = url.strip_prefix("ssh://") {
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

pub fn build_ssh_cmd(
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

pub fn spawn_ssh(
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

pub fn read_pkt_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len_str = std::str::from_utf8(&len_buf)?;
    let len = usize::from_str_radix(len_str, 16).context("invalid pkt-line length")?;
    if len == 0 {
        return Ok(None);
    }
    if len == 1 {
        return Ok(None);
    }
    let mut data = vec![0u8; len - 4];
    reader.read_exact(&mut data)?;
    Ok(Some(data))
}

pub fn write_pkt_line(writer: &mut impl Write, data: &[u8]) -> Result<()> {
    let len = data.len() + 4;
    if len > 0xffff {
        bail!("pkt-line too long: {} bytes", len);
    }
    write!(writer, "{:04x}", len)?;
    writer.write_all(data)?;
    Ok(())
}

pub fn write_flush(writer: &mut impl Write) -> Result<()> {
    writer.write_all(b"0000")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ref advertisement reader
// ---------------------------------------------------------------------------

pub fn read_refs(reader: &mut impl BufRead) -> Result<RefAdvertisement> {
    let mut refs = HashMap::new();
    let mut capabilities = String::new();

    loop {
        let line = read_pkt_line(reader)?;
        match line {
            None => break,
            Some(data) => {
                let line_str = std::str::from_utf8(&data)?;
                let hex_len = 40;
                if line_str.len() < hex_len + 1 {
                    continue;
                }
                let hex = &line_str[..hex_len];
                let rest = &line_str[hex_len + 1..];

                let oid_bytes = hex::decode(hex)?;
                let oid = ObjectId::from(<[u8; 20]>::try_from(&oid_bytes[..]).unwrap());

                if let Some(null_pos) = rest.find('\0') {
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

pub struct FetchResult {
    pub head_commit_oid: ObjectId,
    pub packfile: Vec<u8>,
}

const MAX_RETRIES: u32 = 3;

pub fn do_fetch(
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
    unreachable!()
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

    let adv = read_refs(&mut stdout)?;

    let head_ref = format!("refs/heads/{}", branch);
    let head_oid = adv
        .refs
        .get(&head_ref)
        .copied()
        .with_context(|| format!("branch '{}' not found on remote", branch))?;

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

    drop(stdin);

    let mut raw = Vec::new();
    stdout.read_to_end(&mut raw)?;

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
    let packfile = raw[offset..].to_vec();

    child.wait()?;

    Ok(FetchResult { head_commit_oid: head_oid, packfile })
}

// ---------------------------------------------------------------------------
// Push protocol
// ---------------------------------------------------------------------------

pub fn do_push(
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

    let adv = read_refs(&mut stdout)?;

    let head_ref = format!("refs/heads/{}", branch);
    let old_oid = adv
        .refs
        .get(&head_ref)
        .copied()
        .unwrap_or(ObjectId::from([0u8; 20]));

    let push_caps = ["report-status", "side-band-64k", "quiet", "agent"];
    let caps_str = adv.capabilities
        .split_ascii_whitespace()
        .filter(|c| {
            let name = c.split('=').next().unwrap_or(c);
            push_caps.contains(&name)
        })
        .collect::<Vec<_>>()
        .join(" ");

    let cmd_line = format!(
        "{} {} {}\0{}\n",
        old_oid, new_commit_oid, head_ref, caps_str
    );
    write_pkt_line(&mut stdin, cmd_line.as_bytes())?;
    write_flush(&mut stdin)?;

    stdin.write_all(packfile)?;
    stdin.flush()?;

    drop(stdin);

    let mut report = String::new();
    while let Some(data) = read_pkt_line(&mut stdout)? {
        let line = String::from_utf8_lossy(&data);
        report.push_str(&line);
    }

    child.wait()?;

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
