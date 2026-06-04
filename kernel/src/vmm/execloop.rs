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
use crate::vmm::guest_symbols::{GuestSymbolContext, maybe_resolve_linux_banner};
use crate::vmm::tcp_log::maybe_log_tcp_connections;

use core::ops::DerefMut;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use cpuarch::vmsa::GuestVMExit;

const GUEST_EXIT_LOG_INTERVAL_SECS: u64 = 30;
const KVM_CPUID_SIGNATURE: u32 = 0x4000_0000;
const KVM_CPUID_TSC_FREQUENCY: u32 = KVM_CPUID_SIGNATURE | 0x10;

static GUEST_EXIT_COUNT: AtomicU64 = AtomicU64::new(0);
static GUEST_EXIT_LAST_LOG_TSC: AtomicU64 = AtomicU64::new(0);
static GUEST_ENTRY_LOGGED_CPUS: AtomicU64 = AtomicU64::new(0);
static GUEST_RETURN_LOGGED_CPUS: AtomicU64 = AtomicU64::new(0);
static TSC_HZ_LOGGED: AtomicBool = AtomicBool::new(false);
static TSC_HZ_PROBED: AtomicBool = AtomicBool::new(false);
static TSC_HZ_CACHE: AtomicU64 = AtomicU64::new(0);

fn log_once_for_cpu(mask: &AtomicU64, cpu_index: usize) -> bool {
    if cpu_index >= u64::BITS as usize {
        return false;
    }

    let bit = 1_u64 << cpu_index;
    (mask.fetch_or(bit, Ordering::AcqRel) & bit) == 0
}

fn tsc_hz() -> u64 {
    if TSC_HZ_PROBED.load(Ordering::Acquire) {
        return TSC_HZ_CACHE.load(Ordering::Relaxed);
    }

    let max_leaf = SVSM_PLATFORM.cpuid(0, 0).map(|r| r.eax).unwrap_or(0);
    let max_hypervisor_leaf = SVSM_PLATFORM
        .cpuid(KVM_CPUID_SIGNATURE, 0)
        .map(|r| r.eax)
        .unwrap_or(0);
    let mut hz = 0;
    let mut leaf15_regs = None;
    let mut leaf16_regs = None;
    let mut leaf40000010_regs = None;

    if max_leaf >= 0x15 {
        if let Some(leaf) = SVSM_PLATFORM.cpuid(0x15, 0) {
            leaf15_regs = Some((leaf.eax, leaf.ebx, leaf.ecx, leaf.edx));
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
            leaf16_regs = Some((leaf.eax, leaf.ebx, leaf.ecx, leaf.edx));
            let mhz = (leaf.eax & 0xffff) as u64;
            if mhz != 0 {
                hz = mhz.saturating_mul(1_000_000);
            }
        }
    }

    if hz == 0 && max_hypervisor_leaf >= KVM_CPUID_TSC_FREQUENCY {
        if let Some(leaf) = SVSM_PLATFORM.cpuid(KVM_CPUID_TSC_FREQUENCY, 0) {
            leaf40000010_regs = Some((leaf.eax, leaf.ebx, leaf.ecx, leaf.edx));
            let khz = leaf.eax as u64;
            if khz != 0 {
                hz = khz.saturating_mul(1_000);
            }
        }
    }

    TSC_HZ_CACHE.store(hz, Ordering::Relaxed);
    TSC_HZ_PROBED.store(true, Ordering::Release);

    if !TSC_HZ_LOGGED.swap(true, Ordering::AcqRel) {
        log::info!(
            "guest exit heartbeat: tsc_hz={} max_cpuid_leaf={:#x} max_hypervisor_leaf={:#x} cpuid_15={:?} cpuid_16={:?} cpuid_40000010={:?}",
            hz,
            max_leaf,
            max_hypervisor_leaf,
            leaf15_regs,
            leaf16_regs,
            leaf40000010_regs
        );
    }

    hz
}

fn maybe_log_guest_exit(
    cpu_index: usize,
    exit_code: GuestVMExit,
    guest_symbol_ctx: GuestSymbolContext,
) {
    let count = GUEST_EXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let hz = tsc_hz();
    if hz == 0 {
        if count <= 8 || count.is_power_of_two() {
            maybe_resolve_linux_banner(guest_symbol_ctx);
            maybe_log_tcp_connections(guest_symbol_ctx);
            log::info!(
                "guest exit heartbeat: count={} cpu={} exit_code={:?} tsc_hz=unknown",
                count,
                cpu_index,
                exit_code
            );
        }
        return;
    }

    let interval = hz.saturating_mul(GUEST_EXIT_LOG_INTERVAL_SECS);
    let now = rdtsc();
    let last = GUEST_EXIT_LAST_LOG_TSC.load(Ordering::Relaxed);

    if last != 0 && now.wrapping_sub(last) < interval {
        return;
    }

    if GUEST_EXIT_LAST_LOG_TSC
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        maybe_resolve_linux_banner(guest_symbol_ctx);
        maybe_log_tcp_connections(guest_symbol_ctx);
        log::info!(
            "guest exit heartbeat: count={} cpu={} exit_code={:?} tsc={:#x} tsc_hz={}",
            count,
            cpu_index,
            exit_code,
            now,
            hz
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
    let cpu_index = cpu.get_cpu_index();

    // If no VMSA or CAA are configured, then the guest cannot be entered.
    if cpu.update_guest_mappings().is_err() {
        log::info!("enter_guest debug: cpu={} has no guest mappings", cpu_index);
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

        if log_once_for_cpu(&GUEST_ENTRY_LOGGED_CPUS, cpu_index) {
            log::info!(
                "enter_guest debug: cpu={} switching to guest vmpl={}",
                cpu_index,
                GUEST_VMPL
            );
        }

        switch_to_vmpl(GUEST_VMPL as u32);

        // Interrupts can safely be reenabled once the guest has returned to the
        // SVSM.
        drop(guard);

        // If no mapping exists, then indicate to the caller that the guest
        // exited with no valid mappings.
        if cpu.update_guest_mappings().is_err() {
            log::info!(
                "enter_guest debug: cpu={} returned without guest mappings",
                cpu_index
            );
            return GuestExitMessage::NoMappings;
        }

        // Obtain a reference to the VMSA just long enough to extract the
        // request parameters.
        {
            let mut vmsa_ref = cpu.guest_vmsa_ref();
            let vmsa = vmsa_ref.vmsa();
            let exit_code = vmsa.guest_exit_code;
            let guest_symbol_ctx = GuestSymbolContext::from_vmsa(vmsa);

            if log_once_for_cpu(&GUEST_RETURN_LOGGED_CPUS, cpu_index) {
                log::info!(
                    "enter_guest debug: cpu={} returned from guest exit_code={:?}",
                    cpu_index,
                    exit_code
                );
            }

            // Clear EFER.SVME in guest VMSA.
            vmsa.disable();

            cpu.ai_handle_intercepts(vmsa);
            maybe_log_guest_exit(cpu_index, exit_code, guest_symbol_ctx);

            if let Some(msg) = get_svsm_request_message(vmsa_ref.deref_mut()) {
                return msg;
            }
        }
    }
}
