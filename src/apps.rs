//! Application attribution engine (platform-neutral).
//!
//! Directories are attributed to applications from three sources:
//!   1. programs the OS reports as installed (`Installed::registered`), most precise, added first;
//!   2. the platform's own roots ([`Platform::app_roots`]): system areas, program folders, per-user
//!      application data, user folders;
//!   3. game libraries that look the same everywhere (`steamapps/common/<game>`, `Epic Games/<game>`).
//!
//! Attribution is then propagated top-down in a single linear pass; the nearest attributed ancestor
//! wins, so an installed app nested inside a heuristic container keeps its own name.

use std::collections::HashMap;

use crate::model::{Tree, flag};
use crate::platform::{Installed, Platform};

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

/// What a platform adapter uses to declare "this folder belongs to that application".
pub struct AppBuilder<'a> {
    tree: &'a Tree,
    apps: Vec<AppInfo>,
    by_name: HashMap<String, u16>,
    roots: HashMap<u32, u16>,
}

impl<'a> AppBuilder<'a> {
    fn new(tree: &'a Tree) -> Self {
        AppBuilder { tree, apps: Vec::new(), by_name: HashMap::new(), roots: HashMap::new() }
    }

    /// Declare `node` (a folder or file) as a root of the application `name`. Roots with the same
    /// name and kind are merged into one application; a node is only ever claimed once.
    pub fn add(&mut self, node: u32, name: &str, publisher: &str, kind: AppKind) {
        if self.roots.contains_key(&node) || self.apps.len() >= u16::MAX as usize - 1 {
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

    pub fn name(&self, node: u32) -> &'a str {
        self.tree.name(node)
    }

    /// The scanned root with `/` separators on every OS, so adapters can match it without caring
    /// whether the host uses `\`.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))] // used by the macOS adapter
    pub fn root_path(&self) -> String {
        if std::path::MAIN_SEPARATOR == '/' {
            self.tree.root_path.clone()
        } else {
            self.tree.root_path.replace(std::path::MAIN_SEPARATOR, "/")
        }
    }

    /// Resolve a platform path within the scanned scope.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))] // used by the macOS adapter
    pub fn absolute(&self, path: &str) -> Option<u32> {
        self.tree.find(path)
    }

    /// Live child folders of `node`.
    pub fn dirs(&self, node: u32) -> Vec<u32> {
        self.tree
            .children(node)
            .filter(|&c| {
                let n = self.tree.node(c);
                n.is_dir() && !n.is_deleted()
            })
            .collect()
    }

    /// Follow `path` (case-insensitive names) down from `from`.
    pub fn at(&self, from: u32, path: &[&str]) -> Option<u32> {
        let mut cur = from;
        for p in path {
            cur = self.tree.child_named(cur, p)?;
        }
        Some(cur)
    }

    /// Every child folder of `container` is an application; `clean` turns the folder name into a
    /// display name. Vendor folders (Microsoft, Google, ...) are split one level further so
    /// "Google/Chrome" shows up as "Google Chrome".
    #[cfg_attr(not(any(windows, test)), allow(dead_code))] // used by the Windows adapter
    pub fn container(&mut self, container: u32, kind: AppKind, skip: &[&str], clean: fn(&str) -> String) {
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
                self.add(d, &clean(&name), "", kind);
            }
        }
    }
}

pub fn detect(tree: &Tree, platform: &dyn Platform, inst: &Installed) -> (Vec<AppInfo>, Vec<u16>) {
    let mut b = AppBuilder::new(tree);
    let n = tree.nodes.len();

    // 1. Programs the OS says are installed (inserted first so they win over heuristics).
    for r in &inst.registered {
        b.add(r.node, &r.name, &r.publisher, AppKind::Installed);
    }

    // 2. The platform's own areas.
    platform.app_roots(&mut b, inst);

    // 3. Game libraries anywhere on the disk: steamapps/common/<game>, Epic Games/<game>.
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
