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
    git.refresh_if_stale().await;

    let rel_path = davpath_to_rel(req.uri().path());

    match req.method().as_str() {
        "GET" => handle_get(&rel_path, &req, &git).await,
        "HEAD" => handle_head(&rel_path, &req, &git).await,
        "PUT" => handle_put(&rel_path, req, &git).await,
        "DELETE" => handle_delete(&rel_path, &git).await,
        "MKCOL" => handle_mkcol(&rel_path, req, &git).await,
        "OPTIONS" => handle_options(),
        "PROPFIND" => handle_propfind(&rel_path, &req, &git).await,
        "PROPPATCH" => not_implemented(),
        "MOVE" => handle_copy_move(&rel_path, &req, &git, true).await,
        "COPY" => handle_copy_move(&rel_path, &req, &git, false).await,
        "LOCK" => method_not_allowed(),
        "UNLOCK" => method_not_allowed(),
        _ => method_not_allowed(),
    }
}

// ---------------------------------------------------------------------------
// Handler functions
// ---------------------------------------------------------------------------

async fn handle_get(path: &Path, req: &Request<Incoming>, git: &GitRepo) -> Response<Full<Bytes>> {
    if git.is_directory(path) {
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
    // PUT to a non-existent parent must fail with 409
    let parent = path.parent().unwrap_or(Path::new(""));
    if !parent.as_os_str().is_empty() && !git.is_directory(parent) {
        return conflict();
    }

    let data = match read_body(req.into_body()).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("read body failed: {}", e);
            return internal_error();
        }
    };

    match git.write_path(path, &data).await {
        Ok(_) => {
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
    if git.is_directory(path) {
        let entries = match git.list_dir(path) {
            Ok(e) => e,
            Err(_) => return internal_error(),
        };
        let deletes: Vec<PathBuf> = entries
            .iter()
            .filter(|(_, is_dir, _)| !*is_dir)
            .map(|(name, _, _)| path.join(name))
            .collect();
        if !deletes.is_empty() {
            match git.batch_paths(&[], &deletes).await {
                Ok(_) => {
                    git.remove_dir_marker(path);
                    no_content()
                }
                Err(e) => {
                    tracing::error!("batch_paths delete dir failed: {}", e);
                    internal_error()
                }
            }
        } else {
            git.remove_dir_marker(path);
            no_content()
        }
    } else if git.file_size(path).is_some() {
        match git.delete_path(path).await {
            Ok(_) => no_content(),
            Err(e) => {
                tracing::error!("delete_path({:?}) failed: {}", path, e);
                internal_error()
            }
        }
    } else {
        not_found()
    }
}

async fn handle_mkcol(path: &Path, req: Request<Incoming>, git: &GitRepo) -> Response<Full<Bytes>> {
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
    git.create_dir(path);
    created()
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
    if !git.is_directory(path) && git.file_size(path).is_none() {
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
        for (name, entry_is_dir, entry_len) in entries {
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
    let dest_path = match parse_destination(req) {
        Some(p) => p,
        None => return bad_request(),
    };

    if !git.is_directory(path) && git.file_size(path).is_none() {
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

    let (writes, mut deletes) = if git.is_directory(path) {
        let entries = match git.list_dir(path) {
            Ok(entries) => entries,
            Err(_) => return internal_error(),
        };
        let writes: Vec<(PathBuf, Vec<u8>)> = entries
            .iter()
            .filter(|(_, is_dir, _)| !*is_dir)
            .filter_map(|(name, _, _)| {
                let src = path.join(name);
                git.read_file(&src)
                    .ok()
                    .flatten()
                    .map(|data| (dest_path.join(name), data))
            })
            .collect();
        if is_move {
            let deletes: Vec<PathBuf> = entries
                .iter()
                .filter(|(_, is_dir, _)| !*is_dir)
                .map(|(name, _, _)| path.join(name))
                .collect();
            (writes, deletes)
        } else {
            (writes, vec![])
        }
    } else {
        let data = match git.read_file(path) {
            Ok(Some(d)) => d,
            _ => return internal_error(),
        };
        let writes = vec![(dest_path.clone(), data)];
        let deletes = if is_move {
            vec![path.to_path_buf()]
        } else {
            vec![]
        };
        (writes, deletes)
    };

    if git.is_directory(&dest_path)
        && let Ok(entries) = git.list_dir(&dest_path)
    {
        for (name, is_dir, _) in &entries {
            if !is_dir {
                deletes.push(dest_path.join(name));
            }
        }
    }

    match git.batch_paths(&writes, &deletes).await {
        Ok(_) => {
            if is_move && git.is_directory(path) {
                git.remove_dir_marker(path);
            }
            // 201 if destination was created new, 204 if updated
            if dest_exists { no_content() } else { created() }
        }
        Err(e) => {
            tracing::error!("batch_paths for copy/move failed: {}", e);
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

fn not_implemented() -> Response<Full<Bytes>> {
    Response::builder()
        .status(501)
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
