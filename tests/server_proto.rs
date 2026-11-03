use imagecropper::server::proto::*;

#[test]
fn frame_roundtrip() {
    let f = Frame::new(EV_THUMB, 42, vec![1, 2, 3, 4]).last();
    let encoded = f.encode();
    let decoded = Frame::decode(&encoded).unwrap();
    assert_eq!(decoded, f);
}

#[test]
fn frame_decode_rejects_malformed() {
    assert!(Frame::decode(&[]).is_none());
    assert!(Frame::decode(&[0; HEADER_LEN - 1]).is_none());
    // Declared length != actual length.
    let mut buf = Frame::new(1, 1, vec![9; 10]).encode();
    buf.truncate(HEADER_LEN + 5);
    assert!(Frame::decode(&buf).is_none());
    let mut buf = Frame::new(1, 1, vec![9; 10]).encode();
    buf[9] = 200; // len field now says 200
    assert!(Frame::decode(&buf).is_none());
}

#[test]
fn frame_flags_last() {
    let f = Frame::new(BK_MANIFEST, 7, vec![]).last();
    assert_eq!(f.flags & FLAG_LAST, FLAG_LAST);
    let decoded = Frame::decode(&f.encode()).unwrap();
    assert_eq!(decoded.flags & FLAG_LAST, FLAG_LAST);
}

#[test]
fn thumb_payload_layout() {
    let jpeg = b"\xff\xd8data".to_vec();
    let p = thumb_payload(0xAB, 0x1122334455667788, &jpeg);
    assert_eq!(&p[0..4], &0xABu32.to_le_bytes());
    assert_eq!(&p[4..12], &0x1122334455667788u64.to_le_bytes());
    assert_eq!(&p[12..], jpeg.as_slice());
}

#[test]
fn thumb_batch_payload_layout() {
    let thumbs = vec![(1u32, 10u64, vec![0xAA; 3]), (2u32, 20u64, vec![0xBB; 5])];
    let p = thumb_batch_payload(&thumbs);
    assert_eq!(u16::from_le_bytes(p[0..2].try_into().unwrap()), 2);
    let mut off = 2;
    assert_eq!(u32::from_le_bytes(p[off..off + 4].try_into().unwrap()), 1);
    off += 4 + 8;
    assert_eq!(u32::from_le_bytes(p[off..off + 4].try_into().unwrap()), 3);
    off += 4 + 3;
    assert_eq!(u32::from_le_bytes(p[off..off + 4].try_into().unwrap()), 2);
    off += 4 + 8;
    assert_eq!(u32::from_le_bytes(p[off..off + 4].try_into().unwrap()), 5);
    assert_eq!(p.len(), off + 4 + 5);
}

#[test]
fn want_codec() {
    let ids = vec![5u32, 9, 100000];
    let mut p = Vec::new();
    p.extend_from_slice(&(ids.len() as u16).to_le_bytes());
    for id in &ids {
        p.extend_from_slice(&id.to_le_bytes());
    }
    assert_eq!(decode_want(&p).unwrap(), ids);
    assert!(decode_want(&p[..3]).is_none()); // truncated
    assert!(decode_want(&[]).is_none());
}

#[test]
fn viewport_codec() {
    let mut p = Vec::new();
    p.extend_from_slice(&10u32.to_le_bytes());
    p.extend_from_slice(&40u32.to_le_bytes());
    p.push(0xFF); // i8 -1
    assert_eq!(decode_viewport(&p), Some((10, 40, -1)));
    assert!(decode_viewport(&p[..7]).is_none());
}

#[test]
fn focus_credit_getfull_codecs() {
    assert_eq!(decode_focus(&7u32.to_le_bytes()), Some(7));
    assert_eq!(decode_credit(&123456u64.to_le_bytes()), Some(123456));
    let mut p = Vec::new();
    p.extend_from_slice(&9u32.to_le_bytes());
    p.extend_from_slice(&0xDEADBEEFu64.to_le_bytes());
    p.push(1); // fmt jpeg
    p.push(85); // q
    assert_eq!(decode_get_full(&p), Some((9, 0xDEADBEEF, 1, 85)));
    assert!(decode_get_full(&p[..10]).is_none());
}

#[test]
fn full_head_layout() {
    let p = full_head_payload(7, 4000, 3000, 1, 999);
    assert_eq!(u64::from_le_bytes(p[0..8].try_into().unwrap()), 7);
    assert_eq!(u32::from_le_bytes(p[8..12].try_into().unwrap()), 4000);
    assert_eq!(u32::from_le_bytes(p[12..16].try_into().unwrap()), 3000);
    assert_eq!(p[16], 1);
    assert_eq!(u64::from_le_bytes(p[17..25].try_into().unwrap()), 999);
}
