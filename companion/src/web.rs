//! Generic outbound connection for an authorized hosted app. App orchestration
//! and relay hosting live outside Seatline. Only neutral provider frames pass.
use crate::{PROTOCOL_VERSION, client, config, wire};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::io;
use std::path::Path;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

fn error(message: &str) -> io::Error {
    io::Error::other(message)
}
fn aad(direction: &str, sequence: u64) -> Vec<u8> {
    format!("seatline:1:{direction}:{sequence}").into_bytes()
}

pub fn encrypt(
    key: &Aes256Gcm,
    direction: &str,
    sequence: u64,
    value: &Value,
) -> io::Result<Value> {
    let mut iv = [0_u8; 12];
    getrandom::fill(&mut iv).map_err(|_| error("randomness unavailable"))?;
    let data = serde_json::to_vec(value)?;
    let encrypted = key
        .encrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: &data,
                aad: &aad(direction, sequence),
            },
        )
        .map_err(|_| error("encryption failed"))?;
    Ok(
        json!({"type":"data","seq":sequence,"iv":STANDARD.encode(iv),"body":STANDARD.encode(encrypted)}),
    )
}
pub fn decrypt(key: &Aes256Gcm, direction: &str, envelope: &Value) -> io::Result<Value> {
    let sequence = envelope["seq"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991)
        .ok_or_else(|| error("invalid sequence"))?;
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
        .decrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: &body,
                aad: &aad(direction, sequence),
            },
        )
        .map_err(|_| error("invalid encrypted message"))?;
    if clear.len() > wire::MAX_FRAME {
        return Err(error("message too large"));
    }
    serde_json::from_slice(&clear).map_err(io::Error::other)
}

pub async fn pair(root: &Path, app: &str, relay: &str, site: &str, launch: bool) -> io::Result<()> {
    let grant = config::load_grant(root, app)?;
    let relay = reqwest::Url::parse(relay).map_err(io::Error::other)?;
    let mut site = reqwest::Url::parse(site).map_err(io::Error::other)?;
    if relay.scheme() != "https"
        || site.scheme() != "https"
        || !relay.username().is_empty()
        || !site.username().is_empty()
        || relay.password().is_some()
        || site.password().is_some()
        || relay.query().is_some()
        || relay.fragment().is_some()
        || !grant
            .web_origins
            .contains(&site.origin().ascii_serialization())
        || !grant
            .web_relays
            .contains(&relay.origin().ascii_serialization())
    {
        return Err(error(
            "website and relay must be locally authorized HTTPS origins",
        ));
    }
    let _lock = config::lock(root, &format!("{app}-web.lock"))?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let response = http
        .post(relay.join("/pair").map_err(io::Error::other)?)
        .json(&json!({"app":app,"origin":site.origin().ascii_serialization()}))
        .send()
        .await
        .map_err(io::Error::other)?;
    if !response.status().is_success() {
        return Err(error("relay refused pairing"));
    }
    let bytes = response.bytes().await.map_err(io::Error::other)?;
    if bytes.len() > 4096 {
        return Err(error("invalid pairing response"));
    }
    let pair: Value = serde_json::from_slice(&bytes)?;
    for name in ["id", "helper", "browser"] {
        if !pair[name]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(error("invalid pairing response"));
        }
    }
    let mut key_bytes = [0_u8; 32];
    getrandom::fill(&mut key_bytes).map_err(|_| error("randomness unavailable"))?;
    let key = Aes256Gcm::new_from_slice(&key_bytes).map_err(|_| error("invalid key"))?;
    site.set_fragment(Some(&format!(
        "seatline={}:{}:{}",
        pair["id"].as_str().unwrap(),
        pair["browser"].as_str().unwrap(),
        URL_SAFE_NO_PAD.encode(key_bytes)
    )));
    println!("Open this private pairing link:\n{site}");
    if launch {
        open_website(site.as_str())?;
    }
    let mut endpoint = relay
        .join(&format!(
            "/channels/{}/helper",
            pair["id"].as_str().unwrap()
        ))
        .map_err(io::Error::other)?;
    endpoint
        .set_scheme("wss")
        .map_err(|_| error("invalid relay scheme"))?;
    let (mut sent, mut received) = (0_u64, 0_u64);
    loop {
        let current = config::load_grant(root, app)?;
        if !config::same_token(&current.token, &grant.token) {
            return Err(error("app authorization revoked"));
        }
        let result: io::Result<()> = async {
            let (mut socket,_) = tokio_tungstenite::connect_async(endpoint.as_str()).await.map_err(io::Error::other)?;
            socket.send(Message::Text(json!({"type":"auth","token":pair["helper"]}).to_string().into())).await.map_err(io::Error::other)?;
            let mut broker_writer = None;
            let mut broker_reader: Option<tokio::task::JoinHandle<()>> = None;
            let (output,mut events) = tokio::sync::mpsc::channel::<Value>(64);
            let mut check = tokio::time::interval(Duration::from_secs(1));
            let result: io::Result<()> = async {
                loop {
                    tokio::select! {
                        _ = check.tick() => {
                            if !config::load_grant(root,app).is_ok_and(|current| config::same_token(&current.token,&grant.token)) { return Err(error("app authorization revoked")); }
                        },
                        event = events.recv(), if broker_writer.is_some() => {
                            let event = event.ok_or_else(|| error("broker disconnected"))?;
                            if event.is_null() { return Err(error("broker disconnected")); }
                            sent = sent.checked_add(1).ok_or_else(|| error("sequence exhausted"))?;
                            socket.send(Message::Text(encrypt(&key,"helper",sent,&event)?.to_string().into())).await.map_err(io::Error::other)?;
                        },
                        message = socket.next() => {
                            let Some(message) = message else { return Err(error("relay disconnected")); };
                            let message = message.map_err(io::Error::other)?;
                            let Message::Text(text) = message else { if message.is_close() {return Err(error("relay disconnected"));} continue; };
                            if text.len()>768*1024 { return Err(error("relay frame too large")); }
                            let value: Value = serde_json::from_str(&text)?;
                            match value["type"].as_str() {
                                Some("ready"|"peer") => {
                                    let connected = value["peer"]==true || value["connected"]==true;
                                    if !connected {
                                        broker_writer = None;
                                        if let Some(reader) = broker_reader.take() {reader.abort();}
                                        while events.try_recv().is_ok() {}
                                    } else if broker_writer.is_none() {
                                        let mut stream = client::connect(root).await?;
                                        wire::write_frame(&mut stream,&json!({"version":PROTOCOL_VERSION,"app":app,"token":grant.token})).await?;
                                        let hello = wire::read_frame(&mut stream).await?;
                                        if hello["type"]!="ready" {return Err(error("broker refused app"));}
                                        let (mut reader,writer) = tokio::io::split(stream);
                                        broker_writer = Some(writer);
                                        let output = output.clone();
                                        broker_reader = Some(tokio::spawn(async move {
                                            loop {match wire::read_frame(&mut reader).await {
                                                Ok(event) => {if output.send(event).await.is_err() {break;}},
                                                Err(_) => {let _ = output.send(Value::Null).await;break;},
                                            }}
                                        }));
                                    }
                                },
                                Some("data") => {
                                    let sequence = value["seq"].as_u64().ok_or_else(|| error("invalid sequence"))?;
                                    if sequence<=received {continue;}
                                    let request = decrypt(&key,"browser",&value)?;
                                    received = sequence;
                                    if let Some(writer) = &mut broker_writer {wire::write_frame(writer,&request).await?;}
                                },
                                Some("pong") => {},
                                _ => return Err(error("invalid relay frame")),
                            }
                        },
                    }
                }
            }.await;
            if let Some(reader) = broker_reader {reader.abort();}
            drop(broker_writer);
            result
        }.await;
        if result.is_err() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

#[allow(clippy::disallowed_methods)] // Opens a locally authorized HTTPS website with a fixed OS launcher; no shell.
fn open_website(url: &str) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let path =
            std::env::var_os("WINDIR").ok_or_else(|| error("Windows directory unavailable"))?;
        let mut command = std::process::Command::new(
            std::path::PathBuf::from(path).join("System32/rundll32.exe"),
        );
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("/usr/bin/open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("/usr/bin/xdg-open");
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_provider_frames_reject_reflection_tampering_and_wrong_keys() {
        let key = Aes256Gcm::new_from_slice(&[1; 32]).unwrap();
        let value = json!({"id":"hello","method":"send","text":"private prompt"});
        let frame = encrypt(&key, "browser", 1, &value).unwrap();
        assert_eq!(decrypt(&key, "browser", &frame).unwrap(), value);
        assert!(!frame.to_string().contains("private prompt"));
        assert!(decrypt(&key, "helper", &frame).is_err());
        let mut tampered = frame.clone();
        tampered["seq"] = json!(2);
        assert!(decrypt(&key, "browser", &tampered).is_err());
        assert!(
            decrypt(
                &Aes256Gcm::new_from_slice(&[2; 32]).unwrap(),
                "browser",
                &frame
            )
            .is_err()
        );
    }
}
