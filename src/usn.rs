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
    READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V1, USN_RECORD_V2,
};

pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

/// Low 48 bits of a file reference is the MFT record number; we key on 32 bits
/// (a volume never holds 4G records).
#[inline]
pub fn mft(frn: u64) -> u32 {
    frn as u32
}

#[derive(Clone)]
pub struct Change {
    pub frn: u32,
    pub parent: u32,
    pub name: String,
    pub attrs: u32,
    pub reason: u32,
}

pub struct Volume {
    pub drive: char,
    pub h: HANDLE,
    evt: HANDLE,
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    buf: Vec<u8>,
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
        buf: vec![0u8; 1 << 20],
    })
}

impl Volume {
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
                    eprintln!(
                        "usn enum err {} (total so far {total})",
                        unsafe { windows_sys::Win32::Foundation::GetLastError() }
                    );
                }
                break;
            }
            let base = self.buf.as_ptr() as usize;
            let end = base + got as usize;
            // Header: the MFT record to resume from.
            let hdr = u64::from_le_bytes([
                self.buf[0], self.buf[1], self.buf[2], self.buf[3], self.buf[4], self.buf[5],
                self.buf[6], self.buf[7],
            ]);
            let mut p = base + 8; // first 8 bytes = resume FRN
            while p + std::mem::size_of::<USN_RECORD_V2>() <= end {
                let r = p as *const USN_RECORD_V2;
                let rec_len = unsafe { (*r).RecordLength } as usize;
                if rec_len == 0 || p + rec_len > end {
                    break;
                }
                let frn = unsafe { (*r).FileReferenceNumber };
                let par = unsafe { (*r).ParentFileReferenceNumber };
                let attrs = unsafe { (*r).FileAttributes };
                let nlen = unsafe { (*r).FileNameLength } as usize / 2;
                let noff = unsafe { (*r).FileNameOffset } as usize;
                if noff >= rec_len || (nlen * 2) + noff > rec_len {
                    p += rec_len;
                    continue;
                }
                let name = unsafe { std::slice::from_raw_parts((p + noff) as *const u16, nlen) };
                cb(mft(frn), mft(par), name, attrs);
                total += 1;
                p += rec_len;
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
            MinMajorVersion: 2,
            MaxMajorVersion: 2, // keep V2 records, same parser as the builder
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
        // First 8 bytes of the output buffer = the USN to continue from (the
        // record-level Usn is inclusive when used as StartUsn, so reusing it
        // would re-read the last record forever).
        let hdr_next = i64::from_le_bytes([
            self.buf[0], self.buf[1], self.buf[2], self.buf[3], self.buf[4], self.buf[5],
            self.buf[6], self.buf[7],
        ]);
        self.next_usn = hdr_next;
        let base = self.buf.as_ptr() as usize;
        let end = base + got as usize;
        let mut p = base + 8;
        let mut out = Vec::new();
        while p + std::mem::size_of::<USN_RECORD_V2>() <= end {
            let r = p as *const USN_RECORD_V2;
            let rec_len = unsafe { (*r).RecordLength } as usize;
            if rec_len == 0 || p + rec_len > end {
                break;
            }
            let reason = unsafe { (*r).Reason };
            let frn = unsafe { (*r).FileReferenceNumber };
            let par = unsafe { (*r).ParentFileReferenceNumber };
            let attrs = unsafe { (*r).FileAttributes };
            let nlen = unsafe { (*r).FileNameLength } as usize / 2;
            let noff = unsafe { (*r).FileNameOffset } as usize;
            if noff < rec_len && (nlen * 2) + noff <= rec_len {
                let name = unsafe { std::slice::from_raw_parts((p + noff) as *const u16, nlen) };
                if !name.is_empty() {
                    out.push(Change {
                        frn: mft(frn),
                        parent: mft(par),
                        name: String::from_utf16_lossy(name),
                        attrs,
                        reason,
                    });
                }
            }
            p += rec_len;
        }
        Ok(out)
    }
}