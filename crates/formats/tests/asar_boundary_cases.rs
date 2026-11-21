use craft_anything_formats::asar::{self, AsarError, AsarKind, MAX_HEADER_BYTES};
use serde_json::{Value, json};

fn archive_with_json(json: &[u8]) -> Vec<u8> {
    let pad = (4 - json.len() % 4) % 4;
    let payload = json.len() + 4 + pad;
    let mut out = Vec::new();
    for word in [4, payload + 4, payload, json.len()] {
        out.extend_from_slice(&(word as u32).to_le_bytes());
    }
    out.extend_from_slice(json);
    out.extend(std::iter::repeat_n(0u8, pad));
    out
}

fn archive_from_value(files: Value) -> Vec<u8> {
    let json = serde_json::to_vec(&json!({ "files": files })).unwrap();
    archive_with_json(&json)
}

#[test]
fn header_over_max_bytes_is_rejected() {
    let oversized = MAX_HEADER_BYTES + 1;
    let mut header = Vec::new();
    header.extend_from_slice(&4u32.to_le_bytes());
    header.extend_from_slice(&((oversized + 8) as u32).to_le_bytes());
    header.extend_from_slice(&((oversized + 4) as u32).to_le_bytes());
    header.extend_from_slice(&(oversized as u32).to_le_bytes());
    assert_eq!(asar::parse(&header, 10), Err(AsarError::HeaderTooLarge(oversized)));
}

#[test]
fn non_object_json_headers_are_rejected() {
    for raw in [b"".as_slice(), b"[]", b"123", b"\"str\""] {
        let bytes = archive_with_json(raw);
        assert!(matches!(asar::parse(&bytes, 10), Err(AsarError::NotAsar(_))), "raw {:?}", String::from_utf8_lossy(raw));
    }
}

#[test]
fn file_bytes_rejects_non_files_unpacked_and_overflowing_ranges() {
    let bytes = archive_from_value(json!({
        "dir": { "files": {} },
        "link": { "link": "dir" },
        "unpacked_with_offset": { "size": 1, "offset": "0", "unpacked": true },
        "overflow_offset": { "size": 1, "offset": u64::MAX.to_string() },
        "huge_size": { "size": u64::MAX, "offset": "0" }
    }));
    let archive = asar::parse(&bytes, 10).unwrap();

    let dir = archive.get("dir").unwrap();
    assert_eq!(dir.kind, AsarKind::Directory);
    assert_eq!(asar::file_bytes(&bytes, &archive, dir), None);

    let link = archive.get("link").unwrap();
    assert_eq!(link.kind, AsarKind::Link);
    assert_eq!(asar::file_bytes(&bytes, &archive, link), None);

    let unpacked = archive.get("unpacked_with_offset").unwrap();
    assert!(unpacked.unpacked);
    assert_eq!(unpacked.offset, None);
    assert_eq!(asar::file_bytes(&bytes, &archive, unpacked), None);

    assert_eq!(asar::file_bytes(&bytes, &archive, archive.get("overflow_offset").unwrap()), None);
    assert_eq!(asar::file_bytes(&bytes, &archive, archive.get("huge_size").unwrap()), None);
}

#[test]
fn empty_dot_backslash_and_nul_names_are_rejected() {
    let bytes = archive_from_value(json!({
        "": { "size": 0, "offset": "0" },
        ".": { "size": 0, "offset": "0" },
        "a\\b": { "size": 0, "offset": "0" },
        "a\0b": { "size": 0, "offset": "0" },
        "ok": { "size": 0, "offset": "0" }
    }));
    let archive = asar::parse(&bytes, 10).unwrap();
    assert_eq!(archive.entries.len(), 1);
    assert!(archive.get("ok").is_some());
    assert_eq!(archive.rejected_names.len(), 4);
}
