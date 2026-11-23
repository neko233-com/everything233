mod daemon;
mod ignore;
mod index;
mod reg;

use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return;
    }
    if args[0] == "--version" || args[0] == "-V" {
        println!("everything {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    match args[0].as_str() {
        "serve" => serve(&args[1..]),
        "install" => {
            let exe = current_exe();
            match reg::install_run(&exe) {
                Ok(_) => println!("daemon registered (HKCU Run): \"{exe}\" serve --background"),
                Err(e) => eprintln!("register failed: {e}"),
            }
        }
        "uninstall" => match reg::uninstall_run() {
            Ok(_) => println!("daemon unregistered"),
            Err(e) => eprintln!("unregister failed: {e}"),
        },
        _ => query(&args),
    }
}

fn current_exe() -> String {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    String::from_utf16_lossy(&buf[..n as usize])
}

/// `serve --background`: relaunch detached, then exit.
fn detach_self() {
    let exe = current_exe();
    let cmd = format!("\"{exe}\" serve --background");
    let mut cmdw: Vec<u16> = std::ffi::OsStr::new(&cmd)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut si = unsafe { std::mem::zeroed::<STARTUPINFOW>() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi = unsafe { std::mem::zeroed::<PROCESS_INFORMATION>() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmdw.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &mut si,
            &mut pi,
        )
    };
    if ok != 0 {
        unsafe {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        std::process::exit(0);
    }
}

fn serve(rest: &[String]) -> ! {
    if rest.iter().any(|a| a == "--background") {
        detach_self();
    }
    let rebuild = rest.iter().any(|a| a == "--rebuild");
    let cache = index::cache_path();
    eprintln!("everything v{} starting...", env!("CARGO_PKG_VERSION"));
    let built = match &cache {
        Some(p) if !rebuild && p.exists() => index::Index::load(p).unwrap_or_default(),
        _ => {
            eprintln!("building index (first run, or --rebuild)...");
            let i = index::build();
            if let Some(p) = &cache {
                i.save(p);
            }
            i
        }
    };
    let idx: index::Shared = Arc::new(Mutex::new(built));
    eprintln!("indexed {} entries", idx.lock().unwrap().len());
    for root in index::fixed_roots() {
        index::spawn_watch(root, idx.clone());
    }
    eprintln!("serving on \\\\.\\pipe\\everything233");
    daemon::run(idx)
}

fn query(args: &[String]) {
    let mut dir_only = false;
    let mut file_only = false;
    let mut limit = 200usize;
    let mut q = String::new();
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "-d" => dir_only = true,
            "-f" => file_only = true,
            "-n" => {
                i += 1;
                limit = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(200);
            }
            other => {
                if !q.is_empty() {
                    q.push(' ');
                }
                q.push_str(other);
            }
        }
        i += 1;
    }
    if q.is_empty() {
        print_help();
        return;
    }

    let flags = if dir_only { "d" } else if file_only { "f" } else { "" };
    match daemon::client(&q, limit, flags) {
        Ok(resp) => print!("{resp}"),
        Err(_) => {
            eprintln!("daemon not running. Start it once: everything serve  (or: everything install)");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!(
        "everything v{} — filename search for Windows (no content scan)\n\
         \n\
         Usage:\n\
           everything <query>      search via daemon; on-the-fly build if daemon absent\n\
           everything serve        load/build index, watch changes, answer pipe queries\n\
           everything serve --rebuild   force full rebuild, rewrite cache\n\
           everything install     register daemon at logon (HKCU Run, no admin)\n\
           everything uninstall   unregister daemon\n\
         \n\
         Options:\n\
           -d           folders only\n\
           -f           files only\n\
           -n <num>     result limit (default 200)\n\
           *            wildcard in query\n\
         \n\
         Built-in skips: .git .svn .hg .jj .vs .idea node_modules Library Temp obj \\\n\
         target __pycache__ .gradle .nuget .cache; extra names in \\\n\
         %APPDATA%\\everything233\\ignore.txt",
        env!("CARGO_PKG_VERSION")
    );
}
