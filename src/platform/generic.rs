//! Fallback for operating systems without a dedicated adapter (e.g. Linux): portable rules only,
//! `std::fs` reader, no application attribution.

use std::sync::Arc;

use super::{Installed, Platform, RuleHit};
use crate::apps::AppBuilder;
use crate::cleanup::Suggestion;
use crate::dirreader::DirReader;
use crate::model::Tree;

pub struct Generic;

impl Platform for Generic {
    fn name(&self) -> &'static str {
        "generic"
    }
    fn default_root(&self) -> String {
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    }
    fn dir_reader(&self, on_disk: bool) -> Arc<dyn DirReader> {
        Arc::new(crate::dirreader::StdReader { on_disk, skip: None })
    }
    fn trash_name(&self) -> &'static str {
        "Trash"
    }
    fn trash_note(&self) -> &'static str {
        "Items go to the Trash and can usually be restored from your file manager."
    }
    fn reveal(&self, path: &str) {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
    fn installed(&self, _tree: &Tree) -> Installed {
        Installed::default()
    }
    fn app_roots(&self, _b: &mut AppBuilder, _inst: &Installed) {}
    fn classify_dir(&self, _tree: &Tree, _id: u32, _l: &str) -> Option<RuleHit> {
        None
    }
    fn classify_file(&self, _tree: &Tree, _id: u32, _l: &str, _size: u64) -> Option<RuleHit> {
        None
    }
    fn installer_exts(&self) -> &'static [&'static str] {
        &["iso", "zip", "7z", "rar", "deb", "rpm", "appimage"]
    }
    fn is_protected(&self, p: &str) -> bool {
        ["/usr/", "/bin/", "/sbin/", "/lib/", "/etc/", "/var/lib/", "/boot/"].iter().any(|x| p.contains(x))
    }
    fn leftovers(&self, _tree: &Tree, _inst: &Installed) -> Vec<Suggestion> {
        Vec::new()
    }
}
