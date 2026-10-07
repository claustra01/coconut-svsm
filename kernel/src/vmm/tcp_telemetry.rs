// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded, best-effort queue for guest TCP observations.

use super::tcp_event::{EventQueue, FRAME_SIZE, TcpEvent};
use crate::cpu::msr::rdtsc;
use crate::locking::SpinLock;

static QUEUE: SpinLock<Producer> = SpinLock::new(Producer {
    events: EventQueue::new(),
    sequence: 0,
    dropped: 0,
});

struct Producer {
    events: EventQueue,
    sequence: u64,
    dropped: u64,
}

pub fn enqueue_tcp_event(
    sock_ptr: u64,
    state: u8,
    source_ipv4: [u8; 4],
    source_port: u16,
    destination_ipv4: [u8; 4],
    destination_port: u16,
) {
    let mut queue = QUEUE.lock();
    queue.sequence += 1;
    let event = TcpEvent {
        sequence: queue.sequence,
        observed_tsc: rdtsc(),
        sock_ptr,
        state,
        source_ipv4,
        source_port,
        destination_ipv4,
        destination_port,
    };
    if !queue.events.push(event) {
        queue.dropped += 1;
    }
}

pub fn pop_frame() -> Option<[u8; FRAME_SIZE]> {
    let mut queue = QUEUE.lock();
    queue.events.pop().map(|event| event.encode(queue.dropped))
}

pub fn pending() -> bool {
    !QUEUE.lock().events.is_empty()
}
