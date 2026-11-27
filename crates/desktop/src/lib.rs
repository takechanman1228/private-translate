//! Integration with the desktop outside the app's window.
//!
//! - [`clipboard`]: a double copy (Ctrl+C or ⌘C pressed twice) in any application.
//! - [`instance`]: one running instance per user. Starting the app again shows the running one,
//!   and on Windows its notification-area icon opens or quits it.
//!
//! Nothing here needs a permission: no keyboard hook and no accessibility access.

pub mod clipboard;
pub mod instance;

#[cfg(windows)]
mod win;

/// Available volume capacity, including space macOS can reclaim for important writes.
#[cfg(target_os = "macos")]
pub fn volume_capacity_for_important_usage(dir: &std::path::Path) -> Option<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, ptr::NonNull};

    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSNumber, NSURL, NSURLVolumeAvailableCapacityForImportantUsageKey};

    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    autoreleasepool(|_| {
        // SAFETY: path is NUL-terminated and stays alive for the call.
        let url = unsafe {
            NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
                NonNull::new(path.as_ptr().cast_mut())?,
                true,
                None,
            )
        };
        let mut value = None;
        // SAFETY: the key is a Foundation constant and value is an initialized, writable slot.
        unsafe {
            url.getResourceValue_forKey_error(
                &mut value,
                NSURLVolumeAvailableCapacityForImportantUsageKey,
            )
            .ok()?;
        }
        u64::try_from(value?.downcast::<NSNumber>().ok()?.longLongValue()).ok()
    })
}
