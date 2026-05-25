use std::path::PathBuf;

#[cfg(target_os = "linux")]
pub fn read_cwd(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
pub fn read_cwd(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }

    const PROC_PIDVNODEPATHINFO: nix::libc::c_int = 9;
    const MAXPATHLEN: usize = 1024;

    #[repr(C)]
    struct VInfoStat {
        vst_dev: u32,
        vst_mode: u16,
        vst_nlink: u16,
        vst_ino: u64,
        vst_uid: u32,
        vst_gid: u32,
        vst_atime: i64,
        vst_atimensec: i64,
        vst_mtime: i64,
        vst_mtimensec: i64,
        vst_ctime: i64,
        vst_ctimensec: i64,
        vst_birthtime: i64,
        vst_birthtimensec: i64,
        vst_size: i64,
        vst_blocks: i64,
        vst_blksize: i32,
        vst_flags: u32,
        vst_gen: u32,
        vst_rdev: u32,
        vst_qspare: [i64; 2],
    }

    #[repr(C)]
    struct VnodeInfo {
        vi_stat: VInfoStat,
        vi_type: i32,
        vi_pad: i32,
        vi_fsid: [i32; 2],
    }

    #[repr(C)]
    struct VnodeInfoPath {
        vip_vi: VnodeInfo,
        vip_path: [u8; MAXPATHLEN],
    }

    #[repr(C)]
    struct ProcVnodepathinfo {
        pvi_cdir: VnodeInfoPath,
        pvi_rdir: VnodeInfoPath,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: nix::libc::c_int,
            flavor: nix::libc::c_int,
            arg: u64,
            buffer: *mut nix::libc::c_void,
            buffersize: nix::libc::c_int,
        ) -> nix::libc::c_int;
    }

    let mut info: ProcVnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<ProcVnodepathinfo>() as nix::libc::c_int;
    let rc = unsafe {
        proc_pidinfo(
            pid as nix::libc::c_int,
            PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut nix::libc::c_void,
            size,
        )
    };
    if rc <= 0 {
        return None;
    }

    let path_bytes = &info.pvi_cdir.vip_path;
    let end = path_bytes.iter().position(|&b| b == 0).unwrap_or(MAXPATHLEN);
    if end == 0 {
        return None;
    }
    std::str::from_utf8(&path_bytes[..end])
        .ok()
        .map(PathBuf::from)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read_cwd(_pid: i32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn reads_own_cwd() {
        let pid = std::process::id() as i32;
        let read = read_cwd(pid).expect("read_cwd returned None for self");
        let expected = std::env::current_dir().expect("current_dir");
        let read = std::fs::canonicalize(&read).unwrap_or(read);
        let expected = std::fs::canonicalize(&expected).unwrap_or(expected);
        assert_eq!(read, expected);
    }

    #[test]
    fn rejects_invalid_pid() {
        assert!(read_cwd(0).is_none());
        assert!(read_cwd(-1).is_none());
    }
}
