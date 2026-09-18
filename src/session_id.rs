//! Deterministic vendor session ids, decided by spar instead of scraped from the stream.
//!
//! A cold dispatch names its vendor session up front as
//! `v5(SESSION_NAMESPACE, "spar:{run}:{slot}:{round}")`: reproducible from state
//! alone, unique per dispatch, and readable back out of a run directory without any
//! log parsing. The round is in the derivation, so a new round is a new session for
//! cold dispatches by construction. A resumed implementer round never derives; it
//! reuses the previous round's id via the existing marker plus `build_resume`.
//!
//! Only adapters whose CLI accepts a caller-supplied id (claude, grok, muse) ever
//! receive a derived id. The rest keep capture-from-stream exactly as before.

/// Frozen namespace for every id spar derives. The bytes are `SPAR-SESSION-ID`
/// plus a trailing zero; the value is a contract (changing it renames every future
/// session), so it must never be edited in place.
pub const SESSION_NAMESPACE: [u8; 16] = [
    0x53, 0x50, 0x41, 0x52, 0x2d, 0x53, 0x45, 0x53, 0x53, 0x49, 0x4f, 0x4e, 0x2d, 0x49, 0x44, 0x00,
];

/// Name a cold dispatch's session: `spar:{run}:{slot}:{round}` under the frozen
/// namespace. Same triple in any process yields the same id; any differing
/// component yields a different one.
pub fn derive(run_id: &str, slot_id: &str, round: u32) -> String {
    v5(
        &SESSION_NAMESPACE,
        &format!("spar:{run_id}:{slot_id}:{round}"),
    )
}

/// Name a recovery turn's session for an adapter that refuses to reuse an
/// already-existing id (grok: `--session-id` must not already exist). Distinct from
/// `derive` for the same triple by construction, so the recovery spawn never
/// collides with the session the failed dispatch already created.
pub fn derive_recovery(run_id: &str, slot_id: &str, round: u32) -> String {
    v5(
        &SESSION_NAMESPACE,
        &format!("spar:{run_id}:{slot_id}:{round}:recovery"),
    )
}

/// RFC 4122 section 4.3 v5 UUID, hyphenated lowercase. SHA-1 is embedded rather
/// than a new dependency: it is only a name hash here, and the two test vectors
/// below pin it byte for byte.
fn v5(namespace: &[u8; 16], name: &str) -> String {
    let mut input = Vec::with_capacity(16 + name.len());
    input.extend_from_slice(namespace);
    input.extend_from_slice(name.as_bytes());
    let digest = sha1(&input);
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11],
        b[12], b[13], b[14], b[15]
    )
}

fn sha1(msg: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    let mut padded = msg.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    let (chunks, _) = padded.as_chunks::<64>();
    for chunk in chunks {
        let mut w = [0u32; 80];
        for (i, slot) in w.iter_mut().take(16).enumerate() {
            *slot = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// True for the hyphenated lowercase shape every assigning adapter's CLI requires
/// (claude and grok both reject a malformed `--session-id` before doing any work).
pub fn is_uuid_shape(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &c) in bytes.iter().enumerate() {
        let hex = c.is_ascii_hexdigit();
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            14 => {
                if c != b'5' {
                    return false;
                }
            }
            19 => {
                if !matches!(c, b'8' | b'9' | b'a' | b'b' | b'A' | b'B') {
                    return false;
                }
            }
            _ => {
                if !hex {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(digest: &[u8; 20]) -> String {
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha1_matches_fips_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn v5_matches_rfc_vector() {
        let dns: [u8; 16] = [
            0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
            0x30, 0xc8,
        ];
        assert_eq!(
            v5(&dns, "www.example.com"),
            "2ed6657d-e927-568b-95e1-2665a8aea6a2"
        );
    }

    #[test]
    fn derivation_is_stable_and_well_formed() {
        let a = derive("8506ddde", "impl", 1);
        assert_eq!(a, derive("8506ddde", "impl", 1));
        // Frozen vector, cross-checked against an independent SHA-1 (Python
        // hashlib): the same triple in any process, any language, yields this.
        assert_eq!(a, "071aa685-28b7-57bd-a82b-03163dd2fa20");
        assert_eq!(
            derive_recovery("8506ddde", "impl", 1),
            "7c87d64b-e716-5aca-9d6a-318a1d8b84be"
        );
        assert!(is_uuid_shape(&a), "not a v5 UUID: {a}");
    }

    #[test]
    fn derivation_is_distinct_across_round_slot_run_and_recovery() {
        let base = derive("8506ddde", "impl", 1);
        assert_ne!(base, derive("8506ddde", "impl", 2));
        assert_ne!(base, derive("8506ddde", "reviewer", 1));
        assert_ne!(base, derive("12345678", "impl", 1));
        let rec = derive_recovery("8506ddde", "impl", 1);
        assert_ne!(base, rec);
        assert!(is_uuid_shape(&rec), "not a v5 UUID: {rec}");
    }
}
