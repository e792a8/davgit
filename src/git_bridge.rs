use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use git2::{Oid, Repository, Signature};
use tempfile::TempDir;
use tracing::info;

pub struct GitRepo {
    pub repo: Mutex<Repository>,
    repo_path: PathBuf,
    _temp_dir: TempDir,
    pub branch: String,
    _remote_url: String,
    author_name: String,
    author_email: String,
    password: Option<String>,
    ssh_key: Option<String>,
    last_fetch: Mutex<Instant>,
}

impl GitRepo {
    pub fn init_and_fetch(
        remote_url: &str,
        branch: &str,
        ssh_key: Option<&str>,
        password: Option<&str>,
        author_name: &str,
        author_email: &str,
    ) -> Result<Self> {
        let temp_dir = TempDir::with_prefix("davgit-")?;
        let repo_path = temp_dir.path().join("repo");

        let repo = Repository::init_bare(&repo_path).context("failed to init bare repository")?;

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

        let repo_path = repo.path().to_path_buf();

        let git_repo = GitRepo {
            repo: Mutex::new(repo),
            repo_path,
            _temp_dir: temp_dir,
            branch: branch.to_owned(),
            _remote_url: remote_url.to_owned(),
            author_name,
            author_email,
            password: password.map(|s| s.to_owned()),
            ssh_key: ssh_key.map(|s| s.to_owned()),
            last_fetch: Mutex::new(Instant::now()),
        };

        {
            let repo = git_repo.repo.lock().unwrap();
            git_repo.ensure_remote(&repo)?;
            git_repo.fetch_locked(&repo)?;
        }

        Ok(git_repo)
    }

    /// Re-fetch from remote and return the latest tree contents as a HashMap.
    /// Returns `None` when the remote has no commits yet.
    /// Skips fetching if called less than 3 seconds after the last fetch.
    pub fn refresh_tree(&self) -> Result<Option<HashMap<PathBuf, Vec<u8>>>> {
        {
            let last = self.last_fetch.lock().unwrap();
            if last.elapsed() < Duration::from_secs(3) {
                return Ok(None);
            }
        }

        let repo = self.repo.lock().unwrap();
        self.fetch_locked(&repo)?;

        let tree_id = match self.resolve_head_tree_locked(&repo)? {
            Some(id) => id,
            None => return Ok(None),
        };

        let map = self.read_tree_locked(&repo, tree_id)?;
        *self.last_fetch.lock().unwrap() = Instant::now();
        Ok(Some(map))
    }

    fn ensure_remote(&self, repo: &Repository) -> Result<()> {
        if repo.find_remote("origin").is_ok() {
            return Ok(());
        }
        repo.remote("origin", &self._remote_url)
            .context("failed to add remote")?;
        Ok(())
    }

    fn fetch_locked(&self, _repo: &Repository) -> Result<()> {
        for attempt in 1..=3 {
            let mut cmd = Command::new("git");
            cmd.args([
                "-C",
                self.repo_path.to_str().unwrap(),
                "fetch",
                "origin",
                &self.branch,
            ]);
            self.configure_ssh(&mut cmd);

            match cmd.output() {
                Ok(out) if out.status.success() => {
                    *self.last_fetch.lock().unwrap() = Instant::now();
                    info!("fetch succeeded (attempt {})", attempt);
                    return Ok(());
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if attempt < 3 {
                        tracing::warn!(
                            "fetch attempt {} failed (will retry): {}",
                            attempt,
                            stderr.trim()
                        );
                    } else {
                        tracing::warn!("fetch failed after 3 attempts: {}", stderr.trim());
                        return Ok(()); // non-fatal: proceed with stale state
                    }
                }
                Err(e) => {
                    if attempt < 3 {
                        tracing::warn!("fetch attempt {} error (will retry): {}", attempt, e);
                    } else {
                        tracing::warn!("fetch failed after 3 attempts: {}", e);
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Push the local branch to remote. Returns Ok(true) on success,
    /// Ok(false) on non-fast-forward, Err on other errors.
    fn push(&self) -> Result<bool> {
        let refspec = format!("refs/heads/{}:refs/heads/{}", self.branch, self.branch);

        for attempt in 1..=3 {
            let mut cmd = Command::new("git");
            cmd.args([
                "-C",
                self.repo_path.to_str().unwrap(),
                "push",
                "origin",
                &refspec,
            ]);
            self.configure_ssh(&mut cmd);

            match cmd.output() {
                Ok(out) if out.status.success() => {
                    info!("pushed {} to remote (attempt {})", self.branch, attempt);
                    return Ok(true);
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if stderr.contains("non-fast-forward") || stderr.contains("fetch first") {
                        tracing::warn!("non-fast-forward, will retry");
                        return Ok(false);
                    }
                    if attempt < 3 {
                        tracing::warn!(
                            "push attempt {} failed (will retry): {}",
                            attempt,
                            stderr.trim()
                        );
                    } else {
                        return Err(anyhow::anyhow!(
                            "push failed after 3 attempts: {}",
                            stderr.trim()
                        ));
                    }
                }
                Err(e) => {
                    if attempt < 3 {
                        tracing::warn!("push attempt {} error (will retry): {}", attempt, e);
                    } else {
                        return Err(anyhow::anyhow!("push failed after 3 attempts: {}", e));
                    }
                }
            }
        }
        Err(anyhow::anyhow!("push failed after 3 attempts"))
    }

    /// Configure environment for SSH-based git commands.
    fn configure_ssh(&self, cmd: &mut Command) {
        let mut ssh = String::from("ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10");
        if let Some(ref key_path) = self.ssh_key {
            ssh.push_str(" -i ");
            ssh.push_str(key_path);
        }
        cmd.env("GIT_SSH_COMMAND", &ssh);
    }

    pub fn resolve_head_tree(&self) -> Result<Option<Oid>> {
        let repo = self.repo.lock().unwrap();
        self.resolve_head_tree_locked(&repo)
    }

    fn resolve_head_tree_locked(&self, repo: &Repository) -> Result<Option<Oid>> {
        let remote_ref = format!("refs/remotes/origin/{}", self.branch);
        let result = repo.find_reference(&remote_ref);
        match result {
            Ok(refr) => match refr.peel_to_commit() {
                Ok(c) => {
                    let tree = c.tree()?;
                    Ok(Some(tree.id()))
                }
                Err(_) => Ok(None),
            },
            Err(_) => Ok(None),
        }
    }

    pub fn read_tree_to_memory(&self, tree_id: Oid) -> Result<HashMap<PathBuf, Vec<u8>>> {
        let mut map = HashMap::new();
        let repo = self.repo.lock().unwrap();
        let tree = repo.find_tree(tree_id)?;
        walk_tree(&repo, &tree, PathBuf::new(), &mut map)?;
        Ok(map)
    }

    fn read_tree_locked(
        &self,
        repo: &Repository,
        tree_id: Oid,
    ) -> Result<HashMap<PathBuf, Vec<u8>>> {
        let mut map = HashMap::new();
        let tree = repo.find_tree(tree_id)?;
        walk_tree(repo, &tree, PathBuf::new(), &mut map)?;
        Ok(map)
    }

    pub fn write_path(&self, path: &Path, data: &[u8]) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let data = data.to_vec();
        let components: Vec<&str> = path_str.split('/').filter(|s| !s.is_empty()).collect();
        let msg = format!("update {}", path_str);

        for _ in 0..3 {
            let blob_id = {
                let repo = self.repo.lock().unwrap();
                self.commit_locked(&repo, &components, Some(&data), &msg)?
            };
            if self.push()? {
                return Ok(());
            }
            // NonFastForward: drop blob reference, loop to retry
            let _ = blob_id;
        }
        Err(anyhow::anyhow!("push failed after 3 attempts"))
    }

    pub fn delete_path(&self, path: &Path) -> Result<()> {
        let path_str = path.to_str().context("invalid path")?.to_owned();
        let components: Vec<&str> = path_str.split('/').filter(|s| !s.is_empty()).collect();
        let msg = format!("delete {}", path_str);

        for _ in 0..3 {
            {
                let repo = self.repo.lock().unwrap();
                self.commit_locked(&repo, &components, None, &msg)?;
            }
            if self.push()? {
                return Ok(());
            }
        }
        Err(anyhow::anyhow!("push failed after 3 attempts"))
    }

    /// Apply multiple writes and deletes in a single commit+push cycle.
    /// Each operation builds on the previous tree, so all changes are
    /// reflected in one tree object, one commit, and one push.
    pub fn batch_paths(&self, writes: &[(PathBuf, Vec<u8>)], deletes: &[PathBuf]) -> Result<()> {
        for _ in 0..3 {
            {
                let guard = self.repo.lock().unwrap();
                let r: &Repository = &guard;
                self.fetch_locked(r)?;

                let mut tree = self.find_current_tree(r);
                for (path, data) in writes {
                    let path_str = path.to_str().context("invalid path")?;
                    let parts: Vec<&str> = path_str.split('/').filter(|s| !s.is_empty()).collect();
                    let blob_id = r.blob(data).context("failed to write blob")?;
                    tree = Some(
                        build_tree(r, tree, &parts, Some(blob_id))
                            .context("failed to build tree")?,
                    );
                }
                for path in deletes {
                    let path_str = path.to_str().context("invalid path")?;
                    let parts: Vec<&str> = path_str.split('/').filter(|s| !s.is_empty()).collect();
                    tree = Some(build_tree(r, tree, &parts, None).context("failed to build tree")?);
                }

                if let Some(tree_id) = tree {
                    self.create_commit_inner(r, tree_id, "batch update")?;
                }
            }
            if self.push()? {
                return Ok(());
            }
        }
        Err(anyhow::anyhow!("batch commit failed after 3 attempts"))
    }

    /// Fetch, build tree, commit. Returns the blob Oid (if any) for drop-ordering.
    /// Holds repo lock — caller must drop the lock before push.
    fn commit_locked(
        &self,
        repo: &Repository,
        components: &[&str],
        data: Option<&[u8]>,
        msg: &str,
    ) -> Result<Option<Oid>> {
        self.fetch_locked(repo)?;

        let blob_id = data
            .map(|d| repo.blob(d).context("failed to write blob"))
            .transpose()?;
        let current_tree = self.find_current_tree(repo);
        let new_tree_id =
            build_tree(repo, current_tree, components, blob_id).context("failed to build tree")?;
        self.create_commit_inner(repo, new_tree_id, msg)?;
        Ok(blob_id)
    }

    fn find_current_tree(&self, repo: &Repository) -> Option<Oid> {
        let remote_ref = format!("refs/remotes/origin/{}", self.branch);
        repo.find_reference(&remote_ref)
            .ok()
            .and_then(|refr| refr.peel_to_commit().ok())
            .and_then(|c| c.tree().ok())
            .map(|t| t.id())
    }

    fn create_commit_inner(&self, repo: &Repository, tree_id: Oid, message: &str) -> Result<()> {
        let branch_ref = format!("refs/heads/{}", self.branch);
        let remote_ref = format!("refs/remotes/origin/{}", self.branch);

        let parent_ids: Vec<Oid> = match repo.find_reference(&remote_ref) {
            Ok(refr) => {
                if let Ok(commit) = refr.peel_to_commit() {
                    vec![commit.id()]
                } else {
                    vec![]
                }
            }
            Err(_) => vec![],
        };

        let author = Signature::now(&self.author_name, &self.author_email)
            .context("failed to create author signature")?;
        let committer = Signature::now(&self.author_name, &self.author_email)
            .context("failed to create committer signature")?;

        let tree = repo.find_tree(tree_id)?;
        let parents: Vec<git2::Commit> = parent_ids
            .iter()
            .filter_map(|id| repo.find_commit(*id).ok())
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let commit_id = repo
            .commit(
                Some(&branch_ref),
                &author,
                &committer,
                message,
                &tree,
                &parent_refs,
            )
            .context("failed to create commit")?;

        let obj = repo.find_object(commit_id, None)?;
        repo.reference(&remote_ref, obj.id(), true, message)?;

        Ok(())
    }
}

fn walk_tree(
    repo: &Repository,
    tree: &git2::Tree,
    base: PathBuf,
    map: &mut HashMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    for entry in tree.iter() {
        let name = entry.name().unwrap_or("");
        let path = base.join(name);

        match entry.kind() {
            Some(git2::ObjectType::Tree) => {
                if let Ok(subtree) = entry.to_object(repo)?.peel_to_tree() {
                    walk_tree(repo, &subtree, path, map)?;
                }
            }
            Some(git2::ObjectType::Blob) => {
                if let Ok(blob) = entry.to_object(repo)?.peel_to_blob() {
                    map.insert(path, blob.content().to_vec());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn build_tree(
    repo: &Repository,
    existing_tree: Option<Oid>,
    components: &[&str],
    blob_id: Option<Oid>,
) -> Result<Oid> {
    let source_tree = existing_tree.and_then(|tid| repo.find_tree(tid).ok());
    let mut tb = repo
        .treebuilder(source_tree.as_ref())
        .context("failed to create treebuilder")?;
    let name = components[0];

    if components.len() == 1 {
        if let Some(bid) = blob_id {
            tb.insert(name, bid, 0o100644)?;
        } else {
            tb.remove(name)?;
        }
    } else {
        let subtree_id = source_tree.as_ref().and_then(|tree| {
            tree.get_name(name)
                .and_then(|entry| entry.to_object(repo).ok())
                .and_then(|obj| obj.peel_to_tree().ok())
                .map(|t| t.id())
        });

        let new_subtree_id = build_tree(repo, subtree_id, &components[1..], blob_id)?;
        tb.insert(name, new_subtree_id, 0o040000)?;
    }

    let tree_id = tb.write()?;
    Ok(tree_id)
}
