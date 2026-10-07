// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fixed-address UDP framing for the dedicated QEMU user-network backend.

pub const PAYLOAD_SIZE: usize = 56;
pub const PACKET_SIZE: usize = 14 + 20 + 8 + PAYLOAD_SIZE;
const SOURCE_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x57];
const SOURCE_IP: [u8; 4] = [10, 0, 2, 15];
const HOST_IP: [u8; 4] = [10, 0, 2, 2];
const PORT: u16 = 4050;

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in bytes.chunks(2) {
        sum += (u32::from(pair[0]) << 8) | u32::from(*pair.get(1).unwrap_or(&0));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub fn encode(payload: &[u8; PAYLOAD_SIZE]) -> [u8; PACKET_SIZE] {
    let mut packet = [0; PACKET_SIZE];
    // L2 broadcast deliberately avoids an ARP receive path on this private backend.
    packet[..6].fill(0xff);
    packet[6..12].copy_from_slice(&SOURCE_MAC);
    packet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    let ip = &mut packet[14..34];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((PACKET_SIZE - 14) as u16).to_be_bytes());
    ip[6] = 0x40; // Don't fragment; the complete packet is below the MTU.
    ip[8] = 64;
    ip[9] = 17; // UDP
    ip[12..16].copy_from_slice(&SOURCE_IP);
    ip[16..20].copy_from_slice(&HOST_IP);
    let ip_checksum = checksum(ip);
    ip[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
    packet[34..36].copy_from_slice(&PORT.to_be_bytes());
    packet[36..38].copy_from_slice(&PORT.to_be_bytes());
    packet[38..40].copy_from_slice(&((8 + PAYLOAD_SIZE) as u16).to_be_bytes());
    // IPv4 permits a zero UDP checksum (no checksum).
    packet[42..].copy_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_complete_ipv4_udp_datagram() {
        let payload = [0xa5; PAYLOAD_SIZE];
        let packet = encode(&payload);
        assert_eq!(
            &packet[..14],
            &[255, 255, 255, 255, 255, 255, 82, 84, 0, 18, 52, 87, 8, 0]
        );
        assert_eq!(packet[14], 0x45);
        assert_eq!(u16::from_be_bytes([packet[16], packet[17]]), 84);
        assert_eq!(checksum(&packet[14..34]), 0);
        assert_eq!(&packet[26..34], &[10, 0, 2, 15, 10, 0, 2, 2]);
        assert_eq!(&packet[34..42], &[0x0f, 0xd2, 0x0f, 0xd2, 0, 64, 0, 0]);
        assert_eq!(&packet[42..], &payload);
    }
}
