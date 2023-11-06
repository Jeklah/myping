// Simple ping program.
// Author: Arthur Bowers
//

use std::net::Ipv4Addr;
use std::net::UdpSocket;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const ICMP_HEADER_LEN: usize = 8;
const ECHO_REQUEST_TYPE: u8 = 8;
const ECHO_REQUEST_CODE: u8 = 0;
const ICMP_PAYLOAD_LEN: usize = 56;
const TIMEOUT_SEC: u64 = 2;

fn main() {
    let target_addr = "127.0.0.1";
    let target_ip: Ipv4Addr = target_addr.parse().unwrap();
    let icmp_socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind socket");

    let mut seq_no = 0;

    loop {
        seq_no += 1;
        let icmp_packet = build_icmp_packet(seq_no);

        let start_time = Instant::now();
        icmp_socket
            .send_to(&icmp_packet, (target_ip, 7))
            .expect("Failed to send packet");

        let mut buffer = [0; 1024]; // 1K buffer
        match recv_timeout(&icmp_socket, &mut buffer, Duration::from_secs(TIMEOUT_SEC)) {
            Ok(_) => {
                let elapsed_time = start_time.elapsed();
                println!("Reply from {}: time={:?}", target_addr, elapsed_time);
            }
            Err(_) => println!("Request timed out"),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn recv_timeout(
    socket: &UdpSocket,
    buffer: &mut [u8],
    timeout: Duration,
) -> std::io::Result<usize> {
    socket.set_read_timeout(Some(timeout))?;
    socket.recv(buffer)
}

fn build_icmp_packet(seq_no: u16) -> Vec<u8> {
    let mut packet = vec![0; ICMP_HEADER_LEN + ICMP_PAYLOAD_LEN];

    // ICMP echo request header
    packet[0] = ECHO_REQUEST_TYPE;
    packet[1] = ECHO_REQUEST_CODE;
    // ICMP checksum (calculated later)
    packet[2] = 0;
    packet[3] = 0;
    // ICMP identifier (arbitrary value for simplicity)
    packet[4] = 0x12;
    packet[5] = 0x34;
    // ICMP sequence number (incremented on each request)
    packet[6] = (seq_no >> 8) as u8;
    packet[7] = seq_no as u8;

    // ICMP payload (arbitrary payload for testing)
    for i in ICMP_HEADER_LEN..(ICMP_HEADER_LEN + ICMP_PAYLOAD_LEN) {
        packet[i] = (i - ICMP_HEADER_LEN) as u8;
    }

    packet
}
