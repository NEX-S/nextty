//! Padded RFC 4648 Base64 for OSC 52 clipboard text.

const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn decode_utf8(input: &[u8]) -> Option<String> {
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let (quartets, _) = input.as_chunks::<4>();
    let mut output = vec![0; quartets.len() * 3];
    let mut length = output.len();
    let (decoded, _) = output.as_chunks_mut::<3>();
    for (index, (quartet, bytes)) in quartets.iter().zip(decoded.iter_mut()).enumerate() {
        let last = index + 1 == quartets.len();
        let a = digit(quartet[0])?;
        let b = digit(quartet[1])?;
        bytes[0] = a << 2 | b >> 4;
        match (quartet[2], quartet[3]) {
            (b'=', b'=') if last && b & 15 == 0 => length -= 2,
            (c, b'=') if last => {
                let c = digit(c)?;
                if c & 3 != 0 {
                    return None;
                }
                bytes[1] = b << 4 | c >> 2;
                length -= 1;
            }
            (c, d) => {
                let c = digit(c)?;
                let d = digit(d)?;
                bytes[1] = b << 4 | c >> 2;
                bytes[2] = c << 6 | d;
            }
        }
    }
    output.truncate(length);
    String::from_utf8(output).ok()
}

#[cfg(feature = "tui")]
pub(crate) fn encode(bytes: &[u8], output: &mut Vec<u8>) {
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
    const TABLE: [u8; 256] = {
        let mut table = [u8::MAX; 256];
        let mut index = 0;
        while index < DIGITS.len() {
            table[DIGITS[index] as usize] = index as u8;
            index += 1;
        }
        table
    };
    let value = TABLE[byte as usize];
    (value != u8::MAX).then_some(value)
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
