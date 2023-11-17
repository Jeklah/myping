// Simple ping program.
// Author: Arthur Bowers
//

use mio::unix::SourceFd;
use mio::{event::Source, Interest as InterestMio, Registry, Token};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::io::Read;
use std::iter::Iterator;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::io::{AsRawFd, RawFd};
use std::process;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::time::{self, Duration};

// A simple ICMP echo implementation in Rust.
//
// NOTE: Requires root access on Linux to run.
// Root access is required for the raw socket.
//
// RUN: cargo build && sudo ./target/debug/ping "1.1.1.1,3,1000"
//
// MONITOR: sudo tcpdump --interface wlp3s0 -c5 icmp

#[tokio::main]
async fn main() -> io::Result<()> {
    // Expected parameter: "1.1.1.1, 3, 1000"
    // IPv4, request_count, interval_in_milliseconds

    // Read command-line parameter
    let param_str = match std::env::args().nth(1) {
        None => {
            // Ensure one string parameter exists
            println!("Missing parameter: specify one parameter, \"1.1.1.1,3,1000\"");
            process::exit(1);
        }
        Some(s) => s,
    };
    // println!("param_str: {}", param_str);

    // Parse the command-line parameter to a cfg object
    let cfg = match parse_cfg(param_str) {
        Err(e) => {
            println!("{}", e);
            process::exit(1);
        }
        Ok(v) => v,
    };
    // println!("Cfg: {:?}", Cfg);

    // Create an ICMP socket
    let socket = IcmpSocketTokio::new()?;
    socket.connect(cfg.ipv4_adr).await?;

    let mut complete_count: u16 = 0;
    let mut seq: u16 = 0;
    let mut seqs: HashMap<u16, Instant> = HashMap::new();
    let mut interval = time::interval(Duration::from_millis(cfg.interval_ms));

    // Send, receive and timeout pings
    loop {
        // tokio::select! runs on one task and thread
        tokio::select! {
            _ = interval.tick() => {
                // Send pings on an interval
                if seq >= cfg.ping_count {
                    continue
                }
                seq += 1;

                // Send one ping message
                let res_tx = socket.send_ping(PKT_ID, seq).await?;

                // Store ping send time by sequence number
                seqs.insert(seq, res_tx.tme);
            }
            res_rx = socket.recv_ping() => {
                // Ping response messages may return sequence numbers out of order
                // Map is used to associate the sequence number to the send time

                complete_count += 1;

                let seq_time_rx = match res_rx {
                    Err(e) => {
                        println!("socket.recv_ping: res_rx: {}", e);
                        process::exit(1);
                    }
                    Ok(v) => v,
                };
                if let Some(time_tx) = seqs.remove(&seq_time_rx.seq) {
                    // Calculate the round-trip time
                    let dur = seq_time_rx.tme - time_tx;
                    // IPv4, icmp_sequence_number, elapsed_time_in_milliseconds
                    println!("{}, {}, {}us", cfg.ipv4_adr, seq_time_rx.seq, dur.as_micros());

                    // Check if all pings have completed
                    if complete_count >= cfg.ping_count {
                        break;
                    }
                } else {
                    println!("socket.recv_ping: missing seq: '{}' in map", seq_time_rx.seq);
                    process::exit(1);
                }
            }
            _ = async {
                // Remove timed out pings
                const PING_TIMEOUT: Duration = Duration::new(5, 0);
                seqs.retain(|_, tme_tx| {
                    let retain = Instant::now() < *tme_tx + PING_TIMEOUT;
                    if !retain {
                        // Timeout occurred
                        complete_count += 1;
                    }

                    retain
                })
            } => {
                // All pings received or timed out?
                if complete_count >= cfg.ping_count {
                    // Exit select loop
                    break
                }
            }
        };
    }
    Ok(())
}

// Parse a string to a Cfguration object.
fn parse_cfg(param_str: String) -> Result<Cfg, String> {
    // Split the string into a vector of strings
    let params: Vec<&str> = param_str.split(',').collect();
    if params.len() != 3 {
        return Err(format!(
            "Invalid parameter part count(actual: {}, expected: 3)",
            params.len()
        ));
    }

    // Parse the IPv4 address
    let ipv4 = match params[0].parse::<Ipv4Addr>() {
        Err(e) => {
            return Err(format!(
                "Invalid parameter part: unable to parse address '{}' as IPv4: {}",
                params[0], e
            ));
        }
        Ok(v) => v,
    };

    // println!("ipv4: {}", ipv4_adr);

    // Parse the ping count
    let ping_count = match params[1].parse::<u16>() {
        Err(e) => {
            return Err(format!(
                "Invalid parameter part: unable to parse ping count '{}' as u16: {}",
                params[1], e
            ));
        }
        Ok(v) => v,
    };

    // Validate ping count minimum
    if ping_count < 1 {
        return Err(format!(
            "Invalid parameter part: ping count is too small: (actual: {}, expected: >= 1)",
            ping_count
        ));
    }

    // Validate ping count maximum
    if ping_count > 10 {
        return Err(format!(
            "Invalid parameter part: ping count is too large: (actual: {}, expected: <= 10)",
            ping_count
        ));
    }
    // println!("ping_count: {}", ping_count);

    // Parse the interval
    let interval_ms = match params[2].parse::<u64>() {
        Err(e) => {
            return Err(format!(
                "Invalid parameter part: unable to parse interval '{}' as u64: {}",
                params[2], e
            ));
        }
        Ok(v) => v,
    };

    // Validate interval minimum
    if interval_ms > 1000 {
        return Err(format!(
            "Invalid parameter part: interval is too large: (actual: {}, expected: <= 1000)",
            interval_ms
        ));
    }
    // println!("interval_ms: {}", interval_ms);

    // Create the Configuration object for async passing
    return Ok(Cfg {
        ipv4_adr: ipv4,
        ping_count,
        interval_ms,
    });
}

// Cfg Cfgures a ping operation.
#[derive(Debug)]
struct Cfg {
    ipv4_adr: Ipv4Addr,
    ping_count: u16,
    interval_ms: u64,
}

// An ICMP socket used with low-level `mio` events.
//
// Modeled on [mio::UdpSocket] (https://github.com/tokio-rs/mio/blob/master/src/sys/unix/udp.rs)
//
// See [RFC 792] (https://tools.ietf.org/html/rfc792) for more details on ICMP.
pub struct IcmpSocketMio {
    socket: Socket,
}

impl IcmpSocketMio {
    // `new` creates a new `IcmpSocketMio` bound to the specified address.
    //
    // `new` creates a raw system socket which is non-blocking.
    //
    // Calling `new` requires root access on Linux for the raw network
    // capability `CAP_NET_RAW` to open a raw socket.
    //
    // `new` returns a "no permission" error if `CAP_NET_RAW` is not available.
    pub fn new() -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))?;

        // Set non-blocking before connect()
        socket.set_nonblocking(true)?;

        Ok(Self { socket: socket })
    }

    // Connect the ICMP socket with a destination IPv4 address.
    //
    // Used by the `send` and `recv` functions.
    pub fn connect(&self, ip: Ipv4Addr) -> io::Result<()> {
        let addr = SocketAddr::new(IpAddr::V4(ip), 0);
        self.socket.connect(&addr.into())
    }

    // Send data to the socket and remote address.
    //
    // Ensure `connect` was called once before calling `send`.
    //
    // Returns the number of bytes sent; or, an error.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.socket.send(buf)
    }

    // Receive data from the socket and remote address.
    //
    // Ensure `connect` was called once before calling `recv`.
    //
    // Returns the number of bytes recived or, an error.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // Simpler to call `read` than recv`.
        // `recv` requires cumbersome, unsafe casting with MaybeUninit.
        (&self.socket).read(buf)

        // Avoid using ugly MaybeUninit with `recv`
        // let buf2 - &mut [ 0u8; PKT_BUF_SIZE];
        // let buf3 = unsafe { buf2 as *mut [u8] };
        // let buf4 = unsafe { buf3 as *mut [MaybeUninit<u8>] };
        // let buf5 = unsafe { &mut *buf4 };
    }
}

// Implement Source for the `mio` crate.
impl Source for IcmpSocketMio {
    fn register(&mut self, poll: &Registry, token: Token, interest: InterestMio) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).register(poll, token, interest)
    }

    fn reregister(
        &mut self,
        poll: &Registry,
        token: Token,
        interest: InterestMio,
    ) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).reregister(poll, token, interest)
    }

    fn deregister(&mut self, poll: &Registry) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).deregister(poll)
    }
}

// Implement AsRawFs for the `mio` crate.
impl AsRawFd for IcmpSocketMio {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

// An ICMP socket used with `tokio` events.
#[derive(Clone)]
pub struct IcmpSocketTokio {
    socket: Arc<AsyncFd<IcmpSocketMio>>,
}

impl IcmpSocketTokio {
    // `new` creates an IcmpSocketTokio instance.
    //
    // `new` creates a raw system socket which is non-blocking.
    //
    // Calling `new requires root permission for the
    // raw network capability `CAP_NET_RAW` to open a raw socket.
    //
    // `new` returns a "no permission" error if `CAP_NET_RAW` is not available.
    pub fn new() -> io::Result<Self> {
        let mio_socket = IcmpSocketMio::new()?;
        let fd_mio_socket = AsyncFd::new(mio_socket)?;
        Ok(Self {
            socket: Arc::new(fd_mio_socket),
        })
    }

    // Connect the ICMP socket with a destination IPv4 address.
    //
    // Used by the `send` and `recv` functions.
    //
    // Pattern based on [tokio::net::UdpSocket.connect] (https://docs.rs/tokio/1.9.0/tokio/net/struct.UdpSocket.html#method.connect)
    pub async fn connect(&self, ip: Ipv4Addr) -> io::Result<()> {
        self.socket.get_ref().connect(ip)
    }

    // Send data to the socket and remote address.
    //
    // Ensure `connect` was called once before calling `send`.
    //
    // Returns the number of bytes sent or, and error.
    //
    // Pattern is based on [tokio::net::UdpSocket.send] (https://docs.rs/tokio/1.9.0/tokio/net/struct.UdpSocket.html#method.send)
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.socket
            .async_io(Interest::WRITABLE, |socket| socket.send(buf))
            .await
    }

    // Receive data from the socket and remote address.
    //
    // Ensure `connect` was called once before calling `recv`.
    //
    // Returns the number of bytes received or an error.
    //
    // Pattern is based on [tokio::net::UdpSocket.recv] (https://docs.rs/tokio/1.9.0/tokio/net/struct.UdpSocket.html#method.recv)
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket
            .async_io(Interest::READABLE, |socket| socket.recv(buf))
            .await
    }

    // Send a ping echo message.
    pub async fn send_ping(&self, id: u16, seq: u16) -> io::Result<SeqTme> {
        let buf_tx = create_echo_pkt(id, seq);

        let res = match self
            .socket
            .async_io(Interest::WRITABLE, |socket| socket.send(&buf_tx))
            .await
        {
            Err(e) => Err(e),
            Ok(count_tx) => {
                let tme = Instant::now();
                assert_eq!(count_tx, PKT_BUF_SIZE);
                Ok(SeqTme { seq, tme })
            }
        };

        res
    }

    // Receive a ping echo reply message.
    pub async fn recv_ping(&self) -> io::Result<SeqTme> {
        let mut buf_rx = [0u8; PKT_BUF_SIZE];

        let res = match self
            .socket
            .async_io(Interest::READABLE, |socket| socket.recv(&mut buf_rx))
            .await
        {
            Err(e) => Err(e),
            Ok(count_rx) => {
                let tme = Instant::now();
                let seq = read_seq_from_echo_pkt(buf_rx);
                assert_eq!(count_rx, PKT_BUF_SIZE);
                Ok(SeqTme { seq, tme })
            }
        };

        res
    }
}

// A ping sequence number and time.
#[derive(Debug)]
pub struct SeqTme {
    seq: u16,
    tme: Instant,
}

// An ICMP header type indicating the message is an Echo request.
const HDR_TYP_ECHO: u8 = 8;
// An ICMP header Cfguration indiciating the Echo message contains an id and sequence number.
const HDR_CFG_ECHO: u8 = 0;
// The size of an ICMP packet buffer in bytes.
//
// The buffer is header-only, and doesn't have a payload.
const PKT_BUF_SIZE: usize = 28;
// An echo packet id.
const PKT_ID: u16 = 1;

// Writes an ICMP message header checksum.
//
// See [RFC 792] (https://tools.ietf.org/html/rfc792) for more details on ICMP.
//
// TODO: Switch to public crate for IP checksum.
fn write_checksum(buf: &mut [u8]) {
    let mut sum = 0u32;
    for word in buf.chunks(2) {
        let mut part = u16::from(word[0]) << 8;
        if word.len() > 1 {
            part += u16::from(word[1]);
        }
        sum = sum.wrapping_add(u32::from(part));
    }

    while (sum >> 16) > 0 {
        sum = (sum >> 16) + (sum & 0xffff);
    }

    let sum = !sum as u16;

    buf[2] = (sum >> 8) as u8;
    buf[3] = (sum & 0xff) as u8;
}

// Creates an echo request packet.
pub fn create_echo_pkt(id: u16, seq: u16) -> [u8; PKT_BUF_SIZE] {
    // Create an echo message
    let mut buf = [0u8; PKT_BUF_SIZE];
    buf[0] = HDR_TYP_ECHO;
    buf[1] = HDR_CFG_ECHO;
    // buf[2] will contain the checksum
    // buf[3] will contain the checksum
    buf[4] = (id >> 8) as u8;
    buf[5] = id as u8;
    buf[6] = (seq >> 8) as u8;
    buf[7] = seq as u8;
    // Payload is intentionally left empty

    // Write the message header's checksum
    write_checksum(&mut buf);

    buf
}

// Reads the sequence number from an echo reply message buffer.
pub fn read_seq_from_echo_pkt(buf: [u8; PKT_BUF_SIZE]) -> u16 {
    let a = unsafe { *(buf[26..].as_ptr() as *const [u8; 2]) };
    u16::from_be_bytes(a)
}
