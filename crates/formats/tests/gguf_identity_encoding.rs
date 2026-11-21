use craft_anything_formats::ai_model::{self, ModelReport};

/// Emit integers in either byte order supported by GGUF version 3.
fn integer(bytes: &mut Vec<u8>, value: u64, width: usize, big: bool) {
    let encoded = if big { value.to_be_bytes() } else { value.to_le_bytes() };
    bytes.extend_from_slice(if big { &encoded[8 - width..] } else { &encoded[..width] });
}

/// A length-prefixed string can carry invalid bytes in a damaged model.
fn string(bytes: &mut Vec<u8>, value: &[u8], big: bool) {
    integer(bytes, value.len() as u64, 8, big);
    bytes.extend_from_slice(value);
}

/// Build complete metadata and tensor records without model weights. An architecture key comes
/// first, so a test can see that readable metadata survives a bad name.
fn model(keys: &[&[u8]], tensors: &[(&[u8], &[u64])], big: bool) -> Vec<u8> {
    let mut bytes = b"GGUF".to_vec();
    integer(&mut bytes, 3, 4, big);
    integer(&mut bytes, tensors.len() as u64, 8, big);
    integer(&mut bytes, keys.len() as u64 + 1, 8, big);
    string(&mut bytes, b"general.architecture", big);
    integer(&mut bytes, 8, 4, big);
    string(&mut bytes, b"llama", big);
    for key in keys {
        string(&mut bytes, key, big);
        integer(&mut bytes, 4, 4, big);
        integer(&mut bytes, 1, 4, big);
    }
    for (i, (name, shape)) in tensors.iter().enumerate() {
        string(&mut bytes, name, big);
        integer(&mut bytes, shape.len() as u64, 4, big);
        for d in *shape {
            integer(&mut bytes, *d, 8, big);
        }
        integer(&mut bytes, 0, 4, big);
        integer(&mut bytes, i as u64 * 32, 8, big);
    }
    bytes
}

/// Parse a fixture whose file is a little longer than its header.
fn parse(bytes: &[u8]) -> ModelReport {
    ai_model::parse(bytes, bytes.len() as u64 + 4096).unwrap()
}

/// Metadata keys as (text, flagged) pairs, in file order.
fn keys(report: &ModelReport) -> Vec<(&str, bool)> {
    report.metadata.iter().map(|m| (m.key.as_str(), m.key_not_utf8)).collect()
}

/// Tensor names as (text, flagged) pairs, in file order.
fn names(report: &ModelReport) -> Vec<(&str, bool)> {
    report.tensors.iter().map(|t| (t.name.as_str(), t.name_not_utf8)).collect()
}

/// A bad key or tensor name is kept escaped and flagged; the rest of the model is still reported.
#[test]
fn invalid_names_are_kept_distinct_and_flagged() {
    for big in [false, true] {
        let bytes = model(&[b"valid.key", b"bad.\xff", b"bad.\xfe"], &[(b"valid", &[4]), (b"bad.\xff", &[4]), (b"bad.\xfe", &[4])], big);
        let report = parse(&bytes);
        assert!(report.complete);
        assert_eq!(report.architecture.as_deref(), Some("llama"));
        assert_eq!(keys(&report), [("general.architecture", false), ("valid.key", false), ("bad.\\xff", true), ("bad.\\xfe", true)]);
        assert_eq!(names(&report), [("valid", false), ("bad.\\xff", true), ("bad.\\xfe", true)]);
        assert_eq!((report.metadata_read, report.tensors_read, report.parameters), (4, 3, 12));
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].starts_with("2 metadata key(s) and 2 tensor name(s) aren't valid UTF-8"), "{:?}", report.warnings);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["metadata"][2]["key_not_utf8"], true);
        assert_eq!(json["tensors"][1]["name_not_utf8"], true);
    }
}

/// Lone continuation bytes, overlong and cut-short sequences, and a bad byte past the display
/// limit are all flagged; the valid tensor beside each one still counts.
#[test]
fn every_kind_of_invalid_sequence_is_flagged() {
    let mut long = vec![b'x'; 1100];
    long.push(0xff);
    for big in [false, true] {
        for (name, shown) in [(b"bad.\x80".as_slice(), "bad.\\x80"), (b"bad.\xc0\xaf", "bad.\\xc0\\xaf"), (b"bad.\xe2\x82", "bad.\\xe2\\x82")] {
            let report = parse(&model(&[], &[(b"valid", &[4]), (name, &[4])], big));
            assert_eq!(names(&report), [("valid", false), (shown, true)]);
            assert_eq!(report.parameters, 8);
        }
        let report = parse(&model(&[], &[(b"valid", &[4]), (&long, &[4])], big));
        assert_eq!(report.tensors.len(), 2);
        assert!(report.tensors[1].name_not_utf8);
        assert_eq!(report.tensors[1].name.chars().count(), 1024);
        assert!(report.warnings[0].starts_with("0 metadata key(s) and 1 tensor name(s)"), "{:?}", report.warnings);
    }
}

/// A backslash in a bad name is escaped too, so names that differ only in where an escape-like
/// run sits stay apart. A valid name spelled like an escape keeps its text and isn't flagged.
#[test]
fn escaped_names_cannot_collide() {
    for big in [false, true] {
        let tensors: [(&[u8], &[u64]); 3] = [(b"\\xff\xfe", &[4]), (b"\xff\\xfe", &[4]), (b"bad.\\xff", &[4])];
        let report = parse(&model(&[b"bad.\xff", b"bad.\\xff"], &tensors, big));
        assert_eq!(names(&report), [("\\\\xff\\xfe", true), ("\\xff\\\\xfe", true), ("bad.\\xff", false)]);
        assert_eq!(keys(&report)[1..], [("bad.\\xff", true), ("bad.\\xff", false)]);
    }
}

/// The summary warning comes first, so a list already full of other warnings can't drop it.
#[test]
fn the_summary_warning_survives_a_full_list() {
    let mut tensors: Vec<(Vec<u8>, Vec<u64>)> = (0..40).map(|i| (format!("big.{i}").into_bytes(), vec![u64::MAX, 2])).collect();
    tensors.push((b"bad.\xff".to_vec(), vec![4]));
    let tensors: Vec<(&[u8], &[u64])> = tensors.iter().map(|(n, s)| (n.as_slice(), s.as_slice())).collect();
    for big in [false, true] {
        let report = parse(&model(&[], &tensors, big));
        assert_eq!(report.warnings.len(), 32);
        assert!(report.warnings[0].starts_with("0 metadata key(s) and 1 tensor name(s)"), "{:?}", report.warnings[0]);
        assert!(report.tensors[40].name_not_utf8);
    }
}

/// Valid multibyte names, including an actual replacement character and a backslash, stay as
/// they are, with no flag and no warning.
#[test]
fn valid_unicode_names_are_preserved() {
    for big in [false, true] {
        let bytes = model(&["键.test".as_bytes()], &[("层.weight".as_bytes(), &[4]), ("actual.\u{fffd}".as_bytes(), &[4]), (b"back\\slash", &[4])], big);
        let report = parse(&bytes);
        assert!(report.complete);
        assert_eq!(keys(&report), [("general.architecture", false), ("键.test", false)]);
        assert_eq!(names(&report), [("层.weight", false), ("actual.\u{fffd}", false), ("back\\slash", false)]);
        assert_eq!(report.parameters, 12);
        assert!(report.warnings.is_empty());
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("not_utf8"));
    }
}
