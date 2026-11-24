// Daemon registration.
// Preferred: a SYSTEM scheduled task at boot with highest privileges -> raw
// volume access (USN journal), index built in seconds, nothing to elevate later.
// Fallback (non-admin): HKCU Run key, walk build.
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::process::{Command, Stdio};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "everything233";
pub const TASK_NAME: &str = "everything233";

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn run_hidden(cmd: &str, args: &[&str]) -> io::Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        return Err(io::Error::new(io::ErrorKind::Other, s.trim().to_string()));
    }
    Ok(s)
}

/// Publish the caller's config machine-wide: the SYSTEM daemon cannot read a
/// user profile, so it falls back to `%ProgramData%\everything233\*`.
pub fn publish_config() {
    let (Some(app), Some(pd)) = (
        std::env::var_os("APPDATA"),
        std::env::var_os("ProgramData"),
    ) else {
        return;
    };
    let dst_dir = std::path::Path::new(&pd).join("everything233");
    let _ = std::fs::create_dir_all(&dst_dir);
    for name in ["include.txt", "ignore.txt"] {
        let src = std::path::Path::new(&app).join("everything233").join(name);
        let dst = dst_dir.join(name);
        if src.exists() && !dst.exists() {
            let _ = std::fs::copy(src, dst);
        }
    }
}

/// SYSTEM + highest privileges, started at boot/logon. Requires admin.
pub fn install_task(exe: &str) -> io::Result<()> {
    publish_config();
    let tr = format!("\"{exe}\" serve");
    run_hidden(
        "schtasks",
        &[
            "/create",
            "/tn",
            TASK_NAME,
            "/tr",
            &tr,
            "/sc",
            "onstart",
            "/ru",
            "SYSTEM",
            "/rl",
            "HIGHEST",
            "/delay",
            "0000:10",
            "/f",
        ],
    )?;
    // Restart it now so the new build/config takes effect immediately.
    let _ = run_hidden("schtasks", &["/end", "/tn", TASK_NAME]);
    let _ = run_hidden("schtasks", &["/run", "/tn", TASK_NAME]);
    Ok(())
}

pub fn uninstall_task() -> io::Result<()> {
    // Stop a running instance, then delete the task.
    let _ = run_hidden("schtasks", &["/end", "/tn", TASK_NAME]);
    run_hidden("schtasks", &["/delete", "/tn", TASK_NAME, "/f"]).map(|_| ())
}

pub fn install_run(exe: &str) -> io::Result<()> {
    let sub = wide(RUN_KEY);
    let mut h: HANDLE = std::ptr::null_mut();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut h) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let val = format!("\"{exe}\" serve --background");
    let w = wide(&val);
    let bytes = (w.len() * 2) as u32;
    unsafe {
        RegSetValueExW(
            h,
            wide(VALUE_NAME).as_ptr(),
            0,
            REG_SZ,
            w.as_ptr() as *const u8,
            bytes,
        );
        RegCloseKey(h);
    }
    Ok(())
}

pub fn uninstall_run() -> io::Result<()> {
    let sub = wide(RUN_KEY);
    let mut h: HANDLE = std::ptr::null_mut();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut h) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        RegDeleteValueW(h, wide(VALUE_NAME).as_ptr());
        RegCloseKey(h);
    }
    Ok(())
}

/// True when this process already holds an elevated token.
pub fn is_elevated() -> bool {
    // Raw physical-drive access is administrator-only; volume handles are not
    // (read-only IOCTLs like USN query are callable unelevated on modern Windows).
    crate::usn::can_open_physical()
}
