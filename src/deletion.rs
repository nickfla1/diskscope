//! Deletion: the only module that removes anything from disk.
//!
//! Interface:
//!   * [`plan`] turns a selection into a [`Plan`], enforcing the safety rules (callers cannot bypass them).
//!   * [`run`] executes a plan through a [`Remover`], updating the shared tree as each item goes, reporting
//!     [`Progress`] to an observer, honouring cancellation, and carrying on after individual failures.
//!
//! The seam is [`Remover`]: the Recycle Bin and permanent adapters are the production ones, tests use an
//! in-memory fake.

use std::collections::HashSet;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crate::cleanup::Risk;
use crate::model::Tree;

/// The seam. One call removes one path from disk.
pub trait Remover: Send + Sync {
    fn remove(&self, path: &str, is_dir: bool) -> Result<(), String>;
    /// True if removal cannot be undone (drives wording only).
    fn permanent(&self) -> bool;
}

pub struct RecycleBin;
impl Remover for RecycleBin {
    fn remove(&self, path: &str, _is_dir: bool) -> Result<(), String> {
        trash::delete(path).map_err(|e| e.to_string())
    }
    fn permanent(&self) -> bool {
        false
    }
}

pub struct Permanent;
impl Remover for Permanent {
    fn remove(&self, path: &str, is_dir: bool) -> Result<(), String> {
        if is_dir { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }.map_err(|e| e.to_string())
    }
    fn permanent(&self) -> bool {
        true
    }
}

pub struct Item {
    pub id: u32,
    pub path: String,
    pub is_dir: bool,
}

pub struct Refused {
    pub path: String,
    pub reason: &'static str,
}

#[derive(Default)]
pub struct Plan {
    pub items: Vec<Item>,
    pub refused: Vec<Refused>,
    pub total: u64,
    pub dirs: usize,
}

/// Build a plan. `risk_of` is the per-node cleanup risk map (3 = Caution); pass `&[]` if unknown.
///
/// Rules: the scan root, Caution items, unknown ids and already-removed nodes are refused (with a reason);
/// duplicate ids and items nested inside another selected item are dropped silently (the parent covers them).
pub fn plan(tree: &Tree, ids: &[u32], risk_of: &[u8]) -> Plan {
    let mut plan = Plan::default();
    let eligible = |id: u32| {
        (id as usize) < tree.nodes.len()
            && id != 0
            && !tree.is_gone(id)
            && risk_of.get(id as usize).copied() != Some(Risk::Caution as u8)
    };
    // only eligible items can cover their descendants; a refused parent must not swallow its children
    let selected: HashSet<u32> = ids.iter().copied().filter(|&i| eligible(i)).collect();
    let mut seen = HashSet::new();
    for &id in ids {
        if !seen.insert(id) {
            continue;
        }
        let refuse = |reason| Refused { path: if (id as usize) < tree.nodes.len() { tree.path(id) } else { String::new() }, reason };
        if id as usize >= tree.nodes.len() {
            plan.refused.push(refuse("unknown item"));
        } else if id == 0 {
            plan.refused.push(refuse("the scan root cannot be deleted"));
        } else if tree.is_gone(id) {
            plan.refused.push(refuse("already removed"));
        } else if risk_of.get(id as usize).copied() == Some(Risk::Caution as u8) {
            plan.refused.push(refuse("system-managed (Caution)"));
        } else if tree.ancestors(id).any(|a| selected.contains(&a)) {
            continue;
        } else {
            let n = tree.node(id);
            plan.total += n.size;
            plan.dirs += usize::from(n.is_dir());
            plan.items.push(Item { id, path: tree.path(id), is_dir: n.is_dir() });
        }
    }
    plan
}

#[allow(dead_code)] // Failed carries detail for observers that want it
pub enum Progress<'a> {
    Started { total: usize },
    Working { path: &'a str },
    Removed { id: u32 },
    Failed { path: &'a str, error: &'a str },
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub ok: usize,
    pub failed: Vec<(String, String)>,
    pub cancelled: bool,
    pub permanent: bool,
}

/// Execute a plan. Blocking; call from a worker thread. The tree's write lock is only held for the
/// in-memory removal of each item, never across the file-system call.
pub fn run(
    tree: &RwLock<Tree>,
    plan: Plan,
    remover: &dyn Remover,
    cancel: &AtomicBool,
    observer: &mut dyn FnMut(Progress),
) -> Outcome {
    let mut out = Outcome { permanent: remover.permanent(), ..Outcome::default() };
    observer(Progress::Started { total: plan.items.len() });
    for item in plan.items {
        if cancel.load(Relaxed) {
            out.cancelled = true;
            break;
        }
        observer(Progress::Working { path: &item.path });
        match remover.remove(&item.path, item.is_dir) {
            Ok(()) => {
                tree.write().unwrap().remove(item.id);
                out.ok += 1;
                observer(Progress::Removed { id: item.id });
            }
            Err(e) => {
                observer(Progress::Failed { path: &item.path, error: &e });
                out.failed.push((item.path, e));
            }
        }
    }
    out
}
