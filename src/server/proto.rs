//! Binary WebSocket frame codec shared by server and (later) the wasm client.
//!
//! Frame layout (little-endian):
//! ```text
//! [ u8 type ][ u32 req_id ][ u32 flags ][ u32 len ][ payload… ]
//! ```
//! `req_id` multiplexes request/response on one socket (0 = unsolicited/push).
//! `flags` bit 0 = LAST for chunked messages.

pub const HEADER_LEN: usize = 13;
pub const FLAG_LAST: u32 = 1;

// server -> client, /data channel
pub const EV_EVENT: u8 = 0x01;
pub const EV_THUMB: u8 = 0x02;
pub const EV_THUMB_BATCH: u8 = 0x03;
pub const EV_PONG: u8 = 0x06;

// client -> server, /data channel
pub const CL_HELLO: u8 = 0x10;
pub const CL_WANT: u8 = 0x11;
pub const CL_VIEWPORT: u8 = 0x12;
pub const CL_FOCUS: u8 = 0x13;
pub const CL_ACTION: u8 = 0x14;
pub const CL_CREDIT: u8 = 0x15;
pub const CL_PAUSE: u8 = 0x16;
pub const CL_RESUME: u8 = 0x17;
pub const CL_PING: u8 = 0x18;
pub const CL_CANCEL: u8 = 0x19;

// /bulk channel
pub const BK_MANIFEST: u8 = 0x20;
pub const BK_FULL_CHUNK: u8 = 0x21;
pub const BK_FULL_HEAD: u8 = 0x22;
pub const BK_MANIFEST_PATCH: u8 = 0x23;
pub const BK_GET_MANIFEST: u8 = 0x28;
pub const BK_GET_FULL: u8 = 0x29;
// BK_CANCEL reuses CL_CANCEL (0x19).

/// Chunk size for bulk transfers (full images, manifests).
pub const BULK_CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: u8,
    pub req_id: u32,
    pub flags: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(msg_type: u8, req_id: u32, payload: Vec<u8>) -> Self {
        Self {
            msg_type,
            req_id,
            flags: 0,
            payload,
        }
    }

    pub fn last(mut self) -> Self {
        self.flags |= FLAG_LAST;
        self
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(self.msg_type);
        out.extend_from_slice(&self.req_id.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode one frame; returns None on malformed input.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let msg_type = buf[0];
        let req_id = u32::from_le_bytes(buf[1..5].try_into().ok()?);
        let flags = u32::from_le_bytes(buf[5..9].try_into().ok()?);
        let len = u32::from_le_bytes(buf[9..13].try_into().ok()?) as usize;
        if buf.len() != HEADER_LEN + len {
            return None;
        }
        Some(Self {
            msg_type,
            req_id,
            flags,
            payload: buf[HEADER_LEN..].to_vec(),
        })
    }
}

/// THUMB payload: {u32 id, u64 sig, jpeg bytes}
pub fn thumb_payload(id: u32, sig: u64, jpeg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + jpeg.len());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&sig.to_le_bytes());
    out.extend_from_slice(jpeg);
    out
}

/// THUMB_BATCH payload: {u16 count} × {u32 id, u64 sig, u32 len, bytes}
pub fn thumb_batch_payload(thumbs: &[(u32, u64, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(thumbs.len() as u16).to_le_bytes());
    for (id, sig, jpeg) in thumbs {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&sig.to_le_bytes());
        out.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        out.extend_from_slice(jpeg);
    }
    out
}

/// WANT payload: {u16 count} × u32 id — returns ids in order.
pub fn decode_want(payload: &[u8]) -> Option<Vec<u32>> {
    if payload.len() < 2 {
        return None;
    }
    let n = u16::from_le_bytes(payload[0..2].try_into().ok()?) as usize;
    if payload.len() != 2 + n * 4 {
        return None;
    }
    Some(
        payload[2..]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

/// VIEWPORT payload: {u32 first, u32 last, i8 dir}
pub fn decode_viewport(payload: &[u8]) -> Option<(u32, u32, i8)> {
    if payload.len() != 9 {
        return None;
    }
    Some((
        u32::from_le_bytes(payload[0..4].try_into().ok()?),
        u32::from_le_bytes(payload[4..8].try_into().ok()?),
        payload[8] as i8,
    ))
}

/// FOCUS payload: {u32 id}
pub fn decode_focus(payload: &[u8]) -> Option<u32> {
    if payload.len() != 4 {
        return None;
    }
    Some(u32::from_le_bytes(payload[0..4].try_into().ok()?))
}

/// CREDIT payload: {u64 bytes}
pub fn decode_credit(payload: &[u8]) -> Option<u64> {
    if payload.len() != 8 {
        return None;
    }
    Some(u64::from_le_bytes(payload[0..8].try_into().ok()?))
}

/// FULL_HEAD payload on the wire: {u64 sig, u32 w, u32 h, u8 fmt, u64 total}
pub fn full_head_payload(sig: u64, w: u32, h: u32, fmt: u8, total: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(25);
    out.extend_from_slice(&sig.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.push(fmt);
    out.extend_from_slice(&total.to_le_bytes());
    out
}

/// GET_FULL payload: {u32 id, u64 sig, u8 want_fmt, u8 q}
pub fn decode_get_full(payload: &[u8]) -> Option<(u32, u64, u8, u8)> {
    if payload.len() != 14 {
        return None;
    }
    Some((
        u32::from_le_bytes(payload[0..4].try_into().ok()?),
        u64::from_le_bytes(payload[4..12].try_into().ok()?),
        payload[12],
        payload[13],
    ))
}
