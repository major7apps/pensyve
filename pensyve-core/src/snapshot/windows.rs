//! Win32 creation-time access control and write-through publication for snapshots.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SECURITY_MAX_SID_SIZE, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::SystemServices::{
    FILE_PERSISTENT_ACLS, SECURITY_DESCRIPTOR_REVISION,
};

fn check(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// Canonicalizing the existing parent gives Win32 an absolute, verbatim path, including
// for long paths and UNC shares. The final component may not exist yet.
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut wide: Vec<_> = parent
        .canonicalize()?
        .join(name)
        .as_os_str()
        .encode_wide()
        .collect();
    if wide.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    wide.push(0);
    Ok(wide)
}

fn with_owner_security<T>(
    create: impl FnOnce(&SECURITY_ATTRIBUTES) -> io::Result<T>,
) -> io::Result<T> {
    // An ACL must be DWORD aligned. Reserve a header, one allowed ACE, and its variable SID.
    const ACL_BYTES: usize =
        size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() + SECURITY_MAX_SID_SIZE as usize;
    #[repr(C)]
    struct TokenBuffer {
        user: TOKEN_USER,
        sid: [u8; SECURITY_MAX_SID_SIZE as usize],
    }
    let mut token = TokenBuffer {
        user: TOKEN_USER::default(),
        sid: [0; SECURITY_MAX_SID_SIZE as usize],
    };
    let mut returned = 0;
    // GetCurrentThreadEffectiveToken is an inline Windows SDK function (processthreadsapi.h).
    // Its pseudo-handle selects the impersonation token, or the process token when absent.
    let effective_token = -6isize as HANDLE;
    // SAFETY: the aligned buffer holds TOKEN_USER and the largest possible SID.
    unsafe {
        check(GetTokenInformation(
            effective_token,
            TokenUser,
            (&raw mut token).cast(),
            size_of::<TokenBuffer>() as u32,
            &raw mut returned,
        ))?;
    }
    let owner = token.user.User.Sid;
    let mut acl_words = [0u32; ACL_BYTES.div_ceil(4)];
    let acl_size = size_of_val(&acl_words) as u32;
    let acl = acl_words.as_mut_ptr().cast();
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let sd = (&raw mut descriptor).cast();
    // SAFETY: all pointers reference correctly sized, aligned storage which outlives create().
    // The descriptor and ACL borrow the token SID and are consumed synchronously by Win32.
    unsafe {
        check(InitializeAcl(acl, acl_size, ACL_REVISION))?;
        check(AddAccessAllowedAce(
            acl,
            ACL_REVISION,
            FILE_ALL_ACCESS,
            owner,
        ))?;
        check(InitializeSecurityDescriptor(
            sd,
            SECURITY_DESCRIPTOR_REVISION,
        ))?;
        check(SetSecurityDescriptorOwner(sd, owner, 0))?;
        check(SetSecurityDescriptorDacl(sd, 1, acl, 0))?;
        check(SetSecurityDescriptorControl(
            sd,
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        ))?;
    }
    create(&SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    })
}

pub(super) fn create_snapshot_dir(dir: &Path) -> io::Result<()> {
    let absolute = std::path::absolute(dir)?;
    let missing: Vec<_> = absolute.ancestors().take_while(|p| !p.is_dir()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    with_owner_security(|attributes| {
        for path in missing.into_iter().rev() {
            let wide = wide_path(path)?;
            // SAFETY: wide is terminated and attributes borrows the live descriptor.
            if let Err(error) = check(unsafe { CreateDirectoryW(wide.as_ptr(), attributes) })
                && (error.kind() != io::ErrorKind::AlreadyExists || !path.is_dir())
            {
                return Err(error);
            }
        }
        Ok(())
    })
}

pub(super) fn create_owner_only_file(path: &Path) -> io::Result<File> {
    let wide = wide_path(path)?;
    let file = with_owner_security(|attributes| {
        // SAFETY: wide and the descriptor are live; CREATE_NEW never opens an existing file.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW transferred this new handle to us.
        Ok(unsafe { File::from_raw_handle(handle) })
    })?;
    // FAT and exFAT may ignore a supplied descriptor. Check the opened file's actual
    // volume before writing any content, including when a mount point redirects the path.
    let mut flags = 0;
    // SAFETY: file owns its open handle, and all unused outputs have null buffers.
    let volume = check(unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &raw mut flags,
            null_mut(),
            0,
        )
    });
    if volume.is_err() || flags & FILE_PERSISTENT_ACLS == 0 {
        drop(file);
        let _ = std::fs::remove_file(path);
        volume?;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "snapshot volume cannot enforce persistent ACLs",
        ));
    }
    Ok(file)
}

// Tests can fail a specific writer's next move at the syscall boundary without racing
// other writers on the parallel test runner.
#[cfg(test)]
std::thread_local! {
    pub(super) static FAIL_NEXT_MOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn publish_snapshot(from: &Path, to: &Path, _dir: &Path) -> io::Result<()> {
    let from = wide_path(from)?;
    let to = wide_path(to)?;
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    #[cfg(test)]
    if FAIL_NEXT_MOVE.replace(false) {
        assert_eq!(
            flags & MOVEFILE_WRITE_THROUGH,
            MOVEFILE_WRITE_THROUGH,
            "publication must request MOVEFILE_WRITE_THROUGH"
        );
        return Err(io::Error::other("injected write-through move failure"));
    }
    // SAFETY: both paths are terminated UTF-16 buffers live for this call.
    check(unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) })
}
