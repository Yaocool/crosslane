//! Darwin extended ACLs can grant access independently of Unix mode bits.
use std::fs::File;
use std::os::fd::AsRawFd;

use anyhow::{bail, Context, Result};
use libc::{c_int, c_void};

// Darwin <sys/acl.h>: acl_t/acl_entry_t are opaque pointers, acl_type_t is a C
// enum, ACL_TYPE_EXTENDED = 0x100 and ACL_FIRST_ENTRY = 0. These APIs are in
// libSystem, but this libc crate version does not expose their declarations.
unsafe extern "C" {
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
    fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
    fn acl_free(object: *mut c_void) -> c_int;
}

pub(super) fn ensure_private(file: &File) -> Result<()> {
    // SAFETY: file owns the live descriptor and 0x100 is ACL_TYPE_EXTENDED.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        // Darwin reports ENOENT for an object without an extended ACL. A
        // filesystem without ACL support still has the checked Unix modes.
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTSUP | libc::EOPNOTSUPP) => Ok(()),
            _ => Err(error).context("inspect private credential ACL"),
        };
    }
    let result = inspect_entries(acl);
    // SAFETY: the copy came from acl_get_fd_np and is released exactly once;
    // neither the ACL nor its entry pointer is used after this call.
    unsafe {
        acl_free(acl);
    }
    result
}

fn inspect_entries(acl: *mut c_void) -> Result<()> {
    let mut entry_id = 0; // ACL_FIRST_ENTRY; subsequent entries use ACL_NEXT_ENTRY.
    loop {
        let mut entry = std::ptr::null_mut();
        // SAFETY: the caller holds the valid ACL copy for this complete call.
        let result = unsafe { acl_get_entry(acl, entry_id, &mut entry) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            // Darwin returns -1/EINVAL at the end of a valid ACL, including an
            // empty ACL. Every other inspection failure is rejected.
            return if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(())
            } else {
                Err(error).context("inspect private credential ACL entry")
            };
        }
        let mut tag = 0;
        // SAFETY: the successful get_entry returned a live entry descriptor.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("inspect private credential ACL tag");
        }
        // Deny-only entries cannot broaden access, so preserve restrictive ACLs.
        // Additional allow entries (ACL_EXTENDED_ALLOW=1), or unknown tags, are
        // rejected; Unix owner-only modes remain the source of granted access.
        if tag != 2 {
            // ACL_EXTENDED_DENY
            bail!("private credential files and directories must not have additional ACL grants");
        }
        entry_id = -1; // ACL_NEXT_ENTRY
    }
}
