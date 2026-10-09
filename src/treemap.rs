//! Cushion treemap: squarified layout + CPU rasterisation into a texture and a per-pixel id buffer.
//!
//! The treemap is rendered *once* per (root, size, colour mode) on a background thread and uploaded as a
//! single texture, so the cost per UI frame is one textured quad no matter how many millions of files
//! are shown. The id buffer makes hover/click hit-testing O(1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::model::{CAT_COLORS, Tree};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorMode {
    Type,
    App,
    Cleanup,
}

pub struct ColorSource<'a> {
    pub mode: ColorMode,
    pub app_of: &'a [u16],
    pub risk_of: &'a [u8],
}

pub const NO_ID: u32 = u32::MAX;

pub struct Rendered {
    pub root: u32,
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
    pub ids: Vec<u32>,
    /// Rectangles (x0, y0, x1, y1) of every node that was laid out, for selection outlines.
    pub rects: HashMap<u32, [f32; 4]>,
}

pub fn app_color(app: u16) -> [u8; 3] {
    if app == 0 {
        return [88, 90, 98];
    }
    // golden-angle hue walk gives well separated colours for consecutive ids
    let hue = (app as f32 * 137.508) % 360.0;
    hsv(hue, 0.55, 0.95)
}

fn hsv(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8]
}

#[derive(Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

struct Leaf {
    id: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    /// Cushion surface coefficients (s1x, s2x, s1y, s2y).
    s: [f32; 4],
    color: [f32; 3],
    /// Background fill only (no shading) - makes gaps inside a folder hit-testable.
    bg: bool,
}

struct Ctx<'a> {
    tree: &'a Tree,
    cs: &'a ColorSource<'a>,
    leaves: Vec<Leaf>,
    rects: HashMap<u32, [f32; 4]>,
    w: i32,
    h: i32,
    counter: &'a AtomicU64,
    generation: u64,
    stale: bool,
}

const RIDGE_H: f32 = 0.38;
const RIDGE_F: f32 = 0.91;
const MIN_RECURSE: f64 = 5.0;

fn add_ridge(s: &mut [f32; 4], r: &Rect, h: f32) {
    let (w, hh) = ((r.x1 - r.x0) as f32, (r.y1 - r.y0) as f32);
    if w <= 0.0 || hh <= 0.0 {
        return;
    }
    s[1] -= 4.0 * h / w;
    s[0] += 4.0 * h * ((r.x1 + r.x0) as f32) / w;
    s[3] -= 4.0 * h / hh;
    s[2] += 4.0 * h * ((r.y1 + r.y0) as f32) / hh;
}

fn node_color(cx: &Ctx, id: u32) -> [f32; 3] {
    let nd = cx.tree.node(id);
    let c = match cx.cs.mode {
        ColorMode::Type => CAT_COLORS[nd.cat as usize],
        ColorMode::App => app_color(cx.cs.app_of.get(id as usize).copied().unwrap_or(0)),
        ColorMode::Cleanup => match cx.cs.risk_of.get(id as usize).copied().unwrap_or(0) {
            1 => [70, 200, 110],
            2 => [235, 175, 60],
            3 => [225, 80, 80],
            _ => [92, 94, 104],
        },
    };
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0]
}

fn ratio(sum: f64, amax: f64, amin: f64, side: f64) -> f64 {
    let s2 = sum * sum;
    let l2 = side * side;
    (l2 * amax / s2).max(s2 / (l2 * amin))
}

fn squarify(items: &[(u32, f64)], mut r: Rect, out: &mut Vec<(u32, Rect)>) {
    let mut i = 0;
    while i < items.len() {
        let w = r.x1 - r.x0;
        let h = r.y1 - r.y0;
        if w <= 0.0 || h <= 0.0 {
            break;
        }
        let side = w.min(h);
        let mut sum = items[i].1;
        let mut worst = ratio(sum, items[i].1, items[i].1, side);
        let mut j = i + 1;
        while j < items.len() {
            let a = items[j].1;
            let ns = sum + a;
            let nw = ratio(ns, items[i].1, a, side);
            if nw > worst {
                break;
            }
            sum = ns;
            worst = nw;
            j += 1;
        }
        if w >= h {
            let sw = sum / h;
            let mut y = r.y0;
            for it in &items[i..j] {
                let hh = it.1 / sw;
                out.push((it.0, Rect { x0: r.x0, y0: y, x1: r.x0 + sw, y1: y + hh }));
                y += hh;
            }
            r.x0 += sw;
        } else {
            let sh = sum / w;
            let mut x = r.x0;
            for it in &items[i..j] {
                let ww = it.1 / sh;
                out.push((it.0, Rect { x0: x, y0: r.y0, x1: x + ww, y1: r.y0 + sh }));
                x += ww;
            }
            r.y0 += sh;
        }
        i = j;
    }
}

fn layout(cx: &mut Ctx, dir: u32, r: Rect, surf: [f32; 4], h: f32) {
    if cx.stale {
        return;
    }
    if cx.counter.load(Relaxed) != cx.generation {
        cx.stale = true;
        return;
    }
    let mut kids: Vec<(u32, u64)> = cx
        .tree
        .live_children(dir)
        .filter_map(|c| {
            let s = cx.tree.node(c).size;
            (s > 0).then_some((c, s))
        })
        .collect();
    if kids.is_empty() {
        return;
    }
    kids.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    let total: u64 = kids.iter().map(|k| k.1).sum();
    let scale = (r.x1 - r.x0) * (r.y1 - r.y0) / total as f64;
    let mut items: Vec<(u32, f64)> = Vec::with_capacity(kids.len().min(4096));
    for (id, s) in kids {
        let a = s as f64 * scale;
        if a < 0.3 {
            break; // sorted descending: everything after this is sub-pixel
        }
        items.push((id, a));
    }
    let mut placed = Vec::with_capacity(items.len());
    squarify(&items, r, &mut placed);

    for (id, cr) in placed {
        let (ix0, iy0) = (cr.x0.round() as i32, cr.y0.round() as i32);
        let (ix1, iy1) = (cr.x1.round() as i32, cr.y1.round() as i32);
        if ix1 <= ix0 || iy1 <= iy0 || ix0 >= cx.w || iy0 >= cx.h {
            continue;
        }
        let mut s = surf;
        add_ridge(&mut s, &cr, h);
        cx.rects.insert(id, [cr.x0 as f32, cr.y0 as f32, cr.x1 as f32, cr.y1 as f32]);
        let big = (cr.x1 - cr.x0) >= MIN_RECURSE && (cr.y1 - cr.y0) >= MIN_RECURSE;
        if cx.tree.node(id).is_dir() && big {
            cx.leaves.push(Leaf { id, x0: ix0, y0: iy0, x1: ix1, y1: iy1, s, color: [0.12, 0.12, 0.14], bg: true });
            layout(cx, id, cr, s, h * RIDGE_F);
        } else {
            let color = node_color(cx, id);
            cx.leaves.push(Leaf { id, x0: ix0, y0: iy0, x1: ix1, y1: iy1, s, color, bg: false });
        }
    }
}

/// Returns `None` if a newer render request superseded this one (`counter != generation`).
pub fn render(
    tree: &Tree,
    root: u32,
    w: usize,
    h: usize,
    cs: &ColorSource,
    counter: &AtomicU64,
    generation: u64,
) -> Option<Rendered> {
    let w = w.clamp(8, 8192);
    let h = h.clamp(8, 8192);
    let mut cx = Ctx {
        tree,
        cs,
        leaves: Vec::new(),
        rects: HashMap::new(),
        w: w as i32,
        h: h as i32,
        counter,
        generation,
        stale: false,
    };
    let full = Rect { x0: 0.0, y0: 0.0, x1: w as f64, y1: h as f64 };
    cx.rects.insert(root, [0.0, 0.0, w as f32, h as f32]);
    // A single file as root fills the whole area.
    if tree.node(root).is_dir() {
        layout(&mut cx, root, full, [0.0; 4], RIDGE_H);
    } else {
        let mut s = [0.0; 4];
        add_ridge(&mut s, &full, RIDGE_H);
        let color = node_color(&cx, root);
        cx.leaves.push(Leaf { id: root, x0: 0, y0: 0, x1: w as i32, y1: h as i32, s, color, bg: false });
    }
    if cx.stale || counter.load(Relaxed) != generation {
        return None;
    }

    let mut rgba = vec![0u8; w * h * 4];
    for px in rgba.chunks_exact_mut(4) {
        px.copy_from_slice(&[24, 24, 28, 255]);
    }
    let mut ids = vec![NO_ID; w * h];

    // Light from the upper left, as in WinDirStat.
    let (lx, ly, lz) = (-1.0f32, -1.0f32, 10.0f32);
    let ln = (lx * lx + ly * ly + lz * lz).sqrt();
    let (lx, ly, lz) = (lx / ln, ly / ln, lz / ln);
    const AMBIENT: f32 = 0.38;

    for leaf in &cx.leaves {
        let x0 = leaf.x0.max(0) as usize;
        let y0 = leaf.y0.max(0) as usize;
        let x1 = (leaf.x1 as usize).min(w);
        let y1 = (leaf.y1 as usize).min(h);
        for y in y0..y1 {
            let row = y * w;
            if leaf.bg {
                for x in x0..x1 {
                    ids[row + x] = leaf.id;
                    rgba[(row + x) * 4..(row + x) * 4 + 3].copy_from_slice(&[24, 24, 28]);
                }
                continue;
            }
            let ny = -(2.0 * leaf.s[3] * (y as f32 + 0.5) + leaf.s[2]);
            for x in x0..x1 {
                let nx = -(2.0 * leaf.s[1] * (x as f32 + 0.5) + leaf.s[0]);
                let cosa = (nx * lx + ny * ly + lz) / (nx * nx + ny * ny + 1.0).sqrt();
                let i = (AMBIENT + (1.0 - AMBIENT) * cosa.max(0.0)).min(1.0);
                let o = (row + x) * 4;
                rgba[o] = (leaf.color[0] * i * 255.0) as u8;
                rgba[o + 1] = (leaf.color[1] * i * 255.0) as u8;
                rgba[o + 2] = (leaf.color[2] * i * 255.0) as u8;
                ids[row + x] = leaf.id;
            }
        }
    }
    Some(Rendered { root, w, h, rgba, ids, rects: cx.rects })
}
