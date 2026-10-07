// SPDX-License-Identifier: MIT OR Apache-2.0

//! Nonblocking TCP observation export through a dedicated VirtIO MMIO NIC.

use crate::error::SvsmError;
use crate::locking::SpinLock;
use crate::mm::GlobalRangeGuard;
use crate::virtio::VirtioError;
use crate::virtio::hal::SvsmHal;
use crate::virtio::mmio::MmioSlots;
use virtio_drivers::device::net_tx::VirtIONetTx;
use virtio_drivers::transport::{DeviceType, Transport, mmio::MmioTransport};

use super::{tcp_telemetry, tcp_udp};

struct Device {
    tx: VirtIONetTx<SvsmHal, MmioTransport<SvsmHal>>,
    _mmio: GlobalRangeGuard,
    failed: bool,
    first_submitted: bool,
    first_completed: bool,
}

static DEVICE: SpinLock<Option<Device>> = SpinLock::new(None);

pub fn initialize(slots: &mut MmioSlots) -> Result<(), SvsmError> {
    let Some(mut slot) = slots.pop_slot(DeviceType::Network) else {
        log::warn!("TCP UDP: no dedicated virtio-net MMIO device found");
        return Ok(());
    };
    log::info!(
        "TCP UDP: MMIO={:?} device_features={:#018x}",
        slot.transport.version(),
        slot.transport.read_device_features(),
    );
    let tx = VirtIONetTx::new(slot.transport).map_err(|error| {
        log::error!("TCP UDP: device initialization failed: {error:?}");
        VirtioError::InvalidDevice
    })?;
    *DEVICE.lock() = Some(Device {
        tx,
        _mmio: slot.mmio_range,
        failed: false,
        first_submitted: false,
        first_completed: false,
    });
    log::info!("TCP UDP: 10.0.2.15:4050 -> 10.0.2.2:4050");
    log::info!("TCP UDP: device initialized; waiting for TCP observations");
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
            if device.first_submitted && !device.first_completed {
                device.first_completed = true;
                log::info!("TCP UDP: first transmit completed by device");
            }
            let Some(frame) = tcp_telemetry::pop_frame() else {
                return Ok(false);
            };
            device.tx.send(&tcp_udp::encode(&frame))?;
            if !device.first_submitted {
                device.first_submitted = true;
                let sequence = u64::from_be_bytes(frame[8..16].try_into().unwrap());
                log::info!("TCP UDP: first record submitted; sequence={sequence}");
            }
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
