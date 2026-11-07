//! Padded RFC 4648 Base64 for OSC 52 clipboard text.

const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn decode_utf8(input: &[u8]) -> Option<String> {
    let (quartets, tail) = input.as_chunks::<4>();
    if !tail.is_empty() {
        return None;
    }
    let mut output = vec![0; quartets.len() * 3];
    let mut length = output.len();
    let (decoded, _) = output.as_chunks_mut::<3>();
    for (index, (&quartet, bytes)) in quartets.iter().zip(decoded.iter_mut()).enumerate() {
        let [a, b, mut c, mut d] = quartet.map(digit);
        if a | b | c | d == u8::MAX {
            if index + 1 != quartets.len() || a | b == u8::MAX {
                return None;
            }
            match (quartet[2], quartet[3]) {
                (b'=', b'=') if b & 15 == 0 => {
                    c = 0;
                    d = 0;
                    length -= 2;
                }
                (_, b'=') if c != u8::MAX && c & 3 == 0 => {
                    d = 0;
                    length -= 1;
                }
                _ => return None,
            }
        }
        *bytes = [a << 2 | b >> 4, b << 4 | c >> 2, c << 6 | d];
    }
    output.truncate(length);
    String::from_utf8(output).ok()
}

#[cfg(any(feature = "tui", test))]
pub(crate) fn encode(bytes: &[u8], output: &mut Vec<u8>) {
    let start = output.len();
    output.resize(start + bytes.len().div_ceil(3) * 4, b'=');
    let (chunks, tail) = bytes.as_chunks::<3>();
    let (encoded, _) = output[start..].as_chunks_mut::<4>();
    for (&[a, b, c], encoded) in chunks.iter().zip(encoded.iter_mut()) {
        *encoded = [
            DIGITS[(a >> 2) as usize],
            DIGITS[((a & 3) << 4 | b >> 4) as usize],
            DIGITS[((b & 15) << 2 | c >> 6) as usize],
            DIGITS[(c & 63) as usize],
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

fn digit(byte: u8) -> u8 {
    const TABLE: [u8; 256] = {
        let mut table = [u8::MAX; 256];
        let mut index = 0;
        while index < DIGITS.len() {
            table[DIGITS[index] as usize] = index as u8;
            index += 1;
        }
        table
    };
    TABLE[byte as usize]
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
