//! Windows adapter. Registry, Win32 bulk directory reads and Explorer are `cfg(windows)`; the rules
//! themselves are plain code over the tree so they can be tested on any OS.

use std::collections::HashSet;
use std::sync::Arc;

use super::{CACHE_DIR_NAMES, Installed, Platform, Registered, RuleHit};
use crate::apps::{AppBuilder, AppKind};
use crate::cleanup::{Risk, Suggestion, ext_in};
use crate::dirreader::DirReader;
use crate::model::Tree;

pub struct Windows;

// ---------------------------------------------------------------- registry (Windows only)

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

#[cfg(windows)]
fn package_families() -> Option<HashSet<String>> {
    use winreg::RegKey;
    use winreg::enums::*;
    const SUB: &str = r"Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
    let mut set = HashSet::new();
    for (hive, path) in [
        (HKEY_CURRENT_USER, format!(r"Software\Classes\{SUB}")),
        (HKEY_LOCAL_MACHINE, format!(r"SOFTWARE\Classes\{SUB}")),
    ] {
        let Ok(key) = RegKey::predef(hive).open_subkey_with_flags(path, KEY_READ) else { continue };
        for sub in key.enum_keys().flatten() {
            // full name: Name_Version_Arch_ResourceId_PublisherId  ->  family: Name_PublisherId
            let parts: Vec<&str> = sub.split('_').collect();
            if parts.len() >= 2 {
                set.insert(format!("{}_{}", parts[0], parts[parts.len() - 1]).to_lowercase());
            }
        }
    }
    (!set.is_empty()).then_some(set)
}
#[cfg(not(windows))]
fn package_families() -> Option<HashSet<String>> {
    None
}

// ---------------------------------------------------------------- helpers

/// MSIX package folder names look like `Vendor.App_8wekyb3d8bbwe`; keep the readable part.
fn clean_package_name(name: &str) -> String {
    name.split('_').next().unwrap_or(name).to_string()
}

const BINARY_EXTS: [&str; 8] = ["exe", "dll", "sys", "ocx", "msi", "scr", "cpl", "jar"];

/// Program Files children that are shared tooling or OS-owned (config-only, no binaries of their own);
/// never judged. `Windows*` and `Microsoft*` folders are skipped by prefix as well.
const PF_SKIP: [&str; 10] = [
    "common files",
    "windowsapps",
    "modifiablewindowsapps",
    "msbuild",
    "nuget",
    "vulkanrt",
    "dotnet",
    "reference assemblies",
    "internet explorer",
    "windowspowershell",
];

fn has_binary(tree: &Tree, root: u32) -> bool {
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for c in tree.live_children(d) {
            let n = tree.node(c);
            if n.is_dir() {
                stack.push(c);
            } else if ext_in(tree.name(c), &BINARY_EXTS) {
                return true;
            }
        }
    }
    false
}

/// `Name_<13 chars of a-z0-9>`: the shape of a Store package family folder. Browser sandbox containers
/// (`cr.sb.*`, `fx.sb.*`) and similar are not packages and must not be judged.
fn looks_like_package_family(name: &str) -> bool {
    match name.split_once('_') {
        Some((n, publisher)) => {
            !n.is_empty()
                && publisher.len() == 13
                && publisher.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && !n.contains('_')
        }
        None => false,
    }
}

// ---------------------------------------------------------------- the adapter

impl Platform for Windows {
    fn name(&self) -> &'static str {
        "Windows"
    }

    fn default_root(&self) -> String {
        std::env::var("SystemDrive").map(|d| format!("{d}\\")).unwrap_or_else(|_| "C:\\".into())
    }

    fn dir_reader(&self, on_disk: bool) -> Arc<dyn DirReader> {
        #[cfg(windows)]
        {
            Arc::new(crate::dirreader::Win32Reader { on_disk })
        }
        #[cfg(not(windows))]
        {
            Arc::new(crate::dirreader::StdReader { on_disk, skip: None })
        }
    }

    fn trash_name(&self) -> &'static str {
        "Recycle Bin"
    }

    fn trash_note(&self) -> &'static str {
        "Items go to the Recycle Bin and can be restored, unless Windows decides they are too large for it."
    }

    fn reveal(&self, path: &str) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{path}\"")).spawn();
        }
        #[cfg(not(windows))]
        let _ = path;
    }

    fn installed(&self, tree: &Tree) -> Installed {
        let mut inst = Installed::default();
        for (name, loc, publisher) in registry_apps() {
            let Some(node) = tree.find(&loc) else { continue };
            let lc = tree.name(node).to_lowercase();
            if node == 0 || matches!(lc.as_str(), "windows" | "system32" | "users" | "program files" | "program files (x86)") {
                continue;
            }
            inst.nodes.insert(node);
            inst.names_lc.insert(name.to_lowercase());
            inst.registered.push(Registered { node, name, publisher });
        }
        inst.package_families = package_families();
        inst.user_packages_dir =
            std::env::var("LOCALAPPDATA").ok().map(|p| format!("{}\\Packages", p.trim_end_matches('\\')));
        inst
    }

    fn app_roots(&self, b: &mut AppBuilder, _inst: &Installed) {
        // System areas.
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

        // Program Files, ProgramData.
        for pf in ["Program Files", "Program Files (x86)"] {
            if let Some(c) = b.at(0, &[pf]) {
                b.container(c, AppKind::Folder, &["windowsapps", "common files", "modifiablewindowsapps"], clean_package_name);
                if let Some(wa) = b.at(c, &["WindowsApps"]) {
                    b.container(wa, AppKind::Folder, &["deleted", "mutablepackagestorage"], clean_package_name);
                }
                if let Some(cf) = b.at(c, &["Common Files"]) {
                    b.add(cf, &format!("{pf} Common Files"), "", AppKind::Folder);
                }
            }
        }
        if let Some(pd) = b.at(0, &["ProgramData"]) {
            b.container(pd, AppKind::Folder, &["package cache"], clean_package_name);
            if let Some(pc) = b.at(pd, &["Package Cache"]) {
                b.add(pc, "Installer package cache", "", AppKind::System);
            }
        }

        // User profiles.
        if let Some(users) = b.at(0, &["Users"]) {
            for u in b.dirs(users) {
                let uname = b.name(u).to_string();
                if matches!(uname.to_lowercase().as_str(), "all users" | "default user" | "default" | "public") {
                    continue;
                }
                for sub in ["Local", "Roaming", "LocalLow"] {
                    if let Some(c) = b.at(u, &["AppData", sub]) {
                        if sub == "Local" {
                            if let Some(p) = b.at(c, &["Programs"]) {
                                b.container(p, AppKind::Folder, &[], clean_package_name);
                            }
                            if let Some(p) = b.at(c, &["Packages"]) {
                                b.container(p, AppKind::Folder, &[], clean_package_name);
                            }
                            if let Some(t) = b.at(c, &["Temp"]) {
                                b.add(t, "User temp files", "", AppKind::System);
                            }
                        }
                        b.container(c, AppKind::Folder, &["temp", "packages", "programs"], clean_package_name);
                    }
                }
                for d in b.dirs(u) {
                    let name = b.name(d).to_string();
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
    }

    fn classify_dir(&self, tree: &Tree, id: u32, l: &str) -> Option<RuleHit> {
        let path = || tree.path_norm(id);
        Some(match l {
            "$recycle.bin" => (Risk::Review, "Recycle Bin contents - empty it from Explorer"),
            "windows.old" => (Risk::Review, "Previous Windows installation - remove via Disk Cleanup"),
            "$windows.~bt" | "$windows.~ws" | "$sysreset" | "$winreagent" => (Risk::Review, "Windows upgrade / reset leftovers"),
            "temp" | "tmp" => {
                let p = path();
                if p.contains("/appdata/local/temp") || p.ends_with("/windows/temp") {
                    (Risk::Safe, "Temporary files")
                } else {
                    return None;
                }
            }
            n if CACHE_DIR_NAMES.contains(&n) && path().contains("/appdata/") => (Risk::Safe, "Application / browser cache"),
            "inetcache" => (Risk::Safe, "Internet cache"),
            "d3dscache" => (Risk::Safe, "DirectX shader cache"),
            "crashdumps" | "minidump" | "livekernelreports" => (Risk::Safe, "Crash dumps"),
            "wer" if path().contains("/microsoft/windows/wer") => (Risk::Safe, "Windows Error Reporting data"),
            "download" if path().ends_with("/windows/softwaredistribution/download") => {
                (Risk::Review, "Windows Update download cache")
            }
            "$patchcache$" => (Risk::Review, "Windows Installer patch cache"),
            _ => return None,
        })
    }

    fn classify_file(&self, _tree: &Tree, _id: u32, l: &str, _size: u64) -> Option<RuleHit> {
        match l {
            "hiberfil.sys" => return Some((Risk::Caution, "Hibernation file - disable with `powercfg /h off`")),
            "pagefile.sys" | "swapfile.sys" => return Some((Risk::Caution, "Virtual memory file - managed by Windows")),
            "memory.dmp" => return Some((Risk::Safe, "Kernel crash dump")),
            _ => {}
        }
        if l.starts_with("thumbcache_") && l.ends_with(".db") {
            return Some((Risk::Safe, "Thumbnail cache"));
        }
        if ext_in(l, &["etl"]) {
            return Some((Risk::Safe, "Event trace log"));
        }
        None
    }

    fn installer_exts(&self) -> &'static [&'static str] {
        &["exe", "msi", "iso", "zip", "7z", "rar"]
    }

    fn is_protected(&self, p: &str) -> bool {
        // Hard-linked system files (WinSxS) and application installs look like duplicates / old files
        // but are not safe to remove.
        ["/windows/", "/program files", "/programdata/", "/$recycle.bin", "/system volume information"]
            .iter()
            .any(|x| p.contains(x))
    }

    /// Folders that look like remains of uninstalled programs:
    ///   * a `Program Files` folder that is not any registered program's install location and holds no
    ///     executables or libraries at all (only data is left);
    ///   * a Store-package data folder in the current user's `%LOCALAPPDATA%\Packages` whose package is no
    ///     longer installed.
    /// Always graded Review: the signal is a heuristic.
    fn leftovers(&self, tree: &Tree, inst: &Installed) -> Vec<Suggestion> {
        let mut out = Vec::new();
        let mut consider = |c: u32, reason: &'static str| {
            let n = tree.node(c);
            if n.is_dir() && !n.is_deleted() && n.size > 0 {
                out.push(Suggestion { node: c, size: n.size, risk: Risk::Review, reason, leftover: true });
            }
        };

        for pf in ["Program Files", "Program Files (x86)"] {
            let Some(pfn) = tree.child_named(0, pf) else { continue };
            for c in tree.live_children(pfn) {
                let lc = tree.name(c).to_ascii_lowercase();
                if PF_SKIP.contains(&lc.as_str()) || lc.starts_with("windows") || lc.starts_with("microsoft") || inst.nodes.contains(&c) {
                    continue;
                }
                if tree.node(c).is_dir() && !has_binary(tree, c) {
                    consider(c, "Program Files folder with no installed program and no executables - uninstall leftovers");
                }
            }
        }

        if let (Some(dir), Some(fams)) = (&inst.user_packages_dir, &inst.package_families) {
            if let Some(pn) = tree.find(dir) {
                for c in tree.live_children(pn) {
                    let name = tree.name(c);
                    if looks_like_package_family(name) && !fams.contains(&name.to_lowercase()) {
                        consider(c, "Data of a Store app that is no longer installed");
                    }
                }
            }
        }
        out
    }
}
