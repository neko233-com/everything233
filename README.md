# everything233

A tiny, drop-in replacement for voidtools Everything on Windows.
Single static `everything.exe`, filename search only — no content text scanning, no index bloat.

## Why

Stock Everything indexes everything (incl. `.git`, Unity `Library`, `node_modules`, build output),
spikes CPU on reindex and balloons memory. This one ships with a built-in skip list and an
event-driven watcher (idle CPU ~0 after the initial build).

## Features

- `everything <query>` — instant lookup via the resident daemon
- `everything serve` — load cached index, watch changes, answer pipe queries; `--rebuild` forces a full rescan
- Index cache at `%LOCALAPPDATA%\everything233\index.dat` — second start is instant
- `everything install` / `uninstall` — HKCU Run-key logon auto-start (no admin)
- Wildcard `*`, `-d` folders only, `-f` files only, `-n <num>` result limit
- Built-in skips: `.git .svn .hg .jj .vs .idea node_modules Library Temp obj target __pycache__ .gradle .nuget .cache`
- Extra skip names: `%APPDATA%\everything233\ignore.txt` (one per line, `#` comment)
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

v0.1: directory-walk index + ReadDirectoryChangesW watcher.
Roadmap: USN journal / MFT direct read (needs elevation) for instant startup and lower RSS.
