mod analysis;
mod app;
mod apps;
mod cleanup;
mod deletion;
mod dirreader;
mod dupes;
mod model;
mod scan;
mod treemap;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_scanner;
#[cfg(test)]
mod tests_leftovers;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

use model::{CAT_NAMES, fmt_count, fmt_size};

pub fn build_runtime() -> tokio::runtime::Runtime {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores.max(2))
        // Directory reads block in the kernel; more threads than cores keeps the disk queue full.
        .max_blocking_threads(std::env::var("DISKSCOPE_IO_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or((cores * 4).clamp(16, 128)))
        .thread_name("diskscope-io")
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Headless mode used for benchmarking and verification: `diskscope --cli <path>`.
async fn cli(path: PathBuf, on_disk: bool) {
    let progress = Arc::new(scan::Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    let tree = match scan::scan(path, dirreader::system(on_disk), progress.clone(), cancel).await {
        Ok(t) => t,
        Err(e) => return eprintln!("scan failed: {e}"),
    };
    let t_scan = t0.elapsed();
    println!(
        "scanned {} files, {} dirs, {} in {:.2?} ({} unreadable dirs)",
        fmt_count(progress.files.load(Relaxed)),
        fmt_count(progress.dirs.load(Relaxed)),
        fmt_size(tree.nodes[0].size),
        t_scan,
        progress.errors.load(Relaxed)
    );
    println!("tree memory: {} nodes, {} + {} names", tree.nodes.len(), fmt_size((tree.nodes.len() * size_of::<model::Node>()) as u64), fmt_size(tree.names.len() as u64));

    let t1 = Instant::now();
    let a = analysis::analyze(&tree);
    println!("analysis: {:.2?}", t1.elapsed());

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
    for c in analysis::find_clusters(&tree, thr, &a.app_of).iter().take(15) {
        println!(
            "{:>12} {:>3}% {:<18} {}{}",
            fmt_size(c.size),
            c.cat_pct,
            CAT_NAMES[c.cat as usize],
            tree.path(c.node),
            if c.loose { "  [loose files]" } else { "" }
        );
    }
    let safe: u64 = a.suggestions.iter().filter(|s| s.risk == cleanup::Risk::Safe).map(|s| s.size).sum();
    println!("\n== cleanup suggestions: {} (safe: {}) ==", a.suggestions.len(), fmt_size(safe));
    for s in a.suggestions.iter().take(15) {
        println!("{:>12} {:<7} {} -- {}", fmt_size(s.size), s.risk.label(), tree.path(s.node), s.reason);
    }

    let left: Vec<_> = a.suggestions.iter().filter(|s| s.leftover).collect();
    println!(
        "\n== uninstall leftovers: {} ({}) ==",
        left.len(),
        fmt_size(left.iter().map(|s| s.size).sum())
    );
    for s in left.iter().take(15) {
        println!("{:>12} {}  -- {}", fmt_size(s.size), tree.path(s.node), s.reason);
    }

    // Exercise the renderer too.
    let t2 = Instant::now();
    let counter = std::sync::atomic::AtomicU64::new(1);
    let cs = treemap::ColorSource { mode: treemap::ColorMode::Type, app_of: &a.app_of, risk_of: &a.risk_of };
    let r = treemap::render(&tree, 0, 1600, 900, &cs, &counter, 1).unwrap();
    println!("\ntreemap 1600x900: {:.2?}, {} laid-out rects", t2.elapsed(), r.rects.len());
}

fn main() {
    let rt = build_runtime();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        let path = args.get(2).cloned().unwrap_or_else(|| "C:\\".into());
        rt.block_on(cli(PathBuf::from(path), !args.iter().any(|a| a == "--logical")));
        return;
    }
    // GUI launches (e.g. double-click) should not keep a console window around.
    #[cfg(windows)]
    if !args.iter().any(|a| a == "--keep-console") {
        unsafe { windows_sys::Win32::System::Console::FreeConsole() };
    }
    let handle = rt.handle().clone();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("DiskScope")
            .with_inner_size([1500.0, 920.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    // `rt` stays alive on this thread for as long as the window is open.
    if let Err(e) = eframe::run_native("DiskScope", options, Box::new(move |cc| {
        let mut a = app::App::new(cc, handle);
        if let Some(i) = args.iter().position(|x| x == "--shot") {
            a.enable_shot(PathBuf::from(&args[i + 1]), args[i + 2].clone(), args.get(i + 3).map(String::as_str));
        }
        Ok(Box::new(a))
    })) {
        eprintln!("gui error: {e}");
    }
}
