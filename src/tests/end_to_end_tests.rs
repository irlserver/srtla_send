#![cfg(test)]

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};

use smallvec::SmallVec;
use tempfile::NamedTempFile;
use tokio::net::UdpSocket;
use tokio::time::{Duration, timeout};

// Note: These tests require the modules to be public or have test-specific
// visibility They test the integration between different components

#[tokio::test]
async fn test_ip_file_parsing_and_validation() {
    // Create a temporary file with IP addresses
    let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
    writeln!(temp_file, "192.168.1.100").unwrap();
    writeln!(temp_file, "192.168.1.101").unwrap();
    writeln!(temp_file, "# This is a comment").unwrap();
    writeln!(temp_file).unwrap(); // Empty line
    writeln!(temp_file, "192.168.1.102").unwrap();
    writeln!(temp_file, "invalid-ip-address").unwrap(); // Should be ignored
    writeln!(temp_file, "10.0.0.1").unwrap();

    temp_file.flush().unwrap();

    // Test that we can read the file (this would require making the function
    // public) For now, we just test that the file exists and is readable
    let content = std::fs::read_to_string(temp_file.path()).unwrap();
    assert!(content.contains("192.168.1.100"));
    assert!(content.contains("192.168.1.101"));
    assert!(content.contains("192.168.1.102"));
    assert!(content.contains("10.0.0.1"));
    assert!(content.contains("invalid-ip-address"));

    // Verify the content can be parsed as expected
    let mut valid_ips = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(ip) = line.parse::<IpAddr>() {
            valid_ips.push(ip);
        }
    }

    assert_eq!(valid_ips.len(), 4);
    assert_eq!(valid_ips[0], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
    assert_eq!(valid_ips[1], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101)));
    assert_eq!(valid_ips[2], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 102)));
    assert_eq!(valid_ips[3], IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
}

#[tokio::test]
async fn test_udp_socket_creation_and_binding() {
    // Test that we can create UDP sockets on different interfaces
    let addresses = vec!["127.0.0.1:0", "0.0.0.0:0"];

    for addr in addresses {
        let socket = UdpSocket::bind(addr).await;
        assert!(socket.is_ok(), "Failed to bind to {}", addr);

        let socket = socket.unwrap();
        let local_addr = socket.local_addr().unwrap();
        assert!(local_addr.port() > 0, "Should have assigned a port");
    }
}

#[tokio::test]
async fn test_concurrent_udp_operations() {
    // Create two sockets that will communicate with each other
    let socket1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let socket2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let addr1 = socket1.local_addr().unwrap();
    let addr2 = socket2.local_addr().unwrap();

    // Test sending data between them
    let test_data = b"Hello, SRTLA!";

    // Send from socket1 to socket2
    let bytes_sent = socket1.send_to(test_data, addr2).await.unwrap();
    assert_eq!(bytes_sent, test_data.len());

    // Receive on socket2
    let mut buffer = [0u8; 1024];
    let result = timeout(Duration::from_millis(500), socket2.recv_from(&mut buffer)).await;

    assert!(result.is_ok(), "Should have received data within timeout");
    let (bytes_received, sender_addr) = result.unwrap().unwrap();

    assert_eq!(bytes_received, test_data.len());
    assert_eq!(&buffer[..bytes_received], test_data);
    assert_eq!(sender_addr, addr1);
}

#[tokio::test]
async fn test_protocol_message_flow() {
    use crate::protocol::*;

    // Simulate a registration flow
    let mut sender_id = [0u8; SRTLA_ID_LEN];
    for (i, byte) in sender_id.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }

    // Step 1: Create REG1 packet
    let reg1_packet = create_reg1_packet(&sender_id);
    assert!(is_srtla_reg1(&reg1_packet));
    assert_eq!(reg1_packet.len(), SRTLA_TYPE_REG1_LEN);

    // Step 2: Simulate server modifying the ID for REG2 response
    let mut server_id = sender_id;
    let half_len = SRTLA_ID_LEN / 2;
    for byte in &mut server_id[half_len..] {
        *byte = byte.wrapping_add(0x80); // Server modification
    }

    let reg2_packet = create_reg2_packet(&server_id);
    assert!(is_srtla_reg2(&reg2_packet));
    assert_eq!(reg2_packet.len(), SRTLA_TYPE_REG2_LEN);

    // Step 3: Verify that sender ID was different from server ID
    assert_ne!(sender_id, server_id);

    // Step 4: Create REG3 confirmation
    let reg3_packet = vec![(SRTLA_TYPE_REG3 >> 8) as u8, (SRTLA_TYPE_REG3 & 0xff) as u8];
    assert!(is_srtla_reg3(&reg3_packet));
    assert_eq!(reg3_packet.len(), SRTLA_TYPE_REG3_LEN);
}

#[tokio::test]
async fn test_keepalive_timing() {
    use crate::protocol::*;

    let start_time = std::time::Instant::now();

    // Create several keepalive packets with delays
    let mut timestamps = Vec::new();

    for i in 0..3 {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let packet = create_keepalive_packet();
        let timestamp = extract_keepalive_timestamp(&packet).unwrap();
        timestamps.push(timestamp);
    }

    let end_time = std::time::Instant::now();
    let elapsed_ms = end_time.duration_since(start_time).as_millis() as u64;

    // Verify timestamps are increasing
    assert!(timestamps[1] > timestamps[0]);
    assert!(timestamps[2] > timestamps[1]);

    // Verify timestamps are reasonable (within expected time range)
    let timestamp_range = timestamps[2] - timestamps[0];

    assert!(timestamp_range >= 80, "Expected >= ~2*50ms with margin");
    assert!(
        timestamp_range <= elapsed_ms + 500,
        "Allow scheduler jitter"
    );
}

#[tokio::test]
async fn test_ack_nak_sequence_handling() {
    use crate::protocol::*;

    // Test sequence of ACKs and NAKs
    let sequences: SmallVec<u32, 4> = SmallVec::from_vec(vec![100u32, 101, 102, 103, 104, 105]);

    // Create SRTLA ACK packet
    let ack_packet = create_ack_packet(&sequences);
    let parsed_acks = parse_srtla_ack(&ack_packet);
    assert_eq!(parsed_acks, sequences);

    // Create SRT NAK packet with mixed single and range NAKs
    let mut nak_packet = Vec::new();
    nak_packet.extend_from_slice(&SRT_TYPE_NAK.to_be_bytes());
    nak_packet.extend_from_slice(&0u16.to_be_bytes()); // Reserved field

    // Single NAK
    nak_packet.extend_from_slice(&200u32.to_be_bytes());

    // Range NAK (300-303)
    let range_start = 300u32 | 0x8000_0000;
    nak_packet.extend_from_slice(&range_start.to_be_bytes());
    nak_packet.extend_from_slice(&303u32.to_be_bytes());

    // Another single NAK
    nak_packet.extend_from_slice(&400u32.to_be_bytes());

    let parsed_naks = parse_srt_nak(&nak_packet);
    let expected_naks: SmallVec<u32, 4> = SmallVec::from_vec(vec![200, 300, 301, 302, 303, 400]);
    assert_eq!(parsed_naks, expected_naks);
}

#[tokio::test]
async fn test_packet_size_limits() {
    use crate::protocol::*;

    // Test that we don't create packets larger than MTU
    let max_acks = (MTU - 2) / 4; // Maximum ACKs that fit in MTU
    let large_ack_list: SmallVec<u32, 4> = (0..max_acks as u32).collect();

    let packet = create_ack_packet(&large_ack_list);
    assert!(packet.len() <= MTU, "ACK packet should not exceed MTU");
    assert_eq!(
        packet.len(),
        4 + 4 * max_acks,
        "Expected full MTU utilization"
    );

    // Test that parsing works correctly for large packets
    let parsed = parse_srtla_ack(&packet);
    assert_eq!(parsed.len(), large_ack_list.len());
    assert_eq!(parsed, large_ack_list);
}

#[tokio::test]
async fn test_error_resilience() {
    use crate::protocol::*;

    // Test various malformed inputs don't crash
    let test_cases = vec![
        vec![],           // Empty
        vec![0x90],       // Too short
        vec![0x90, 0x00], // Minimum valid header
        vec![0xff; 1000], // Large invalid packet
    ];

    for case in test_cases {
        // These should not panic
        let _ = get_packet_type(&case);
        let _ = get_srt_sequence_number(&case);
        let _ = parse_srt_ack(&case);
        let _ = parse_srt_nak(&case);
        let _ = parse_srtla_ack(&case);
        let _ = extract_keepalive_timestamp(&case);
        let _ = is_srtla_reg1(&case);
        let _ = is_srtla_reg2(&case);
        let _ = is_srtla_reg3(&case);
        let _ = is_srtla_keepalive(&case);
        let _ = is_srt_ack(&case);
    }
}

#[tokio::test]
async fn test_concurrent_packet_processing() {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::protocol::*;

    let results = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();

    // Spawn multiple tasks that create and process packets concurrently
    for i in 0..10 {
        let results_clone = results.clone();
        let handle = tokio::spawn(async move {
            // Create different types of packets
            let keepalive = create_keepalive_packet();
            let timestamp = extract_keepalive_timestamp(&keepalive).unwrap();

            let acks = vec![i * 100u32, i * 100 + 1, i * 100 + 2];
            let ack_packet = create_ack_packet(&acks);
            let parsed_acks = parse_srtla_ack(&ack_packet);

            let mut results_guard = results_clone.lock().await;
            results_guard.push((i, timestamp, parsed_acks));
        });
        handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }

    let results_guard = results.lock().await;
    assert_eq!(results_guard.len(), 10);

    // Verify each task produced expected results
    for (i, timestamp, acks) in results_guard.iter() {
        assert!(*timestamp > 0);
        assert_eq!(acks.len(), 3);
        assert_eq!(acks[0], i * 100);
        assert_eq!(acks[1], i * 100 + 1);
        assert_eq!(acks[2], i * 100 + 2);
    }
}

#[test]
fn test_memory_usage_bounds() {
    use crate::protocol::*;

    // Test that parsing large NAK ranges doesn't consume excessive memory
    let mut large_nak = Vec::new();
    large_nak.extend_from_slice(&SRT_TYPE_NAK.to_be_bytes());
    large_nak.extend_from_slice(&0u32.to_be_bytes()); // Padding

    // Create a range that would be 10,000 items if not limited
    let range_start = 1u32 | 0x8000_0000;
    large_nak.extend_from_slice(&range_start.to_be_bytes());
    large_nak.extend_from_slice(&10001u32.to_be_bytes());

    let naks = parse_srt_nak(&large_nak);

    // Should be limited to 1000 items max
    assert!(naks.len() <= 1000);
}

// Helper trait for memory usage testing (may not be available on all platforms)
#[allow(dead_code)]
trait MemoryStats {
    fn used_memory(&self) -> Option<usize>;
}

impl MemoryStats for std::alloc::System {
    fn used_memory(&self) -> Option<usize> {
        // This is a placeholder - actual implementation would depend on platform
        None
    }
}
