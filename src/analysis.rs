//! Post-scan analysis: aggregation, type statistics, large-cluster detection.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::apps::{self, AppInfo};
use crate::cleanup::{self, Suggestion};
use crate::model::{NCAT, Tree, flag};

/// Bottom-up aggregation. Because children always have larger ids than their parent, walking the
/// id range backwards guarantees every subtree is complete before its parent is visited.
pub fn finalize(tree: &mut Tree) {
    let n = tree.nodes.len();
    for id in (1..n).rev() {
        let (size, files, parent) = {
            let nd = &tree.nodes[id];
            (nd.size, nd.files, nd.parent as usize)
        };
        let p = &mut tree.nodes[parent];
        p.size += size;
        p.files += files;
    }

    // Per-directory category byte counts -> dominant category and its share.
    let mut ord = vec![u32::MAX; n];
    let mut ndirs = 0u32;
    for (i, nd) in tree.nodes.iter().enumerate() {
        if nd.is_dir() {
            ord[i] = ndirs;
            ndirs += 1;
        }
    }
    let mut cats = vec![[0u64; NCAT]; ndirs as usize];
    for id in (0..n).rev() {
        if !tree.nodes[id].is_dir() {
            continue;
        }
        let mut arr = [0u64; NCAT];
        let nd = tree.nodes[id];
        for c in nd.child_start..nd.child_start + nd.child_count {
            let ch = &tree.nodes[c as usize];
            if ch.is_dir() {
                let sub = &cats[ord[c as usize] as usize];
                for k in 0..NCAT {
                    arr[k] += sub[k];
                }
            } else {
                arr[ch.cat as usize] += ch.size;
            }
        }
        let total: u64 = arr.iter().sum();
        let (best, best_v) = arr.iter().enumerate().max_by_key(|(_, v)| **v).map(|(i, v)| (i, *v)).unwrap();
        let node = &mut tree.nodes[id];
        node.cat = best as u8;
        node.dom_pct = if total > 0 { (best_v * 100 / total) as u8 } else { 0 };
        cats[ord[id] as usize] = arr;
    }
}

#[derive(Clone, Default)]
pub struct TypeStat {
    pub cat: u8,
    pub size: u64,
    pub files: u64,
}

#[derive(Default)]
pub struct Analysis {
    pub types: Vec<TypeStat>,
    pub apps: Vec<AppInfo>,
    /// Per node: 0 = none, otherwise `apps` index + 1.
    pub app_of: Vec<u16>,
    pub suggestions: Vec<Suggestion>,
    /// Per node: 0 none, 1 safe, 2 review, 3 caution (inherited by descendants).
    pub risk_of: Vec<u8>,
    pub top_files: Vec<u32>,
}

pub fn analyze(tree: &Tree) -> Analysis {
    let ((apps, app_of), (suggestions, risk_of)) = rayon::join(|| apps::detect(tree), || cleanup::detect(tree));

    let mut sizes = [0u64; NCAT];
    let mut counts = [0u64; NCAT];
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::new();
    const TOP: usize = 1000;
    for (id, nd) in tree.nodes.iter().enumerate() {
        if nd.is_dir() || nd.flags & flag::DELETED != 0 {
            continue;
        }
        sizes[nd.cat as usize] += nd.size;
        counts[nd.cat as usize] += 1;
        if nd.size > 0 && (heap.len() < TOP || nd.size > heap.peek().map_or(0, |r| r.0.0)) {
            heap.push(Reverse((nd.size, id as u32)));
            if heap.len() > TOP {
                heap.pop();
            }
        }
    }
    let mut types: Vec<TypeStat> =
        (0..NCAT).map(|c| TypeStat { cat: c as u8, size: sizes[c], files: counts[c] }).collect();
    types.sort_by_key(|t| Reverse(t.size));
    let mut top: Vec<(u64, u32)> = heap.into_iter().map(|r| r.0).collect();
    top.sort_by_key(|&(s, _)| Reverse(s));

    Analysis { types, apps, app_of, suggestions, risk_of, top_files: top.into_iter().map(|(_, i)| i).collect() }
}

#[derive(Clone)]
pub struct Cluster {
    pub node: u32,
    pub size: u64,
    pub files: u64,
    pub cat: u8,
    pub cat_pct: u8,
    /// True when the cluster is only the files sitting directly inside `node`.
    pub loose: bool,
    pub app: u16,
}

/// Large clusters are the places where space actually concentrates: descend from the root through
/// every folder of at least `threshold` bytes; a folder with no sub-folder that big is a cluster
/// (its mass is spread over many small things or files). Files that sit directly in a folder
/// next to big sub-folders form a "loose files" cluster of their own. The result is
/// non-overlapping.
pub fn find_clusters(tree: &Tree, threshold: u64, app_of: &[u16]) -> Vec<Cluster> {
    let mut out = Vec::new();
    if tree.nodes[0].size < threshold.max(1) {
        return out;
    }
    let app = |id: u32| app_of.get(id as usize).copied().unwrap_or(0);
    let mut stack = vec![0u32];
    while let Some(d) = stack.pop() {
        let mut big = 0;
        let mut loose_size = 0u64;
        let mut loose_files = 0u64;
        let mut loose_cat = [0u64; NCAT];
        for c in tree.children(d) {
            let ch = &tree.nodes[c as usize];
            if ch.is_deleted() {
                continue;
            }
            if ch.is_dir() {
                if ch.size >= threshold {
                    stack.push(c);
                    big += 1;
                }
            } else {
                loose_size += ch.size;
                loose_files += 1;
                loose_cat[ch.cat as usize] += ch.size;
            }
        }
        let nd = &tree.nodes[d as usize];
        if big == 0 {
            if nd.size >= threshold {
                out.push(Cluster {
                    node: d,
                    size: nd.size,
                    files: nd.files as u64,
                    cat: nd.cat,
                    cat_pct: nd.dom_pct,
                    loose: false,
                    app: app(d),
                });
            }
        } else if loose_size >= threshold {
            let (best, v) = loose_cat.iter().enumerate().max_by_key(|(_, v)| **v).map(|(i, v)| (i, *v)).unwrap();
            out.push(Cluster {
                node: d,
                size: loose_size,
                files: loose_files,
                cat: best as u8,
                cat_pct: (v * 100 / loose_size.max(1)) as u8,
                loose: true,
                app: app(d),
            });
        }
    }
    out.sort_by_key(|c| Reverse(c.size));
    out.truncate(2000);
    out
}
