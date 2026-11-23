// Built-in skip list: VCS metadata, Unity/engine caches, build output.
// Directory-name match, case-insensitive. Extra names go in
// %APPDATA%\everything233\ignore.txt (one per line, # = comment).
use std::collections::HashSet;
use std::sync::OnceLock;

const BUILTIN: &[&str] = &[
    // VCS / SCM metadata
    ".git", ".svn", ".hg", ".jj",
    // IDE metadata
    ".vs", ".idea",
    // package manager caches
    "node_modules",
    // Unity editor caches (never source)
    "library", "temp", "obj",
    // Rust build output
    "target",
    // language caches
    "__pycache__", ".gradle", ".nuget", ".cache",
];

static EXTRA: OnceLock<HashSet<String>> = OnceLock::new();

fn extra() -> &'static HashSet<String> {
    EXTRA.get_or_init(|| {
        let mut s = HashSet::new();
        if let Some(dir) = std::env::var_os("APPDATA") {
            let p = std::path::Path::new(&dir)
                .join("everything233")
                .join("ignore.txt");
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

/// True if any path component (backslash-split) is ignored.
pub fn path_ignored(path: &str) -> bool {
    path.split(['\\', '/']).any(|c| !c.is_empty() && is_ignored(c))
}
