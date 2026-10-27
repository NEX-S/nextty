//! Padded RFC 4648 Base64 for OSC 52 clipboard text.

pub(crate) fn decode_utf8(input: &[u8]) -> Option<String> {
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    for (index, quartet) in input.chunks_exact(4).enumerate() {
        let last = index + 1 == input.len() / 4;
        let a = digit(quartet[0])?;
        let b = digit(quartet[1])?;
        output.push(a << 2 | b >> 4);
        match (quartet[2], quartet[3]) {
            (b'=', b'=') if last && b & 15 == 0 => {}
            (c, b'=') if last => {
                let c = digit(c)?;
                if c & 3 != 0 {
                    return None;
                }
                output.push(b << 4 | c >> 2);
            }
            (c, d) => {
                let c = digit(c)?;
                let d = digit(d)?;
                output.push(b << 4 | c >> 2);
                output.push(c << 6 | d);
            }
        }
    }
    String::from_utf8(output).ok()
}

#[cfg(feature = "tui")]
pub(crate) fn encode(bytes: &[u8], output: &mut Vec<u8>) {
    const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let start = output.len();
    output.resize(start + bytes.len().div_ceil(3) * 4, b'=');
    let (chunks, tail) = bytes.as_chunks::<3>();
    let (encoded, _) = output[start..].as_chunks_mut::<4>();
    for (part, encoded) in chunks.iter().zip(encoded.iter_mut()) {
        *encoded = [
            DIGITS[(part[0] >> 2) as usize],
            DIGITS[((part[0] & 3) << 4 | part[1] >> 4) as usize],
            DIGITS[((part[1] & 15) << 2 | part[2] >> 6) as usize],
            DIGITS[(part[2] & 63) as usize],
        ];
    }
    if let Some(&a) = tail.first() {
        let b = tail.get(1).copied().unwrap_or(0);
        *encoded.last_mut().unwrap() = [
            DIGITS[(a >> 2) as usize],
            DIGITS[((a & 3) << 4 | b >> 4) as usize],
            if tail.len() == 2 {
                DIGITS[((b & 15) << 2) as usize]
            } else {
                b'='
            },
            b'=',
        ];
    }
}

fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode_utf8;

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
            assert_eq!(decode_utf8(encoded.as_bytes()).as_deref(), Some(plain));
            #[cfg(feature = "tui")]
            {
                let mut output = Vec::new();
                super::encode(plain.as_bytes(), &mut output);
                assert_eq!(output, encoded.as_bytes());
            }
        }
    }

    #[test]
    fn rejects_noncanonical_and_non_utf8_input() {
        for encoded in [
            "Zg", "Zg=", "Zg===", "Zh==", "Zm9=", "=m9v", "Zm=v", "Zg==Zg==", "Zm9v\n", "Zm9_",
            "Zm9-", "/w==", "wIA=", "7aCA",
        ] {
            assert_eq!(decode_utf8(encoded.as_bytes()), None, "{encoded}");
        }
    }
}
