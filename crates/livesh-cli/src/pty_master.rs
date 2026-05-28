use std::{
    io::{self, Read, Write},
    os::unix::io::{AsRawFd, RawFd},
};

use anyhow::Context;
use nix::libc;
use portable_pty::MasterPty;

/// Raw-fd owner for a PTY master. Used in two ways:
///  * fresh shells: extract the fd from a portable-pty master and keep it
///    alive past the Box's drop (we leak the wrapper, not the fd);
///  * hot-upgrade: adopt a fd that the previous daemon left open across
///    execv.
pub struct OwnedPtyMaster {
    fd: RawFd,
}

impl OwnedPtyMaster {
    /// Adopt a raw fd that already names a PTY master. Caller must ensure
    /// no other handle still owns it.
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }

    /// Take the master fd out of a portable-pty `MasterPty`. The Box is
    /// leaked (small one-time alloc per shell) so its Drop does not close
    /// the kernel fd we still want to use.
    pub fn from_portable(master: Box<dyn MasterPty + Send>) -> anyhow::Result<Self> {
        let fd = master
            .as_raw_fd()
            .context("portable-pty master has no raw fd")?;
        std::mem::forget(master);
        Ok(Self { fd })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.fd, libc::TIOCSWINSZ, &ws) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n =
                unsafe { libc::write(self.fd, bytes.as_ptr() as *const _, bytes.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[n as usize..];
        }
        Ok(())
    }

    /// Hand out a fresh reader fd via `fcntl(F_DUPFD_CLOEXEC)`. The new
    /// fd has CLOEXEC set so it dies on the next execv (only the master
    /// itself survives a hot-upgrade).
    pub fn clone_reader(&self) -> io::Result<OwnedReader> {
        let dup = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedReader { fd: dup })
    }

    /// Clear FD_CLOEXEC on the master fd so it survives execv. Call only
    /// from the hot-upgrade path; once cleared the fd will be inherited
    /// by every child until we exit.
    pub fn clear_cloexec(&self) -> io::Result<()> {
        clear_cloexec(self.fd)
    }
}

impl Drop for OwnedPtyMaster {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

pub struct OwnedReader {
    fd: RawFd,
}

impl AsRawFd for OwnedReader {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Read for OwnedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = unsafe {
                libc::read(self.fd, buf.as_mut_ptr() as *mut _, buf.len())
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            return Ok(n as usize);
        }
    }
}

impl Drop for OwnedReader {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

pub fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// `Write` blanket impl over `&OwnedPtyMaster` to plug it into anything
// that wants `std::io::Write` (we keep the master behind a Mutex<Option<…>>
// and write under the lock).
impl<'a> Write for &'a OwnedPtyMaster {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        OwnedPtyMaster::write_all(self, buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
