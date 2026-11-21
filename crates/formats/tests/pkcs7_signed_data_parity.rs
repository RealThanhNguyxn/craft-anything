//! Read-only CMS SignedData/SignerInfo framing parity tests. No trust policy changes.
use craft_anything_formats::{der, pkcs7, verify};
use craft_anything_testkit::{android, crypto, program_facts};

fn attribute(oid: &str, values: &[Vec<u8>]) -> Vec<u8> {
    android::der(0x30, &[android::der_oid(oid), android::der(0x31, &values.concat())].concat())
}

#[test]
fn detached_and_encapsulated_signed_data_preserve_exact_fields_and_raw_slices() {
    let content = b"cms signed data parity content";
    let detached = crypto::cms_detached_rsa(content, "Parity Signer", "Parity Org");
    let parsed = pkcs7::parse_signed_data(&detached).expect("detached SignedData");
    let certs = der::pkcs7_certificates(&detached);
    assert_eq!(certs.len(), 1);
    assert_eq!(parsed.certificates, vec![certs[0].to_vec()]);
    let parts = der::certificate_parts(certs[0]).expect("certificate parts");
    assert_eq!(parsed.digest, Some(verify::Digest::Sha256));
    assert_eq!(parsed.signature_algorithm, "1.2.840.113549.1.1.1");
    assert!(parsed.pss_parameters.is_none());
    assert!(!parsed.signature.is_empty());
    let reenc = parsed.signed_attrs_reencoded.as_ref().expect("signed attributes");
    assert_eq!(reenc[0], 0x31);
    assert!(parsed.signed_attrs_error.is_none());
    let expected_digest: String = crypto::sha256(content).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(parsed.message_digest.as_deref(), Some(expected_digest.as_str()));
    assert_eq!(parsed.content_type.as_deref(), Some("1.2.840.113549.1.7.1"));
    assert_eq!(parsed.signer_issuer, parts.issuer);
    assert_eq!(parsed.signer_serial, parts.serial);
    assert!(parsed.signer_key_identifier.is_none());
    assert!(parsed.signer_identity_notes.is_empty());
    assert!(!parsed.non_minimal_length);
    assert!(parsed.econtent.is_none());

    let auth = program_facts::authenticode_block(&[0x42; 32], "Authenticode Signer", "Parity Org", false);
    let auth_parsed = pkcs7::parse_signed_data(&auth).expect("Authenticode SignedData");
    assert_eq!(auth_parsed.content_type.as_deref(), Some("1.3.6.1.4.1.311.2.1.4"));
    assert!(auth_parsed.econtent.as_ref().is_some_and(|bytes| !bytes.is_empty()));
    assert!(auth_parsed.signed_attrs_error.is_none());
    assert!(!auth_parsed.non_minimal_length);
}

#[test]
fn subject_key_identifier_unsigned_attrs_multiple_signers_and_outer_tail_keep_framing() {
    let content = b"key id and tails";
    let base = crypto::cms_detached_rsa(content, "Primary", "Parity Org");
    let ci = der::children(&base, &der::read(&base, 0).unwrap());
    let sd = der::children(&base, &ci[1]);
    let sd_fields = der::children(&base, &sd[0]);
    let signers = der::children(&base, sd_fields.last().unwrap());
    let signer_fields = der::children(&base, &signers[0]);

    let key_id = vec![0x11, 0x22, 0x33, 0x44];
    let mut first_signer = Vec::new();
    first_signer.extend_from_slice(&android::der(0x02, &[3]));
    first_signer.extend_from_slice(&android::der(0x80, &key_id));
    first_signer.extend_from_slice(signer_fields[2].whole(&base).unwrap());
    let raw_attrs = signer_fields[3].whole(&base).unwrap();
    first_signer.extend_from_slice(raw_attrs);
    first_signer.extend_from_slice(signer_fields[4].whole(&base).unwrap());
    first_signer.extend_from_slice(signer_fields[5].whole(&base).unwrap());
    first_signer.extend_from_slice(&android::der(0xa1, &attribute("1.2.840.113549.1.9.6", &[android::der(0x05, &[])])));

    let second_signer = signers[0].whole(&base).unwrap();
    let mut signed_data_body = Vec::new();
    for field in &sd_fields[..sd_fields.len() - 1] {
        signed_data_body.extend_from_slice(field.whole(&base).unwrap());
    }
    signed_data_body.extend_from_slice(&android::der(0xa1, &[]));
    signed_data_body.extend_from_slice(&android::der(0x31, &[android::der(0x30, &first_signer), second_signer.to_vec()].concat()));

    let content_info = android::der(0x30, &[ci[0].whole(&base).unwrap(), &android::der(0xa0, &android::der(0x30, &signed_data_body))].concat());
    let with_tail = [content_info, android::der(0x05, &[])].concat();

    let parsed = pkcs7::parse_signed_data(&with_tail).expect("SignedData with SKI, unsignedAttrs, second signer, CRL, and outer tail");
    assert!(parsed.signer_issuer.is_empty());
    assert!(parsed.signer_serial.is_empty());
    assert_eq!(parsed.signer_key_identifier.as_deref(), Some(key_id.as_slice()));
    assert!(parsed.signer_identity_notes.is_empty());
    let mut expected_reenc = raw_attrs.to_vec();
    expected_reenc[0] = 0x31;
    assert_eq!(parsed.signed_attrs_reencoded.as_deref(), Some(expected_reenc.as_slice()));
    assert_eq!(parsed.signature, signer_fields[5].bytes(&base).unwrap());
}

#[test]
fn signed_attribute_and_signer_info_limits_match_shared_cursor_bounds() {
    let content = b"attribute bound content";
    let base = crypto::cms_detached_rsa(content, "Limit Signer", "Parity Org");
    let ci = der::children(&base, &der::read(&base, 0).unwrap());
    let sd = der::children(&base, &ci[1]);
    let sd_fields = der::children(&base, &sd[0]);
    let signers = der::children(&base, sd_fields.last().unwrap());
    let signer_fields = der::children(&base, &signers[0]);

    let ct_attr = attribute("1.2.840.113549.1.9.3", &[android::der_oid("1.2.840.113549.1.7.1")]);
    let md_attr = attribute("1.2.840.113549.1.9.4", &[android::der(0x04, &crypto::sha256(content))]);
    let dummy_attr = attribute("1.2.3.4", &[]);

    let build_with_attrs = |extra_count: usize| -> Vec<u8> {
        let mut attrs_body = Vec::new();
        attrs_body.extend_from_slice(&ct_attr);
        attrs_body.extend_from_slice(&md_attr);
        for _ in 0..extra_count {
            attrs_body.extend_from_slice(&dummy_attr);
        }
        let mut signer = Vec::new();
        signer.extend_from_slice(signer_fields[0].whole(&base).unwrap());
        signer.extend_from_slice(signer_fields[1].whole(&base).unwrap());
        signer.extend_from_slice(signer_fields[2].whole(&base).unwrap());
        signer.extend_from_slice(&android::der(0xa0, &attrs_body));
        signer.extend_from_slice(signer_fields[4].whole(&base).unwrap());
        signer.extend_from_slice(signer_fields[5].whole(&base).unwrap());
        let mut sd_body = Vec::new();
        for field in &sd_fields[..sd_fields.len() - 1] {
            sd_body.extend_from_slice(field.whole(&base).unwrap());
        }
        sd_body.extend_from_slice(&android::der(0x31, &android::der(0x30, &signer)));
        android::der(0x30, &[ci[0].whole(&base).unwrap(), &android::der(0xa0, &android::der(0x30, &sd_body))].concat())
    };

    let at_limit = pkcs7::parse_signed_data(&build_with_attrs(4094)).expect("4096 total attributes");
    assert!(at_limit.signed_attrs_error.is_none());
    assert!(at_limit.message_digest.is_some());
    assert!(at_limit.content_type.is_some());

    let over_limit = pkcs7::parse_signed_data(&build_with_attrs(4095)).expect("4097 attributes still yields metadata");
    assert_eq!(over_limit.signed_attrs_error, Some("CMS signed attributes: the attribute list is incomplete."));
    assert!(over_limit.signed_attrs_reencoded.is_some());
    assert_eq!(over_limit.digest, Some(verify::Digest::Sha256));
    assert!(!over_limit.signature.is_empty());

    // SignerInfo without signature OCTET STRING is rejected.
    let mut missing_sig = Vec::new();
    for field in &signer_fields[..5] {
        missing_sig.extend_from_slice(field.whole(&base).unwrap());
    }
    let mut sd_body = Vec::new();
    for field in &sd_fields[..sd_fields.len() - 1] {
        sd_body.extend_from_slice(field.whole(&base).unwrap());
    }
    sd_body.extend_from_slice(&android::der(0x31, &android::der(0x30, &missing_sig)));
    let no_sig_cms = android::der(0x30, &[ci[0].whole(&base).unwrap(), &android::der(0xa0, &android::der(0x30, &sd_body))].concat());
    assert!(pkcs7::parse_signed_data(&no_sig_cms).is_none());
}
