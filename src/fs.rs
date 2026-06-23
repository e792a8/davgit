use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;
use tracing::error;

use bytes::Bytes;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::stream;

use crate::git_bridge::GitRepo;

type WriteCtx = (Arc<GitRepo>, Arc<RwLock<HashMap<PathBuf, Vec<u8>>>>);

#[derive(Clone)]
pub struct GitDavFs {
    tree: Arc<RwLock<HashMap<PathBuf, Vec<u8>>>>,
    git: Arc<GitRepo>,
}

impl GitDavFs {
    pub fn new(tree: HashMap<PathBuf, Vec<u8>>, git: GitRepo) -> Self {
        GitDavFs {
            tree: Arc::new(RwLock::new(tree)),
            git: Arc::new(git),
        }
    }

    fn refresh_if_stale(&self) {
        match self.git.refresh_tree() {
            Ok(Some(new_tree)) => {
                *self.tree.write().unwrap() = new_tree;
            }
            Ok(None) => {}
            Err(e) => error!("failed to refresh tree: {}", e),
        }
    }

    fn davpath_to_pathbuf(&self, path: &DavPath) -> PathBuf {
        let s = path.as_pathbuf();
        s.strip_prefix("/").unwrap_or(&s).to_path_buf()
    }

    fn is_dir(&self, path: &Path) -> bool {
        let tree = self.tree.read().unwrap();
        if path.as_os_str().is_empty() {
            return true;
        }
        if tree.contains_key(path) {
            return false;
        }
        let mut prefix = path.to_path_buf();
        prefix.push("");
        tree.keys().any(|k| k.starts_with(&prefix))
    }

    fn file_exists(&self, path: &Path) -> bool {
        let tree = self.tree.read().unwrap();
        tree.contains_key(path)
    }

    fn commit_delete(&self, path: &Path) -> Result<(), FsError> {
        self.git.delete_path(path).map_err(|e| {
            tracing::error!("git delete failed: {}", e);
            FsError::GeneralFailure
        })
    }
}

impl DavFileSystem for GitDavFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            self.refresh_if_stale();
            let p = self.davpath_to_pathbuf(path);

            if options.write {
                let data = if options.truncate || options.create {
                    Vec::new()
                } else {
                    self.tree
                        .read()
                        .unwrap()
                        .get(&p)
                        .cloned()
                        .unwrap_or_default()
                };
                Ok(Box::new(GitDavFile::new(
                    data,
                    p,
                    true,
                    Some((self.git.clone(), self.tree.clone())),
                )) as Box<dyn DavFile>)
            } else {
                let data = self
                    .tree
                    .read()
                    .unwrap()
                    .get(&p)
                    .cloned()
                    .ok_or(FsError::NotFound)?;
                Ok(Box::new(GitDavFile::new(data, p, false, None)) as Box<dyn DavFile>)
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            self.refresh_if_stale();
            let p = self.davpath_to_pathbuf(path);
            let tree = self.tree.read().unwrap();

            let prefix = if p.as_os_str().is_empty() {
                PathBuf::new()
            } else {
                let mut pp = p.clone();
                pp.push("");
                pp
            };

            let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();

            for key in tree.keys() {
                if let Ok(relative) = key.strip_prefix(&prefix) {
                    if relative.as_os_str().is_empty() {
                        continue;
                    }
                    let first = relative.components().next().unwrap();
                    let name = first.as_os_str().to_string_lossy().to_string();
                    if name == ".davgit_dir" {
                        continue;
                    }
                    if entries.iter().any(|e| e.name() == name.as_bytes()) {
                        continue;
                    }

                    let is_dir = relative.components().count() > 1;
                    let child_path = prefix.join(&name);
                    let len = if is_dir {
                        0
                    } else {
                        tree.get(&child_path).map(|d| d.len() as u64).unwrap_or(0)
                    };

                    entries.push(Box::new(GitDavDirEntry {
                        name: name.clone(),
                        is_dir,
                        len,
                    }));
                }
            }

            if entries.is_empty() && !p.as_os_str().is_empty() && !self.is_dir(&p) {
                return Err(FsError::NotFound);
            }

            let stream: FsStream<Box<dyn DavDirEntry>> =
                Box::pin(stream::iter(entries.into_iter().map(Ok)));
            Ok(stream)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            self.refresh_if_stale();
            let p = self.davpath_to_pathbuf(path);

            if self.is_dir(&p) {
                Ok(Box::new(GitDavMeta {
                    len: 0,
                    is_dir: true,
                }) as Box<dyn DavMetaData>)
            } else if self.file_exists(&p) {
                let len = self
                    .tree
                    .read()
                    .unwrap()
                    .get(&p)
                    .map(|d| d.len() as u64)
                    .unwrap_or(0);
                Ok(Box::new(GitDavMeta { len, is_dir: false }) as Box<dyn DavMetaData>)
            } else {
                Err(FsError::NotFound)
            }
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = self.davpath_to_pathbuf(path);
            let mut tree = self.tree.write().unwrap();
            let marker = p.join(".davgit_dir");
            tree.entry(marker).or_default();
            drop(tree);
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = self.davpath_to_pathbuf(path);
            {
                let mut tree = self.tree.write().unwrap();
                tree.remove(&p);
            }
            self.commit_delete(&p)?;
            Ok(())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = self.davpath_to_pathbuf(path);
            let prefix = {
                let mut pp = p.clone();
                pp.push("");
                pp
            };

            let keys_to_remove: Vec<PathBuf> = {
                let tree = self.tree.read().unwrap();
                tree.keys()
                    .filter(|k| k.starts_with(&prefix) || **k == p)
                    .cloned()
                    .collect()
            };

            let deletes: Vec<PathBuf> = keys_to_remove
                .iter()
                .filter(|k| k.file_name().is_none_or(|n| n != ".davgit_dir"))
                .cloned()
                .collect();

            if !deletes.is_empty() {
                self.git.batch_paths(&[], &deletes).map_err(|e| {
                    tracing::error!("git batch delete failed: {}", e);
                    FsError::GeneralFailure
                })?;
            }

            let mut tree = self.tree.write().unwrap();
            for key in keys_to_remove {
                tree.remove(&key);
            }
            Ok(())
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let src = self.davpath_to_pathbuf(from);
            let dst = self.davpath_to_pathbuf(to);

            let data = {
                let tree = self.tree.read().unwrap();
                tree.get(&src).cloned()
            };

            if let Some(data) = data {
                self.git
                    .batch_paths(&[(dst.clone(), data.clone())], std::slice::from_ref(&src))
                    .map_err(|e| {
                        tracing::error!("git rename failed: {}", e);
                        FsError::GeneralFailure
                    })?;

                let mut tree = self.tree.write().unwrap();
                tree.insert(dst, data);
                tree.remove(&src);
            } else {
                let entries: Vec<(PathBuf, Vec<u8>)> = {
                    let tree = self.tree.read().unwrap();
                    let prefix = {
                        let mut pp = src.clone();
                        pp.push("");
                        pp
                    };
                    tree.iter()
                        .filter(|(k, _)| k.starts_with(&prefix))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                };

                let writes: Vec<(PathBuf, Vec<u8>)> = entries
                    .iter()
                    .map(|(old_path, data)| {
                        let rel = old_path.strip_prefix(&src).unwrap();
                        (dst.join(rel), data.clone())
                    })
                    .collect();
                let deletes: Vec<PathBuf> = entries
                    .iter()
                    .map(|(old_path, _)| old_path.clone())
                    .collect();

                self.git.batch_paths(&writes, &deletes).map_err(|e| {
                    tracing::error!("git batch rename failed: {}", e);
                    FsError::GeneralFailure
                })?;
                drop(writes);

                let mut tree = self.tree.write().unwrap();
                for (old_path, data) in entries {
                    let rel = old_path.strip_prefix(&src).unwrap();
                    let new_path = dst.join(rel);
                    tree.insert(new_path, data);
                    tree.remove(&old_path);
                }
            }
            Ok(())
        })
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let src = self.davpath_to_pathbuf(from);
            let dst = self.davpath_to_pathbuf(to);

            let data = {
                let tree = self.tree.read().unwrap();
                tree.get(&src).cloned()
            };

            if let Some(data) = data {
                self.git
                    .batch_paths(&[(dst.clone(), data.clone())], &[])
                    .map_err(|e| {
                        tracing::error!("git copy failed: {}", e);
                        FsError::GeneralFailure
                    })?;

                let mut tree = self.tree.write().unwrap();
                tree.insert(dst, data);
            } else {
                let entries: Vec<(PathBuf, Vec<u8>)> = {
                    let tree = self.tree.read().unwrap();
                    let prefix = {
                        let mut pp = src.clone();
                        pp.push("");
                        pp
                    };
                    tree.iter()
                        .filter(|(k, _)| k.starts_with(&prefix))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                };

                let writes: Vec<(PathBuf, Vec<u8>)> = entries
                    .iter()
                    .map(|(old_path, data)| {
                        let rel = old_path.strip_prefix(&src).unwrap();
                        (dst.join(rel), data.clone())
                    })
                    .collect();

                self.git.batch_paths(&writes, &[]).map_err(|e| {
                    tracing::error!("git batch copy failed: {}", e);
                    FsError::GeneralFailure
                })?;

                let mut tree = self.tree.write().unwrap();
                for (old_path, data) in entries {
                    let rel = old_path.strip_prefix(&src).unwrap();
                    let new_path = dst.join(rel);
                    tree.insert(new_path, data);
                }
            }
            Ok(())
        })
    }
}

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

struct GitDavFile {
    data: Vec<u8>,
    pos: usize,
    path: PathBuf,
    is_write: bool,
    write_ctx: Option<WriteCtx>,
    meta: GitDavMeta,
}

impl GitDavFile {
    fn new(data: Vec<u8>, path: PathBuf, is_write: bool, write_ctx: Option<WriteCtx>) -> Self {
        let len = data.len() as u64;
        GitDavFile {
            data,
            pos: 0,
            path,
            is_write,
            write_ctx,
            meta: GitDavMeta { len, is_dir: false },
        }
    }
}

impl std::fmt::Debug for GitDavFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitDavFile")
            .field("path", &self.path)
            .field("pos", &self.pos)
            .field("is_write", &self.is_write)
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
            if let Some((ref git, ref tree_lock)) = self.write_ctx {
                let data = std::mem::take(&mut self.data);
                git.write_path(&self.path, &data).map_err(|e| {
                    tracing::error!("git write on flush failed: {}", e);
                    FsError::GeneralFailure
                })?;
                let mut tree = tree_lock.write().unwrap();
                tree.insert(self.path.clone(), data);
            }
            Ok(())
        })
    }
}

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
