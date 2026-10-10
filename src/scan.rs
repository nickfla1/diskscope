//! Asynchronous, multi-threaded directory scanner.
//!
//! Every directory is a tokio task. The blocking directory read (through the [`DirReader`] seam) runs on
//! the runtime's blocking pool, so many directories are read in parallel, which is what hides disk
//! latency. Finished directories are streamed over an unbounded channel to a single collector that owns
//! the arena. Workers reserve contiguous id ranges from an atomic counter, so no lock is ever taken on
//! the hot path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::SystemTime;

use tokio::sync::mpsc;

use crate::dirreader::{DirBuilder, DirReader, Raw};
use crate::model::{Node, Tree, flag};

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
}

struct Batch {
    parent: u32,
    first_id: u32,
    names: Vec<u8>,
    entries: Vec<Raw>,
    denied: bool,
}

struct Shared {
    next_id: AtomicU32,
    tx: mpsc::UnboundedSender<Batch>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    reader: Arc<dyn DirReader>,
}

fn read_dir_batch(sh: &Shared, parent: u32, path: &Path) -> Vec<(u32, PathBuf)> {
    let mut b = DirBuilder::new();
    if !sh.reader.read(path, &mut b) {
        sh.progress.errors.fetch_add(1, Relaxed);
        let _ = sh.tx.send(Batch { parent, first_id: 0, names: Vec::new(), entries: Vec::new(), denied: true });
        return Vec::new();
    }

    sh.progress.files.fetch_add(b.files, Relaxed);
    sh.progress.dirs.fetch_add(b.ndirs, Relaxed);
    sh.progress.bytes.fetch_add(b.bytes, Relaxed);
    if b.entries.is_empty() {
        return Vec::new();
    }
    let first_id = sh.next_id.fetch_add(b.entries.len() as u32, Relaxed);
    let subdirs = b.dirs.into_iter().map(|(idx, name)| (first_id + idx as u32, path.join(name))).collect();
    let _ = sh.tx.send(Batch { parent, first_id, names: b.names, entries: b.entries, denied: false });
    subdirs
}

fn spawn_dir(sh: Arc<Shared>, id: u32, path: PathBuf) {
    tokio::spawn(async move {
        if sh.cancel.load(Relaxed) {
            return;
        }
        let worker = sh.clone();
        let subdirs = tokio::task::spawn_blocking(move || read_dir_batch(&worker, id, &path))
            .await
            .unwrap_or_default();
        for (cid, p) in subdirs {
            spawn_dir(sh.clone(), cid, p);
        }
    });
}

/// Scan `root` through `reader` (use `platform::current().dir_reader(..)` for the real file system).
pub async fn scan(
    root: PathBuf,
    reader: Arc<dyn DirReader>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
) -> Result<Tree, String> {
    let mut root_str = root.to_string_lossy().to_string();
    if root_str.len() == 2 && root_str.ends_with(':') {
        root_str.push('\\');
    }
    let root = PathBuf::from(&root_str);

    let (tx, mut rx) = mpsc::unbounded_channel::<Batch>();
    let shared = Arc::new(Shared { next_id: AtomicU32::new(1), tx, progress, cancel: cancel.clone(), reader });
    spawn_dir(shared.clone(), 0, root);
    drop(shared); // the channel closes once the last directory task finishes

    let mut names: Vec<u8> = root_str.as_bytes().to_vec();
    let mut root_node = Node::EMPTY;
    root_node.flags = flag::DIR;
    root_node.name_len = root_str.len().min(u16::MAX as usize) as u16;
    let mut nodes = vec![root_node];

    while let Some(b) = rx.recv().await {
        let p = b.parent as usize;
        if b.denied {
            nodes[p].flags |= flag::DENIED;
            continue;
        }
        let first = b.first_id as usize;
        let end = first + b.entries.len();
        if nodes.len() < end {
            nodes.resize(end, Node::EMPTY);
        }
        if names.len() + b.names.len() >= u32::MAX as usize {
            return Err("name arena overflow (more than 4 GiB of names)".into());
        }
        let mut off = names.len() as u32;
        names.extend_from_slice(&b.names);
        for (i, e) in b.entries.iter().enumerate() {
            nodes[first + i] = Node {
                size: e.size,
                name_off: off,
                parent: b.parent,
                child_start: 0,
                child_count: 0,
                files: u32::from(e.flags & flag::DIR == 0),
                mtime: e.mtime,
                name_len: e.name_len,
                cat: e.cat,
                flags: e.flags,
                dom_pct: 0,
            };
            off += e.name_len as u32;
        }
        nodes[p].child_start = b.first_id;
        nodes[p].child_count = b.entries.len() as u32;
    }

    if cancel.load(Relaxed) {
        return Err("cancelled".into());
    }
    if nodes[0].flags & flag::DENIED != 0 {
        return Err(format!("'{root_str}' is not a readable directory"));
    }

    let mut tree = Tree { nodes, names, root_path: root_str, scanned_at: SystemTime::now() };
    tree.nodes.shrink_to_fit();
    tree.names.shrink_to_fit();
    // Aggregation is pure CPU work; keep it off the async workers.
    let tree = tokio::task::spawn_blocking(move || {
        crate::analysis::finalize(&mut tree);
        tree
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(tree)
}
