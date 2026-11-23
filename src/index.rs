// In-memory filename index, arena-compacted to keep RSS low.
// Build = recursive FindFirstFile walk with the skip list; names + parent chain only,
// full paths are resolved lazily for matched results.
// Live updates = ReadDirectoryChangesW per drive root (event-driven, idle CPU ~0).
// No content reading, no MFT/USN (needs elevation; roadmap).
use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindClose, FindFirstFileW, FindNextFileW, GetFileAttributesExW,
    GetLogicalDrives, OPEN_EXISTING, ReadDirectoryChangesW, WIN32_FILE_ATTRIBUTE_DATA,
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_INFORMATION,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileExInfoStandard,
    WIN32_FIND_DATAW,
};

#[derive(Clone, Copy)]
struct Rec {
    parent: u32, // record id; u32::MAX = drive root
    name: u32,   // offset into names arena
    nlen: u32,
    size: u64,
    mtime: u64, // FILETIME
    is_dir: bool,
}

#[derive(Clone)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Default)]
pub struct Index {
    recs: Vec<Rec>,
    names: Vec<u8>,
    // Only for entries added by the watcher (rare); build-time entries keep no String path.
    watch_path: HashMap<String, u32>,
}

pub type Shared = Arc<Mutex<Index>>;

pub fn cache_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| {
        Path::new(&d).join("everything233").join("index.dat")
    })
}

impl Index {
    pub fn save(&self, p: &Path) {
        use std::io::Write;
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(mut f) = std::fs::File::create(p) else { return };
        let _ = f.write_all(&self.names.len().to_le_bytes());
        let _ = f.write_all(&self.names);
        let _ = f.write_all(&(self.recs.len() as u32).to_le_bytes());
        for r in &self.recs {
            let _ = f.write_all(&r.parent.to_le_bytes());
            let _ = f.write_all(&r.name.to_le_bytes());
            let _ = f.write_all(&r.nlen.to_le_bytes());
            let _ = f.write_all(&r.size.to_le_bytes());
            let _ = f.write_all(&r.mtime.to_le_bytes());
            let _ = f.write_all(&[r.is_dir as u8]);
        }
        let _ = f.write_all(&(self.watch_path.len() as u32).to_le_bytes());
        for (k, &v) in &self.watch_path {
            let kb = k.as_bytes();
            let _ = f.write_all(&(kb.len() as u32).to_le_bytes());
            let _ = f.write_all(kb);
            let _ = f.write_all(&v.to_le_bytes());
        }
    }

    pub fn load(p: &Path) -> Option<Self> {
        use std::io::Read;
        let mut f = std::fs::File::open(p).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let mut o = 0usize;
        let mut rd = |n: usize| -> Option<&[u8]> { let s = o; o += n; Some(&buf[s..o]) };
        let nl = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
        let names = rd(nl)?.to_vec();
        let rc = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
        let mut recs = Vec::with_capacity(rc);
        for _ in 0..rc {
            let parent = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            let name = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            let nlen = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            let size = u64::from_le_bytes(rd(8)?.try_into().ok()?);
            let mtime = u64::from_le_bytes(rd(8)?.try_into().ok()?);
            let is_dir = rd(1)?[0] != 0;
            recs.push(Rec { parent, name, nlen, size, mtime, is_dir });
        }
        let wc = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
        let mut watch_path = HashMap::new();
        for _ in 0..wc {
            let kl = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
            let k = String::from_utf8_lossy(rd(kl)?).into_owned();
            let v = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            watch_path.insert(k, v);
        }
        Some(Index { recs, names, watch_path })
    }
}

impl Index {
    fn push_name(&mut self, s: &str) -> (u32, u32) {
        let off = self.names.len() as u32;
        self.names.extend_from_slice(s.as_bytes());
        (off, s.len() as u32)
    }

    fn add_dir(&mut self, parent: u32, name: &str) -> u32 {
        let (off, nlen) = self.push_name(name);
        let id = self.recs.len() as u32;
        self.recs.push(Rec { parent, name: off, nlen, size: 0, mtime: 0, is_dir: true });
        id
    }

    fn add_file(&mut self, parent: u32, name: &str, size: u64, mtime: u64) -> u32 {
        let (off, nlen) = self.push_name(name);
        let id = self.recs.len() as u32;
        self.recs.push(Rec { parent, name: off, nlen, size, mtime, is_dir: false });
        id
    }

    fn rec_name(&self, r: Rec) -> &str {
        std::str::from_utf8(&self.names[r.name as usize..r.name as usize + r.nlen as usize]).unwrap_or("")
    }

    /// Reconstruct full path by walking the parent chain.
    fn resolve(&self, id: u32) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        for _ in 0..128 {
            let r = self.recs[cur as usize];
            parts.push(self.rec_name(r).to_string());
            if r.parent == u32::MAX {
                break;
            }
            cur = r.parent;
        }
        parts.reverse();
        let mut path = String::new();
        for p in parts {
            if path.is_empty() || path.ends_with('\\') || p.starts_with('\\') {
                path.push_str(&p);
            } else {
                path.push('\\');
                path.push_str(&p);
            }
        }
        path
    }

    pub fn remove_path(&mut self, path: &str) {
        if let Some(&id) = self.watch_path.get(path) {
            self.watch_path.remove(path);
            let last = self.recs.len() - 1;
            self.recs.swap_remove(id as usize);
            if id as usize != last {
                if let Some(v) = self.watch_path.values_mut().find(|v| **v == last as u32) {
                    *v = id;
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.recs.len()
    }

    pub fn search(&self, q: &str, dir_only: bool, file_only: bool, limit: usize) -> Vec<Entry> {
        let segments: Vec<&str> = q.split('*').filter(|s| !s.is_empty()).collect();
        let ql = q.to_ascii_lowercase();
        let mut out = Vec::new();
        for (id, r) in self.recs.iter().enumerate() {
            if dir_only && !r.is_dir { continue; }
            if file_only && r.is_dir { continue; }
            let nl = self.rec_name(*r).to_ascii_lowercase();
            let hit = if segments.is_empty() {
                nl.contains(&ql)
            } else {
                segments.iter().all(|s| nl.contains(&s.to_ascii_lowercase()))
            };
            if !hit { continue; }
            let path = self.watch_path.iter()
                .find(|(_, &v)| v == id as u32)
                .map(|(p, _)| p.clone())
                .unwrap_or_else(|| self.resolve(id as u32));
            if crate::ignore::path_ignored(&path) { continue; }
            out.push(Entry {
                path,
                name: self.rec_name(*r).to_string(),
                is_dir: r.is_dir,
                size: r.size,
                mtime: r.mtime,
            });
            if out.len() >= limit { break; }
        }
        out.sort_by(|a, b| {
            a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.path.cmp(&b.path))
        });
        out
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn from_wide(p: &[u16]) -> String {
    let n = p.iter().position(|&c| c == 0).unwrap_or(p.len());
    String::from_utf16_lossy(&p[..n])
}

fn ft_u64(ft: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

// -------- initial build: recursive walk --------

pub fn build() -> Index {
    let mut idx = Index::default();
    let drives = unsafe { GetLogicalDrives() };
    for bit in 0u32..26 {
        if drives & (1 << bit) == 0 { continue; }
        let root = format!("{}:\\", (b'A' + bit as u8) as char);
        walk_root(&root, &mut idx);
    }
    idx
}

fn walk_root(root: &str, idx: &mut Index) {
    let root_id = idx.add_dir(u32::MAX, root);
    let mut stack = vec![(root.to_string(), root_id)];
    while let Some((dir, parent)) = stack.pop() {
        let pattern = format!("\\\\?\\{dir}\\*");
        let w = wide(&pattern);
        let mut fd = unsafe { std::mem::zeroed::<WIN32_FIND_DATAW>() };
        let h = unsafe { FindFirstFileW(w.as_ptr(), &mut fd) };
        if h == INVALID_HANDLE_VALUE { continue; }
        loop {
            let name = from_wide(&fd.cFileName);
            if name != "." && name != ".." {
                let full = format!("{dir}\\{name}");
                let is_dir = fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                if is_dir {
                    if fd.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                        && !crate::ignore::is_ignored(&name)
                    {
                        let id = idx.add_dir(parent, &name);
                        stack.push((full, id));
                    }
                } else {
                    let size = (fd.nFileSizeHigh as u64) << 32 | fd.nFileSizeLow as u64;
                    idx.add_file(parent, &name, size, ft_u64(&fd.ftLastWriteTime));
                }
            }
            if unsafe { FindNextFileW(h, &mut fd) } == 0 { break; }
        }
        unsafe { FindClose(h) };
    }
}

// -------- live updates: ReadDirectoryChangesW per drive root --------

pub fn fixed_roots() -> Vec<String> {
    let mut roots = Vec::new();
    let drives = unsafe { GetLogicalDrives() };
    for bit in 0u32..26 {
        if drives & (1 << bit) != 0 {
            roots.push(format!("{}:\\", (b'A' + bit as u8) as char));
        }
    }
    roots
}

pub fn spawn_watch(root: String, idx: Shared) {
    std::thread::Builder::new()
        .name(format!("watch-{root}"))
        .spawn(move || {
            let w = wide(&format!("\\\\?\\{root}"));
            let h = unsafe {
                CreateFileW(
                    w.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if h == INVALID_HANDLE_VALUE { return; }
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut returned = 0u32;
                let ok = unsafe {
                    ReadDirectoryChangesW(
                        h, buf.as_mut_ptr() as *mut _, buf.len() as u32, 1,
                        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                        &mut returned, std::ptr::null_mut(), None,
                    )
                };
                if ok == 0 { break; }
                if returned == 0 { continue; }
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    parse_changes(&buf, returned as usize, &root, &idx);
                }));
            }
            unsafe { CloseHandle(h) };
        })
        .ok();
}

fn parse_changes(buf: &[u8], len: usize, root: &str, idx: &Shared) {
    let mut off = 0usize;
    while off < len {
        if off + 24 > len { break; }
        let p = unsafe { &*(buf[off..].as_ptr() as *const FILE_NOTIFY_INFORMATION) };
        let rel = unsafe {
            let n = (p.FileNameLength as usize / 2).min((len - off - 24) / 2);
            let s = std::slice::from_raw_parts(p.FileName.as_ptr(), n);
            String::from_utf16_lossy(s)
        };
        let full = format!("{root}{rel}");
        if !crate::ignore::path_ignored(&full) {
            match p.Action {
                FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => stat_add(&full, idx),
                FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => {
                    idx.lock().unwrap().remove_path(&full);
                }
                FILE_ACTION_MODIFIED => stat_add(&full, idx),
                _ => {}
            }
        }
        if p.NextEntryOffset == 0 { break; }
        off += p.NextEntryOffset as usize;
    }
}

fn stat_add(full: &str, idx: &Shared) {
    let mut info = unsafe { std::mem::zeroed::<WIN32_FILE_ATTRIBUTE_DATA>() };
    let w = wide(&format!("\\\\?\\{full}"));
    let ok = unsafe {
        GetFileAttributesExW(w.as_ptr(), GetFileExInfoStandard, &mut info as *mut _ as *mut _)
    };
    if ok == 0 { return; }
    let name = full.rsplit(['\\', '/']).next().unwrap_or(full).to_string();
    let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let size = (info.nFileSizeHigh as u64) << 32 | info.nFileSizeLow as u64;
    let mtime = ft_u64(&info.ftLastWriteTime);
    let mut g = idx.lock().unwrap();
    if let Some(&id) = g.watch_path.get(full) {
        let r = &mut g.recs[id as usize];
        r.size = size;
        r.mtime = mtime;
        r.is_dir = is_dir;
        return;
    }
    let id = if is_dir { g.add_dir(u32::MAX, &name) } else { g.add_file(u32::MAX, &name, size, mtime) };
    g.watch_path.insert(full.to_string(), id);
}

// -------- display helpers --------

pub fn fmt_ft(ft: u64) -> String {
    if ft == 0 { return "-".to_string(); }
    let secs = (ft - 116_444_736_000_000_000) / 10_000_000;
    let (days, tod) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    let (hh, mm) = (tod / 3600, (tod % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

fn civil_from_days(mut z: i64) -> (i64, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 24);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}
