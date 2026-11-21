use craft_anything_formats::{FormatError, arsc, axml};
use craft_anything_testkit::android as fx;

#[test]
fn axml_rejects_wrong_root_chunk_type_and_short_header() {
    let mut wrong_type = fx::manifest_axml();
    wrong_type[0..2].copy_from_slice(&0x0001u16.to_le_bytes());
    assert!(matches!(axml::parse(&wrong_type), Err(FormatError::Malformed(_))));

    let mut short_header = fx::manifest_axml();
    short_header[2..4].copy_from_slice(&4u16.to_le_bytes());
    assert!(matches!(axml::parse(&short_header), Err(FormatError::Truncated(_))));
}

#[test]
fn axml_to_xml_respects_max_bytes_budget() {
    let doc = axml::parse(&fx::manifest_axml()).unwrap();
    let xml = doc.to_xml(10);
    assert!(xml.ends_with("<!-- truncated -->\n"), "{xml}");
}

#[test]
fn arsc_rejects_wrong_root_chunk_type_and_short_header() {
    let mut wrong_type = fx::resources_arsc();
    wrong_type[0..2].copy_from_slice(&0x0003u16.to_le_bytes());
    assert!(matches!(arsc::parse(&wrong_type, arsc::Limits::default()), Err(FormatError::Malformed(_))));

    let mut short_header = fx::resources_arsc();
    short_header[2..4].copy_from_slice(&4u16.to_le_bytes());
    assert!(matches!(arsc::parse(&short_header, arsc::Limits::default()), Err(FormatError::Truncated(_) | FormatError::Malformed(_))));
}

#[test]
fn arsc_respects_max_entries_limit() {
    let table = arsc::parse(&fx::resources_arsc(), arsc::Limits { max_entries: 1 }).unwrap();
    assert!(table.truncated);
    assert_eq!(table.entries.len(), 1);
}
