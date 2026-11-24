# everything233

Everything, but faster on Windows. Single static `everything.exe`, filename search only — no content scanning.

**v0.7**: both NTFS and ReFS are indexed from the filesystem's own metadata and both stay live.
NTFS enumerates MFT records out of the USN journal and tails it with a blocking read. ReFS has no
MFT, so it is enumerated with bulk directory calls (one syscall per directory, returning name +
attributes + 128-bit file id) and patched from journal **V3** records keyed by that same 128-bit
id, polled once per second. Same experience on both: sub-2 s updates, 0% idle CPU.

## Measured (this machine)

| Volume | Filesystem | Indexed records | Build | Query (200 hits) | Idle CPU | Live update |
|---|---|---|---|---|---|---|
| `F:` 500 GB | NTFS | 190,616 | **355 ms** | 11.5 ms | **0.00 s / 8 s** | create/rename/delete <150 ms |
| `C:` 1.36 TB | NTFS | 6,495,917 | **7.2 s** (cold) | 11–21 ms | 0.00 s / 10 s | same path, sub-second |
| `D:` 3.8 TB | ReFS | 5,458,634 | **22.0 s** (cold), 28.6 s under SYSTEM | 12–21 ms | **0.00 s / 10 s** | create/rename/delete/deep-path ≤2 s |

RSS: 13 MB for the 190 k NTFS volume, 140 MB for the 5.4 M-entry ReFS volume. ReFS cache load is
64 ms; a restart replays the journal from the watermark instead of re-walking.

## Why ReFS needed its own route

`FSCTL_ENUM_USN_DATA` (MFT enumeration) is **NTFS-only** — on ReFS it returns
`ERROR_INVALID_FUNCTION`. ReFS does have a journal (`QUERY_USN_JOURNAL` and `READ_USN_JOURNAL`
both work, `Maximum record version 4`), so live updates are possible, but three things had to be
measured before it worked:

1. **Blocking reads are broken on ReFS.** `BytesToWaitFor > 0` with a `Timeout` is ignored: the
   call blocks 30–101 s and then fails with `ERROR_INVALID_USER_BUFFER` (1784). ReFS is polled
   with `BytesToWaitFor = 0` once a second — one syscall, 0% CPU, ≤1.2 s latency.
2. **V2 records cannot join.** V2 packs a 64-bit id; ReFS ids are 128-bit and the packed value
   does not match what directory enumeration reports. We request **V3** records and use the raw
   16 bytes as the key — enumeration and journal agree byte for byte.
3. **Per-directory `FileIdInfo` is a trap.** Opening every directory to ask for its id cost 600 k+
   syscalls and took 152 s. `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` returns the
   whole directory (names + ids) in one call, and child ids are inherited from the parent listing:
   build time dropped to 22 s.

## How it works

- **Build, NTFS**: `FSCTL_QUERY_USN_JOURNAL` on the raw volume handle, then `FSCTL_ENUM_USN_DATA`
  returns every live MFT record with name + parent file reference; one level-order pass over the
  parent/child adjacency table applies the whitelist prefixes. No `FindFirstFile` storm.
- **Build, ReFS**: root id from `FileIdInfo`, then `FileIdExtdDirectoryInfo` per directory
  (ids inherited, never reopened). Records keep a `dirs` map of 128-bit directory id → record so
  journal events can resolve parents, including directories created after the build.
- **Live, NTFS**: `FSCTL_READ_USN_JOURNAL` **overlapped**, waiting on the event → sub-second
  latency, zero polling. (The resume USN must be the 8-byte header of the output buffer, not the
  last record's `Usn`, or you re-read the same record forever.)
- **Live, ReFS**: same ioctl with `BytesToWaitFor = 0`, one poll per second per volume. One parser
  dispatches on `MajorVersion` (V2 / V3) — both carry `FileNameLength` as a **WORD**.
- **Records are 8 bytes**: `parent: u32` (bit31 dir / bit30 tombstone / low 30 parent id) +
  `name_id: u32`. Sizes and timestamps are not stored — one `GetFileAttributesExW` per returned
  hit (≤ limit calls) saves 12 B/entry and doubles as a liveness check on ReFS.
- **Name-chain inverted index**: `first`/`next` link records that share a name, so a query scans
  the ~650 k unique-name table instead of every record.
- **Rename pairing**: a batch `RENAME_OLD_NAME` + `RENAME_NEW_NAME` updates the record in place and
  relinks it into the new name chain — works for files, which have no id map.
- **Locking**: `Arc<RwLock<Index>>`. Readers run fully concurrent; the write lock is held only for
  the microseconds it takes to apply one journal batch.
- **Cache v6** at `%LOCALAPPDATA%\everything233\index.dat` (records + name chains + `frn` +
  `dirs` + walk flags); the `usn.meta` sidecar carries journal watermarks so a restart replays
  offline changes instead of rewriting the index. Rotated journal ⇒ re-enumerate (7 s NTFS, 22 s ReFS).

## Install

MSI runs `everything.exe install` as SYSTEM during install, which registers a scheduled task
(`everything233`, SYSTEM, highest privileges, at start). The daemon therefore runs in session 0
with a pipe ACL (`SY…BA…WD`) so your normal user shells can query it.

```powershell
msiexec /i everything233.msi /qn        # installs + registers the SYSTEM task + starts it
everything ".rs" -n 20                 # query from any shell
```

Not elevated? `everything install` falls back to an HKCU Run entry and the directory-walk builder
(correct, just slower). Run `everything install` once from an elevated shell to upgrade.

## Usage

```
everything <query> [-d] [-f] [-n 200]   search
everything serve [--rebuild] [--no-usn] [--auto-update]
everything install / uninstall
everything update
everything probe                       per-volume capability + enumeration timing
everything build                       build the index and print records/time/journal flags
everything readtest C|F|D              dump live journal batches (NTFS blocking vs ReFS poll)
everything idprobe <path...>           print both 64-bit halves of a file id
```

## Index scope

Whitelist in `%APPDATA%\everything233\include.txt` (one root per line, `D:\` = whole volume,
`#` = comment). Extra skip names in `%APPDATA%\everything233\ignore.txt`. The SYSTEM daemon reads
the machine-wide `C:\ProgramData\everything233\include.txt` — set both if you change it.

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

v0.7.0 — NTFS (USN V2 enumeration + blocking tail) and ReFS (bulk directory enumeration + journal
V3 1 s poll) both fully live; 8 B records, name-chain index, `frn`/`dirs` resolvers, rename
pairing, cache v6 + watermarks, SYSTEM task install, MSI auto-registration.
