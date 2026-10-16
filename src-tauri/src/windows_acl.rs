//! Protect the machine-wide print credentials before they can be persisted.
//! A failed legacy migration never changes the existing config or ledger bytes.

use std::{
    ffi::c_void,
    io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        GetSecurityDescriptorDacl, LookupAccountNameW, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SID_NAME_USE,
    },
    System::RemoteDesktop::{
        WTSActive, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSQuerySessionInformationW, WTSUserName, WTS_CURRENT_SERVER_HANDLE,
    },
};

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn win_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn session_field(session: u32, field: i32) -> io::Result<String> {
    let mut buffer = std::ptr::null_mut();
    let mut bytes = 0;
    // SAFETY: WTS allocates `buffer`; it is freed below on every success path.
    if unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session,
            field,
            &mut buffer,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let value = if buffer.is_null() || bytes == 0 {
        String::new()
    } else {
        // SAFETY: WTS reports the UTF-16 allocation length in bytes.
        let units = unsafe { std::slice::from_raw_parts(buffer, bytes as usize / 2) };
        String::from_utf16_lossy(units.split(|unit| *unit == 0).next().unwrap_or(&[]))
    };
    // SAFETY: this pointer came from WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(buffer.cast()) };
    Ok(value)
}

fn account_sid(account: &str) -> io::Result<String> {
    let name = wide(std::ffi::OsStr::new(account));
    let mut sid_bytes = 0;
    let mut domain_units = 0;
    let mut use_kind: SID_NAME_USE = 0;
    // SAFETY: the first call asks Windows for required output buffer sizes.
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_bytes,
            std::ptr::null_mut(),
            &mut domain_units,
            &mut use_kind,
        );
    }
    // An unknown account is not a usable identity; never guess a different SID.
    if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || sid_bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut sid = vec![0u8; sid_bytes as usize];
    let mut domain = vec![0u16; domain_units as usize];
    // SAFETY: both buffers have the sizes supplied by LookupAccountNameW.
    if unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_bytes,
            domain.as_mut_ptr(),
            &mut domain_units,
            &mut use_kind,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut sid_text = std::ptr::null_mut();
    // SAFETY: LookupAccountNameW filled a valid binary SID.
    if unsafe { ConvertSidToStringSidW(sid.as_mut_ptr().cast(), &mut sid_text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated allocation.
    let len = unsafe { (0..184).find(|n| *sid_text.add(*n) == 0) };
    let value = len
        .map(|len| String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_text, len) }));
    // SAFETY: this pointer came from ConvertSidToStringSidW.
    unsafe { LocalFree(sid_text.cast()) };
    value.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SID text is too long"))
}

/// Resolve the uniquely active console or RDP user, not the elevated process.
pub fn interactive_user_sid() -> io::Result<String> {
    let mut sessions = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: WTS allocates the session array and reports its element count.
    if unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut sessions, &mut count) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let lookup = (|| -> io::Result<Vec<String>> {
        let mut identities = Vec::new();
        // SAFETY: the array has exactly `count` elements until WTSFreeMemory.
        let active = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
        for session in active.iter().filter(|session| session.State == WTSActive) {
            let name = session_field(session.SessionId, WTSUserName)?;
            if name.is_empty() {
                continue;
            }
            let domain = session_field(session.SessionId, WTSDomainName)?;
            let account = if domain.is_empty() {
                name
            } else {
                format!("{domain}\\{name}")
            };
            identities.push(account_sid(&account)?);
        }
        Ok(identities)
    })();
    // SAFETY: this pointer came from WTSEnumerateSessionsW.
    unsafe { WTSFreeMemory(sessions.cast()) };
    let mut identities = lookup?;
    identities.sort();
    identities.dedup();
    match identities.as_slice() {
        [sid] => Ok(sid.clone()),
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "exactly one active interactive Windows account is required",
        )),
    }
}

fn set_private_dacl(path: &Path, user_sid: &str, directory: bool) -> io::Result<()> {
    if !user_sid.starts_with("S-1-")
        || !user_sid
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'S' || b == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid user SID",
        ));
    }
    let flags = if directory { "OICI" } else { "" };
    let sddl = format!("D:P(A;{flags};FA;;;{user_sid})(A;{flags};FA;;;SY)(A;{flags};FA;;;BA)");
    let descriptor_text = wide(std::ffi::OsStr::new(&sddl));
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: Windows parses the NUL-terminated SDDL into an owned descriptor.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    // SAFETY: `descriptor` stays alive through SetNamedSecurityInfoW.
    let got_dacl =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    let result = if got_dacl == 0 || present == 0 || dacl.is_null() {
        Err(io::Error::last_os_error())
    } else {
        let path_wide = wide(path.as_os_str());
        // SAFETY: all pointers remain valid for the synchronous call.
        let code = unsafe {
            SetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if code == 0 {
            Ok(())
        } else {
            Err(win_error(code))
        }
    };
    // SAFETY: this pointer came from ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(descriptor as *mut c_void) };
    result
}

const SENSITIVE_FILES: [&str; 4] = [
    "config.json",
    "config.json.tmp",
    "local-print-intents.jsonl",
    "local-print-intents.lock",
];

fn exists_or_error(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_attributes() & 0x400 != 0 => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "credential path is a Windows reparse point",
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Preflight *all* legacy files before modifying any ACL or allowing writes.
pub fn protect_data_dir_for_user(dir: &Path, user_sid: &str) -> io::Result<()> {
    if exists_or_error(dir)? {
        std::fs::read_dir(dir)?;
    }
    for name in SENSITIVE_FILES {
        let path = dir.join(name);
        if exists_or_error(&path)? {
            std::fs::File::open(path)?;
        }
    }
    std::fs::create_dir_all(dir)?;
    // Files first: an existing readable config remains accessible even if
    // hardening the installer-owned parent fails without WRITE_DAC.
    for name in SENSITIVE_FILES {
        let path = dir.join(name);
        if exists_or_error(&path)? {
            set_private_dacl(&path, user_sid, false)?;
        }
    }
    set_private_dacl(dir, user_sid, true)
}

pub fn protect_file_for_user(path: &Path, user_sid: &str) -> io::Result<()> {
    set_private_dacl(path, user_sid, false)
}
