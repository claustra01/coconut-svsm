# TCP telemetry through guest Linux

This branch starts at `sock-trawl` (`990fc4fe`). SVSM queues observed TCP
connections. A guest Linux misc driver reads batches through an experimental
SVSM protocol, and a Python process forwards each record as a UDP datagram.
This branch does not need a vsock device or a QEMU source change.

Build SVSM with `make TCP_LOG_MODE=guest` and use the resulting IGVM image in
the existing `run.sh`. The existing guest virtio-net-pci is sufficient.

The **Linux kernel running inside the VM** needs the helper in
`tools/tcp-guest-linux/linux-6.11-socktrawl.patch`. The patch is based on
upstream Linux v6.11; check it against the actual coconut guest kernel before
applying. A host `uname -r` does not establish the guest's kernel version.

```sh
# In the guest kernel source tree:
git apply --check /path/to/coconut-svsm/tools/tcp-guest-linux/linux-6.11-socktrawl.patch
git apply /path/to/coconut-svsm/tools/tcp-guest-linux/linux-6.11-socktrawl.patch
# Rebuild/install this kernel with CONFIG_AMD_MEM_ENCRYPT=y and boot the VM with it.
```

Build and load the companion module in the guest, using the build tree and
Module.symvers from that patched kernel:

```sh
make -C tools/tcp-guest-linux KDIR=/path/to/patched-linux-build
sudo insmod tools/tcp-guest-linux/socktrawl.ko
```

Start the collector on the QEMU host:

```sh
python3 scripts/tcp-udp-receiver.py --bind 127.0.0.1 --port 4050
```

Then run the relay in the guest. With the provided QEMU user-networking
configuration, `10.0.2.2` addresses the host:

```sh
sudo python3 scripts/tcp-guest-relay.py --host 10.0.2.2 --port 4050
```

The collector prints JSON. The relay polls the nonblocking `/dev/socktrawl`
device every second while empty (`--interval`). A read drains up to 73
56-byte records; it returns EAGAIN when empty. Use one collector process.
The buffer stays guest-private; it is not converted into host-shared memory.

Protocol `0x80000054` is a local experimental ID, not an assigned standard.
Call 0 takes a page-aligned output GPA in RCX and a capacity from 56 to 4096
bytes in RDX. RCX returns bytes copied, always a multiple of 56. Core protocol
query advertises version 1. Reads consume events; there is no replay or ACK.
The kernel patch provides `snp_socktrawl_read()` because the Linux 6.11 SVSM
call dispatcher is private to the SEV implementation.

Records use the same CTCP format as `virtio-vsock`. Delivery is best-effort;
the producer queue holds 128 events, UDP can lose records, and the frame's
drop counter counts producer queue overflow only. The relay does not use
TLS or maintain a persistent spool. Guest shutdown stops forwarding.

Validation on the development host: `svsm` and `stage2` were also
cross-built and linked for `x86_64-unknown-none`, using the Rust toolchain
`ld.lld` via `CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER`. This build enabled
only `tcp-log-guest`; a complete IGVM/vTPM image was not built here.

```sh
cargo check --locked --offline -p svsm --target x86_64-unknown-none --features tcp-log-guest
rustc --edition=2024 --test kernel/src/vmm/tcp_event.rs -o /tmp/tcp-event-tests
/tmp/tcp-event-tests
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests -v
```

The patch applies to upstream Linux v6.11. The guest module build and actual
SVSM calls must be validated in the guest's patched kernel environment; that
Linux build tree and an SNP VM are not present on the development host.
