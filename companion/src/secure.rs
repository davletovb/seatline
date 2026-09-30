//! Frame protection for the hosted-app transport (web protocol 2).
//!
//! The relay is untrusted: it forwards `{"type":"data","seq","iv","body"}`
//! envelopes and can delay, drop, reorder, replay or forge them. This module
//! makes every one of those visible to the receiver.
//!
//! * AES-256-GCM under the pairing key, random 96-bit IV per frame.
//! * A handshake per relay connection. The browser sends a `hello` with a fresh
//!   nonce; the helper answers with its own fresh nonce and an echo of the
//!   browser's. The pair of nonces is the *epoch*.
//! * Data frames are bound to their direction, the epoch and their sequence
//!   number, which starts at 1 in each epoch and must increase by exactly one.
//!   A frame from an earlier epoch cannot verify, because each side's nonce is
//!   fresh, and a missing or repeated frame is a hard error, so the connection
//!   is dropped and the next handshake starts clean.
//!
//! The additional authenticated data is, byte for byte:
//!
//! * hello (envelope `seq` 0): `seatline:2:hello:{direction}`
//! * data: `seatline:2:{direction}:{helper nonce hex}:{browser nonce hex}:{seq}`
//!
//! `direction` is `browser` for frames the browser sends and `helper` for frames
//! the helper sends. Nonces are 16 random bytes in lowercase hex. The plaintext
//! of a hello is `{"type":"hello","nonce":"<hex>","echo":null|"<hex>"}`.
//! `tests/vectors/web-protocol-v2.json` fixes these bytes for other
//! implementations to check against.
use std::io;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::wire;

pub const WEB_PROTOCOL: u32 = 2;
const MAX_SEQUENCE: u64 = 9_007_199_254_740_991;

fn error(message: &str) -> io::Error {
    io::Error::other(message)
}

pub type Nonce16 = [u8; 16];

/// The two fresh nonces that scope one handshake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Epoch {
    pub helper: Nonce16,
    pub browser: Nonce16,
}

pub fn fresh_nonce() -> io::Result<Nonce16> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| error("randomness unavailable"))?;
    Ok(nonce)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn nonce_from_hex(text: &str) -> Option<Nonce16> {
    if text.len() != 32 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut nonce = [0_u8; 16];
    for (index, byte) in nonce.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(nonce)
}

fn hello_aad(direction: &str) -> Vec<u8> {
    format!("seatline:2:hello:{direction}").into_bytes()
}

fn data_aad(direction: &str, epoch: &Epoch, sequence: u64) -> Vec<u8> {
    format!(
        "seatline:2:{direction}:{}:{}:{sequence}",
        to_hex(&epoch.helper),
        to_hex(&epoch.browser)
    )
    .into_bytes()
}

fn seal_with_iv(
    key: &Aes256Gcm,
    iv: [u8; 12],
    aad: &[u8],
    sequence: u64,
    data: &[u8],
) -> io::Result<Value> {
    let encrypted = key
        .encrypt(Nonce::from_slice(&iv), Payload { msg: data, aad })
        .map_err(|_| error("encryption failed"))?;
    Ok(
        json!({"type":"data","seq":sequence,"iv":STANDARD.encode(iv),"body":STANDARD.encode(encrypted)}),
    )
}

fn seal(key: &Aes256Gcm, aad: &[u8], sequence: u64, value: &Value) -> io::Result<Value> {
    let mut iv = [0_u8; 12];
    getrandom::fill(&mut iv).map_err(|_| error("randomness unavailable"))?;
    seal_with_iv(key, iv, aad, sequence, &serde_json::to_vec(value)?)
}

fn open(key: &Aes256Gcm, aad: &[u8], envelope: &Value) -> io::Result<Value> {
    let iv = STANDARD
        .decode(envelope["iv"].as_str().ok_or_else(|| error("missing IV"))?)
        .map_err(|_| error("invalid IV"))?;
    if iv.len() != 12 {
        return Err(error("invalid IV"));
    }
    let body = STANDARD
        .decode(
            envelope["body"]
                .as_str()
                .ok_or_else(|| error("missing ciphertext"))?,
        )
        .map_err(|_| error("invalid ciphertext"))?;
    let clear = key
        .decrypt(Nonce::from_slice(&iv), Payload { msg: &body, aad })
        .map_err(|_| error("invalid encrypted message"))?;
    if clear.len() > wire::MAX_FRAME {
        return Err(error("message too large"));
    }
    serde_json::from_slice(&clear).map_err(io::Error::other)
}

/// The sequence number an envelope claims, before anything is decrypted.
pub fn envelope_sequence(envelope: &Value) -> io::Result<u64> {
    envelope["seq"]
        .as_u64()
        .filter(|sequence| *sequence <= MAX_SEQUENCE)
        .ok_or_else(|| error("invalid sequence"))
}

pub fn seal_hello(
    key: &Aes256Gcm,
    direction: &str,
    nonce: &Nonce16,
    echo: Option<&Nonce16>,
) -> io::Result<Value> {
    seal(
        key,
        &hello_aad(direction),
        0,
        &json!({"type":"hello","nonce":to_hex(nonce),"echo":echo.map(|echo| to_hex(echo))}),
    )
}

/// The sender's nonce, and the nonce it echoes back if it is answering a hello.
pub fn open_hello(
    key: &Aes256Gcm,
    direction: &str,
    envelope: &Value,
) -> io::Result<(Nonce16, Option<Nonce16>)> {
    if envelope_sequence(envelope)? != 0 {
        return Err(error("not a hello"));
    }
    let clear = open(key, &hello_aad(direction), envelope)?;
    if clear["type"] != "hello" {
        return Err(error("not a hello"));
    }
    let nonce = clear["nonce"]
        .as_str()
        .and_then(nonce_from_hex)
        .ok_or_else(|| error("invalid hello"))?;
    let echo = match &clear["echo"] {
        Value::Null => None,
        other => Some(
            other
                .as_str()
                .and_then(nonce_from_hex)
                .ok_or_else(|| error("invalid hello"))?,
        ),
    };
    Ok((nonce, echo))
}

pub fn seal_frame(
    key: &Aes256Gcm,
    direction: &str,
    epoch: &Epoch,
    sequence: u64,
    value: &Value,
) -> io::Result<Value> {
    if sequence == 0 || sequence > MAX_SEQUENCE {
        return Err(error("invalid sequence"));
    }
    seal(key, &data_aad(direction, epoch, sequence), sequence, value)
}

/// Opens a data frame under `epoch`. Whether its sequence number is the *next*
/// one is the caller's decision, made before it calls this.
pub fn open_frame(
    key: &Aes256Gcm,
    direction: &str,
    epoch: &Epoch,
    envelope: &Value,
) -> io::Result<Value> {
    let sequence = envelope_sequence(envelope)?;
    if sequence == 0 {
        return Err(error("invalid sequence"));
    }
    open(key, &data_aad(direction, epoch, sequence), envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::KeyInit;

    const VECTORS: &str = include_str!("../tests/vectors/web-protocol-v2.json");

    fn key() -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&[1; 32]).unwrap()
    }
    fn epoch() -> Epoch {
        Epoch {
            helper: [0xaa; 16],
            browser: [0xbb; 16],
        }
    }

    #[test]
    fn frames_reject_reflection_tampering_wrong_keys_and_other_epochs() {
        let value = json!({"id":"hello","method":"send","text":"private prompt"});
        let frame = seal_frame(&key(), "browser", &epoch(), 1, &value).unwrap();
        assert_eq!(
            open_frame(&key(), "browser", &epoch(), &frame).unwrap(),
            value
        );
        assert!(!frame.to_string().contains("private prompt"));

        // Sent by the browser, so it cannot be presented as the helper's.
        assert!(open_frame(&key(), "helper", &epoch(), &frame).is_err());
        // The sequence number is part of what is authenticated.
        let mut tampered = frame.clone();
        tampered["seq"] = json!(2);
        assert!(open_frame(&key(), "browser", &epoch(), &tampered).is_err());
        // Another key fails.
        let other = Aes256Gcm::new_from_slice(&[2; 32]).unwrap();
        assert!(open_frame(&other, "browser", &epoch(), &frame).is_err());
        // A frame from an earlier epoch fails when either nonce is different.
        for changed in [
            Epoch {
                helper: [0xcc; 16],
                ..epoch()
            },
            Epoch {
                browser: [0xcc; 16],
                ..epoch()
            },
        ] {
            assert!(open_frame(&key(), "browser", &changed, &frame).is_err());
        }
    }

    #[test]
    fn hello_is_authenticated_and_cannot_pose_as_data_or_the_other_direction() {
        let nonce = [7; 16];
        let hello = seal_hello(&key(), "browser", &nonce, None).unwrap();
        assert_eq!(hello["seq"], 0);
        assert_eq!(
            open_hello(&key(), "browser", &hello).unwrap(),
            (nonce, None)
        );
        assert!(open_hello(&key(), "helper", &hello).is_err());
        assert!(open_frame(&key(), "browser", &epoch(), &hello).is_err());
        let echoed = seal_hello(&key(), "helper", &[9; 16], Some(&nonce)).unwrap();
        assert_eq!(
            open_hello(&key(), "helper", &echoed).unwrap(),
            ([9; 16], Some(nonce))
        );
        // A data frame is not a hello.
        let data = seal_frame(&key(), "browser", &epoch(), 1, &json!({})).unwrap();
        assert!(open_hello(&key(), "browser", &data).is_err());
    }

    #[test]
    fn nonces_round_trip_and_only_lowercase_hex_is_accepted() {
        let nonce = fresh_nonce().unwrap();
        assert_eq!(nonce_from_hex(&to_hex(&nonce)), Some(nonce));
        assert_eq!(nonce_from_hex("AA".repeat(16).as_str()), None);
        assert_eq!(nonce_from_hex("aa"), None);
        assert_eq!(nonce_from_hex(&"zz".repeat(16)), None);
    }

    /// The published vectors pin the exact bytes, so an implementation in
    /// another language can be checked against them.
    #[test]
    fn published_vectors_match() {
        let vectors: Value = serde_json::from_str(VECTORS).unwrap();
        let key_bytes: Vec<u8> = (0..32)
            .map(|index| {
                u8::from_str_radix(
                    &vectors["key"].as_str().unwrap()[index * 2..index * 2 + 2],
                    16,
                )
                .unwrap()
            })
            .collect();
        let key = Aes256Gcm::new_from_slice(&key_bytes).unwrap();
        for case in vectors["cases"].as_array().unwrap() {
            let iv: Vec<u8> = STANDARD.decode(case["iv"].as_str().unwrap()).unwrap();
            let iv: [u8; 12] = iv.try_into().unwrap();
            let aad = case["aad"].as_str().unwrap().as_bytes();
            let plaintext = case["plaintext"].as_str().unwrap();
            let sequence = case["seq"].as_u64().unwrap();
            let sealed = seal_with_iv(&key, iv, aad, sequence, plaintext.as_bytes()).unwrap();
            assert_eq!(sealed["body"], case["body"], "{}", case["name"]);
            assert_eq!(
                open(&key, aad, &sealed).unwrap(),
                serde_json::from_str::<Value>(plaintext).unwrap()
            );

            // The documented AAD is what the protocol functions actually use.
            let direction = case["direction"].as_str().unwrap();
            if case["kind"] == "hello" {
                assert_eq!(aad, hello_aad(direction).as_slice(), "{}", case["name"]);
            } else {
                let epoch = Epoch {
                    helper: nonce_from_hex(case["helper_nonce"].as_str().unwrap()).unwrap(),
                    browser: nonce_from_hex(case["browser_nonce"].as_str().unwrap()).unwrap(),
                };
                assert_eq!(
                    aad,
                    data_aad(direction, &epoch, sequence).as_slice(),
                    "{}",
                    case["name"]
                );
            }
        }
    }
}
