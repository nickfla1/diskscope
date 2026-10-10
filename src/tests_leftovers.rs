//! Uninstall-leftover rules, driven by a fake `Installed` so they do not depend on the machine's registry.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::platform::windows::Windows;
use crate::platform::{Installed, Platform};
use crate::cleanup::{self, Risk};
use crate::model::Tree;
use crate::scan::{self, Progress};

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn touch(root: &PathBuf, rel: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![1u8; 4096]).unwrap();
}

async fn build() -> (PathBuf, Guard, Tree) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("diskscope-leftovers-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let g = Guard(root.clone());
    // Program Files
    touch(&root, "Program Files/OldApp/settings.ini");
    touch(&root, "Program Files/OldApp/cache/blob.dat");
    touch(&root, "Program Files/LiveApp/app.exe");
    touch(&root, "Program Files/Registered/data.bin");
    touch(&root, "Program Files/Vendor/Sub/tool.dll");
    touch(&root, "Program Files/Common Files/shared.bin");
    touch(&root, "Program Files/Windows Defender/defs.bin");
    touch(&root, "Program Files (x86)/OldTool/readme.txt");
    // Store packages
    let pk = "Users/me/AppData/Local/Packages";
    touch(&root, &format!("{pk}/Gone.App_abcdefghjkmnp/cache.dat"));
    touch(&root, &format!("{pk}/Live.App_abcdefghjkmnp/x.dat"));
    touch(&root, &format!("{pk}/cr.sb.odm13DA26A24F6CD7A2E509D4AAADC/x.dat"));
    touch(&root, &format!("{pk}/Name_toolongpublisherid1/x.dat"));
    touch(&root, "Users/other/AppData/Local/Packages/Gone2.App_abcdefghjkmnp/x.dat");
    let tree = scan::scan(root.clone(), crate::platform::current().dir_reader(false), Arc::new(Progress::default()), Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    (root, g, tree)
}

fn installed(tree: &Tree, root: &PathBuf, families: Option<&[&str]>) -> Installed {
    Installed {
        nodes: HashSet::from([tree.find(&root.join("Program Files/Registered").to_string_lossy()).unwrap()]),
        package_families: families.map(|f| f.iter().map(|s| s.to_lowercase()).collect()),
        user_packages_dir: Some(root.join("Users/me/AppData/Local/Packages").to_string_lossy().to_string()),
        ..Default::default()
    }
}

fn names(tree: &Tree, s: &[cleanup::Suggestion]) -> HashSet<String> {
    s.iter().map(|x| tree.path(x.node).replace('\\', "/").split("diskscope-leftovers-").last().unwrap().split_once('/').unwrap().1.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_files_and_store_package_leftovers() {
    let (root, _g, tree) = build().await;
    let inst = installed(&tree, &root, Some(&["Live.App_abcdefghjkmnp"]));
    let found = Windows.leftovers(&tree, &inst);
    assert!(found.iter().all(|s| s.leftover && s.risk == Risk::Review));
    assert_eq!(
        names(&tree, &found),
        HashSet::from([
            "Program Files/OldApp".to_string(),
            "Program Files (x86)/OldTool".to_string(),
            "Users/me/AppData/Local/Packages/Gone.App_abcdefghjkmnp".to_string(),
        ]),
        "binaries, registered installs, shared/OS folders, non-package names and other users' packages are not flagged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_store_rule_without_a_package_list() {
    let (root, _g, tree) = build().await;
    let inst = installed(&tree, &root, None); // registry unreadable
    let found = Windows.leftovers(&tree, &inst);
    assert!(names(&tree, &found).iter().all(|n| n.starts_with("Program Files")), "must not guess about Store apps");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leftovers_flow_through_detect_with_and_colour_the_tree() {
    let (root, _g, tree) = build().await;
    let inst = installed(&tree, &root, Some(&["Live.App_abcdefghjkmnp"]));
    let (sugg, risk_of) = cleanup::detect(&tree, &Windows, &inst);
    let old = tree.find(&root.join("Program Files/OldApp").to_string_lossy()).unwrap();
    assert!(sugg.iter().any(|s| s.node == old && s.leftover));
    let inner = tree.find(&root.join("Program Files/OldApp/cache/blob.dat").to_string_lossy()).unwrap();
    assert_eq!(risk_of[inner as usize], Risk::Review as u8, "contents inherit the folder's risk");
}
