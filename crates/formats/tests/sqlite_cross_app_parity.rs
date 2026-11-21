use craft_anything_formats::FormatError;
use craft_anything_formats::sqlite::{CellValue, MAX_ROWS, SqliteDb};

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = vec![u8::try_from(value & 0x7f).unwrap_or(0)];
    value >>= 7;
    while value != 0 {
        out.push(u8::try_from(value & 0x7f).unwrap_or(0) | 0x80);
        value >>= 7;
    }
    out.reverse();
    out
}

fn leaf(page: &mut [u8], base: usize, rowid: u64, payload: &[u8]) {
    page[base] = 0x0d;
    page[base + 3..base + 5].copy_from_slice(&1u16.to_be_bytes());
    let mut cell = varint(u64::try_from(payload.len()).unwrap());
    cell.extend(varint(rowid));
    cell.extend_from_slice(payload);
    let start = page.len() - cell.len();
    page[start..].copy_from_slice(&cell);
    page[base + 5..base + 7].copy_from_slice(&u16::try_from(start).unwrap().to_be_bytes());
    page[base + 8..base + 10].copy_from_slice(&u16::try_from(start).unwrap().to_be_bytes());
}

fn make_page1(page_count: u32, wal_mode: bool) -> Vec<u8> {
    let mut page = vec![0u8; 512];
    page[..16].copy_from_slice(b"SQLite format 3\0");
    page[16..18].copy_from_slice(&512u16.to_be_bytes());
    page[18] = if wal_mode { 2 } else { 1 };
    page[19] = if wal_mode { 2 } else { 1 };
    page[21] = 64;
    page[22] = 32;
    page[23] = 32;
    page[28..32].copy_from_slice(&page_count.to_be_bytes());
    page[44..48].copy_from_slice(&4u32.to_be_bytes());
    page[56..60].copy_from_slice(&1u32.to_be_bytes());
    page
}

fn schema_payload(name: &str, rootpage: u8, sql: &str) -> Vec<u8> {
    let mut header_body = Vec::new();
    let mut body = Vec::new();
    for s in ["table", name, name] {
        header_body.extend(varint(u64::try_from(s.len() * 2 + 13).unwrap()));
        body.extend_from_slice(s.as_bytes());
    }
    header_body.push(1);
    body.push(rootpage);
    header_body.extend(varint(u64::try_from(sql.len() * 2 + 13).unwrap()));
    body.extend_from_slice(sql.as_bytes());
    let mut out = varint(u64::try_from(header_body.len() + 1).unwrap());
    out.extend(header_body);
    out.extend(body);
    out
}

#[test]
fn cross_app_parity_wal_mode_and_shrink_over_larger_main_disclosures() {
    let mut page1 = make_page1(2, true);
    let schema = schema_payload("t", 2, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
    leaf(&mut page1, 100, 1, &schema);

    let mut page2 = vec![0u8; 512];
    leaf(&mut page2, 0, 2, &[3, 0, 23, b'f', b'r', b'e', b's', b'h']);

    // Physically 3 pages while header db_size_pages is 2 (shrink over physically larger main).
    let mut main = Vec::with_capacity(3 * 512);
    main.extend(page1);
    main.extend(page2);
    main.extend(vec![0u8; 512]);

    let db = SqliteDb::parse(&main).unwrap();
    let overview = db.overview();
    assert!(overview.header.wal_mode);
    assert_eq!(overview.header.db_size_pages, 2);
    assert_eq!(overview.page_count_from_size, 3);
    assert!(overview.integrity_notes.iter().any(|n| n.contains("WAL mode")));
    assert!(overview.integrity_notes.iter().any(|n| n.contains("Header records 2 pages but the file holds 3")));

    let page = db.read_rows(2, 0, 10);
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].values, vec![CellValue::Int(2), CellValue::Text { text: "fresh".into(), truncated: false }]);
}

#[test]
fn cross_app_parity_text_blob_null_overflow_and_ordering() {
    let mut page1 = make_page1(5, true);
    let schema =
        schema_payload("samples", 2, "CREATE TABLE samples(id INTEGER PRIMARY KEY, n INTEGER, r REAL, missing, label TEXT, overflow TEXT, binary BLOB)");
    leaf(&mut page1, 100, 1, &schema);

    // Interior page 2 -> leaf 3 (left) and leaf 4 (right).
    let mut page2 = vec![0u8; 512];
    page2[0] = 0x05;
    page2[3..5].copy_from_slice(&1u16.to_be_bytes());
    page2[8..12].copy_from_slice(&4u32.to_be_bytes());
    let mut icell = 3u32.to_be_bytes().to_vec();
    icell.extend(varint(1));
    let icell_off = 512 - icell.len();
    page2[icell_off..].copy_from_slice(&icell);
    page2[5..7].copy_from_slice(&u16::try_from(icell_off).unwrap().to_be_bytes());
    page2[12..14].copy_from_slice(&u16::try_from(icell_off).unwrap().to_be_bytes());

    let overflow_text = "x".repeat(600);
    let mut hdr_body = vec![0u8, 3, 7, 0];
    hdr_body.extend(varint(u64::try_from("Päris".len() * 2 + 13).unwrap()));
    hdr_body.extend(varint(u64::try_from(overflow_text.len() * 2 + 13).unwrap()));
    hdr_body.push(16); // 2-byte blob
    let mut row1 = varint(u64::try_from(hdr_body.len() + 1).unwrap());
    row1.extend(hdr_body);
    row1.extend(&(-8_388_607i32).to_be_bytes()[1..4]);
    row1.extend(1.25f64.to_be_bytes());
    row1.extend("Päris".as_bytes());
    row1.extend(overflow_text.as_bytes());
    row1.extend([0x00, 0xff]);

    let local = craft_sqlite::local_payload_len(512, row1.len() as u64, craft_sqlite::PayloadKind::TableLeaf).unwrap() as usize;
    let mut page3 = vec![0u8; 512];
    page3[0] = 0x0d;
    page3[3..5].copy_from_slice(&1u16.to_be_bytes());
    let mut cell1 = varint(u64::try_from(row1.len()).unwrap());
    cell1.extend(varint(1));
    cell1.extend(&row1[..local]);
    cell1.extend(5u32.to_be_bytes());
    let c1_off = 512 - cell1.len();
    page3[c1_off..].copy_from_slice(&cell1);
    page3[5..7].copy_from_slice(&u16::try_from(c1_off).unwrap().to_be_bytes());
    page3[8..10].copy_from_slice(&u16::try_from(c1_off).unwrap().to_be_bytes());

    let mut page5 = vec![0u8; 512];
    let rem = row1.len() - local;
    page5[4..4 + rem].copy_from_slice(&row1[local..]);

    let mut page4 = vec![0u8; 512];
    leaf(&mut page4, 0, 2, &[8, 0, 8, 9, 0, 13, 13, 12]);

    let mut main = Vec::with_capacity(5 * 512);
    main.extend(page1);
    main.extend(page2);
    main.extend(page3);
    main.extend(page4);
    main.extend(page5);

    let db = SqliteDb::parse(&main).unwrap();
    let page = db.read_rows(2, 0, 10);
    assert!(!page.truncated);
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0].rowid, Some(1));
    assert_eq!(
        page.rows[0].values,
        vec![
            CellValue::Int(1),
            CellValue::Int(-8_388_607),
            CellValue::Real(1.25),
            CellValue::Null,
            CellValue::Text { text: "Päris".into(), truncated: false },
            CellValue::Text { text: overflow_text, truncated: false },
            CellValue::Blob { len: 2, sha256_prefix: "06eb7d6a69ee19e5".into() },
        ]
    );
    assert_eq!(page.rows[1].rowid, Some(2));
    assert_eq!(
        page.rows[1].values,
        vec![
            CellValue::Int(2),
            CellValue::Int(0),
            // Column `r` has REAL affinity: Craft Anything promotes integer-stored 1 to Real(1.0).
            CellValue::Real(1.0),
            CellValue::Null,
            CellValue::Text { text: String::new(), truncated: false },
            CellValue::Text { text: String::new(), truncated: false },
            CellValue::Blob { len: 0, sha256_prefix: "e3b0c44298fc1c14".into() },
        ]
    );

    // Cyclic overflow chain reports partial-read disclosure instead of looping.
    let mut cyclic = main.clone();
    cyclic[4 * 512..4 * 512 + 4].copy_from_slice(&5u32.to_be_bytes());
    let cyclic_local = craft_sqlite::local_payload_len(512, 1200, craft_sqlite::PayloadKind::TableLeaf).unwrap() as usize;
    let mut cyclic_cell = varint(1200);
    cyclic_cell.extend(varint(1));
    cyclic_cell.extend(vec![b'x'; cyclic_local]);
    cyclic_cell.extend(5u32.to_be_bytes());
    let cc_off = 512 - cyclic_cell.len();
    cyclic[2 * 512 + cc_off..3 * 512].copy_from_slice(&cyclic_cell);
    cyclic[2 * 512 + 5..2 * 512 + 7].copy_from_slice(&u16::try_from(cc_off).unwrap().to_be_bytes());
    cyclic[2 * 512 + 8..2 * 512 + 10].copy_from_slice(&u16::try_from(cc_off).unwrap().to_be_bytes());
    let cyclic_page = SqliteDb::parse(&cyclic).unwrap().read_rows(2, 0, 10);
    assert!(cyclic_page.notes.iter().any(|n| n.contains("repeats a page") || n.contains("partially read")));
}

#[test]
fn cross_app_parity_truncation_corruption_and_limits() {
    let mut page1 = make_page1(2, false);
    let schema = schema_payload("t", 2, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
    leaf(&mut page1, 100, 1, &schema);
    let mut page2 = vec![0u8; 512];
    page2[0] = 0x05;
    page2[8..12].copy_from_slice(&2u32.to_be_bytes()); // self-cycle

    let mut main = Vec::with_capacity(2 * 512);
    main.extend(page1);
    main.extend(page2);

    assert!(matches!(SqliteDb::parse(&main[..50]), Err(FormatError::Truncated(_))));
    let mut bad_ps = main.clone();
    bad_ps[16..18].copy_from_slice(&1000u16.to_be_bytes());
    assert!(matches!(SqliteDb::parse(&bad_ps), Err(FormatError::Malformed(_))));
    let mut bad_usable = main.clone();
    bad_usable[20] = 40; // 512 - 40 = 472 < 480
    assert!(matches!(SqliteDb::parse(&bad_usable), Err(FormatError::Malformed(_))));
    let partial_len = SqliteDb::parse(&main[..600]).unwrap().overview();
    assert!(partial_len.integrity_notes.iter().any(|n| n.contains("not a whole number of pages")));

    let db = SqliteDb::parse(&main).unwrap();
    let out_of_bounds = db.read_rows(99, 0, MAX_ROWS + 1);
    assert!(out_of_bounds.truncated);
    assert!(out_of_bounds.notes.iter().any(|n| n.contains("unreadable b-tree page 99")));

    let cycle = db.read_rows(2, 0, 10);
    assert!(cycle.truncated);
    assert!(cycle.notes.iter().any(|n| n.contains("repeated b-tree child page 2")));
}
