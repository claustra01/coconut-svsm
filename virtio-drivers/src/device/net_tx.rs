// SPDX-License-Identifier: MIT

//! Minimal, polled transmit-only driver for modern VirtIO network devices.

use crate::hal::Hal;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, Transport};
use crate::{Error, Result};
use alloc::{boxed::Box, vec};
use bitflags::bitflags;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const QUEUE_SIZE: usize = 16;
const HEADER_SIZE: usize = 12;

bitflags! {
    #[derive(Clone, Copy, Debug)]
    struct Features: u64 {
        const VERSION_1 = 1 << 32;
        const ACCESS_PLATFORM = 1 << 33;
    }
}

struct Pending {
    token: u16,
    buffer: Box<[u8]>,
}

/// Owns one pending Ethernet frame until the device completes its descriptor.
///
/// No offloads, receive buffers, interrupt handler, or control queue are used.
pub struct VirtIONetTx<H: Hal, T: Transport> {
    transport: T,
    _rx: VirtQueue<H, QUEUE_SIZE>,
    tx: VirtQueue<H, QUEUE_SIZE>,
    pending: Option<Pending>,
}

impl<H: Hal, T: Transport> core::fmt::Debug for VirtIONetTx<H, T> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("VirtIONetTx")
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl<H: Hal, T: Transport> VirtIONetTx<H, T> {
    /// Initializes the required RX/TX queue pair, leaving RX empty.
    pub fn new(mut transport: T) -> Result<Self> {
        if transport.device_type() != DeviceType::Network {
            return Err(Error::InvalidParam);
        }
        // All queue and packet memory goes through H's DMA/share methods.
        // QEMU requires ACCESS_PLATFORM for confidential guests, even when
        // the MMIO DMA addresses are shared GPAs rather than IOMMU mappings.
        let features = transport.begin_init(Features::VERSION_1 | Features::ACCESS_PLATFORM);
        if !features.contains(Features::VERSION_1)
            || !transport.get_status().contains(DeviceStatus::FEATURES_OK)
        {
            return Err(Error::Unsupported);
        }
        let mut rx = VirtQueue::new(&mut transport, RX_QUEUE, false, false)?;
        let mut tx = match VirtQueue::new(&mut transport, TX_QUEUE, false, false) {
            Ok(queue) => queue,
            Err(error) => {
                transport.queue_unset(RX_QUEUE);
                return Err(error);
            }
        };
        rx.set_dev_notify(false);
        tx.set_dev_notify(false);
        transport.finish_init();
        Ok(Self {
            transport,
            _rx: rx,
            tx,
            pending: None,
        })
    }

    /// Reclaims a completed buffer without waiting; returns whether TX is free.
    pub fn poll_complete(&mut self) -> Result<bool> {
        let Some(pending) = self.pending.as_ref() else {
            return Ok(true);
        };
        if !self.tx.can_pop() {
            return Ok(false);
        }
        // SAFETY: the boxed buffer has not moved or been touched since add().
        // Its lifetime extends through completion of this exact descriptor.
        unsafe {
            self.tx
                .pop_used(pending.token, &[&pending.buffer], &mut [])?;
        }
        self.pending = None;
        Ok(true)
    }

    /// Copies and submits an Ethernet frame, returning immediately.
    ///
    /// Returns [`Error::NotReady`] while a previous frame is still pending.
    pub fn send(&mut self, frame: &[u8]) -> Result {
        if !self.poll_complete()? {
            return Err(Error::NotReady);
        }
        if !(14..=1514).contains(&frame.len()) {
            return Err(Error::InvalidParam);
        }
        // VERSION_1 always uses the 12-byte header. Zero disables all offloads.
        let mut buffer = vec![0; HEADER_SIZE + frame.len()].into_boxed_slice();
        buffer[HEADER_SIZE..].copy_from_slice(frame);
        // SAFETY: ownership is retained in pending until pop_used completes.
        let token = unsafe { self.tx.add(&[&buffer], &mut [])? };
        self.pending = Some(Pending { token, buffer });
        if self.tx.should_notify() {
            self.transport.notify(TX_QUEUE);
        }
        Ok(())
    }
}

impl<H: Hal, T: Transport> Drop for VirtIONetTx<H, T> {
    fn drop(&mut self) {
        self.transport.queue_unset(TX_QUEUE);
        self.transport.queue_unset(RX_QUEUE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hal::fake::FakeHal;
    use crate::transport::fake::{FakeTransport, QueueStatus, State};
    use alloc::sync::Arc;
    use core::ptr::NonNull;
    use std::sync::Mutex;

    fn transport(device_features: u64) -> (FakeTransport<()>, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport::<()> {
            device_type: DeviceType::Network,
            max_queue_size: QUEUE_SIZE as u32,
            device_features,
            config_space: NonNull::dangling(),
            state: state.clone(),
        };
        (transport, state)
    }

    #[test]
    fn negotiates_platform_dma_when_required() {
        // QEMU enables iommu_platform for confidential guests. Bit 33 is
        // mandatory there even though the SVSM MMIO device uses shared GPAs.
        let offered = (1u64 << 32) | (1u64 << 33);
        let (transport, state) = transport(offered);
        let _driver = VirtIONetTx::<FakeHal, _>::new(transport).unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.driver_features, offered);
        assert!(
            state
                .status
                .contains(DeviceStatus::FEATURES_OK | DeviceStatus::DRIVER_OK)
        );
    }

    #[test]
    fn retains_packet_until_device_completes() {
        let (transport, state) = transport(Features::VERSION_1.bits());
        let mut driver = VirtIONetTx::<FakeHal, _>::new(transport).unwrap();
        let mut frame = [0x55; 98];
        driver.send(&frame).unwrap();
        frame.fill(0x77);
        assert!(!driver.poll_complete().unwrap());
        assert_eq!(driver.send(&frame), Err(Error::NotReady));
        let packet = state
            .lock()
            .unwrap()
            .read_from_queue::<QUEUE_SIZE>(TX_QUEUE);
        assert_eq!(&packet[..HEADER_SIZE], &[0; HEADER_SIZE]);
        assert_eq!(&packet[HEADER_SIZE..], &[0x55; 98]);
        assert!(driver.poll_complete().unwrap());
        driver.send(&frame).unwrap();
        let packet = state
            .lock()
            .unwrap()
            .read_from_queue::<QUEUE_SIZE>(TX_QUEUE);
        assert_eq!(&packet[HEADER_SIZE..], &frame);
        assert!(driver.poll_complete().unwrap());
        drop(driver);
        assert_eq!(state.lock().unwrap().queues[TX_QUEUE as usize].size, 0);
    }
}
