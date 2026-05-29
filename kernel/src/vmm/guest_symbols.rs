// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Shinya Murakami <sny430@gmail.com>

use crate::address::{Address, PhysAddr, VirtAddr};
use crate::cpu::control_regs::{CR0Flags, CR4Flags};
use crate::cpu::efer::EFERFlags;
use crate::locking::SpinLock;
use crate::mm::guestmem::copy_slice_from_guest;
use crate::mm::pagetable::{PTEntry, PagingMode};
use crate::types::{PAGE_SIZE, PAGE_SIZE_1G, PAGE_SIZE_2M};
use core::cmp::min;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use cpuarch::vmsa::VMSA;

const LINUX_BANNER_PREFIX: &[u8] = b"Linux version";
const LINUX_BANNER_MAX_LEN: usize = 256;
const LINUX_BANNER_PATTERN_BASE: u64 = 0xffff_ffff_0009_86c0;
const LINUX_BANNER_PATTERN_STEP: u64 = 1 << 20;
const LINUX_BANNER_PATTERN_MAX: u16 = 0x0fff;
pub const TCP_HASHINFO_OFFSET: u64 = 0x011b_1400;

static LINUX_BANNER_SCAN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static LINUX_BANNER_GVA: AtomicU64 = AtomicU64::new(0);
static LINUX_BANNER_GPA: AtomicU64 = AtomicU64::new(0);
static TCP_HASHINFO_GVA: AtomicU64 = AtomicU64::new(0);
static TCP_HASHINFO_GPA: AtomicU64 = AtomicU64::new(0);
static LINUX_BANNER_CACHE: SpinLock<BannerCache> = SpinLock::new(BannerCache::new());

#[derive(Clone, Copy, Debug)]
pub struct GuestSymbolContext {
    cr0: u64,
    cr3: u64,
    cr4: u64,
    efer: u64,
}

impl GuestSymbolContext {
    pub fn from_vmsa(vmsa: &VMSA) -> Self {
        Self {
            cr0: vmsa.cr0,
            cr3: vmsa.cr3,
            cr4: vmsa.cr4,
            efer: vmsa.efer,
        }
    }
}

struct BannerCache {
    bytes: [u8; LINUX_BANNER_MAX_LEN],
    len: usize,
}

impl BannerCache {
    const fn new() -> Self {
        Self {
            bytes: [0; LINUX_BANNER_MAX_LEN],
            len: 0,
        }
    }

    fn set(&mut self, src: &[u8]) {
        let len = min(src.len(), LINUX_BANNER_MAX_LEN);
        self.bytes[..len].copy_from_slice(&src[..len]);
        self.len = len;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BannerSnapshot {
    pub banner_gva: u64,
    pub banner_gpa: u64,
    pub tcp_hashinfo_gva: u64,
    pub tcp_hashinfo_gpa: u64,
    bytes: [u8; LINUX_BANNER_MAX_LEN],
    len: usize,
}

impl BannerSnapshot {
    pub fn banner_str(&self) -> &str {
        if self.len == 0 {
            "<unknown>"
        } else {
            core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("<non-utf8>")
        }
    }
}

fn is_printable_ascii(byte: u8) -> bool {
    matches!(byte, 0x20..=0x7e | b'\n' | b'\r' | b'\t')
}

fn linux_banner_len(buf: &[u8]) -> Option<usize> {
    if !buf.starts_with(LINUX_BANNER_PREFIX) {
        return None;
    }

    let mut len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }

    if !buf[..len].iter().all(|&b| is_printable_ascii(b)) {
        return None;
    }

    while len > 0 && matches!(buf[len - 1], b'\n' | b'\r') {
        len -= 1;
    }

    if len == 0 { None } else { Some(len) }
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

fn read_guest_slice(ctx: GuestSymbolContext, gva: VirtAddr, buf: &mut [u8]) -> Option<()> {
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

pub fn maybe_resolve_linux_banner(ctx: GuestSymbolContext) {
    if LINUX_BANNER_GVA.load(Ordering::Acquire) != 0 {
        return;
    }

    if LINUX_BANNER_SCAN_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    for idx in 0..=LINUX_BANNER_PATTERN_MAX {
        let gva_val = LINUX_BANNER_PATTERN_BASE + (u64::from(idx) * LINUX_BANNER_PATTERN_STEP);
        let gva = VirtAddr::from(gva_val as usize);
        let Some(gpa) = translate_gva(ctx, gva) else {
            continue;
        };

        let mut buf = [0u8; LINUX_BANNER_MAX_LEN];
        if read_guest_slice(ctx, gva, &mut buf).is_none() {
            continue;
        }

        let Some(len) = linux_banner_len(&buf) else {
            continue;
        };

        let mut tcp_hashinfo_gva = 0;
        let mut tcp_hashinfo_gpa = 0;
        if let Some(tcp_gva_val) = gva_val.checked_add(TCP_HASHINFO_OFFSET) {
            tcp_hashinfo_gva = tcp_gva_val;
            if let Some(tcp_gpa) = translate_gva(ctx, VirtAddr::from(tcp_gva_val as usize)) {
                tcp_hashinfo_gpa = tcp_gpa.bits() as u64;
            }
        }

        {
            let mut cache = LINUX_BANNER_CACHE.lock();
            cache.set(&buf[..len]);
        }

        TCP_HASHINFO_GVA.store(tcp_hashinfo_gva, Ordering::Relaxed);
        TCP_HASHINFO_GPA.store(tcp_hashinfo_gpa, Ordering::Relaxed);
        LINUX_BANNER_GPA.store(gpa.bits() as u64, Ordering::Relaxed);
        LINUX_BANNER_GVA.store(gva_val, Ordering::Release);

        let snapshot = banner_snapshot();
        log::info!(
            "guest symbols: linux_banner_gva={:#x} linux_banner_gpa={:#x} tcp_hashinfo_gva={:#x} tcp_hashinfo_gpa={:#x} linux_banner=\"{}\"",
            snapshot.banner_gva,
            snapshot.banner_gpa,
            snapshot.tcp_hashinfo_gva,
            snapshot.tcp_hashinfo_gpa,
            snapshot.banner_str()
        );
        break;
    }

    LINUX_BANNER_SCAN_IN_PROGRESS.store(false, Ordering::Release);
}

pub fn banner_snapshot() -> BannerSnapshot {
    let cache = LINUX_BANNER_CACHE.lock();
    BannerSnapshot {
        banner_gva: LINUX_BANNER_GVA.load(Ordering::Relaxed),
        banner_gpa: LINUX_BANNER_GPA.load(Ordering::Relaxed),
        tcp_hashinfo_gva: TCP_HASHINFO_GVA.load(Ordering::Relaxed),
        tcp_hashinfo_gpa: TCP_HASHINFO_GPA.load(Ordering::Relaxed),
        bytes: cache.bytes,
        len: cache.len,
    }
}

pub fn tcp_hashinfo_gva() -> Option<VirtAddr> {
    let gva = TCP_HASHINFO_GVA.load(Ordering::Acquire);
    if gva == 0 {
        None
    } else {
        Some(VirtAddr::from(gva as usize))
    }
}
