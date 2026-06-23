use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gix_object::{tree, Tree, TreeRefIter, WriteTo};

use crate::git::packfile::{hash_object, ObjectId, OBJ_BLOB, OBJ_TREE};

type ObjectList = Vec<(u8, Vec<u8>)>;

// ---------------------------------------------------------------------------
// Tree walking
// ---------------------------------------------------------------------------

/// Walk a tree and build both a path→OID index and a path→content map.
pub fn build_index_and_contents(
    objects: &HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
) -> Result<(HashMap<PathBuf, ObjectId>, HashMap<PathBuf, Vec<u8>>)> {
    let mut index = HashMap::new();
    let mut contents = HashMap::new();
    walk_rec(objects, tree_oid, Path::new(""), &mut index, &mut contents)?;
    Ok((index, contents))
}

fn walk_rec(
    objects: &HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
    base: &Path,
    index: &mut HashMap<PathBuf, ObjectId>,
    contents: &mut HashMap<PathBuf, Vec<u8>>,
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
            walk_rec(objects, &entry_oid, &path, index, contents)?;
        } else {
            index.insert(path.clone(), entry_oid);
            if let Some(data) = objects.get(&entry_oid) {
                contents.insert(path, data.to_vec());
            }
        }
    }
    Ok(())
}



// ---------------------------------------------------------------------------
// Tree building
// ---------------------------------------------------------------------------

fn build_tree_object(
    entries: &BTreeMap<String, (u32, ObjectId)>,
) -> Result<Vec<u8>> {
    let gix_tree = Tree {
        entries: entries
            .iter()
            .map(|(name, &(mode, oid))| {
                Ok(tree::Entry {
                    mode: tree::EntryMode::try_from(mode)
                        .map_err(|m| anyhow::anyhow!("invalid tree entry mode {:o}", m))?,
                    filename: name.as_bytes().into(),
                    oid,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let mut buf = Vec::new();
    gix_tree.write_to(&mut buf)?;
    Ok(buf)
}

pub fn build_trees(
    files: &HashMap<PathBuf, Vec<u8>>,
) -> Result<(ObjectId, ObjectList)> {
    let mut blob_oids: HashMap<PathBuf, ObjectId> = HashMap::new();
    let mut all_objects: Vec<(u8, Vec<u8>)> = Vec::new();

    for (path, data) in files {
        let oid = hash_object(OBJ_BLOB, data);
        blob_oids.insert(path.clone(), oid);
        all_objects.push((OBJ_BLOB, data.clone()));
    }

    let mut dir_files: HashMap<PathBuf, BTreeMap<String, (u32, ObjectId)>> = HashMap::new();

    for (path, oid) in &blob_oids {
        let parent = path.parent().unwrap_or(Path::new(""));
        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        dir_files
            .entry(parent.to_path_buf())
            .or_default()
            .insert(filename, (0o100644, *oid));
    }

    let all_dirs: Vec<PathBuf> = dir_files.keys().cloned().collect();
    let mut dirs_by_depth: Vec<PathBuf> = all_dirs;
    dirs_by_depth.sort_by_key(|b| std::cmp::Reverse(b.components().count()));

    let mut tree_oids: HashMap<PathBuf, ObjectId> = HashMap::new();

    for dir in &dirs_by_depth {
        let mut entries = dir_files.remove(dir).unwrap_or_default();

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
        .or_else(|| tree_oids.values().last())
        .copied()
        .context("no root tree generated")?;

    Ok((root_oid, all_objects))
}

// ---------------------------------------------------------------------------
// Commit building
// ---------------------------------------------------------------------------

pub fn build_commit(
    tree_oid: &ObjectId,
    parent_oid: Option<&ObjectId>,
    author_name: &str,
    author_email: &str,
    message: &str,
) -> Vec<u8> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let time = gix_object::date::Time {
        seconds: timestamp,
        offset: 0,
    };
    let sig = gix_actor::Signature {
        name: author_name.into(),
        email: author_email.into(),
        time,
    };

    let parents = parent_oid
        .map(|o| smallvec::smallvec![*o])
        .unwrap_or_default();

    let commit = gix_object::Commit {
        tree: *tree_oid,
        parents,
        author: sig.clone(),
        committer: sig,
        encoding: None,
        message: message.into(),
        extra_headers: vec![],
    };

    let mut buf = Vec::new();
    commit.write_to(&mut buf).expect("commit serialization");
    buf
}


