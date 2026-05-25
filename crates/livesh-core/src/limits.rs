use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_shells: usize,
    pub scrollback_lines_per_shell: usize,
    pub scrollback_bytes_per_shell: usize,
    pub event_ring_bytes_per_shell: usize,
    pub snapshot_bytes_per_shell: usize,
    pub global_runtime_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_shells: 128,
            scrollback_lines_per_shell: 10_000,
            scrollback_bytes_per_shell: 10 * 1024 * 1024,
            event_ring_bytes_per_shell: 4 * 1024 * 1024,
            snapshot_bytes_per_shell: 2 * 1024 * 1024,
            global_runtime_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BoundedBytes {
    max_bytes: usize,
    bytes: Vec<u8>,
}

impl BoundedBytes {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: Vec::new(),
        }
    }

    pub fn append(&mut self, chunk: &[u8]) {
        if self.max_bytes == 0 {
            self.bytes.clear();
            return;
        }

        if chunk.len() >= self.max_bytes {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&chunk[chunk.len() - self.max_bytes..]);
            return;
        }

        self.bytes.extend_from_slice(chunk);
        let overflow = self.bytes.len().saturating_sub(self.max_bytes);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

#[derive(Debug, Clone)]
pub struct EventRing {
    max_bytes: usize,
    total_bytes: usize,
    events: VecDeque<(u64, Vec<u8>)>,
}

impl EventRing {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            events: VecDeque::new(),
        }
    }

    pub fn append(&mut self, seq: u64, bytes: &[u8]) {
        if self.max_bytes == 0 {
            self.events.clear();
            self.total_bytes = 0;
            return;
        }

        let bytes = if bytes.len() > self.max_bytes {
            bytes[bytes.len() - self.max_bytes..].to_vec()
        } else {
            bytes.to_vec()
        };
        self.total_bytes += bytes.len();
        self.events.push_back((seq, bytes));
        self.prune();
    }

    pub fn after(&self, seq: u64) -> Vec<(u64, Vec<u8>)> {
        self.events
            .iter()
            .filter(|(event_seq, _)| *event_seq > seq)
            .map(|(event_seq, bytes)| (*event_seq, bytes.clone()))
            .collect()
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.total_bytes);
        for (_, event) in &self.events {
            bytes.extend_from_slice(event);
        }
        bytes
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.total_bytes = 0;
    }

    fn prune(&mut self) {
        while self.total_bytes > self.max_bytes {
            let Some((_, bytes)) = self.events.pop_front() else {
                self.total_bytes = 0;
                return;
            };
            self.total_bytes = self.total_bytes.saturating_sub(bytes.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_bytes_discards_old_content() {
        let mut ring = BoundedBytes::new(5);
        ring.append(b"abc");
        ring.append(b"def");
        assert_eq!(ring.bytes(), b"bcdef");
    }

    #[test]
    fn event_ring_keeps_recent_events() {
        let mut ring = EventRing::new(5);
        ring.append(1, b"abc");
        ring.append(2, b"def");
        assert_eq!(ring.after(0), vec![(2, b"def".to_vec())]);
        assert_eq!(ring.total_bytes(), 3);
    }
}
