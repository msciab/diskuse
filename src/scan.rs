use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

/// A directory in the scanned tree. Files are not stored individually,
/// only their total size, to keep memory low when scanning a whole disk.
/// Field names are shortened in the cache file to keep it small.
#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    #[serde(rename = "n")]
    pub name: String,
    /// Total size of this directory, including all subdirectories.
    #[serde(rename = "s")]
    pub size: u64,
    /// Size of the files directly inside this directory.
    #[serde(rename = "f")]
    pub files_size: u64,
    #[serde(rename = "k")]
    pub files_count: u64,
    /// Modification time of the directory itself, in nanoseconds since the
    /// epoch (0 if unknown). It changes when entries are added, removed or
    /// renamed, so an unchanged value means the listing can be reused.
    #[serde(rename = "m")]
    pub mtime: u64,
    /// Subdirectories, sorted by size (largest first).
    #[serde(rename = "c", default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
    /// The directory (or part of it) could not be read.
    #[serde(rename = "e", default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
    /// The directory is the root of a mounted volume. Recomputed on every
    /// scan, so it is not stored in the cache.
    #[serde(skip)]
    pub mount: bool,
}

impl Node {
    fn new(name: String, mtime: u64) -> Self {
        Node {
            name,
            size: 0,
            files_size: 0,
            files_count: 0,
            mtime,
            children: Vec::new(),
            error: false,
            mount: false,
        }
    }
}

/// Pseudo filesystems that make no sense to measure.
const SKIP: &[&str] = &["/proc", "/sys", "/dev"];

pub struct Scanner {
    /// Number of directories scanned so far, for progress display.
    pub dirs: AtomicU64,
    /// Number of those whose listing was reused from the cache.
    pub reused: AtomicU64,
    /// Directories already visited, by (device, inode), so that the same
    /// directory reachable from two paths (e.g. macOS firmlinks) counts once.
    visited: Mutex<HashSet<(u64, u64)>>,
}

impl Scanner {
    pub fn new() -> Self {
        Scanner {
            dirs: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            visited: Mutex::new(HashSet::new()),
        }
    }

    /// Scans `path`. If `cached` is the result of a previous scan of the same
    /// path, directories whose modification time did not change are not
    /// read again: their file sizes are reused, and only their
    /// subdirectories are checked.
    pub fn scan(&self, path: &Path, cached: Option<&Node>) -> Node {
        match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => {
                self.first_visit(&meta);
                self.scan_dir(path, &meta, cached)
            }
            _ => {
                let mut node = Node::new(file_name(path), 0);
                node.error = true;
                node
            }
        }
    }

    fn scan_dir(&self, path: &Path, meta: &fs::Metadata, cached: Option<&Node>) -> Node {
        self.dirs.fetch_add(1, Ordering::Relaxed);
        let mut node = Node::new(file_name(path), mtime(meta));

        if let Some(old) = cached.filter(|c| c.mtime == node.mtime && node.mtime != 0 && !c.error) {
            self.reused.fetch_add(1, Ordering::Relaxed);
            node.files_size = old.files_size;
            node.files_count = old.files_count;
            let mut subdirs = Vec::with_capacity(old.children.len());
            for child in &old.children {
                let sub = path.join(&child.name);
                match fs::symlink_metadata(&sub) {
                    Ok(m) if m.is_dir() => {
                        if self.first_visit(&m) {
                            subdirs.push((sub, m, Some(child)));
                        }
                    }
                    _ => node.error = true,
                }
            }
            return self.finish(node, meta, subdirs);
        }

        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(_) => {
                node.error = true;
                return node;
            }
        };
        let old_children: HashMap<&str, &Node> = cached
            .map(|c| c.children.iter().map(|n| (n.name.as_str(), n)).collect())
            .unwrap_or_default();

        let mut subdirs = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                node.error = true;
                continue;
            };
            // DirEntry::metadata does not follow symlinks.
            let Ok(meta) = entry.metadata() else {
                node.error = true;
                continue;
            };
            if meta.is_dir() {
                let sub = entry.path();
                if !SKIP.iter().any(|s| sub == Path::new(s)) && self.first_visit(&meta) {
                    let old = old_children
                        .get(entry.file_name().to_string_lossy().as_ref())
                        .copied();
                    subdirs.push((sub, meta, old));
                }
            } else {
                node.files_size += disk_size(&meta);
                node.files_count += 1;
            }
        }
        self.finish(node, meta, subdirs)
    }

    fn finish(
        &self,
        mut node: Node,
        meta: &fs::Metadata,
        subdirs: Vec<(PathBuf, fs::Metadata, Option<&Node>)>,
    ) -> Node {
        node.children = subdirs
            .par_iter()
            .map(|(p, m, old)| {
                let mut child = self.scan_dir(p, m, *old);
                child.mount = device(m) != device(meta);
                child
            })
            .collect();
        node.children.sort_by(|a, b| b.size.cmp(&a.size));
        node.size = node.files_size + node.children.iter().map(|c| c.size).sum::<u64>();
        node
    }

    #[cfg(unix)]
    fn first_visit(&self, meta: &fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        self.visited
            .lock()
            .unwrap()
            .insert((meta.dev(), meta.ino()))
    }

    #[cfg(not(unix))]
    fn first_visit(&self, _meta: &fs::Metadata) -> bool {
        true
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn device(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(not(unix))]
fn device(_meta: &fs::Metadata) -> u64 {
    0
}

/// Space of the volume containing a path.
#[derive(Clone, Copy)]
pub struct Space {
    pub used: u64,
    pub free: u64,
}

/// Returns used and free space of the volume containing `path`, as `df`
/// reports them (free is the space available to unprivileged users).
#[cfg(unix)]
pub fn volume_space(path: &Path) -> Option<Space> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let block = st.f_frsize as u64;
    let total = st.f_blocks as u64 * block;
    let used = total.saturating_sub(st.f_bfree as u64 * block);
    let free = st.f_bavail as u64 * block;
    Some(Space { used, free })
}

#[cfg(not(unix))]
pub fn volume_space(_path: &Path) -> Option<Space> {
    None
}

fn mtime(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Space actually allocated on disk (like `du`), falling back to the
/// apparent length where block counts are not available.
#[cfg(unix)]
fn disk_size(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn disk_size(meta: &fs::Metadata) -> u64 {
    meta.len()
}

/// Finds the subtree at `target`. `node_path` is the full path of `node`.
pub fn find<'a>(node: &'a Node, node_path: &Path, target: &Path) -> Option<&'a Node> {
    if node_path == target {
        return Some(node);
    }
    node.children.iter().find_map(|child| {
        let child_path = node_path.join(&child.name);
        if target.starts_with(&child_path) {
            find(child, &child_path, target)
        } else {
            None
        }
    })
}

/// Replaces the subtree at `target` with `new`, updating the sizes of all
/// ancestors. `node_path` is the full path of `node`.
/// Returns false if `target` is not in the tree.
pub fn replace(node: &mut Node, node_path: &Path, target: &Path, mut new: Node) -> bool {
    if node_path == target {
        new.name = std::mem::take(&mut node.name);
        *node = new;
        return true;
    }
    for i in 0..node.children.len() {
        let child_path = node_path.join(&node.children[i].name);
        if target.starts_with(&child_path) {
            if !replace(&mut node.children[i], &child_path, target, new) {
                return false;
            }
            node.children.sort_by(|a, b| b.size.cmp(&a.size));
            node.size = node.files_size + node.children.iter().map(|c| c.size).sum::<u64>();
            return true;
        }
    }
    false
}

pub fn human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
