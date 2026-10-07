// SPDX-License-Identifier: MIT OR Apache-2.0

//! A read-only-to-consumers ring of TCP records in host-shared memory.

use super::tcp_event::{FRAME_SIZE, QUEUE_CAPACITY};
use super::tcp_telemetry;
use crate::address::Address;
use crate::error::SvsmError;
use crate::locking::SpinLock;
use crate::mm::page_visibility::SharedBox;
use crate::mm::virt_to_phys;
use core::sync::atomic::{Ordering, fence};

const RING_SIZE: usize = 16 * 1024;
const HEADER_SIZE: usize = 64;
const SLOT_SIZE: usize = 8 + FRAME_SIZE + 8;
const SLOT_COUNT: usize = 128;
static RING: SpinLock<Option<Ring>> = SpinLock::new(None);

struct Ring {
    memory: SharedBox<[u8; RING_SIZE]>,
}

impl Ring {
    fn new() -> Result<Self, SvsmError> {
        let ring = Self {
            memory: SharedBox::try_new_zeroed()?,
        };
        // Initialize after conversion to shared memory: the private zeroes
        // are not guaranteed to remain zero when viewed without encryption.
        for offset in (0..RING_SIZE).step_by(8) {
            ring.stamp(offset, 0);
        }
        let mut header = [0u8; HEADER_SIZE];
        header[..8].copy_from_slice(b"CTCPRNG1");
        header[8..12].copy_from_slice(&(RING_SIZE as u32).to_le_bytes());
        header[12..16].copy_from_slice(&(SLOT_COUNT as u32).to_le_bytes());
        header[16..20].copy_from_slice(&(SLOT_SIZE as u32).to_le_bytes());
        header[20..24].copy_from_slice(&(FRAME_SIZE as u32).to_le_bytes());
        ring.write_bytes(0, &header);
        fence(Ordering::SeqCst);
        let gpa = virt_to_phys(ring.memory.addr()).bits();
        log::info!("TCP shared ring: gpa={gpa:#x} size={RING_SIZE}");
        Ok(ring)
    }

    fn write_bytes(&self, offset: usize, data: &[u8]) {
        debug_assert!(offset + data.len() <= RING_SIZE);
        for (index, byte) in data.iter().enumerate() {
            // SAFETY: the offset is within this live shared allocation; byte
            // accesses do not create references to host-accessible memory.
            unsafe {
                self.memory
                    .addr()
                    .as_mut_ptr::<u8>()
                    .add(offset + index)
                    .write_volatile(*byte);
            }
        }
    }

    fn stamp(&self, offset: usize, value: u64) {
        // SAFETY: slot stamps are aligned u64s within the allocation. The
        // lock serializes writers, and consumers only read these stamps.
        unsafe {
            self.memory
                .addr()
                .as_mut_ptr::<u8>()
                .add(offset)
                .cast::<u64>()
                .write_volatile(value);
        }
    }

    fn publish(&self, frame: &[u8; FRAME_SIZE]) {
        let sequence = u64::from_be_bytes(frame[8..16].try_into().unwrap());
        let offset = HEADER_SIZE + ((sequence - 1) % SLOT_COUNT as u64) as usize * SLOT_SIZE;
        self.stamp(offset, 0);
        self.stamp(offset + 8 + FRAME_SIZE, 0);
        fence(Ordering::SeqCst);
        self.write_bytes(offset + 8, frame);
        fence(Ordering::SeqCst);
        self.stamp(offset + 8 + FRAME_SIZE, sequence);
        fence(Ordering::SeqCst);
        self.stamp(offset, sequence);
    }
}

/// Publish after releasing the guest VMSA reference. No host acknowledgement
/// is required: new records overwrite old slots if the reader falls behind.
pub fn flush() {
    if !tcp_telemetry::pending() {
        return;
    }
    let mut ring = RING.lock();
    if ring.is_none() {
        match Ring::new() {
            Ok(new) => *ring = Some(new),
            Err(error) => {
                log::warn!("Cannot allocate TCP shared ring: {error:?}");
                return;
            }
        }
    }
    for _ in 0..QUEUE_CAPACITY {
        let Some(frame) = tcp_telemetry::pop_frame() else {
            break;
        };
        ring.as_ref().unwrap().publish(&frame);
    }
}
