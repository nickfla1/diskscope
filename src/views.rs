//! Derived lists the UI shows (clusters, cleanup list, duplicate rows), cached against the session's
//! epoch so nothing ever has to remember which caches a mutation invalidates: a new epoch (scan finished,
//! analysis ready, item removed) simply makes every cached list stale, and a changed filter recomputes
//! only its own list. Removed nodes are filtered out here, not by the callers.

use crate::analysis::{self, Cluster};
use crate::dupes::DupGroup;
use crate::session::Snapshot;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeftoverFilter {
    All,
    Only,
    Hide,
}

/// What the cleanup list shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CleanupFilter {
    pub min_bytes: u64,
    /// Safe, Review, Caution.
    pub risk: [bool; 3],
    /// Bit `c` set = show file-type category `c`.
    pub cats: u32,
    pub leftovers: LeftoverFilter,
}

pub enum DupRow {
    Header(usize),
    File(u32),
}

#[derive(Default)]
pub struct Views {
    clusters: Option<(u64, u64, Vec<Cluster>)>,
    cleanup: Option<(u64, CleanupFilter, Vec<usize>)>,
    dup_rows: Option<(u64, Vec<DupRow>)>,
}

impl Views {
    pub fn reset(&mut self) {
        *self = Views::default();
    }

    /// Large clusters of at least `threshold` bytes.
    pub fn clusters(&mut self, epoch: u64, snap: &Snapshot<'_>, threshold: u64) -> &[Cluster] {
        if !matches!(&self.clusters, Some((e, t, _)) if *e == epoch && *t == threshold) {
            let list = analysis::find_clusters(snap.tree(), threshold, &snap.analysis().app_of);
            self.clusters = Some((epoch, threshold, list));
        }
        &self.clusters.as_ref().unwrap().2
    }

    /// Indices into `snap.analysis().suggestions` that pass `filter` and are still on disk.
    pub fn cleanup(&mut self, epoch: u64, snap: &Snapshot<'_>, filter: &CleanupFilter) -> &[usize] {
        if !matches!(&self.cleanup, Some((e, f, _)) if *e == epoch && f == filter) {
            let (t, an) = (snap.tree(), snap.analysis());
            let list = an
                .suggestions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.size >= filter.min_bytes
                        && match filter.leftovers {
                            LeftoverFilter::All => true,
                            LeftoverFilter::Only => s.leftover,
                            LeftoverFilter::Hide => !s.leftover,
                        }
                        && filter.risk[s.risk as usize - 1]
                        && filter.cats >> t.node(s.node).cat & 1 == 1
                        && !t.is_gone(s.node)
                })
                .map(|(i, _)| i)
                .collect();
            self.cleanup = Some((epoch, *filter, list));
        }
        &self.cleanup.as_ref().unwrap().2
    }

    /// Flattened header + file rows of the duplicate groups. `version` changes whenever `groups` does.
    pub fn dup_rows(&mut self, version: u64, groups: &[DupGroup]) -> &[DupRow] {
        if !matches!(&self.dup_rows, Some((v, _)) if *v == version) {
            let mut rows = Vec::new();
            for (gi, g) in groups.iter().enumerate() {
                rows.push(DupRow::Header(gi));
                rows.extend(g.nodes.iter().map(|&n| DupRow::File(n)));
            }
            self.dup_rows = Some((version, rows));
        }
        &self.dup_rows.as_ref().unwrap().1
    }
}
