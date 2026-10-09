//! Application attribution.
//!
//! Directories are attributed to applications from two sources:
//!   1. the Windows "Uninstall" registry keys (`InstallLocation` -> display name/publisher), and
//!   2. well-known containers (Program Files, ProgramData, per-user AppData, Steam/Epic libraries,
//!      user profile folders) where each child folder is treated as one application.
//! Attribution is then propagated top-down in a single linear pass; the nearest attributed ancestor
//! wins, so a registry-detected app nested inside a heuristic container keeps its own name.

use std::collections::HashMap;

use crate::model::{Tree, flag};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppKind {
    Installed,
    Folder,
    Game,
    System,
    UserData,
}

impl AppKind {
    pub fn label(self) -> &'static str {
        match self {
            AppKind::Installed => "Installed app",
            AppKind::Folder => "App data",
            AppKind::Game => "Game",
            AppKind::System => "System",
            AppKind::UserData => "User data",
        }
    }
}

#[derive(Clone)]
pub struct AppInfo {
    pub name: String,
    pub publisher: String,
    pub kind: AppKind,
    pub roots: Vec<u32>,
    pub size: u64,
    pub files: u64,
}

struct Builder<'a> {
    tree: &'a Tree,
    apps: Vec<AppInfo>,
    by_name: HashMap<String, u16>,
    roots: HashMap<u32, u16>,
}

impl<'a> Builder<'a> {
    fn add(&mut self, node: u32, name: &str, publisher: &str, kind: AppKind) {
        if node == 0 || self.roots.contains_key(&node) || self.apps.len() >= u16::MAX as usize - 1 {
            return;
        }
        let key = format!("{kind:?}|{}", name.to_lowercase());
        let id = *self.by_name.entry(key).or_insert_with(|| {
            self.apps.push(AppInfo {
                name: name.to_string(),
                publisher: publisher.to_string(),
                kind,
                roots: Vec::new(),
                size: 0,
                files: 0,
            });
            self.apps.len() as u16
        });
        self.apps[id as usize - 1].roots.push(node);
        self.roots.insert(node, id);
    }

    fn dirs(&self, node: u32) -> Vec<u32> {
        self.tree
            .children(node)
            .filter(|&c| {
                let n = self.tree.node(c);
                n.is_dir() && !n.is_deleted()
            })
            .collect()
    }

    fn at(&self, from: u32, path: &[&str]) -> Option<u32> {
        let mut cur = from;
        for p in path {
            cur = self.tree.child_named(cur, p)?;
        }
        Some(cur)
    }

    /// Every child folder of `container` is an application. Vendor folders (Microsoft, Google, ...)
    /// are split one level further so "Google\Chrome" shows up as "Google Chrome".
    fn container(&mut self, container: u32, kind: AppKind, skip: &[&str]) {
        const VENDORS: [&str; 10] =
            ["microsoft", "google", "adobe", "jetbrains", "mozilla", "apple", "intel", "nvidia corporation", "amd", "oracle"];
        for d in self.dirs(container) {
            let name = self.tree.name(d).to_string();
            let lc = name.to_lowercase();
            if skip.contains(&lc.as_str()) {
                continue;
            }
            if VENDORS.contains(&lc.as_str()) {
                for g in self.dirs(d) {
                    let gname = format!("{name} {}", self.tree.name(g));
                    self.add(g, &gname, &name, kind);
                }
                // leftovers directly in the vendor folder
                let vname = format!("{name} (shared)");
                self.add(d, &vname, &name, kind);
            } else {
                // MSIX package ids look like "Vendor.App_8wekyb3d8bbwe"
                let clean = name.split('_').next().unwrap_or(&name).to_string();
                self.add(d, &clean, "", kind);
            }
        }
    }
}

#[cfg(windows)]
fn registry_apps() -> Vec<(String, String, String)> {
    use winreg::RegKey;
    use winreg::enums::*;

    let mut out = Vec::new();
    let hives = [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    for (hive, path) in hives {
        let Ok(key) = RegKey::predef(hive).open_subkey_with_flags(path, KEY_READ) else { continue };
        for sub in key.enum_keys().flatten() {
            let Ok(k) = key.open_subkey_with_flags(&sub, KEY_READ) else { continue };
            let Ok(name) = k.get_value::<String, _>("DisplayName") else { continue };
            if k.get_value::<u32, _>("SystemComponent").unwrap_or(0) == 1 {
                continue;
            }
            let publisher = k.get_value::<String, _>("Publisher").unwrap_or_default();
            let mut loc = k.get_value::<String, _>("InstallLocation").unwrap_or_default();
            if loc.trim().is_empty() {
                // Fall back to the folder of the icon / uninstaller executable.
                let icon = k
                    .get_value::<String, _>("DisplayIcon")
                    .or_else(|_| k.get_value::<String, _>("UninstallString"))
                    .unwrap_or_default();
                let icon = icon.trim().trim_start_matches('"');
                let icon = icon.split('"').next().unwrap_or("");
                let icon = icon.split(',').next().unwrap_or("");
                if let Some(parent) = std::path::Path::new(icon).parent() {
                    loc = parent.to_string_lossy().to_string();
                }
            }
            let loc = loc.trim().trim_matches('"').trim_end_matches('\\').to_string();
            if loc.len() > 3 {
                out.push((name, loc, publisher));
            }
        }
    }
    out
}
#[cfg(not(windows))]
fn registry_apps() -> Vec<(String, String, String)> {
    Vec::new()
}

pub fn detect(tree: &Tree) -> (Vec<AppInfo>, Vec<u16>) {
    let mut b = Builder { tree, apps: Vec::new(), by_name: HashMap::new(), roots: HashMap::new() };
    let n = tree.nodes.len();

    // 1. Registry-declared installs (most precise, inserted first so they win over heuristics).
    for (name, loc, publisher) in registry_apps() {
        if let Some(node) = tree.find(&loc) {
            let lc = tree.name(node).to_lowercase();
            if node != 0 && !matches!(lc.as_str(), "windows" | "system32" | "users" | "program files" | "program files (x86)") {
                b.add(node, &name, &publisher, AppKind::Installed);
            }
        }
    }

    // 2. System areas.
    if let Some(w) = b.at(0, &["Windows"]) {
        b.add(w, "Windows (OS)", "Microsoft", AppKind::System);
    }
    for (name, label) in [
        ("$Recycle.Bin", "Recycle Bin"),
        ("System Volume Information", "System Restore / VSS"),
        ("Windows.old", "Previous Windows (Windows.old)"),
        ("pagefile.sys", "Windows swap & hibernation"),
        ("hiberfil.sys", "Windows swap & hibernation"),
        ("swapfile.sys", "Windows swap & hibernation"),
        ("Recovery", "Windows Recovery"),
    ] {
        if let Some(x) = b.at(0, &[name]) {
            b.add(x, label, "Microsoft", AppKind::System);
        }
    }

    // 3. Program Files, ProgramData.
    for pf in ["Program Files", "Program Files (x86)"] {
        if let Some(c) = b.at(0, &[pf]) {
            b.container(c, AppKind::Folder, &["windowsapps", "common files", "modifiablewindowsapps"]);
            if let Some(wa) = b.at(c, &["WindowsApps"]) {
                b.container(wa, AppKind::Folder, &["deleted", "mutablepackagestorage"]);
            }
            if let Some(cf) = b.at(c, &["Common Files"]) {
                b.add(cf, &format!("{pf} Common Files"), "", AppKind::Folder);
            }
        }
    }
    if let Some(pd) = b.at(0, &["ProgramData"]) {
        b.container(pd, AppKind::Folder, &["package cache"]);
        if let Some(pc) = b.at(pd, &["Package Cache"]) {
            b.add(pc, "Installer package cache", "", AppKind::System);
        }
    }

    // 4. User profiles.
    if let Some(users) = b.at(0, &["Users"]) {
        for u in b.dirs(users) {
            let uname = tree.name(u).to_string();
            if matches!(uname.to_lowercase().as_str(), "all users" | "default user" | "default" | "public") {
                continue;
            }
            for sub in ["Local", "Roaming", "LocalLow"] {
                if let Some(c) = b.at(u, &["AppData", sub]) {
                    if sub == "Local" {
                        if let Some(p) = b.at(c, &["Programs"]) {
                            b.container(p, AppKind::Folder, &[]);
                        }
                        if let Some(p) = b.at(c, &["Packages"]) {
                            b.container(p, AppKind::Folder, &[]);
                        }
                        if let Some(t) = b.at(c, &["Temp"]) {
                            b.add(t, "User temp files", "", AppKind::System);
                        }
                    }
                    b.container(c, AppKind::Folder, &["temp", "packages", "programs"]);
                }
            }
            for d in b.dirs(u) {
                let name = tree.name(d).to_string();
                let lc = name.to_lowercase();
                if name.starts_with('.') && name.len() > 1 {
                    b.add(d, &name, "", AppKind::Folder);
                } else if matches!(
                    lc.as_str(),
                    "desktop" | "documents" | "downloads" | "pictures" | "videos" | "music" | "source" | "projects"
                ) || lc.starts_with("onedrive")
                {
                    b.add(d, &format!("{name} ({uname})"), &uname, AppKind::UserData);
                }
            }
        }
    }

    // 5. Game libraries anywhere on the drive: steamapps\common\<game>, Epic Games\<game>.
    for id in 1..n as u32 {
        let nd = tree.node(id);
        if !nd.is_dir() || nd.flags & flag::DELETED != 0 {
            continue;
        }
        match nd.name_len {
            9 if tree.name(id).eq_ignore_ascii_case("steamapps") => {
                if let Some(common) = b.at(id, &["common"]) {
                    for g in b.dirs(common) {
                        let gname = tree.name(g).to_string();
                        b.add(g, &gname, "Steam", AppKind::Game);
                    }
                }
            }
            10 if tree.name(id).eq_ignore_ascii_case("Epic Games") => {
                for g in b.dirs(id) {
                    let gname = tree.name(g).to_string();
                    b.add(g, &gname, "Epic Games", AppKind::Game);
                }
            }
            _ => {}
        }
    }

    // Propagate attribution top-down; parent id < child id so one forward pass is enough.
    let mut app_of = vec![0u16; n];
    for id in 0..n {
        let a = b.roots.get(&(id as u32)).copied().unwrap_or_else(|| {
            if id == 0 { 0 } else { app_of[tree.nodes[id].parent as usize] }
        });
        app_of[id] = a;
    }

    let mut apps = b.apps;
    for (id, nd) in tree.nodes.iter().enumerate() {
        let a = app_of[id];
        if a != 0 && !nd.is_dir() && !nd.is_deleted() {
            let info = &mut apps[a as usize - 1];
            info.size += nd.size;
            info.files += 1;
        }
    }

    // Drop empty apps and renumber (ids stay stable relative to the sorted list).
    let mut order: Vec<usize> = (0..apps.len()).filter(|&i| apps[i].size > 0).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(apps[i].size));
    let mut remap = vec![0u16; apps.len() + 1];
    for (new, &old) in order.iter().enumerate() {
        remap[old + 1] = new as u16 + 1;
    }
    for a in app_of.iter_mut() {
        *a = remap[*a as usize];
    }
    let apps = order.into_iter().map(|i| apps[i].clone()).collect();
    (apps, app_of)
}
