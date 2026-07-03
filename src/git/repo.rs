use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use gix_object::TreeRefIter;

use crate::git::objects::{
    build_commit, collect_all_oids, copy_tree_entry, find_tree_entry, modify_tree_entry,
    remove_tree_entry,
};
use crate::git::packfile::{
    OBJ_BLOB, OBJ_COMMIT, ObjectId, build_packfile, hash_object, parse_packfile,
};
use crate::git::transport::do_fetch;
use crate::git::transport::do_push;

const MAX_RETRIES: u32 = 3;

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
        };

        repo.fetch().await?;
        Ok(repo)
    }

    /// Read a file's content. Returns `None` if the path is a directory or doesn't exist.
    pub fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let Some(head) = *self.head_oid.lock().unwrap() else {
            bail!("repo head is None")
        };
        let objects = self.objects.lock().unwrap();
        let Some(commit_data) = objects.get(&head) else {
            return Ok(None);
        };
        let commit = gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1)?;
        let tree_oid = commit.tree().to_owned();
        match find_tree_entry(&objects, &tree_oid, path)? {
            Some((mode, oid)) if !mode.is_tree() => Ok(objects.get(&oid).cloned()),
            _ => Ok(None),
        }
    }

    /// List entries in a directory: (name, is_dir, size).
    /// Returns empty vec for non-existent paths.
    pub fn list_dir(&self, path: &Path) -> Result<Vec<(String, bool, u64)>> {
        let head = match *self.head_oid.lock().unwrap() {
            Some(h) => h,
            None => return Ok(vec![]),
        };
        let objects = self.objects.lock().unwrap();
        let tree_oid = {
            let commit_data = match objects.get(&head) {
                Some(d) => d,
                None => return Ok(vec![]),
            };
            let commit = gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1)?;
            commit.tree().to_owned()
        };
        let dir_oid = match find_tree_entry(&objects, &tree_oid, path)? {
            Some((mode, oid)) if mode.is_tree() => oid,
            _ => return Ok(vec![]),
        };
        let tree_data = match objects.get(&dir_oid) {
            Some(d) => d,
            None => return Ok(vec![]),
        };
        let mut entries = Vec::new();
        for entry in TreeRefIter::from_bytes(tree_data, gix_hash::Kind::Sha1).flatten() {
            let name = match std::str::from_utf8(entry.filename) {
                Ok(n) => n.to_owned(),
                Err(_) => continue,
            };
            let is_dir = entry.mode.is_tree();
            let len = if is_dir {
                0
            } else {
                objects
                    .get(&entry.oid.to_owned())
                    .map(|d| d.len() as u64)
                    .unwrap_or(0)
            };
            entries.push((name, is_dir, len));
        }
        Ok(entries)
    }

    /// Check if a path is a directory.
    pub fn is_directory(&self, path: &Path) -> bool {
        if path.as_os_str().is_empty() {
            return true;
        }
        let head = match *self.head_oid.lock().unwrap() {
            Some(h) => h,
            None => return false,
        };
        let objects = self.objects.lock().unwrap();
        let commit_data = match objects.get(&head) {
            Some(d) => d,
            None => return false,
        };
        let commit = match gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let tree_oid = commit.tree().to_owned();
        match find_tree_entry(&objects, &tree_oid, path) {
            Ok(Some((mode, _))) => mode.is_tree(),
            _ => false,
        }
    }

    /// Get the content length of a file.
    pub fn file_size(&self, path: &Path) -> Option<u64> {
        let head = (*self.head_oid.lock().unwrap())?;
        let objects = self.objects.lock().unwrap();
        let commit_data = objects.get(&head)?;
        let commit = gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1).ok()?;
        let tree_oid = commit.tree().to_owned();
        let (mode, oid) = find_tree_entry(&objects, &tree_oid, path).ok()??;
        if mode.is_tree() {
            None
        } else {
            objects.get(&oid).map(|d| d.len() as u64)
        }
    }

    /// Create a directory by committing a ".DAV" marker file in it.
    pub async fn create_dir(&self, path: &Path) -> Result<()> {
        let msg = format!("create dir {:?}", path);
        for _ in 0..MAX_RETRIES {
            if self.try_create_dir(path, &msg).await? {
                return Ok(());
            }
        }
        bail!("create dir failed after {} attempts", MAX_RETRIES);
    }

    // -----------------------------------------------------------------------
    // Write operations (commit + push)
    // -----------------------------------------------------------------------

    pub async fn write_path(&self, path: &Path, data: &[u8]) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let msg = format!("update {}", path_str);

        for _ in 0..MAX_RETRIES {
            if self
                .commit_and_push(&[(path.to_path_buf(), data.to_vec())], &[], &msg)
                .await?
            {
                return Ok(());
            }
        }
        bail!("write failed after {} attempts", MAX_RETRIES);
    }

    pub async fn delete_path(&self, path: &Path) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let msg = format!("delete {}", path_str);

        for _ in 0..MAX_RETRIES {
            if self
                .commit_and_push(&[], &[path.to_path_buf()], &msg)
                .await?
            {
                return Ok(());
            }
        }
        bail!("delete failed after {} attempts", MAX_RETRIES);
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------
    /// Fetch latest state from remote and merge into local cache.
    async fn fetch(&self) -> Result<()> {
        let head = *self.head_oid.lock().unwrap();
        let have: Vec<ObjectId> = head.into_iter().collect();
        let result = do_fetch(
            &self.remote_url,
            &self.branch,
            self.ssh_key.as_deref(),
            self.password.as_deref(),
            Some(1),
            &have,
            &have,
        )
        .await?;

        if result.packfile.is_empty() {
            tracing::debug!("fetch: no new objects, using cached state");
            return Ok(());
        }

        let head = result.head_commit_oid;
        let new_objects = parse_packfile(&result.packfile)?;
        let mut merged = self.objects.lock().unwrap().clone();
        merged.extend(new_objects);

        let tree_oid = {
            let commit_data = merged
                .get(&head)
                .context("HEAD commit not found in fetched packfile")?;
            let commit = gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1)?;
            commit.tree().to_owned()
        };

        Self::prune_objects(&mut merged, &tree_oid, &head);

        *self.head_oid.lock().unwrap() = Some(head);
        *self.objects.lock().unwrap() = merged;

        Ok(())
    }

    /// Get owned objects/head/tree data from the internal cache.
    /// Call after a successful fetch().
    fn claim_objects(&self) -> Result<(ObjectId, HashMap<ObjectId, Vec<u8>>, ObjectId)> {
        let head = self.head_oid.lock().unwrap().context("no HEAD")?;
        let objects = self.objects.lock().unwrap().clone();
        let tree_oid = {
            let commit_data = objects
                .get(&head)
                .context("HEAD commit not found in objects")?;
            let commit = gix_object::CommitRef::from_bytes(commit_data, gix_hash::Kind::Sha1)?;
            commit.tree().to_owned()
        };
        Ok((head, objects, tree_oid))
    }
    /// Push a commit built from tree-delta objects and cache on success.
    async fn push_with_cache(
        &self,
        parent_oid: ObjectId,
        new_root_oid: ObjectId,
        pack_objects: Vec<(u8, Vec<u8>)>,
        msg: &str,
    ) -> Result<bool> {
        let commit_data = build_commit(
            &new_root_oid,
            Some(&parent_oid),
            &self.author_name,
            &self.author_email,
            msg,
        );
        let commit_oid = hash_object(OBJ_COMMIT, &commit_data);

        let mut seen: HashSet<ObjectId> = HashSet::new();
        seen.insert(commit_oid);
        let mut deduped: Vec<(u8, Vec<u8>)> = pack_objects
            .into_iter()
            .filter(|(kind, data)| seen.insert(hash_object(*kind, data)))
            .collect();
        deduped.push((OBJ_COMMIT, commit_data));

        let entries: Vec<(u8, &[u8])> = deduped.iter().map(|(k, v)| (*k, v.as_slice())).collect();
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
            let mut objects = self.objects.lock().unwrap();
            for (kind, data) in deduped {
                let oid = hash_object(kind, &data);
                objects.entry(oid).or_insert(data);
            }
            *self.head_oid.lock().unwrap() = Some(commit_oid);
        }

        Ok(pushed)
    }

    /// Prune the objects map to keep only what's needed by the current tree
    /// and the HEAD commit.
    fn prune_objects(
        objects: &mut HashMap<ObjectId, Vec<u8>>,
        tree_oid: &ObjectId,
        head: &ObjectId,
    ) {
        let mut needed = collect_all_oids(objects, tree_oid);
        needed.insert(*head);
        objects.retain(|k, _| needed.contains(k));
    }

    // -----------------------------------------------------------------------
    // Tree-based copy / move / delete
    // -----------------------------------------------------------------------

    pub async fn copy_subtree(&self, src: &Path, dest: &Path) -> Result<()> {
        let msg = format!("copy {:?} -> {:?}", src, dest);
        for _ in 0..MAX_RETRIES {
            if self.try_copy_or_move(src, dest, &msg, false).await? {
                return Ok(());
            }
        }
        bail!("copy failed after {} attempts", MAX_RETRIES);
    }

    pub async fn move_subtree(&self, src: &Path, dest: &Path) -> Result<()> {
        let msg = format!("move {:?} -> {:?}", src, dest);
        for _ in 0..MAX_RETRIES {
            if self.try_copy_or_move(src, dest, &msg, true).await? {
                return Ok(());
            }
        }
        bail!("move failed after {} attempts", MAX_RETRIES);
    }

    /// Delete an entire tree entry (file or directory) at `path`.
    /// Uses `remove_tree_entry` so a single call removes a subtree.
    pub async fn delete_subtree(&self, path: &Path) -> Result<()> {
        let msg = format!("delete {:?}", path);
        for _ in 0..MAX_RETRIES {
            if self.try_delete_subtree(path, &msg).await? {
                return Ok(());
            }
        }
        bail!("delete subtree failed after {} attempts", MAX_RETRIES);
    }

    async fn try_copy_or_move(
        &self,
        src: &Path,
        dest: &Path,
        msg: &str,
        is_move: bool,
    ) -> Result<bool> {
        self.fetch().await?;
        let (parent_oid, mut objects, tree_oid) = self.claim_objects()?;

        let (tmp_root, mut tmp_objs) = if find_tree_entry(&objects, &tree_oid, dest)?.is_some() {
            let (r, o) = remove_tree_entry(&mut objects, &tree_oid, dest)?;
            (r, o)
        } else {
            (tree_oid, vec![])
        };

        let (copy_root, copy_objs) = copy_tree_entry(&mut objects, &tmp_root, src, dest)?;
        tmp_objs.extend(copy_objs);

        let final_root = if is_move {
            let (r, o) = remove_tree_entry(&mut objects, &copy_root, src)?;
            tmp_objs.extend(o);
            r
        } else {
            copy_root
        };

        self.push_with_cache(parent_oid, final_root, tmp_objs, msg)
            .await
    }

    async fn try_delete_subtree(&self, path: &Path, msg: &str) -> Result<bool> {
        self.fetch().await?;
        let (parent_oid, mut objects, tree_oid) = self.claim_objects()?;
        let (new_root, objs) = remove_tree_entry(&mut objects, &tree_oid, path)?;
        self.push_with_cache(parent_oid, new_root, objs, msg).await
    }

    async fn try_create_dir(&self, path: &Path, msg: &str) -> Result<bool> {
        self.fetch().await?;
        let (parent_oid, mut objects, tree_oid) = self.claim_objects()?;
        let dav_path = path.join(".DAV");
        let blob_oid = hash_object(OBJ_BLOB, b"");
        objects.insert(blob_oid, b"".to_vec());
        let (new_root, mut pack_objects) = modify_tree_entry(
            &mut objects,
            &tree_oid,
            &dav_path,
            Some((gix_object::tree::EntryKind::Blob.into(), blob_oid)),
        )?;
        pack_objects.push((OBJ_BLOB, b"".to_vec()));
        self.push_with_cache(parent_oid, new_root, pack_objects, msg)
            .await
    }

    /// Commit a batch of writes and deletes using tree deltas.
    /// Writes are applied first, then deletes (preserving old semantics).
    async fn commit_and_push(
        &self,
        writes: &[(PathBuf, Vec<u8>)],
        deletes: &[PathBuf],
        message: &str,
    ) -> Result<bool> {
        self.fetch().await?;
        let (parent_oid, mut objects, mut tree_oid) = self.claim_objects()?;
        let mut pack_objects = Vec::new();

        for (path, data) in writes {
            let blob_oid = hash_object(OBJ_BLOB, data);
            objects.insert(blob_oid, data.clone());
            let (new_root, objs) = modify_tree_entry(
                &mut objects,
                &tree_oid,
                path,
                Some((gix_object::tree::EntryKind::Blob.into(), blob_oid)),
            )?;
            tree_oid = new_root;
            pack_objects.extend(objs);
            pack_objects.push((OBJ_BLOB, data.clone()));
        }

        for path in deletes {
            let (new_root, objs) = remove_tree_entry(&mut objects, &tree_oid, path)?;
            tree_oid = new_root;
            pack_objects.extend(objs);
        }

        self.push_with_cache(parent_oid, tree_oid, pack_objects, message)
            .await
    }
}
