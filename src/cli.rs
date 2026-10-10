//! Headless adapter on the [`Session`]: `diskscope --cli <path>` scans, analyses and prints a report.
//! Used for benchmarking and verification; it shares every code path with the GUI except painting.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::cleanup::Risk;
use crate::model::{CAT_NAMES, Node, fmt_count, fmt_size};
use crate::platform;
use crate::session::{Change, Session};
use crate::treemap::ColorMode;

/// Run the session until nothing is in flight.
fn settle(s: &mut Session) -> Vec<Change> {
    let mut all = Vec::new();
    while s.busy() {
        all.extend(s.wait());
    }
    all
}

pub fn run(rt: &tokio::runtime::Runtime, path: PathBuf, on_disk: bool) {
    let platform = platform::current();
    println!("platform: {}", platform.name());
    let mut s = Session::new(rt.handle().clone(), platform, Arc::new(|| {}));

    let t0 = Instant::now();
    s.start_scan(path, on_disk);
    let mut t_scan = None;
    while s.busy() {
        for c in s.wait() {
            if matches!(c, Change::ScanFinished { .. }) {
                t_scan = Some(t0.elapsed());
            }
        }
    }
    if let Some(e) = &s.error {
        return eprintln!("scan failed: {e}");
    }
    let (Some(data), Some(stats)) = (s.data(), s.last_scan()) else { return };
    let t_total = t0.elapsed();
    let snap = data.read();
    let (tree, a) = (snap.tree(), snap.analysis());

    println!(
        "scanned {} files, {} dirs, {} in {:.2?} ({} unreadable dirs)",
        fmt_count(stats.files),
        fmt_count(stats.dirs),
        fmt_size(tree.nodes[0].size),
        t_scan.unwrap_or(t_total),
        stats.errors
    );
    println!(
        "tree memory: {} nodes, {} + {} names",
        tree.nodes.len(),
        fmt_size((tree.nodes.len() * size_of::<Node>()) as u64),
        fmt_size(tree.names.len() as u64)
    );
    println!("analysis: {:.2?}", t_total.saturating_sub(t_scan.unwrap_or_default()));

    println!("\n== file types ==");
    for t in a.types.iter().filter(|t| t.size > 0) {
        println!("{:<20} {:>12} {:>10} files", CAT_NAMES[t.cat as usize], fmt_size(t.size), fmt_count(t.files));
    }
    println!("\n== top applications ==");
    for app in a.apps.iter().take(15) {
        println!("{:<40} {:<14} {:>12}", app.name, app.kind.label(), fmt_size(app.size));
    }
    let thr = (tree.nodes[0].size / 100).max(32 << 20);
    println!("\n== large clusters (>= {}) ==", fmt_size(thr));
    for c in s.clusters(&snap, thr).iter().take(15) {
        println!(
            "{:>12} {:>3}% {:<18} {}{}",
            fmt_size(c.size),
            c.cat_pct,
            CAT_NAMES[c.cat as usize],
            tree.path(c.node),
            if c.loose { "  [loose files]" } else { "" }
        );
    }
    let safe: u64 = a.suggestions.iter().filter(|s| s.risk == Risk::Safe).map(|s| s.size).sum();
    println!("\n== cleanup suggestions: {} (safe: {}) ==", a.suggestions.len(), fmt_size(safe));
    for sg in a.suggestions.iter().take(15) {
        println!("{:>12} {:<7} {} -- {}", fmt_size(sg.size), sg.risk.label(), tree.path(sg.node), sg.reason);
    }
    let left: Vec<_> = a.suggestions.iter().filter(|s| s.leftover).collect();
    println!("\n== uninstall leftovers: {} ({}) ==", left.len(), fmt_size(left.iter().map(|s| s.size).sum()));
    for sg in left.iter().take(15) {
        println!("{:>12} {}  -- {}", fmt_size(sg.size), tree.path(sg.node), sg.reason);
    }

    // Exercise the renderer through the same seam the GUI uses.
    let t2 = Instant::now();
    s.request_render(0, 1600, 900, ColorMode::Type);
    settle(&mut s);
    if let Some(r) = s.rendered() {
        println!("\ntreemap 1600x900: {:.2?}, {} laid-out rects", t2.elapsed(), r.rects.len());
    }
}
