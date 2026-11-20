//! Small encoding helpers shared by every module: hex, standard base64,
//! and ISO-8601 UTC timestamps from Unix epochs.

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex encoding of `bytes`.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        out.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parse exactly 40 hex characters into a 20-byte git object id.
pub fn hex_decode_20(text: &str) -> Option<[u8; 20]> {
    let raw = text.as_bytes();
    if raw.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, pair) in raw.as_chunks::<2>().0.iter().enumerate() {
        let hi = hex_value(pair[0])?;
        let lo = hex_value(pair[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let first = group[0] as u32;
        let second = group.get(1).copied().unwrap_or(0) as u32;
        let third = group.get(2).copied().unwrap_or(0) as u32;
        let triple = (first << 16) | (second << 8) | third;
        out.push(B64_ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            B64_ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            B64_ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 decode; padding is optional, whitespace is rejected.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let raw = text.as_bytes();
    if !raw.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    for group in raw.chunks(4) {
        let mut values = [0u32; 4];
        let mut padding = 0usize;
        for (i, &byte) in group.iter().enumerate() {
            if byte == b'=' {
                padding += 1;
            } else {
                if padding > 0 {
                    return None;
                }
                values[i] = b64_value(byte)? as u32;
            }
        }
        let triple = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push((triple >> 16) as u8);
        if padding < 2 {
            out.push((triple >> 8) as u8);
        }
        if padding < 1 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

/// ISO-8601 UTC timestamp (`2025-01-01T00:00:00Z`) for a Unix epoch.
///
/// Civil-date conversion following Howard Hinnant's algorithm; no tables.
pub fn iso_from_epoch(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let day_secs = seconds % 86_400;
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let full_year = if month <= 2 { year + 1 } else { year };
    format!(
        "{full_year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes: Vec<u8> = (0..32).collect();
        assert_eq!(
            hex_decode_20(&hex_encode(&bytes[..20])).unwrap(),
            bytes[..20]
        );
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert!(hex_decode_20("zz").is_none());
        assert!(hex_decode_20("abcd").is_none());
    }

    #[test]
    fn base64_roundtrip() {
        for data in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"\x00\xff\x10\x80",
        ] {
            let encoded = base64_encode(data);
            assert_eq!(base64_decode(&encoded).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Hello, World!"), "SGVsbG8sIFdvcmxkIQ==");
        assert!(base64_decode("!!!").is_none());
        assert!(base64_decode("AB=C").is_none());
    }

    #[test]
    fn iso_known_dates() {
        assert_eq!(iso_from_epoch(1_735_689_600), "2025-01-01T00:00:00Z");
        assert_eq!(iso_from_epoch(0), "1970-01-01T00:00:00Z");
    }
}
