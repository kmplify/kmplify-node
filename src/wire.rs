//! Binary frames for bulk payload on the gateway link (protocol v4).
//!
//! Control stays JSON. What moves bytes (a chunk of a relayed response, of a
//! streamed upload, or of a relayed WebSocket) used to travel as JSON text
//! with the payload base64-encoded inside it: a third more bytes on the wire,
//! an encode on one side, a decode and a JSON parse of a megabyte-long string
//! on the other, for every chunk. Those frames now go out as WebSocket BINARY
//! messages with a fixed 18 byte header and the payload raw behind it.
//!
//! ```text
//! byte 0      kind   (see the KIND_* constants)
//! byte 1      flags  (bit 0: the relayed WebSocket message was binary)
//! bytes 2..18 stream id, the 16 raw bytes of its 32 character hex form
//! bytes 18..  payload
//! ```
//!
//! Negotiated in both directions: the hello says `"binary_frames": true`, and
//! so does the welcome. Each side sends binary only to a peer that said it
//! reads it, and the JSON forms stay valid forever, so every older pairing
//! keeps working unchanged.

use std::sync::atomic::{AtomicBool, Ordering};

pub const HEADER_LEN: usize = 18;

/// node -> gateway: a chunk of a relayed HTTP response (`http_resp_chunk`).
pub const KIND_HTTP_RESP_CHUNK: u8 = 1;
/// gateway -> node: a chunk of a streamed upload (`http_req_chunk`).
pub const KIND_HTTP_REQ_CHUNK: u8 = 2;
/// node -> gateway: a message from a relayed WebSocket (`ws_recv`).
pub const KIND_WS_RECV: u8 = 3;
/// gateway -> node: a message for a relayed WebSocket (`ws_send`).
pub const KIND_WS_SEND: u8 = 4;

/// The relayed WebSocket message was a binary one (else text).
pub const FLAG_BINARY: u8 = 1;

static PEER_READS_BINARY: AtomicBool = AtomicBool::new(false);

/// Record what the gateway's `welcome` said. Called once per connection.
pub fn set_peer_reads_binary(yes: bool) {
    PEER_READS_BINARY.store(yes, Ordering::Relaxed);
}

pub fn peer_reads_binary() -> bool {
    PEER_READS_BINARY.load(Ordering::Relaxed)
}

/// Build a binary frame, or None when `stream_id` is not the 32 hex
/// characters the header can carry (then the caller sends the JSON form).
pub fn pack(kind: u8, flags: u8, stream_id: &str, payload: &[u8]) -> Option<Vec<u8>> {
    let id = hex16(stream_id)?;
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&id);
    out.extend_from_slice(payload);
    Some(out)
}

/// A received binary frame, borrowed from the message that carried it.
pub struct Frame<'a> {
    pub kind: u8,
    pub flags: u8,
    pub stream_id: String,
    pub payload: &'a [u8],
}

pub fn unpack(data: &[u8]) -> Option<Frame<'_>> {
    if data.len() < HEADER_LEN {
        return None;
    }
    let mut stream_id = String::with_capacity(32);
    for b in &data[2..HEADER_LEN] {
        stream_id.push_str(&format!("{b:02x}"));
    }
    Some(Frame {
        kind: data[0],
        flags: data[1],
        stream_id,
        payload: &data[HEADER_LEN..],
    })
}

fn hex16(s: &str) -> Option<[u8; 16]> {
    let bytes = s.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_frame_survives_the_round_trip() {
        let payload: Vec<u8> = (0..=255u8).collect();
        let packed = pack(KIND_WS_RECV, FLAG_BINARY, ID, &payload).unwrap();
        assert_eq!(packed.len(), HEADER_LEN + payload.len());
        let f = unpack(&packed).unwrap();
        assert_eq!((f.kind, f.flags), (KIND_WS_RECV, FLAG_BINARY));
        assert_eq!(f.stream_id, ID);
        assert_eq!(f.payload, &payload[..]);
    }

    /// Byte for byte what the gateway's app/wire.py produces for the same
    /// input. If either side changes its layout, this is where it shows.
    #[test]
    fn the_layout_is_the_one_the_gateway_uses() {
        let packed = pack(KIND_HTTP_RESP_CHUNK, 0, ID, b"hi").unwrap();
        assert_eq!(
            packed,
            [
                1, 0, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
                0xab, 0xcd, 0xef, b'h', b'i'
            ]
        );
    }

    #[test]
    fn an_empty_payload_is_a_valid_frame() {
        let packed = pack(KIND_HTTP_REQ_CHUNK, 0, ID, b"").unwrap();
        assert_eq!(unpack(&packed).unwrap().payload.len(), 0);
    }

    /// Ids the header cannot carry fall back to JSON instead of being mangled.
    #[test]
    fn an_id_that_is_not_32_hex_characters_is_not_packed() {
        assert!(pack(1, 0, "short", b"x").is_none());
        assert!(pack(1, 0, "0123456789abcdef0123456789abcdeg", b"x").is_none());
        assert!(pack(1, 0, "", b"x").is_none());
    }

    #[test]
    fn a_runt_is_refused() {
        assert!(unpack(&[1, 0, 2]).is_none());
        assert!(unpack(&[]).is_none());
    }
}
