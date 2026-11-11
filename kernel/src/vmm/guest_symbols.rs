// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Shinya Murakami <sny430@gmail.com>

use crate::address::{Address, PhysAddr, VirtAddr};
use crate::cpu::control_regs::{CR0Flags, CR4Flags};
use crate::cpu::efer::EFERFlags;
use crate::mm::guestmem::copy_slice_from_guest;
use crate::mm::pagetable::{PTEntry, PagingMode};
use crate::types::{PAGE_SIZE, PAGE_SIZE_1G, PAGE_SIZE_2M};
use core::cmp::min;
use core::sync::atomic::{AtomicBool, Ordering};
use cpuarch::vmsa::VMSA;

static SYMBOLS_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
pub struct GuestSymbolContext {
    cr0: u64,
    cr3: u64,
    cr4: u64,
    efer: u64,
    lstar: u64,
}

impl GuestSymbolContext {
    pub fn from_vmsa(vmsa: &VMSA) -> Self {
        Self {
            cr0: vmsa.cr0,
            cr3: vmsa.cr3,
            cr4: vmsa.cr4,
            efer: vmsa.efer,
            lstar: vmsa.lstar,
        }
    }
}

fn page_table_index(gva: VirtAddr, level: usize) -> usize {
    (gva.bits() >> (12 + level * 9)) & 0x1ff
}

fn cr3_page_table_addr(cr3: u64) -> PhysAddr {
    PTEntry::from_raw(cr3 & !0xfff).address()
}

fn read_guest_pte(gpa: PhysAddr) -> Option<PTEntry> {
    let mut bytes = [0u8; 8];
    copy_slice_from_guest(gpa, &mut bytes).ok()?;
    Some(PTEntry::from_raw(u64::from_le_bytes(bytes)))
}

fn gva_page_offset(gva: VirtAddr, page_size: usize) -> usize {
    gva.bits() & (page_size - 1)
}

fn translate_gva(ctx: GuestSymbolContext, gva: VirtAddr) -> Option<PhysAddr> {
    let cr0 = CR0Flags::from_bits_truncate(ctx.cr0);
    if !cr0.contains(CR0Flags::PG) {
        return Some(PhysAddr::from(gva.bits()));
    }

    let cr4 = CR4Flags::from_bits_truncate(ctx.cr4);
    let efer = EFERFlags::from_bits_truncate(ctx.efer);
    let paging_mode = PagingMode::new(efer, cr0, cr4);
    let top_level = match paging_mode {
        PagingMode::PML4 => 3,
        PagingMode::PML5 => 4,
        _ => return None,
    };

    let mut table_gpa = cr3_page_table_addr(ctx.cr3);
    for level in (0..=top_level).rev() {
        let entry_gpa = table_gpa + page_table_index(gva, level) * core::mem::size_of::<u64>();
        let entry = read_guest_pte(entry_gpa)?;
        if !entry.present() || entry.has_reserved_bits(paging_mode, level) {
            return None;
        }

        if level == 0 {
            return Some(entry.address() + gva_page_offset(gva, PAGE_SIZE));
        }

        if entry.huge() {
            let page_size = match level {
                1 => PAGE_SIZE_2M,
                2 => PAGE_SIZE_1G,
                _ => return None,
            };
            return Some(entry.address() + gva_page_offset(gva, page_size));
        }

        table_gpa = entry.address();
    }

    None
}

pub fn read_guest_virt_slice(ctx: GuestSymbolContext, gva: VirtAddr, buf: &mut [u8]) -> Option<()> {
    let mut remaining = buf.len();
    let mut offset = 0;
    let mut current_gva = gva;

    while remaining > 0 {
        let gpa = translate_gva(ctx, current_gva)?;
        let chunk = min(remaining, PAGE_SIZE - current_gva.page_offset());
        copy_slice_from_guest(gpa, &mut buf[offset..offset + chunk]).ok()?;

        remaining -= chunk;
        offset += chunk;
        current_gva = current_gva + chunk;
    }

    Some(())
}

/// Resolve against this exit's LSTAR, so an uninitialized vCPU or a reboot
/// cannot reuse a previously cached address from another guest context.
pub fn tcp_hashinfo_gva(ctx: GuestSymbolContext) -> Option<VirtAddr> {
    let cr0 = CR0Flags::from_bits_truncate(ctx.cr0);
    let efer = EFERFlags::from_bits_truncate(ctx.efer);
    if !cr0.contains(CR0Flags::PG) || !efer.contains(EFERFlags::LMA | EFERFlags::SCE) {
        return None;
    }

    let (reference_slide, tcp_gva) = relocate_tcp_hashinfo(
        ctx.lstar,
        ENTRY_SYSCALL_64_REFERENCE,
        TCP_HASHINFO_REFERENCE,
    )?;
    let gva = VirtAddr::from(tcp_gva as usize);

    // A user CR3 with KPTI may not map kernel data. Retry on a later exit
    // instead of remembering a failed translation or a CR3-specific GPA.
    let gpa = translate_gva(ctx, gva)?;
    if !SYMBOLS_LOGGED.swap(true, Ordering::Relaxed) {
        log::info!(
            "guest symbols: lstar={:#x} reference_slide={:+} tcp_hashinfo_gva={:#x} tcp_hashinfo_gpa={:#x}",
            ctx.lstar,
            reference_slide,
            tcp_gva,
            gpa.bits()
        );
    }
    Some(gva)
}

// Addresses reported from /proc/kallsyms in the same boot of the target guest.
// Their difference (0x0154_9a40) is invariant under KASLR for this kernel build.
// These are runtime reference addresses, NOT assumed to be link-time addresses.
const ENTRY_SYSCALL_64_REFERENCE: u64 = 0xffff_ffff_8220_0080;
const TCP_HASHINFO_REFERENCE: u64 = 0xffff_ffff_8374_9ac0;

/// Native x86-64 Linux programs LSTAR with entry_SYSCALL_64. Apply its change
/// from the reference boot to tcp_hashinfo. A later boot can relocate either
/// above or below the reference, so the displacement must be signed.
/// The result is relative to the reference boot, not the absolute KASLR slide.
fn relocate_tcp_hashinfo(lstar: u64, entry: u64, tcp: u64) -> Option<(i64, u64)> {
    const KERNEL_IMAGE_START: u64 = 0xffff_ffff_8000_0000;
    if lstar < KERNEL_IMAGE_START || entry < KERNEL_IMAGE_START || tcp < KERNEL_IMAGE_START {
        return None;
    }
    let slide = (lstar as i64).checked_sub(entry as i64)?;
    if slide & (PAGE_SIZE_2M as i64 - 1) != 0 {
        return None;
    }
    let tcp_gva = tcp.checked_add_signed(slide)?;
    (tcp_gva >= KERNEL_IMAGE_START).then_some((slide, tcp_gva))
}

#[cfg(test)]
mod tests {
    use super::{ENTRY_SYSCALL_64_REFERENCE, TCP_HASHINFO_REFERENCE, relocate_tcp_hashinfo};

    // Synthetic link addresses: these are not a profile for a guest kernel.
    const ENTRY: u64 = 0xffff_ffff_8100_0100;
    const TCP: u64 = 0xffff_ffff_8300_0200;

    #[test]
    fn resolves_with_and_without_reference_displacement() {
        assert_eq!(relocate_tcp_hashinfo(ENTRY, ENTRY, TCP), Some((0, TCP)));
        assert_eq!(
            relocate_tcp_hashinfo(0xffff_ffff_a100_0100, ENTRY, TCP),
            Some((0x2000_0000, 0xffff_ffff_a300_0200))
        );
    }

    #[test]
    fn resolves_reported_guest_across_reboots() {
        for (lstar, expected) in [
            (0xffff_ffff_8220_0080, (0, 0xffff_ffff_8374_9ac0)),
            (0xffff_ffff_8240_0080, (0x20_0000, 0xffff_ffff_8394_9ac0)),
            (0xffff_ffff_8200_0080, (-0x20_0000, 0xffff_ffff_8354_9ac0)),
        ] {
            assert_eq!(
                relocate_tcp_hashinfo(lstar, ENTRY_SYSCALL_64_REFERENCE, TCP_HASHINFO_REFERENCE),
                Some(expected)
            );
        }
    }

    #[test]
    fn rejects_uninitialized_and_non_kernel_lstar() {
        for lstar in [0, 0x100, 0x7fff_8100_0100, 0xffff_8000_8100_0100] {
            assert_eq!(relocate_tcp_hashinfo(lstar, ENTRY, TCP), None);
        }
    }

    #[test]
    fn rejects_incompatible_entry_offsets() {
        for lstar in [ENTRY + 1, ENTRY + 0x1000] {
            assert_eq!(relocate_tcp_hashinfo(lstar, ENTRY, TCP), None);
        }
    }

    #[test]
    fn rejects_invalid_link_addresses_and_overflow() {
        assert_eq!(relocate_tcp_hashinfo(ENTRY, 0, TCP), None);
        assert_eq!(relocate_tcp_hashinfo(ENTRY, ENTRY, 0), None);
        assert_eq!(
            relocate_tcp_hashinfo(ENTRY - 0x20_0000, ENTRY, 0xffff_ffff_8000_0000),
            None
        );
        assert_eq!(
            relocate_tcp_hashinfo(ENTRY + 0x20_0000, ENTRY, u64::MAX),
            None
        );
    }

    #[test]
    fn resolves_each_context_without_reusing_a_slide() {
        for (lstar, expected) in [
            (
                0xffff_ffff_8120_0100,
                Some((0x20_0000, 0xffff_ffff_8320_0200)),
            ),
            (0, None),
            (
                0xffff_ffff_8140_0100,
                Some((0x40_0000, 0xffff_ffff_8340_0200)),
            ),
        ] {
            assert_eq!(relocate_tcp_hashinfo(lstar, ENTRY, TCP), expected);
        }
    }
}
