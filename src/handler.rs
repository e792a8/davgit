use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use hyper::{Request, Response};

use crate::git::GitRepo;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn handle_request(req: Request<Incoming>, git: Arc<GitRepo>) -> Response<Full<Bytes>> {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let span = tracing::info_span!("request", method = %method, path = %path);
    let _guard = span.enter();

    tracing::info!("← {} {}", method, path);

    let dest = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(ref d) = dest {
        tracing::debug!("  Destination: {}", d);
    }
    let depth = req
        .headers()
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(ref d) = depth {
        tracing::debug!("  Depth: {}", d);
    }
    let overwrite = req
        .headers()
        .get("overwrite")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(ref o) = overwrite {
        tracing::debug!("  Overwrite: {}", o);
    }
    let content_len = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(ref cl) = content_len {
        tracing::debug!("  Content-Length: {}", cl);
    }

    let rel_path = davpath_to_rel(req.uri().path());

    let resp = match req.method().as_str() {
        "GET" => handle_get(&rel_path, &req, &git).await,
        "HEAD" => handle_head(&rel_path, &req, &git).await,
        "PUT" => handle_put(&rel_path, req, &git).await,
        "DELETE" => handle_delete(&rel_path, &git).await,
        "MKCOL" => handle_mkcol(&rel_path, req, &git).await,
        "OPTIONS" => handle_options(),
        "PROPFIND" => handle_propfind(&rel_path, &req, &git).await,
        "PROPPATCH" => handle_proppatch(&rel_path, req, &git).await,
        "MOVE" => handle_copy_move(&rel_path, &req, &git, true).await,
        "COPY" => handle_copy_move(&rel_path, &req, &git, false).await,
        "LOCK" => handle_lock(&rel_path, req, &git).await,
        "UNLOCK" => handle_unlock(&rel_path, &git),
        _ => method_not_allowed(),
    };

    let status = resp.status().as_u16();
    tracing::info!(
        "→ {} {} ({} bytes)",
        status,
        method,
        resp.body().size_hint().lower()
    );
    resp
}

// ---------------------------------------------------------------------------
// Handler functions
// ---------------------------------------------------------------------------

fn is_dav_path(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(".DAV")
}

async fn handle_get(path: &Path, req: &Request<Incoming>, git: &GitRepo) -> Response<Full<Bytes>> {
    if is_dav_path(path) || git.is_directory(path) {
        return not_found();
    }
    match git.read_file(path) {
        Ok(Some(data)) => {
            let etag = compute_etag(&data);
            let ct = content_type(path);

            if let Some(val) = req.headers().get("if-none-match")
                && val.to_str().ok() == Some(&etag)
            {
                return not_modified();
            }

            Response::builder()
                .status(200)
                .header("Content-Type", ct)
                .header("Content-Length", data.len().to_string())
                .header("ETag", &etag)
                .header("Accept-Ranges", "bytes")
                .body(Full::from(data))
                .unwrap()
        }
        Ok(None) => not_found(),
        Err(e) => {
            tracing::error!("read_file({:?}) failed: {}", path, e);
            internal_error()
        }
    }
}

async fn handle_head(path: &Path, req: &Request<Incoming>, git: &GitRepo) -> Response<Full<Bytes>> {
    let resp = handle_get(path, req, git).await;
    Response::builder()
        .status(resp.status())
        .body(Full::default())
        .unwrap()
}

async fn handle_put(path: &Path, req: Request<Incoming>, git: &GitRepo) -> Response<Full<Bytes>> {
    if is_dav_path(path) {
        return method_not_allowed();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let parent_exists = parent.as_os_str().is_empty() || git.is_directory(parent);
    let file_exists = git.file_size(path).is_some() || git.is_directory(path);
    tracing::debug!(
        "  PUT {:?}: parent={:?} parent_exists={} file_exists={}",
        path,
        parent,
        parent_exists,
        file_exists,
    );

    if !parent_exists {
        tracing::warn!("  PUT parent not found: {:?}", parent);
        return conflict();
    }

    // Handle If-None-Match: * — create only if not exists
    if let Some(val) = req.headers().get("if-none-match")
        && val.to_str().ok() == Some("*")
    {
        if file_exists {
            tracing::debug!("  PUT If-None-Match: * → 412 (file exists)");
            return precondition_failed();
        }
        tracing::debug!("  PUT If-None-Match: * → proceeding (file does not exist)");
    }

    // Handle If-Match: * — update only if exists
    if let Some(val) = req.headers().get("if-match")
        && val.to_str().ok() == Some("*")
    {
        if !file_exists {
            tracing::debug!("  PUT If-Match: * → 412 (file does not exist)");
            return precondition_failed();
        }
        tracing::debug!("  PUT If-Match: * → proceeding (file exists)");
    }

    let data = match read_body(req.into_body()).await {
        Ok(d) => {
            tracing::debug!("  PUT body read: {} bytes", d.len());
            d
        }
        Err(e) => {
            tracing::error!("read body failed: {}", e);
            return internal_error();
        }
    };

    // Windows Explorer sends 0-byte PUT as a "probe" before the real PUT with content.
    // Don't create git objects for empty PUT on new files — avoids expensive commit+push
    // and prevents the file from appearing before the real content arrives.
    if data.is_empty() && !file_exists {
        tracing::debug!("  PUT 0-byte probe on new file — acknowledging without write");
        return created();
    }

    match git.write_path(path, &data).await {
        Ok(_) => {
            tracing::debug!("  PUT success: {} bytes written to {:?}", data.len(), path);
            if data.is_empty() {
                no_content()
            } else {
                created()
            }
        }
        Err(e) => {
            tracing::error!("write_path({:?}) failed: {}", path, e);
            internal_error()
        }
    }
}

async fn handle_delete(path: &Path, git: &GitRepo) -> Response<Full<Bytes>> {
    if is_dav_path(path) {
        return method_not_allowed();
    }
    let exists = git.is_directory(path) || git.file_size(path).is_some();
    tracing::debug!("  DELETE {:?}: exists={}", path, exists);
    if !exists {
        return not_found();
    }

    if git.is_directory(path) {
        match git.delete_subtree(path).await {
            Ok(_) => no_content(),
            Err(e) => {
                tracing::error!("delete_subtree {:?} failed: {}", path, e);
                internal_error()
            }
        }
    } else {
        match git.delete_path(path).await {
            Ok(_) => no_content(),
            Err(e) => {
                tracing::error!("delete_path {:?} failed: {}", path, e);
                internal_error()
            }
        }
    }
}

async fn handle_mkcol(path: &Path, req: Request<Incoming>, git: &GitRepo) -> Response<Full<Bytes>> {
    tracing::debug!(
        "  MKCOL {:?}: existing is_dir={} file_size={:?}",
        path,
        git.is_directory(path),
        git.file_size(path)
    );
    // Reject MKCOL with a body (RFC 4918 section 9.3.1: body must be ignored,
    // but litmus expects non-empty body to fail)
    let body_size = req.body().size_hint().exact();
    if body_size != Some(0) && body_size.is_some() {
        return unsupported_media_type();
    }

    if git.is_directory(path) || git.file_size(path).is_some() {
        return method_not_allowed();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    if !parent.as_os_str().is_empty() && !git.is_directory(parent) {
        return conflict();
    }
    // Drop the body explicitly
    drop(req);
    match git.create_dir(path).await {
        Ok(_) => created(),
        Err(e) => {
            tracing::error!("create_dir({:?}) failed: {}", path, e);
            internal_error()
        }
    }
}

fn handle_options() -> Response<Full<Bytes>> {
    Response::builder()
        .status(200)
        .header("DAV", "1, 2, 3")
        .header(
            "Allow",
            "GET, HEAD, PUT, DELETE, MKCOL, COPY, MOVE, PROPFIND, OPTIONS",
        )
        .header("MS-Author-Via", "DAV")
        .body(Full::default())
        .unwrap()
}

async fn handle_propfind(
    path: &Path,
    req: &Request<Incoming>,
    git: &GitRepo,
) -> Response<Full<Bytes>> {
    if is_dav_path(path) {
        return not_found();
    }
    let exists = git.is_directory(path) || git.file_size(path).is_some();
    tracing::debug!(
        "  PROPFIND {:?}: depth={:?}, is_dir={}, file_size={:?}, exists={}",
        path,
        parse_depth(req.headers()),
        git.is_directory(path),
        git.file_size(path),
        exists,
    );

    if !exists {
        tracing::debug!("  PROPFIND not found: {:?}", path);
        return not_found();
    }

    let depth = parse_depth(req.headers()).unwrap_or(1);

    let mut resources = Vec::new();
    let is_dir = git.is_directory(path);
    let len = if is_dir {
        0
    } else {
        git.file_size(path).unwrap_or(0)
    };
    resources.push(PropfindResource {
        path: path.to_path_buf(),
        is_dir,
        len,
    });

    if depth >= 1
        && is_dir
        && let Ok(entries) = git.list_dir(path)
    {
        tracing::debug!("  PROPFIND listing {:?}: {} entries", path, entries.len());
        for (name, entry_is_dir, entry_len) in &entries {
            tracing::debug!("    {:?}: is_dir={} len={}", name, entry_is_dir, entry_len);
        }
        for (name, entry_is_dir, entry_len) in entries {
            if name == ".DAV" {
                continue;
            }
            resources.push(PropfindResource {
                path: path.join(&name),
                is_dir: entry_is_dir,
                len: entry_len,
            });
        }
    }

    let xml = build_multistatus(&resources);

    Response::builder()
        .status(207)
        .header("Content-Type", "application/xml; charset=utf-8")
        .header("DAV", "1, 2, 3")
        .body(Full::from(xml.into_bytes()))
        .unwrap()
}

async fn handle_copy_move(
    path: &Path,
    req: &Request<Incoming>,
    git: &GitRepo,
    is_move: bool,
) -> Response<Full<Bytes>> {
    if is_dav_path(path) || is_dav_path(&parse_destination(req).unwrap_or_default()) {
        return not_found();
    }
    let dest_path = match parse_destination(req) {
        Some(p) => p,
        None => {
            tracing::warn!("  COPY/MOVE missing or invalid Destination header");
            return bad_request();
        }
    };

    tracing::debug!(
        "  {} {:?} → {:?}: src_is_dir={} src_size={:?}, dest_exists={:?}",
        if is_move { "MOVE" } else { "COPY" },
        path,
        dest_path,
        git.is_directory(path),
        git.file_size(path),
        if git.is_directory(&dest_path) {
            Some("dir")
        } else if git.file_size(&dest_path).is_some() {
            Some("file")
        } else {
            None
        }
    );

    if !git.is_directory(path) && git.file_size(path).is_none() {
        tracing::warn!(
            "  {} source not found: {:?}",
            if is_move { "MOVE" } else { "COPY" },
            path
        );
        return not_found();
    }

    // Destination parent must exist
    let dest_parent = dest_path.parent().unwrap_or(Path::new(""));
    if !dest_parent.as_os_str().is_empty() && !git.is_directory(dest_parent) {
        return conflict();
    }

    let overwrite = parse_overwrite(req.headers());

    let dest_exists = git.is_directory(&dest_path) || git.file_size(&dest_path).is_some();
    if dest_exists && !overwrite {
        return precondition_failed();
    }

    let result = if is_move {
        git.move_subtree(path, &dest_path).await
    } else {
        git.copy_subtree(path, &dest_path).await
    };

    match result {
        Ok(_) => {
            if dest_exists {
                no_content()
            } else {
                created()
            }
        }
        Err(e) => {
            tracing::error!("{} failed: {}", if is_move { "MOVE" } else { "COPY" }, e);
            internal_error()
        }
    }
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

struct PropfindResource {
    path: PathBuf,
    is_dir: bool,
    len: u64,
}

fn build_multistatus(resources: &[PropfindResource]) -> String {
    let mut xml = String::from(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    xml.push_str(r#"<D:multistatus xmlns:D="DAV:">"#);

    for r in resources {
        xml.push_str("<D:response>");
        xml.push_str("<D:href>");
        xml.push_str(&href_encode(&r.path));
        xml.push_str("</D:href>");
        xml.push_str("<D:propstat><D:prop>");

        if r.is_dir {
            xml.push_str("<D:getcontenttype>httpd/unix-directory</D:getcontenttype>");
            xml.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
        } else {
            xml.push_str(&format!(
                "<D:getcontentlength>{}</D:getcontentlength>",
                r.len
            ));
            xml.push_str(&format!(
                "<D:getcontenttype>{}</D:getcontenttype>",
                content_type(&r.path)
            ));
            xml.push_str("<D:resourcetype/>");
        }

        xml.push_str("<D:lockdiscovery/>");
        xml.push_str("<D:supportedlock/>");
        xml.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>");
        xml.push_str("</D:response>");
    }

    xml.push_str("</D:multistatus>");
    xml
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

fn created() -> Response<Full<Bytes>> {
    Response::builder()
        .status(201)
        .body(Full::default())
        .unwrap()
}

fn no_content() -> Response<Full<Bytes>> {
    Response::builder()
        .status(204)
        .body(Full::default())
        .unwrap()
}

fn not_modified() -> Response<Full<Bytes>> {
    Response::builder()
        .status(304)
        .body(Full::default())
        .unwrap()
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(404)
        .body(Full::default())
        .unwrap()
}

fn conflict() -> Response<Full<Bytes>> {
    Response::builder()
        .status(409)
        .body(Full::default())
        .unwrap()
}

fn method_not_allowed() -> Response<Full<Bytes>> {
    Response::builder()
        .status(405)
        .body(Full::default())
        .unwrap()
}

fn bad_request() -> Response<Full<Bytes>> {
    Response::builder()
        .status(400)
        .body(Full::default())
        .unwrap()
}

fn precondition_failed() -> Response<Full<Bytes>> {
    Response::builder()
        .status(412)
        .body(Full::default())
        .unwrap()
}

fn unsupported_media_type() -> Response<Full<Bytes>> {
    Response::builder()
        .status(415)
        .body(Full::default())
        .unwrap()
}

fn internal_error() -> Response<Full<Bytes>> {
    Response::builder()
        .status(500)
        .body(Full::default())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

fn davpath_to_rel(s: &str) -> PathBuf {
    let s = s.strip_prefix('/').unwrap_or(s);
    if s.is_empty() {
        PathBuf::new()
    } else {
        PathBuf::from(s)
    }
}

async fn handle_proppatch(
    path: &Path,
    req: Request<Incoming>,
    _git: &GitRepo,
) -> Response<Full<Bytes>> {
    // Read (and ignore) the request body — no XML parser dependency.
    // Windows Explorer uses PROPPATCH to set file timestamps after PUT.
    // Return 207 Multistatus OK so Windows doesn't error out.
    let _ = read_body(req.into_body()).await;

    let path_encoded = href_encode(path);
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>{0}</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#,
        path_encoded,
    );

    Response::builder()
        .status(207)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Full::from(body))
        .unwrap()
}

async fn handle_lock(path: &Path, req: Request<Incoming>, _git: &GitRepo) -> Response<Full<Bytes>> {
    // Read (and ignore) the request body — spec requires it, Windows sends it
    let _ = read_body(req.into_body()).await;

    let token = generate_lock_token(path);

    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock><D:locktype><D:write/></D:locktype><D:lockscope><D:exclusive/></D:lockscope><D:depth>0</D:depth><D:timeout>Infinite</D:timeout><D:locktoken><D:href>opaquelocktoken:{}</D:href></D:locktoken></D:activelock></D:lockdiscovery></D:prop>"#,
        token,
    );

    Response::builder()
        .status(200)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Full::from(body))
        .unwrap()
}

fn handle_unlock(_path: &Path, _git: &GitRepo) -> Response<Full<Bytes>> {
    no_content()
}

fn generate_lock_token(path: &Path) -> String {
    use sha1::Digest;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = sha1::Sha1::new();
    hasher.update(now.as_nanos().to_le_bytes());
    hasher.update(path.to_str().unwrap_or(""));
    hasher.update(count.to_le_bytes());
    hex::encode(hasher.finalize())
}

fn href_encode(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "/".into()
    } else {
        format!("/{}", path.display())
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

fn compute_etag(data: &[u8]) -> String {
    use sha1::Digest;
    let hash = sha1::Sha1::digest(data);
    format!("\"{}\"", hex::encode(hash))
}

fn parse_depth(headers: &hyper::HeaderMap) -> Option<u32> {
    let val = headers.get("depth")?.to_str().ok()?;
    match val {
        "0" => Some(0),
        "1" => Some(1),
        "infinity" => Some(1),
        _ => None,
    }
}

fn parse_destination(req: &Request<Incoming>) -> Option<PathBuf> {
    let val = req.headers().get("destination")?.to_str().ok()?;
    let path = if val.starts_with('/') {
        val.to_string()
    } else if let Some(pos) = val.find("//") {
        let after_scheme = &val[pos + 2..];
        let slash = after_scheme.find('/')?;
        after_scheme[slash..].to_string()
    } else {
        return None;
    };
    Some(davpath_to_rel(&path))
}

fn parse_overwrite(headers: &hyper::HeaderMap) -> bool {
    !matches!(
        headers.get("overwrite").and_then(|v| v.to_str().ok()),
        Some("F")
    )
}

async fn read_body(body: Incoming) -> Result<Vec<u8>, hyper::Error> {
    let collected = body.collect().await?;
    Ok(collected.to_bytes().to_vec())
}
