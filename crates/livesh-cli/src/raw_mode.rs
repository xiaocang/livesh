use std::os::fd::AsFd;

use nix::sys::termios::{self, SetArg, Termios};

pub struct RawModeGuard {
    original: Termios,
}

impl RawModeGuard {
    pub fn enter() -> anyhow::Result<Self> {
        let stdin = std::io::stdin();
        let original = termios::tcgetattr(stdin.as_fd())?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw)?;
        Ok(Self { original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        let _ = termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &self.original);
    }
}
