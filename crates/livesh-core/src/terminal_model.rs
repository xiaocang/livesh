use crate::limits::BoundedBytes;

const RESET_SCREEN: &[u8] = b"\x1b[?1049l\x1b[H\x1b[2J\x1b[3J";

#[derive(Debug, Clone)]
pub struct TerminalModel {
    snapshot: BoundedBytes,
}

impl TerminalModel {
    pub fn new(snapshot_bytes: usize) -> Self {
        Self {
            snapshot: BoundedBytes::new(snapshot_bytes),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.snapshot.append(bytes);
    }

    pub fn snapshot_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(RESET_SCREEN.len() + self.snapshot.len());
        bytes.extend_from_slice(RESET_SCREEN);
        bytes.extend_from_slice(&self.snapshot.bytes());
        bytes
    }

    pub fn raw_snapshot(&self) -> Vec<u8> {
        self.snapshot.bytes()
    }

    pub fn clear(&mut self) {
        self.snapshot.clear();
    }
}
