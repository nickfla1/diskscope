use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use crate::analysis;
use crate::cleanup::Risk;
use crate::model::{Tree, category_of};
use crate::scan::{self, Progress};
use crate::treemap::{self, ColorMode, ColorSource, NO_ID};

fn write(p: &PathBuf, bytes: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![7u8; bytes]).unwrap();
}

/// Builds:
///   media/  movie.mp4 (2000) clip.mkv (1000)
///   proj/   package.json (10)  node_modules/dep/index.js (500)  src/main.rs (100)
///   notes.txt (50), big.tmp (3000)
pub(crate) fn fixture() -> (PathBuf, impl Drop) {
    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let root = std::env::temp_dir().join(format!("diskscope-test-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
    let _ = fs::remove_dir_all(&root);
    write(&root.join("media/movie.mp4"), 2000);
    write(&root.join("media/clip.mkv"), 1000);
    write(&root.join("proj/package.json"), 10);
    write(&root.join("proj/node_modules/dep/index.js"), 500);
    write(&root.join("proj/src/main.rs"), 100);
    write(&root.join("notes.txt"), 50);
    write(&root.join("big.tmp"), 3000);
    (root.clone(), Guard(root))
}

async fn scan_fixture(root: PathBuf) -> (Tree, Arc<Progress>) {
    let progress = Arc::new(Progress::default());
    let tree = scan::scan(root, crate::dirreader::system(false), progress.clone(), Arc::new(AtomicBool::new(false))).await.unwrap();
    (tree, progress)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_aggregates_sizes_and_counts() {
    let (root, _g) = fixture();
    let (tree, progress) = scan_fixture(root.clone()).await;
    assert_eq!(tree.node(0).size, 2000 + 1000 + 10 + 500 + 100 + 50 + 3000);
    assert_eq!(tree.node(0).files, 7);
    assert_eq!(progress.files.load(std::sync::atomic::Ordering::Relaxed), 7);

    let media = tree.find(&root.join("media").to_string_lossy()).unwrap();
    assert_eq!(tree.node(media).size, 3000);
    assert_eq!(tree.node(media).cat, 1, "media folder is dominated by video");
    assert_eq!(tree.node(media).dom_pct, 100);
    // paths round-trip and lookup is case-insensitive
    let dep = tree.find(&root.join("PROJ/Node_Modules/dep/index.js").to_string_lossy()).unwrap();
    assert!(tree.path(dep).ends_with("index.js"));
    // children ids are always greater than the parent id (the invariant every linear pass relies on)
    for (i, n) in tree.nodes.iter().enumerate().skip(1) {
        assert!((n.parent as usize) < i);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_updates_ancestors() {
    let (root, _g) = fixture();
    let (mut tree, _) = scan_fixture(root.clone()).await;
    let nm = tree.find(&root.join("proj/node_modules").to_string_lossy()).unwrap();
    let proj = tree.find(&root.join("proj").to_string_lossy()).unwrap();
    let before = tree.node(0).size;
    tree.remove(nm);
    assert_eq!(tree.node(0).size, before - 500);
    assert_eq!(tree.node(proj).size, 110);
    assert!(tree.is_gone(tree.find(&root.join("proj/node_modules/dep").to_string_lossy()).unwrap_or(nm)));
    assert!(tree.find(&root.join("proj/node_modules").to_string_lossy()).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cleanup_and_types() {
    let (root, _g) = fixture();
    let (tree, _) = scan_fixture(root.clone()).await;
    let a = analysis::analyze(&tree);
    let nm = tree.find(&root.join("proj/node_modules").to_string_lossy()).unwrap();
    // node_modules next to package.json is Safe; the file inside it is not listed separately
    let s = a.suggestions.iter().find(|s| s.node == nm);
    // (files < 64 KiB are skipped by the hot loop, but directories are always classified)
    assert!(s.is_some_and(|s| s.risk == Risk::Safe));
    assert_eq!(a.risk_of[tree.find(&root.join("proj/node_modules/dep/index.js").to_string_lossy()).unwrap() as usize], Risk::Safe as u8);
    let video = a.types.iter().find(|t| t.cat == 1).unwrap();
    assert_eq!((video.size, video.files), (3000, 2));
    assert_eq!(a.types[0].cat, 1, "types sorted by size, video is largest");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clusters_are_non_overlapping() {
    let (root, _g) = fixture();
    let (tree, _) = scan_fixture(root).await;
    let clusters = analysis::find_clusters(&tree, 500, &[]);
    assert!(!clusters.is_empty());
    let total: u64 = clusters.iter().map(|c| c.size).sum();
    assert!(total <= tree.node(0).size, "clusters must not double count");
    for (i, a) in clusters.iter().enumerate() {
        for b in &clusters[i + 1..] {
            let anc = |x: u32, y: u32| tree.ancestors(x).any(|p| p == y);
            // a folder cluster never contains another folder cluster
            if !a.loose && !b.loose {
                assert!(!anc(a.node, b.node) && !anc(b.node, a.node));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn treemap_covers_area_and_hit_test_matches_sizes() {
    let (root, _g) = fixture();
    let (tree, _) = scan_fixture(root).await;
    let cs = ColorSource { mode: ColorMode::Type, app_of: &[], risk_of: &[] };
    let counter = AtomicU64::new(1);
    let r = treemap::render(&tree, 0, 400, 300, &cs, &counter, 1).unwrap();
    let covered = r.ids.iter().filter(|&&i| i != NO_ID).count() as f64 / (400.0 * 300.0);
    assert!(covered > 0.98, "treemap should fill the area, got {covered}");
    // the largest file (3000 of 6660 bytes) should own roughly its share of the pixels
    let big = tree.nodes.iter().position(|n| n.size == 3000 && !n.is_dir()).unwrap() as u32;
    let share = r.ids.iter().filter(|&&i| i == big).count() as f64 / (400.0 * 300.0);
    assert!((share - 3000.0 / 6660.0).abs() < 0.03, "share {share}");
    // a superseded render is abandoned
    let stale = treemap::render(&tree, 0, 400, 300, &cs, &counter, 99);
    assert!(stale.is_none());
}

#[test]
fn categories() {
    assert_eq!(category_of("Movie.MP4"), 1);
    assert_eq!(category_of("a.tar.gz"), 5);
    assert_eq!(category_of("noext"), 0);
    assert_eq!(category_of("trailing."), 0);
    assert_eq!(category_of("weird.thisextensionistoolong"), 0);
}

// ---------------------------------------------------------------- deletion

mod deletion_tests {
    use super::*;
    use crate::deletion::{self, Permanent, Progress, Remover};
    use std::collections::HashSet;
    use std::sync::{Mutex, RwLock};

    /// In-memory adapter: records calls, fails for paths containing a marker.
    struct Fake {
        fail_on: Option<&'static str>,
        calls: Mutex<Vec<String>>,
    }
    impl Fake {
        fn new(fail_on: Option<&'static str>) -> Self {
            Fake { fail_on, calls: Mutex::new(Vec::new()) }
        }
    }
    impl Remover for Fake {
        fn remove(&self, path: &str, _is_dir: bool) -> Result<(), String> {
            self.calls.lock().unwrap().push(path.to_string());
            match self.fail_on {
                Some(m) if path.contains(m) => Err("locked".into()),
                _ => Ok(()),
            }
        }
        fn permanent(&self) -> bool {
            false
        }
    }

    fn id(tree: &Tree, root: &std::path::Path, rel: &str) -> u32 {
        tree.find(&root.join(rel).to_string_lossy()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plan_enforces_safety_rules() {
        let (root, _g) = fixture();
        let (tree, _) = scan_fixture(root.clone()).await;
        let (media, movie, notes) = (id(&tree, &root, "media"), id(&tree, &root, "media/movie.mp4"), id(&tree, &root, "notes.txt"));
        let mut risk = vec![0u8; tree.nodes.len()];
        risk[id(&tree, &root, "big.tmp") as usize] = 3; // Caution

        let p = deletion::plan(&tree, &[0, 9999, notes, notes, media, movie, id(&tree, &root, "big.tmp")], &risk);
        let refused: Vec<&str> = p.refused.iter().map(|r| r.reason).collect();
        assert_eq!(refused.len(), 3, "root, unknown, caution: {refused:?}");
        // duplicate `notes` collapsed, `movie` dropped because its parent `media` is selected
        let ids: HashSet<u32> = p.items.iter().map(|i| i.id).collect();
        assert_eq!(ids, HashSet::from([notes, media]));
        assert_eq!(p.total, 3000 + 50);
        assert_eq!(p.dirs, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_continues_after_failure_and_updates_tree() {
        let (root, _g) = fixture();
        let (tree, _) = scan_fixture(root.clone()).await;
        let (notes, media) = (id(&tree, &root, "notes.txt"), id(&tree, &root, "media"));
        let plan = deletion::plan(&tree, &[media, notes], &[]);
        let shared = RwLock::new(tree);
        let fake = Fake::new(Some("media"));
        let mut events = Vec::new();
        let out = deletion::run(&shared, plan, &fake, &AtomicBool::new(false), &mut |p| {
            events.push(match p {
                Progress::Started { total } => format!("start {total}"),
                Progress::Working { .. } => "work".into(),
                Progress::Removed { id } => format!("removed {id}"),
                Progress::Failed { error, .. } => format!("failed {error}"),
            })
        });
        assert_eq!((out.ok, out.failed.len(), out.cancelled), (1, 1, false));
        assert_eq!(events.first().unwrap(), "start 2");
        assert!(events.iter().any(|e| e == "failed locked"));
        let t = shared.read().unwrap();
        assert!(t.is_gone(notes), "removed item leaves the tree");
        assert!(!t.is_gone(media), "failed item stays");
        assert_eq!(t.node(0).size, 6660 - 50);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_honours_cancel_between_items() {
        let (root, _g) = fixture();
        let (tree, _) = scan_fixture(root.clone()).await;
        let ids = [id(&tree, &root, "notes.txt"), id(&tree, &root, "big.tmp"), id(&tree, &root, "media")];
        let plan = deletion::plan(&tree, &ids, &[]);
        let shared = RwLock::new(tree);
        let cancel = AtomicBool::new(false);
        let fake = Fake::new(None);
        let out = deletion::run(&shared, plan, &fake, &cancel, &mut |p| {
            if matches!(p, Progress::Removed { .. }) {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        assert!(out.cancelled);
        assert_eq!(out.ok, 1);
        assert_eq!(fake.calls.lock().unwrap().len(), 1, "no further removals after cancel");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanent_adapter_deletes_from_disk() {
        let (root, _g) = fixture();
        let (tree, _) = scan_fixture(root.clone()).await;
        let (media, notes) = (id(&tree, &root, "media"), id(&tree, &root, "notes.txt"));
        let plan = deletion::plan(&tree, &[media, notes], &[]);
        let shared = RwLock::new(tree);
        let out = deletion::run(&shared, plan, &Permanent, &AtomicBool::new(false), &mut |_| {});
        assert_eq!((out.ok, out.failed.len(), out.permanent), (2, 0, true));
        assert!(!root.join("media").exists() && !root.join("notes.txt").exists());
        assert!(root.join("proj/package.json").exists(), "unselected files untouched");
        assert_eq!(shared.read().unwrap().node(0).size, 6660 - 3000 - 50);
    }
}
