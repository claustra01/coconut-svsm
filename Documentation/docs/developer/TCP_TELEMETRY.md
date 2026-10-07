# TCP telemetry through shared memory

This branch starts at `sock-trawl` (`990fc4fe`). It keeps the TCP scan and
publishes the observed connection records in a 16 KiB host-shared ring.
The collector uses QEMU's QMP `pmemsave` command to copy that ring. No QEMU
source change, guest Linux driver, or vsock device is required. This is a
polling prototype, not a zero-copy host mapping.

Build with `make TCP_LOG_MODE=shmem`. With the repository launcher, add
`--tcp-shmem-qmp /tmp/svsm-tcp-qmp.sock`.

For the existing `run.sh` configuration, keep `-serial mon:stdio` and add:

```sh
-qmp unix:/tmp/svsm-tcp-qmp.sock,server=on,wait=off
```

After the first TCP observation, SVSM prints:

```text
TCP shared ring: gpa=0x... size=16384
```

On the QEMU host, use the printed GPA:

```sh
sudo python3 scripts/tcp-shmem-relay.py \
  --qmp /tmp/svsm-tcp-qmp.sock --gpa 0xADDRESS
```

The collector prints one JSON object per connection. QEMU and the collector
must run on the same host and be able to access the collector's temporary
directory. The collector polls every 200 ms by default (`--interval`). Run it
again with the new GPA after a VM reboot.

The first 64 bytes contain `CTCPRNG1` and four little-endian u32 values:
allocation size, slot count (128), slot size (72), and frame size (56).
Each slot contains a little-endian u64 stamp, a CTCP frame, and a matching
u64 stamp. The producer clears both stamps before updating a slot and
publishes the new sequence after the frame. The reader skips incomplete
slots and already-seen sequences. Slots wrap; there is no acknowledgement
or replay. Sequence gaps can therefore reflect overwritten slots or a full
128-event producer queue. The frame's dropped-events counter counts producer
queue overflow only.

TCP frames use the same 56-byte CTCP representation as `virtio-vsock`:
magic/version/type/length, sequence, observation TSC, socket address, TCP
state, IPv4 source/destination and ports, and dropped-events count. The
payload is plaintext in host-shared memory.

Validation on the development host: `svsm` and `stage2` were also
cross-built and linked for `x86_64-unknown-none`, using the Rust toolchain
`ld.lld` via `CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER`. This build enabled
only `tcp-log-shmem`; a complete IGVM/vTPM image was not built here.

```sh
cargo check --locked --offline -p svsm --target x86_64-unknown-none --features tcp-log-shmem
rustc --edition=2024 --test kernel/src/vmm/tcp_event.rs -o /tmp/tcp-event-tests
/tmp/tcp-event-tests
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests -v
```

Actual SEV-SNP execution and QMP reads of these shared pages still need to be
checked on the QEMU host. The existing scan interval and guest-kernel layout
assumptions are unchanged.
