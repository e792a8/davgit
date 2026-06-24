use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::client;
use russh::keys::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

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

pub struct FetchResult {
    pub head_commit_oid: ObjectId,
    pub packfile: Vec<u8>,
}

// ---------------------------------------------------------------------------
// URL parsing (unchanged)
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
// SSH client handler
// ---------------------------------------------------------------------------

struct SshHandler;

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

async fn connect_ssh(
    target: &SshTarget,
    ssh_key: Option<&str>,
    _password: Option<&str>,
) -> Result<client::Handle<SshHandler>> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let addr = (target.host.as_str(), target.port.unwrap_or(22));
    let mut session = client::connect(config, addr, SshHandler).await?;

    let user = target.user.clone().unwrap_or_else(|| "git".to_string());

    if let Some(key_path) = ssh_key {
        let key_pair = load_secret_key(key_path, None)?;
        let hash = session.best_supported_rsa_hash().await?.flatten();
        let auth_res = session
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash))
            .await?;
        if !auth_res.success() {
            bail!("public key authentication failed");
        }
    } else {
        // Try SSH agent first, then default key files
        if !try_agent_auth(&mut session, &user).await
            && !try_default_keys(&mut session, &user).await
        {
            bail!("authentication failed (no key provided, ssh-agent unavailable, and no default key worked)");
        }
    }

    Ok(session)
}

async fn try_agent_auth(
    session: &mut client::Handle<SshHandler>,
    user: &str,
) -> bool {
    let mut agent = match agent::client::AgentClient::connect_env().await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!("agent connect_env failed: {}", e);
            return false;
        }
    };
    let identities = match agent.request_identities().await {
        Ok(i) => i,
        Err(e) => {
            tracing::debug!("agent list identities failed: {}", e);
            return false;
        }
    };
    for identity in &identities {
        let pubkey = identity.public_key().into_owned();
        match session
            .authenticate_publickey_with(user.to_owned(), pubkey, None, &mut agent)
            .await
        {
            Ok(r) if r.success() => return true,
            Ok(_) => {}
            Err(e) => tracing::debug!("agent key auth failed: {e}"),
        }
    }
    false
}

async fn try_default_keys(
    session: &mut client::Handle<SshHandler>,
    user: &str,
) -> bool {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return false,
    };
    let default_paths = [
        "id_ed25519",
        "id_rsa",
        "id_ecdsa",
        "id_ecdsa_sk",
        "id_ed25519_sk",
        "id_dsa",
    ];
    for filename in &default_paths {
        let path = std::path::Path::new(&home).join(".ssh").join(filename);
        if !path.exists() {
            continue;
        }
        match load_secret_key(&path, None) {
            Ok(key_pair) => {
                let hash = match session.best_supported_rsa_hash().await {
                    Ok(h) => h.flatten(),
                    Err(_) => None,
                };
                match session
                    .authenticate_publickey(
                        user.to_owned(),
                        PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash),
                    )
                    .await
                {
                    Ok(r) if r.success() => {
                        tracing::debug!("authenticated with default key {:?}", path);
                        return true;
                    }
                    _ => continue,
                }
            }
            Err(e) => {
                tracing::debug!("failed to load key {:?}: {}", path, e);
                continue;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// pkt-line async helpers
// ---------------------------------------------------------------------------

async fn read_pkt_line(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    if let Err(e) = reader.read_exact(&mut len_buf).await {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(e.into());
    }
    let len_str = std::str::from_utf8(&len_buf)?;
    let len = usize::from_str_radix(len_str, 16).context("invalid pkt-line length")?;
    if len <= 1 {
        return Ok(None);
    }
    let mut data = vec![0u8; len - 4];
    reader.read_exact(&mut data).await?;
    Ok(Some(data))
}

async fn write_pkt_line(writer: &mut (impl AsyncWriteExt + Unpin), data: &[u8]) -> Result<()> {
    let len = data.len() + 4;
    if len > 0xffff {
        bail!("pkt-line too long: {} bytes", len);
    }
    let header = format!("{:04x}", len);
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(data).await?;
    Ok(())
}

async fn write_flush(writer: &mut (impl AsyncWriteExt + Unpin)) -> Result<()> {
    writer.write_all(b"0000").await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ref advertisement reader
// ---------------------------------------------------------------------------

async fn read_refs(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<RefAdvertisement> {
    let mut refs = HashMap::new();
    let mut capabilities = String::new();

    loop {
        let line = read_pkt_line(reader).await?;
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

const MAX_RETRIES: u32 = 3;

pub async fn do_fetch(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
) -> Result<FetchResult> {
    for attempt in 1..=MAX_RETRIES {
        match try_fetch(remote_url, branch, ssh_key, password).await {
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

async fn try_fetch(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
) -> Result<FetchResult> {
    let target = parse_ssh_url(remote_url)?;
    let session = connect_ssh(&target, ssh_key, password).await?;

    let channel = session.channel_open_session().await?;
    channel
        .exec(true, format!("git-upload-pack '{}'", target.path))
        .await?;
    let stream = channel.into_stream();
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    let adv = read_refs(&mut reader).await?;

    let head_ref = format!("refs/heads/{}", branch);
    let head_oid = adv
        .refs
        .get(&head_ref)
        .copied()
        .with_context(|| format!("branch '{}' not found on remote", branch))?;

    let supported_caps = [
        "multi_ack",
        "multi_ack_detailed",
        "no-progress",
        "ofs-delta",
    ];

    let caps_str = adv
        .capabilities
        .split_ascii_whitespace()
        .filter(|c| supported_caps.contains(c))
        .collect::<Vec<_>>()
        .join(" ");

    let want_line = if caps_str.is_empty() {
        format!("want {}\n", head_oid)
    } else {
        format!("want {} {}\n", head_oid, caps_str)
    };

    write_pkt_line(&mut writer, want_line.as_bytes()).await?;
    write_flush(&mut writer).await?;
    write_pkt_line(&mut writer, b"done\n").await?;
    writer.flush().await?;
    writer.shutdown().await?;
    drop(writer);

    let mut raw = Vec::new();
    reader.read_to_end(&mut raw).await?;
    drop(reader);

    // Skip ACK/NAK pkt-lines before the packfile
    let mut offset = 0;
    while offset + 8 <= raw.len() {
        let len_str = std::str::from_utf8(&raw[offset..offset + 4]).unwrap_or("xxxx");
        if let Ok(len) = usize::from_str_radix(len_str, 16) {
            if len >= 4 && offset + len <= raw.len() {
                let content = &raw[offset + 4..offset + len];
                if content.starts_with(b"NAK") || content.starts_with(b"ACK") {
                    offset += len;
                    continue;
                }
            }
        }
        break;
    }
    let packfile = raw[offset..].to_vec();

    Ok(FetchResult {
        head_commit_oid: head_oid,
        packfile,
    })
}

// ---------------------------------------------------------------------------
// Push protocol
// ---------------------------------------------------------------------------

pub async fn do_push(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
    new_commit_oid: ObjectId,
    packfile: &[u8],
) -> Result<bool> {
    for attempt in 1..=MAX_RETRIES {
        match try_push(remote_url, branch, ssh_key, password, new_commit_oid, packfile).await {
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

async fn try_push(
    remote_url: &str,
    branch: &str,
    ssh_key: Option<&str>,
    password: Option<&str>,
    new_commit_oid: ObjectId,
    packfile: &[u8],
) -> Result<bool> {
    let target = parse_ssh_url(remote_url)?;
    let session = connect_ssh(&target, ssh_key, password).await?;

    let channel = session.channel_open_session().await?;
    channel
        .exec(true, format!("git-receive-pack '{}'", target.path))
        .await?;
    let stream = channel.into_stream();
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    let adv = read_refs(&mut reader).await?;

    let head_ref = format!("refs/heads/{}", branch);
    let old_oid = adv
        .refs
        .get(&head_ref)
        .copied()
        .unwrap_or(ObjectId::from([0u8; 20]));

    let push_caps = ["report-status", "side-band-64k", "quiet", "agent"];
    let caps_str = adv
        .capabilities
        .split_ascii_whitespace()
        .filter(|c| {
            let name = c.split('=').next().unwrap_or(c);
            push_caps.contains(&name)
        })
        .collect::<Vec<_>>()
        .join(" ");

    let cmd_line = format!(
        "{old} {new} {ref}\0{caps}\n",
        old = old_oid,
        new = new_commit_oid,
        ref = head_ref,
        caps = caps_str,
    );

    write_pkt_line(&mut writer, cmd_line.as_bytes()).await?;
    write_flush(&mut writer).await?;
    writer.write_all(packfile).await?;
    writer.flush().await?;
    writer.shutdown().await?;
    drop(writer);

    let mut report = String::new();
    while let Some(data) = read_pkt_line(&mut reader).await? {
        report.push_str(&String::from_utf8_lossy(&data));
    }
    drop(reader);

    if report.contains("unpack ok") && report.contains("ok") {
        Ok(true)
    } else if report.contains("non-fast-forward")
        || report.contains("fetch first")
        || report.contains("NG")
    {
        let detail = report
            .lines()
            .find(|l| l.contains("NG"))
            .or_else(|| report.lines().find(|l| l.contains("non-fast")))
            .or_else(|| report.lines().find(|l| l.contains("fetch first")))
            .unwrap_or(&report);
        tracing::warn!("push rejected by server: {}", detail.trim());
        Ok(false)
    } else {
        bail!("push rejected: {}", report.trim());
    }
}
