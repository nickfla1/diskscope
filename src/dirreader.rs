//! DirReader: the seam under the scanner. One call lists one directory.
//!
//! Interface: `read(dir, out) -> bool`. The reader pushes each entry into the [`DirBuilder`] and returns
//! `false` if the directory could not be read at all (access denied, vanished). Symlinks and junctions
//! must not be pushed (they would double-count and can loop).
//!
//! Adapters: [`Win32Reader`] (bulk `GetFileInformationByHandleEx`, Windows only), [`StdReader`]
//! (`std::fs::read_dir`, portable), and fakes in tests. A raw-MFT reader would be a further adapter.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use crate::model::{category_of, flag};

pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x40000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

pub trait DirReader: Send + Sync {
    fn read(&self, dir: &Path, out: &mut DirBuilder) -> bool;
}

/// The reader for this platform. `on_disk`: report allocated size (what deleting frees) instead of the
/// logical length; only the Win32 adapter can tell the two apart.
pub fn system(on_disk: bool) -> Arc<dyn DirReader> {
    #[cfg(windows)]
    {
        Arc::new(Win32Reader { on_disk })
    }
    #[cfg(not(windows))]
    {
        let _ = on_disk;
        Arc::new(StdReader)
    }
}

pub(crate) struct Raw {
    pub size: u64,
    pub mtime: u32,
    pub name_len: u16,
    pub cat: u8,
    pub flags: u8,
}

/// Sink for one directory's entries (names accumulate in a flat byte buffer).
pub struct DirBuilder {
    pub(crate) names: Vec<u8>,
    pub(crate) entries: Vec<Raw>,
    pub(crate) dirs: Vec<(usize, OsString)>,
    pub(crate) files: u64,
    pub(crate) ndirs: u64,
    pub(crate) bytes: u64,
}

impl DirBuilder {
    pub(crate) fn new() -> Self {
        DirBuilder { names: Vec::with_capacity(1024), entries: Vec::new(), dirs: Vec::new(), files: 0, ndirs: 0, bytes: 0 }
    }

    /// Add an entry. `attrs` are Windows file attributes (0 elsewhere); entries flagged as cloud
    /// placeholders count as 0 bytes because their data is not on disk.
    #[cfg_attr(not(test), allow(dead_code))] // convenience for adapters with plain UTF-8 names (test fakes)
    pub fn push(&mut self, name: &str, is_dir: bool, size: u64, mtime: u32, attrs: u32) {
        self.push_with(name, || OsString::from(name), is_dir, size, mtime, attrs);
    }

    /// Like [`push`](Self::push) but lets the adapter supply the lossless OS name for directories
    /// (needed to rebuild the child path when the name is not valid Unicode).
    pub fn push_with(&mut self, name: &str, dir_os_name: impl FnOnce() -> OsString, is_dir: bool, raw_size: u64, mtime: u32, a: u32) {
        if name.len() > u16::MAX as usize {
            return;
        }
        let mut flags = 0u8;
        if a & ATTR_HIDDEN != 0 {
            flags |= flag::HIDDEN;
        }
        if a & ATTR_SYSTEM != 0 {
            flags |= flag::SYSTEM;
        }
        let cloud = a & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0;
        if cloud {
            flags |= flag::CLOUD;
        }
        let (size, cat);
        if is_dir {
            flags |= flag::DIR;
            size = 0;
            cat = 0;
            self.ndirs += 1;
            self.dirs.push((self.entries.len(), dir_os_name()));
        } else {
            size = if cloud { 0 } else { raw_size };
            cat = category_of(name);
            self.files += 1;
            self.bytes += size;
        }
        self.names.extend_from_slice(name.as_bytes());
        self.entries.push(Raw { size, mtime, name_len: name.len() as u16, cat, flags });
    }
}

// ---------------------------------------------------------------- std adapter

/// Portable adapter: `std::fs::read_dir`. Reports logical length.
#[cfg_attr(all(windows, not(test)), allow(dead_code))]
pub struct StdReader;

impl DirReader for StdReader {
    fn read(&self, dir: &Path, out: &mut DirBuilder) -> bool {
        use std::time::UNIX_EPOCH;
        let Ok(rd) = std::fs::read_dir(dir) else { return false };
        for ent in rd {
            let Ok(ent) = ent else { continue };
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let Ok(md) = ent.metadata() else { continue };
            let os_name = ent.file_name();
            let name = os_name.to_string_lossy();
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs().min(u32::MAX as u64) as u32)
                .unwrap_or(0);
            #[cfg(windows)]
            let attrs = {
                use std::os::windows::fs::MetadataExt;
                md.file_attributes()
            };
            #[cfg(not(windows))]
            let attrs = 0;
            out.push_with(&name, || os_name.clone(), ft.is_dir(), md.len(), mtime, attrs);
        }
        true
    }
}

// ---------------------------------------------------------------- Win32 adapter

/// Windows adapter: one `GetFileInformationByHandleEx(FileFullDirectoryInfo)` call returns hundreds of
/// entries (name, size, allocation size, attributes, mtime) per syscall, with no per-entry allocation.
#[cfg(windows)]
pub struct Win32Reader {
    pub on_disk: bool,
}

#[cfg(windows)]
impl DirReader for Win32Reader {
    fn read(&self, dir: &Path, out: &mut DirBuilder) -> bool {
        fill_win(dir, out, self.on_disk)
    }
}

#[cfg(windows)]
fn fill_win(path: &Path, b: &mut DirBuilder, on_disk: bool) -> bool {
    use std::cell::RefCell;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FULL_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };

    const REPARSE: u32 = 0x400;
    const TAG_SYMLINK: u32 = 0xA000_000C;
    const TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const DIRECTORY: u32 = 0x10;
    const BUF_U64: usize = 16 * 1024; // 128 KiB

    thread_local! {
        static BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; BUF_U64]);
        static NAME: RefCell<String> = RefCell::new(String::with_capacity(512));
    }

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    // Long-path support for drive-letter paths.
    if wide.len() >= 240 && !wide.starts_with(&[92, 92]) {
        let mut v: Vec<u16> = r"\\?\".encode_utf16().collect();
        v.extend_from_slice(&wide);
        wide = v;
    }
    wide.push(0);

    // SAFETY: plain Win32 calls; the buffer is 8-byte aligned (Vec<u64>) and outlives every pointer
    // derived from it, and entries are only read within `NextEntryOffset` chains the kernel produced.
    unsafe {
        let h = CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return false;
        }
        BUF.with(|buf| {
            NAME.with(|name_buf| {
                let mut buf = buf.borrow_mut();
                let mut name_buf = name_buf.borrow_mut();
                let mut class = FileFullDirectoryRestartInfo;
                loop {
                    let ok = GetFileInformationByHandleEx(h, class, buf.as_mut_ptr().cast(), (BUF_U64 * 8) as u32);
                    class = FileFullDirectoryInfo;
                    if ok == 0 {
                        break; // ERROR_NO_MORE_FILES (or an I/O error: keep what we have)
                    }
                    let base = buf.as_ptr().cast::<u8>();
                    let mut off = 0usize;
                    loop {
                        let info = base.add(off).cast::<FILE_FULL_DIR_INFO>();
                        let attrs = (*info).FileAttributes;
                        let nlen = (*info).FileNameLength as usize / 2;
                        let name16 =
                            std::slice::from_raw_parts(std::ptr::addr_of!((*info).FileName).cast::<u16>(), nlen);
                        let dot = name16 == [46] || name16 == [46, 46];
                        let tag = (*info).EaSize; // holds the reparse tag when REPARSE is set
                        let link = attrs & REPARSE != 0 && (tag == TAG_SYMLINK || tag == TAG_MOUNT_POINT);
                        if !dot && !link {
                            name_buf.clear();
                            for c in char::decode_utf16(name16.iter().copied()) {
                                name_buf.push(c.unwrap_or('\u{FFFD}'));
                            }
                            let size = if on_disk { (*info).AllocationSize } else { (*info).EndOfFile } as u64;
                            let ft = (*info).LastWriteTime as u64;
                            let mtime = (ft / 10_000_000).saturating_sub(11_644_473_600).min(u32::MAX as u64) as u32;
                            b.push_with(&name_buf, || OsString::from_wide(name16), attrs & DIRECTORY != 0, size, mtime, attrs);
                        }
                        let next = (*info).NextEntryOffset as usize;
                        if next == 0 {
                            break;
                        }
                        off += next;
                    }
                }
            })
        });
        CloseHandle(h);
    }
    true
}
