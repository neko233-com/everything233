// Direct NTFS USN change-journal access (requires elevation: raw volume handle).
//
// build  = FSCTL_ENUM_USN_DATA: enumerate every live MFT entry with name+parent,
//          no directory walking at all -> whole disk in a couple of seconds.
// live   = FSCTL_READ_USN_JOURNAL with BytesToWaitFor+Timeout: the kernel blocks
//          the thread until records exist -> 0 idle CPU and sub-second updates.
//
// No content is read; only names, parent file references and attributes.
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V1,
    READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V1,
};

pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

/// Low 48 bits of a file reference is the MFT record number; we key on 32 bits
/// (a volume never holds 4G records).
#[inline]
pub fn mft(frn: u64) -> u32 {
    frn as u32
}

#[derive(Clone)]
pub struct Change {
    /// Low 64 bits of the record's file id (MFT number on NTFS).
    pub frn: u64,
    /// Low 64 bits of the parent directory's file id.
    pub parent: u64,
    /// Full 128-bit ids (V3 records): the key ReFS directory entries are filed
    /// under, since its numeric ids often live in the high half only.
    pub frn128: u128,
    pub parent128: u128,
    pub name: String,
    pub attrs: u32,
    pub reason: u32,
}

/// One parsed USN record: fixed fields + the byte range of its UTF-16 name.
struct Rec {
    len: usize,
    frn: u64,
    parent: u64,
    /// Raw 128-bit ids (V3 only): ReFS keys objects by the full FILE_ID_128,
    /// where the low half is often 0 and the meaningful id sits in the high half.
    frn128: u128,
    parent128: u128,
    attrs: u32,
    reason: u32,
    name_off: usize,
    /// Always in bytes (FileNameLength is byte-based in V2 (DWORD) and V3 (WORD)).
    name_bytes: usize,
}

/// Version-agnostic record parser.
///
/// NTFS hands back USN_RECORD_V2 (64-bit ids). ReFS cannot produce V2 at all —
/// asking for it yields V3/V4 records with 128-bit FILE_ID_128 ids, and casting
/// that buffer to USN_RECORD_V2 silently produces empty names. Offsets are read
/// byte-wise per version instead of relying on a single struct layout.
fn parse(buf: &[u8], p: usize, end: usize) -> Option<Rec> {
    if p + 8 > end {
        return None;
    }
    let len = u32::from_le_bytes(buf.get(p..p + 4)?.try_into().ok()?) as usize;
    let major = u16::from_le_bytes(buf.get(p + 4..p + 6)?.try_into().ok()?);
    if len < 8 || p + len > end {
        return None;
    }
    let u64_at = |o: usize| -> Option<u64> {
        Some(u64::from_le_bytes(buf.get(p + o..p + o + 8)?.try_into().ok()?))
    };
    let u32_at = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(buf.get(p + o..p + o + 4)?.try_into().ok()?))
    };
    let u16_at = |o: usize| -> Option<u16> {
        Some(u16::from_le_bytes(buf.get(p + o..p + o + 2)?.try_into().ok()?))
    };
    let u128_at = |o: usize| -> Option<u128> {
        Some(u128::from_le_bytes(
            buf.get(p + o..p + o + 16)?.try_into().ok()?,
        ))
    };
    let (frn, parent, frn128, parent128, reason, attrs, name_len, name_off) = match major {
        // USN_RECORD_V2: frn@8 parent@16 usn@24 ts@32 reason@40 src@44 sid@48 attr@52 nlen@56(WORD) noff@58
        2 => (
            u64_at(8)?,
            u64_at(16)?,
            0,
            0,
            u32_at(40)?,
            u32_at(52)?,
            u16_at(56)? as usize,
            u16_at(58)? as usize,
        ),
        // USN_RECORD_V3: FILE_ID_128 frn@8 parent@24 usn@40 ts@48 reason@56 src@60
        // sid@64 attr@68 nlen@72(WORD!) noff@74
        3 => (
            u64_at(8)?,
            u64_at(24)?,
            u128_at(8)?,
            u128_at(24)?,
            u32_at(56)?,
            u32_at(68)?,
            u16_at(72)? as usize,
            u16_at(74)? as usize,
        ),
        _ => return None, // V4 (multi-record batches) is never requested
    };
    if name_off >= len || name_off + name_len > len {
        return None;
    }
    Some(Rec {
        len,
        frn,
        parent,
        frn128,
        parent128,
        attrs,
        reason,
        name_off: p + name_off,
        name_bytes: name_len,
    })
}

pub struct Volume {
    pub drive: char,
    pub h: HANDLE,
    evt: HANDLE,
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    /// USN record major version this volume emits.
    pub ver: u16,
    /// True when this volume cannot do a blocking journal wait (ReFS ignores
    /// READ_USN_JOURNAL_DATA_V1.Timeout, blocks for minutes and then fails with
    /// ERROR_INVALID_USER_BUFFER(1784)), so the tail polls instead.
    pub poll: bool,
    buf: Vec<u8>,
}

/// USN records sit 8-byte aligned inside `buf` and names start on even offsets,
/// so the u16 view is aligned in practice; going through a copy would cost an
/// allocation per record on a 6M-entry enumeration.
#[inline]
fn name16(buf: &[u8], off: usize, chars: usize) -> &[u16] {
    unsafe { std::slice::from_raw_parts(buf.as_ptr().add(off) as *const u16, chars) }
}

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.evt);
            CloseHandle(self.h);
        }
    }
}

// The handle is owned by this struct and touched only by its own thread.
unsafe impl Send for Volume {}

fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Filesystem name of a volume ("NTFS", "ReFS", "FAT32"...). MFT enumeration
/// (FSCTL_ENUM_USN_DATA) is NTFS-only; ReFS has a journal but no MFT walk.
pub fn fs(drive: char) -> String {
    let root = format!("{drive}:\\");
    let w = wide(&root);
    let mut name: [u16; 32] = [0; 32];
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return String::new();
    }
    let p: &[u16] = &name;
    let n = p.iter().position(|&c| c == 0).unwrap_or(p.len());
    String::from_utf16_lossy(&p[..n])
}

/// True admin capability probe that does not depend on volume-open quirks:
/// raw physical-drive access is administrator-only.
pub fn can_open_physical() -> bool {
    let w = wide("\\\\.\\PHYSICALDRIVE0");
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    unsafe { CloseHandle(h) };
    true
}

/// Open a volume for USN work. Fails without elevation (raw volume handle).
pub fn open(drive: char) -> Option<Volume> {
    let path = format!("\\\\.\\{}:", drive);
    let w = wide(&path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        eprintln!(
            "usn: open {path} failed err {}",
            unsafe { windows_sys::Win32::Foundation::GetLastError() }
        );
        return None;
    }
    let evt = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
    if evt.is_null() {
        unsafe { CloseHandle(h) };
        return None;
    }
    let mut jd: USN_JOURNAL_DATA_V1 = unsafe { std::mem::zeroed() };
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_QUERY_USN_JOURNAL,
            ptr::null(),
            0,
            &mut jd as *mut _ as *mut _,
            std::mem::size_of::<USN_JOURNAL_DATA_V1>() as u32,
            &mut got,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        eprintln!(
            "usn: query journal {} failed err {}",
            drive,
            unsafe { windows_sys::Win32::Foundation::GetLastError() }
        );
        unsafe {
            CloseHandle(evt);
            CloseHandle(h);
        }
        return None;
    }
    Some(Volume {
        drive,
        h,
        evt,
        journal_id: jd.UsnJournalID,
        first_usn: jd.FirstUsn,
        next_usn: jd.NextUsn,
        // NTFS journals default to V2 (compact 64-bit ids). ReFS answers V2 too
        // but its ids only exist as 128-bit FILE_ID_128, so ask for V3 there and
        // join on the raw 128-bit value the directory enumeration reports.
        ver: if fs(drive) == "NTFS" { 2 } else { 3 },
        poll: fs(drive) != "NTFS",
        buf: vec![0u8; 1 << 20],
    })
}

impl Volume {
    /// Diagnostic non-blocking read used to pin down what a filesystem accepts
    /// (version range / buffer size). Returns the byte count and records.
    pub fn read_probe(
        &mut self,
        min: u16,
        max: u16,
        bufsz: usize,
        wait: u64,
        timeout: u64,
    ) -> Result<(u32, usize), i32> {
        if self.buf.len() < bufsz {
            self.buf = vec![0u8; bufsz];
        }
        let rd = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: self.next_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: timeout,
            BytesToWaitFor: wait,
            UsnJournalID: self.journal_id,
            MinMajorVersion: min,
            MaxMajorVersion: max,
        };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_READ_USN_JOURNAL,
                &rd as *const _ as *const _,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                self.buf.as_mut_ptr() as *mut _,
                bufsz as u32,
                &mut got,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(unsafe { GetLastError() } as i32);
        }
        let mut n = 0usize;
        let mut p = 8usize;
        while let Some(r) = parse(&self.buf, p, got as usize) {
            n += 1;
            p += r.len;
        }
        if n == 0 && got > 8 {
            // Raw dump: tells us the real record version/layout the FS returned.
            let head: Vec<String> = self.buf[..(got as usize).min(80)]
                .chunks(4)
                .map(|c| format!("{:08x}", u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
                .collect();
            eprintln!("raw: {}", head.join(" "));
        }
        Ok((got, n))
    }

    /// Enumerate all live MFT entries (name, parent, attributes). Returns count.
    pub fn enumerate<F: FnMut(u32, u32, &[u16], u32)>(&mut self, mut cb: F) -> usize {
        let mut med = MFT_ENUM_DATA_V1 {
            StartFileReferenceNumber: 0,
            LowUsn: 0, // 0 = every live MFT record; FirstUsn would drop records whose USN predates the journal window
            HighUsn: self.next_usn,
            MinMajorVersion: 2,
            MaxMajorVersion: 2, // force USN_RECORD_V2 (64-bit ids); V3 breaks the parse
        };
        let mut total = 0usize;
        loop {
            let mut got = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.h,
                    FSCTL_ENUM_USN_DATA,
                    &med as *const _ as *const _,
                    std::mem::size_of::<MFT_ENUM_DATA_V1>() as u32,
                    self.buf.as_mut_ptr() as *mut _,
                    self.buf.len() as u32,
                    &mut got,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || got <= 8 {
                if ok == 0 {
                    let e = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                    if e != 38 {
                        // 38 = ERROR_HANDLE_EOF: the enumeration simply finished.
                        eprintln!("usn enum err {e} (total so far {total})");
                    }
                }
                break;
            }
            let end = got as usize;
            // Header: the MFT record to resume from.
            let hdr = u64::from_le_bytes([
                self.buf[0], self.buf[1], self.buf[2], self.buf[3], self.buf[4], self.buf[5],
                self.buf[6], self.buf[7],
            ]);
            let mut p = 8usize; // first 8 bytes = resume FRN
            while let Some(r) = parse(&self.buf, p, end) {
                let name = name16(&self.buf, r.name_off, r.name_bytes / 2);
                cb(mft(r.frn), mft(r.parent), name, r.attrs);
                total += 1;
                p += r.len;
            }
            if hdr == med.StartFileReferenceNumber {
                break; // no forward progress
            }
            med.StartFileReferenceNumber = hdr;
        }
        total
    }

    /// Wait up to `timeout_sec` for new journal records.
    ///
    /// The kernel Timeout field does not fire reliably on a synchronous call,
    /// so the read goes out overlapped and we own the wait: event signaled ->
    /// records, timeout -> cancel, 0 CPU while idle.
    /// Err(errno): the caller rebuilds when the journal wrapped.
    pub fn read_batch(&mut self, timeout_sec: u64) -> Result<Vec<Change>, i32> {
        let rd = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: self.next_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: timeout_sec, // kernel wakes us when records exist, or after this
            BytesToWaitFor: 1,    // >0 = block instead of returning an empty buffer
            UsnJournalID: self.journal_id,
            MinMajorVersion: self.ver,
            // ReFS cannot emit USN_RECORD_V2 (its ids are 128-bit), NTFS keeps V2
            // unless asked otherwise; `ver` is pinned per volume at open time.
            MaxMajorVersion: self.ver,
        };
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.evt;
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_READ_USN_JOURNAL,
                &rd as *const _ as *const _,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                self.buf.as_mut_ptr() as *mut _,
                self.buf.len() as u32,
                &mut got,
                &mut ov as *mut _,
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING {
                return Err(e as i32);
            }
            let w = unsafe { WaitForSingleObject(self.evt, (timeout_sec.max(1) * 1000 + 500) as u32) };
            if w == WAIT_TIMEOUT {
                unsafe {
                    CancelIoEx(self.h, &ov as *const _);
                    let mut z = 0u32;
                    GetOverlappedResult(self.h, &ov as *const _, &mut z, 1);
                }
                return Ok(Vec::new());
            }
            if w != WAIT_OBJECT_0 {
                return Err(-1);
            }
            let mut z = 0u32;
            if unsafe { GetOverlappedResult(self.h, &ov as *const _, &mut z, 0) } == 0 {
                let e = unsafe { GetLastError() };
                if e == ERROR_OPERATION_ABORTED {
                    return Ok(Vec::new());
                }
                return Err(e as i32);
            }
            got = z;
        }
        if std::env::var_os("E233_DBG").is_some() {
            eprintln!("dbg read ok got={got} start={}", rd.StartUsn);
        }
        if got <= 8 {
            self.next_usn = rd.StartUsn;
            return Ok(Vec::new());
        }
        Ok(self.decode(got))
    }

    /// Read records from an explicit USN and hex-dump them (diagnosis only).
    pub fn dump_from(&mut self, start: i64, ver: u16) -> String {
        let rd = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: start,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: self.journal_id,
            MinMajorVersion: ver,
            MaxMajorVersion: ver,
        };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_READ_USN_JOURNAL,
                &rd as *const _ as *const _,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                self.buf.as_mut_ptr() as *mut _,
                self.buf.len() as u32,
                &mut got,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return format!("err {}", unsafe { GetLastError() });
        }
        let mut s = format!("start {start} got {got}\n");
        let n = (got as usize).min(200);
        for row in (0..n).step_by(16) {
            s.push_str(&format!("{row:04x} "));
            for b in &self.buf[row..(row + 16).min(n)] {
                s.push_str(&format!("{b:02x} "));
            }
            s.push('\n');
        }
        let mut p = 8usize;
        let mut count = 0;
        while let Some(r) = parse(&self.buf, p, got as usize) {
            s.push_str(&format!(
                "rec {} len {} frn {:x} par {:x} rs {:x} namebytes {} off {}\n",
                count, r.len, r.frn, r.parent, r.reason, r.name_bytes, r.name_off - p
            ));
            count += 1;
            p += r.len;
            if count > 5 {
                break;
            }
        }
        s
    }

    /// Non-blocking journal read, used on volumes where the kernel's blocking
    /// wait is broken (ReFS). One syscall per tick, ~1 µs, so a 1 s poll costs
    /// nothing measurable and gives sub-second freshness.
    pub fn read_poll(&mut self) -> Result<Vec<Change>, i32> {
        let rd = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: self.next_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: self.journal_id,
            MinMajorVersion: self.ver,
            MaxMajorVersion: self.ver,
        };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_READ_USN_JOURNAL,
                &rd as *const _ as *const _,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                self.buf.as_mut_ptr() as *mut _,
                self.buf.len() as u32,
                &mut got,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(unsafe { GetLastError() } as i32);
        }
        if got <= 8 {
            self.next_usn = rd.StartUsn;
            return Ok(Vec::new());
        }
        Ok(self.decode(got))
    }

    /// Turn a filled buffer into records and advance the watermark.
    ///
    /// The first 8 bytes of the output are the resume USN (the record-level Usn
    /// is inclusive when used as StartUsn, so reusing it re-reads forever).
    fn decode(&mut self, got: u32) -> Vec<Change> {
        self.next_usn = i64::from_le_bytes([
            self.buf[0], self.buf[1], self.buf[2], self.buf[3], self.buf[4], self.buf[5],
            self.buf[6], self.buf[7],
        ]);
        let end = got as usize;
        let mut p = 8usize;
        let mut out = Vec::new();
        while let Some(r) = parse(&self.buf, p, end) {
            if r.name_bytes > 0 {
                out.push(Change {
                    frn: r.frn,
                    parent: r.parent,
                    frn128: r.frn128,
                    parent128: r.parent128,
                    name: String::from_utf16_lossy(name16(&self.buf, r.name_off, r.name_bytes / 2)),
                    attrs: r.attrs,
                    reason: r.reason,
                });
            }
            p += r.len;
        }
        out
    }
}