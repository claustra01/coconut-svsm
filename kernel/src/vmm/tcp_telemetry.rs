// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Shinya Murakami <sny430@gmail.com>

//! Delivery of observed guest TCP connection metadata over virtio-vsock.

extern crate alloc;

use crate::cpu::msr::rdtsc;
use crate::io::Write;
use crate::locking::SpinLock;
use crate::task::{KernelThreadStartInfo, schedule, start_kernel_task};
use crate::vsock::{VMADDR_CID_HOST, stream::VsockStream};
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const TELEMETRY_PORT: u32 = 4050;
const QUEUE_CAPACITY: usize = 128;
const FRAME_SIZE: usize = 56;
const FRAME_MAGIC: &[u8; 4] = b"CTCP";
const FRAME_VERSION: u8 = 1;
const FRAME_TYPE_TCP_CONNECTION: u8 = 1;
const SEND_BATCH_SIZE: usize = 32;

static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);
static SENDER_STARTED: AtomicBool = AtomicBool::new(false);
static EVENT_QUEUE: SpinLock<EventQueue> = SpinLock::new(EventQueue::new());

#[derive(Clone, Copy, Debug)]
struct TcpEvent {
    sequence: u64,
    observed_tsc: u64,
    sock_ptr: u64,
    state: u8,
    source_ipv4: [u8; 4],
    source_port: u16,
    destination_ipv4: [u8; 4],
    destination_port: u16,
}

impl TcpEvent {
    fn encode(self, dropped_events: u64) -> [u8; FRAME_SIZE] {
        let mut frame = [0u8; FRAME_SIZE];
        frame[0..4].copy_from_slice(FRAME_MAGIC);
        frame[4] = FRAME_VERSION;
        frame[5] = FRAME_TYPE_TCP_CONNECTION;
        frame[6..8].copy_from_slice(&(FRAME_SIZE as u16).to_be_bytes());
        frame[8..16].copy_from_slice(&self.sequence.to_be_bytes());
        frame[16..24].copy_from_slice(&self.observed_tsc.to_be_bytes());
        frame[24..32].copy_from_slice(&self.sock_ptr.to_be_bytes());
        frame[32] = self.state;
        frame[36..40].copy_from_slice(&self.source_ipv4);
        frame[40..42].copy_from_slice(&self.source_port.to_be_bytes());
        frame[42..46].copy_from_slice(&self.destination_ipv4);
        frame[46..48].copy_from_slice(&self.destination_port.to_be_bytes());
        frame[48..56].copy_from_slice(&dropped_events.to_be_bytes());
        frame
    }
}

struct EventQueue {
    entries: [Option<TcpEvent>; QUEUE_CAPACITY],
    head: usize,
    len: usize,
}

impl EventQueue {
    const fn new() -> Self {
        Self {
            entries: [None; QUEUE_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, event: TcpEvent) -> bool {
        if self.len == QUEUE_CAPACITY {
            return false;
        }

        let tail = (self.head + self.len) % QUEUE_CAPACITY;
        self.entries[tail] = Some(event);
        self.len += 1;
        true
    }

    fn front(&self) -> Option<TcpEvent> {
        if self.len == 0 {
            None
        } else {
            self.entries[self.head]
        }
    }

    fn pop_front(&mut self, sequence: u64) {
        let Some(event) = self.front() else {
            return;
        };
        if event.sequence != sequence {
            return;
        }

        self.entries[self.head] = None;
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        self.len -= 1;
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub fn enqueue_tcp_event(
    sock_ptr: u64,
    state: u8,
    source_ipv4: [u8; 4],
    source_port: u16,
    destination_ipv4: [u8; 4],
    destination_port: u16,
) -> bool {
    let event = TcpEvent {
        sequence: NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1,
        observed_tsc: rdtsc(),
        sock_ptr,
        state,
        source_ipv4,
        source_port,
        destination_ipv4,
        destination_port,
    };

    if EVENT_QUEUE.lock().push(event) {
        true
    } else {
        DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed);
        false
    }
}

pub fn telemetry_pending() -> bool {
    !EVENT_QUEUE.lock().is_empty()
}

/// Starts the sender lazily on the first event, then yields to it on later
/// batches. This keeps the normal SVSM boot scheduling path unchanged.
pub fn wake_tcp_telemetry_sender() {
    if SENDER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        if let Err(error) = start_kernel_task(
            KernelThreadStartInfo::new(tcp_telemetry_task, 0),
            String::from("TCP telemetry sender"),
        ) {
            SENDER_STARTED.store(false, Ordering::Release);
            log::warn!("Failed to launch TCP telemetry sender task: {error:?}");
        }
    } else {
        schedule();
    }
}

fn send_frame(stream: &mut VsockStream, frame: &[u8]) -> bool {
    let mut sent = 0;
    while sent < frame.len() {
        match stream.write(&frame[sent..]) {
            Ok(0) | Err(_) => return false,
            Ok(len) => sent += len,
        }
    }
    true
}

/// Sends queued events to a host listener on CID 2, port 4050.
///
/// The host listener must be started before the first event is produced. The
/// current vsock connection implementation polls synchronously for a response.
pub fn tcp_telemetry_task(_: usize) {
    let mut stream = None;
    let mut batch_count = 0;

    log::info!("TCP telemetry sender started; host port={TELEMETRY_PORT}");

    loop {
        let Some(event) = EVENT_QUEUE.lock().front() else {
            batch_count = 0;
            schedule();
            continue;
        };

        if stream.is_none() {
            match VsockStream::connect(TELEMETRY_PORT, VMADDR_CID_HOST) {
                Ok(connected) => {
                    log::info!("TCP telemetry connected to host vsock port {TELEMETRY_PORT}");
                    stream = Some(connected);
                }
                Err(error) => {
                    log::warn!("TCP telemetry vsock connection failed: {error:?}");
                    schedule();
                    continue;
                }
            }
        }

        let frame = event.encode(DROPPED_EVENTS.load(Ordering::Relaxed));
        if !send_frame(stream.as_mut().unwrap(), &frame) {
            log::warn!("TCP telemetry vsock send failed; reconnecting");
            stream = None;
            schedule();
            continue;
        }

        EVENT_QUEUE.lock().pop_front(event.sequence);
        batch_count += 1;
        if batch_count == SEND_BATCH_SIZE {
            batch_count = 0;
            schedule();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_event_wire_format() {
        let event = TcpEvent {
            sequence: 7,
            observed_tsc: 11,
            sock_ptr: 13,
            state: 1,
            source_ipv4: [192, 0, 2, 1],
            source_port: 1234,
            destination_ipv4: [198, 51, 100, 2],
            destination_port: 443,
        };
        let frame = event.encode(17);

        assert_eq!(&frame[0..4], b"CTCP");
        assert_eq!(frame[4], 1);
        assert_eq!(u16::from_be_bytes(frame[6..8].try_into().unwrap()), 56);
        assert_eq!(u64::from_be_bytes(frame[8..16].try_into().unwrap()), 7);
        assert_eq!(&frame[36..40], &[192, 0, 2, 1]);
        assert_eq!(u16::from_be_bytes(frame[46..48].try_into().unwrap()), 443);
        assert_eq!(u64::from_be_bytes(frame[48..56].try_into().unwrap()), 17);
    }
}
