// SPDX-License-Identifier: MIT OR Apache-2.0

//! Nonblocking TCP observation export through a dedicated VirtIO MMIO NIC.

use crate::error::SvsmError;
use crate::locking::SpinLock;
use crate::mm::GlobalRangeGuard;
use crate::virtio::VirtioError;
use crate::virtio::hal::SvsmHal;
use crate::virtio::mmio::MmioSlots;
use virtio_drivers::device::net_tx::VirtIONetTx;
use virtio_drivers::transport::{DeviceType, mmio::MmioTransport};

use super::{tcp_telemetry, tcp_udp};

struct Device {
    tx: VirtIONetTx<SvsmHal, MmioTransport<SvsmHal>>,
    _mmio: GlobalRangeGuard,
    failed: bool,
}

static DEVICE: SpinLock<Option<Device>> = SpinLock::new(None);

pub fn initialize(slots: &mut MmioSlots) -> Result<(), SvsmError> {
    let Some(slot) = slots.pop_slot(DeviceType::Network) else {
        log::warn!("TCP UDP: no dedicated virtio-net MMIO device found");
        return Ok(());
    };
    let tx = VirtIONetTx::new(slot.transport).map_err(|_| VirtioError::InvalidDevice)?;
    *DEVICE.lock() = Some(Device {
        tx,
        _mmio: slot.mmio_range,
        failed: false,
    });
    log::info!("TCP UDP: 10.0.2.15:4050 -> 10.0.2.2:4050");
    Ok(())
}

/// Called after releasing the guest VMSA. Never waits for the device or host.
pub fn flush() {
    let mut device = DEVICE.lock();
    let Some(device) = device.as_mut() else {
        return;
    };
    if device.failed {
        return;
    }
    for _ in 0..32 {
        let result = device.tx.poll_complete().and_then(|ready| {
            if !ready {
                return Ok(false);
            }
            let Some(frame) = tcp_telemetry::pop_frame() else {
                return Ok(false);
            };
            device.tx.send(&tcp_udp::encode(&frame))?;
            Ok(true)
        });
        match result {
            Ok(true) => (),
            Ok(false) => break,
            Err(error) => {
                device.failed = true;
                log::warn!("TCP UDP: transmit stopped: {error:?}");
                break;
            }
        }
    }
}
