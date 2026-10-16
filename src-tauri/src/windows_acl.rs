//! Protect the machine-wide print credentials before they can be persisted.
//! A failed legacy migration never changes the existing config or ledger bytes.

use std::{
    fmt, io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER},
    Security::{
        AclSizeInformation,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        GetAce, GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        IsValidSid, LookupAccountNameW, ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        SID_NAME_USE,
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

#[derive(Debug)]
pub struct ProtectedUserMismatch;

impl fmt::Display for ProtectedUserMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("protected credential files belong to another Windows user")
    }
}

impl std::error::Error for ProtectedUserMismatch {}

fn sid_text(sid: *mut std::ffi::c_void) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    // SAFETY: callers pass a valid Windows SID; Windows owns the output until LocalFree.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returns NUL-terminated UTF-16.
    let len = unsafe { (0..184).find(|n| *text.add(*n) == 0) };
    let value =
        len.map(|len| String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) }));
    // SAFETY: the allocation came from ConvertSidToStringSidW.
    unsafe { LocalFree(text.cast()) };
    value.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SID text is too long"))
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
    // SAFETY: LookupAccountNameW filled a valid binary SID.
    sid_text(sid.as_mut_ptr().cast())
}

fn unique_active_sid(mut identities: Vec<String>) -> io::Result<String> {
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
        let active = if count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(sessions, count as usize) }
        };
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
    if !sessions.is_null() {
        unsafe { WTSFreeMemory(sessions.cast()) };
    }
    unique_active_sid(lookup?)
}

#[cfg(test)]
mod selection_tests {
    use super::unique_active_sid;

    #[test]
    fn only_one_active_cashier_is_selected_even_if_the_process_is_elevated() {
        let cashier = "S-1-5-21-1-2-3-1001".to_string();
        let elevated_process = "S-1-5-21-1-2-3-500".to_string();
        assert_ne!(cashier, elevated_process);
        assert_eq!(
            unique_active_sid(vec![cashier.clone(), cashier.clone()]).unwrap(),
            cashier
        );
        assert!(unique_active_sid(vec![]).is_err());
        assert!(unique_active_sid(vec![cashier, elevated_process]).is_err());
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
    // OWNER RIGHTS suppresses an old owner's implicit WRITE_DAC. Grant only
    // READ_CONTROL there; the cashier, SYSTEM, and Administrators retain FA.
    let sddl = format!(
        "D:P(A;{flags};FA;;;{user_sid})(A;{flags};FA;;;SY)(A;{flags};FA;;;BA)(A;{flags};RC;;;OW)"
    );
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
    unsafe { LocalFree(descriptor) };
    result
}

const SENSITIVE_FILES: [&str; 7] = [
    "config.json",
    "config.json.tmp",
    "local-print-intents.jsonl",
    "local-print-intents.lock",
    "print-jobs.jsonl",
    "print-jobs.jsonl.tmp",
    "print-jobs.lock",
];

fn ambiguous_acl() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "bridge-managed credential ACL is ambiguous",
    )
}

/// An OWNER RIGHTS ACE marks the DACL written by this bridge. Legacy ACLs
/// without that marker may migrate; a marked ACL has exactly one cashier SID.
fn managed_user_sid(path: &Path) -> io::Result<Option<String>> {
    let path_wide = wide(path.as_os_str());
    let mut descriptor = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    // SAFETY: Windows allocates `descriptor`, which owns `dacl` until LocalFree.
    let code = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(win_error(code));
    }
    let result = (|| {
        if descriptor.is_null() {
            return Err(ambiguous_acl());
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: `descriptor` remains allocated through this inspection.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Ok(None);
        }
        if dacl.is_null() {
            return Err(ambiguous_acl());
        }
        let mut info = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: `dacl` points inside the returned descriptor; `info` has
        // the exact Windows ACL_SIZE_INFORMATION layout and size.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut cashier = None;
        let mut system_count = 0;
        let mut admin_count = 0;
        let mut owner_rights_count = 0;
        let mut unexpected = false;
        for index in 0..info.AceCount {
            let mut raw = std::ptr::null_mut();
            // SAFETY: GetAce bounds-checks `index` and returns an ACE inside
            // `dacl`; the descriptor is still alive.
            if unsafe { GetAce(dacl, index, &mut raw) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if raw.is_null() {
                return Err(ambiguous_acl());
            }
            // ACCESS_ALLOWED_ACE_TYPE is 0 in the Windows ACE header.
            let header = unsafe { &*(raw as *const windows_sys::Win32::Security::ACE_HEADER) };
            if header.AceType != 0 {
                unexpected = true;
                continue;
            }
            if usize::from(header.AceSize) < 16 {
                return Err(ambiguous_acl());
            }
            let ace = raw as *const ACCESS_ALLOWED_ACE;
            // SAFETY: ACCESS_ALLOWED_ACE stores the SID at offset 8. Check its
            // encoded length stays inside this ACE before calling Windows.
            let sid: *mut std::ffi::c_void =
                unsafe { std::ptr::addr_of!((*ace).SidStart).cast_mut().cast() };
            let subauthorities = unsafe { *sid.cast::<u8>().add(1) } as usize;
            if 8 + subauthorities * 4 > usize::from(header.AceSize) - 8
                || unsafe { IsValidSid(sid) } == 0
            {
                return Err(ambiguous_acl());
            }
            let trustee = sid_text(sid)?;
            let mask = unsafe { (*ace).Mask };
            match trustee.as_str() {
                "S-1-5-18" => {
                    system_count += 1;
                    unexpected |= mask != 0x001f_01ff;
                }
                "S-1-5-32-544" => {
                    admin_count += 1;
                    unexpected |= mask != 0x001f_01ff;
                }
                "S-1-3-4" => {
                    owner_rights_count += 1;
                    unexpected |= mask != 0x0002_0000;
                }
                _ if mask == 0x001f_01ff && cashier.is_none() => cashier = Some(trustee),
                _ => unexpected = true,
            }
        }
        if owner_rights_count == 0 {
            return Ok(None);
        }
        if info.AceCount != 4
            || system_count != 1
            || admin_count != 1
            || owner_rights_count != 1
            || unexpected
        {
            return Err(ambiguous_acl());
        }
        cashier.map(Some).ok_or_else(ambiguous_acl)
    })();
    // SAFETY: this allocation came from GetNamedSecurityInfoW.
    unsafe { LocalFree(descriptor) };
    result
}

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
    let existed = exists_or_error(dir)?;
    if existed {
        std::fs::read_dir(dir)?;
    }
    for name in SENSITIVE_FILES {
        let path = dir.join(name);
        if exists_or_error(&path)? {
            std::fs::File::open(path)?;
        }
    }
    // Check every marked ACL before the first mutation. Re-ACLing the parent
    // can propagate inheritable entries to children, so a late check is unsafe.
    for path in std::iter::once(dir.to_path_buf())
        .filter(|_| existed)
        .chain(SENSITIVE_FILES.iter().map(|name| dir.join(name)))
    {
        if exists_or_error(&path)? && managed_user_sid(&path)?.is_some_and(|sid| sid != user_sid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                ProtectedUserMismatch,
            ));
        }
    }
    std::fs::create_dir_all(dir)?;
    if !existed {
        set_private_dacl(dir, user_sid, true)?;
    }
    // Files first: an existing readable config remains accessible even if
    // hardening the installer-owned parent fails without WRITE_DAC.
    for name in SENSITIVE_FILES {
        let path = dir.join(name);
        if exists_or_error(&path)? {
            set_private_dacl(&path, user_sid, false)?;
        }
    }
    if existed {
        set_private_dacl(dir, user_sid, true)?;
    }
    Ok(())
}

pub fn protect_file_for_user(path: &Path, user_sid: &str) -> io::Result<()> {
    set_private_dacl(path, user_sid, false)
}
