# everything233

Everything, but faster on Windows. Single static `everything.exe`, filename search only — no content scanning.

**v0.6**: elevated USN-journal indexing. NTFS volumes are enumerated straight from the MFT (no
directory walking at all), then kept live by a blocking journal tail — sub-second updates at
literally 0% idle CPU.

## Measured (this machine)

| Volume | Filesystem | Indexed records | Build | Query (200 hits) | Idle CPU | Live update |
|---|---|---|---|---|---|---|
| `F:` 500 GB | NTFS | 190,631 | **345 ms** | 11.5 ms | **0.00 s / 15 s** | create/rename/delete visible in <150 ms |
| `D:` 3.8 TB | ReFS | 5,463,059 | 24.0 s (walk) | 12–21 ms | 0.00 s / 15 s | rebuild only (see below) |

RSS: 13.5 MB for the NTFS index, 122 MB for the 5.4 M-entry ReFS volume. Old v0.5 took 122 ms
for the same 200-hit query on that index — the name-chain layout searched 650 k unique names
instead of 4 M records.

## Why ReFS is different

`FSCTL_ENUM_USN_DATA` (MFT enumeration) is **NTFS-only**. On ReFS the call returns
`ERROR_INVALID_FUNCTION`; ReFS has a journal but no MFT walk. So ReFS/exFAT volumes get a
one-time directory walk and cannot be patched per-record. Rebuild them with:

```powershell
everything serve --rebuild
```

This is a filesystem limitation, not a shortcut. Everything has the same wall.

## How it works

- **Build**: `FSCTL_QUERY_USN_JOURNAL` on the raw volume handle, then `FSCTL_ENUM_USN_DATA`
  returns every live MFT record with name + parent file reference; we build a parent/child
  adjacency table and do one level-order pass that applies the whitelist prefixes. No
  `FindFirstFile` storm, no per-directory syscalls.
- **Live**: one thread per volume issues `FSCTL_READ_USN_JOURNAL` **overlapped** and waits on
  the event with a 15 s timeout, cancelling on timeout. The kernel wakes us the moment a record
  exists → sub-second latency, no polling, no `ReadDirectoryChangesW` handles. (The kernel's
  `Timeout` field alone does not fire reliably; and the resume USN must be the 8-byte header of
  the output buffer, not the last record's `Usn`, or you re-read the same record forever.)
- **Records are 8 bytes**: `parent: u32` (bit31 dir / bit30 tombstone / low 30 parent id) +
  `name_id: u32`. Sizes and timestamps are not stored — one `GetFileAttributesExW` per returned
  hit (≤ limit calls) saves 12 B/entry.
- **Name-chain inverted index**: `first`/`next` link records that share a name, so a query
  scans the ~650 k unique-name table instead of every record.
- **`frn` table**: MFT number → record, per-volume offset, so a journal record patches the
  index in O(1).
- **Locking**: `Arc<RwLock<Index>>`. Readers run fully concurrent; the write lock is held only
  for the microseconds it takes to apply one journal batch. No allocation on the query path
  beyond the result strings.
- **Cache v5** at `%LOCALAPPDATA%\everything233\index.dat`; a 17-byte-per-volume
  `usn.meta` sidecar carries the journal watermark so a restart replays offline changes
  instead of rewriting 100 MB of index. Rotated/wrapped journal ⇒ re-enumerate (seconds).

## Install

MSI runs `everything.exe install` as SYSTEM during install, which registers a scheduled task
(`everything233`, SYSTEM, highest privileges, at start). The daemon therefore runs in session 0
with a pipe ACL (`SY…BA…WD`) so your normal user shells can query it.

```powershell
msiexec /i everything233.msi /qn        # installs + registers the SYSTEM task + starts it
everything ".rs" -n 20                 # query from any shell
```

Not elevated? `everything install` falls back to an HKCU Run entry and the directory-walk
builder (correct, just slower). Run `everything install` once from an elevated shell to upgrade
to the SYSTEM task.

## Usage

```
everything <query> [-d] [-f] [-n 200]   search
everything serve [--rebuild] [--no-usn] [--auto-update]
everything install / uninstall
everything update
everything probe                       per-volume capability + enumeration timing
```

## Index scope

Whitelist in `%APPDATA%\everything233\include.txt` (one root per line, `D:\` = whole volume,
`#` = comment). Extra skip names in `%APPDATA%\everything233\ignore.txt`.

USN only hands out names and parent IDs, so filtering is by **name** at any depth:
`.git .svn .hg .jj .vs .idea .vscode node_modules site-packages .venv .next Pods .dart_tool
Library Temp obj target __pycache__ .gradle .nuget .cache build out` plus system trees
(`Windows`, `Program Files[ (x86)]`, `ProgramData`, `AppData`, `$Recycle.Bin`,
`System Volume Information`, `WinSxS`, `Recovery`, …) and game-platform / cache / backup
top-levels (`Steam`, `steamapps`, `WeGame`, `Cache`, `Temp`, `Backups`, …).

## Build

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release
```

MSI (WiX v7): `wix build packaging\Product.wxs -o dist\everything233.msi -arch x64`

## Status

v0.6.0 — USN enumeration + blocking tail, 8 B records, name-chain index, SYSTEM task install,
MSI auto-registration, cache v5 + watermarks. Roadmap: MFT-direct enumeration for ReFS via
`GetFileInformationByHandle` on directories, so ReFS can be patched live too.
