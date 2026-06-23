use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::stream;

use crate::git::GitRepo;

// ---------------------------------------------------------------------------
// GitDavFs
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GitDavFs {
    git: Arc<GitRepo>,
}

impl GitDavFs {
    pub fn new(git: GitRepo) -> Self {
        GitDavFs {
            git: Arc::new(git),
        }
    }

    fn davpath_to_pathbuf(&self, path: &DavPath) -> PathBuf {
        let s = path.as_pathbuf();
        s.strip_prefix("/").unwrap_or(&s).to_path_buf()
    }
}

impl DavFileSystem for GitDavFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            self.git.refresh_if_stale();
            let p = self.davpath_to_pathbuf(path);

            if options.write {
                let data = if options.truncate || options.create {
                    Vec::new()
                } else {
                    self.git
                        .read_file(&p)
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                };
                Ok(Box::new(GitDavFile::new(data, p, self.git.clone()))
                    as Box<dyn DavFile>)
            } else {
                let data = self
                    .git
                    .read_file(&p)
                    .ok()
                    .flatten()
                    .ok_or(FsError::NotFound)?;
                Ok(Box::new(GitDavFile::new(data, p, self.git.clone()))
                    as Box<dyn DavFile>)
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            self.git.refresh_if_stale();
            let p = self.davpath_to_pathbuf(path);

            let entries = self.git.list_dir(&p).map_err(|e| {
                tracing::error!("list_dir failed: {}", e);
                FsError::GeneralFailure
            })?;

            if entries.is_empty() && !p.as_os_str().is_empty() && !self.git.is_directory(&p) {
                return Err(FsError::NotFound);
            }

            let stream: FsStream<Box<dyn DavDirEntry>> = Box::pin(
                stream::iter(entries.into_iter().map(|(name, is_dir, len)| {
                    Ok(Box::new(GitDavDirEntry { name, is_dir, len }) as Box<dyn DavDirEntry>)
                })),
            );
            Ok(stream)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            self.git.refresh_if_stale();
            let p = self.davpath_to_pathbuf(path);

            if self.git.is_directory(&p) {
                Ok(Box::new(GitDavMeta {
                    len: 0,
                    is_dir: true,
                }) as Box<dyn DavMetaData>)
            } else if let Some(len) = self.git.file_size(&p) {
                Ok(Box::new(GitDavMeta { len, is_dir: false }) as Box<dyn DavMetaData>)
            } else {
                Err(FsError::NotFound)
            }
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = self.davpath_to_pathbuf(path);
            self.git.create_dir(&p);
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = self.davpath_to_pathbuf(path);
            self.git.delete_path(&p).map_err(|e| {
                tracing::error!("git delete failed: {}", e);
                FsError::GeneralFailure
            })?;
            Ok(())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = self.davpath_to_pathbuf(path);

            let (writes, deletes) = self.dir_diff(&p);
            if !deletes.is_empty() {
                self.git.batch_paths(&writes, &deletes).map_err(|e| {
                    tracing::error!("git batch delete failed: {}", e);
                    FsError::GeneralFailure
                })?;
            }
            Ok(())
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let src = self.davpath_to_pathbuf(from);
            let dst = self.davpath_to_pathbuf(to);

            if let Some(data) = self.git.read_file(&src).ok().flatten() {
                self.git
                    .batch_paths(&[(dst.clone(), data)], &[src])
                    .map_err(|e| {
                        tracing::error!("git rename failed: {}", e);
                        FsError::GeneralFailure
                    })?;
            } else {
                let (writes, deletes) = self.copy_diff(&src, &dst);
                self.git.batch_paths(&writes, &deletes).map_err(|e| {
                    tracing::error!("git batch rename failed: {}", e);
                    FsError::GeneralFailure
                })?;
            }
            Ok(())
        })
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let src = self.davpath_to_pathbuf(from);
            let dst = self.davpath_to_pathbuf(to);

            if let Some(data) = self.git.read_file(&src).ok().flatten() {
                self.git
                    .batch_paths(&[(dst.clone(), data)], &[])
                    .map_err(|e| {
                        tracing::error!("git copy failed: {}", e);
                        FsError::GeneralFailure
                    })?;
            } else {
                let entries: Vec<(PathBuf, Vec<u8>)> = {
                    let list = self
                        .git
                        .list_dir(&src)
                        .ok()
                        .unwrap_or_default();
                    let mut result = Vec::new();
                    for (name, is_dir, _len) in list {
                        if !is_dir {
                            let src_path = src.join(&name);
                            if let Some(data) = self.git.read_file(&src_path).ok().flatten() {
                                result.push((dst.join(&name), data));
                            }
                        }
                    }
                    result
                };

                self.git.batch_paths(&entries, &[]).map_err(|e| {
                    tracing::error!("git batch copy failed: {}", e);
                    FsError::GeneralFailure
                })?;
            }
            Ok(())
        })
    }
}

impl GitDavFs {
    /// Collect all files under `dir` that should be deleted, plus any writes
    /// needed (none, for removal). Returns (writes, deletes).
    fn dir_diff(&self, dir: &Path) -> (Vec<(PathBuf, Vec<u8>)>, Vec<PathBuf>) {
        let deletes: Vec<PathBuf> = self
            .git
            .list_dir(dir)
            .ok()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, is_dir, _)| !*is_dir)
            .map(|(name, _, _)| dir.join(name))
            .collect();
        (vec![], deletes)
    }

    /// Collect all files under `src` to be moved to `dst` as writes and the
    /// respective source paths as deletes.
    fn copy_diff(
        &self,
        src: &Path,
        dst: &Path,
    ) -> (Vec<(PathBuf, Vec<u8>)>, Vec<PathBuf>) {
        let list = self.git.list_dir(src).ok().unwrap_or_default();
        let mut writes = Vec::new();
        let mut deletes = Vec::new();
        for (name, is_dir, _len) in list {
            if !is_dir {
                let src_path = src.join(&name);
                if let Some(data) = self.git.read_file(&src_path).ok().flatten() {
                    writes.push((dst.join(&name), data));
                    deletes.push(src_path);
                }
            }
        }
        (writes, deletes)
    }
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct GitDavMeta {
    len: u64,
    is_dir: bool,
}

impl DavMetaData for GitDavMeta {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Err(FsError::NotImplemented)
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

// ---------------------------------------------------------------------------
// File handle
// ---------------------------------------------------------------------------

struct GitDavFile {
    data: Vec<u8>,
    pos: usize,
    path: PathBuf,
    git: Arc<GitRepo>,
    meta: GitDavMeta,
}

impl GitDavFile {
    fn new(data: Vec<u8>, path: PathBuf, git: Arc<GitRepo>) -> Self {
        let len = data.len() as u64;
        GitDavFile {
            data,
            pos: 0,
            path,
            git,
            meta: GitDavMeta { len, is_dir: false },
        }
    }
}

impl std::fmt::Debug for GitDavFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitDavFile")
            .field("path", &self.path)
            .field("pos", &self.pos)
            .finish()
    }
}

impl DavFile for GitDavFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move { Ok(Box::new(self.meta.clone()) as Box<dyn DavMetaData>) })
    }

    fn write_buf(&mut self, buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let mut buf = buf;
            while buf.has_remaining() {
                let chunk = buf.chunk();
                self.data.extend_from_slice(chunk);
                buf.advance(chunk.len());
            }
            Ok(())
        })
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.data.extend_from_slice(&buf);
            Ok(())
        })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let available = self.data.len() - self.pos;
            let to_read = count.min(available);
            let result = Bytes::copy_from_slice(&self.data[self.pos..self.pos + to_read]);
            self.pos += to_read;
            Ok(result)
        })
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            match pos {
                std::io::SeekFrom::Start(offset) => {
                    self.pos = offset as usize;
                }
                std::io::SeekFrom::End(offset) => {
                    let len = self.data.len();
                    if offset >= 0 {
                        self.pos = len.saturating_add(offset as usize);
                    } else {
                        let abs = (-offset) as usize;
                        self.pos = len.saturating_sub(abs);
                    }
                }
                std::io::SeekFrom::Current(offset) => {
                    if offset >= 0 {
                        self.pos = self.pos.saturating_add(offset as usize);
                    } else {
                        let abs = (-offset) as usize;
                        self.pos = self.pos.saturating_sub(abs);
                    }
                }
            }
            Ok(self.pos as u64)
        })
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let data = std::mem::take(&mut self.data);
            self.git.write_path(&self.path, &data).map_err(|e| {
                tracing::error!("git write on flush failed: {}", e);
                FsError::GeneralFailure
            })?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Directory entry
// ---------------------------------------------------------------------------

struct GitDavDirEntry {
    name: String,
    is_dir: bool,
    len: u64,
}

impl DavDirEntry for GitDavDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            Ok(Box::new(GitDavMeta {
                len: self.len,
                is_dir: self.is_dir,
            }) as Box<dyn DavMetaData>)
        })
    }
}
