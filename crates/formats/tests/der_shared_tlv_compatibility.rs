//! The partial shared-reader cutover preserves offsets and the app's definite-length contract.
use craft_anything_formats::der;

#[test]
fn shared_tlv_preserves_offsets_raw_bytes_and_length_notes() {
    for (encoding, non_minimal) in [
        (vec![4, 3, 1, 2, 3], false),
        (vec![4, 0x81, 3, 1, 2, 3], true),
        (vec![4, 0x82, 0, 3, 1, 2, 3], true),
        ([vec![4, 0x81, 128], vec![0; 128]].concat(), false),
    ] {
        let mut bytes = vec![0xaa, 0xbb];
        bytes.extend_from_slice(&encoding);
        bytes.push(0xcc);
        let value = der::read(&bytes, 2).expect("definite-length element");
        assert_eq!(value.start, 2);
        assert_eq!(value.end(), 2 + encoding.len());
        assert_eq!(value.whole(&bytes), Some(encoding.as_slice()));
        assert_eq!(value.non_minimal_length, non_minimal);
        assert_eq!(value.bytes(&bytes).unwrap().len(), value.len);
    }
}

#[test]
fn shared_tlv_keeps_definite_only_policy() {
    // A valid BER indefinite SEQUENCE is outside this application's DER reader contract.
    assert!(der::read(&[0x30, 0x80, 0x05, 0, 0, 0], 0).is_none());
    assert!(der::read(&[0x30, 2, 0x05, 0], 0).is_some());
}

#[test]
fn shared_inspection_metadata_matches_each_benign_sibling_at_original_offsets() {
    let elements = [vec![2, 1, 7], vec![4, 0x81, 2, 8, 9], vec![0x30, 2, 5, 0]];
    let payload = elements.concat();
    let mut bytes = vec![0xaa, 0xbb];
    bytes.extend_from_slice(&payload);
    let mut cursor = craft_pkcs7::inspection::Cursor::new(&bytes[2..]);
    let mut offset = 2;
    for encoding in elements {
        let shared = cursor.next_element().unwrap().unwrap();
        let local = der::read(&bytes, offset).unwrap();
        assert_eq!(local.tag, shared.tag());
        assert_eq!(local.whole(&bytes), Some(shared.raw()));
        assert_eq!(local.bytes(&bytes), Some(shared.value()));
        assert_eq!(local.non_minimal_length, shared.non_minimal_length());
        assert_eq!(local.start, offset);
        assert_eq!(local.end(), offset + encoding.len());
        offset = local.end();
    }
    assert!(cursor.next_element().unwrap().is_none());
    assert!(der::read(&bytes, offset).is_none());
}
