use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gix_object::{Tree, TreeRefIter, WriteTo, tree};

use crate::git::packfile::{OBJ_TREE, ObjectId, hash_object};

type ObjectList = Vec<(u8, Vec<u8>)>;
type IndexContents = (HashMap<PathBuf, ObjectId>, HashMap<PathBuf, Vec<u8>>);

// ---------------------------------------------------------------------------
// Tree walking
// ---------------------------------------------------------------------------

/// Walk a tree and build both a path→OID index and a path→content map.
pub fn build_index_and_contents(
    objects: &HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
) -> Result<IndexContents> {
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
// Tree manipulation (path walking, copy, remove)
// ---------------------------------------------------------------------------

/// Walk a tree path and return the entry (mode, oid) at the final component.
pub fn find_tree_entry(
    objects: &HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
    path: &Path,
) -> Result<Option<(tree::EntryMode, ObjectId)>> {
    if path.as_os_str().is_empty() {
        return Ok(Some((tree::EntryKind::Tree.into(), *tree_oid)));
    }

    let mut current = *tree_oid;
    let components: Vec<_> = path.components().collect();

    for (i, component) in components.iter().enumerate() {
        let name = component
            .as_os_str()
            .to_str()
            .with_context(|| format!("invalid path component in {:?}", path))?;

        let tree_data = objects
            .get(&current)
            .with_context(|| format!("missing tree object {} at {:?}", current, path))?;

        let mut found = false;
        for entry in TreeRefIter::from_bytes(tree_data, gix_hash::Kind::Sha1) {
            let entry = entry?;
            if entry.filename == name.as_bytes() {
                let entry_oid: ObjectId = entry.oid.to_owned();
                if i == components.len() - 1 {
                    return Ok(Some((entry.mode, entry_oid)));
                }
                if entry.mode.is_tree() {
                    current = entry_oid;
                    found = true;
                } else {
                    return Ok(None);
                }
                break;
            }
        }
        if !found {
            return Ok(None);
        }
    }

    Ok(Some((tree::EntryKind::Tree.into(), current)))
}

/// Recursively collect all tree OIDs reachable from the root tree.
pub fn collect_tree_oids(
    objects: &HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
) -> HashSet<ObjectId> {
    let mut result = HashSet::new();
    collect_tree_oids_rec(objects, tree_oid, &mut result);
    result
}

fn collect_tree_oids_rec(
    objects: &HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
    result: &mut HashSet<ObjectId>,
) {
    if !result.insert(*tree_oid) {
        return;
    }
    if let Some(tree_data) = objects.get(tree_oid) {
        let iter = TreeRefIter::from_bytes(tree_data, gix_hash::Kind::Sha1);
        for entry in iter.flatten() {
            if entry.mode.is_tree() {
                collect_tree_oids_rec(objects, &entry.oid.to_owned(), result);
            }
        }
    }
}

/// Recursively modify the tree entry at `path` under `tree_oid`.
///
/// If `entry` is `Some((mode, oid))`: insert or replace the leaf entry at
/// `path` with the given mode and oid.  Intermediate directories that don't
/// exist are created as empty trees.
///
/// If `entry` is `None`: remove the leaf entry at `path`.
///
/// Returns `(new_tree_oid, new_tree_objects)` bottom-up (deepest first).
pub fn modify_tree_entry(
    objects: &mut HashMap<ObjectId, Vec<u8>>,
    tree_oid: &ObjectId,
    path: &Path,
    entry: Option<(tree::EntryMode, ObjectId)>,
) -> Result<(ObjectId, ObjectList)> {
    let mut comps = path.components();
    let first = comps.next().context("empty path")?;
    let name = first
        .as_os_str()
        .to_str()
        .context("non-UTF-8 path component")?;
    let rest = comps.as_path();

    let tree_data = objects
        .get(tree_oid)
        .context("tree object not found")?
        .clone();

    if rest.as_os_str().is_empty() {
        // --- base case: upsert / delete name in this tree ---
        let mut new_entries: Vec<tree::Entry> = Vec::new();
        let mut found = false;

        for e in TreeRefIter::from_bytes(&tree_data, gix_hash::Kind::Sha1).flatten() {
            if e.filename == name.as_bytes() {
                found = true;
                if let Some((mode, oid)) = entry {
                    new_entries.push(tree::Entry {
                        mode,
                        filename: name.as_bytes().into(),
                        oid,
                    });
                }
            } else {
                new_entries.push(tree::Entry {
                    mode: e.mode,
                    filename: e.filename.to_vec().into(),
                    oid: e.oid.to_owned(),
                });
            }
        }

        if !found {
            match entry {
                Some((mode, oid)) => new_entries.push(tree::Entry {
                    mode,
                    filename: name.as_bytes().into(),
                    oid,
                }),
                None => bail!("entry {:?} not found", name),
            }
        }

        new_entries.sort();

        let tree = Tree {
            entries: new_entries,
        };
        let mut buf = Vec::new();
        tree.write_to(&mut buf)?;
        let new_oid = hash_object(OBJ_TREE, &buf);
        objects.insert(new_oid, buf.clone());
        Ok((new_oid, vec![(OBJ_TREE, buf)]))
    } else {
        // --- recursive case: walk into the child tree ---
        let mut entries: Vec<(tree::EntryMode, Vec<u8>, ObjectId)> = Vec::new();
        let mut child_oid = None;

        for e in TreeRefIter::from_bytes(&tree_data, gix_hash::Kind::Sha1).flatten() {
            if e.filename == name.as_bytes() {
                child_oid = Some(e.oid.to_owned());
            }
            entries.push((e.mode, e.filename.to_vec(), e.oid.to_owned()));
        }

        let child_oid = match child_oid {
            Some(oid) => oid,
            None => {
                // Intermediate path doesn't exist yet.
                if entry.is_some() {
                    let empty = Tree { entries: vec![] };
                    let mut buf = Vec::new();
                    empty.write_to(&mut buf)?;
                    let oid = hash_object(OBJ_TREE, &buf);
                    objects.insert(oid, buf);
                    entries.push((tree::EntryKind::Tree.into(), name.as_bytes().to_vec(), oid));
                    oid
                } else {
                    bail!("parent tree {:?} not found for {:?}", tree_oid, name);
                }
            }
        };

        let (new_child_oid, mut child_objects) =
            modify_tree_entry(objects, &child_oid, rest, entry)?;

        // Rebuild this tree with the updated child OID
        let mut new_entries: Vec<tree::Entry> = Vec::with_capacity(entries.len());
        for (mode, filename, oid) in &entries {
            if filename == name.as_bytes() {
                new_entries.push(tree::Entry {
                    mode: *mode,
                    filename: filename.clone().into(),
                    oid: new_child_oid,
                });
            } else {
                new_entries.push(tree::Entry {
                    mode: *mode,
                    filename: filename.clone().into(),
                    oid: *oid,
                });
            }
        }

        new_entries.sort();

        let tree = Tree {
            entries: new_entries,
        };
        let mut buf = Vec::new();
        tree.write_to(&mut buf)?;
        let new_oid = hash_object(OBJ_TREE, &buf);
        objects.insert(new_oid, buf.clone());

        let mut result = vec![(OBJ_TREE, buf)];
        result.append(&mut child_objects);
        Ok((new_oid, result))
    }
}

/// Copy a tree entry (file or subtree) from `src` to `dest`.
/// Creates intermediate directories if they don't exist.
/// Returns (new_root_oid, new_tree_objects_for_packfile).
pub fn copy_tree_entry(
    objects: &mut HashMap<ObjectId, Vec<u8>>,
    root_oid: &ObjectId,
    src: &Path,
    dest: &Path,
) -> Result<(ObjectId, ObjectList)> {
    let (mode, src_oid) =
        find_tree_entry(objects, root_oid, src)?.context("source path not found")?;
    modify_tree_entry(objects, root_oid, dest, Some((mode, src_oid)))
}

/// Remove the entry at `path` from the tree hierarchy.
/// Returns (new_root_oid, new_tree_objects_for_packfile).
pub fn remove_tree_entry(
    objects: &mut HashMap<ObjectId, Vec<u8>>,
    root_oid: &ObjectId,
    path: &Path,
) -> Result<(ObjectId, ObjectList)> {
    modify_tree_entry(objects, root_oid, path, None)
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
