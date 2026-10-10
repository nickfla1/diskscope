//! macOS adapter. Only the OS calls (`open -R`, `st_flags`) are `cfg(target_os = "macos")`; the rules are
//! plain code over the tree, so they are tested on every OS against synthetic trees rooted at `/`.
//!
//! Leftover detection for uninstalled apps is not implemented on macOS yet (no reliable installed-app list
//! to compare `~/Library` folders against), so `leftovers` returns nothing.

use std::path::Path;
use std::sync::Arc;

use super::{CACHE_DIR_NAMES, Installed, Platform, Registered, RuleHit};
use crate::apps::{AppBuilder, AppKind};
use crate::cleanup::{Risk, Suggestion, ext_in};
use crate::dirreader::DirReader;
use crate::model::Tree;

pub struct MacOs;

/// Directories the scanner must not enter: other mounted volumes (a scan of `/` would otherwise count
/// them, and the firmlinked Data volume, twice), device nodes and the VM swap area.
pub fn skip_dir(p: &Path) -> bool {
    matches!(
        p.to_str(),
        Some("/dev" | "/Volumes" | "/System/Volumes" | "/private/var/vm" | "/cores" | "/Network" | "/home" | "/net")
    )
}

fn is_app_bundle(name: &str) -> bool {
    name.len() > 4 && name.get(name.len() - 4..).is_some_and(|s| s.eq_ignore_ascii_case(".app"))
}

/// `com.google.Chrome` -> `Chrome`, `group.com.foo.Bar` -> `Bar`, `UBF8T346G9.Office` -> `Office`,
/// `Safari.app` -> `Safari`.
pub fn clean_bundle_name(name: &str) -> String {
    let n = if is_app_bundle(name) { &name[..name.len() - 4] } else { name };
    let n = n.strip_prefix("group.").unwrap_or(n);
    let n = match n.split_once('.') {
        Some((team, rest)) if team.len() == 10 && team.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) => rest,
        _ => n,
    };
    let reverse_dns = n.matches('.').count() >= 2
        && matches!(n.split('.').next(), Some("com" | "org" | "net" | "io" | "co" | "de" | "app" | "dev" | "me" | "us" | "uk" | "eu"));
    if reverse_dns { n.rsplit('.').next().unwrap_or(n).to_string() } else { n.to_string() }
}

impl Platform for MacOs {
    fn name(&self) -> &'static str {
        "macOS"
    }

    fn default_root(&self) -> String {
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    }

    fn dir_reader(&self, on_disk: bool) -> Arc<dyn DirReader> {
        Arc::new(crate::dirreader::StdReader { on_disk, skip: Some(skip_dir) })
    }

    fn trash_name(&self) -> &'static str {
        "Trash"
    }

    fn trash_note(&self) -> &'static str {
        "Items go to the Trash and can be restored from Finder with Put Back."
    }

    fn reveal(&self, path: &str) {
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
        }
        #[cfg(not(target_os = "macos"))]
        let _ = path;
    }

    /// Application bundles in the scanned scope, including system and personal applications.
    /// Nested helper bundles belong to their enclosing application.
    fn installed(&self, tree: &Tree) -> Installed {
        let mut inst = Installed::default();
        let mut register = |node: u32| {
            let name = clean_bundle_name(if node == 0 {
                Path::new(&tree.root_path).file_name().and_then(|n| n.to_str()).unwrap_or(tree.name(node))
            } else { tree.name(node) });
            inst.nodes.insert(node);
            inst.names_lc.insert(name.to_lowercase());
            inst.registered.push(Registered { node, name, publisher: String::new() });
        };
        for c in 0..tree.nodes.len() as u32 {
            if tree.node(c).is_dir() && !tree.is_gone(c) && is_app_bundle(tree.name(c))
                && !tree.ancestors(c).any(|a| is_app_bundle(tree.name(a)))
            {
                register(c);
            }
        }
        inst
    }

    fn app_roots(&self, b: &mut AppBuilder, _inst: &Installed) {
        let personal_scope = b.root_path().to_lowercase().contains("/users/");
        // `/Library` first: containers inside it are nearer roots and win.
        if !personal_scope && let Some(l) = b.at(0, &["Library"]) {
            b.add(l, "macOS system library", "Apple", AppKind::System);
        }
        for name in ["System", "usr", "bin", "sbin", "private"] {
            if let Some(x) = b.at(0, &[name]) {
                b.add(x, "macOS (system)", "Apple", AppKind::System);
            }
        }
        for path in [&["opt", "homebrew"][..], &["usr", "local", "Cellar"], &["usr", "local", "Homebrew"]] {
            if let Some(x) = b.at(0, path) {
                b.add(x, "Homebrew", "", AppKind::Folder);
            }
        }
        if !personal_scope && let Some(l) = b.at(0, &["Library"]) {
            for sub in ["Application Support", "Caches", "Logs"] {
                if let Some(c) = b.at(l, &[sub]) {
                    add_children(b, c);
                }
            }
        }

        let mut user_roots: Vec<(u32, String)> = b.at(0, &["Users"])
            .map(|users| b.dirs(users).into_iter().map(|u| (u, b.name(u).to_string())).collect())
            .unwrap_or_default();
        // A home-folder (the default), Library, or Library-container scan still has the same
        // attribution anchors. Resolve only anchors that are inside the scanned scope.
        let root = b.root_path().to_string();
        let components: Vec<&str> = root.split('/').filter(|s| !s.is_empty()).collect();
        if let Some(i) = components.iter().rposition(|s| s.eq_ignore_ascii_case("Users"))
            && let Some(uname) = components.get(i + 1) {
                let home = format!("/{}", components[..i + 2].join("/"));
                if let Some(u) = b.absolute(&home) {
                    user_roots.push((u, (*uname).to_string()));
                } else {
                    for sub in ["Application Support", "Caches", "Containers", "Group Containers", "Logs", "Saved Application State", "HTTPStorages", "WebKit"] {
                        if let Some(c) = b.absolute(&format!("{home}/Library/{sub}")) {
                            add_children(b, c);
                        } else {
                            let prefix = format!("{home}/Library/{sub}/");
                            if let Some(rest) = root.strip_prefix(&prefix) {
                                let name = rest.split('/').next().unwrap_or(rest);
                                let apple = name.starts_with("com.apple.") || name.starts_with("group.com.apple.");
                                let display = if apple { "Apple system data".to_string() } else { clean_bundle_name(name) };
                                b.add(0, &display, "", if apple { AppKind::System } else { AppKind::Folder });
                            }
                        }
                    }
                    if let Some(d) = b.absolute(&format!("{home}/Library/Developer")) {
                        b.add(d, "Xcode developer data", "Apple", AppKind::Folder);
                    }
                    for sub in ["Mail", "Messages", "Mobile Documents"] {
                        if let Some(d) = b.absolute(&format!("{home}/Library/{sub}")) {
                            let name = if sub == "Mobile Documents" { format!("iCloud Drive ({uname})") } else { sub.to_string() };
                            b.add(d, &name, "Apple", if sub == "Mobile Documents" { AppKind::UserData } else { AppKind::Folder });
                        }
                    }
                    if let Some(cs) = b.absolute(&format!("{home}/Library/CloudStorage")) {
                        for d in b.dirs(cs) {
                            let name = format!("{} ({uname})", b.name(d));
                            b.add(d, &name, uname, AppKind::UserData);
                        }
                    }
                }
        }
        for (u, uname) in user_roots {
            if matches!(uname.as_str(), "Shared" | "Guest") {
                continue;
            }
            if let Some(lib) = b.at(u, &["Library"]) {
                for sub in [
                    "Application Support",
                    "Caches",
                    "Containers",
                    "Group Containers",
                    "Logs",
                    "Saved Application State",
                    "HTTPStorages",
                    "WebKit",
                ] {
                    if let Some(c) = b.at(lib, &[sub]) {
                        add_children(b, c);
                    }
                }
                if let Some(x) = b.at(lib, &["Developer"]) {
                    b.add(x, "Xcode developer data", "Apple", AppKind::Folder);
                }
                if let Some(x) = b.at(lib, &["Mobile Documents"]) {
                    b.add(x, &format!("iCloud Drive ({uname})"), &uname, AppKind::UserData);
                }
                if let Some(cs) = b.at(lib, &["CloudStorage"]) {
                    for d in b.dirs(cs) {
                        let n = b.name(d).to_string();
                        b.add(d, &format!("{n} ({uname})"), &uname, AppKind::UserData);
                    }
                }
                for sub in ["Mail", "Messages"] {
                    if let Some(x) = b.at(lib, &[sub]) {
                        b.add(x, sub, "Apple", AppKind::Folder);
                    }
                }
            }
            for d in b.dirs(u) {
                let name = b.name(d).to_string();
                let lc = name.to_lowercase();
                if lc == ".trash" {
                    b.add(d, "Trash", "", AppKind::System);
                } else if name.starts_with('.') && name.len() > 1 {
                    b.add(d, &name, "", AppKind::Folder);
                } else if matches!(
                    lc.as_str(),
                    "desktop" | "documents" | "downloads" | "pictures" | "movies" | "music" | "projects" | "developer" | "sites"
                ) || lc.starts_with("onedrive")
                    || matches!(lc.as_str(), "dropbox" | "google drive")
                {
                    b.add(d, &format!("{name} ({uname})"), &uname, AppKind::UserData);
                }
            }
        }
    }

    fn classify_dir(&self, tree: &Tree, id: u32, l: &str) -> Option<RuleHit> {
        let path = || tree.path_norm(id);
        Some(match l {
            ".trash" => (Risk::Review, "Trash contents - empty it from Finder"),
            "caches" if path().ends_with("/library/caches") => (Risk::Safe, "Application caches - rebuilt by the apps"),
            "logs" if path().ends_with("/library/logs") => (Risk::Safe, "Application logs and crash reports"),
            "deriveddata" if path().ends_with("/library/developer/xcode/deriveddata") => {
                (Risk::Safe, "Xcode build products - rebuilt by Xcode")
            }
            "ios devicesupport" | "watchos devicesupport" | "tvos devicesupport" | "xros devicesupport" | "visionos devicesupport"
                if path().contains("/library/developer/xcode/") =>
            {
                (Risk::Safe, "Xcode device support symbols - re-created when a device connects")
            }
            "archives" if path().ends_with("/library/developer/xcode/archives") => {
                (Risk::Review, "Xcode archives - keep any you may need to symbolicate or resubmit")
            }
            "caches" if path().ends_with("/library/developer/coresimulator/caches") => {
                if path() == "/library/developer/coresimulator/caches" {
                    (Risk::Caution, "System-owned simulator caches - managed by Xcode; use simulator runtime cleanup")
                } else {
                    (Risk::Safe, "Simulator caches - rebuilt by Xcode")
                }
            }
            n if is_device_id(n) && tree.path_norm(tree.node(id).parent).ends_with("/library/developer/coresimulator/devices") => {
                (Risk::Caution, "Simulator device and data - manage with `xcrun simctl list devices` and `xcrun simctl delete <UUID>`; deletion is permanent")
            }
            "profiles" if path().ends_with("/library/developer/coresimulator/profiles") => {
                (Risk::Caution, "Simulator runtimes - remove individual versions in Xcode Settings > Components")
            }
            "backup" if path().ends_with("/mobilesync/backup") => (Risk::Review, "iPhone / iPad backups"),
            n if CACHE_DIR_NAMES.contains(&n) && path().contains("/library/") => (Risk::Safe, "Application / browser cache"),
            _ => return None,
        })
    }

    fn classify_file(&self, tree: &Tree, id: u32, l: &str, _size: u64) -> Option<RuleHit> {
        match l {
            "sleepimage" => Some((Risk::Caution, "Hibernation image - managed by macOS")),
            "docker.raw" => Some((Risk::Review, "Docker Desktop disk image - reclaim with `docker system prune`")),
            n if n.starts_with("swapfile") && tree.path_norm(id).contains("/private/var/vm/") => {
                Some((Risk::Caution, "macOS swap file"))
            }
            n if ext_in(n, &["ipsw"]) => Some((Risk::Review, "iOS/macOS firmware image - re-downloaded when needed")),
            _ => None,
        }
    }

    fn installer_exts(&self) -> &'static [&'static str] {
        &["dmg", "pkg", "iso", "zip", "xip", "7z", "rar"]
    }

    fn is_protected(&self, p: &str) -> bool {
        ["/system/", "/usr/", "/bin/", "/sbin/", "/private/var/db/", "/applications/", "/library/apple/", ".app/contents/", "/library/developer/commandlinetools/"]
            .iter()
            .any(|x| p.contains(x))
    }

    fn leftovers(&self, _tree: &Tree, _inst: &Installed) -> Vec<Suggestion> {
        Vec::new()
    }
}

fn is_device_id(name: &str) -> bool {
    name.len() == 36 && name.bytes().enumerate().all(|(i, c)| {
        if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { c.is_ascii_hexdigit() }
    })
}

/// Every child folder of a `~/Library` container is an application data folder. Apple's own bundle
/// ids are lumped together so they do not drown out real applications.
fn add_children(b: &mut AppBuilder, container: u32) {
    for d in b.dirs(container) {
        let name = b.name(d).to_string();
        let lc = name.to_lowercase();
        if lc.starts_with("com.apple.") || lc == "apple" || lc.starts_with("group.com.apple.") {
            b.add(d, "Apple system data", "Apple", AppKind::System);
        } else {
            b.add(d, &clean_bundle_name(&name), "", AppKind::Folder);
        }
    }
}
