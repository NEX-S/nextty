//! RFC 4648 Base64 for OSC 52 clipboard text.

const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Padding is optional; bits after the last whole byte are ignored.
pub(crate) fn decode_utf8(input: &[u8]) -> Option<String> {
    let input = (input.strip_suffix(b"==").or(input.strip_suffix(b"="))).unwrap_or(input);
    let mut output = Vec::with_capacity(input.len() / 4 * 3 + 2);
    let (mut bits, mut pending) = (0u32, 0);
    for &byte in input {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = bits << 6 | u32::from(digit);
        pending += 6;
        if pending >= 8 {
            pending -= 8;
            output.push((bits >> pending) as u8);
        }
    }
    String::from_utf8(output).ok()
}

pub(crate) fn encode(bytes: &[u8], output: &mut Vec<u8>) {
    for chunk in bytes.chunks(3) {
        let bits = chunk
            .iter()
            .fold(0, |bits, &byte| bits << 8 | u32::from(byte));
        let bits = bits << (8 * (3 - chunk.len()));
        for index in 0..4 {
            output.push(if index <= chunk.len() {
                DIGITS[((bits >> (18 - 6 * index)) & 63) as usize]
            } else {
                b'='
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rfc_vectors_and_unicode() {
        for (encoded, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
            ("5Lit5paH", "中文"),
        ] {
            assert_eq!(
                super::decode_utf8(encoded.as_bytes()).as_deref(),
                Some(plain)
            );
            let mut output = Vec::new();
            super::encode(plain.as_bytes(), &mut output);
            assert_eq!(output, encoded.as_bytes());
        }
    }
}
