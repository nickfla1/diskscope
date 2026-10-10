//! Session: owns everything that is *not* painting. The tree, its analysis, the epoch counter, every
//! background job (scan, analysis, treemap render, duplicate search, deletion) with its progress, and the
//! derived lists the UI shows.
//!
//! Interface: commands in (`start_scan`, `delete`, `request_render`, ...), changes out (`poll` / the
//! blocking `wait`, each returning [`Change`]s), and read access through [`Data::read`] snapshots. It
//! knows nothing about egui: the GUI and the CLI are two adapters on the same seam, and tests drive it
//! headless with a fake [`DirReader`] and a fake [`Remover`].
//!
//! **Epoch.** One counter that increases whenever the tree or its analysis changes (scan finished,
//! analysis ready, an item removed). Cached views and renders are keyed by it; an analysis result tagged
//! with an older epoch is discarded and recomputed, so stale results are never shown.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use crate::analysis::{self, Analysis, Cluster};
use crate::deletion::{self, Plan, Remover};
use crate::dirreader::DirReader;
use crate::dupes::{self, DupGroup, DupProgress};
use crate::model::Tree;
use crate::platform::Platform;
use crate::scan::{self, Progress};
use crate::treemap::{self, ColorMode, ColorSource, Rendered};
use crate::views::{CleanupFilter, DupRow, Views};

pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Something observable changed; the adapter reacts (reset UI state, upload a texture, print, ...).
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// A scan ended. `ok == false`: cancelled or failed (see `Session::error`).
    ScanFinished { ok: bool },
    AnalysisReady,
    /// One item left the tree.
    Removed(u32),
    DupsReady,
    /// A deletion job ended (see `Session::notice`).
    DeleteFinished,
    /// `Session::rendered` holds a new treemap.
    RenderReady,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanStatus {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct DupStatus {
    /// 1 grouping by size, 2 quick hash, 3 full hash.
    pub stage: u8,
    pub total: u64,
    pub done: u64,
}

#[derive(Clone, Debug)]
pub struct DeleteStatus {
    pub done: u64,
    pub total: u64,
    pub current: String,
    pub permanent: bool,
}

/// The shared, immutable-between-epochs state. Cheap to clone (two `Arc`s).
#[derive(Clone)]
pub struct Data {
    tree: Arc<RwLock<Tree>>,
    an: Arc<Analysis>,
}

impl Data {
    /// Take the read lock for the lifetime of the snapshot. Do not hold one across a blocking call.
    pub fn read(&self) -> Snapshot<'_> {
        Snapshot { tree: self.tree.read().unwrap(), an: &self.an }
    }
}

pub struct Snapshot<'a> {
    tree: RwLockReadGuard<'a, Tree>,
    an: &'a Analysis,
}

impl Snapshot<'_> {
    pub fn tree(&self) -> &Tree {
        &self.tree
    }
    pub fn analysis(&self) -> &Analysis {
        self.an
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RenderKey {
    root: u32,
    w: usize,
    h: usize,
    mode: ColorMode,
    epoch: u64,
}

enum Event {
    Scanned(Result<Tree, String>),
    Analyzed(u64, Analysis),
    Render(RenderKey, Option<Arc<Rendered>>),
    Dups(Vec<DupGroup>),
    Removed(u32),
    DeleteDone { ok: usize, failed: Vec<(String, String)>, permanent: bool, cancelled: bool },
}

struct ScanJob {
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

struct DupJob {
    progress: Arc<DupProgress>,
    cancel: Arc<AtomicBool>,
}

struct DeleteJob {
    done: AtomicU64,
    total: AtomicU64,
    current: Mutex<String>,
    cancel: AtomicBool,
    permanent: bool,
}

pub struct Session {
    rt: tokio::runtime::Handle,
    platform: &'static dyn Platform,
    wake: Wake,
    tx: Sender<Event>,
    rx: Receiver<Event>,

    data: Option<Data>,
    epoch: u64,
    views: Views,

    scan: Option<ScanJob>,
    last_scan: Option<ScanStatus>,
    analyzing: bool,
    reanalyze: bool,
    delete: Option<Arc<DeleteJob>>,

    dup_job: Option<DupJob>,
    dup_groups: Vec<DupGroup>,
    dup_version: u64,
    dups_done: bool,

    render_counter: Arc<AtomicU64>,
    render_requested: Option<RenderKey>,
    rendered_key: Option<RenderKey>,
    rendered: Option<Arc<Rendered>>,
    renders_inflight: usize,

    /// Last error (scan failure) and last result message (deletion); the adapter shows them.
    pub error: Option<String>,
    pub notice: Option<String>,
}

impl Session {
    /// `wake` is called (from background threads) whenever an event is waiting, e.g. to repaint a window.
    pub fn new(rt: tokio::runtime::Handle, platform: &'static dyn Platform, wake: Wake) -> Self {
        let (tx, rx) = channel();
        Session {
            rt,
            platform,
            wake,
            tx,
            rx,
            data: None,
            epoch: 0,
            views: Views::default(),
            scan: None,
            last_scan: None,
            analyzing: false,
            reanalyze: false,
            delete: None,
            dup_job: None,
            dup_groups: Vec::new(),
            dup_version: 0,
            dups_done: false,
            render_counter: Arc::new(AtomicU64::new(0)),
            render_requested: None,
            rendered_key: None,
            rendered: None,
            renders_inflight: 0,
            error: None,
            notice: None,
        }
    }

    fn send(tx: &Sender<Event>, wake: &Wake, ev: Event) {
        let _ = tx.send(ev);
        wake();
    }

    // ------------------------------------------------------------------ queries

    pub fn platform(&self) -> &'static dyn Platform {
        self.platform
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    /// The loaded tree and analysis, if a scan has completed.
    pub fn data(&self) -> Option<Data> {
        self.data.clone()
    }
    pub fn is_scanning(&self) -> bool {
        self.scan.is_some()
    }
    pub fn is_analyzing(&self) -> bool {
        self.analyzing
    }
    pub fn is_deleting(&self) -> bool {
        self.delete.is_some()
    }
    pub fn is_finding_dups(&self) -> bool {
        self.dup_job.is_some()
    }
    /// Anything still running (so a headless caller knows whether `wait` can still produce changes).
    pub fn busy(&self) -> bool {
        self.scan.is_some() || self.analyzing || self.delete.is_some() || self.dup_job.is_some() || self.renders_inflight > 0
    }
    pub fn scan_status(&self) -> Option<ScanStatus> {
        self.scan.as_ref().map(|j| ScanStatus {
            files: j.progress.files.load(Relaxed),
            dirs: j.progress.dirs.load(Relaxed),
            bytes: j.progress.bytes.load(Relaxed),
            errors: j.progress.errors.load(Relaxed),
            elapsed: j.started.elapsed(),
        })
    }
    /// Totals of the last finished scan.
    pub fn last_scan(&self) -> Option<ScanStatus> {
        self.last_scan
    }
    pub fn delete_status(&self) -> Option<DeleteStatus> {
        self.delete.as_ref().map(|j| DeleteStatus {
            done: j.done.load(Relaxed),
            total: j.total.load(Relaxed),
            current: j.current.lock().unwrap().clone(),
            permanent: j.permanent,
        })
    }
    pub fn dup_status(&self) -> Option<DupStatus> {
        self.dup_job.as_ref().map(|j| DupStatus {
            stage: j.progress.stage.load(Relaxed),
            total: j.progress.total.load(Relaxed),
            done: j.progress.done.load(Relaxed),
        })
    }
    pub fn dups_done(&self) -> bool {
        self.dups_done
    }
    /// The latest treemap, and whether a render has been requested at all.
    pub fn rendered(&self) -> Option<Arc<Rendered>> {
        self.rendered.clone()
    }
    pub fn render_requested(&self) -> bool {
        self.render_requested.is_some()
    }
    /// True when `rendered()` is the render for the current epoch and the last requested view.
    pub fn render_is_current(&self) -> bool {
        self.rendered.is_some() && self.rendered_key == self.render_requested && self.render_requested.is_some_and(|k| k.epoch == self.epoch)
    }

    // ------------------------------------------------------------------ views

    pub fn clusters(&mut self, snap: &Snapshot<'_>, threshold: u64) -> &[Cluster] {
        self.views.clusters(self.epoch, snap, threshold)
    }

    pub fn cleanup(&mut self, snap: &Snapshot<'_>, filter: &CleanupFilter) -> &[usize] {
        self.views.cleanup(self.epoch, snap, filter)
    }

    /// Duplicate groups and their flattened display rows.
    pub fn dup_view(&mut self) -> (&[DupGroup], &[DupRow]) {
        let rows = self.views.dup_rows(self.dup_version, &self.dup_groups);
        (&self.dup_groups, rows)
    }

    /// Plan a deletion of `ids` against the current snapshot (applies the safety rules).
    pub fn plan(&self, snap: &Snapshot<'_>, ids: &[u32]) -> Plan {
        deletion::plan(snap.tree(), ids, &snap.analysis().risk_of)
    }

    // ------------------------------------------------------------------ commands

    /// Scan `root` with the platform's directory reader. `on_disk`: allocated size instead of logical.
    pub fn start_scan(&mut self, root: PathBuf, on_disk: bool) {
        let reader = self.platform.dir_reader(on_disk);
        self.start_scan_with(root, reader);
    }

    pub fn start_scan_with(&mut self, root: PathBuf, reader: Arc<dyn DirReader>) {
        if self.scan.is_some() {
            return;
        }
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan = Some(ScanJob { progress: progress.clone(), cancel: cancel.clone(), started: Instant::now() });
        self.error = None;
        self.notice = None;
        self.data = None;
        self.views.reset();
        self.rendered = None;
        self.rendered_key = None;
        self.render_requested = None;
        self.render_counter.fetch_add(1, Relaxed); // abandon any render still running
        self.dup_groups.clear();
        self.dup_version += 1;
        self.dups_done = false;
        self.epoch += 1;
        let (tx, wake) = (self.tx.clone(), self.wake.clone());
        self.rt.spawn(async move {
            let res = scan::scan(root, reader, progress, cancel).await;
            Self::send(&tx, &wake, Event::Scanned(res));
        });
    }

    pub fn cancel_scan(&self) {
        if let Some(j) = &self.scan {
            j.cancel.store(true, Relaxed);
        }
    }

    pub fn start_dups(&mut self, min_bytes: u64) {
        let Some(d) = self.data.clone() else { return };
        if self.dup_job.is_some() {
            return;
        }
        let progress = Arc::new(DupProgress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        self.dup_job = Some(DupJob { progress: progress.clone(), cancel: cancel.clone() });
        self.dup_groups.clear();
        self.dup_version += 1;
        self.dups_done = false;
        let (tx, wake, platform) = (self.tx.clone(), self.wake.clone(), self.platform);
        self.rt.spawn_blocking(move || {
            let g = dupes::find(&d.tree, min_bytes, &progress, &cancel, platform);
            Self::send(&tx, &wake, Event::Dups(g));
        });
    }

    pub fn cancel_dups(&self) {
        if let Some(j) = &self.dup_job {
            j.cancel.store(true, Relaxed);
        }
    }

    /// Run a deletion plan through `remover` in the background. Each removed item is reported as
    /// [`Change::Removed`] as it happens; [`Change::DeleteFinished`] ends the job.
    pub fn delete(&mut self, plan: Plan, remover: Arc<dyn Remover>) {
        let Some(d) = self.data.clone() else { return };
        if self.delete.is_some() || plan.items.is_empty() {
            return;
        }
        let job = Arc::new(DeleteJob {
            done: AtomicU64::new(0),
            total: AtomicU64::new(plan.items.len() as u64),
            current: Mutex::new(String::new()),
            cancel: AtomicBool::new(false),
            permanent: remover.permanent(),
        });
        self.delete = Some(job.clone());
        let (tx, wake) = (self.tx.clone(), self.wake.clone());
        self.rt.spawn_blocking(move || {
            let out = deletion::run(&d.tree, plan, &*remover, &job.cancel, &mut |p| match p {
                deletion::Progress::Started { total } => job.total.store(total as u64, Relaxed),
                deletion::Progress::Working { path } => *job.current.lock().unwrap() = path.to_string(),
                deletion::Progress::Removed { id } => {
                    job.done.fetch_add(1, Relaxed);
                    Self::send(&tx, &wake, Event::Removed(id));
                }
                deletion::Progress::Failed { .. } => {
                    job.done.fetch_add(1, Relaxed);
                }
            });
            Self::send(
                &tx,
                &wake,
                Event::DeleteDone { ok: out.ok, failed: out.failed, permanent: out.permanent, cancelled: out.cancelled },
            );
        });
    }

    pub fn cancel_delete(&self) {
        if let Some(j) = &self.delete {
            j.cancel.store(true, Relaxed);
        }
    }

    /// Render the treemap of `root` at `w` x `h` pixels. Re-requesting the same view in the same epoch is
    /// free; a new request abandons one still in flight. The result arrives as [`Change::RenderReady`].
    pub fn request_render(&mut self, root: u32, w: usize, h: usize, mode: ColorMode) {
        let Some(d) = self.data.clone() else { return };
        let key = RenderKey { root, w, h, mode, epoch: self.epoch };
        if self.render_requested == Some(key) || w < 8 || h < 8 {
            return;
        }
        self.render_requested = Some(key);
        self.renders_inflight += 1;
        let counter = self.render_counter.clone();
        let generation = counter.fetch_add(1, Relaxed) + 1;
        let (tx, wake) = (self.tx.clone(), self.wake.clone());
        self.rt.spawn_blocking(move || {
            let r = {
                let t = d.tree.read().unwrap();
                let cs = ColorSource { mode: key.mode, app_of: &d.an.app_of, risk_of: &d.an.risk_of };
                treemap::render(&t, key.root, key.w, key.h, &cs, &counter, generation)
            };
            Self::send(&tx, &wake, Event::Render(key, r.map(Arc::new)));
        });
    }

    // ------------------------------------------------------------------ analysis

    fn spawn_analysis(&mut self) {
        let Some(d) = &self.data else { return };
        let tree = d.tree.clone();
        let (tx, wake, tag, platform) = (self.tx.clone(), self.wake.clone(), self.epoch, self.platform);
        self.analyzing = true;
        self.rt.spawn_blocking(move || {
            let a = analysis::analyze(&tree.read().unwrap(), platform);
            Self::send(&tx, &wake, Event::Analyzed(tag, a));
        });
    }

    fn want_analysis(&mut self) {
        if self.analyzing {
            self.reanalyze = true;
        } else {
            self.spawn_analysis();
        }
    }

    // ------------------------------------------------------------------ events

    /// Process everything that has happened since the last call; never blocks.
    pub fn poll(&mut self) -> Vec<Change> {
        let mut changes = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            self.handle(ev, &mut changes);
        }
        changes
    }

    /// Block until at least one event has been processed (or 30 s pass), then drain. Returns at once if
    /// nothing is running. Intended for headless callers.
    pub fn wait(&mut self) -> Vec<Change> {
        let mut changes = Vec::new();
        if !self.busy() {
            return changes;
        }
        if let Ok(ev) = self.rx.recv_timeout(Duration::from_secs(30)) {
            self.handle(ev, &mut changes);
        }
        changes.extend(self.poll());
        changes
    }

    fn handle(&mut self, ev: Event, changes: &mut Vec<Change>) {
        match ev {
            Event::Scanned(res) => {
                let job = self.scan.take();
                self.last_scan = job.map(|j| ScanStatus {
                    files: j.progress.files.load(Relaxed),
                    dirs: j.progress.dirs.load(Relaxed),
                    bytes: j.progress.bytes.load(Relaxed),
                    errors: j.progress.errors.load(Relaxed),
                    elapsed: j.started.elapsed(),
                });
                match res {
                    Ok(tree) => {
                        self.data = Some(Data { tree: Arc::new(RwLock::new(tree)), an: Arc::new(Analysis::default()) });
                        self.epoch += 1;
                        self.views.reset();
                        changes.push(Change::ScanFinished { ok: true });
                        self.want_analysis();
                    }
                    Err(e) => {
                        if e != "cancelled" {
                            self.error = Some(e);
                        }
                        changes.push(Change::ScanFinished { ok: false });
                    }
                }
            }
            Event::Analyzed(tag, a) => {
                self.analyzing = false;
                if tag == self.epoch {
                    if let Some(d) = &mut self.data {
                        d.an = Arc::new(a);
                    }
                    self.epoch += 1;
                    changes.push(Change::AnalysisReady);
                } else {
                    self.reanalyze = true; // the tree changed while we were computing
                }
                if self.reanalyze && self.delete.is_none() && self.data.is_some() {
                    self.reanalyze = false;
                    self.spawn_analysis();
                }
            }
            Event::Render(key, r) => {
                self.renders_inflight = self.renders_inflight.saturating_sub(1);
                if self.render_requested == Some(key) {
                    if let Some(r) = r {
                        self.rendered = Some(r);
                        self.rendered_key = Some(key);
                        changes.push(Change::RenderReady);
                    }
                }
            }
            Event::Dups(g) => {
                self.dup_job = None;
                self.dups_done = true;
                self.dup_groups = g;
                self.dup_version += 1;
                changes.push(Change::DupsReady);
            }
            Event::Removed(id) => {
                // `deletion::run` already updated the tree; bumping the epoch invalidates every view.
                self.epoch += 1;
                changes.push(Change::Removed(id));
            }
            Event::DeleteDone { ok, failed, permanent, cancelled } => {
                self.delete = None;
                if ok > 0 {
                    if let Some(d) = self.data.clone() {
                        let t = d.tree.read().unwrap();
                        self.dup_groups.retain_mut(|g| {
                            g.nodes.retain(|&n| !t.is_gone(n));
                            g.nodes.len() > 1
                        });
                        self.dup_version += 1;
                    }
                    self.reanalyze = false;
                    self.want_analysis();
                }
                let verb = if permanent {
                    "Permanently deleted".to_string()
                } else {
                    format!("Moved to the {}:", self.platform.trash_name())
                };
                self.notice = Some(match (failed.first(), cancelled) {
                    (None, false) => format!("{verb} {ok} item(s)."),
                    (None, true) => format!("{verb} {ok} item(s); cancelled."),
                    (Some(f), _) => format!("{verb} {ok} item(s); {} failed (first: {} - {}).", failed.len(), f.0, f.1),
                });
                changes.push(Change::DeleteFinished);
            }
        }
    }
}
