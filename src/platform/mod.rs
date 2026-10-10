//! Platform: everything that differs per operating system, behind one seam.
//!
//! The rest of the program (scanner, analysis, cleanup engine, treemap, UI) never mentions Windows or
//! macOS; it asks the [`Platform`]. One adapter is compiled into a release build (`cfg(target_os)`), but
//! the Windows and macOS adapters are both compiled under `cfg(test)`, so their rules are unit-tested on
//! any developer machine against synthetic folder trees.
//!
//! Paths given to rules are *normalised*: lower-case with `/` separators ([`Tree::path_norm`]), so a rule
//! is written once and works on either path style.

use std::collections::HashSet;
use std::sync::Arc;

use crate::apps::AppBuilder;
use crate::cleanup::{Risk, Suggestion};
use crate::dirreader::DirReader;
use crate::model::Tree;

#[cfg(any(windows, test))]
pub mod windows;

#[cfg(any(target_os = "macos", test))]
pub mod macos;

#[cfg(not(any(windows, target_os = "macos")))]
pub mod generic;

/// Folder names that hold regenerable application / browser cache data (Electron, Chromium, Firefox).
/// Each platform decides *where* such folders count as cache.
pub const CACHE_DIR_NAMES: [&str; 10] = [
    "cache",
    "code cache",
    "gpucache",
    "cache2",
    "shadercache",
    "grshadercache",
    "dawncache",
    "dawngraphitecache",
    "dawnwebgpucache",
    "media cache",
];

/// A rule verdict: how risky it is to delete, and why.
pub type RuleHit = (Risk, &'static str);

/// A program the operating system knows about, resolved to its install folder in the scanned tree.
pub struct Registered {
    pub node: u32,
    pub name: String,
    pub publisher: String,
}

/// What the OS says is installed right now (registry on Windows, application bundles on macOS).
#[derive(Default)]
pub struct Installed {
    pub registered: Vec<Registered>,
    /// Install roots of `registered`, for fast lookup.
    pub nodes: HashSet<u32>,
    /// Lower-cased display names of installed programs.
    pub names_lc: HashSet<String>,
    /// Windows: lower-cased Store package family names (`Name_<publisherid>`) known to the OS; `None`
    /// if they could not be read (then no Store-package rule may fire).
    #[cfg_attr(not(any(windows, test)), allow(dead_code))] // used by the Windows adapter
    pub package_families: Option<HashSet<String>>,
    /// Windows: the *current user's* `%LOCALAPPDATA%\Packages`. Other users' packages are never judged,
    /// because their installed-package lists live in registry hives that are not loaded.
    #[cfg_attr(not(any(windows, test)), allow(dead_code))] // used by the Windows adapter
    pub user_packages_dir: Option<String>,
}

pub trait Platform: Send + Sync {
    fn name(&self) -> &'static str;

    /// Where the scan path box starts (system drive, home folder, ...).
    fn default_root(&self) -> String;

    /// The directory reader for this OS. `on_disk`: report allocated size instead of logical length.
    fn dir_reader(&self, on_disk: bool) -> Arc<dyn DirReader>;

    /// "Recycle Bin" / "Trash", and the sentence shown when deleting to it.
    fn trash_name(&self) -> &'static str;
    fn trash_note(&self) -> &'static str;

    /// Show the item in the system file manager (fire and forget).
    fn reveal(&self, path: &str);

    /// Programs the OS reports as installed, resolved against `tree`.
    fn installed(&self, tree: &Tree) -> Installed;

    /// Register application roots (system areas, program folders, per-user app data, user folders).
    /// `inst.registered` entries have already been added by the engine.
    fn app_roots(&self, b: &mut AppBuilder, inst: &Installed);

    /// OS-specific folder / file rules, consulted after the portable ones. `name_lc` is lower-case.
    fn classify_dir(&self, tree: &Tree, id: u32, name_lc: &str) -> Option<RuleHit>;
    fn classify_file(&self, tree: &Tree, id: u32, name_lc: &str, size: u64) -> Option<RuleHit>;

    /// Extensions treated as installers when found in an old Downloads folder.
    fn installer_exts(&self) -> &'static [&'static str];

    /// Paths the "large old file" and duplicate rules must leave alone (system and application files).
    fn is_protected(&self, path_norm: &str) -> bool;

    /// Folders left behind by uninstalled programs (always Review).
    fn leftovers(&self, tree: &Tree, inst: &Installed) -> Vec<Suggestion>;
}

/// The adapter for the OS this binary was built for.
pub fn current() -> &'static dyn Platform {
    #[cfg(windows)]
    {
        &windows::Windows
    }
    #[cfg(target_os = "macos")]
    {
        &macos::MacOs
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        &generic::Generic
    }
}
