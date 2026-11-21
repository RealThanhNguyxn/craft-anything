//! Ordinary read-only listing fixtures. No signature verification is performed.
use craft_anything_formats::der::*;
use craft_anything_testkit::android::{certificate_der, der, der_oid, pkcs7_der};

// Exact pre-cutover listing from main 640fd942, preserving Afloat's positional certificate rules.
fn legacy_certificates(data: &[u8]) -> Vec<&[u8]> {
    let Some(ci) = read(data, 0).filter(|t| t.tag == 0x30) else { return Vec::new() };
    let kids = children(data, &ci);
    let is_signed = kids.first().is_some_and(|t| t.tag == 0x06 && oid(t.bytes(data).unwrap_or(&[])) == "1.2.840.113549.1.7.2");
    if !is_signed {
        return Vec::new();
    }
    let Some(explicit) = kids.get(1).filter(|t| t.tag == 0xa0) else { return Vec::new() };
    let Some(sd) = children(data, explicit).into_iter().next().filter(|t| t.tag == 0x30) else { return Vec::new() };
    let fields = children(data, &sd);
    let [version, digests, content, certificates, ..] = fields.as_slice() else { return Vec::new() };
    if (version.tag, digests.tag, content.tag, certificates.tag) != (0x02, 0x31, 0x30, 0xa0) {
        return Vec::new();
    }
    // Only the untagged SEQUENCE choice is an X.509 certificate.
    children(data, certificates).into_iter().filter(|t| t.tag == 0x30).take(64).filter_map(|t| t.whole(data)).collect()
}

fn assert_parity(bytes: &[u8], expected: &[Vec<u8>]) {
    let actual = pkcs7_certificates(bytes);
    assert_eq!(actual, legacy_certificates(bytes));
    assert_eq!(actual, expected.iter().map(Vec::as_slice).collect::<Vec<_>>());
    for slice in actual {
        let offset = slice.as_ptr() as usize - bytes.as_ptr() as usize;
        assert_eq!(slice, &bytes[offset..offset + slice.len()]);
    }
}

#[test]
fn ordinary_certificate_order_empty_set_and_retention_limit_match_legacy() {
    let a = certificate_der("Alice", "Parity");
    let b = certificate_der("Bob", "Parity");
    assert_parity(&pkcs7_der(&[]), &[]);
    assert_parity(&pkcs7_der(&[a.clone(), b.clone()].concat()), &[a.clone(), b]);
    for count in [63, 64, 65] {
        assert_parity(&pkcs7_der(&a.repeat(count)), &vec![a.clone(); count.min(64)]);
    }
}

#[test]
fn all_certificate_choices_count_toward_the_shared_child_limit() {
    let cert = certificate_der("At boundary", "Parity");
    // Opaque alternate CertificateChoices are listed structurally, not interpreted as X.509.
    for ignored in [4094, 4095, 4096] {
        let mut choices = [0xa2, 0].repeat(ignored);
        choices.extend_from_slice(&cert);
        let expected = if ignored < 4096 { vec![cert.clone()] } else { vec![] };
        assert_parity(&pkcs7_der(&choices), &expected);
    }
}

#[test]
fn ordinary_optional_crl_signer_and_outer_tails_do_not_change_listing() {
    let cert = certificate_der("Listed", "Parity");
    let prefix = [der(2, &[1]), der(0x31, &[]), der(0x30, &der_oid("1.2.840.113549.1.7.1"))].concat();
    for include_certs in [false, true] {
        let cert_field = if include_certs { der(0xa0, &cert) } else { vec![] };
        let sd = der(0x30, &[prefix.clone(), cert_field, der(0xa1, &[]), der(0x31, &[])].concat());
        let content = der(0x30, &[der_oid("1.2.840.113549.1.7.2"), der(0xa0, &sd)].concat());
        // A second complete outer element belongs to the caller, not this listing.
        let bytes = [content, der(5, &[])].concat();
        let expected = if include_certs { vec![cert.clone()] } else { vec![] };
        assert_parity(&bytes, &expected);
    }
}
