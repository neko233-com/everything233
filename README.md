# everything233

A tiny, drop-in replacement for voidtools Everything on Windows.
Single static `everything.exe`, filename search only — no content text scanning, no index bloat.

## Why

Stock Everything indexes everything (incl. `.git`, Unity `Library`, `node_modules`, build output),
spikes CPU on reindex and balloons memory. This one uses a whitelist (only the roots you name),
a built-in skip list, and a 30s light root-signature refresh (idle CPU ~0 after the initial build).

## Features

- `everything <query>` — instant lookup via the resident daemon
- `everything serve` — load cached index, answer pipe queries; 30s light root-signature refresh; `--rebuild` forces a full rescan
- Index cache at `%LOCALAPPDATA%\everything233\index.dat` — second start is instant
- `everything install` / `uninstall` — HKCU Run-key logon auto-start (no admin)
- `everything update` — manual check; probes GitHub mirrors by latency, picks the fastest route, downloads MSI, elevates and installs
- `everything serve --auto-update` — background check; stages the MSI and applies automatically (UAC)
- Wildcard `*`, `-d` folders only, `-f` files only, `-n <num>` result limit
- Built-in skips (dir name): `.git .svn .hg .jj .vs .idea .vscode node_modules site-packages .venv venv .next Pods .dart_tool .mypy_cache .pytest_cache Library Temp obj Logs Saved CrashDumps target .gradle .nuget .cache __pycache__ build out`
- System/package-cache prefixes never indexed: `Windows`, `Program Files[ (x86)]`, `ProgramData`, `AppData`, `go\pkg\mod`, `.cargo\registry`
- Extra skip names: `%APPDATA%\everything233\ignore.txt` (one per line, `#` comment)
- Whitelist roots: `%APPDATA%\everything233\include.txt` (one root per line, `#` comment); missing = C: office folders only. System/temp/game/cache trees never indexed
- Compact arena (20B/entry, mtime unix seconds); filenames deduplicated into a shared blob; no content scan
- Static CRT, zero runtime DLLs

## Install (one command)

```powershell
irm https://raw.githubusercontent.com/neko233-com/everything233/main/install.ps1 | iex
```

Or grab the MSI from Releases, then:

```powershell
everything serve        # first build takes a while; queries hit the pipe after
everything install      # optional: auto-start at logon
```

## Usage

```
everything <query> [-d] [-f] [-n 200]
```

## Build (Windows, MSVC static)

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release
```

> Note: Windows has no musl; `crt-static` is the Windows equivalent (single exe, no VC++ redist).

## Status

v0.4: whitelist-scoped index, filename dedup, 20B Rec, 30s root-signature refresh (no resident change handle).
Roadmap: USN journal / MFT direct read (needs elevation) for instant startup and lower RSS; bucket/inverted index for large roots.
