//! RC4 stream cipher (key scheduling plus pseudo-random generation), as used
//! by the standard security handler for `/V2` and revisions 2 to 4.

/// Encrypts or decrypts `data` with `key` (the operation is symmetric).
/// An empty key is treated as the single byte 0, which cannot arise from the
/// handler's key derivation but keeps the schedule total.
pub(crate) fn apply(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key: &[u8] = if key.is_empty() { &[0] } else { key };
    let mut state: [u8; 256] = [0; 256];
    for (i, slot) in state.iter_mut().enumerate() {
        *slot = i as u8;
    }
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, usize::from(j));
    }

    let mut i: u8 = 0;
    let mut j: u8 = 0;
    data.iter()
        .map(|byte| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(state[usize::from(i)]);
            state.swap(usize::from(i), usize::from(j));
            let k = state[usize::from(state[usize::from(i)].wrapping_add(state[usize::from(j)]))];
            byte ^ k
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::apply;

    /// Wikipedia / classic test vectors: (key, plaintext, ciphertext hex).
    const VECTORS: [(&[u8], &[u8], &str); 3] = [
        (b"Key", b"Plaintext", "bbf316e8d940af0ad3"),
        (b"Wiki", b"pedia", "1021bf0420"),
        (b"Secret", b"Attack at dawn", "45a01f645fc35b383552544b9bf5"),
    ];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn encrypts_published_vectors() {
        for (key, plain, want) in VECTORS {
            assert_eq!(hex(&apply(key, plain)), want, "key {key:?}");
        }
    }

    #[test]
    fn rfc6229_40_bit_keystream() {
        // RFC 6229, key 0102030405: first 32 keystream bytes (offset 0).
        let stream = apply(&[1, 2, 3, 4, 5], &[0; 32]);
        assert_eq!(
            hex(&stream),
            "b2396305f03dc027ccc3524a0a1118a869829\
             44f18fc82d589c403a47a0d0919"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn decrypting_ciphertext_restores_plaintext() {
        let key = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa];
        let plain: Vec<u8> = (0..=255).collect();
        assert_eq!(apply(&key, &apply(&key, &plain)), plain);
    }
}
