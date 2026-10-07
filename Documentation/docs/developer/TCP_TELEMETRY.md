# TCP telemetry directly from SVSM over UDP

This branch starts at `sock-trawl` (`990fc4fe`). SVSM emits the same 56-byte
CTCP connection records as `virtio-vsock`, through a dedicated virtio-net
MMIO device. The guest Linux network stack is not involved.

The repository's `virtio-drivers` crate supplies MMIO, virtqueues, and the
SVSM DMA/shared-memory HAL. This branch adds a small transmit-only net
driver and Ethernet/IPv4/UDP framing. It does not add smoltcp or another
network stack dependency.

Build with `make TCP_LOG_MODE=net`. With the repository launcher, add
`--tcp-net`. With the provided `run.sh`, append `x-svsm-virtio-mmio=on` to
the existing `-machine` argument:

```sh
-machine q35,confidential-guest-support=sev0,memory-backend=ram1,igvm-cfg=igvm0,x-svsm-virtio-mmio=on
```

Add these options while keeping the existing guest virtio-net-pci device:

```sh
-global virtio-mmio.force-legacy=false \
-netdev user,id=svsm_tcp \
-device virtio-net-device,netdev=svsm_tcp,mac=52:54:00:12:34:57
```

This requires the Coconut QEMU support for `x-svsm-virtio-mmio`, just as
the SVSM vsock transport does. A virtio-net-pci device is not a substitute:
this driver discovers only the MMIO devices advertised through fw_cfg.

On the QEMU host:

```sh
python3 scripts/tcp-udp-receiver.py --bind 127.0.0.1 --port 4050
```

SVSM uses source `10.0.2.15:4050`, destination `10.0.2.2:4050`, and MAC
`52:54:00:12:34:57`. This is a separate QEMU user-network backend from
`net0`, so its IP range can be the same. The receiver prints JSON records.
No hostfwd option is needed for traffic originating inside the VM.

The Ethernet destination is broadcast; the IPv4 destination is the QEMU
host address. This intentionally avoids ARP and is scoped to the dedicated
user-network backend above, not a general-purpose bridged network. There
is no DHCP, receive path, TCP, TLS, or retransmission. The IPv4 header has
a checksum; IPv4 UDP's optional checksum is zero. Addresses are constants
in `kernel/src/vmm/tcp_udp.rs`.

The modern VirtIO network header is 12 zero bytes (no offloads). RX queue 0
is initialized without buffers; TX queue 1 holds at most one outstanding
frame. The driver owns its buffer until descriptor completion. After each
guest exit, outside the VMSA reference, SVSM polls completion and submits up
to 32 records without waiting. Further progress requires later guest exits.
There is no continuously polling sender task. A driver error stops export
with a warning; no device recovery is attempted.

The producer queue holds 128 events. UDP loss, queue overflow, or stopping
the VM can lose records. The CTCP dropped-events field counts only producer
queue overflow. Existing TCP scan timing, deduplication, and assumptions
about the guest Linux layout remain unchanged.

Validation on the development host: `svsm` and `stage2` were also
cross-built and linked for `x86_64-unknown-none`, using the Rust toolchain
`ld.lld` via `CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER`. This build enabled
only `tcp-log-net`; a complete IGVM/vTPM image was not built here.

```sh
cargo check --locked --offline -p svsm --target x86_64-unknown-none --features tcp-log-net
cargo test --locked --offline -p virtio-drivers --lib
rustc --edition=2024 --test kernel/src/vmm/tcp_udp.rs -o /tmp/tcp-udp-tests
/tmp/tcp-udp-tests
rustc --edition=2024 --test kernel/src/vmm/tcp_event.rs -o /tmp/tcp-event-tests
/tmp/tcp-event-tests
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests -v
```

The fake transport test checks the actual queue bytes and buffer ownership
until completion. A real SNP VM, QEMU network backend, and this user's QEMU
binary were not available locally; end-to-end delivery still needs a run
on the QEMU host.
