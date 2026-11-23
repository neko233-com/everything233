// HKCU Run-key registration for the daemon. No admin needed.
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "everything233";

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
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
