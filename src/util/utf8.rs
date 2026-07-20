/// Decode a freshly read byte chunk as UTF-8, carrying any trailing bytes that
/// fall mid-character across the next read so multibyte sequences split on a
/// read boundary aren't mangled. Genuinely invalid bytes become a replacement
/// character and are consumed so decoding always makes progress (the carry
/// stays bounded even for binary output).
///
/// Callers keep a `Vec<u8>` carry across reads and pass the same buffer each
/// time; at EOF, flush any remainder with `String::from_utf8_lossy(&carry)`.
pub(crate) fn decode_stream_chunk(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    match std::str::from_utf8(carry) {
        Ok(text) => {
            let decoded = text.to_string();
            carry.clear();
            decoded
        }
        Err(error) => {
            let valid = error.valid_up_to();
            let mut decoded = String::from_utf8_lossy(&carry[..valid]).into_owned();
            match error.error_len() {
                Some(invalid_len) => {
                    decoded.push('\u{FFFD}');
                    carry.drain(..valid + invalid_len);
                }
                None => {
                    carry.drain(..valid);
                }
            }
            decoded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_multibyte_split_across_boundary() {
        // "€" is E2 82 AC. Split it across two chunks.
        let mut carry = Vec::new();
        let first = decode_stream_chunk(&mut carry, &[0xE2, 0x82]);
        assert_eq!(first, "");
        assert_eq!(carry, vec![0xE2, 0x82]);
        let second = decode_stream_chunk(&mut carry, &[0xAC]);
        assert_eq!(second, "€");
        assert!(carry.is_empty());
    }

    #[test]
    fn passes_through_clean_ascii() {
        let mut carry = Vec::new();
        assert_eq!(decode_stream_chunk(&mut carry, b"hello"), "hello");
        assert!(carry.is_empty());
    }

    #[test]
    fn invalid_bytes_become_replacement_and_make_progress() {
        let mut carry = Vec::new();
        // 0xFF is never valid UTF-8; it must be consumed (not stall the carry),
        // emitting the valid prefix plus one replacement. Bytes after the invalid
        // sequence stay carried and decode on the next call — so decoding always
        // makes forward progress even on binary input.
        let first = decode_stream_chunk(&mut carry, &[b'a', 0xFF, b'b']);
        assert_eq!(first, "a\u{FFFD}");
        assert_eq!(carry, vec![b'b']);
        let second = decode_stream_chunk(&mut carry, &[]);
        assert_eq!(second, "b");
        assert!(carry.is_empty());
    }
}
