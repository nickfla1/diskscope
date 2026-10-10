mod analysis;
mod app;
mod apps;
mod cleanup;
mod cli;
mod deletion;
mod dirreader;
mod dupes;
mod model;
mod platform;
mod scan;
mod session;
mod treemap;
mod views;
#[cfg(target_os = "macos")]
mod simulator;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_leftovers;
#[cfg(test)]
mod tests_platform;
#[cfg(test)]
mod tests_scanner;
#[cfg(test)]
mod tests_session;

use std::path::PathBuf;

pub fn build_runtime() -> tokio::runtime::Runtime {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores.max(2))
        // Directory reads block in the kernel; more threads than cores keeps the disk queue full.
        .max_blocking_threads(
            std::env::var("DISKSCOPE_IO_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or((cores * 4).clamp(16, 128)),
        )
        .thread_name("diskscope-io")
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn main() {
    let rt = build_runtime();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        let path = args.get(2).cloned().unwrap_or_else(|| platform::current().default_root());
        cli::run(&rt, PathBuf::from(path), !args.iter().any(|a| a == "--logical"));
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
            .with_title(format!("DiskScope ({})", platform::current().name()))
            .with_inner_size([1500.0, 920.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    // `rt` stays alive on this thread for as long as the window is open.
    if let Err(e) = eframe::run_native(
        "DiskScope",
        options,
        Box::new(move |cc| {
            let mut a = app::App::new(cc, handle);
            if let Some(i) = args.iter().position(|x| x == "--shot") {
                a.enable_shot(PathBuf::from(&args[i + 1]), args[i + 2].clone(), args.get(i + 3).map(String::as_str));
            }
            Ok(Box::new(a))
        }),
    ) {
        eprintln!("gui error: {e}");
    }
}
