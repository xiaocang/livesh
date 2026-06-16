//! Best-effort process-title rewriting so `ps`/`w` show a meaningful name —
//! e.g. `livesh [shell] (sh_4cceeab1) ~/work/livesh` — instead of the raw
//! `…/livesh --open sh_<32hex>` argv that cmux launches us with.
//!
//! macOS exposes the real, writable argv buffer through the libc
//! `_NSGetArgv`/`_NSGetArgc` accessors. `ps`/`w` read that same buffer back via
//! `KERN_PROCARGS2`, so overwriting it in place changes what they display. The
//! rewrite is bounded by the original argv span (argv[0]..last arg) — there is
//! no way to grow it — which is ample for our titles.
//!
//! On other platforms there is no portable way to reach the argv buffer without
//! capturing it in a startup constructor, so this is a no-op.

/// Set the process title shown by `ps`/`w`. Best-effort: silently does nothing
/// if the platform is unsupported or the argv buffer can't be reached.
///
/// Must be called *after* the program is done reading its own arguments —
/// on macOS `std::env::args` reads the very buffer this overwrites.
pub fn set_title(title: &str) {
    #[cfg(target_os = "macos")]
    macos::set_title(title);
    #[cfg(not(target_os = "macos"))]
    let _ = title;
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CStr;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn _NSGetArgv() -> *mut *mut *mut c_char;
        fn _NSGetArgc() -> *mut c_int;
    }

    pub fn set_title(title: &str) {
        unsafe {
            let argc_ptr = _NSGetArgc();
            let argv_ptr = _NSGetArgv();
            if argc_ptr.is_null() || argv_ptr.is_null() {
                return;
            }
            let argc = *argc_ptr;
            let argv = *argv_ptr;
            if argc < 1 || argv.is_null() {
                return;
            }
            let first = *argv;
            let last = *argv.add((argc - 1) as usize);
            if first.is_null() || last.is_null() {
                return;
            }

            // argv strings sit contiguously on the stack; reclaim the whole span
            // from argv[0] to the end of the last argument as one writable buffer.
            let last_len = CStr::from_ptr(last).to_bytes().len();
            let cap = (last as usize + last_len + 1).saturating_sub(first as usize);
            if cap < 2 {
                return;
            }

            let buf = std::slice::from_raw_parts_mut(first as *mut u8, cap);
            buf.fill(0); // also blanks argv[1..] so `w` shows only the title
            let bytes = title.as_bytes();
            let n = bytes.len().min(cap - 1); // leave the final NUL
            buf[..n].copy_from_slice(&bytes[..n]);
        }
    }
}
