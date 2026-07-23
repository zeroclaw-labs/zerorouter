//! Pure unit tests for Stripe webhook signature verification.
//!
//! No network and no database: signatures are constructed locally with the
//! same `hmac` crate the router verifies with.

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use zerorouter::stripe::{WebhookVerifyError, verify_webhook_signature};

const SECRET: &str = "whsec_test_secret";
const TOLERANCE: Duration = Duration::from_secs(300);
const NOW: i64 = 1_752_000_000;
const PAYLOAD: &[u8] = br#"{"id":"evt_test","type":"checkout.session.completed"}"#;

/// Hex HMAC-SHA256 over `{timestamp}.{payload}`, exactly as Stripe signs.
fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

fn header(timestamp: i64, signatures: &[&str]) -> String {
    let mut header = format!("t={timestamp}");
    for signature in signatures {
        header.push_str(",v1=");
        header.push_str(signature);
    }
    header
}

#[test]
fn valid_signature_verifies() {
    let signature = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Ok(())
    );
    // Skew inside the tolerance window (either direction) is accepted.
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW + 300),
        Ok(())
    );
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW - 300),
        Ok(())
    );
}

#[test]
fn tampered_payload_fails() {
    let signature = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    let mut tampered = PAYLOAD.to_vec();
    tampered[0] ^= 1;
    assert_eq!(
        verify_webhook_signature(&tampered, &header, SECRET, TOLERANCE, NOW),
        Err(WebhookVerifyError::SignatureMismatch)
    );
}

#[test]
fn wrong_secret_fails() {
    let signature = sign("whsec_other_secret", NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Err(WebhookVerifyError::SignatureMismatch)
    );
}

#[test]
fn stale_timestamp_fails() {
    // Correctly signed, but one second past the tolerance in either
    // direction: replayed captures and clock-skewed forgeries both fail.
    let signature = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW + 301),
        Err(WebhookVerifyError::TimestampOutOfTolerance)
    );
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW - 301),
        Err(WebhookVerifyError::TimestampOutOfTolerance)
    );
}

#[test]
fn malformed_headers_fail() {
    let signature = sign(SECRET, NOW, PAYLOAD);
    let cases = [
        String::new(),
        "garbage".to_owned(),
        format!("v1={signature}"),              // no timestamp
        format!("t=notanumber,v1={signature}"), // unparseable timestamp
        format!("t={NOW}"),                     // no v1 candidate
        format!("t {NOW},v1 {signature}"),      // no key=value separators
    ];
    for header in &cases {
        assert_eq!(
            verify_webhook_signature(PAYLOAD, header, SECRET, TOLERANCE, NOW),
            Err(WebhookVerifyError::MalformedHeader),
            "{header:?} should be malformed"
        );
    }
}

#[test]
fn any_matching_candidate_verifies() {
    // First candidate: valid hex but signed over different bytes. Second:
    // not hex at all. Third: the real signature. Verification must accept
    // the set (Stripe sends multiple v1 values during secret rotation).
    let wrong = sign(SECRET, NOW, b"different payload");
    let valid = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&wrong, "not-hex", &valid]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Ok(())
    );
}

#[test]
fn candidate_set_with_no_match_fails() {
    let wrong = sign(SECRET, NOW, b"different payload");
    let also_wrong = sign("whsec_other_secret", NOW, PAYLOAD);
    let header = header(NOW, &[&wrong, "not-hex", &also_wrong]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Err(WebhookVerifyError::SignatureMismatch)
    );
}
