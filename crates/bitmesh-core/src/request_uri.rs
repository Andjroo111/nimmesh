//! # request_uri — G18 "pay me X NIM" request links (amount + address)
//!
//! The fat-finger fix for getting paid: instead of reading an amount aloud and hoping the
//! payer types it right, the payee shows a **request link** — a standard `nimiq:` URI that
//! carries the recipient address **and** the exact amount — as a QR. The payer scans it and
//! the Send screen is pre-filled, amount and all. Recent recipients + named contacts (the
//! other half of #29) are pure local UI state in the app; this module owns the one piece that
//! needs to be exact and shared across platforms: the URI codec.
//!
//! **Pure, key-free, non-money-path:** it only formats/parses public data (an address, an
//! amount, an optional message). It signs nothing and touches no seed. The same audited codec
//! runs byte-identically on iOS and Android via UniFFI, and is symmetric — `parse(build(x)) ==
//! x` for any valid input (property-tested).
//!
//! Format (the Nimiq URI scheme): `nimiq:<ADDRESS>?amount=<NIM>&message=<text>`, address with
//! spaces stripped, amount as a decimal **NIM** string (≤ 5 dp, the luna precision), message
//! percent-encoded. `amount` / `message` are optional.

use crate::nimiq::address::Address;

/// luna per NIM (Albatross: 1 NIM = 10^5 luna) — the request amount's precision.
const LUNA_PER_NIM: u64 = 100_000;
/// Decimal places of a NIM amount (matches `LUNA_PER_NIM = 10^NIM_DECIMALS`).
const NIM_DECIMALS: usize = 5;

/// A parsed "pay me" request — a recipient, an optional exact amount, an optional message.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PaymentRequest {
    /// The recipient address in the canonical user-friendly `NQ…` form (spaced, uppercase).
    pub address: String,
    /// The requested amount in **luna** (`0` means "no amount specified").
    pub amount_luna: u64,
    /// An optional human message / label for the payment.
    pub message: Option<String>,
}

/// Build a `nimiq:` request URI from a recipient + optional amount + optional message.
///
/// Returns `None` if `address` is not a valid Nimiq address. `amount_luna == 0` omits the
/// amount param (a plain address request). The address is validated and re-serialized to its
/// canonical form (so spacing/case in the input never corrupts the link).
#[uniffi::export]
pub fn build_request_uri(
    address: String,
    amount_luna: u64,
    message: Option<String>,
) -> Option<String> {
    let addr = Address::from_user_friendly(&address).ok()?;
    // Canonical, space-free address for the URI body.
    let compact: String = addr
        .to_user_friendly()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut uri = format!("nimiq:{compact}");
    let mut params: Vec<String> = Vec::new();
    if amount_luna > 0 {
        params.push(format!("amount={}", format_nim(amount_luna)));
    }
    if let Some(msg) = message.as_ref().filter(|m| !m.is_empty()) {
        params.push(format!("message={}", percent_encode(msg)));
    }
    if !params.is_empty() {
        uri.push('?');
        uri.push_str(&params.join("&"));
    }
    Some(uri)
}

/// Parse a `nimiq:` request URI back into a [`PaymentRequest`].
///
/// Returns `None` for any non-`nimiq:` URI, an invalid address, or a malformed amount. Unknown
/// query params are ignored (forward-compatible); a missing amount yields `amount_luna == 0`.
#[uniffi::export]
pub fn parse_request_uri(uri: String) -> Option<PaymentRequest> {
    let rest = uri.strip_prefix("nimiq:")?;
    let (addr_part, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let address = Address::from_user_friendly(addr_part)
        .ok()?
        .to_user_friendly();

    let mut amount_luna = 0u64;
    let mut message = None;
    if let Some(query) = query {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "amount" => amount_luna = parse_nim(value)?, // a present-but-bad amount fails
                "message" => {
                    let decoded = percent_decode(value)?;
                    if !decoded.is_empty() {
                        message = Some(decoded);
                    }
                }
                _ => {} // ignore unknown params
            }
        }
    }
    Some(PaymentRequest {
        address,
        amount_luna,
        message,
    })
}

/// Format `luna` as a decimal NIM string (trailing-zero-trimmed): `150_000 → "1.5"`,
/// `100_000 → "1"`, `1 → "0.00001"`.
fn format_nim(luna: u64) -> String {
    let int = luna / LUNA_PER_NIM;
    let frac = luna % LUNA_PER_NIM;
    if frac == 0 {
        return int.to_string();
    }
    let frac = format!("{frac:0width$}", width = NIM_DECIMALS);
    let frac = frac.trim_end_matches('0');
    format!("{int}.{frac}")
}

/// Parse a decimal NIM string into luna. `None` for non-numeric input, more than
/// [`NIM_DECIMALS`] fractional digits (would lose precision), or an overflow.
fn parse_nim(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let (int_str, frac_str) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    // Empty integer part is allowed only as ".5"-style? No — require digits each side present.
    if int_str.is_empty() || !int_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac_str.len() > NIM_DECIMALS || !frac_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int: u64 = int_str.parse().ok()?;
    let frac_padded = format!("{frac_str:0<width$}", width = NIM_DECIMALS);
    let frac: u64 = if frac_padded.is_empty() {
        0
    } else {
        frac_padded.parse().ok()?
    };
    int.checked_mul(LUNA_PER_NIM)?.checked_add(frac)
}

/// The RFC-3986 unreserved set — left as-is by [`percent_encode`].
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// Percent-encode a string for a URI query value (encodes everything outside the unreserved
/// set, so `&`, `=`, spaces, and Unicode all survive a round-trip).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Decode a percent-encoded query value. `None` on a malformed `%XX` escape or invalid UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16))?;
                let lo = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16))?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b'+' => {
                out.push(b' '); // tolerate form-style '+'-for-space on decode
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> String {
        Address::from_bytes([0x11; 20]).to_user_friendly()
    }

    #[test]
    fn nim_formatting_round_trips() {
        assert_eq!(format_nim(100_000), "1");
        assert_eq!(format_nim(150_000), "1.5");
        assert_eq!(format_nim(1), "0.00001");
        assert_eq!(format_nim(0), "0");
        assert_eq!(format_nim(123_456_789), "1234.56789");
        for luna in [0u64, 1, 99_999, 100_000, 150_000, 123_456_789, u64::MAX] {
            assert_eq!(
                parse_nim(&format_nim(luna)),
                Some(luna),
                "round-trip {luna}"
            );
        }
    }

    #[test]
    fn nim_parsing_rejects_bad_input() {
        assert_eq!(parse_nim(""), None);
        assert_eq!(parse_nim("abc"), None);
        assert_eq!(parse_nim("1.234567"), None); // > 5 decimals would lose precision
        assert_eq!(parse_nim(".5"), None); // require an integer part
        assert_eq!(parse_nim("1.2.3"), None);
        assert_eq!(parse_nim("10"), Some(1_000_000));
        assert_eq!(parse_nim("10.5"), Some(1_050_000));
    }

    #[test]
    fn builds_a_full_request_uri() {
        let uri = build_request_uri(addr(), 1_050_000, Some("Coffee & cake".into())).unwrap();
        assert!(uri.starts_with("nimiq:"));
        assert!(uri.contains("amount=10.5"));
        assert!(uri.contains("message=Coffee%20%26%20cake")); // space + & encoded
        assert!(!uri.contains(' '));
    }

    #[test]
    fn builds_an_address_only_request() {
        let uri = build_request_uri(addr(), 0, None).unwrap();
        assert!(!uri.contains('?')); // no amount, no message → bare address
    }

    #[test]
    fn rejects_an_invalid_address() {
        assert!(build_request_uri("not-an-address".into(), 1, None).is_none());
        assert!(parse_request_uri("nimiq:not-an-address".into()).is_none());
        assert!(parse_request_uri("bitcoin:whatever".into()).is_none());
    }

    #[test]
    fn parse_round_trips_build() {
        for (amount, msg) in [
            (0u64, None),
            (1_050_000u64, None),
            (12_345u64, Some("hi there".to_string())),
            (500_000u64, Some("rent — June".to_string())),
        ] {
            let uri = build_request_uri(addr(), amount, msg.clone()).unwrap();
            let parsed = parse_request_uri(uri).unwrap();
            assert_eq!(parsed.address, addr());
            assert_eq!(parsed.amount_luna, amount);
            assert_eq!(parsed.message, msg.filter(|m| !m.is_empty()));
        }
    }

    #[test]
    fn parse_ignores_unknown_params_and_missing_amount() {
        let uri = format!("nimiq:{}?foo=bar", addr().replace(' ', ""));
        let p = parse_request_uri(uri).unwrap();
        assert_eq!(p.amount_luna, 0);
        assert_eq!(p.message, None);
    }

    #[test]
    fn parse_fails_on_a_malformed_amount() {
        let uri = format!("nimiq:{}?amount=abc", addr().replace(' ', ""));
        assert!(parse_request_uri(uri).is_none());
    }
}
