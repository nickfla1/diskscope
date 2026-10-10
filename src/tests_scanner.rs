//! Scanner tests through the DirReader seam: an in-memory fake adapter makes denied directories,
//! cancellation, cloud placeholders and large trees deterministic; one parity test checks that the two
//! production adapters agree on a real directory. Paths are built with `PathBuf`, so the tests behave the
//! same on every OS.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use crate::dirreader::{ATTR_RECALL_ON_DATA_ACCESS, DirBuilder, DirReader};
use crate::model::{Tree, flag};
use crate::scan::{self, Progress};

pub(crate) struct FakeEntry {
    name: String,
    is_dir: bool,
    size: u64,
    attrs: u32,
}

/// In-memory adapter. `on_read(n)` runs before the n-th directory read (used to inject cancellation).
#[derive(Default)]
pub(crate) struct FakeFs {
    pub(crate) dirs: HashMap<PathBuf, Vec<FakeEntry>>,
    pub(crate) denied: HashSet<PathBuf>,
    pub(crate) reads: AtomicUsize,
    pub(crate) on_read: Option<Box<dyn Fn(usize) + Send + Sync>>,
}

impl FakeFs {
    /// A fake disk whose root directory exists and is empty.
    pub(crate) fn with_root() -> Self {
        let mut fs = FakeFs::default();
        fs.dirs.insert(fake_root(), vec![]);
        fs
    }
    pub(crate) fn add(&mut self, dir: &Path, name: &str, is_dir: bool, size: u64) {
        self.add_attr(dir, name, is_dir, size, 0);
    }
    pub(crate) fn add_attr(&mut self, dir: &Path, name: &str, is_dir: bool, size: u64, attrs: u32) {
        self.dirs.entry(dir.to_path_buf()).or_default().push(FakeEntry { name: name.into(), is_dir, size, attrs });
        if is_dir {
            self.dirs.entry(dir.join(name)).or_default();
        }
    }
}

impl DirReader for FakeFs {
    fn read(&self, dir: &Path, out: &mut DirBuilder) -> bool {
        let n = self.reads.fetch_add(1, Relaxed);
        if let Some(f) = &self.on_read {
            f(n);
        }
        if self.denied.contains(dir) {
            return false;
        }
        let Some(entries) = self.dirs.get(dir) else { return false };
        for e in entries {
            out.push(&e.name, e.is_dir, e.size, 1_700_000_000, e.attrs);
        }
        true
    }
}

/// Root of the fake disk. It is never touched on the real file system.
pub(crate) fn fake_root() -> PathBuf {
    std::env::temp_dir().join("diskscope-fake-root")
}

pub(crate) fn path_of(parts: &[&str]) -> PathBuf {
    parts.iter().fold(fake_root(), |p, s| p.join(s))
}

async fn run(fs: FakeFs, root: PathBuf) -> (Result<Tree, String>, Arc<Progress>) {
    let progress = Arc::new(Progress::default());
    let r = scan::scan(root, Arc::new(fs), progress.clone(), Arc::new(AtomicBool::new(false))).await;
    (r, progress)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_directories_aggregate_deterministically() {
    let mut fs = FakeFs::with_root();
    let root = fake_root();
    for d in 0..200 {
        let dir = format!("d{d}");
        fs.add(&root, &dir, true, 0);
        let full = root.join(&dir);
        for f in 0..30 {
            fs.add(&full, &format!("f{f}.mp4"), false, 100 + f);
        }
        fs.add(&full, "sub", true, 0);
        fs.add(&full.join("sub"), "deep.txt", false, 7);
    }
    let expected: u64 = 200 * ((0..30).map(|f| 100 + f).sum::<u64>() + 7);
    let (tree, progress) = run(fs, root).await;
    let tree = tree.unwrap();
    assert_eq!(tree.node(0).size, expected);
    assert_eq!(tree.node(0).files, 200 * 31);
    assert_eq!(progress.files.load(Relaxed), 200 * 31);
    assert_eq!(progress.dirs.load(Relaxed), 400);
    for (i, n) in tree.nodes.iter().enumerate().skip(1) {
        assert!((n.parent as usize) < i, "child id must exceed parent id");
    }
    let deep = tree.find(&path_of(&["d199", "sub", "deep.txt"]).to_string_lossy()).unwrap();
    assert_eq!(tree.node(deep).size, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn denied_directory_is_flagged_and_siblings_still_scan() {
    let mut fs = FakeFs::with_root();
    let root = fake_root();
    fs.add(&root, "ok", true, 0);
    fs.add(&root, "locked", true, 0);
    fs.add(&root.join("ok"), "a.bin", false, 50);
    fs.add(&root.join("locked"), "secret.bin", false, 999);
    fs.denied.insert(root.join("locked"));
    let (tree, progress) = run(fs, root.clone()).await;
    let tree = tree.unwrap();
    let locked = tree.find(&root.join("locked").to_string_lossy()).unwrap();
    assert!(tree.node(locked).flags & flag::DENIED != 0);
    assert_eq!(tree.node(0).size, 50);
    assert_eq!(progress.errors.load(Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_root_is_an_error() {
    let (r, _) = run(FakeFs::default(), path_of(&["nowhere"])).await;
    assert!(r.err().unwrap().contains("not a readable directory"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_stops_the_scan() {
    let mut fs = FakeFs::with_root();
    let root = fake_root();
    for d in 0..500 {
        fs.add(&root, &format!("d{d}"), true, 0);
        fs.add(&root.join(format!("d{d}")), "x.bin", false, 1);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    fs.on_read = Some(Box::new(move |n| {
        if n == 3 {
            c2.store(true, Relaxed);
        }
    }));
    let r = scan::scan(root, Arc::new(fs), Arc::new(Progress::default()), cancel).await;
    assert_eq!(r.err().as_deref(), Some("cancelled"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloud_placeholders_count_as_zero_and_oversized_names_are_skipped() {
    let mut fs = FakeFs::with_root();
    let root = fake_root();
    fs.add_attr(&root, "online-only.docx", false, 5_000_000, ATTR_RECALL_ON_DATA_ACCESS);
    fs.add(&root, "real.docx", false, 10);
    fs.add(&root, &"x".repeat(70_000), false, 123);
    let (tree, _) = run(fs, root.clone()).await;
    let tree = tree.unwrap();
    assert_eq!(tree.node(0).size, 10);
    assert_eq!(tree.node(0).files, 2, "oversized name dropped, placeholder kept");
    let c = tree.find(&root.join("online-only.docx").to_string_lossy()).unwrap();
    assert!(tree.node(c).flags & flag::CLOUD != 0);
}

/// The two production adapters must agree on a real directory tree.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn win32_and_std_adapters_agree() {
    use crate::dirreader::{StdReader, Win32Reader};
    let (root, _g) = crate::tests::fixture();
    let snapshot = |tree: &Tree| {
        (1..tree.nodes.len() as u32)
            .map(|i| {
                let n = tree.node(i);
                (tree.path(i), (n.size, n.mtime, n.is_dir(), n.cat, n.files))
            })
            .collect::<HashMap<_, _>>()
    };
    let go = |reader: Arc<dyn DirReader>, root: PathBuf| async move {
        scan::scan(root, reader, Arc::new(Progress::default()), Arc::new(AtomicBool::new(false))).await.unwrap()
    };
    let a = go(Arc::new(Win32Reader { on_disk: false }), root.clone()).await;
    let b = go(Arc::new(StdReader::default()), root).await;
    assert_eq!(snapshot(&a), snapshot(&b));
    assert_eq!(a.node(0).size, b.node(0).size);
}
