//! Persistent cache of scan results in `~/.diskuse.json`.
//!
//! The cache holds one tree per scanned root. A root that lies inside an
//! already cached root is stored as part of that tree instead.

use crate::scan::{self, Node};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const VERSION: u32 = 1;

/// Serializes load-modify-save cycles from different threads.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    version: u32,
    roots: HashMap<String, Node>,
}

fn cache_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".diskuse.json"))
}

fn read() -> Cache {
    let Some(path) = cache_path() else {
        return Cache::default();
    };
    let Ok(file) = fs::File::open(path) else {
        return Cache::default();
    };
    match serde_json::from_reader::<_, Cache>(BufReader::new(file)) {
        Ok(cache) if cache.version == VERSION => cache,
        _ => Cache::default(),
    }
}

fn write(cache: &Cache) -> io::Result<()> {
    let path = cache_path().ok_or_else(|| io::Error::other("HOME is not set"))?;
    // Write to a temporary file first so a crash never leaves a broken cache.
    let tmp = path.with_extension("json.tmp");
    let mut out = BufWriter::new(fs::File::create(&tmp)?);
    serde_json::to_writer(&mut out, cache)?;
    out.flush()?;
    fs::rename(tmp, path)
}

/// Returns the cached tree for `path`, taken from its own entry or from
/// the entry of a cached ancestor.
pub fn load(path: &Path) -> Option<Node> {
    let _guard = LOCK.lock().unwrap();
    let cache = read();
    cache.roots.iter().find_map(|(root, node)| {
        let root = Path::new(root);
        if path.starts_with(root) {
            scan::find(node, root, path).cloned()
        } else {
            None
        }
    })
}

/// Stores the tree scanned at `path`.
pub fn store(path: &Path, node: Node) -> io::Result<()> {
    let _guard = LOCK.lock().unwrap();
    let mut cache = read();
    cache.version = VERSION;

    let ancestor = cache
        .roots
        .keys()
        .find(|root| path.starts_with(root) && path != Path::new(root))
        .cloned();
    if let Some(root) = ancestor {
        let tree = cache.roots.get_mut(&root).unwrap();
        if scan::find(tree, Path::new(&root), path).is_some() {
            scan::replace(tree, Path::new(&root), path, node);
            return write(&cache);
        }
    }
    // Roots inside the new one are now covered by it.
    cache
        .roots
        .retain(|root, _| !Path::new(root).starts_with(path));
    cache
        .roots
        .insert(path.to_string_lossy().into_owned(), node);
    write(&cache)
}
