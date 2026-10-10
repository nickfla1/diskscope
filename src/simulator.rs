//! Simulator runtime management through Apple's tools, independent of filesystem deletion.
use std::sync::mpsc::{self, Receiver};

#[derive(Clone)]
struct Runtime {
    id: String,
    name: String,
    size: u64,
    deletable: bool,
}

fn command(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("/usr/bin/xcrun").arg("simctl").arg("runtime").args(args)
        .output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("{}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse(json: &str) -> Result<Vec<Runtime>, String> {
    let data: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let object = data.as_object().ok_or("Unexpected runtime list format")?;
    let mut runtimes = Vec::new();
    for (id, value) in object {
        if !valid_id(id) { return Err("Invalid runtime identifier".into()); }
        let name = value["runtimeIdentifier"].as_str().unwrap_or("Unknown runtime")
            .trim_start_matches("com.apple.CoreSimulator.SimRuntime.").replace('-', " ");
        runtimes.push(Runtime {
            id: id.clone(),
            name: format!("{name} ({})", value["build"].as_str().unwrap_or("unknown build")),
            size: value["sizeBytes"].as_u64().unwrap_or(0),
            deletable: value["deletable"].as_bool().unwrap_or(false),
        });
    }
    runtimes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(runtimes)
}

fn valid_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().enumerate().all(|(i, b)| {
        if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() }
    })
}

#[derive(Default)]
pub struct Manager {
    open: bool,
    runtimes: Vec<Runtime>,
    pending: Option<Runtime>,
    job: Option<Receiver<Result<Vec<Runtime>, String>>>,
    message: String,
}

impl Manager {
    fn start(&mut self, ctx: &eframe::egui::Context, delete: Option<String>) {
        let (tx, rx) = mpsc::channel();
        self.job = Some(rx);
        self.message = if delete.is_some() { "Removing runtime…" } else { "Loading runtimes…" }.into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                if let Some(id) = delete {
                    if !valid_id(&id) { return Err("Invalid runtime identifier".into()); }
                    command(&["delete", &id])?;
                }
                parse(&command(&["list", "--json"])? )
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    pub fn ui(&mut self, ui: &mut eframe::egui::Ui) {
        if ui.button("Manage simulator runtimes…").clicked() {
            self.open = true;
            if self.job.is_none() { self.start(ui.ctx(), None); }
        }
        if let Some(rx) = &self.job {
            match rx.try_recv() {
                Ok(result) => {
                    self.job = None;
                    match result {
                        Ok(list) => { self.runtimes = list; self.message = "Runtime list updated. Rescan after removal to update the disk tree.".into(); }
                        Err(e) => self.message = e,
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => { self.job = None; self.message = "Runtime worker stopped unexpectedly".into(); }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !self.open { return; }
        let mut open = self.open;
        eframe::egui::Window::new("Simulator runtimes").open(&mut open).show(ui.ctx(), |ui| {
            ui.label("System-wide runtimes reported by Xcode; sizes are independent of the current scan.");
            ui.label(&self.message);
            if ui.add_enabled(self.job.is_none(), eframe::egui::Button::new("Refresh")).clicked() { self.start(ui.ctx(), None); }
            for runtime in &self.runtimes {
                ui.horizontal(|ui| {
                    ui.label(format!("{} — {}", runtime.name, crate::model::fmt_size(runtime.size)));
                    if ui.add_enabled(runtime.deletable && self.job.is_none() && self.pending.is_none(), eframe::egui::Button::new("Remove…")).clicked() {
                        self.pending = Some(runtime.clone());
                    }
                });
            }
            if let Some(runtime) = self.pending.clone() {
                ui.separator();
                ui.label(format!("Permanently remove {}?", runtime.name));
                ui.label("Apple's simctl will shut down simulators using this runtime and unmount it. This skips Trash; download it again in Xcode if needed.");
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { self.pending = None; }
                    if ui.add_enabled(self.job.is_none(), eframe::egui::Button::new("Remove runtime permanently")).clicked() {
                        self.pending = None;
                        self.start(ui.ctx(), Some(runtime.id));
                    }
                });
            }
        });
        self.open = open;
    }
}

#[test]
fn runtime_list_requires_exact_identifiers_and_explicit_deletability() {
    let list = parse(r#"{"12345678-1234-1234-1234-123456789ABC":{"runtimeIdentifier":"com.apple.CoreSimulator.SimRuntime.iOS-18-2","build":"22C150","sizeBytes":42,"deletable":true}}"#).unwrap();
    assert_eq!(list[0].name, "iOS 18 2 (22C150)");
    assert_eq!(list[0].size, 42);
    assert!(list[0].deletable);
    assert!(parse(r#"{"all":{}}"#).is_err());
    assert!(!parse(r#"{"12345678-1234-1234-1234-123456789ABC":{}}"#).unwrap()[0].deletable);
}
