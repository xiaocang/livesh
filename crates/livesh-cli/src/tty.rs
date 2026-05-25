use std::{
    io::IsTerminal,
    os::fd::{AsRawFd, RawFd},
};

use nix::libc;

#[derive(Debug, Clone, Copy)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Default for Size {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

pub fn stdin_stdout_are_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

pub fn current_size() -> Size {
    size_for_fd(std::io::stdout().as_raw_fd()).unwrap_or_default()
}

fn size_for_fd(fd: RawFd) -> Option<Size> {
    let mut winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) };
    if rc == 0 && winsize.ws_col > 0 && winsize.ws_row > 0 {
        Some(Size {
            cols: winsize.ws_col,
            rows: winsize.ws_row,
        })
    } else {
        None
    }
}
