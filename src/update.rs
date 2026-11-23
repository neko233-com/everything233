// Manual `everything update` and startup auto-check.
// Download route: probe GitHub mirrors by latency, pick the fastest, fallback chain.
// Applier: staged MSI + elevated msiexec (PowerShell Start-Process -Verb RunAs), then restart.
// Uses built-in curl.exe / powershell.exe (ship with Windows); no extra runtime dep.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

const REPO: &str = "neko233-com/everything233";
const API: &str = "https://api.github.com/repos/neko233-com/everything233/releases/latest";

// Mirror prefixes prepended to the github URL; direct first, proxies as fallback/fast route.
const MIRRORS: &[&str] = &[
    "https://github.com",
    "https://ghfast.top/https://github.com",
    "https://gh-proxy.com/https://github.com",
    "https://ghproxy.net/https://github.com",
];

fn curl(args: &[&str]) -> Option<String> {
    let out = Command::new("curl.exe").args(args).output().ok()?;
    if !out.status.success() {
        eprintln!("curl {} exited {:?}", args.join(" "), out.status.code());
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Latest release tag from GitHub API, e.g. "v0.2.0".
pub fn latest_tag() -> Option<String> {
    let body = curl(&["-s", "-m", "10", "-A", "everything233-update", "-H", "Accept: application/vnd.github+json", API])?;
    let i = body.find("\"tag_name\"")?;
    let colon = body[i..].find(':')? + i + 1;
    let q1 = body[colon..].find('"')? + colon + 1;
    let q2 = body[q1..].find('"')? + q1;
    Some(body[q1..q2].to_string())
}

fn parse_ver(tag: &str) -> Option<(u64, u64, u64)> {
    let v = tag.trim_start_matches('v');
    let mut p = v.split('.');
    let a = p.next()?.parse().ok()?;
    let b = p.next()?.parse().ok()?;
    let c = p.next().unwrap_or("0").parse().ok()?;
    Some((a, b, c))
}

pub fn newer_than_current(remote_tag: &str) -> bool {
    let (Some(rv), Some(lv)) = (parse_ver(remote_tag), parse_ver(std::env!("CARGO_PKG_VERSION"))) else {
        return false;
    };
    rv > lv
}

/// Time a mirror with a HEAD request; lower is better. None = unreachable.
fn probe(base: &str) -> Option<f64> {
    let url = format!("{base}/{REPO}/releases/latest");
    let s = curl(&["-s", "-o", "NUL", "-m", "4", "-I", "-w", "%{time_total}", &url])?;
    s.trim().parse::<f64>().ok()
}

/// Pick the fastest reachable mirror by measured latency; fallback to direct.
fn pick_mirror() -> &'static str {
    let mut best: Option<(f64, &'static str)> = None;
    for m in MIRRORS {
        if let Some(t) = probe(m) {
            match best {
                Some((bt, _)) if bt <= t => {}
                _ => best = Some((t, m)),
            }
        }
    }
    best.map(|(_, m)| m).unwrap_or(MIRRORS[0])
}

fn asset_url(tag: &str, mirror: &str) -> String {
    format!("{mirror}/{REPO}/releases/download/{tag}/everything233.msi")
}

fn download(url: &str, dest: &std::path::Path) -> bool {
    Command::new("curl")
        .args(["-L", "--fail", "--retry", "2", "-m", "600", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn stage_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| {
        let p = PathBuf::from(d).join("everything233").join("update");
        let _ = std::fs::create_dir_all(&p);
        p
    })
}

/// Kill every everything.exe except this CLI process (the daemon).
fn stop_daemon() {
    let self_pid = unsafe { GetCurrentProcessId() };
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return;
    }
    let mut e = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
    e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snap, &mut e) };
    while ok != 0 {
        let name = String::from_utf16_lossy(
            &e.szExeFile[..e.szExeFile.iter().position(|&c| c == 0).unwrap_or(260)],
        );
        if name.eq_ignore_ascii_case("everything.exe") && e.th32ProcessID != self_pid {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &e.th32ProcessID.to_string()])
                .status();
        }
        ok = unsafe { Process32NextW(snap, &mut e) };
    }
    unsafe { CloseHandle(snap) };
}

/// Elevated msiexec install via PowerShell Start-Process -Verb RunAs (UAC prompt).
fn elevate_install(msi: &std::path::Path) {
    let ps = format!(
        "Start-Process msiexec -ArgumentList '/i','{}','/qn','/norestart' -Verb RunAs",
        msi.display()
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .status();
}

fn relaunch_daemon() {
    std::thread::sleep(Duration::from_secs(8));
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let _ = Command::new("cmd")
        .args(["/c", "start", "\"\"", &format!("\"{exe}\" serve --background")])
        .spawn();
}

/// Manual `everything update`: check, pick fastest mirror, download MSI, elevate-install, restart.
pub fn manual() {
    println!("checking latest release...");
    let Some(tag) = latest_tag() else {
        eprintln!("cannot reach GitHub API; check network");
        return;
    };
    if !newer_than_current(&tag) {
        println!("already up to date (v{})", std::env!("CARGO_PKG_VERSION"));
        return;
    }
    println!("new version {tag} available (current v{})", std::env!("CARGO_PKG_VERSION"));

    let mirror = pick_mirror();
    println!("fastest route: {mirror}");
    let Some(dir) = stage_dir() else {
        eprintln!("cannot create stage dir");
        return;
    };
    let msi = dir.join("everything233.msi");
    let url = asset_url(&tag, mirror);
    println!("downloading {url}");
    let mut ok = download(&url, &msi);
    if !ok {
        // fallback chain: try remaining mirrors in order
        for m in MIRRORS {
            if *m == mirror { continue; }
            let u = asset_url(&tag, m);
            println!("retry via {m}");
            if download(&u, &msi) { ok = true; break; }
        }
    }
    if !ok {
        eprintln!("download failed on all routes");
        return;
    }
    println!("installing (UAC prompt)...");
    stop_daemon();
    elevate_install(&msi);
    relaunch_daemon();
    println!("update applied; daemon restarting");
}

/// Startup auto-check (background thread in `serve`).
/// Stages the MSI and prints a notice; with `auto` it elevates and applies.
pub fn auto_check(auto: bool) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(8));
        let Some(tag) = latest_tag() else { return };
        if !newer_than_current(&tag) { return; }
        let mirror = pick_mirror();
        let Some(dir) = stage_dir() else { return };
        let msi = dir.join("latest.msi");
        if !download(&asset_url(&tag, mirror), &msi) { return; }
        if auto {
            eprintln!("auto-update: applying {tag} (UAC prompt)...");
            stop_daemon();
            elevate_install(&msi);
            relaunch_daemon();
        } else {
            eprintln!("new version {tag} staged at {}; run `everything update` to install", msi.display());
        }
    });
}
