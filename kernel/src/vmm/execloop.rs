// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use super::{GuestExitMessage, GuestRegister, set_guest_register};
use crate::cpu::msr::rdtsc;
use crate::cpu::percpu::{GuestVmsaRef, this_cpu};
use crate::cpu::{IrqGuard, flush_tlb_global_sync};
use crate::mm::GuestPtr;
use crate::platform::SVSM_PLATFORM;
use crate::protocols::RequestParams;
use crate::protocols::errors::SvsmReqError;
use crate::requests::SvsmCaa;
use crate::sev::ghcb::switch_to_vmpl;
use crate::sev::vmsa::VMSAControl;
use crate::types::GUEST_VMPL;

use core::ops::DerefMut;
use core::sync::atomic::{AtomicU64, Ordering};
use cpuarch::vmsa::GuestVMExit;

const GUEST_EXIT_LOG_INTERVAL_SECS: u64 = 30;
static GUEST_EXIT_LAST_LOG_TSC: AtomicU64 = AtomicU64::new(0);
static TSC_HZ_CACHE: AtomicU64 = AtomicU64::new(0);

fn tsc_hz() -> u64 {
    let cached = TSC_HZ_CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }

    let max_leaf = SVSM_PLATFORM.cpuid(0, 0).map(|r| r.eax).unwrap_or(0);
    let mut hz = 0;

    if max_leaf >= 0x15 {
        if let Some(leaf) = SVSM_PLATFORM.cpuid(0x15, 0) {
            let denom = leaf.eax as u64;
            let numer = leaf.ebx as u64;
            let crystal = leaf.ecx as u64;
            if denom != 0 && numer != 0 && crystal != 0 {
                hz = crystal.saturating_mul(numer) / denom;
            }
        }
    }

    if hz == 0 && max_leaf >= 0x16 {
        if let Some(leaf) = SVSM_PLATFORM.cpuid(0x16, 0) {
            let mhz = (leaf.eax & 0xffff) as u64;
            if mhz != 0 {
                hz = mhz.saturating_mul(1_000_000);
            }
        }
    }

    if hz != 0 {
        TSC_HZ_CACHE.store(hz, Ordering::Relaxed);
    }

    hz
}

fn maybe_log_guest_exit(cpu_index: usize, exit_code: GuestVMExit) {
    let hz = tsc_hz();
    if hz == 0 {
        return;
    }

    let interval = hz.saturating_mul(GUEST_EXIT_LOG_INTERVAL_SECS);
    let now = rdtsc();
    let last = GUEST_EXIT_LAST_LOG_TSC.load(Ordering::Relaxed);

    if now.wrapping_sub(last) < interval {
        return;
    }

    if GUEST_EXIT_LAST_LOG_TSC
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        log::info!(
            "guest exit periodic log: cpu={} exit_code={:?} tsc={:#x}",
            cpu_index,
            exit_code,
            now
        );
    }
}

fn get_and_clear_caa_request_flag(vmsa_ref: &GuestVmsaRef) -> Result<bool, SvsmReqError> {
    if let Some(caa) = vmsa_ref.caa() {
        let calling_area = GuestPtr::<SvsmCaa>::from(caa);
        // SAFETY: guest vmsa and ca are always validated before beeing updated
        // (core_remap_ca(), core_create_vcpu() or prepare_fw_launch()) so
        // they're safe to use.
        let caa = unsafe { calling_area.read()? };

        let caa_serviced = caa.serviced();

        // SAFETY: guest vmsa is always validated before beeing updated
        // (core_remap_ca() or core_create_vcpu()) so it's safe to use.
        unsafe {
            calling_area.write(caa_serviced)?;
        }

        Ok(caa.call_pending())
    } else {
        Ok(false)
    }
}

fn get_svsm_request_message(vmsa_ref: &mut GuestVmsaRef) -> Option<GuestExitMessage> {
    let vmsa = vmsa_ref.vmsa();

    // Ignore guest exits that were not initiated by VMFEXIT.
    if !matches!(vmsa.guest_exit_code, GuestVMExit::VMGEXIT) {
        return None;
    }
    let rax = vmsa.rax;
    let protocol = (rax >> 32) as u32;
    let request = (rax & 0xffff_ffff) as u32;
    let params = RequestParams::from_vmsa(vmsa);

    match get_and_clear_caa_request_flag(vmsa_ref) {
        Ok(pending) => {
            if pending {
                return Some(GuestExitMessage::Svsm((protocol, request, params)));
            }
        }
        Err(SvsmReqError::RequestError(code)) => {
            log::debug!("Soft error handling protocol {protocol} request {request}: {code:?}");
        }
        Err(SvsmReqError::FatalError(err)) => {
            panic!(
                "Fatal error handling core protocol request {}: {:?}",
                request, err
            );
        }
    }

    None
}

pub fn enter_guest(mut regs: &[GuestRegister]) -> GuestExitMessage {
    let cpu = this_cpu();

    // If no VMSA or CAA are configured, then the guest cannot be entered.
    if cpu.update_guest_mappings().is_err() {
        return GuestExitMessage::NoMappings;
    }

    loop {
        // Modify guest registers before disabling interrupts.
        let mut vmsa_ref = cpu.guest_vmsa_ref();
        let caa_addr = vmsa_ref.caa();
        let vmsa = vmsa_ref.vmsa();

        for reg in regs {
            set_guest_register(vmsa, reg);
        }

        // Ensure that no further register modification occurs if the loop
        // restarts.
        regs = &[];

        // No interrupts may be processed once guest APIC state is updated,
        // since handling an interrupt may modify the guest APIC state
        // calculations, which could cause state corruption.  If interrupts are
        // disabled, then any additional guest APIC updates generated by the
        // host will block the VMPL transition and permit reevaluation of
        // guest APIC state.
        let guard = IrqGuard::new();

        // Update APIC interrupt emulation state if required.
        cpu.update_apic_emulation(vmsa, caa_addr);

        // Make VMSA runnable again by setting EFER.SVME.
        vmsa.enable();

        // The VMSA reference must not be held when the guest is running.
        drop(vmsa_ref);

        flush_tlb_global_sync();

        switch_to_vmpl(GUEST_VMPL as u32);

        // Interrupts can safely be reenabled once the guest has returned to the
        // SVSM.
        drop(guard);

        // If no mapping exists, then indicate to the caller that the guest
        // exited with no valid mappings.
        if cpu.update_guest_mappings().is_err() {
            return GuestExitMessage::NoMappings;
        }

        // Obtain a reference to the VMSA just long enough to extract the
        // request parameters.
        {
            let mut vmsa_ref = cpu.guest_vmsa_ref();
            let vmsa = vmsa_ref.vmsa();

            // Clear EFER.SVME in guest VMSA.
            vmsa.disable();

            cpu.ai_handle_intercepts(vmsa);
            maybe_log_guest_exit(cpu.get_cpu_index(), vmsa.guest_exit_code);

            if let Some(msg) = get_svsm_request_message(vmsa_ref.deref_mut()) {
                return msg;
            }
        }
    }
}
