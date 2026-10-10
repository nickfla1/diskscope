//! Platform adapters tested on any OS: both rule sets are compiled under `cfg(test)` and run against real
//! temp folders laid out like a macOS or Windows disk (the temp folder plays the part of `/` or `C:\`).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use crate::apps;
use crate::cleanup::{self, Risk};
use crate::model::Tree;
use crate::platform::macos::{self, MacOs};
use crate::platform::windows::Windows;
use crate::platform::{Installed, Platform};
use crate::scan::{self, Progress};

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Create `files` (relative path, size) under a fresh temp root and scan it. `days_old` back-dates a file.
async fn build(tag: &str, files: &[(&str, usize)], days_old: &[(&str, u64)]) -> (PathBuf, Guard, Tree) {
    let root = std::env::temp_dir().join(format!("diskscope-plat-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let g = Guard(root.clone());
    for (rel, size) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![1u8; *size]).unwrap();
    }
    for (rel, days) in days_old {
        let f = fs::OpenOptions::new().write(true).open(root.join(rel)).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(days * 86_400)).unwrap();
    }
    let tree = scan::scan(root.clone(), crate::platform::current().dir_reader(false), Arc::new(Progress::default()), Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    (root, g, tree)
}

/// Suggestions as `relative/path -> risk`.
fn verdicts(tree: &Tree, root: &PathBuf, p: &dyn Platform, inst: &Installed) -> BTreeMap<String, Risk> {
    let prefix = format!("{}/", tree.path_norm(0).trim_end_matches('/'));
    let _ = root;
    cleanup::detect(tree, p, inst).0.iter().map(|s| (tree.path_norm(s.node).trim_start_matches(&prefix).to_string(), s.risk)).collect()
}

const BIG: usize = 70_000; // above the 64 KiB floor that file rules apply to

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn macos_cleanup_rules() {
    let (root, _g, tree) = build(
        "mac-clean",
        &[
            ("Users/me/Library/Caches/com.foo.Bar/blob.dat", 4096),
            ("Users/me/Library/Logs/DiagnosticReports/a.crash", 4096),
            ("Users/me/Library/Developer/Xcode/DerivedData/App-abc/b.o", 4096),
            ("Users/me/Library/Developer/Xcode/iOS DeviceSupport/16.0/sym", 4096),
            ("Users/me/Library/Developer/Xcode/Archives/2024/x.xcarchive/foo", 4096),
            ("Users/me/Library/Developer/CoreSimulator/Devices/12345678-1234-1234-1234-123456789ABC/data", 4096),
            ("Users/me/Library/Developer/CoreSimulator/Caches/cache.bin", 4096),
            ("Users/me/Library/Application Support/MobileSync/Backup/abc/Manifest.db", 4096),
            ("Users/me/Library/Application Support/Slack/Cache/blob", 4096),
            ("Users/me/Library/Application Support/Slack/Local Storage/x", 4096),
            ("Users/me/.Trash/stuff.txt", 4096),
            ("Users/me/Library/Containers/com.docker.docker/Data/vms/0/data/docker.raw", BIG),
            ("Users/me/Downloads/Installer.dmg", BIG),
            ("Users/me/Downloads/Fresh.dmg", BIG),
            ("private/var/vm/sleepimage", BIG),
            ("private/var/vm/swapfile0", BIG),
            ("Users/me/project/package.json", 10),
            ("Users/me/project/node_modules/x/index.js", 4096),
        ],
        &[("Users/me/Downloads/Installer.dmg", 200)],
    )
    .await;
    let v = verdicts(&tree, &root, &MacOs, &Installed::default());
    let expect: BTreeMap<String, Risk> = [
        ("users/me/library/caches", Risk::Safe),
        ("users/me/library/logs", Risk::Safe),
        ("users/me/library/developer/xcode/deriveddata", Risk::Safe),
        ("users/me/library/developer/xcode/ios devicesupport", Risk::Safe),
        ("users/me/library/developer/xcode/archives", Risk::Review),
        ("users/me/library/developer/coresimulator/devices/12345678-1234-1234-1234-123456789abc", Risk::Caution),
        ("users/me/library/developer/coresimulator/caches", Risk::Safe),
        ("users/me/library/application support/mobilesync/backup", Risk::Review),
        ("users/me/library/application support/slack/cache", Risk::Safe),
        ("users/me/.trash", Risk::Review),
        ("users/me/library/containers/com.docker.docker/data/vms/0/data/docker.raw", Risk::Review),
        ("users/me/downloads/installer.dmg", Risk::Review),
        ("private/var/vm/sleepimage", Risk::Caution),
        ("private/var/vm/swapfile0", Risk::Caution),
        ("users/me/project/node_modules", Risk::Safe),
    ]
    .into_iter()
    .map(|(k, r)| (k.to_string(), r))
    .collect();
    assert_eq!(v, expect, "fresh .dmg, Slack/Local Storage and project files must not be flagged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn macos_installed_apps_and_attribution() {
    let (_root, _g, tree) = build(
        "mac-apps",
        &[
            ("Applications/Safari.app/Contents/MacOS/Safari", 4096),
            ("Applications/Utilities/Terminal.app/Contents/x", 4096),
            ("Applications/Notes.txt", 100),
            ("Users/me/Applications/Mine.app/Contents/x", 4096),
            ("Users/me/Library/Containers/com.google.Chrome/Data/x", 4096),
            ("Users/me/Library/Containers/com.apple.Notes/Data/x", 4096),
            ("Users/me/Library/Application Support/Slack/x", 4096),
            ("Users/me/Library/Developer/Xcode/DerivedData/x", 4096),
            ("Users/me/Documents/readme.txt", 4096),
            ("Users/me/.Trash/old", 4096),
            ("Users/me/.cargo/registry/x", 4096),
            ("System/Library/x", 4096),
            ("opt/homebrew/bin/brew", 4096),
        ],
        &[],
    )
    .await;
    let inst = MacOs.installed(&tree);
    assert_eq!(inst.names_lc, HashSet::from(["safari".to_string(), "terminal".to_string(), "mine".to_string()]));
    assert_eq!(inst.registered.len(), 3);

    let (list, app_of) = apps::detect(&tree, &MacOs, &inst);
    let by_name: BTreeMap<&str, apps::AppKind> = list.iter().map(|a| (a.name.as_str(), a.kind)).collect();
    for (name, kind) in [
        ("Safari", apps::AppKind::Installed),
        ("Terminal", apps::AppKind::Installed),
        ("Mine", apps::AppKind::Installed),
        ("Chrome", apps::AppKind::Folder),
        ("Apple system data", apps::AppKind::System),
        ("Slack", apps::AppKind::Folder),
        ("Xcode developer data", apps::AppKind::Folder),
        ("Documents (me)", apps::AppKind::UserData),
        ("Trash", apps::AppKind::System),
        (".cargo", apps::AppKind::Folder),
        ("macOS (system)", apps::AppKind::System),
        ("Homebrew", apps::AppKind::Folder),
    ] {
        assert_eq!(by_name.get(name), Some(&kind), "missing or wrong kind for {name}: {by_name:?}");
    }
    // files inside a bundle are attributed to it
    let exe = tree.find(&_root.join("Applications/Safari.app/Contents/MacOS/Safari").to_string_lossy()).unwrap();
    assert_eq!(list[app_of[exe as usize] as usize - 1].name, "Safari");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_cleanup_rules_still_hold_after_the_split() {
    let (root, _g, tree) = build(
        "win-clean",
        &[
            ("Users/me/AppData/Local/Temp/x.bin", 4096),
            ("Users/me/AppData/Local/Google/Chrome/User Data/Default/Cache/blob", 4096),
            ("Users/me/AppData/Local/Microsoft/Windows/INetCache/y", 4096),
            ("Windows/SoftwareDistribution/Download/a", 4096),
            ("Windows/Temp/t", 4096),
            ("$Recycle.Bin/s/x", 4096),
            ("hiberfil.sys", BIG),
            ("pagefile.sys", BIG),
            ("Users/me/Downloads/setup.exe", BIG),
            ("Users/me/Documents/keep.txt", 4096),
            ("Users/me/proj/package.json", 10),
            ("Users/me/proj/node_modules/x/i.js", 4096),
        ],
        &[("Users/me/Downloads/setup.exe", 120)],
    )
    .await;
    let v = verdicts(&tree, &root, &Windows, &Installed::default());
    let expect: BTreeMap<String, Risk> = [
        ("users/me/appdata/local/temp", Risk::Safe),
        ("users/me/appdata/local/google/chrome/user data/default/cache", Risk::Safe),
        ("users/me/appdata/local/microsoft/windows/inetcache", Risk::Safe),
        ("windows/softwaredistribution/download", Risk::Review),
        ("windows/temp", Risk::Safe),
        ("$recycle.bin", Risk::Review),
        ("hiberfil.sys", Risk::Caution),
        ("pagefile.sys", Risk::Caution),
        ("users/me/downloads/setup.exe", Risk::Review),
        ("users/me/proj/node_modules", Risk::Safe),
    ]
    .into_iter()
    .map(|(k, r)| (k.to_string(), r))
    .collect();
    assert_eq!(v, expect);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn macos_attribution_for_home_and_library_scans() {
    let (root, _g, _) = build("mac-subtree", &[
        ("Users/me/Applications/Mine.APP/Contents/x", 4096),
        ("Users/me/Applications/Mine.APP/Contents/Helpers/Helper.app/x", 4096),
        ("Users/me/Library/Application Support/Slack/x", 4096),
        ("Users/me/Library/Containers/com.google.Chrome/Data/x", 4096),
        ("Users/me/Documents/x", 4096),
        ("Users/me/Applications/日本語/x", 4096),
    ], &[]).await;
    for scope in ["Users/me", "Users/me/Library", "Users/me/Library/Containers", "Users/me/Library/Containers/com.google.Chrome"] {
        let tree = scan::scan(root.join(scope), MacOs.dir_reader(false), Arc::new(Progress::default()), Arc::new(AtomicBool::new(false)))
            .await.unwrap();
        let inst = MacOs.installed(&tree);
        let (list, _) = apps::detect(&tree, &MacOs, &inst);
        assert!(list.iter().any(|a| a.name == "Chrome"), "Chrome missing in {scope}");
        if scope == "Users/me" {
            assert_eq!(inst.names_lc, HashSet::from(["mine".to_string()]));
            assert!(list.iter().any(|a| a.name == "Documents (me)"));
        }
        if !scope.starts_with("Users/me/Library/Containers") {
            assert!(list.iter().any(|a| a.name == "Slack"), "Slack missing in {scope}");
        }
    }
    let tree = scan::scan(root.join("Users/me/Applications/Mine.APP"), MacOs.dir_reader(false), Arc::new(Progress::default()), Arc::new(AtomicBool::new(false)))
        .await.unwrap();
    let inst = MacOs.installed(&tree);
    assert_eq!(inst.names_lc, HashSet::from(["mine".to_string()]));
    assert_eq!(apps::detect(&tree, &MacOs, &inst).0[0].name, "Mine");
}

#[test]
fn macos_helpers() {
    assert_eq!(macos::clean_bundle_name("com.google.Chrome"), "Chrome");
    assert_eq!(macos::clean_bundle_name("group.com.foo.Bar"), "Bar");
    assert_eq!(macos::clean_bundle_name("UBF8T346G9.Office"), "Office");
    assert_eq!(macos::clean_bundle_name("Safari.app"), "Safari");
    assert_eq!(macos::clean_bundle_name("Mine.APP"), "Mine");
    assert_eq!(macos::clean_bundle_name("日本語"), "日本語");
    assert_eq!(macos::clean_bundle_name("Application Support"), "Application Support");
    assert_eq!(macos::clean_bundle_name("v1.2.3"), "v1.2.3", "version-like names are not reverse DNS");
    for p in ["/Volumes", "/System/Volumes", "/dev", "/private/var/vm"] {
        assert!(macos::skip_dir(std::path::Path::new(p)), "{p}");
    }
    assert!(!macos::skip_dir(std::path::Path::new("/Users")));
    assert!(!macos::skip_dir(std::path::Path::new("/Volumes/Backup/stuff")), "only the mount directory itself");
    assert!(MacOs.is_protected("/applications/safari.app/contents/x") && !MacOs.is_protected("/users/me/documents/a"));
    assert!(Windows.is_protected("c:/windows/winsxs/x") && !Windows.is_protected("c:/users/me/a"));
}
