mod daemon;
mod ignore;
mod index;
mod reg;
mod update;
mod usn;

use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, RwLock};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, CreateProcessW, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_INFORMATION,
    STARTUPINFOW,
};

/// One daemon only. A SYSTEM task and an interactive shell live in different
/// sessions, so a local mutex is not enough: probe the pipe first (the pipe
/// namespace is global), then take the mutex as a same-session backstop.
fn single_instance() {
    if daemon::probe() {
        eprintln!("everything daemon already running; exiting");
        std::process::exit(0);
    }
    const ERROR_ALREADY_EXISTS: u32 = 183;
    let name: Vec<u16> = std::ffi::OsStr::new("everything233-single")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let m = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    unsafe {
        if !m.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            eprintln!("everything daemon already running; exiting");
            std::process::exit(0);
        }
    }
    // Handle stays open for process lifetime; the OS releases it on exit.
}

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
            if reg::is_elevated() {
                match reg::install_task(&exe) {
                    Ok(_) => println!(
                        "daemon registered: scheduled task '{0}' (SYSTEM, highest) -> \"{1}\" serve",
                        reg::TASK_NAME, exe
                    ),
                    Err(e) => eprintln!("task registration failed: {e}"),
                }
            } else {
                match reg::install_run(&exe) {
                    Ok(_) => println!(
                        "daemon registered (HKCU Run, no elevation): \"{exe}\" serve --background\n\
                         note: run this elevated once to install the SYSTEM task and get USN speed"
                    ),
                    Err(e) => eprintln!("register failed: {e}"),
                }
            }
        }
        "uninstall" => {
            let _ = reg::uninstall_task();
            match reg::uninstall_run() {
                Ok(_) => println!("daemon unregistered"),
                Err(e) => eprintln!("unregister failed: {e}"),
            }
        }
        "update" => update::manual(),
        "probe" => {
            for d in ['C', 'D', 'E', 'F'] {
                let p = format!("{d}:\\");
                if !std::path::Path::new(&p).exists() {
                    println!("{d}: no such drive");
                    continue;
                }
                match usn::open(d) {
                    Some(mut v) => {
                        let t = std::time::Instant::now();
                        let mut rootc = 0u32;
                        let mut maxf = 0u32;
                        let mut par0 = 0u32;
                        let mut sample: Vec<(u32, u32, String)> = Vec::new();
                        let n = v.enumerate(|frn, par, name, _| {
                            if frn == par {
                                rootc += 1;
                            }
                            if par == 0 {
                                par0 += 1;
                            }
                            maxf = maxf.max(frn).max(par);
                            if sample.len() < 4 && !name.is_empty() {
                                sample.push((frn, par, String::from_utf16_lossy(name)));
                            }
                        });
                        println!(
                            "{d}: {} journal ok id={} enum {n} rootc {rootc} max_frn {maxf} par0 {par0} in {} ms",
                            usn::fs(d),
                            v.journal_id,
                            t.elapsed().as_millis()
                        );
                        for s in sample {
                            println!("   {s:?}");
                        }
                    }
                    None => println!("{d}: journal unavailable"),
                }
            }
        }
        "readtest" => {
            let d = args
                .get(1)
                .and_then(|s| s.chars().next())
                .unwrap_or('F');
            let Some(mut v) = usn::open(d) else {
                println!("no journal on {d}");
                return;
            };
            println!("{d}: next_usn={}", v.next_usn);
            for i in 0..3 {
                let t = std::time::Instant::now();
                let r = v.read_batch(15);
                let ms = t.elapsed().as_millis();
                match r {
                    Ok(x) => println!("{i}: {ms} ms -> ok {} change(s)", x.len()),
                    Err(e) => println!("{i}: {ms} ms -> err {e}"),
                }
            }
        }
        "query" => query(&args[1..]),
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
    let cmd = format!("\"{exe}\" serve");
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
    single_instance();
    if rest.iter().any(|a| a == "--background") {
        detach_self();
    }
    let rebuild = rest.iter().any(|a| a == "--rebuild");
    let auto_update = rest.iter().any(|a| a == "--auto-update");
    let force_walk = rest.iter().any(|a| a == "--no-usn");
    let cache = index::cache_path();
    let t0 = std::time::Instant::now();
    plog(&format!("everything v{} starting", env!("CARGO_PKG_VERSION")));
    update::auto_check(auto_update);
    let roots = index::scan_roots();
    let loaded = match &cache {
        Some(p) if !rebuild && p.exists() => {
            // An empty cache (e.g. whitelist was empty last run) is not "loaded".
            index::Index::load(p).filter(|i| i.len() > 0)
        }
        _ => None,
    };
    let mut built = match loaded {
        Some(i) => {
            plog(&format!(
                "loaded cache: {} entries, {} ms",
                i.len(),
                t0.elapsed().as_millis()
            ));
            i
        }
        None => {
            plog("building index (first run, or --rebuild)");
            let i = if force_walk {
                plog("--no-usn: directory walk");
                index::build_walk(&roots)
            } else {
                match index::build_usn(&roots) {
                    Some(i) => i,
                    None => {
                        plog("no USN-capable volume (empty whitelist, journal off, or not elevated); falling back to walk");
                        index::build_walk(&roots)
                    }
                }
            };
            if let Some(p) = &cache {
                if i.len() > 0 {
                    i.save(p);
                }
            }
            plog(&format!("build done in {} ms", t0.elapsed().as_millis()));
            i
        }
    };
    // Sidecar watermarks: replay every change that happened while we were down.
    if !built.vol.is_empty() {
        for w in index::load_watermarks() {
            if let Some(v) = built.vol.iter_mut().find(|m| m.drive == w.drive) {
                if w.journal_id == v.journal_id && w.next_usn > v.next_usn {
                    v.next_usn = w.next_usn;
                }
            }
        }
    }

    // Open the journals. A rotated/wrapped journal cannot replay the gap, so
    // enumerate again (seconds, not minutes) instead of shipping a stale index.
    let mut vols = Vec::new();
    let mut stale = false;
    for m in built.vol.clone() {
        if m.journal_id == 0 {
            continue; // walk-built volume (ReFS/exFAT): nothing to tail
        }
        if let Some(mut v) = usn::open(m.drive as char) {
            if v.journal_id != m.journal_id || m.next_usn < v.first_usn || m.next_usn > v.next_usn {
                stale = true;
            } else {
                v.next_usn = m.next_usn;
            }
            vols.push(v);
        }
    }
    if !force_walk && stale {
        plog("journal rotated or wrapped; rebuilding from enumeration");
        if let Some(fresh) = index::build_usn(&roots) {
            if let Some(p) = &cache {
                fresh.save(p);
            }
            built = fresh;
            vols.clear();
            for m in &built.vol {
                if let Some(v) = usn::open(m.drive as char) {
                    vols.push(v);
                }
            }
        }
    }

    let idx: index::Shared = Arc::new(RwLock::new(built));
    plog(&format!("indexed {} entries", idx.read().unwrap().len()));
    let has_walk = idx.read().unwrap().vol.iter().any(|m| m.journal_id == 0);
    if vols.is_empty() {
        plog("watcher: 30s root-signature poll");
        index::spawn_poll(idx.clone());
    } else {
        plog("journal tail active (blocking read, 0 idle CPU)");
        index::spawn_usn(idx.clone(), vols);
    }
    if has_walk {
        plog("note: non-NTFS volume (ReFS/exFAT) has no MFT enumeration; run `everything serve --rebuild` to pick up new files");
    }
    plog("serving on \\\\.\\pipe\\everything233");
    daemon::run(idx)
}

/// Serve diagnostics must survive the SYSTEM task (session 0 has no console).
fn plog(msg: &str) {
    use std::io::Write;
    eprintln!("{msg}");
    let Some(base) = std::env::var_os("ProgramData") else { return };
    let dir = std::path::Path::new(&base).join("everything233");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
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
           everything serve        load/build index, answer pipe queries\n\
           everything serve --rebuild     force a full rebuild (USN enumeration)\n\
           everything serve --no-usn      force the directory-walk builder\n\
           everything install      register daemon: SYSTEM scheduled task when elevated\n\
           everything uninstall    remove task + Run-key entry\n\
           everything update       manual check + fastest-mirror download + elevated install\n\
         \n\
         Index:\n\
           elevated  USN journal enumeration (whole volume in seconds) + blocking\n\
                     tail = sub-second updates, 0 idle CPU\n\
           fallback  no elevation: directory walk + 30s root-signature poll\n\
         \n\
         Options:\n\
           -d           folders only\n\
           -f           files only\n\
           -n <num>     result limit (default 200)\n\
           *            wildcard in query\n\
         \n\
         Index scope: whitelist in %APPDATA%\\everything233\\include.txt\n\
         (one root per line, drive root = whole volume, # = comment)\n\
         Built-in skips: .git .svn .hg .jj .vs .idea .vscode node_modules Library \\\n\
         Temp obj target __pycache__ .gradle .nuget .cache build out; \\\n\
         system trees (Windows/Program Files/ProgramData/AppData/...) and game \\\n\
         platforms at the drive root; extra names in \\\n\
         %APPDATA%\\everything233\\ignore.txt",
        env!("CARGO_PKG_VERSION")
    );
}
