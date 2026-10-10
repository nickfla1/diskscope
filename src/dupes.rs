//! Duplicate file detection: size -> partial hash (64 KiB) -> full BLAKE3, each stage parallel.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;

use crate::model::{Tree, flag};
use crate::platform::Platform;

#[derive(Default)]
pub struct DupProgress {
    /// 0 idle, 1 grouping by size, 2 partial hash, 3 full hash, 4 done
    pub stage: AtomicU8,
    pub total: AtomicU64,
    pub done: AtomicU64,
}

#[derive(Clone)]
pub struct DupGroup {
    pub size: u64,
    pub nodes: Vec<u32>,
}

impl DupGroup {
    pub fn wasted(&self) -> u64 {
        self.size * (self.nodes.len() as u64 - 1)
    }
}

fn hash_file(path: &PathBuf, limit: Option<usize>) -> Option<[u8; 32]> {
    let mut f = File::open(path).ok()?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut left = limit.unwrap_or(usize::MAX);
    while left > 0 {
        let want = buf.len().min(left);
        let n = f.read(&mut buf[..want]).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        left -= n;
    }
    Some(*h.finalize().as_bytes())
}

fn regroup(
    groups: Vec<Vec<(u32, PathBuf)>>,
    limit: Option<usize>,
    prog: &DupProgress,
    cancel: &AtomicBool,
) -> Vec<Vec<(u32, PathBuf)>> {
    let total: usize = groups.iter().map(|g| g.len()).sum();
    prog.total.store(total as u64, Relaxed);
    prog.done.store(0, Relaxed);
    groups
        .into_par_iter()
        .flat_map(|g| {
            let mut by_hash: HashMap<[u8; 32], Vec<(u32, PathBuf)>> = HashMap::new();
            for (id, p) in g {
                if cancel.load(Relaxed) {
                    return Vec::new();
                }
                if let Some(h) = hash_file(&p, limit) {
                    by_hash.entry(h).or_default().push((id, p));
                }
                prog.done.fetch_add(1, Relaxed);
            }
            by_hash.into_values().filter(|v| v.len() > 1).collect::<Vec<_>>()
        })
        .collect()
}

pub fn find(
    tree: &RwLock<Tree>,
    min_size: u64,
    prog: &DupProgress,
    cancel: &AtomicBool,
    platform: &dyn Platform,
) -> Vec<DupGroup> {
    prog.stage.store(1, Relaxed);
    let mut size_of: Vec<u64> = Vec::new();
    let groups: Vec<Vec<(u32, PathBuf)>> = {
        let t = tree.read().unwrap();
        size_of.resize(t.nodes.len(), 0);
        let mut by_size: HashMap<u64, Vec<u32>> = HashMap::new();
        for (id, nd) in t.nodes.iter().enumerate() {
            if nd.is_dir() || nd.size < min_size || nd.flags & (flag::DELETED | flag::CLOUD) != 0 {
                continue;
            }
            size_of[id] = nd.size;
            by_size.entry(nd.size).or_default().push(id as u32);
        }
        by_size
            .into_values()
            .filter(|v| v.len() > 1)
            .map(|v| {
                v.into_iter()
                    .filter_map(|id| {
                        let p = t.path(id);
                        (!platform.is_protected(&t.path_norm(id))).then(|| (id, PathBuf::from(p)))
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| v.len() > 1)
            .collect()
    };

    prog.stage.store(2, Relaxed);
    let groups = regroup(groups, Some(64 * 1024), prog, cancel);
    prog.stage.store(3, Relaxed);
    let groups = regroup(groups, None, prog, cancel);

    let mut out: Vec<DupGroup> = groups
        .into_iter()
        .map(|g| DupGroup { size: size_of[g[0].0 as usize], nodes: g.into_iter().map(|(id, _)| id).collect() })
        .collect();
    out.sort_by_key(|g| std::cmp::Reverse(g.wasted()));
    prog.stage.store(4, Relaxed);
    out
}
