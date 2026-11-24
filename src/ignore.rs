// Built-in skip list: VCS metadata, package/engine caches, build output, system trees.
// Directory-name match, case-insensitive. Extra names go in
// %APPDATA%\everything233\ignore.txt (one per line, # = comment).
use std::collections::HashSet;
use std::sync::OnceLock;

const BUILTIN: &[&str] = &[
    // VCS / SCM metadata
    ".git", ".svn", ".hg", ".jj",
    // IDE metadata
    ".vs", ".idea", ".vscode",
    // package manager caches
    "node_modules", "site-packages", ".venv", "venv", ".conda",
    ".next", "pods", ".dart_tool",
    ".mypy_cache", ".pytest_cache", ".tox", ".ruff_cache", ".eslintcache",
    // Unity editor caches (never source)
    "library", "temp", "obj", "logs", "saved", "crashdumps", "userdata",
    // Rust / Java build output
    "target", ".gradle", ".nuget", ".cache",
    // bootstrap tool caches inside office folders
    ".rust-bootstrap",
    ".python-bootstrap",
    ".node-bootstrap",
    ".deno-bootstrap",
    // Python bytecode
    "__pycache__",
    // generic build output dirs
    "build", "out",
];

// System trees that must never be entered, matched by NAME at any depth (the
// USN builder sees names only, so this replaces the old full-path prefixes).
const SYS: &[&str] = &[
    "windows", "program files", "program files (x86)", "programdata", "appdata",
    "$recycle.bin", "system volume information", "recovery", "msocache",
    "config.msi", "winsxs", "$windows.~bt", "$windows.~ws", "boot",
    "efi", "perflogs", "intel", "documents and settings",
];

// Full-path segment prefixes (lowercased) never indexed: OS trees, package caches.
const PREFIX: &[&str] = &[
    r"\windows\",
    r"\program files\",
    r"\program files (x86)\",
    r"\programdata\",
    r"\appdata\",
    r"\go\pkg\mod\",
    r"\.cargo\registry\",
    r"\cargo\registry\",
    r"\node_modules\",
];

// Top-level directory names on data drives that are never entered: game platforms,
// downloaders, caches, temp, recycle bin, backups. Matched case-insensitively on the
// first level under a scanned drive root only. NOTE: keep names precise — bare
// "game"/"games" would block real project folders like GameApp/GameVideos.
const TOP_SKIP: &[&str] = &[
    // game platforms & libraries (exact platform names only)
    "steam", "steamlibrary", "steamapps", "wegame", "wegameapps", "taptap",
    "qldownload", "qldownloadgame", "mihoyo-games", "mihoyo", "epic games",
    "riot games", "battle.net", "xbox", "gog galaxy", "origin games",
    "ubisoft", "uplay", "epic",
    // caches / temp / downloaders / logs
    "cache", "qycache", "tmp", "temp", "ttnet", "mailmasterdata", "logs",
    "log", "wechat_logs", "nvidia corporation", "nvidia", "crashdumps",
    "$recycle.bin", "system volume information", "recovery", "config.msi",
    "msocache", "baidu netdiskdownload", "baidunetdiskdownload",
    // program trees that also appear on data drives
    "program files", "program files (x86)", "programdata",
    // backups (bulk, not search targets)
    "backups", "backup", "_backup",
];

static EXTRA: OnceLock<HashSet<String>> = OnceLock::new();

fn extra() -> &'static HashSet<String> {
    EXTRA.get_or_init(|| {
        let mut s = HashSet::new();
        if let Some(p) = crate::index::cfg_file("ignore.txt") {
            if let Ok(txt) = std::fs::read_to_string(p) {
                for line in txt.lines() {
                    let l = line.trim();
                    if !l.is_empty() && !l.starts_with('#') {
                        s.insert(l.to_ascii_lowercase());
                    }
                }
            }
        }
        s
    })
}

pub fn is_ignored(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    BUILTIN.contains(&n.as_str()) || extra().contains(&n)
}

/// Data-drive top-level dir to skip entirely (game platforms, caches, temp, backups).
pub fn top_skipped(name: &str) -> bool {
    TOP_SKIP.contains(&name.to_ascii_lowercase().as_str())
}

/// Single-call name filter used by the USN builder and live patcher: VCS/build
/// caches, system trees, and game/cache/backup top-level dirs.
pub fn skip_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    BUILTIN.contains(&n.as_str())
        || SYS.contains(&n.as_str())
        || TOP_SKIP.contains(&n.as_str())
        || extra().contains(&n)
}

/// True if any path component (backslash-split) is ignored.
pub fn path_ignored(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    PREFIX.iter().any(|p| l.contains(p))
        || l.split(['\\', '/']).any(|c| !c.is_empty() && is_ignored(c))
}

/// Build-time path filter: same rules, used while walking.
pub fn walk_ignored(path: &str) -> bool {
    path_ignored(path)
}
