//! PNG encoder (stage 8): deterministic PNG output for the paint canvas.
//!
//! Subset, taken from the normative documents, zero dependencies:
//! - CRC-32 (ISO 3309 / ITU-T V.42, referenced by the PNG specification),
//!   table-driven, reflected, init 0xFFFFFFFF, final xor 0xFFFFFFFF;
//! - Adler-32 (RFC 1950 §8.2): A = 1 + sum, B = sum of running A, mod 65521;
//! - zlib stream (RFC 1950) carrying STORED deflate blocks only (RFC 1951
//!   §3.2.4): every 65535-byte slice becomes a BTYPE=00 block with LEN/NLEN;
//!   the result is a valid zlib stream any conforming inflater can decompress;
//! - PNG (ISO/IEC 15948 / PNG 1.2): 8-bit RGBA (color type 6), filter type 0
//!   (None) per scanline, no interlace, one IHDR + one IDAT + IEND.
//!
//! The output is byte-for-byte deterministic. The encoder is a pure module:
//! no I/O, no panics, no environment; `encode` returns Option and rejects
//! invalid geometry or a wrong pixel-buffer length honestly (the canvas it
//! consumes is fixed 800x600, but the module does not trust its caller).
//!
//! Verification scheme (anti-circular): the CRC-32/Adler-32 implementations
//! are pinned by published vectors; the frozen zlib/PNG byte expectations were
//! cross-checked against an independent OS-level inflater (Python stdlib
//! zlib); e2e T26 decodes the binary's IDAT with that external inflater and
//! compares the scanlines against the PPM ground truth.

/// Precompute the CRC-32 lookup table (256 entries, reflection poly 0xEDB88320).
fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}

/// Running CRC-32 state (init 0xFFFFFFFF, no final xor yet).
fn crc32_state(state: u32, data: &[u8], t: &[u32; 256]) -> u32 {
    let mut c = state;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// CRC-32 of `data` (the checksum used by PNG chunks).
pub fn crc32(data: &[u8]) -> u32 {
    crc32_state(0xFFFF_FFFF, data, &crc_table()) ^ 0xFFFF_FFFF
}

/// Adler-32 (RFC 1950 §8.2) of `data`.
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &x in data {
        a = (a + x as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn push_be32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&[(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8]);
}

/// zlib stream (RFC 1950) with STORED deflate blocks only (RFC 1951 §3.2.4).
///
/// Layout: CMF=0x78 (CM=8 deflate, CINFO=7), FLG=0x01 (FCHECK makes
/// 0x7801 % 31 == 0, no preset dictionary), then for each 65535-byte slice a
/// byte-aligned stored block (BFINAL bit, BTYPE=00, LEN, NLEN=!LEN, payload),
/// then the big-endian Adler-32 of the uncompressed data.
pub fn zlib_store(data: &[u8]) -> Vec<u8> {
    let blocks = if data.is_empty() { 1 } else { data.len().div_ceil(65535) };
    let mut out = Vec::with_capacity(2 + data.len() + 5 * blocks + 4);
    out.push(0x78);
    out.push(0x01);
    const MAX: usize = 65535;
    let mut i = 0usize;
    loop {
        let last = i + MAX >= data.len();
        let end = if last { data.len() } else { i + MAX };
        let n = end - i;
        out.push(if last { 0x01 } else { 0x00 });
        out.push((n & 0xFF) as u8);
        out.push(((n >> 8) & 0xFF) as u8);
        out.push(((!(n as u16)) & 0xFF) as u8);
        out.push(((!(n as u16) >> 8) & 0xFF) as u8);
        out.extend_from_slice(&data[i..end]);
        i = end;
        if last {
            break;
        }
    }
    let ad = adler32(data);
    out.extend_from_slice(&[(ad >> 24) as u8, (ad >> 16) as u8, (ad >> 8) as u8, ad as u8]);
    out
}

/// Append one PNG chunk: length + type + data + CRC-32 over type|data.
fn chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
    push_be32(out, data.len() as u32);
    out.extend_from_slice(typ);
    out.extend_from_slice(data);
    let t = crc_table();
    let st = crc32_state(crc32_state(0xFFFF_FFFF, typ, &t), data, &t);
    push_be32(out, st ^ 0xFFFF_FFFF);
}

/// Encode a PNG image from an RGBA8 pixel buffer (row-major, top-left origin).
///
/// Returns None on invalid geometry (non-positive or overflowing width/height)
/// or when `rgba` does not hold exactly w*h*4 bytes — honest rejection, no
/// panics. Color type 6 (RGBA, 8-bit), one filter-type-0 byte per scanline,
/// stored-block zlib IDAT, no interlace.
pub fn encode(w: i64, h: i64, rgba: &[u8]) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 || w > u32::MAX as i64 || h > u32::MAX as i64 {
        return None;
    }
    let (wu, hu) = (w as u64, h as u64);
    let need = wu.checked_mul(hu)?.checked_mul(4)?;
    if rgba.len() as u64 != need {
        return None;
    }
    // raw scanlines: per row one filter byte + w*4 pixel bytes
    let raw_len = need.checked_add(hu)?;
    let mut raw = Vec::with_capacity(raw_len as usize);
    let row_bytes = (wu * 4) as usize;
    let mut off = 0usize;
    for _ in 0..hu {
        raw.push(0u8); // filter type 0 (None)
        raw.extend_from_slice(&rgba[off..off + row_bytes]);
        off += row_bytes;
    }
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 64);
    out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&(w as u32).to_be_bytes());
    ihdr[4..8].copy_from_slice(&(h as u32).to_be_bytes());
    ihdr[8] = 8; // bit depth
    ihdr[9] = 6; // color type: RGBA
    ihdr[10] = 0; // compression method: deflate
    ihdr[11] = 0; // filter method: 0 (per-scanline bytes below use type None)
    ihdr[12] = 0; // interlace method: none
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_store(&raw));
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_published_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn adler32_published_vectors() {
        assert_eq!(adler32(b""), 0x0000_0001);
        assert_eq!(adler32(b"abc"), 0x024D_0127);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn zlib_store_frozen_bytes() {
        // header + one final stored block + big-endian adler32
        assert_eq!(
            zlib_store(b"abc"),
            vec![0x78, 0x01, 0x01, 0x03, 0x00, 0xFC, 0xFF, 0x61, 0x62, 0x63,
                 0x02, 0x4D, 0x01, 0x27]
        );
        // empty payload: one final empty stored block
        assert_eq!(
            zlib_store(b""),
            vec![0x78, 0x01, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01]
        );
    }

    /// Minimal stored-block inflater, test-side only: validates our own block
    /// framing (LEN/NLEN complement, final-block bit) and the Adler-32 trailer
    /// on large multi-block streams. The external oracle remains the OS
    /// inflater used by e2e T26.
    fn inflate_store(z: &[u8]) -> Option<Vec<u8>> {
        if z.len() < 6 || z[0] != 0x78 {
            return None;
        }
        let mut out = Vec::new();
        let mut i = 2usize;
        loop {
            if i >= z.len() {
                return None;
            }
            let last = z[i] & 0x01 == 0x01;
            if z[i] & 0x06 != 0x00 {
                return None; // only stored blocks are produced by this encoder
            }
            i += 1;
            if i + 4 > z.len() {
                return None;
            }
            let len = z[i] as usize | ((z[i + 1] as usize) << 8);
            let nlen = z[i + 2] as usize | ((z[i + 3] as usize) << 8);
            if len != (!nlen & 0xFFFF) || i + 4 + len > z.len() {
                return None;
            }
            out.extend_from_slice(&z[i + 4..i + 4 + len]);
            i += 4 + len;
            if last {
                break;
            }
        }
        if i + 4 != z.len() {
            return None;
        }
        let ad = ((z[i] as u32) << 24) | ((z[i + 1] as u32) << 16)
            | ((z[i + 2] as u32) << 8) | z[i + 3] as u32;
        if ad != adler32(&out) {
            return None;
        }
        Some(out)
    }

    #[test]
    fn zlib_store_multiblock_roundtrip() {
        // 70000 bytes -> 2 stored blocks; own inflater restores the payload
        let data: Vec<u8> = (0..70000).map(|i| ((i * 7 + 13) % 256) as u8).collect();
        let z = zlib_store(&data);
        assert_eq!(z.len(), 2 + 5 * 2 + 70000 + 4);
        assert_eq!(z[2], 0x00, "first block must not be final");
        assert_eq!(inflate_store(&z).as_deref(), Some(data.as_slice()));
        // exactly 65535 -> one final block, boundary round-trips
        let edge: Vec<u8> = (0..65535).map(|i| (i % 251) as u8).collect();
        let z2 = zlib_store(&edge);
        assert_eq!(z2[2], 0x01, "single block must be final");
        assert_eq!(inflate_store(&z2).as_deref(), Some(edge.as_slice()));
    }

    #[test]
    fn encode_png_frozen_bytes_2x1() {
        // red, blue; frozen byte-for-byte; cross-checked against an external
        // OS-level inflater (scripts/png_constants.py, decode-ok)
        let frozen: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x02,
            0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0xF4, 0x22, 0x7F,
            0x8A, 0x00, 0x00, 0x00, 0x14, 0x49, 0x44, 0x41, 0x54, 0x78, 0x01, 0x01,
            0x09, 0x00, 0xF6, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF,
            0xFF, 0x0F, 0xF9, 0x03, 0xFD, 0xA6, 0x2E, 0x70, 0x51, 0x00, 0x00, 0x00,
            0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let got = encode(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]).unwrap();
        assert_eq!(got, frozen);
    }

    #[test]
    fn encode_png_rejects_invalid_input() {
        assert!(encode(0, 1, &[]).is_none()); // zero width
        assert!(encode(1, -1, &[]).is_none()); // negative height
        assert!(encode(i64::MAX, 2, &[1]).is_none()); // overflow
        assert!(encode(2, 1, &[1, 2, 3]).is_none()); // wrong buffer length
        assert!(encode(1, 1, &[1, 2, 3, 4, 5]).is_none()); // extra bytes
    }

    #[test]
    fn encode_png_full_canvas_structure() {
        // 800x600 opaque white: deterministic length (30 stored blocks),
        // signature, IHDR fields, first pixel opaque white
        let big = encode(800, 600, &[255u8; 800 * 600 * 4]).unwrap();
        // raw = 600 * (1 + 3200) = 1_920_600; blocks = ceil(raw/65535) = 30
        // zlib = 2 + 5*30 + raw + 4; png = 8 + 25 + (12 + zlib) + 12
        assert_eq!(big.len(), 1_920_813);
        assert_eq!(&big[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&big[12..16], b"IHDR");
        assert_eq!(&big[16..29], &[0, 0, 3, 32, 0, 0, 2, 88, 8, 6, 0, 0, 0]);
        assert_eq!(&big[big.len() - 8..big.len() - 4], b"IEND");
        assert_eq!(&big[big.len() - 4..], &[0xAE, 0x42, 0x60, 0x82]);
        // idat payload inflates back to the exact scanline buffer (own inflater);
        // take the payload bounds from the IDAT length field, not by hand
        let ilen = u32::from_be_bytes(big[33..37].try_into().unwrap()) as usize;
        let idat = &big[41..41 + ilen];
        let raw = inflate_store(idat).unwrap();
        assert_eq!(raw.len(), 600 * 3201);
        assert!(raw.iter().enumerate().all(|(i, b)| if i % 3201 == 0 { *b == 0 } else { *b == 255 }));
    }
}
