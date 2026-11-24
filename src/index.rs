// In-memory filename index, arena-compacted to keep RSS low.
// Build = recursive FindFirstFile walk with the skip list; names + parent chain only,
// full paths are resolved lazily for matched results.
// Live updates = ReadDirectoryChangesW per drive root (event-driven, idle CPU ~0).
// No content reading, no MFT/USN (needs elevation; roadmap).
use std::collections::{HashMap, HashSet};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

#[derive(Clone, Copy)]
struct Rec {
    // bit31 = is_dir; bits 0..30 = parent record id; ROOT_PARENT = drive root
    parent: u32,
    name_id: u32, // index into the de-duplicated name table
    size: u64,
    mtime: u32, // unix seconds; 0 = unknown
}

const DIR_BIT: u32 = 0x8000_0000;
/// parent sentinel for drive roots (stored in lower 31 bits)
const ROOT: u32 = 0x7FFF_FFFF;

impl Rec {
    fn is_dir(self) -> bool {
        self.parent & DIR_BIT != 0
    }
    fn parent_id(self) -> u32 {
        self.parent & !DIR_BIT
    }
    fn new(parent: u32, is_dir: bool, name_id: u32, size: u64, mtime: u32) -> Self {
        Rec {
            parent: parent | (if is_dir { DIR_BIT } else { 0 }),
            name_id,
            size,
            mtime,
        }
    }
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
    // De-duplicated name table: u16 byte-length prefix + raw bytes; id = sequential index.
    names: Vec<u8>,
    name_off: Vec<u32>, // id -> offset of its u16 length prefix in names
    // Build-phase seen map; cleared after build, reused for watcher-added names only.
    seen: HashMap<String, u32>,
    // Tombstoned record ids (watcher deletes); never swapped out so ids stay valid.
    dead: HashSet<u32>,
    // Only for entries added by the watcher (rare); build-time entries keep no String path.
    watch_path: HashMap<String, u32>,
}

/// Lock-free published index: readers take an Acquire snapshot with no lock;
/// a rebuild atomically swaps the pointer and drops the old Index.
#[derive(Clone)]
pub struct Shared {
    inner: Arc<AtomicPtr<Index>>,
}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    pub fn new(idx: Index) -> Self {
        let ptr = Arc::into_raw(Arc::new(idx)).cast_mut();
        Self { inner: Arc::new(AtomicPtr::new(ptr)) }
    }
    /// Read-only snapshot; zero locking on the query path.
    pub fn view(&self) -> &Index {
        unsafe { &*self.inner.load(Ordering::Acquire) }
    }
    /// Publish a fresh Index and release the previous generation.
    pub fn swap(&self, fresh: Index) {
        let new = Arc::into_raw(Arc::new(fresh)).cast_mut();
        let old = self.inner.swap(new, Ordering::AcqRel);
        unsafe { drop(Arc::from_raw(old)); }
    }
}

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
        // magic + format version; v3 = de-duplicated name table, Rec 20B (no nlen)
        let _ = f.write_all(b"E233");
        let _ = f.write_all(&3u32.to_le_bytes());
        let _ = f.write_all(&(self.names.len() as u32).to_le_bytes());
        let _ = f.write_all(&self.names);
        let _ = f.write_all(&(self.name_off.len() as u32).to_le_bytes());
        for o in &self.name_off {
            let _ = f.write_all(&o.to_le_bytes());
        }
        let _ = f.write_all(&(self.recs.len() as u32).to_le_bytes());
        for r in &self.recs {
            let _ = f.write_all(&r.parent.to_le_bytes());
            let _ = f.write_all(&r.name_id.to_le_bytes());
            let _ = f.write_all(&r.size.to_le_bytes());
            let _ = f.write_all(&r.mtime.to_le_bytes());
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
        if buf.len() < 8 || &buf[..4] != b"E233" { return None; }
        let ver = u32::from_le_bytes(buf[4..8].try_into().ok()?);
        if ver != 3 { return None; }
        let mut o = 8usize;
        let mut rd = |n: usize| -> Option<&[u8]> { let s = o; o += n; Some(&buf[s..o]) };
        let nl = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
        let names = rd(nl)?.to_vec();
        let nc = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
        let mut name_off = Vec::with_capacity(nc);
        for _ in 0..nc {
            name_off.push(u32::from_le_bytes(rd(4)?.try_into().ok()?));
        }
        let rc = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
        let mut recs = Vec::with_capacity(rc);
        for _ in 0..rc {
            let parent = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            let name_id = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            let size = u64::from_le_bytes(rd(8)?.try_into().ok()?);
            let mtime = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            recs.push(Rec { parent, name_id, size, mtime });
        }
        let wc = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
        let mut watch_path = HashMap::new();
        for _ in 0..wc {
            let kl = u32::from_le_bytes(rd(4)?.try_into().ok()?) as usize;
            let k = String::from_utf8_lossy(rd(kl)?).into_owned();
            let v = u32::from_le_bytes(rd(4)?.try_into().ok()?);
            watch_path.insert(k, v);
        }
        let idx = Index { recs, names, name_off, seen: HashMap::new(), dead: HashSet::new(), watch_path };
        Some(idx)
    }
}

impl Index {
    /// Intern a name into the de-duplicated table; returns its id.
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.seen.get(s) {
            return id;
        }
        let id = self.name_off.len() as u32;
        let b = s.as_bytes();
        let off = self.names.len() as u32;
        self.names.extend_from_slice(&(b.len() as u16).to_le_bytes());
        self.names.extend_from_slice(b);
        self.name_off.push(off);
        self.seen.insert(s.to_string(), id);
        id
    }

    fn add_dir(&mut self, parent: u32, name: &str) -> u32 {
        let nid = self.intern(name);
        let id = self.recs.len() as u32;
        self.recs.push(Rec::new(parent, true, nid, 0, 0));
        id
    }

    fn add_file(&mut self, parent: u32, name: &str, size: u64, mtime: u32) -> u32 {
        let nid = self.intern(name);
        let id = self.recs.len() as u32;
        self.recs.push(Rec::new(parent, false, nid, size, mtime));
        id
    }

    fn name_at(&self, id: u32) -> &str {
        let off = self.name_off[id as usize] as usize;
        let nlen = u16::from_le_bytes([self.names[off], self.names[off + 1]]) as usize;
        std::str::from_utf8(&self.names[off + 2..off + 2 + nlen]).unwrap_or("")
    }

    fn rec_name(&self, r: Rec) -> &str {
        self.name_at(r.name_id)
    }

    /// Reconstruct full path by walking the parent chain.
    fn resolve(&self, id: u32) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        for _ in 0..128 {
            let r = self.recs[cur as usize];
            parts.push(self.name_at(r.name_id).to_string());
            if r.parent_id() == ROOT || r.parent == u32::MAX {
                break;
            }
            cur = r.parent_id();
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

    pub fn len(&self) -> usize {
        self.recs.len()
    }

    /// Byte-wise lowercase substring match; candidate lowercased into a stack buffer (no alloc).
    #[inline]
    fn name_hit(&self, r: Rec, segs: &[Vec<u8>], qb: &[u8], stack: &mut [u8; 512]) -> bool {
        let b = self.name_at(r.name_id).as_bytes();
        let low: &[u8] = if b.len() <= stack.len() {
            for (d, &c) in stack[..b.len()].iter_mut().zip(b) {
                *d = c.to_ascii_lowercase();
            }
            &stack[..b.len()]
        } else {
            // Rare long name: alloc once and compare directly.
            let l = b.to_ascii_lowercase();
            return if segs.is_empty() {
                l.windows(qb.len()).any(|w| w == qb)
            } else {
                segs.iter().all(|s| l.windows(s.len()).any(|w| w == s))
            };
        };
        if segs.is_empty() {
            !qb.is_empty() && low.windows(qb.len()).any(|w| w == qb)
        } else {
            segs.iter().all(|s| low.windows(s.len()).any(|w| w == s))
        }
    }

    pub fn search(&self, q: &str, dir_only: bool, file_only: bool, limit: usize) -> Vec<Entry> {
        let segs: Vec<Vec<u8>> = q
            .split('*')
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_ascii_lowercase())
            .collect();
        let qb = q.as_bytes().to_ascii_lowercase();

        let mut out = Vec::new();
        let mut stack = [0u8; 512];
        for (id, r) in self.recs.iter().enumerate() {
            if out.len() >= limit { break; }
            if self.dead.contains(&(id as u32)) { continue; }
            if dir_only && !r.is_dir() { continue; }
            if file_only && r.is_dir() { continue; }
            if !self.name_hit(*r, &segs, &qb, &mut stack) { continue; }
            let path = self
                .watch_path
                .iter()
                .find(|(_, &v)| v == id as u32)
                .map(|(p, _)| p.clone())
                .unwrap_or_else(|| self.resolve(id as u32));
            if crate::ignore::path_ignored(&path) { continue; }
            out.push(Entry {
                path,
                name: self.rec_name(*r).to_string(),
                is_dir: r.is_dir(),
                size: r.size,
                mtime: r.mtime as u64,
            });
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

// FILETIME (100ns since 1601) -> unix seconds, truncated to u32.
fn ft_to_unix(ft: &windows_sys::Win32::Foundation::FILETIME) -> u32 {
    let v = ft_u64(ft);
    ((v.saturating_sub(116_444_736_000_000_000)) / 10_000_000) as u32
}

// -------- initial build: recursive walk --------

// -------- scan roots: system drive only office dirs; data drives whole, top-level filtered --------

/// Whitelist roots from %APPDATA%\everything233\include.txt (one root per line, # = comment).
fn include_roots() -> Option<Vec<String>> {
    let dir = std::env::var_os("APPDATA")?;
    let p = Path::new(&dir).join("everything233").join("include.txt");
    let txt = std::fs::read_to_string(p).ok()?;
    let list: Vec<String> = txt
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| Path::new(l).is_dir())
        .map(|l| l.to_string())
        .collect();
    if list.is_empty() { None } else { Some(list) }
}

pub fn scan_roots() -> Vec<String> {
    // Whitelist file overrides everything; data drives are never scanned by default.
    if let Some(extra) = include_roots() {
        return extra;
    }
    // Default: C: office/work folders only; never the OS tree or other drives.
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let h = Path::new(&home);
        for sub in [
            "Desktop", "Documents", "Downloads", "OneDrive", "Favorites",
            "Pictures", "Videos", "Music", "source", "scripts", "DoubaoWork",
            "WPS Cloud", "WPS Cloud Files",
        ] {
            let p = h.join(sub);
            if p.is_dir() {
                roots.push(p.to_string_lossy().into_owned());
            }
        }
    }
    roots
}

pub fn build() -> Index {
    // Background-grade priority: a full-disk walk must not preempt the foreground.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    let mut idx = Index::default();
    for root in scan_roots() {
        walk_root(&root, &mut idx, 0);
    }
    eprintln!(
        "names: {} unique, {} bytes ({} records)",
        idx.name_off.len(),
        idx.names.len(),
        idx.recs.len()
    );
    // Drop the build-phase seen map; reused empty for watcher-added names only.
    idx.seen.clear();
    idx.seen.shrink_to_fit();
    idx
}

fn walk_root(root: &str, idx: &mut Index, depth: u32) {
    let root_id = idx.add_dir(ROOT, root);
    let mut stack: Vec<(String, u32, u32)> = vec![(root.to_string(), root_id, depth)];
    let mut budget: u32 = 0;
    while let Some((dir, parent, dep)) = stack.pop() {
        let pattern = format!("\\\\?\\{dir}\\*");
        let w = wide(&pattern);
        let mut fd = unsafe { std::mem::zeroed::<WIN32_FIND_DATAW>() };
        let h = unsafe { FindFirstFileW(w.as_ptr(), &mut fd) };
        if h == INVALID_HANDLE_VALUE { continue; }
        loop {
            let name = from_wide(&fd.cFileName);
            if name != "." && name != ".." {
                let full = format!("{dir}\\{name}");
                if !crate::ignore::walk_ignored(&full) {
                    let is_dir = fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                    if is_dir {
                        let blocked = fd.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                            || crate::ignore::is_ignored(&name)
                            || (dep == 0 && crate::ignore::top_skipped(&name));
                        if !blocked {
                            let id = idx.add_dir(parent, &name);
                            stack.push((full, id, dep + 1));
                        }
                    } else {
                        let size = (fd.nFileSizeHigh as u64) << 32 | fd.nFileSizeLow as u64;
                        idx.add_file(parent, &name, size, ft_to_unix(&fd.ftLastWriteTime));
                    }
                }
                // Yield the CPU every ~4k entries so a full-disk walk never
                // pins one core / starves the foreground.
                budget += 1;
                if budget & 0xFFF == 0 {
                    std::thread::yield_now();
                }
            }
            if unsafe { FindNextFileW(h, &mut fd) } == 0 { break; }
        }
        unsafe { FindClose(h) };
    }
}

// -------- live refresh: 30s light top-level signature check; rebuild only on change --------

/// Cheap signature of scan roots' top level; changed => a rebuild is warranted.
fn root_sig() -> u64 {
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
    for root in scan_roots() {
        let pattern = format!("\\\\?\\{root}\\*");
        let w = wide(&pattern);
        let mut fd = unsafe { std::mem::zeroed::<WIN32_FIND_DATAW>() };
        let hnd = unsafe { FindFirstFileW(w.as_ptr(), &mut fd) };
        if hnd == INVALID_HANDLE_VALUE { continue; }
        loop {
            let name = from_wide(&fd.cFileName);
            if name != "." && name != ".." {
                h = h.wrapping_add(name.len() as u64);
                h = h.wrapping_mul(0x1000_0000_1B3);
                h ^= ft_u64(&fd.ftLastWriteTime);
            }
            if unsafe { FindNextFileW(hnd, &mut fd) } == 0 { break; }
        }
        unsafe { FindClose(hnd) };
    }
    h
}

pub fn spawn_poll(idx: Shared) {
    std::thread::Builder::new()
        .name("poll".to_string())
        .spawn(move || {
            let mut sig = root_sig();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let cur = root_sig();
                if cur != sig {
                    sig = cur;
                    let fresh = build();
                    idx.swap(fresh);
                    eprintln!("index refreshed (root changed)");
                }
            }
        })
        .ok();
}

// -------- display helpers --------

pub fn fmt_ft(secs: u64) -> String {
    if secs == 0 { return "-".to_string(); }
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
