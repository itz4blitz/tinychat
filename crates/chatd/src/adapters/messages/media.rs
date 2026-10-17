//! Photos and files on the Messages relay are AES-256-GCM, in 32 KiB chunks.
//!
//! Wire: `0x00 || log2(chunk size) || [nonce(12) || ciphertext || tag(16)]…`
//! Each chunk's extra data is `isLast (1 byte) || chunk index (4 bytes, big-endian)`.
//! chatd opens the bytes and writes them to disk. It does not decode the picture.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

const CHUNK: usize = 32 * 1024;
const LOG2_CHUNK: u8 = 15;

pub fn pack(key: &[u8], plain: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = cipher(key)?;
    let mut out = vec![0u8, LOG2_CHUNK];
    if plain.is_empty() {
        return Ok(out);
    }
    let last = plain.len().div_ceil(CHUNK) - 1;
    for (index, chunk) in plain.chunks(CHUNK).enumerate() {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce)
            .map_err(|_| "Messages could not make a media key.".to_owned())?;
        let sealed = seal_chunk(&cipher, &nonce, chunk, index == last, index as u32)?;
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
    }
    Ok(out)
}

pub fn unpack(key: &[u8], packed: &[u8]) -> Result<Vec<u8>, String> {
    if key.len() != 32 {
        return Err("Messages sent a media key of the wrong size.".into());
    }
    if packed.len() < 2 || packed[0] != 0 {
        return Err("Messages sent media in a form tinyclient cannot open.".into());
    }
    let chunk = 1usize << packed[1].min(24);
    let cipher = cipher(key)?;
    let mut at = 2;
    let mut index = 0u32;
    let mut plain = Vec::new();
    while at < packed.len() {
        if packed.len() - at < 12 + 16 {
            return Err("Messages sent a media chunk that is too short.".into());
        }
        let nonce = &packed[at..at + 12];
        at += 12;
        let rest = packed.len() - at;
        let body = if rest > chunk + 16 { chunk + 16 } else { rest };
        let sealed = &packed[at..at + body];
        at += body;
        let last = at >= packed.len();
        plain.extend(open_chunk(&cipher, nonce, sealed, last, index)?);
        index += 1;
    }
    Ok(plain)
}

fn cipher(key: &[u8]) -> Result<Aes256Gcm, String> {
    Aes256Gcm::new_from_slice(key)
        .map_err(|_| "Messages sent a media key of the wrong size.".into())
}

fn seal_chunk(
    cipher: &Aes256Gcm,
    nonce: &[u8],
    chunk: &[u8],
    last: bool,
    index: u32,
) -> Result<Vec<u8>, String> {
    let aad = aad(last, index);
    cipher
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: chunk,
                aad: &aad,
            },
        )
        .map_err(|_| "Messages could not seal that file.".into())
}

fn open_chunk(
    cipher: &Aes256Gcm,
    nonce: &[u8],
    sealed: &[u8],
    last: bool,
    index: u32,
) -> Result<Vec<u8>, String> {
    let aad = aad(last, index);
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: &aad,
            },
        )
        .map_err(|_| "Messages could not open that file.".into())
}

fn aad(last: bool, index: u32) -> [u8; 5] {
    let mut aad = [0u8; 5];
    aad[0] = u8::from(last);
    aad[1..5].copy_from_slice(&index.to_be_bytes());
    aad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_photo_seals_and_opens_including_a_short_last_chunk() {
        let key = [9u8; 32];
        let plain: Vec<u8> = (0..CHUNK as u32 + 20).map(|n| n as u8).collect();
        let packed = pack(&key, &plain).unwrap();
        assert_eq!(packed[0], 0);
        assert_eq!(packed[1], LOG2_CHUNK);
        assert_eq!(unpack(&key, &packed).unwrap(), plain);
        assert!(unpack(&[1u8; 32], &packed).is_err());
        assert!(unpack(&key, &[]).is_err());
    }
}
