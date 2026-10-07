// SPDX-License-Identifier: MIT OR Apache-2.0

//! TCP observation records shared by the telemetry producer and transport.

pub const FRAME_SIZE: usize = 56;
pub const QUEUE_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug)]
pub struct TcpEvent {
    pub sequence: u64,
    pub observed_tsc: u64,
    pub sock_ptr: u64,
    pub state: u8,
    pub source_ipv4: [u8; 4],
    pub source_port: u16,
    pub destination_ipv4: [u8; 4],
    pub destination_port: u16,
}

impl TcpEvent {
    pub fn encode(self, dropped_events: u64) -> [u8; FRAME_SIZE] {
        let mut frame = [0; FRAME_SIZE];
        frame[..4].copy_from_slice(b"CTCP");
        frame[4] = 1;
        frame[5] = 1;
        frame[6..8].copy_from_slice(&(FRAME_SIZE as u16).to_be_bytes());
        frame[8..16].copy_from_slice(&self.sequence.to_be_bytes());
        frame[16..24].copy_from_slice(&self.observed_tsc.to_be_bytes());
        frame[24..32].copy_from_slice(&self.sock_ptr.to_be_bytes());
        frame[32] = self.state;
        frame[36..40].copy_from_slice(&self.source_ipv4);
        frame[40..42].copy_from_slice(&self.source_port.to_be_bytes());
        frame[42..46].copy_from_slice(&self.destination_ipv4);
        frame[46..48].copy_from_slice(&self.destination_port.to_be_bytes());
        frame[48..].copy_from_slice(&dropped_events.to_be_bytes());
        frame
    }
}

#[derive(Debug)]
pub struct EventQueue {
    entries: [Option<TcpEvent>; QUEUE_CAPACITY],
    head: usize,
    len: usize,
}

impl EventQueue {
    pub const fn new() -> Self {
        Self {
            entries: [None; QUEUE_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    pub fn push(&mut self, event: TcpEvent) -> bool {
        if self.len == QUEUE_CAPACITY {
            return false;
        }
        self.entries[(self.head + self.len) % QUEUE_CAPACITY] = Some(event);
        self.len += 1;
        true
    }

    pub fn pop(&mut self) -> Option<TcpEvent> {
        if self.len == 0 {
            return None;
        }
        let event = self.entries[self.head].take();
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        self.len -= 1;
        event
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64) -> TcpEvent {
        TcpEvent {
            sequence,
            observed_tsc: 11,
            sock_ptr: 13,
            state: 1,
            source_ipv4: [192, 0, 2, 1],
            source_port: 1234,
            destination_ipv4: [198, 51, 100, 2],
            destination_port: 443,
        }
    }

    #[test]
    fn wire_format_matches_receiver() {
        assert_eq!(
            event(7).encode(17),
            [
                67, 84, 67, 80, 1, 1, 0, 56, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 11, 0, 0,
                0, 0, 0, 0, 0, 13, 1, 0, 0, 0, 192, 0, 2, 1, 4, 210, 198, 51, 100, 2, 1, 187, 0, 0,
                0, 0, 0, 0, 0, 17,
            ]
        );
    }

    #[test]
    fn full_queue_preserves_order_after_wrap() {
        let mut queue = EventQueue::new();
        for sequence in 0..QUEUE_CAPACITY as u64 {
            assert!(queue.push(event(sequence)));
        }
        assert!(!queue.push(event(999)));
        for sequence in 0..64 {
            assert_eq!(queue.pop().unwrap().sequence, sequence);
        }
        for sequence in 128..192 {
            assert!(queue.push(event(sequence)));
        }
        for sequence in 64..192 {
            assert_eq!(queue.pop().unwrap().sequence, sequence);
        }
        assert!(queue.is_empty());
        assert!(queue.pop().is_none());
    }
}
