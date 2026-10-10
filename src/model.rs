//! Compact arena-based file tree.
//!
//! All nodes live in one `Vec<Node>` and all names in one byte arena. The children of a
//! directory occupy a *contiguous id range* and a child's id is always greater than its
//! parent's id. That lets every aggregation (sizes, file counts, categories, application
//! attribution) run as a single linear pass over the vector, with no recursion, no
//! per-node allocations and no pointer chasing.

use std::ops::Range;
use std::path::MAIN_SEPARATOR;
use std::time::SystemTime;

pub const NCAT: usize = 11;
pub const CAT_NAMES: [&str; NCAT] = [
    "Other",
    "Video",
    "Audio",
    "Images",
    "Documents",
    "Archives",
    "Code",
    "Executables & libs",
    "Disk images & VMs",
    "Databases",
    "Logs & temp",
];
pub const CAT_COLORS: [[u8; 3]; NCAT] = [
    [150, 150, 162],
    [226, 92, 92],
    [236, 150, 64],
    [96, 190, 110],
    [86, 150, 230],
    [170, 110, 220],
    [64, 190, 200],
    [230, 210, 80],
    [226, 100, 172],
    [150, 176, 70],
    [170, 128, 96],
];

pub mod flag {
    pub const DIR: u8 = 1;
    /// Cloud placeholder (OneDrive etc.): logical size is not on disk, counted as 0.
    pub const CLOUD: u8 = 2;
    pub const HIDDEN: u8 = 4;
    pub const SYSTEM: u8 = 8;
    pub const DELETED: u8 = 16;
    /// Directory could not be read (access denied).
    pub const DENIED: u8 = 32;
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// File size, or aggregated subtree size for directories.
    pub size: u64,
    pub name_off: u32,
    pub parent: u32,
    pub child_start: u32,
    pub child_count: u32,
    /// File count in the subtree (1 for files).
    pub files: u32,
    /// Modification time, seconds since the unix epoch.
    pub mtime: u32,
    pub name_len: u16,
    /// Files: extension category. Directories: dominant category of their content.
    pub cat: u8,
    pub flags: u8,
    /// Directories: percentage of the subtree bytes belonging to `cat`.
    pub dom_pct: u8,
}

impl Node {
    pub const EMPTY: Node = Node {
        size: 0,
        name_off: 0,
        parent: 0,
        child_start: 0,
        child_count: 0,
        files: 0,
        mtime: 0,
        name_len: 0,
        cat: 0,
        flags: 0,
        dom_pct: 0,
    };
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flag::DIR != 0
    }
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & flag::DELETED != 0
    }
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub names: Vec<u8>,
    pub root_path: String,
    #[allow(dead_code)]
    pub scanned_at: SystemTime,
}

/// Path separators accepted by [`Tree::find`]. On Unix a backslash is an ordinary file-name character.
#[cfg(windows)]
fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}
#[cfg(not(windows))]
#[allow(dead_code)] // only the Windows adapter (and tests) resolve paths with `find`
fn is_sep(c: char) -> bool {
    c == '/'
}

impl Tree {
    #[inline]
    pub fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }

    #[inline]
    pub fn name(&self, id: u32) -> &str {
        let n = &self.nodes[id as usize];
        let b = &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize];
        // SAFETY: names are produced by `String::from_utf8_lossy`/`to_string_lossy` in the
        // scanner and only ever appended whole, so every slice is valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    }

    #[inline]
    pub fn children(&self, id: u32) -> Range<u32> {
        let n = &self.nodes[id as usize];
        n.child_start..n.child_start + n.child_count
    }

    /// Lower-case path with `/` separators, whatever the platform: the form platform rules match against.
    pub fn path_norm(&self, id: u32) -> String {
        let p = self.path(id).to_lowercase();
        if MAIN_SEPARATOR == '/' { p } else { p.replace(MAIN_SEPARATOR, "/") }
    }

    pub fn path(&self, id: u32) -> String {
        if id == 0 {
            return self.root_path.clone();
        }
        let mut comps = Vec::new();
        let mut cur = id;
        while cur != 0 {
            comps.push(self.name(cur));
            cur = self.nodes[cur as usize].parent;
        }
        let mut s = self.root_path.trim_end_matches(MAIN_SEPARATOR).to_string();
        for c in comps.iter().rev() {
            s.push(MAIN_SEPARATOR);
            s.push_str(c);
        }
        s
    }

    pub fn child_named(&self, dir: u32, name: &str) -> Option<u32> {
        if !self.nodes[dir as usize].is_dir() {
            return None;
        }
        self.children(dir)
            .find(|&c| !self.nodes[c as usize].is_deleted() && self.name(c).eq_ignore_ascii_case(name))
    }

    /// Resolve an absolute path (case-insensitive) to a node.
    #[cfg_attr(not(any(windows, test)), allow(dead_code))] // used by the Windows adapter
    pub fn find(&self, p: &str) -> Option<u32> {
        let root = self.root_path.trim_end_matches(is_sep);
        let pl = p.trim_end_matches(is_sep);
        if pl.len() < root.len() || !pl.is_char_boundary(root.len()) {
            return None;
        }
        // case-insensitive, and `/` matches `\` on Windows (callers may build paths with either)
        let same_prefix = pl[..root.len()]
            .chars()
            .zip(root.chars())
            .all(|(a, b)| (is_sep(a) && is_sep(b)) || a.eq_ignore_ascii_case(&b));
        if !same_prefix {
            return None;
        }
        let rest = &pl[root.len()..];
        if !rest.is_empty() && !rest.starts_with(is_sep) {
            return None;
        }
        let mut cur = 0;
        for comp in rest.split(is_sep).filter(|c| !c.is_empty()) {
            cur = self.child_named(cur, comp)?;
        }
        Some(cur)
    }

    pub fn ancestors(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        let mut cur = id;
        std::iter::from_fn(move || {
            if cur == 0 {
                return None;
            }
            cur = self.nodes[cur as usize].parent;
            Some(cur)
        })
    }

    /// True if the node or any ancestor has been removed.
    pub fn is_gone(&self, id: u32) -> bool {
        self.nodes[id as usize].is_deleted()
            || self.ancestors(id).any(|a| self.nodes[a as usize].is_deleted())
    }

    /// Mark a subtree as deleted and subtract it from every ancestor.
    pub fn remove(&mut self, id: u32) {
        if id == 0 || self.nodes[id as usize].is_deleted() {
            return;
        }
        let (size, files) = (self.nodes[id as usize].size, self.nodes[id as usize].files);
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let node = &mut self.nodes[n as usize];
            node.flags |= flag::DELETED;
            node.size = 0;
            node.files = 0;
            if node.is_dir() {
                stack.extend(node.child_start..node.child_start + node.child_count);
            }
        }
        let mut p = self.nodes[id as usize].parent;
        loop {
            let n = &mut self.nodes[p as usize];
            n.size = n.size.saturating_sub(size);
            n.files = n.files.saturating_sub(files);
            if p == 0 {
                break;
            }
            p = n.parent;
        }
    }

    pub fn live_children(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        self.children(id).filter(|&c| {
            let n = &self.nodes[c as usize];
            !n.is_deleted() && (n.size > 0 || n.is_dir())
        })
    }
}

/// Map a file name to one of the `CAT_NAMES` categories by extension.
pub fn category_of(name: &str) -> u8 {
    let Some(dot) = name.rfind('.') else { return 0 };
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > 8 {
        return 0;
    }
    let mut buf = [0u8; 8];
    for (i, b) in ext.bytes().enumerate() {
        buf[i] = b.to_ascii_lowercase();
    }
    match &buf[..ext.len()] {
        b"mp4" | b"mkv" | b"avi" | b"mov" | b"wmv" | b"flv" | b"webm" | b"m4v" | b"mpg" | b"mpeg" | b"m2ts"
        | b"vob" | b"3gp" => 1,
        b"mp3" | b"wav" | b"flac" | b"aac" | b"ogg" | b"m4a" | b"wma" | b"opus" | b"aiff" | b"mid" => 2,
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"bmp" | b"tif" | b"tiff" | b"webp" | b"heic" | b"heif" | b"raw"
        | b"cr2" | b"nef" | b"arw" | b"dng" | b"psd" | b"svg" | b"ico" | b"xcf" => 3,
        b"pdf" | b"doc" | b"docx" | b"xls" | b"xlsx" | b"ppt" | b"pptx" | b"txt" | b"rtf" | b"odt" | b"ods"
        | b"odp" | b"md" | b"csv" | b"epub" | b"mobi" | b"one" | b"pst" | b"ost" => 4,
        b"zip" | b"rar" | b"7z" | b"tar" | b"gz" | b"tgz" | b"bz2" | b"xz" | b"zst" | b"cab" | b"lz4" => 5,
        b"rs" | b"c" | b"h" | b"cpp" | b"hpp" | b"cc" | b"cs" | b"java" | b"py" | b"js" | b"ts" | b"tsx"
        | b"jsx" | b"go" | b"rb" | b"php" | b"swift" | b"kt" | b"html" | b"htm" | b"css" | b"json" | b"xml"
        | b"yml" | b"yaml" | b"toml" | b"sh" | b"ps1" | b"bat" | b"cmd" | b"sql" | b"lua" | b"map" => 6,
        b"exe" | b"dll" | b"sys" | b"msi" | b"ocx" | b"cpl" | b"scr" | b"jar" | b"apk" | b"rlib" | b"lib"
        | b"obj" | b"pdb" | b"node" | b"o" | b"a" | b"so" | b"ilk" | b"exp" | b"mui" => 7,
        b"iso" | b"img" | b"vhd" | b"vhdx" | b"vmdk" | b"vdi" | b"qcow2" | b"wim" | b"esd" | b"dmg" | b"ova"
        | b"ovf" | b"vmem" | b"vmss" | b"nvram" => 8,
        b"db" | b"sqlite" | b"sqlite3" | b"mdb" | b"accdb" | b"mdf" | b"ldf" | b"sdf" | b"edb" | b"ldb"
        | b"parquet" | b"bson" => 9,
        b"log" | b"tmp" | b"temp" | b"dmp" | b"mdmp" | b"etl" | b"evtx" | b"bak" | b"old" | b"chk" | b"swp" => 10,
        _ => 0,
    }
}

/// Binary-unit size formatting ("1.5 GiB").
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn now_unix() -> u32 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}
