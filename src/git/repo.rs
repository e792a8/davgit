use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::git::objects::{build_commit, build_index_and_contents, build_trees};
use crate::git::packfile::{build_packfile, hash_object, parse_packfile, ObjectId, OBJ_COMMIT};
use crate::git::transport::do_fetch;
use crate::git::transport::do_push;

const FETCH_THROTTLE: Duration = Duration::from_secs(3);
const MAX_RETRIES: u32 = 3;

type ObjectList = Vec<(u8, Vec<u8>)>;

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
    objects: Mutex<HashMap<ObjectId, Vec<u8>>>,
    index: Mutex<HashMap<PathBuf, ObjectId>>,
    dirs: Mutex<HashSet<PathBuf>>,
    last_fetch: Mutex<Instant>,
}

impl GitRepo {
    pub async fn init_and_fetch(
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
            objects: Mutex::new(HashMap::new()),
            index: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashSet::new()),
            last_fetch: Mutex::new(Instant::now()),
        };

        repo.try_fetch_and_cache(None).await;
        Ok(repo)
    }

    /// Try to fetch fresh data and update the internal cache.
    /// Returns true if the cache was updated.
    pub async fn refresh_if_stale(&self) -> bool {
        {
            let last = self.last_fetch.lock().unwrap();
            if last.elapsed() < FETCH_THROTTLE {
                return false;
            }
        }
        self.try_fetch_and_cache(Some(true)).await
    }

    /// Read a file's content. Returns `None` if the path is a directory or doesn't exist.
    pub fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let index = self.index.lock().unwrap();
        match index.get(path) {
            Some(oid) => {
                let objects = self.objects.lock().unwrap();
                Ok(objects.get(oid).cloned())
            }
            None => Ok(None),
        }
    }

    /// List entries in a directory: (name, is_dir, size).
    /// Returns empty vec for non-existent paths.
    pub fn list_dir(&self, path: &Path) -> Result<Vec<(String, bool, u64)>> {
        let index = self.index.lock().unwrap();
        let objects = self.objects.lock().unwrap();
        let dirs = self.dirs.lock().unwrap();

        let prefix = if path.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            let mut p = path.to_path_buf();
            p.push("");
            p
        };

        let mut entries: Vec<(String, bool, u64)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        // Collect from the file index
        for key in index.keys() {
            if let Ok(rel) = key.strip_prefix(&prefix) {
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let first = rel.components().next().unwrap();
                let name = first.as_os_str().to_string_lossy().to_string();
                if seen.contains(&name) {
                    continue;
                }
                seen.insert(name.clone());

                let is_dir = rel.components().count() > 1;
                let len = if is_dir {
                    0
                } else if let Some(oid) = index.get(key) {
                    objects.get(oid).map(|d| d.len() as u64).unwrap_or(0)
                } else {
                    0
                };
                entries.push((name, is_dir, len));
            }
        }

        // Collect empty directories (dir markers)
        for dir_path in dirs.iter() {
            if let Ok(rel) = dir_path.strip_prefix(&prefix) {
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let first = rel.components().next().unwrap();
                let name = first.as_os_str().to_string_lossy().to_string();
                if seen.contains(&name) {
                    continue;
                }
                seen.insert(name.clone());
                entries.push((name, true, 0));
            }
        }

        Ok(entries)
    }

    /// Check if a path is a directory.
    pub fn is_directory(&self, path: &Path) -> bool {
        if path.as_os_str().is_empty() {
            return true; // root is always a directory
        }
        let index = self.index.lock().unwrap();
        let dirs = self.dirs.lock().unwrap();
        if dirs.contains(path) {
            return true;
        }
        // Check if any index key is a child of `path` (not `path` itself)
        let has_child = index.keys().any(|k| k != path && k.starts_with(path));
        if has_child {
            return true;
        }
        false
    }

    /// Get the content length of a file.
    pub fn file_size(&self, path: &Path) -> Option<u64> {
        let index = self.index.lock().unwrap();
        let oid = index.get(path)?;
        let objects = self.objects.lock().unwrap();
        objects.get(oid).map(|d| d.len() as u64)
    }

    /// Notify that a directory was created (MKCOL) — tracked in-memory.
    pub fn create_dir(&self, path: &Path) {
        self.dirs.lock().unwrap().insert(path.to_path_buf());
    }

    /// Remove a directory marker (empty dir) from in-memory tracking.
    pub fn remove_dir_marker(&self, path: &Path) {
        self.dirs.lock().unwrap().remove(path);
    }

    // -----------------------------------------------------------------------
    // Write operations (commit + push)
    // -----------------------------------------------------------------------

    pub async fn write_path(&self, path: &Path, data: &[u8]) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let msg = format!("update {}", path_str);

        for _ in 0..MAX_RETRIES {
            if self.commit_and_push(
                &[(path.to_path_buf(), data.to_vec())],
                &[],
                &msg,
            ).await? {
                return Ok(());
            }
        }
        bail!("write failed after {} attempts", MAX_RETRIES);
    }

    pub async fn delete_path(&self, path: &Path) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let msg = format!("delete {}", path_str);

        for _ in 0..MAX_RETRIES {
            if self.commit_and_push(&[], &[path.to_path_buf()], &msg).await? {
                return Ok(());
            }
        }
        bail!("delete failed after {} attempts", MAX_RETRIES);
    }

    pub async fn batch_paths(
        &self,
        writes: &[(PathBuf, Vec<u8>)],
        deletes: &[PathBuf],
    ) -> Result<()> {
        for _ in 0..MAX_RETRIES {
            if self.commit_and_push(writes, deletes, "batch update").await? {
                return Ok(());
            }
        }
        bail!("batch commit failed after {} attempts", MAX_RETRIES);
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn try_fetch_and_cache(&self, _is_refresh: Option<bool>) -> bool {
        let result = match do_fetch(
            &self.remote_url,
            &self.branch,
            self.ssh_key.as_deref(),
            self.password.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("fetch failed: {}", e);
                *self.last_fetch.lock().unwrap() = Instant::now();
                return false;
            }
        };

        let objects = match parse_packfile(&result.packfile) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("parse_packfile failed: {}", e);
                *self.last_fetch.lock().unwrap() = Instant::now();
                return false;
            }
        };

        let head = result.head_commit_oid;

        // Extract tree_oid before consuming objects
        let tree_oid = {
            let commit_data = match objects.get(&head) {
                Some(c) => c,
                None => {
                    tracing::warn!("HEAD commit not found in fetched packfile");
                    *self.last_fetch.lock().unwrap() = Instant::now();
                    return false;
                }
            };
            let commit = match gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("commit parse failed: {}", e);
                    *self.last_fetch.lock().unwrap() = Instant::now();
                    return false;
                }
            };
            commit.tree().to_owned()
        };

        let (index, _contents) = match build_index_and_contents(&objects, &tree_oid) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("build_index failed: {}", e);
                *self.last_fetch.lock().unwrap() = Instant::now();
                return false;
            }
        };

        *self.head_oid.lock().unwrap() = Some(head);
        *self.objects.lock().unwrap() = objects;
        *self.index.lock().unwrap() = index;
        *self.last_fetch.lock().unwrap() = Instant::now();
        true
    }

    async fn fetch_for_write(&self) -> Result<(HashMap<PathBuf, Vec<u8>>, ObjectId)> {
        let result = do_fetch(
            &self.remote_url,
            &self.branch,
            self.ssh_key.as_deref(),
            self.password.as_deref(),
        )
        .await?;
        let head = result.head_commit_oid;
        let objects = parse_packfile(&result.packfile)?;

        let tree_oid = {
            let tree_data = objects
                .get(&head)
                .context("HEAD commit not found in fetched packfile")?;
            let commit = gix_object::CommitRef::from_bytes(tree_data, gix_hash::Kind::Sha1)?;
            commit.tree().to_owned()
        };

        let (index, files) = build_index_and_contents(&objects, &tree_oid)?;

        *self.head_oid.lock().unwrap() = Some(head);
        *self.objects.lock().unwrap() = objects;
        *self.index.lock().unwrap() = index;
        *self.last_fetch.lock().unwrap() = Instant::now();

        Ok((files, head))
    }

    async fn commit_and_push(
        &self,
        writes: &[(PathBuf, Vec<u8>)],
        deletes: &[PathBuf],
        message: &str,
    ) -> Result<bool> {
        let (mut files, parent_oid) = self.fetch_for_write().await?;

        for (path, data) in writes {
            files.insert(path.clone(), data.clone());
        }
        for path in deletes {
            files.remove(path);
        }

        let (commit_oid, object_list) = build_change_commit(
            &files,
            Some(parent_oid),
            &self.author_name,
            &self.author_email,
            message,
        )?;

        let entries: Vec<(u8, &[u8])> = object_list.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        let packfile = build_packfile(&entries)?;

        let pushed = do_push(
            &self.remote_url,
            &self.branch,
            self.ssh_key.as_deref(),
            self.password.as_deref(),
            commit_oid,
            &packfile,
        )
        .await?;

        if pushed {
            // Cache the new commit and objects
            let new_objects: HashMap<ObjectId, Vec<u8>> = object_list
                .into_iter()
                .map(|(kind, data)| (hash_object(kind, &data), data))
                .collect();

            {
                let mut objects = self.objects.lock().unwrap();
                for (oid, data) in new_objects {
                    objects.entry(oid).or_insert(data);
                }
            }

            // Rebuild index from the new commit
            let objects = self.objects.lock().unwrap();
            let commit_data = objects.get(&commit_oid).context("new commit missing")?;
            let commit = gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1)?;
            let tree_oid: ObjectId = commit.tree().to_owned();
            let (new_index, _new_files) = build_index_and_contents(&objects, &tree_oid)?;
            *self.index.lock().unwrap() = new_index;

            *self.head_oid.lock().unwrap() = Some(commit_oid);
            *self.last_fetch.lock().unwrap() = Instant::now();
        }

        Ok(pushed)
    }
}

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

    // Deduplicate objects by OID (e.g. when same content exists at multiple paths)
    let mut seen = std::collections::HashSet::new();
    let deduped: ObjectList = objects
        .into_iter()
        .filter(|(kind, data)| seen.insert(hash_object(*kind, data)))
        .collect();

    Ok((commit_oid, deduped))
}
