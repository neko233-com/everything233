// Named-pipe IPC between `serve` (daemon) and query clients.
// Request line: `query\nlimit\nflags`  (flags: "" / "d" / "f")
// Response: one message, `size\tdate\tpath` lines, count header first.
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

use crate::index::{fmt_ft, human_size, Shared};

const PIPE: &str = r"\\.\pipe\everything233";

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

pub fn run(idx: Shared) -> ! {
    let name = wide(PIPE);
    loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                1,
                65_536,
                65_536,
                0,
                std::ptr::null(),
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let ok = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // ERROR_PIPE_CONNECTED (535) means a client already arrived; fine.
            if err.raw_os_error() != Some(535) {
                unsafe { CloseHandle(pipe) };
                continue;
            }
        }
        serve_one(pipe, &idx);
        unsafe {
            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
    }
}

fn serve_one(pipe: HANDLE, idx: &Shared) {
    let mut buf = vec![0u8; 16_384];
    let mut got = 0u32;
    let ok = unsafe { ReadFile(pipe, buf.as_mut_ptr(), buf.len() as u32, &mut got, std::ptr::null_mut()) };
    if ok == 0 || got == 0 {
        return;
    }
    let units: Vec<u16> = buf[..got as usize]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let req = String::from_utf16_lossy(&units);
    let mut parts = req.split('\n');
    let q = parts.next().unwrap_or("").trim().to_string();
    let limit: usize = parts
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(200);
    let flags = parts.next().unwrap_or("").trim().to_string();
    let dir_only = flags == "d";
    let file_only = flags == "f";

    let entries = idx.lock().unwrap().search(&q, dir_only, file_only, limit);
    let mut out = format!("{} result(s) for \"{q}\"\n", entries.len());
    for e in &entries {
        let size = if e.is_dir {
            "       <DIR>  ".to_string()
        } else {
            human_size(e.size)
        };
        out.push_str(&format!("{size:>12}  {}  {}\n", fmt_ft(e.mtime), e.path));
    }
    let w = wide(&out);
    let mut sent = 0u32;
    unsafe { WriteFile(pipe, w.as_ptr() as *const _, (w.len() * 2) as u32, &mut sent, std::ptr::null_mut()) };
}

/// Talk to a running daemon. Err => daemon not up; caller falls back to foreground build.
pub fn client(q: &str, limit: usize, flags: &str) -> io::Result<String> {
    let name = wide(PIPE);
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(io::ErrorKind::NotFound, "daemon not running"));
    }
    let req = format!("{q}\n{limit}\n{flags}\n");
    let w = wide(&req);
    let mut sent = 0u32;
    unsafe { WriteFile(h, w.as_ptr() as *const _, (w.len() * 2) as u32, &mut sent, std::ptr::null_mut()) };

    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 65_536];
    loop {
        let mut got = 0u32;
        let ok = unsafe { ReadFile(h, chunk.as_mut_ptr(), chunk.len() as u32, &mut got, std::ptr::null_mut()) };
        if ok == 0 || got == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..got as usize]);
    }
    unsafe { CloseHandle(h) };
    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}
