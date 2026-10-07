// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experimental socket-observation protocol: call 0 drains a batch.

extern crate alloc;
use crate::address::{Address, PhysAddr};
use crate::mm::guestmem::copy_slice_to_guest;
use crate::protocols::{RequestParams, errors::SvsmReqError};
use crate::types::PAGE_SIZE;
use crate::vmm::{tcp_event::FRAME_SIZE, tcp_telemetry::pop_frame};
use alloc::vec::Vec;

/// Experimental local protocol number, not an assigned SVSM standard ID.
pub const TCP_TELEMETRY_PROTOCOL: u32 = 0x8000_0054;

/// RCX: page-aligned guest-private output GPA; RDX: capacity, at most one
/// page. Returns bytes copied in RCX. An empty queue returns zero bytes.
pub fn request(request: u32, params: &mut RequestParams) -> Result<(), SvsmReqError> {
    if request != 0 {
        return Err(SvsmReqError::unsupported_call());
    }
    let destination = PhysAddr::from(params.rcx);
    let capacity = usize::try_from(params.rdx).map_err(|_| SvsmReqError::invalid_parameter())?;
    if destination.is_null()
        || destination.bits() % PAGE_SIZE != 0
        || !(FRAME_SIZE..=PAGE_SIZE).contains(&capacity)
    {
        return Err(SvsmReqError::invalid_parameter());
    }
    let mut batch = Vec::with_capacity(capacity);
    while batch.len() + FRAME_SIZE <= capacity {
        let Some(frame) = pop_frame() else {
            break;
        };
        batch.extend_from_slice(&frame);
    }
    if !batch.is_empty() {
        copy_slice_to_guest(&batch, destination).map_err(|_| SvsmReqError::invalid_address())?;
    }
    params.rcx = batch.len() as u64;
    Ok(())
}
