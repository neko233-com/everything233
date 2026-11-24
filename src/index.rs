// In-memory filename index.
//
// Two build paths:
//   * elevated (preferred): direct USN journal enumeration (src/usn.rs) -> whole
//     volume in seconds, no directory walking; then a blocking journal tail
//     gives sub-second updates at zero idle CPU (no polling thread).
//   * fallback (no elevation): FindFirstFile walk + 30s root-signature poll.
//
// Layout (v4, memory-first):
//   Rec          8 B  { parent(u32: bit31 dir | bit30 dead | low30 parent id), name_id }
//   names        len(u16)+utf8, de-duplicated; name_off: id -> offset
//   first/next   4 B each: name_id -> chain of records sharing that name, so a
//                query scans unique names (~650k) instead of records (~4M).
//   frn          4 B: MFT record number -> rec id + 1, for live journal patches.
// Size/mtime are NOT stored: one GetFileAttributesExW per returned hit fills them
// (<= limit calls), saving 12 B/entry.
use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, FindNextFileW, GetFileAttributesExW, GetFileExInfoStandard,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FILE_ATTRIBUTE_DATA,
    WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST,
};

use crate::usn::{self, Change};

#[derive(Clone, Copy)]
struct Rec {
    // bit31 = is_dir, bit30 = dead (tombstoned by the live journal), low30 = parent rec id
    parent: u32,
    name_id: u32,
}

const DIR_BIT: u32 = 0x8000_0000;
const DEAD_BIT: u32 = 0x4000_0000;
const ID_MASK: u32 = 0x3FFF_FFFF;
/// parent sentinel for volume roots (stored in the low bits)
const ROOT: u32 = ID_MASK;

impl Rec {
    fn is_dir(self) -> bool {
        self.parent & DIR_BIT != 0
    }
    fn dead(self) -> bool {
        self.parent & DEAD_BIT != 0
    }
    fn parent_id(self) -> u32 {
        self.parent & ID_MASK
    }
}

#[derive(Clone, Copy)]
pub struct VolMeta {
    pub drive: u8,
    /// Offset of this volume's MFT-number space inside `frn` (multi-drive keys).
    pub base: u32,
    pub journal_id: u64,
    pub next_usn: i64,
}

#[derive(Default)]
pub struct Index {
    recs: Vec<Rec>,
    // De-duplicated name table: u16 byte-length prefix + raw bytes; id = sequential index.
    names: Vec<u8>,
    name_off: Vec<u32>, // id -> offset of its u16 length prefix in names
    first: Vec<u32>,    // name_id -> first rec id + 1 (0 = none)
    next: Vec<u32>,     // rec id -> next rec id + 1 sharing the same name (0 = end)
    frn: Vec<u32>,      // volume-offset MFT key -> rec id + 1 (0 = absent)
    pub vol: Vec<VolMeta>,
    // Build-phase name map; cleared after build.
    seen: HashMap<String, u32>,
}

pub type Shared = Arc<RwLock<Index>>;

pub fn cache_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| {
        Path::new(&d).join("everything233").join("index.dat")
    })
}

/// Tiny sidecar with USN watermarks: lets a restart replay changes that
/// happened while the daemon was down, without rewriting the big cache.
pub fn meta_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| {
        Path::new(&d).join("everything233").join("usn.meta")
    })
}

pub fn save_watermarks(vol: &[VolMeta]) {
    let Some(p) = meta_path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut b: Vec<u8> = Vec::with_capacity(8 + vol.len() * 17);
    b.extend_from_slice(b"E2UW");
    b.extend_from_slice(&(vol.len() as u32).to_le_bytes());
    for v in vol {
        b.push(v.drive);
        b.extend_from_slice(&v.journal_id.to_le_bytes());
        b.extend_from_slice(&v.next_usn.to_le_bytes());
    }
    let _ = std::fs::write(p, b);
}

pub fn load_watermarks() -> Vec<VolMeta> {
    let Some(p) = meta_path() else { return Vec::new() };
    let Ok(buf) = std::fs::read(p) else { return Vec::new() };
    if buf.len() < 8 || &buf[..4] != b"E2UW" {
        return Vec::new();
    }
    let n = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4])) as usize;
    let mut out = Vec::with_capacity(n);
    let mut o = 8;
    for _ in 0..n {
        let Some(drive) = buf.get(o).copied() else { break };
        let Some(j) = buf.get(o + 1..o + 9) else { break };
        let Some(u) = buf.get(o + 9..o + 17) else { break };
        out.push(VolMeta {
            drive,
            base: 0,
            journal_id: u64::from_le_bytes(j.try_into().unwrap_or([0; 8])),
            next_usn: i64::from_le_bytes(u.try_into().unwrap_or([0; 8])),
        });
        o += 17;
    }
    out
}

pub fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn from_wide(p: &[u16]) -> String {
    let n = p.iter().position(|&c| c == 0).unwrap_or(p.len());
    String::from_utf16_lossy(&p[..n])
}

fn ft_u64(ft: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

// -------- cache v4 --------

impl Index {
    pub fn save(&self, p: &Path) {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let mut b: Vec<u8> = Vec::with_capacity(
            16 + self.names.len() + self.name_off.len() * 4 + self.recs.len() * 8,
        );
        b.extend_from_slice(b"E233");
        b.extend_from_slice(&5u32.to_le_bytes());
        b.extend_from_slice(&(self.names.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.names);
        b.extend_from_slice(&(self.name_off.len() as u32).to_le_bytes());
        for v in &self.name_off {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&(self.recs.len() as u32).to_le_bytes());
        for r in &self.recs {
            b.extend_from_slice(&r.parent.to_le_bytes());
            b.extend_from_slice(&r.name_id.to_le_bytes());
        }
        b.extend_from_slice(&(self.first.len() as u32).to_le_bytes());
        for v in &self.first {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&(self.next.len() as u32).to_le_bytes());
        for v in &self.next {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&(self.frn.len() as u32).to_le_bytes());
        for v in &self.frn {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&(self.vol.len() as u32).to_le_bytes());
        for v in &self.vol {
            b.push(v.drive);
            b.extend_from_slice(&v.base.to_le_bytes());
            b.extend_from_slice(&v.journal_id.to_le_bytes());
            b.extend_from_slice(&v.next_usn.to_le_bytes());
        }
        if let Some(mut f) = std::fs::File::create(p).ok() {
            use std::io::Write;
            let _ = f.write_all(&b);
        }
    }

    pub fn load(p: &Path) -> Option<Self> {
        let buf = std::fs::read(p).ok()?;
        if buf.len() < 8 || &buf[..4] != b"E233" {
            return None;
        }
        if u32::from_le_bytes(buf[4..8].try_into().ok()?) != 5 {
            return None;
        }
        let mut o = 8usize;
        let ln = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4;
        let names = buf.get(o..o + ln)?.to_vec();
        o += ln;
        let nl = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4;
        let mut name_off = Vec::with_capacity(nl);
        for _ in 0..nl {
            name_off.push(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?));
            o += 4;
        }
        let rl = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4;
        let mut recs = Vec::with_capacity(rl);
        for _ in 0..rl {
            let parent = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?);
            let name_id = u32::from_le_bytes(buf.get(o + 4..o + 8)?.try_into().ok()?);
            o += 8;
            recs.push(Rec { parent, name_id });
        }
        let fl = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4;
        let mut first = Vec::with_capacity(fl);
        for _ in 0..fl {
            first.push(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?));
            o += 4;
        }
        let xl = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4;
        let mut next = Vec::with_capacity(xl);
        for _ in 0..xl {
            next.push(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?));
            o += 4;
        }
        let gl = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4;
        let mut frn = Vec::with_capacity(gl);
        for _ in 0..gl {
            frn.push(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?));
            o += 4;
        }
        let vc = u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?) as usize;
        o += 4;
        let mut vol = Vec::with_capacity(vc);
        for _ in 0..vc {
            let drive = *buf.get(o)?;
            let base = u32::from_le_bytes(buf.get(o + 1..o + 5)?.try_into().ok()?);
            let journal_id = u64::from_le_bytes(buf.get(o + 5..o + 13)?.try_into().ok()?);
            let next_usn = i64::from_le_bytes(buf.get(o + 13..o + 21)?.try_into().ok()?);
            o += 21;
            vol.push(VolMeta {
                drive,
                base,
                journal_id,
                next_usn,
            });
        }
        if recs.len() != next.len() || name_off.len() != first.len() {
            return None;
        }
        Some(Index {
            recs,
            names,
            name_off,
            first,
            next,
            frn,
            vol,
            seen: HashMap::new(),
        })
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
        self.first.push(0);
        self.seen.insert(s.to_string(), id);
        id
    }

    fn push_rec(&mut self, parent: u32, is_dir: bool, name_id: u32) -> u32 {
        let id = self.recs.len() as u32;
        let head = self.first[name_id as usize];
        self.recs.push(Rec {
            parent: parent | if is_dir { DIR_BIT } else { 0 },
            name_id,
        });
        self.next.push(head);
        self.first[name_id as usize] = id + 1;
        id
    }

    fn add_named(&mut self, parent: u32, is_dir: bool, name: &str) -> u32 {
        let nid = self.intern(name);
        self.push_rec(parent, is_dir, nid)
    }

    fn name_at(&self, id: u32) -> &str {
        let off = self.name_off[id as usize] as usize;
        let nlen = u16::from_le_bytes([self.names[off], self.names[off + 1]]) as usize;
        std::str::from_utf8(&self.names[off + 2..off + 2 + nlen]).unwrap_or("")
    }

    /// Reconstruct full path by walking the parent chain.
    fn resolve(&self, id: u32) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(8);
        let mut cur = id;
        for _ in 0..128 {
            let r = self.recs[cur as usize];
            parts.push(self.name_at(r.name_id));
            if r.parent_id() == ROOT {
                break;
            }
            cur = r.parent_id();
        }
        parts.reverse();
        let mut path = String::with_capacity(64);
        for p in parts {
            if path.is_empty() || path.ends_with('\\') || p.starts_with('\\') {
                path.push_str(p);
            } else {
                path.push('\\');
                path.push_str(p);
            }
        }
        path
    }

    pub fn len(&self) -> usize {
        self.recs.len()
    }

    /// Byte-wise lowercase substring match; candidate goes into a stack buffer (no alloc).
    #[inline]
    fn name_hit_bytes(b: &[u8], segs: &[Vec<u8>], qb: &[u8], stack: &mut [u8; 512]) -> bool {
        if b.len() <= stack.len() {
            for (d, &c) in stack[..b.len()].iter_mut().zip(b) {
                *d = c.to_ascii_lowercase();
            }
            let low = &stack[..b.len()];
            if segs.is_empty() {
                !qb.is_empty() && low.windows(qb.len()).any(|w| w == qb)
            } else {
                segs.iter().all(|s| low.windows(s.len()).any(|w| w == s))
            }
        } else {
            let l = b.to_ascii_lowercase();
            if segs.is_empty() {
                !qb.is_empty() && l.windows(qb.len()).any(|w| w == qb)
            } else {
                segs.iter().all(|s| l.windows(s.len()).any(|w| w == s))
            }
        }
    }

    pub fn search(&self, q: &str, dir_only: bool, file_only: bool, limit: usize) -> Vec<Entry> {
        let segs: Vec<Vec<u8>> = q
            .split('*')
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_ascii_lowercase())
            .collect();
        let qb = q.as_bytes().to_ascii_lowercase();
        let mut out: Vec<Entry> = Vec::with_capacity(limit.min(256));
        let mut stack = [0u8; 512];
        // Scan the unique-name table (~650k) instead of every record (~4M).
        for nid in 0..self.name_off.len() {
            if !Self::name_hit_bytes(self.name_at(nid as u32).as_bytes(), &segs, &qb, &mut stack) {
                continue;
            }
            let mut cur = self.first[nid];
            while cur != 0 {
                let id = cur - 1;
                cur = self.next[id as usize];
                let r = self.recs[id as usize];
                if r.dead() || r.name_id != nid as u32 {
                    continue; // dead, or a stale entry from a rename
                }
                if dir_only && !r.is_dir() {
                    continue;
                }
                if file_only && r.is_dir() {
                    continue;
                }
                if out.len() >= limit {
                    break;
                }
                let path = self.resolve(id);
                if crate::ignore::path_ignored(&path) {
                    continue;
                }
                let (size, mtime) = stat_path(&path);
                out.push(Entry {
                    path,
                    name: self.name_at(r.name_id).to_string(),
                    is_dir: r.is_dir(),
                    size,
                    mtime: mtime as u64,
                });
            }
            if out.len() >= limit {
                break;
            }
        }
        out.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.path.cmp(&b.path))
        });
        out
    }

    /// Apply live journal records: create / rename / delete. `base` is the
    /// volume's MFT-number offset inside `frn`.
    pub fn apply(&mut self, base: u32, chs: &[Change]) {
        for ch in chs {
            let is_dir = ch.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
            let k = base as usize + ch.frn as usize;
            let rid = self.frn.get(k).copied().unwrap_or(0);
            if ch.reason & usn::REASON_FILE_DELETE != 0 {
                if rid != 0 {
                    if let Some(r) = self.recs.get_mut((rid - 1) as usize) {
                        r.parent |= DEAD_BIT;
                    }
                }
                if let Some(s) = self.frn.get_mut(k) {
                    *s = 0;
                }
                continue;
            }
            if ch.reason & (usn::REASON_FILE_CREATE | usn::REASON_RENAME_NEW_NAME) == 0 {
                continue;
            }
            if crate::ignore::skip_name(&ch.name) {
                continue;
            }
            let pid = self
                .frn
                .get(base as usize + ch.parent as usize)
                .copied()
                .unwrap_or(0);
            if pid == 0 {
                // Parent pruned (ignored subtree) or not indexed: skip.
                continue;
            }
            let nid = self.intern(&ch.name);
            if rid != 0 {
                let ri = (rid - 1) as usize;
                let old_nid = self.recs[ri].name_id;
                self.recs[ri].name_id = nid;
                self.recs[ri].parent = (pid - 1) | if is_dir { DIR_BIT } else { 0 };
                if old_nid != nid {
                    // Re-link into the new name's chain: search walks unique-name
                    // chains, so a rename without relinking would be invisible.
                    self.next[ri] = self.first[nid as usize];
                    self.first[nid as usize] = rid;
                }
            } else {
                let id = self.push_rec(pid - 1, is_dir, nid);
                if k >= self.frn.len() {
                    self.frn.resize(k + 1, 0);
                }
                self.frn[k] = id + 1;
            }
        }
    }

    pub fn sync_vol(&mut self, drive: u8, journal_id: u64, next_usn: i64) {
        if let Some(v) = self.vol.iter_mut().find(|v| v.drive == drive) {
            v.journal_id = journal_id;
            v.next_usn = next_usn;
        } else {
            self.vol.push(VolMeta {
                drive,
                base: 0,
                journal_id,
                next_usn,
            });
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

/// One stat per returned hit (sizes/dates are not stored in the index).
fn stat_path(p: &str) -> (u64, u32) {
    let w = wide(p);
    let mut fd: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(w.as_ptr(), GetFileExInfoStandard, &mut fd as *mut _ as *mut _)
    };
    if ok == 0 {
        return (0, 0);
    }
    let size = (fd.nFileSizeHigh as u64) << 32 | fd.nFileSizeLow as u64;
    (size, ft_to_unix(&fd.ftLastWriteTime))
}

// -------- scan roots --------

/// Per-user config first, then machine-wide (the SYSTEM task has its own empty
/// profile, so `%APPDATA%` under SYSTEM is useless).
pub fn cfg_file(name: &str) -> Option<PathBuf> {
    for base in ["APPDATA", "ProgramData"] {
        if let Some(d) = std::env::var_os(base) {
            let p = Path::new(&d).join("everything233").join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Whitelist roots from include.txt (one root per line, # = comment).
fn include_roots() -> Option<Vec<String>> {
    let p = cfg_file("include.txt")?;
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
            "Desktop", "Documents", "Downloads", "OneDrive", "Favorites", "Pictures",
            "Videos", "Music", "source", "scripts", "DoubaoWork", "WPS Cloud",
            "WPS Cloud Files",
        ] {
            let p = h.join(sub);
            if p.is_dir() {
                roots.push(p.to_string_lossy().into_owned());
            }
        }
    }
    roots
}

/// (drive, path prefix below that drive, lowercased); "" = the whole volume.
fn root_specs(roots: &[String]) -> Vec<(char, String)> {
    let mut v: Vec<(char, String)> = Vec::new();
    for r in roots {
        let n = r.replace('/', "\\");
        let mut it = n.chars();
        let Some(d) = it.next() else { continue };
        let rest: String = it.collect();
        if !rest.starts_with(':') {
            continue;
        }
        let drive = d.to_ascii_uppercase();
        let prefix = rest[1..].trim_start_matches('\\').to_ascii_lowercase();
        v.push((drive, prefix));
    }
    v.sort();
    v.dedup();
    v
}

// -------- build: elevated USN enumeration --------

pub fn build_usn(roots: &[String]) -> Option<Index> {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    let specs = root_specs(roots);
    if specs.is_empty() {
        return None;
    }
    let mut idx = Index::default();
    let mut drives: Vec<char> = specs.iter().map(|s| s.0).collect();
    drives.sort_unstable();
    drives.dedup();
    let mut used = 0usize;
    let mut base_next: u32 = 0;

    for drive in drives {
        let Some(mut vol) = usn::open(drive) else { continue };

        // FSCTL_ENUM_USN_DATA is NTFS-only. ReFS/exFAT volumes get a one-time
        // walk (their journal cannot be mapped back to our 8-byte records).
        if usn::fs(drive) != "NTFS" {
            let dr = format!("{}:", drive);
            for r in roots {
                if r.to_ascii_uppercase().starts_with(&dr) {
                    walk_root(r, &mut idx, 0);
                }
            }
            idx.vol.push(VolMeta {
                drive: drive as u8,
                base: 0,
                journal_id: 0,
                next_usn: 0,
            });
            used += 1;
            continue;
        }

        // Whitelist prefixes for this drive, as lowercase path components.
        let mut prefixes: Vec<Vec<String>> = Vec::new();
        let mut whole = false;
        for (d, p) in &specs {
            if *d != drive {
                continue;
            }
            if p.is_empty() {
                whole = true;
            } else {
                prefixes.push(p.split('\\').map(|s| s.to_string()).collect());
            }
        }
        if prefixes.len() > 15 {
            whole = true; // bitmask capacity; broadening is safe, never narrows
        }

        // Pass 1: compact arrays of kept-by-name entries.
        let mut par: Vec<u32> = Vec::new();
        let mut nid: Vec<u32> = Vec::new();
        let mut dir: Vec<u8> = Vec::new();
        let mut mft: Vec<u32> = Vec::new();
        let mut root_mft: u32 = u32::MAX;
        let mut max_mft: u32 = 0;
        let mut scratch = String::with_capacity(256);
        {
            let names = &mut idx.names;
            let name_off = &mut idx.name_off;
            let first = &mut idx.first;
            let seen = &mut idx.seen;
            vol.enumerate(|frn, parent, name16, attrs| {
                if frn == parent {
                    root_mft = frn; // $Root: no name, becomes the "D:" anchor
                    return;
                }
                if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return; // junctions / symlinks: never descend
                }
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                scratch.clear();
                for &c in name16 {
                    scratch.push(char::from_u32(c as u32).unwrap_or('\u{fffd}'));
                }
                if scratch.is_empty() || crate::ignore::skip_name(&scratch) {
                    return;
                }
                let name_id = match seen.get(scratch.as_str()) {
                    Some(&id) => id,
                    None => {
                        let id = name_off.len() as u32;
                        let off = names.len() as u32;
                        names.extend_from_slice(&(scratch.len() as u16).to_le_bytes());
                        names.extend_from_slice(scratch.as_bytes());
                        name_off.push(off);
                        first.push(0);
                        seen.insert(scratch.clone(), id);
                        id
                    }
                };
                mft.push(frn);
                par.push(parent);
                nid.push(name_id);
                dir.push(is_dir as u8);
                if frn > max_mft {
                    max_mft = frn;
                }
            });
        }
        if mft.is_empty() {
            continue;
        }
        if root_mft == u32::MAX {
            root_mft = 5;
        }

        // MFT no -> raw index + 1
        let mut m2r = vec![0u32; max_mft as usize + 2];
        for (i, &m) in mft.iter().enumerate() {
            m2r[m as usize] = i as u32 + 1;
        }

        // Children adjacency (head[parent] and sib[child]); `top` = children of $Root.
        let mut head = vec![0u32; mft.len() + 1];
        let mut sib = vec![0u32; mft.len()];
        let mut top: Vec<u32> = Vec::new();
        for i in 0..mft.len() {
            let p = par[i];
            if p == root_mft {
                top.push(i as u32);
                continue;
            }
            let pi = m2r[p as usize];
            if pi == 0 {
                continue; // parent pruned by name
            }
            let pidx = (pi - 1) as usize;
            sib[i] = head[pidx];
            head[pidx] = i as u32 + 1;
        }

        // Volume anchor record: "D:". MFT-number keys live in one flat vec,
        // offset by `base`, so several drives never collide.
        let base = base_next as usize;
        let span = max_mft as usize + 2;
        if idx.frn.len() < base + span {
            idx.frn.resize(base + span, 0);
        }
        let root_rec = idx.add_named(ROOT, true, &format!("{}:", drive));
        idx.frn[base + root_mft as usize] = root_rec + 1;

        // Level-order walk, keeping whitelist prefixes (bitmask = subtree fully matched).
        let nprefix = prefixes.len();
        let full_bits: u16 = if nprefix >= 16 { u16::MAX } else { (1u16 << nprefix) - 1 };
        let mut fid = vec![u32::MAX; mft.len()];
        let mut stack: Vec<(u32, u16, u16)> = Vec::with_capacity(1024);
        for &t in &top {
            stack.push((t, 1, if whole { full_bits } else { 0 }));
        }
        let mut qi = 0usize;
        let mut emitted = 0usize;
        while qi < stack.len() {
            let (raw, depth, bits) = stack[qi];
            qi += 1;
            let i = raw as usize;
            let is_dir = dir[i] != 0;

            let mut nb = bits;
            if !whole {
                scratch.clear();
                for c in idx.name_at(nid[i]).chars() {
                    scratch.push(c.to_ascii_lowercase());
                }
                let mut partial = false;
                for (pi, comps) in prefixes.iter().enumerate() {
                    if bits & (1u16 << pi) != 0 {
                        continue;
                    }
                    let d = depth as usize;
                    if d <= comps.len() && comps[d - 1].as_str() == scratch.as_str() {
                        if d == comps.len() {
                            nb |= 1u16 << pi;
                        } else {
                            partial = true;
                        }
                    }
                }
                if !is_dir {
                    if nb == 0 {
                        continue; // file outside any whitelisted subtree
                    }
                } else if nb == 0 && !partial {
                    continue; // dir neither kept nor an ancestor of a kept subtree
                }
            }

            // Parent must already be emitted (BFS order guarantees it).
            let prec = if par[i] == root_mft {
                root_rec
            } else {
                let pi = m2r[par[i] as usize];
                if pi == 0 {
                    continue;
                }
                let v = fid[(pi - 1) as usize];
                if v == u32::MAX {
                    continue;
                }
                v
            };
            let rec = idx.push_rec(prec, is_dir, nid[i]);
            idx.frn[base + mft[i] as usize] = rec + 1;
            fid[i] = rec;
            emitted += 1;
            if is_dir {
                let mut c = head[i];
                while c != 0 {
                    let ci = (c - 1) as usize;
                    stack.push((ci as u32, depth + 1, nb));
                    c = sib[ci];
                }
            }
        }

        if emitted > 0 {
            used += 1;
            idx.vol.push(VolMeta {
                drive: drive as u8,
                base: base as u32,
                journal_id: vol.journal_id,
                next_usn: vol.next_usn,
            });
        }
        base_next = (base + span) as u32;
        // Transient build arrays die here.
        drop(m2r);
        drop(head);
        drop(sib);
        drop(fid);
        drop(par);
        drop(nid);
        drop(dir);
        drop(mft);
    }

    if used == 0 || idx.recs.is_empty() {
        return None;
    }
    eprintln!(
        "usn: {} unique names, {} bytes, {} records",
        idx.name_off.len(),
        idx.names.len(),
        idx.recs.len()
    );
    idx.seen.clear();
    idx.seen.shrink_to_fit();
    Some(idx)
}

// -------- build: fallback walk (no elevation) --------

pub fn build_walk(roots: &[String]) -> Index {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    let mut idx = Index::default();
    for root in roots {
        walk_root(root, &mut idx, 0);
    }
    eprintln!(
        "walk: {} unique names, {} bytes, {} records",
        idx.name_off.len(),
        idx.names.len(),
        idx.recs.len()
    );
    idx.seen.clear();
    idx.seen.shrink_to_fit();
    idx
}

fn walk_root(root: &str, idx: &mut Index, depth: u32) {
    let root_id = idx.add_named(ROOT, true, root);
    let mut stack: Vec<(String, u32, u32)> = vec![(root.to_string(), root_id, depth)];
    let mut budget: u32 = 0;
    while let Some((dir, parent, dep)) = stack.pop() {
        let pattern = format!("\\\\?\\{dir}\\*");
        let w = wide(&pattern);
        let mut fd = unsafe { std::mem::zeroed::<WIN32_FIND_DATAW>() };
        let h = unsafe { FindFirstFileW(w.as_ptr(), &mut fd) };
        if h == INVALID_HANDLE_VALUE {
            continue;
        }
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
                            let id = idx.add_named(parent, true, &name);
                            stack.push((full, id, dep + 1));
                        }
                    } else {
                        idx.add_named(parent, false, &name);
                    }
                }
                budget += 1;
                if budget & 0xFFF == 0 {
                    std::thread::yield_now();
                }
            }
            if unsafe { FindNextFileW(h, &mut fd) } == 0 {
                break;
            }
        }
        unsafe { FindClose(h) };
    }
}

// -------- live refresh --------

/// Blocking journal tail: one thread per volume, kernel-side wait -> 0 idle CPU.
pub fn spawn_usn(idx: Shared, vols: Vec<usn::Volume>) {
    for mut v in vols {
        let idx = idx.clone();
        let drive = v.drive;
        let mut base = idx
            .read()
            .unwrap()
            .vol
            .iter()
            .find(|m| m.drive == drive as u8)
            .map(|m| m.base)
            .unwrap_or(0);
        std::thread::Builder::new()
            .name(format!("usn-{drive}"))
            .spawn(move || {
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
                let mut dirty = false;
                let mut last_flush = std::time::Instant::now();
                let dbg = std::env::var_os("E233_DBG").is_some();
                let mut iters: u64 = 0;
                loop {
                    let t_read = std::time::Instant::now();
                    match v.read_batch(15) {
                        Ok(chs) => {
                            if dbg && (iters < 40 || iters % 500 == 0) {
                                eprintln!(
                                    "dbg tail {drive} #{iters} {} ms len {}",
                                    t_read.elapsed().as_millis(),
                                    chs.len()
                                );
                            }
                            iters += 1;
                            if !chs.is_empty() {
                                let mut g = idx.write().unwrap();
                                g.apply(base, &chs);
                                g.sync_vol(drive as u8, v.journal_id, v.next_usn);
                                dirty = true;
                            }
                            if dirty && last_flush.elapsed().as_secs() >= 30 {
                                // Watermarks only: tiny write, keeps restart catch-up exact.
                                save_watermarks(&idx.read().unwrap().vol);
                                dirty = false;
                                last_flush = std::time::Instant::now();
                            }
                        }
                        Err(e) => {
                            eprintln!("usn {drive}: err {e}; rebuilding");
                            match usn::open(drive) {
                                Some(nv) => v = nv,
                                None => {
                                    std::thread::sleep(std::time::Duration::from_secs(30));
                                    continue;
                                }
                            }
                            if let Some(fresh) = build_usn(&scan_roots()) {
                                base = fresh
                                    .vol
                                    .iter()
                                    .find(|m| m.drive == drive as u8)
                                    .map(|m| m.base)
                                    .unwrap_or(0);
                                *idx.write().unwrap() = fresh;
                                dirty = true;
                            }
                        }
                    }
                }
            })
            .ok();
    }
}

/// Cheap signature of scan roots' top level; changed => a rebuild is warranted.
fn root_sig() -> u64 {
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
    for root in scan_roots() {
        let pattern = format!("\\\\?\\{root}\\*");
        let w = wide(&pattern);
        let mut fd = unsafe { std::mem::zeroed::<WIN32_FIND_DATAW>() };
        let hnd = unsafe { FindFirstFileW(w.as_ptr(), &mut fd) };
        if hnd == INVALID_HANDLE_VALUE {
            continue;
        }
        loop {
            let name = from_wide(&fd.cFileName);
            if name != "." && name != ".." {
                h = h.wrapping_add(name.len() as u64);
                h = h.wrapping_mul(0x1000_0000_1B3);
                h ^= ft_u64(&fd.ftLastWriteTime);
            }
            if unsafe { FindNextFileW(hnd, &mut fd) } == 0 {
                break;
            }
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
                    let fresh = build_walk(&scan_roots());
                    *idx.write().unwrap() = fresh;
                    eprintln!("index refreshed (root changed)");
                }
            }
        })
        .ok();
}

// -------- display helpers --------

pub fn fmt_ft(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
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
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn ft_to_unix(ft: &windows_sys::Win32::Foundation::FILETIME) -> u32 {
    let t = ft_u64(ft);
    if t == 0 {
        return 0;
    }
    ((t / 10_000_000).saturating_sub(11_644_473_600)) as u32
}

pub fn human_size(n: u64) -> String {
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    let s = if u == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", ["KB", "MB", "GB", "TB"][u - 1])
    };
    s
}
