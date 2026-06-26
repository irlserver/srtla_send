//! SRTLA wire-conformance golden tests.
//!
//! These tests LOCK byte-level compatibility between this sender and the SRTLA
//! wire format the receiver speaks:
//!
//! - Type codes + `SRTLA_ID_LEN` + REG frame sizes (`common.h`).
//! - REG1/REG2/REG3 build: `htobe16(type)` header + 256-byte id.
//! - ACK layout `struct { uint32_t type; uint32_t acks[10]; }` with
//!   `ack.type = htobe32(SRTLA_TYPE_ACK << 16)`.
//!
//! They treat our `constants`/`builders`/`parsers` as a black box via the public
//! `protocol` re-exports — no test-only seams, so they cannot drift from
//! production behavior. If any assertion here fails, a wire constant or layout
//! has changed and the sender is no longer interoperable with an SRTLA receiver;
//! that is a deliberate, versioned protocol change, never an accident.

use srtla_send::protocol::{
    SRTLA_ID_LEN, SRTLA_TYPE_ACK, SRTLA_TYPE_KEEPALIVE, SRTLA_TYPE_REG_ERR, SRTLA_TYPE_REG_NAK,
    SRTLA_TYPE_REG_NGP, SRTLA_TYPE_REG1, SRTLA_TYPE_REG1_LEN, SRTLA_TYPE_REG2, SRTLA_TYPE_REG2_LEN,
    SRTLA_TYPE_REG3, SRTLA_TYPE_REG3_LEN, create_ack_packet, create_reg1_packet,
    create_reg2_packet, parse_srtla_ack,
};

/// `RECV_ACK_INT` in the receiver (`srtla_rec.c`): the fixed number of
/// per-connection sequence numbers carried in one SRTLA ACK frame.
const RECV_ACK_INT: usize = 10;

/// `sizeof(srtla_ack_pkt)` = `sizeof(u32 type) + sizeof(u32 acks[10])` = 44.
const ACK_PKT_LEN: usize = 4 + 4 * RECV_ACK_INT;

// ---------------------------------------------------------------------------
// 1. Type codes — exact hex, big-endian wire order (common.h)
// ---------------------------------------------------------------------------

#[test]
fn type_codes_match_common_h() {
    assert_eq!(SRTLA_TYPE_KEEPALIVE, 0x9000, "KEEPALIVE type code drift");
    assert_eq!(SRTLA_TYPE_ACK, 0x9100, "ACK type code drift");
    assert_eq!(SRTLA_TYPE_REG1, 0x9200, "REG1 type code drift");
    assert_eq!(SRTLA_TYPE_REG2, 0x9201, "REG2 type code drift");
    assert_eq!(SRTLA_TYPE_REG3, 0x9202, "REG3 type code drift");
    assert_eq!(SRTLA_TYPE_REG_ERR, 0x9210, "REG_ERR type code drift");
    assert_eq!(SRTLA_TYPE_REG_NGP, 0x9211, "REG_NGP type code drift");
    assert_eq!(SRTLA_TYPE_REG_NAK, 0x9212, "REG_NAK type code drift");
}

#[test]
fn type_codes_serialize_big_endian_on_the_wire() {
    // The receiver sends headers via `htobe16(type)`; our builders use
    // `to_be_bytes()`. Pin the resulting on-wire byte pairs so a host-endian
    // regression (little-endian leak) is caught.
    assert_eq!(SRTLA_TYPE_KEEPALIVE.to_be_bytes(), [0x90, 0x00]);
    assert_eq!(SRTLA_TYPE_ACK.to_be_bytes(), [0x91, 0x00]);
    assert_eq!(SRTLA_TYPE_REG1.to_be_bytes(), [0x92, 0x00]);
    assert_eq!(SRTLA_TYPE_REG2.to_be_bytes(), [0x92, 0x01]);
    assert_eq!(SRTLA_TYPE_REG3.to_be_bytes(), [0x92, 0x02]);
    assert_eq!(SRTLA_TYPE_REG_ERR.to_be_bytes(), [0x92, 0x10]);
    assert_eq!(SRTLA_TYPE_REG_NGP.to_be_bytes(), [0x92, 0x11]);
    assert_eq!(SRTLA_TYPE_REG_NAK.to_be_bytes(), [0x92, 0x12]);
}

// ---------------------------------------------------------------------------
// 2. Sizes — SRTLA_ID_LEN and REG frame lengths (common.h)
// ---------------------------------------------------------------------------

#[test]
fn srtla_id_len_is_256() {
    assert_eq!(SRTLA_ID_LEN, 256, "SRTLA_ID_LEN drift from common.h");
}

#[test]
fn reg1_reg2_frame_is_258_bytes() {
    // common.h: `SRTLA_TYPE_REG1_LEN = (2 + SRTLA_ID_LEN)` = 258.
    assert_eq!(SRTLA_TYPE_REG1_LEN, 258, "REG1 frame length drift");
    assert_eq!(SRTLA_TYPE_REG2_LEN, 258, "REG2 frame length drift");
    assert_eq!(SRTLA_TYPE_REG1_LEN, 2 + SRTLA_ID_LEN);
    assert_eq!(SRTLA_TYPE_REG2_LEN, 2 + SRTLA_ID_LEN);
}

#[test]
fn reg3_frame_is_2_bytes() {
    // common.h: `SRTLA_TYPE_REG3_LEN = 2` (bare type, no body).
    assert_eq!(SRTLA_TYPE_REG3_LEN, 2, "REG3 frame length drift");
}

#[test]
fn ack_layout_is_44_bytes_type_plus_ten_acks() {
    // srtla_rec.c: `struct { uint32_t type; uint32_t acks[10]; }`.
    assert_eq!(RECV_ACK_INT, 10);
    assert_eq!(
        ACK_PKT_LEN, 44,
        "ACK struct = 4 (type) + 40 (10x u32 acks) = 44"
    );
}

// ---------------------------------------------------------------------------
// 3. Builders produce wire-exact bytes
// ---------------------------------------------------------------------------

#[test]
fn reg1_builder_matches_wire_layout() {
    // Distinct per-byte id so a misplaced copy is visible.
    let mut id = [0u8; SRTLA_ID_LEN];
    for (i, b) in id.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let pkt = create_reg1_packet(&id);

    assert_eq!(pkt.len(), 258, "REG1 frame must be 258 bytes");
    // Header: htobe16(SRTLA_TYPE_REG1) at bytes 0-1.
    assert_eq!(&pkt[0..2], &[0x92, 0x00], "REG1 header bytes");
    // Body: full 256-byte id at bytes 2..258.
    assert_eq!(&pkt[2..], &id[..], "REG1 id body must be the id verbatim");
}

#[test]
fn reg2_builder_matches_wire_layout() {
    let mut id = [0u8; SRTLA_ID_LEN];
    for (i, b) in id.iter_mut().enumerate() {
        *b = (255 - (i & 0xff)) as u8;
    }
    let pkt = create_reg2_packet(&id);

    assert_eq!(pkt.len(), 258, "REG2 frame must be 258 bytes");
    // Header: htobe16(SRTLA_TYPE_REG2) at bytes 0-1.
    assert_eq!(&pkt[0..2], &[0x92, 0x01], "REG2 header bytes");
    assert_eq!(&pkt[2..], &id[..], "REG2 id body must be the id verbatim");
}

#[test]
fn ack_builder_matches_wire_layout() {
    // ack.type = htobe32(SRTLA_TYPE_ACK << 16) = 0x9100_0000
    // => on-wire bytes [0x91, 0x00, 0x00, 0x00]; then 10 big-endian acks.
    let acks: [u32; 10] = [
        0x0000_0001,
        0x0000_00ff,
        0x0000_abcd,
        0x1234_5678,
        0x7fff_ffff,
        0x0000_0000,
        0xdead_beef,
        0x0010_0000,
        0x00ff_ff00,
        0xcafe_babe,
    ];
    let pkt = create_ack_packet(&acks);

    assert_eq!(
        pkt.len(),
        ACK_PKT_LEN,
        "ACK frame must be exactly 44 bytes for 10 acks"
    );

    // Type field (4 bytes): high u16 = 0x9100, low u16 = 0x0000.
    assert_eq!(
        &pkt[0..4],
        &[0x91, 0x00, 0x00, 0x00],
        "ACK type word must be htobe32(0x9100 << 16)"
    );

    // Each ack at offset 4 + i*4, big-endian, in order.
    for (i, &ack) in acks.iter().enumerate() {
        let off = 4 + i * 4;
        assert_eq!(
            &pkt[off..off + 4],
            &ack.to_be_bytes(),
            "ACK seq #{i} must be big-endian at offset {off}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Parser reads a wire-shaped ACK
// ---------------------------------------------------------------------------

#[test]
fn parser_reads_wire_shaped_ack() {
    // Construct the frame EXACTLY as srtla_rec.c emits it (independent of our
    // own builder), then assert our parser recovers all 10 sequence numbers.
    let seqs: [u32; 10] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 0x7fff_ffff];

    let mut frame = [0u8; ACK_PKT_LEN];
    // ack.type = htobe32(SRTLA_TYPE_ACK << 16)
    frame[0..4].copy_from_slice(&((u32::from(SRTLA_TYPE_ACK)) << 16).to_be_bytes());
    // ack.acks[i] = htobe32(sn)
    for (i, &sn) in seqs.iter().enumerate() {
        let off = 4 + i * 4;
        frame[off..off + 4].copy_from_slice(&sn.to_be_bytes());
    }

    let parsed = parse_srtla_ack(&frame);
    assert_eq!(
        parsed.as_slice(),
        &seqs[..],
        "parser must recover all 10 ACK seqs in order"
    );
}

#[test]
fn ack_builder_parser_roundtrip() {
    // Our own builder -> our own parser must round-trip the full 10-ack vector,
    // confirming both ends agree on the 44-byte layout.
    let acks: [u32; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 0xffff_fffe];
    let pkt = create_ack_packet(&acks);
    let parsed = parse_srtla_ack(&pkt);
    assert_eq!(parsed.as_slice(), &acks[..], "ACK build->parse round-trip");
}
