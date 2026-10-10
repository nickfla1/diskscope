//! Session tests, fully headless: a fake disk (DirReader) in, a fake Remover for deletion, no window and
//! no real file system. They exercise the whole pipeline: scan -> analysis -> views -> render -> delete.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use crate::deletion::Remover;
use crate::platform::windows::Windows;
use crate::session::{Change, Session};
use crate::tests_scanner::{FakeFs, fake_root, path_of};
use crate::treemap::ColorMode;
use crate::views::{CleanupFilter, LeftoverFilter};

fn session() -> Session {
    Session::new(tokio::runtime::Handle::current(), &Windows, Arc::new(|| {}))
}

/// Run until nothing is in flight, collecting every change.
fn settle(s: &mut Session) -> Vec<Change> {
    let mut all = Vec::new();
    while s.busy() {
        all.extend(s.wait());
    }
    all
}

/// project/{package.json, node_modules/dep/index.js (50 MB), src/main.rs}, media/movie.mp4 (200 MB),
/// notes/todo.txt
fn fake_disk() -> FakeFs {
    let mut fs = FakeFs::with_root();
    let r = fake_root();
    for d in ["project", "media", "notes"] {
        fs.add(&r, d, true, 0);
    }
    fs.add(&r.join("project"), "package.json", false, 100);
    fs.add(&r.join("project"), "node_modules", true, 0);
    fs.add(&r.join("project/node_modules"), "dep", true, 0);
    fs.add(&r.join("project/node_modules/dep"), "index.js", false, 50_000_000);
    fs.add(&r.join("project"), "src", true, 0);
    fs.add(&r.join("project/src"), "main.rs", false, 4_000);
    fs.add(&r.join("media"), "movie.mp4", false, 200_000_000);
    fs.add(&r.join("notes"), "todo.txt", false, 300);
    fs
}

fn filter() -> CleanupFilter {
    CleanupFilter { min_bytes: 0, risk: [true; 3], cats: u32::MAX, leftovers: LeftoverFilter::All }
}

/// Remover that records calls and fails for paths containing a marker.
struct FakeRemover {
    fail_on: Option<&'static str>,
    calls: Mutex<Vec<String>>,
}
impl Remover for FakeRemover {
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

async fn scanned() -> Session {
    let mut s = session();
    s.start_scan_with(fake_root(), Arc::new(fake_disk()));
    settle(&mut s);
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_then_analysis_then_views() {
    let mut s = session();
    assert!(s.data().is_none() && !s.busy());
    s.start_scan_with(fake_root(), Arc::new(fake_disk()));
    assert!(s.is_scanning() && s.scan_status().is_some());
    let changes = settle(&mut s);

    let pos = |c: Change| changes.iter().position(|x| *x == c);
    assert!(pos(Change::ScanFinished { ok: true }) < pos(Change::AnalysisReady), "analysis follows the scan: {changes:?}");
    assert!(!s.is_analyzing() && s.last_scan().unwrap().files == 5);

    let data = s.data().unwrap();
    let snap = data.read();
    assert_eq!(snap.tree().node(0).size, 100 + 50_000_000 + 4_000 + 200_000_000 + 300);
    // portable rule: node_modules next to package.json is Safe, and it is the only suggestion
    let list: Vec<usize> = s.cleanup(&snap, &filter()).to_vec();
    assert_eq!(list.len(), 1);
    let sug = &snap.analysis().suggestions[list[0]];
    assert_eq!(snap.tree().name(sug.node), "node_modules");
    // clusters: the 200 MB media folder is the biggest concentration
    let clusters = s.clusters(&snap, 40_000_000);
    assert_eq!(snap.tree().name(clusters[0].node), "media");
    assert!(clusters.len() == 2, "media and node_modules");
    // a filter that excludes Safe empties the list
    let mut f = filter();
    f.risk = [false, true, true];
    assert!(s.cleanup(&snap, &f).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_updates_views_immediately_and_reanalyses_once_done() {
    let mut s = scanned().await;
    let data = s.data().unwrap();
    let (nm, todo) = {
        let snap = data.read();
        (
            snap.tree().find(&path_of(&["project", "node_modules"]).to_string_lossy()).unwrap(),
            snap.tree().find(&path_of(&["notes", "todo.txt"]).to_string_lossy()).unwrap(),
        )
    };
    let epoch_before = s.epoch();

    let plan = {
        let snap = data.read();
        s.plan(&snap, &[nm, todo, 0])
    };
    assert_eq!((plan.items.len(), plan.refused.len()), (2, 1), "the scan root is refused");

    let remover = Arc::new(FakeRemover { fail_on: Some("todo"), calls: Mutex::new(vec![]) });
    s.delete(plan, remover.clone());
    assert!(s.is_deleting());
    let changes = settle(&mut s);

    assert!(changes.contains(&Change::Removed(nm)) && !changes.contains(&Change::Removed(todo)), "failed item stays: {changes:?}");
    assert!(changes.contains(&Change::DeleteFinished) && changes.contains(&Change::AnalysisReady));
    assert!(s.epoch() > epoch_before);
    assert!(s.notice.as_ref().unwrap().contains("1 failed"), "{:?}", s.notice);
    assert_eq!(remover.calls.lock().unwrap().len(), 2);

    // `data` was cloned before the batch and still holds the old analysis; take a fresh handle, as the
    // GUI does every frame
    let data = s.data().unwrap();
    let snap = data.read();
    assert_eq!(snap.tree().node(0).size, 100 + 4_000 + 200_000_000 + 300, "node_modules left the tree");
    assert!(s.cleanup(&snap, &filter()).is_empty(), "the removed suggestion is gone from the view");
    assert!(snap.analysis().suggestions.is_empty(), "analysis was recomputed after the batch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn render_is_current_only_for_the_latest_epoch() {
    let mut s = scanned().await;
    assert!(!s.render_requested() && s.rendered().is_none());
    s.request_render(0, 320, 200, ColorMode::Type);
    s.request_render(0, 320, 200, ColorMode::Type); // identical request is free
    let changes = settle(&mut s);
    assert_eq!(changes.iter().filter(|c| **c == Change::RenderReady).count(), 1);
    assert!(s.render_is_current());
    let r = s.rendered().unwrap();
    assert_eq!((r.w, r.h, r.root), (320, 200, 0));

    // a different size supersedes it
    s.request_render(0, 400, 240, ColorMode::App);
    settle(&mut s);
    assert_eq!(s.rendered().unwrap().w, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_scan_clears_everything_and_a_failed_scan_reports() {
    let mut s = scanned().await;
    s.request_render(0, 200, 100, ColorMode::Type);
    settle(&mut s);
    assert!(s.rendered().is_some());

    s.start_scan_with(Path::new("/definitely/not/there").to_path_buf(), Arc::new(FakeFs::default()));
    assert!(s.data().is_none() && s.rendered().is_none() && !s.render_requested(), "state reset at scan start");
    let changes = settle(&mut s);
    assert!(changes.contains(&Change::ScanFinished { ok: false }));
    assert!(s.error.as_ref().unwrap().contains("not a readable directory"));
    assert!(s.data().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_returns_immediately_when_idle() {
    let mut s = session();
    assert!(s.wait().is_empty());
}
