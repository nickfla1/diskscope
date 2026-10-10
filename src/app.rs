//! egui front-end: painting and UI state only.
//!
//! Everything else (scan, analysis, treemap rendering, hashing, deletion, the epoch and the derived
//! lists) lives in [`Session`], which the CLI uses too. The UI thread reads the shared tree through a
//! short-lived [`Snapshot`] and paints pre-rendered data, so it stays at full frame rate while a
//! multi-million file scan is running.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::analysis::Analysis;
use crate::apps::AppKind;
use crate::cleanup::Risk;
use crate::deletion::{self, Remover};
use crate::model::{CAT_COLORS, CAT_NAMES, Tree, fmt_count, fmt_size, now_unix};
use crate::session::{Change, Session, Snapshot};
use crate::treemap::{self, ColorMode, app_color};
use crate::views::{CleanupFilter, DupRow, LeftoverFilter};

const ROW_H: f32 = 20.0;
const ROW_H2: f32 = 38.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Folders,
    Apps,
    Clusters,
    Cleanup,
    Duplicates,
    Types,
    Large,
}

impl Tab {
    const ALL: [(Tab, &'static str); 7] = [
        (Tab::Folders, "Folders"),
        (Tab::Apps, "Applications"),
        (Tab::Clusters, "Clusters"),
        (Tab::Cleanup, "Cleanup"),
        (Tab::Duplicates, "Duplicates"),
        (Tab::Types, "Types"),
        (Tab::Large, "Large files"),
    ];
}

/// Deferred UI actions: the panels only *record* what the user did, `apply` performs it afterwards.
enum Action {
    Select(u32),
    SelectZoom(u32),
    Zoom(u32),
    Reveal(u32),
    CopyPath(u32),
    Trash(Vec<u32>),
}

struct DriveInfo {
    mount: String,
    label: String,
    total: u64,
    free: u64,
}

/// The uploaded treemap texture (the pixels themselves live in the session).
#[derive(Default)]
struct TreemapState {
    texture: Option<egui::TextureHandle>,
}

pub struct App {
    session: Session,
    ctx: egui::Context,

    drives: Vec<DriveInfo>,
    path_input: String,
    on_disk: bool,
    color_mode: ColorMode,
    tab: Tab,

    tm: TreemapState,
    treemap_root: u32,
    selected: Option<u32>,
    hover: Option<u32>,

    // folder tree (rebuilt whenever the session epoch changes)
    expanded: HashSet<u32>,
    rows: Vec<(u32, u16)>,
    rows_dirty: bool,
    rows_epoch: u64,
    sorted: HashMap<u32, Vec<u32>>,
    pending_scroll: Option<u32>,

    // clusters
    cluster_mb: f64,

    // cleanup filters
    cleanup_min_mb: f64,
    show_risk: [bool; 3],
    leftover_filter: LeftoverFilter,
    show_cat: u32,
    checked: HashSet<u32>,
    pending_trash: Option<deletion::Plan>,
    delete_permanent: bool,

    // duplicates
    dup_min_mb: f64,
    shot: Option<Shot>,
}

fn list_drives() -> Vec<DriveInfo> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut v: Vec<DriveInfo> = disks
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| DriveInfo {
            mount: d.mount_point().to_string_lossy().to_string(),
            label: d.name().to_string_lossy().to_string(),
            total: d.total_space(),
            free: d.available_space(),
        })
        .collect();
    v.sort_by(|a, b| a.mount.cmp(&b.mount));
    v
}

fn age_str(mtime: u32) -> String {
    if mtime == 0 {
        return "?".into();
    }
    let days = now_unix().saturating_sub(mtime) / 86_400;
    match days {
        0 => "today".into(),
        1..=59 => format!("{days}d ago"),
        60..=729 => format!("{}mo ago", days / 30),
        _ => format!("{}y ago", days / 365),
    }
}

fn risk_color(r: Risk) -> Color32 {
    match r {
        Risk::Safe => Color32::from_rgb(70, 200, 110),
        Risk::Review => Color32::from_rgb(235, 175, 60),
        Risk::Caution => Color32::from_rgb(225, 80, 80),
    }
}

fn cat_color(c: u8) -> Color32 {
    let [r, g, b] = CAT_COLORS[c as usize];
    Color32::from_rgb(r, g, b)
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Allocate a full-width clickable row and paint hover/selection background.
fn row(ui: &mut Ui, h: f32, selected: bool) -> (Rect, egui::Response) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 2.0, ui.visuals().selection.bg_fill.gamma_multiply(0.7));
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 2.0, ui.visuals().widgets.hovered.bg_fill.gamma_multiply(0.5));
    }
    (rect, resp)
}

fn text(ui: &Ui, pos: Pos2, align: Align2, s: impl ToString, color: Color32) {
    ui.painter().text(pos, align, s.to_string(), FontId::proportional(13.0), color);
}

fn chip(ui: &Ui, rect: Rect, color: Color32) {
    ui.painter().rect_filled(rect, 2.0, color);
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let ctx = cc.egui_ctx.clone();
        let wake_ctx = ctx.clone();
        let session = Session::new(rt, crate::platform::current(), Arc::new(move || wake_ctx.request_repaint()));
        let path_input = session.platform().default_root();
        App {
            session,
            ctx,
            drives: list_drives(),
            path_input,
            on_disk: true,
            color_mode: ColorMode::Type,
            tab: Tab::Folders,
            tm: TreemapState::default(),
            treemap_root: 0,
            selected: None,
            hover: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            rows_epoch: 0,
            sorted: HashMap::new(),
            pending_scroll: None,
            cluster_mb: 256.0,
            cleanup_min_mb: 1.0,
            show_risk: [true, true, true],
            leftover_filter: LeftoverFilter::All,
            show_cat: u32::MAX,
            checked: HashSet::new(),
            pending_trash: None,
            delete_permanent: false,
            dup_min_mb: 4.0,
            shot: None,
        }
    }

    // ---------------------------------------------------------------- session glue

    fn start_scan(&mut self) {
        if self.session.is_scanning() {
            return;
        }
        self.tm = TreemapState::default();
        self.checked.clear();
        self.selected = None;
        self.hover = None;
        self.treemap_root = 0;
        self.session.start_scan(PathBuf::from(self.path_input.trim()), self.on_disk);
    }

    fn remover(permanent: bool) -> Arc<dyn Remover> {
        if permanent { Arc::new(deletion::Permanent) } else { Arc::new(deletion::SystemTrash) }
    }

    /// Apply what the session reports to the UI state.
    fn poll(&mut self) {
        for change in self.session.poll() {
            match change {
                Change::ScanFinished { ok: true } => {
                    self.expanded = HashSet::from([0]);
                    if let Some(d) = self.session.data() {
                        let total = d.read().tree().node(0).size;
                        self.cluster_mb = ((total / 200).max(64 << 20) as f64 / 1048576.0).round();
                    }
                }
                Change::Removed(id) => {
                    self.checked.remove(&id);
                    if self.selected == Some(id) {
                        self.selected = None;
                    }
                }
                Change::RenderReady => {
                    if let Some(r) = self.session.rendered() {
                        let img = egui::ColorImage::from_rgba_unmultiplied([r.w, r.h], &r.rgba);
                        let opts = egui::TextureOptions::NEAREST;
                        match &mut self.tm.texture {
                            Some(t) => t.set(img, opts),
                            None => self.tm.texture = Some(self.ctx.load_texture("treemap", img, opts)),
                        }
                    }
                }
                Change::ScanFinished { ok: false } | Change::AnalysisReady | Change::DupsReady | Change::DeleteFinished => {}
            }
        }
        // The folder list is a pure function of the tree, so it follows the session epoch.
        if self.rows_epoch != self.session.epoch() {
            self.rows_epoch = self.session.epoch();
            self.sorted.clear();
            self.rows_dirty = true;
        }
    }

    // ---------------------------------------------------------------- navigation helpers

    fn sorted_children<'a>(sorted: &'a mut HashMap<u32, Vec<u32>>, t: &Tree, id: u32) -> &'a Vec<u32> {
        sorted.entry(id).or_insert_with(|| {
            let mut v: Vec<u32> = t.live_children(id).collect();
            v.sort_unstable_by(|&a, &b| t.node(b).size.cmp(&t.node(a).size));
            v
        })
    }

    fn rebuild_rows(&mut self, t: &Tree) {
        self.rows.clear();
        let mut stack = vec![(0u32, 0u16)];
        while let Some((id, depth)) = stack.pop() {
            self.rows.push((id, depth));
            if t.node(id).is_dir() && self.expanded.contains(&id) {
                let kids = Self::sorted_children(&mut self.sorted, t, id);
                for &c in kids.iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }
        self.rows_dirty = false;
    }

    fn select(&mut self, t: &Tree, id: u32) {
        self.selected = Some(id);
        for a in t.ancestors(id) {
            self.expanded.insert(a);
        }
        self.rows_dirty = true;
        self.pending_scroll = Some(id);
    }

    fn zoom_target(t: &Tree, id: u32) -> u32 {
        if t.node(id).is_dir() { id } else { t.node(id).parent }
    }

    fn apply(&mut self, snap: &Snapshot<'_>, actions: Vec<Action>) {
        let t = snap.tree();
        for a in actions {
            match a {
                Action::Select(id) => self.select(t, id),
                Action::SelectZoom(id) => {
                    self.select(t, id);
                    self.treemap_root = Self::zoom_target(t, id);
                }
                Action::Zoom(id) => self.treemap_root = id,
                Action::Reveal(id) => self.session.platform().reveal(&t.path(id)),
                Action::CopyPath(id) => self.ctx.copy_text(t.path(id)),
                Action::Trash(ids) => {
                    let plan = self.session.plan(snap, &ids);
                    if !plan.items.is_empty() || !plan.refused.is_empty() {
                        self.pending_trash = Some(plan);
                    }
                }
            }
        }
    }

    fn node_menu(ui: &mut Ui, id: u32, actions: &mut Vec<Action>) {
        if ui.button("Zoom treemap here").clicked() {
            actions.push(Action::Zoom(id));
            ui.close();
        }
        if ui.button("Show in file manager").clicked() {
            actions.push(Action::Reveal(id));
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            actions.push(Action::CopyPath(id));
            ui.close();
        }
        ui.separator();
        if ui.button("Delete...").clicked() {
            actions.push(Action::Trash(vec![id]));
            ui.close();
        }
    }

    // ---------------------------------------------------------------- panels

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let sel_text = self
                .drives
                .iter()
                .find(|d| d.mount == self.path_input)
                .map_or("Drive".to_string(), |d| format!("{} {}", d.mount, d.label));
            egui::ComboBox::from_id_salt("drive").selected_text(sel_text).width(150.0).show_ui(ui, |ui| {
                for d in &self.drives {
                    let label = format!("{} {}  ({} free of {})", d.mount, d.label, fmt_size(d.free), fmt_size(d.total));
                    if ui.selectable_label(self.path_input == d.mount, label).clicked() {
                        self.path_input = d.mount.clone();
                    }
                }
            });
            let te = ui.add(egui::TextEdit::singleline(&mut self.path_input).desired_width(260.0).hint_text("folder or drive to scan"));
            if ui.button("Browse...").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.path_input = p.to_string_lossy().to_string();
                }
            }
            let scanning = self.session.is_scanning();
            if scanning {
                if ui.button("Stop").clicked() {
                    self.session.cancel_scan();
                }
            } else if ui.button(egui::RichText::new("Scan").strong()).clicked()
                || (te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                self.start_scan();
            }
            ui.add_enabled(!scanning, egui::Checkbox::new(&mut self.on_disk, "Size on disk"))
                .on_hover_text("Count allocated clusters (what deleting actually frees) instead of logical file length.");

            ui.separator();
            ui.label("Color by");
            for (m, name) in [(ColorMode::Type, "Type"), (ColorMode::App, "Application"), (ColorMode::Cleanup, "Cleanup risk")] {
                ui.selectable_value(&mut self.color_mode, m, name);
            }
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(st) = self.session.delete_status() {
                let verb = if st.permanent { "Deleting" } else { "Trashing" };
                ui.add(
                    egui::ProgressBar::new(if st.total > 0 { st.done as f32 / st.total as f32 } else { 0.0 })
                        .desired_width(260.0)
                        .text(format!("{verb} {}/{}", st.done, st.total)),
                );
                if ui.small_button("Stop").clicked() {
                    self.session.cancel_delete();
                }
                ui.add(egui::Label::new(egui::RichText::new(st.current).weak().size(11.5)).truncate());
                ui.ctx().request_repaint_after(Duration::from_millis(100));
                return;
            }
            if let Some(st) = self.session.scan_status() {
                ui.spinner();
                ui.label(format!(
                    "Scanning... {} files, {} folders, {}  -  {:.0}s{}",
                    fmt_count(st.files),
                    fmt_count(st.dirs),
                    fmt_size(st.bytes),
                    st.elapsed.as_secs_f32(),
                    match st.errors {
                        0 => String::new(),
                        n => format!("  ({n} unreadable)"),
                    }
                ));
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            } else if let Some(d) = self.session.data() {
                let snap = d.read();
                let t = snap.tree();
                let n = t.node(0);
                ui.label(format!("{}  -  {} files  -  {}", t.root_path, fmt_count(n.files as u64), fmt_size(n.size)));
                if let Some(s) = self.session.last_scan() {
                    ui.weak(format!("scanned in {:.1}s", s.elapsed.as_secs_f32()));
                }
                if self.session.is_analyzing() {
                    ui.spinner();
                    ui.weak("analysing...");
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                if let Some(h) = self.hover.filter(|&h| (h as usize) < t.nodes.len()) {
                    ui.separator();
                    ui.label(format!("{}  ({})", t.path(h), fmt_size(t.node(h).size)));
                }
            } else {
                ui.weak("Choose a drive or folder and press Scan.");
            }
            if let Some(n) = &self.session.notice {
                ui.separator();
                ui.colored_label(Color32::from_rgb(120, 200, 140), n);
            }
            if let Some(e) = &self.session.error {
                ui.separator();
                ui.colored_label(Color32::from_rgb(230, 100, 100), e);
            }
        });
    }

    fn tabs_ui(&mut self, ui: &mut Ui, snap: &Snapshot<'_>, actions: &mut Vec<Action>) {
        let (t, an) = (snap.tree(), snap.analysis());
        ui.horizontal_wrapped(|ui| {
            for (tab, name) in Tab::ALL {
                ui.selectable_value(&mut self.tab, tab, name);
            }
        });
        ui.separator();
        match self.tab {
            Tab::Folders => self.tab_folders(ui, t, actions),
            Tab::Apps => self.tab_apps(ui, t, an, actions),
            Tab::Clusters => self.tab_clusters(ui, snap, actions),
            Tab::Cleanup => self.tab_cleanup(ui, snap, actions),
            Tab::Duplicates => self.tab_dups(ui, snap, actions),
            Tab::Types => self.tab_types(ui, t, an),
            Tab::Large => self.tab_large(ui, t, an, actions),
        }
    }

    fn tab_folders(&mut self, ui: &mut Ui, t: &Tree, actions: &mut Vec<Action>) {
        if self.rows_dirty {
            self.rebuild_rows(t);
        }
        let mut scroll = None;
        if let Some(n) = self.pending_scroll.take() {
            if let Some(i) = self.rows.iter().position(|r| r.0 == n) {
                scroll = Some((i as f32 - 3.0).max(0.0) * (ROW_H + ui.spacing().item_spacing.y));
            }
        }
        let mut toggles = Vec::new();
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(off) = scroll {
            area = area.vertical_scroll_offset(off);
        }
        let total = self.rows.len();
        area.show_rows(ui, ROW_H, total, |ui, range| {
            for i in range {
                let (id, depth) = self.rows[i];
                let nd = t.node(id);
                let (rect, resp) = row(ui, ROW_H, self.selected == Some(id));
                let right = rect.right() - 6.0;
                let weak = ui.visuals().weak_text_color();
                let strong = ui.visuals().text_color();
                // columns from the right: files | size | bar
                text(ui, pos2(right, rect.center().y), Align2::RIGHT_CENTER, fmt_count(nd.files as u64), weak);
                text(ui, pos2(right - 66.0, rect.center().y), Align2::RIGHT_CENTER, fmt_size(nd.size), strong);
                let parent_size = if id == 0 { nd.size } else { t.node(nd.parent).size };
                let frac = if parent_size > 0 { nd.size as f32 / parent_size as f32 } else { 0.0 };
                let bar = Rect::from_min_size(pos2(right - 206.0, rect.top() + 4.0), vec2(80.0, ROW_H - 8.0));
                ui.painter().rect_filled(bar, 1.0, Color32::from_gray(40));
                ui.painter().rect_filled(
                    Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())),
                    1.0,
                    cat_color(nd.cat).gamma_multiply(0.85),
                );
                text(ui, pos2(bar.center().x, rect.center().y), Align2::CENTER_CENTER, format!("{:.0}%", frac * 100.0), Color32::WHITE);

                let x0 = rect.left() + 4.0 + depth as f32 * 14.0;
                if nd.is_dir() {
                    let c = pos2(x0 + 6.0, rect.center().y);
                    let open = self.expanded.contains(&id);
                    let pts = if open {
                        vec![c + vec2(-4.0, -2.5), c + vec2(4.0, -2.5), c + vec2(0.0, 3.0)]
                    } else {
                        vec![c + vec2(-2.5, -4.0), c + vec2(3.0, 0.0), c + vec2(-2.5, 4.0)]
                    };
                    ui.painter().add(Shape::convex_polygon(pts, weak, Stroke::NONE));
                    if resp.clicked() && resp.interact_pointer_pos().is_some_and(|p| p.x < x0 + 14.0) {
                        toggles.push(id);
                    }
                }
                chip(ui, Rect::from_min_size(pos2(x0 + 16.0, rect.center().y - 4.0), vec2(8.0, 8.0)), cat_color(nd.cat));
                let name_rect = Rect::from_min_max(pos2(x0 + 28.0, rect.top()), pos2(right - 212.0, rect.bottom()));
                let painter = ui.painter_at(name_rect);
                let mut name = t.name(id).to_string();
                if nd.flags & crate::model::flag::DENIED != 0 {
                    name.push_str("  [access denied]");
                }
                painter.text(pos2(name_rect.left(), rect.center().y), Align2::LEFT_CENTER, name, FontId::proportional(13.0), if nd.is_dir() { strong } else { weak.gamma_multiply(1.4) });

                if resp.clicked() && !toggles.contains(&id) {
                    actions.push(Action::Select(id));
                }
                if resp.double_clicked() {
                    if nd.is_dir() {
                        toggles.push(id);
                    }
                    actions.push(Action::SelectZoom(id));
                }
                resp.context_menu(|ui| Self::node_menu(ui, id, actions));
            }
        });
        for id in toggles {
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
            self.rows_dirty = true;
        }
    }

    fn tab_apps(&mut self, ui: &mut Ui, t: &Tree, an: &Analysis, actions: &mut Vec<Action>) {
        if self.session.is_analyzing() && an.apps.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Detecting applications...");
            });
            return;
        }
        ui.weak("Folders are attributed to applications using the system's list of installed programs, well-known application folders, game libraries and user folders.");
        let total = t.node(0).size.max(1);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H2, an.apps.len(), |ui, range| {
            for i in range {
                let a = &an.apps[i];
                let (rect, resp) = row(ui, ROW_H2, false);
                let right = rect.right() - 6.0;
                chip(ui, Rect::from_min_size(pos2(rect.left() + 4.0, rect.center().y - 6.0), vec2(12.0, 12.0)), rgb(app_color(i as u16 + 1)));
                let name_area = Rect::from_min_max(rect.min + vec2(22.0, 0.0), pos2(right - 130.0, rect.bottom()));
                ui.painter_at(name_area).text(pos2(name_area.left(), rect.top() + 11.0), Align2::LEFT_CENTER, &a.name, FontId::proportional(13.5), ui.visuals().text_color());
                let sub = if a.publisher.is_empty() { a.kind.label().to_string() } else { format!("{}  -  {}", a.kind.label(), a.publisher) };
                ui.painter_at(name_area).text(pos2(name_area.left(), rect.top() + 27.0), Align2::LEFT_CENTER, sub, FontId::proportional(11.5), ui.visuals().weak_text_color());
                text(ui, pos2(right, rect.top() + 11.0), Align2::RIGHT_CENTER, fmt_size(a.size), ui.visuals().text_color());
                text(ui, pos2(right, rect.top() + 27.0), Align2::RIGHT_CENTER, format!("{:.1}%  -  {} files", a.size as f64 * 100.0 / total as f64, fmt_count(a.files)), ui.visuals().weak_text_color());
                let kind_col = match a.kind {
                    AppKind::Installed => Color32::from_rgb(90, 160, 240),
                    AppKind::Game => Color32::from_rgb(230, 140, 70),
                    AppKind::System => Color32::from_rgb(160, 160, 170),
                    AppKind::UserData => Color32::from_rgb(110, 200, 130),
                    AppKind::Folder => Color32::from_rgb(180, 140, 220),
                };
                ui.painter().rect_filled(Rect::from_min_size(pos2(rect.left(), rect.top() + 2.0), vec2(2.0, ROW_H2 - 4.0)), 1.0, kind_col);
                if resp.clicked() {
                    if let Some(&r) = a.roots.first() {
                        actions.push(Action::SelectZoom(r));
                    }
                }
                if let Some(&r) = a.roots.first() {
                    resp.context_menu(|ui| Self::node_menu(ui, r, actions));
                }
            }
        });
    }

    fn tab_clusters(&mut self, ui: &mut Ui, snap: &Snapshot<'_>, actions: &mut Vec<Action>) {
        let (t, an) = (snap.tree(), snap.analysis());
        ui.horizontal(|ui| {
            ui.label("Cluster threshold");
            ui.add(egui::DragValue::new(&mut self.cluster_mb).range(1.0..=1_000_000.0).speed(8.0).suffix(" MiB"));
        });
        ui.weak("Large clusters are where space concentrates: the deepest folders of at least the threshold size, plus loose files sitting beside big sub-folders. They never overlap.");
        let total = t.node(0).size.max(1);
        let clusters = self.session.clusters(snap, (self.cluster_mb * 1048576.0) as u64);
        ui.label(format!("{} clusters, covering {}", clusters.len(), fmt_size(clusters.iter().map(|c| c.size).sum())));
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H2, clusters.len(), |ui, range| {
            for i in range {
                let c = &clusters[i];
                let (rect, resp) = row(ui, ROW_H2, self.selected == Some(c.node));
                let right = rect.right() - 6.0;
                let frac = c.size as f32 / total as f32;
                ui.painter().rect_filled(Rect::from_min_size(pos2(rect.left(), rect.bottom() - 3.0), vec2((rect.width() * frac * 3.0).min(rect.width()), 2.0)), 1.0, cat_color(c.cat));
                chip(ui, Rect::from_min_size(pos2(rect.left() + 4.0, rect.top() + 6.0), vec2(8.0, 8.0)), cat_color(c.cat));
                text(ui, pos2(rect.left() + 18.0, rect.top() + 11.0), Align2::LEFT_CENTER, fmt_size(c.size), ui.visuals().text_color());
                let mut meta = format!("{}% {}  -  {} files", c.cat_pct, CAT_NAMES[c.cat as usize], fmt_count(c.files));
                if c.loose {
                    meta.push_str("  -  loose files");
                }
                if c.app != 0 {
                    if let Some(a) = an.apps.get(c.app as usize - 1) {
                        meta.push_str(&format!("  -  {}", a.name));
                    }
                }
                ui.painter_at(Rect::from_min_max(rect.min, pos2(right, rect.bottom()))).text(pos2(rect.left() + 90.0, rect.top() + 11.0), Align2::LEFT_CENTER, meta, FontId::proportional(12.0), ui.visuals().weak_text_color());
                let path_rect = Rect::from_min_max(pos2(rect.left() + 18.0, rect.top() + 20.0), pos2(right, rect.bottom()));
                ui.painter_at(path_rect).text(pos2(right, rect.top() + 27.0), Align2::RIGHT_CENTER, t.path(c.node), FontId::proportional(11.5), ui.visuals().text_color().gamma_multiply(0.75));
                if resp.clicked() {
                    actions.push(Action::SelectZoom(c.node));
                }
                resp.context_menu(|ui| Self::node_menu(ui, c.node, actions));
            }
        });
    }

    fn tab_cleanup(&mut self, ui: &mut Ui, snap: &Snapshot<'_>, actions: &mut Vec<Action>) {
        let (t, an) = (snap.tree(), snap.analysis());
        if self.session.is_analyzing() && an.suggestions.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking for deletable files...");
            });
            return;
        }
        let (mut safe, mut review) = (0u64, 0u64);
        for s in &an.suggestions {
            if t.is_gone(s.node) {
                continue;
            }
            match s.risk {
                Risk::Safe => safe += s.size,
                Risk::Review => review += s.size,
                Risk::Caution => {}
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(risk_color(Risk::Safe), format!("Safe: {}", fmt_size(safe)));
            ui.colored_label(risk_color(Risk::Review), format!("Review: {}", fmt_size(review)));
            ui.separator();
            ui.label("min size");
            ui.add(egui::DragValue::new(&mut self.cleanup_min_mb).range(0.0..=100_000.0).speed(0.5).suffix(" MiB"));
        });
        ui.horizontal(|ui| {
            for (i, r) in [Risk::Safe, Risk::Review, Risk::Caution].into_iter().enumerate() {
                ui.checkbox(&mut self.show_risk[i], egui::RichText::new(r.label()).color(risk_color(r)));
            }
            ui.separator();
            egui::ComboBox::from_id_salt("leftovers")
                .selected_text(["All suggestions", "Only uninstall leftovers", "Hide uninstall leftovers"][self.leftover_filter as usize])
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.leftover_filter, LeftoverFilter::All, "All suggestions");
                    ui.selectable_value(&mut self.leftover_filter, LeftoverFilter::Only, "Only uninstall leftovers");
                    ui.selectable_value(&mut self.leftover_filter, LeftoverFilter::Hide, "Hide uninstall leftovers");
                })
                .response
                .on_hover_text("Folders left behind by uninstalled programs (for example Program Files folders with nothing runnable in them, or data of Store apps that are no longer installed on Windows).");
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Types:");
            for c in 0..CAT_NAMES.len() {
                let on = self.show_cat >> c & 1 == 1;
                let label = egui::RichText::new(CAT_NAMES[c]).size(12.0).color(if on { cat_color(c as u8) } else { Color32::from_gray(110) });
                if ui.selectable_label(on, label).clicked() {
                    self.show_cat ^= 1 << c;
                }
            }
            if ui.small_button("All").clicked() {
                self.show_cat = u32::MAX;
            }
        });
        let deleting = self.session.is_deleting();
        let filter = CleanupFilter {
            min_bytes: (self.cleanup_min_mb * 1048576.0) as u64,
            risk: self.show_risk,
            cats: self.show_cat,
            leftovers: self.leftover_filter,
        };
        let view = self.session.cleanup(snap, &filter);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Check all Safe").clicked() {
                for &i in view {
                    if an.suggestions[i].risk == Risk::Safe {
                        self.checked.insert(an.suggestions[i].node);
                    }
                }
            }
            if ui.button("Clear").clicked() {
                self.checked.clear();
            }
            let sel: Vec<u32> = self.checked.iter().copied().filter(|&n| !t.is_gone(n)).collect();
            let sel_size: u64 = sel.iter().map(|&n| t.node(n).size).sum();
            if ui.add_enabled(!sel.is_empty() && !deleting, egui::Button::new(format!("Delete {} checked ({}) ...", sel.len(), fmt_size(sel_size)))).clicked() {
                actions.push(Action::Trash(sel));
            }
        });
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H2, view.len(), |ui, range| {
            for i in range {
                let s = &an.suggestions[view[i]];
                let (rect, resp) = row(ui, ROW_H2, self.selected == Some(s.node));
                let right = rect.right() - 6.0;
                let cb = Rect::from_min_size(pos2(rect.left() + 6.0, rect.center().y - 7.0), vec2(14.0, 14.0));
                let checkable = s.risk != Risk::Caution;
                if checkable {
                    let on = self.checked.contains(&s.node);
                    ui.painter().rect_stroke(cb, 2.0, Stroke::new(1.5, Color32::from_gray(150)), StrokeKind::Inside);
                    if on {
                        ui.painter().rect_filled(cb.shrink(3.0), 1.0, Color32::from_rgb(110, 190, 255));
                    }
                }
                ui.painter().rect_filled(Rect::from_min_size(pos2(rect.left() + 28.0, rect.top() + 4.0), vec2(3.0, 14.0)), 1.0, risk_color(s.risk));
                text(ui, pos2(rect.left() + 38.0, rect.top() + 11.0), Align2::LEFT_CENTER, fmt_size(s.size), ui.visuals().text_color());
                ui.painter_at(Rect::from_min_max(pos2(rect.left() + 105.0, rect.top()), pos2(right, rect.bottom()))).text(pos2(rect.left() + 105.0, rect.top() + 11.0), Align2::LEFT_CENTER, format!("{}  -  {}", s.risk.label(), s.reason), FontId::proportional(12.0), risk_color(s.risk));
                let path_rect = Rect::from_min_max(pos2(rect.left() + 28.0, rect.top() + 20.0), pos2(right, rect.bottom()));
                ui.painter_at(path_rect).text(pos2(right, rect.top() + 27.0), Align2::RIGHT_CENTER, t.path(s.node), FontId::proportional(11.5), ui.visuals().text_color().gamma_multiply(0.75));
                if resp.clicked() {
                    let on_cb = resp.interact_pointer_pos().is_some_and(|p| p.x < rect.left() + 26.0);
                    if checkable && on_cb {
                        if !self.checked.remove(&s.node) {
                            self.checked.insert(s.node);
                        }
                    } else {
                        actions.push(Action::SelectZoom(s.node));
                    }
                }
                resp.context_menu(|ui| Self::node_menu(ui, s.node, actions));
            }
        });
    }

    fn tab_dups(&mut self, ui: &mut Ui, snap: &Snapshot<'_>, actions: &mut Vec<Action>) {
        let t = snap.tree();
        let finding = self.session.is_finding_dups();
        let deleting = self.session.is_deleting();
        ui.horizontal(|ui| {
            ui.label("Files at least");
            ui.add_enabled(!finding, egui::DragValue::new(&mut self.dup_min_mb).range(0.1..=100_000.0).speed(0.5).suffix(" MiB"));
            if finding {
                if ui.button("Cancel").clicked() {
                    self.session.cancel_dups();
                }
            } else if ui.button("Find duplicates").clicked() {
                self.session.start_dups((self.dup_min_mb * 1048576.0) as u64);
            }
        });
        if let Some(st) = self.session.dup_status() {
            let label = match st.stage {
                1 => "Grouping files by size...".to_string(),
                2 => format!("Quick-hashing {}/{} candidates", st.done, st.total),
                3 => format!("Full-hashing {}/{} candidates", st.done, st.total),
                _ => "Working...".to_string(),
            };
            ui.add(egui::ProgressBar::new(if st.total > 0 { st.done as f32 / st.total as f32 } else { 0.0 }).text(label).animate(true));
            ui.ctx().request_repaint_after(Duration::from_millis(100));
            return;
        }
        if !self.session.dups_done() {
            ui.weak("Hashes (BLAKE3) only files that share an exact size, first a 64 KiB prefix and then the whole file, in parallel. System and application paths are excluded (hard-linked copies are not real duplicates).");
            return;
        }
        let (groups, rows) = self.session.dup_view();
        let wasted: u64 = groups.iter().map(|g| g.wasted()).sum();
        ui.horizontal(|ui| {
            ui.label(format!("{} groups, {} reclaimable", groups.len(), fmt_size(wasted)));
            if ui.button("Check all but oldest copy").clicked() {
                for g in groups {
                    let keep = g.nodes.iter().copied().min_by_key(|&n| t.node(n).mtime);
                    for &n in &g.nodes {
                        if Some(n) != keep {
                            self.checked.insert(n);
                        }
                    }
                }
            }
            let sel: Vec<u32> = self.checked.iter().copied().filter(|&n| !t.is_gone(n)).collect();
            if ui.add_enabled(!sel.is_empty() && !deleting, egui::Button::new(format!("Delete {} checked...", sel.len()))).clicked() {
                actions.push(Action::Trash(sel));
            }
        });
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for i in range {
                match rows[i] {
                    DupRow::Header(gi) => {
                        let g = &groups[gi];
                        let (rect, _) = row(ui, ROW_H, false);
                        text(ui, pos2(rect.left() + 4.0, rect.center().y), Align2::LEFT_CENTER, format!("{} copies x {}  -  wastes {}", g.nodes.len(), fmt_size(g.size), fmt_size(g.wasted())), Color32::from_rgb(240, 200, 110));
                    }
                    DupRow::File(id) => {
                        let (rect, resp) = row(ui, ROW_H, self.selected == Some(id));
                        let cb = Rect::from_min_size(pos2(rect.left() + 14.0, rect.center().y - 6.0), vec2(12.0, 12.0));
                        ui.painter().rect_stroke(cb, 2.0, Stroke::new(1.5, Color32::from_gray(150)), StrokeKind::Inside);
                        if self.checked.contains(&id) {
                            ui.painter().rect_filled(cb.shrink(3.0), 1.0, Color32::from_rgb(110, 190, 255));
                        }
                        let label = format!("{}   {}", age_str(t.node(id).mtime), t.path(id));
                        ui.painter_at(Rect::from_min_max(pos2(rect.left() + 32.0, rect.top()), rect.max)).text(pos2(rect.right() - 6.0, rect.center().y), Align2::RIGHT_CENTER, label, FontId::proportional(12.0), ui.visuals().text_color());
                        if resp.clicked() {
                            if resp.interact_pointer_pos().is_some_and(|p| p.x < rect.left() + 30.0) {
                                if !self.checked.remove(&id) {
                                    self.checked.insert(id);
                                }
                            } else {
                                actions.push(Action::SelectZoom(id));
                            }
                        }
                        resp.context_menu(|ui| Self::node_menu(ui, id, actions));
                    }
                }
            }
        });
    }

    fn tab_types(&mut self, ui: &mut Ui, t: &Tree, an: &Analysis) {
        let total = t.node(0).size.max(1);
        for ts in an.types.iter().filter(|x| x.size > 0) {
            let (rect, _) = row(ui, 26.0, false);
            let frac = ts.size as f32 / total as f32;
            let bar = Rect::from_min_size(pos2(rect.left() + 150.0, rect.top() + 6.0), vec2((rect.width() - 330.0).max(10.0), 14.0));
            ui.painter().rect_filled(bar, 2.0, Color32::from_gray(38));
            ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 2.0, cat_color(ts.cat));
            chip(ui, Rect::from_min_size(pos2(rect.left() + 4.0, rect.center().y - 5.0), vec2(10.0, 10.0)), cat_color(ts.cat));
            text(ui, pos2(rect.left() + 20.0, rect.center().y), Align2::LEFT_CENTER, CAT_NAMES[ts.cat as usize], ui.visuals().text_color());
            text(ui, pos2(rect.right() - 100.0, rect.center().y), Align2::RIGHT_CENTER, format!("{} ({:.1}%)", fmt_size(ts.size), frac * 100.0), ui.visuals().text_color());
            text(ui, pos2(rect.right() - 4.0, rect.center().y), Align2::RIGHT_CENTER, format!("{} files", fmt_count(ts.files)), ui.visuals().weak_text_color());
        }
    }

    fn tab_large(&mut self, ui: &mut Ui, t: &Tree, an: &Analysis, actions: &mut Vec<Action>) {
        let items: Vec<u32> = an.top_files.iter().copied().filter(|&i| !t.is_gone(i)).collect();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H2, items.len(), |ui, range| {
            for i in range {
                let id = items[i];
                let nd = t.node(id);
                let (rect, resp) = row(ui, ROW_H2, self.selected == Some(id));
                let right = rect.right() - 6.0;
                chip(ui, Rect::from_min_size(pos2(rect.left() + 4.0, rect.top() + 6.0), vec2(8.0, 8.0)), cat_color(nd.cat));
                text(ui, pos2(rect.left() + 18.0, rect.top() + 11.0), Align2::LEFT_CENTER, fmt_size(nd.size), ui.visuals().text_color());
                text(ui, pos2(rect.left() + 100.0, rect.top() + 11.0), Align2::LEFT_CENTER, format!("{}  -  modified {}", CAT_NAMES[nd.cat as usize], age_str(nd.mtime)), ui.visuals().weak_text_color());
                let path_rect = Rect::from_min_max(pos2(rect.left() + 18.0, rect.top() + 20.0), pos2(right, rect.bottom()));
                ui.painter_at(path_rect).text(pos2(right, rect.top() + 27.0), Align2::RIGHT_CENTER, t.path(id), FontId::proportional(11.5), ui.visuals().text_color().gamma_multiply(0.75));
                if resp.clicked() {
                    actions.push(Action::SelectZoom(id));
                }
                resp.context_menu(|ui| Self::node_menu(ui, id, actions));
            }
        });
    }

    // ---------------------------------------------------------------- treemap

    fn legend(&self, ui: &mut Ui, an: &Analysis, t: &Tree) {
        ui.horizontal_wrapped(|ui| {
            let item = |ui: &mut Ui, c: Color32, label: String| {
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, c);
                ui.label(egui::RichText::new(label).size(12.0));
            };
            match self.color_mode {
                ColorMode::Type => {
                    let total = t.node(0).size.max(1);
                    for ts in an.types.iter().filter(|x| x.size > 0).take(9) {
                        item(ui, cat_color(ts.cat), format!("{} {:.0}%", CAT_NAMES[ts.cat as usize], ts.size as f64 * 100.0 / total as f64));
                    }
                }
                ColorMode::App => {
                    for (i, a) in an.apps.iter().enumerate().take(8) {
                        item(ui, rgb(app_color(i as u16 + 1)), a.name.clone());
                    }
                    item(ui, rgb(app_color(0)), "unattributed".into());
                }
                ColorMode::Cleanup => {
                    item(ui, risk_color(Risk::Safe), "Safe to delete".into());
                    item(ui, risk_color(Risk::Review), "Review first".into());
                    item(ui, risk_color(Risk::Caution), "System-managed".into());
                    item(ui, Color32::from_rgb(92, 94, 104), "Keep".into());
                }
            }
        });
    }

    fn treemap_ui(&mut self, ui: &mut Ui, t: &Tree, an: &Analysis, actions: &mut Vec<Action>) {
        // breadcrumb
        ui.horizontal_wrapped(|ui| {
            let mut chain: Vec<u32> = vec![self.treemap_root];
            chain.extend(t.ancestors(self.treemap_root));
            chain.reverse();
            if ui.add_enabled(self.treemap_root != 0, egui::Button::new("Up")).clicked() {
                actions.push(Action::Zoom(t.node(self.treemap_root).parent));
            }
            let n = chain.len();
            for (i, id) in chain.into_iter().enumerate() {
                let label = if id == 0 { t.root_path.clone() } else { t.name(id).to_string() };
                if ui.add(egui::Button::new(label).small().frame(false)).clicked() {
                    actions.push(Action::Zoom(id));
                }
                if i + 1 < n {
                    ui.weak(">");
                }
            }
        });
        self.legend(ui, an, t);

        let avail = ui.available_size();
        let (resp, painter) = ui.allocate_painter(avail, Sense::click());
        let rect = resp.rect;
        let ppp = ui.ctx().pixels_per_point();
        self.session.request_render(self.treemap_root, (rect.width() * ppp) as usize, (rect.height() * ppp) as usize, self.color_mode);

        let (Some(tex), Some(r)) = (&self.tm.texture, self.session.rendered()) else {
            if self.session.render_requested() {
                painter.text(rect.center(), Align2::CENTER_CENTER, "Rendering...", FontId::proportional(16.0), Color32::GRAY);
            }
            self.hover = None;
            return;
        };
        painter.image(tex.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        let to_screen = |b: [f32; 4]| {
            let (sx, sy) = (rect.width() / r.w as f32, rect.height() / r.h as f32);
            Rect::from_min_max(pos2(rect.left() + b[0] * sx, rect.top() + b[1] * sy), pos2(rect.left() + b[2] * sx, rect.top() + b[3] * sy))
        };

        // selection outline: walk up until an ancestor that was laid out is found
        if let Some(sel) = self.selected.filter(|&s| (s as usize) < t.nodes.len() && !t.is_gone(s)) {
            let mut cur = sel;
            loop {
                if let Some(b) = r.rects.get(&cur) {
                    if cur != r.root {
                        painter.rect_stroke(to_screen(*b), 0.0, Stroke::new(2.0, Color32::from_rgb(255, 235, 120)), StrokeKind::Inside);
                    }
                    break;
                }
                if cur == 0 {
                    break;
                }
                cur = t.node(cur).parent;
            }
        }

        let mut hover = None;
        if r.root == self.treemap_root {
            if let Some(p) = resp.hover_pos() {
                let x = (((p.x - rect.left()) / rect.width()) * r.w as f32) as usize;
                let y = (((p.y - rect.top()) / rect.height()) * r.h as f32) as usize;
                if x < r.w && y < r.h {
                    let id = r.ids[y * r.w + x];
                    if id != treemap::NO_ID && (id as usize) < t.nodes.len() && !t.is_gone(id) {
                        hover = Some(id);
                    }
                }
            }
        }
        self.hover = hover;
        if let Some(h) = hover {
            if let Some(b) = r.rects.get(&h) {
                painter.rect_stroke(to_screen(*b), 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Inside);
            }
            let nd = t.node(h);
            let root_size = t.node(r.root).size.max(1);
            let app = an.app_of.get(h as usize).copied().unwrap_or(0);
            let risk = an.risk_of.get(h as usize).copied().unwrap_or(0);
            resp.clone().on_hover_ui_at_pointer(|ui| {
                ui.set_max_width(520.0);
                ui.label(egui::RichText::new(t.name(h)).strong());
                ui.label(egui::RichText::new(t.path(h)).weak().size(11.5));
                ui.label(format!("{}  ({:.2}% of view)", fmt_size(nd.size), nd.size as f64 * 100.0 / root_size as f64));
                if nd.is_dir() {
                    ui.label(format!("{} files  -  mostly {} ({}%)", fmt_count(nd.files as u64), CAT_NAMES[nd.cat as usize], nd.dom_pct));
                } else {
                    ui.label(format!("{}  -  modified {}", CAT_NAMES[nd.cat as usize], age_str(nd.mtime)));
                }
                if app != 0 {
                    if let Some(a) = an.apps.get(app as usize - 1) {
                        ui.label(format!("Application: {}", a.name));
                    }
                }
                if risk != 0 {
                    ui.colored_label(
                        match risk { 1 => risk_color(Risk::Safe), 2 => risk_color(Risk::Review), _ => risk_color(Risk::Caution) },
                        match risk { 1 => "Safe to delete", 2 => "Review before deleting", _ => "System-managed" },
                    );
                }
            });
        }
        if resp.clicked() {
            if let Some(h) = hover {
                actions.push(Action::Select(h));
            }
        }
        if resp.double_clicked() {
            if let Some(h) = hover {
                actions.push(Action::SelectZoom(h));
            }
        }
        let ctx_target = hover;
        resp.context_menu(|ui| match ctx_target {
            Some(id) => Self::node_menu(ui, id, actions),
            None => {
                ui.label("Right-click a tile for actions");
            }
        });
    }

    fn trash_dialog(&mut self, ctx: &egui::Context) {
        let Some(plan) = &self.pending_trash else { return };
        let mut close = false;
        let mut confirm = false;
        let can_run = !plan.items.is_empty();
        egui::Window::new("Delete items")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if can_run {
                    ui.label(format!("{} item(s) ({} folders), {}", plan.items.len(), plan.dirs, fmt_size(plan.total)));
                    ui.add_space(4.0);
                    for it in plan.items.iter().take(8) {
                        ui.label(egui::RichText::new(&it.path).size(12.0));
                    }
                    if plan.items.len() > 8 {
                        ui.weak(format!("... and {} more", plan.items.len() - 8));
                    }
                }
                if !plan.refused.is_empty() {
                    ui.add_space(4.0);
                    ui.colored_label(Color32::from_rgb(235, 175, 60), format!("{} item(s) will be skipped:", plan.refused.len()));
                    for r in plan.refused.iter().take(6) {
                        ui.label(egui::RichText::new(format!("{}  -  {}", r.path, r.reason)).size(12.0).weak());
                    }
                }
                if can_run {
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.delete_permanent, format!("Skip {} (delete permanently)", self.session.platform().trash_name()));
                    if self.delete_permanent {
                        ui.colored_label(Color32::from_rgb(230, 100, 100), "Permanent deletion cannot be undone.");
                    } else {
                        ui.weak(self.session.platform().trash_note());
                    }
                }
                ui.add_space(6.0);
                let perm = self.delete_permanent;
                ui.horizontal(|ui| {
                    if can_run && ui.button(if perm { "Delete permanently".to_string() } else { format!("Move to {}", self.session.platform().trash_name()) }).clicked() {
                        confirm = true;
                    }
                    if ui.button(if can_run { "Cancel" } else { "OK" }).clicked() {
                        close = true;
                    }
                });
            });
        if confirm {
            if let Some(plan) = self.pending_trash.take() {
                let remover = Self::remover(self.delete_permanent);
                self.session.delete(plan, remover);
            }
        } else if close {
            self.pending_trash = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        self.drive_shot(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        let data = self.session.data();
        let mut actions: Vec<Action> = Vec::new();
        if let Some(d) = &data {
            let snap = d.read();
            egui::Panel::left("tabs").resizable(true).default_size(560.0).min_size(380.0).show(ui, |ui| self.tabs_ui(ui, &snap, &mut actions));
            egui::CentralPanel::default().show(ui, |ui| self.treemap_ui(ui, snap.tree(), snap.analysis(), &mut actions));
            self.trash_dialog(&ctx);
            self.apply(&snap, actions);
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    if let Some(st) = self.session.scan_status() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() * 0.4);
                            ui.spinner();
                            ui.heading(format!("{} files", fmt_count(st.files)));
                            ui.label(format!("{} in {} folders", fmt_size(st.bytes), fmt_count(st.dirs)));
                        });
                    } else {
                        ui.heading("DiskScope");
                    }
                });
            });
        }
    }
}

/// Hidden verification mode (`diskscope --shot out.png <path> [tab]`): scan, wait until everything
/// is rendered, save a screenshot of the window and exit.
pub struct Shot {
    pub path: PathBuf,
    pub stable: u32,
    pub requested: bool,
}

impl App {
    pub fn enable_shot(&mut self, out: PathBuf, scan_path: String, tab: Option<&str>) {
        self.path_input = scan_path;
        if let Some(t) = tab {
            if let Some((tab, _)) = Tab::ALL.iter().find(|(_, n)| n.eq_ignore_ascii_case(t)) {
                self.tab = *tab;
            }
        }
        self.shot = Some(Shot { path: out, stable: 0, requested: false });
        self.start_scan();
    }

    fn drive_shot(&mut self, ctx: &egui::Context) {
        let ready = self.session.data().is_some() && !self.session.is_analyzing() && self.session.render_is_current();
        let Some(s) = &mut self.shot else { return };
        ctx.request_repaint();
        if ready {
            s.stable += 1;
        }
        if s.stable == 12 && !s.requested {
            s.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Screenshot { image, .. } = ev {
                let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                if let Ok(f) = std::fs::File::create(&s.path) {
                    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), image.size[0] as u32, image.size[1] as u32);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    if let Ok(mut w) = enc.write_header() {
                        let _ = w.write_image_data(&rgba);
                    }
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}
