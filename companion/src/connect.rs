//! The background web service, `seatline-companion web`: a website an app is
//! authorized for connects to Seatline without anyone running `pair`.
//!
//! For each authorized website (an app, one of its website origins and one of
//! its relays) the service keeps one standing pairing. It is stored in the data
//! directory, readable by the user only, so it outlives restarts, and its helper
//! side stays connected to the relay exactly as `pair` keeps it.
//!
//! The browser gets its side from one page on the loopback interface,
//! `http://127.0.0.1:PORT/connect?app=APP&origin=ORIGIN&relay=RELAY`. The
//! website opens it in a window of its own (a click on its Connect button); the
//! page posts the pairing to the window that opened it, addressed to the
//! authorized origin, and closes. Opened any other way, it forwards to the
//! website with the pairing in the address's fragment, as a `pair` link does.
//! Chrome lets a public website open such a window without the local network
//! prompt that a `fetch` or a WebSocket to the loopback interface needs, and
//! Safari, which refuses those outright from an HTTPS page, opens it too.
//!
//! What the page does and does not do:
//!
//! * It answers only a top-level navigation (`Sec-Fetch-Dest: document`, when
//!   the browser says) addressed to the loopback host by name (`Host` is
//!   `127.0.0.1:PORT` or `localhost:PORT`), so no page can embed or fetch it or
//!   reach it through DNS rebinding, and it cannot be framed.
//! * It hands a pairing only to an origin that the app's grant names, and only
//!   by addressing that origin. A page elsewhere that opens it learns nothing,
//!   not even why nothing came.
//! * It never makes a second pairing for a website that has one: opening the
//!   page again hands over the same one. A pairing ends when the relay refuses
//!   it (expired or unknown) or the app is revoked or reauthorized; the next
//!   visit makes a new one.
use crate::{config, web};
use aes_gcm::{Aes256Gcm, aead::KeyInit};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The loopback port the service listens on unless `SEATLINE_WEB_PORT` says
/// otherwise. Below every platform's ephemeral range, so an outgoing connection
/// never holds it.
pub const DEFAULT_PORT: u16 = 27183;
/// A request head larger than this is refused.
const MAX_HEAD: usize = 8 * 1024;
/// How long a client has to send its request.
const READ_LIMIT: Duration = Duration::from_secs(5);
/// Requests answered at once; more wait for the operating system's backlog.
const MAX_REQUESTS: usize = 16;
/// How long a starting service waits for one that is stopping to let go.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// A standing pairing's helper side probes the relay more often than `pair`
/// does and backs off for less, so after sleep, a network change or a relay
/// restart the website finds Seatline again within seconds, not a minute.
fn standing_timing() -> web::Timing {
    web::Timing {
        ping_every: Duration::from_secs(15),
        dead_after: Duration::from_secs(40),
        backoff_max: Duration::from_secs(15),
        ..web::Timing::default()
    }
}

pub fn port() -> io::Result<u16> {
    match std::env::var("SEATLINE_WEB_PORT") {
        Ok(value) => value
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| io::Error::other("SEATLINE_WEB_PORT must be a port number")),
        Err(_) => Ok(DEFAULT_PORT),
    }
}

/// The browser side of a website's standing pairing, and the helper side.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Standing {
    origin: String,
    relay: String,
    id: String,
    helper: String,
    browser: String,
    /// The pairing key, base64url without padding, as a pairing link carries it.
    key: String,
}

/// One app's standing pairings, for the grant whose token they were made under.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    token: String,
    pairings: Vec<Standing>,
}

fn store_path(root: &Path, app: &str) -> io::Result<PathBuf> {
    // Checks that `app` is an app identifier before it names a file.
    config::app_path(root, app)?;
    Ok(root.join("web").join(format!("{app}.json")))
}

/// The pairings stored for `grant`. Those made under an earlier grant of the
/// same app are not its: reauthorizing an app ends its pairings.
fn load(root: &Path, grant: &config::Grant) -> io::Result<Vec<Standing>> {
    let bytes = match std::fs::read(store_path(root, &grant.app)?) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let stored: Stored = serde_json::from_slice(&bytes)?;
    Ok(if config::same_token(&stored.token, &grant.token) {
        stored.pairings
    } else {
        Vec::new()
    })
}

fn save(root: &Path, grant: &config::Grant, pairings: Vec<Standing>) -> io::Result<()> {
    let stored = Stored {
        token: grant.token.clone(),
        pairings,
    };
    config::write_private(
        &store_path(root, &grant.app)?,
        &serde_json::to_vec_pretty(&stored)?,
    )
}

/// Every app grant in the data directory.
fn grants(root: &Path) -> Vec<config::Grant> {
    let Ok(entries) = std::fs::read_dir(root.join("apps")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "json" {
                return None;
            }
            config::load_grant(root, path.file_stem()?.to_str()?).ok()
        })
        .collect()
}

/// Runs the service until it fails. A service that is already running is left
/// to it: this one says so and returns.
pub async fn serve(root: PathBuf, port: u16) -> io::Result<()> {
    let began = tokio::time::Instant::now();
    let _lock = loop {
        match config::lock(&root, "web-service.lock") {
            Ok(lock) => break lock,
            // One that is being replaced lets go within moments.
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && began.elapsed() < LOCK_WAIT =>
            {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                eprintln!("Seatline: the web service is already running.");
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    };
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let service = Rc::new(Service {
        root,
        port,
        http: web::http_client()?,
        store: tokio::sync::Mutex::new(()),
        running: RefCell::new(HashSet::new()),
    });
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_REQUESTS));
    tokio::task::LocalSet::new()
        .run_until(async move {
            service.resume();
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    // Out of file descriptors, say: try again shortly instead of ending.
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    continue;
                };
                let service = service.clone();
                tokio::task::spawn_local(async move {
                    let _permit = permit;
                    let _ = service.answer(stream).await;
                });
            }
        })
        .await
}

struct Service {
    root: PathBuf,
    port: u16,
    http: reqwest::Client,
    /// Serializes making and forgetting pairings, so two visits at once make one.
    store: tokio::sync::Mutex<()>,
    /// The pairings whose helper side is running: app, origin and relay.
    running: RefCell<HashSet<(String, String, String)>>,
}

impl Service {
    /// Connects the helper side of every stored pairing that its grant still allows.
    fn resume(self: &Rc<Self>) {
        for grant in grants(&self.root) {
            for standing in load(&self.root, &grant).unwrap_or_default() {
                if grant.web_origins.contains(&standing.origin)
                    && grant.web_relays.contains(&standing.relay)
                {
                    self.start_channel(&grant, standing);
                }
            }
        }
    }

    /// Keeps the helper side of `standing` connected until the pairing cannot
    /// work any more, then forgets it, so the next visit makes a new one.
    fn start_channel(self: &Rc<Self>, grant: &config::Grant, standing: Standing) {
        let running = (
            grant.app.clone(),
            standing.origin.clone(),
            standing.relay.clone(),
        );
        if !self.running.borrow_mut().insert(running.clone()) {
            return;
        }
        let service = self.clone();
        let (app, token) = (grant.app.clone(), grant.token.clone());
        tokio::task::spawn_local(async move {
            let ended = async {
                let relay = reqwest::Url::parse(&standing.relay).map_err(io::Error::other)?;
                let key = URL_SAFE_NO_PAD
                    .decode(&standing.key)
                    .ok()
                    .and_then(|key| Aes256Gcm::new_from_slice(&key).ok())
                    .ok_or_else(|| io::Error::other("invalid stored pairing key"))?;
                web::run_channel(
                    &service.root,
                    &app,
                    &token,
                    &web::helper_endpoint(&relay, &standing.id)?,
                    &standing.helper,
                    &key,
                    standing_timing(),
                )
                .await
            }
            .await;
            if let Err(failure) = ended {
                eprintln!(
                    "Seatline: the connection for {} ended: {failure}",
                    standing.origin
                );
            }
            service.running.borrow_mut().remove(&running);
            let _ = service.forget(&app, &standing.id).await;
        });
    }

    async fn forget(&self, app: &str, id: &str) -> io::Result<()> {
        let _serialized = self.store.lock().await;
        let grant = config::load_grant(&self.root, app)?;
        let mut pairings = load(&self.root, &grant)?;
        let before = pairings.len();
        pairings.retain(|standing| standing.id != id);
        if pairings.len() != before {
            save(&self.root, &grant, pairings)?;
        }
        Ok(())
    }

    /// The website's standing pairing, made now if it has none.
    async fn standing(
        self: &Rc<Self>,
        grant: &config::Grant,
        origin: &str,
        relay: &str,
    ) -> io::Result<Standing> {
        let _serialized = self.store.lock().await;
        let mut pairings = load(&self.root, grant)?;
        if let Some(found) = pairings
            .iter()
            .find(|standing| standing.origin == origin && standing.relay == relay)
        {
            let found = found.clone();
            self.start_channel(grant, found.clone());
            return Ok(found);
        }
        let relay_url = reqwest::Url::parse(relay).map_err(io::Error::other)?;
        let pairing = web::request_pairing(&self.http, &grant.app, &relay_url, origin).await?;
        let standing = Standing {
            origin: origin.to_owned(),
            relay: relay.to_owned(),
            id: pairing.id,
            helper: pairing.helper,
            browser: pairing.browser,
            key: URL_SAFE_NO_PAD.encode(web::new_key()?),
        };
        pairings.push(standing.clone());
        save(&self.root, grant, pairings)?;
        self.start_channel(grant, standing.clone());
        Ok(standing)
    }

    async fn answer(self: Rc<Self>, mut stream: TcpStream) -> io::Result<()> {
        let response = match tokio::time::timeout(READ_LIMIT, read_head(&mut stream)).await {
            Ok(Ok(head)) => match Request::parse(&head) {
                Some(request) => self.respond(&request).await,
                None => Response::message(400, "Bad request", "The request was not understood."),
            },
            _ => return Ok(()),
        };
        stream.write_all(&response.into_bytes()).await?;
        stream.shutdown().await
    }

    async fn respond(self: &Rc<Self>, request: &Request) -> Response {
        let connect = match check(request, self.port) {
            Ok(connect) => connect,
            Err(refusal) => return refusal,
        };
        let grant = config::load_grant(&self.root, &connect.app)
            .ok()
            .filter(|grant| grant.web_origins.contains(&connect.origin));
        let Some(grant) = grant else {
            return not_authorized(&connect);
        };
        let relay = match &connect.relay {
            Some(relay) if grant.web_relays.contains(relay) => relay.clone(),
            None if grant.web_relays.len() == 1 => grant.web_relays[0].clone(),
            _ => return not_authorized(&connect),
        };
        match self.standing(&grant, &connect.origin, &relay).await {
            Ok(standing) => deliver(&grant.app, &standing),
            Err(failure) => undelivered(
                &connect.origin,
                &format!("Seatline could not get a pairing from the relay: {failure}."),
            ),
        }
    }
}

async fn read_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::other("connection closed"));
        }
        head.extend_from_slice(&buffer[..read]);
        if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            head.truncate(end);
            return Ok(head);
        }
        if head.len() > MAX_HEAD {
            return Err(io::Error::other("request head too large"));
        }
    }
}

/// An HTTP/1.x request head: what the service reads of a request.
pub struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    pub fn parse(head: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(head).ok()?;
        let mut lines = text.split("\r\n");
        let mut parts = lines.next()?.split(' ');
        let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || !version.starts_with("HTTP/1.") {
            return None;
        }
        let mut headers = Vec::new();
        for line in lines {
            let (name, value) = line.split_once(':')?;
            if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
                return None;
            }
            headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        }
        Some(Self {
            method: method.to_owned(),
            target: target.to_owned(),
            headers,
        })
    }

    /// The header's value, when it is given exactly once.
    fn header(&self, name: &str) -> Option<&str> {
        let mut values = self
            .headers
            .iter()
            .filter(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str());
        let value = values.next()?;
        values.next().is_none().then_some(value)
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(candidate, _)| candidate == name)
    }
}

/// A request for a website's pairing that is well formed and may be answered.
#[derive(Debug, PartialEq, Eq)]
pub struct Connect {
    pub app: String,
    pub origin: String,
    pub relay: Option<String>,
}

/// Everything about a request that can be decided without the data directory.
pub fn check(request: &Request, port: u16) -> Result<Connect, Response> {
    if request.method != "GET" {
        return Err(Response::message(
            405,
            "Method not allowed",
            "This page only answers GET.",
        ));
    }
    // A page that resolved its own name to 127.0.0.1 (DNS rebinding) still
    // sends its own name here.
    let host = request.header("host");
    if host != Some(&format!("127.0.0.1:{port}")) && host != Some(&format!("localhost:{port}")) {
        return Err(Response::message(
            421,
            "Wrong address",
            "Open this page at 127.0.0.1.",
        ));
    }
    // Browsers say what a request is for: only a page of its own may be answered,
    // never a frame, a script, an image or a fetch. A client that does not say is
    // no browser, and could read the data directory anyway.
    let fetched_as = |name: &str, wanted: &str| {
        !request.has_header(name) || request.header(name) == Some(wanted)
    };
    if !fetched_as("sec-fetch-dest", "document") || !fetched_as("sec-fetch-mode", "navigate") {
        return Err(Response::message(
            403,
            "Not here",
            "This page only opens in a window of its own.",
        ));
    }
    let invalid = || Response::message(400, "Bad request", "This connection link is not valid.");
    if !request.target.starts_with('/') {
        return Err(invalid());
    }
    let url =
        url::Url::parse(&format!("http://127.0.0.1{}", request.target)).map_err(|_| invalid())?;
    if url.path() != "/connect" {
        return Err(Response::message(
            404,
            "Not found",
            "There is nothing at this address.",
        ));
    }
    let (mut app, mut origin, mut relay) = (None, None, None);
    for (name, value) in url.query_pairs() {
        let slot = match name.as_ref() {
            "app" => &mut app,
            "origin" => &mut origin,
            "relay" => &mut relay,
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(invalid());
        }
    }
    let app = app
        .filter(|app| seatline_core::turn::Namespace::fixed(app.as_str()).is_ok())
        .ok_or_else(invalid)?;
    let origin = origin
        .and_then(|origin| config::https_origin(&origin).ok())
        .ok_or_else(invalid)?;
    let relay = match relay {
        Some(relay) => Some(config::https_origin(&relay).map_err(|_| invalid())?),
        None => None,
    };
    Ok(Connect { app, origin, relay })
}

/// A page and the status it is answered with.
pub struct Response {
    status: u16,
    title: String,
    body: String,
    /// A script that runs on the page, under a nonce the page's policy names.
    script: Option<String>,
}

impl Response {
    fn message(status: u16, title: &str, text: &str) -> Self {
        Self {
            status,
            title: title.to_owned(),
            body: format!("<p>{}</p>", html(text)),
            script: None,
        }
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn into_bytes(self) -> Vec<u8> {
        let nonce = config::random_token()
            .map(|token| token[..32].to_owned())
            .unwrap_or_default();
        let script = match (&self.script, nonce.is_empty()) {
            (Some(script), false) => format!("<script nonce=\"{nonce}\">{script}</script>"),
            _ => String::new(),
        };
        let page = format!(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width\">\
             <title>Seatline \u{2014} {title}</title>\
             <style>:root{{color-scheme:light dark}}body{{font:15px/1.5 system-ui,sans-serif;\
             margin:12vh auto;max-width:30rem;padding:0 1.25rem}}code{{font-size:13px;\
             word-break:break-all}}</style><h1 style=\"font-size:18px\">{title}</h1>{body}{script}",
            title = html(&self.title),
            body = self.body,
        );
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            421 => "Misdirected Request",
            _ => "Bad Gateway",
        };
        let mut bytes = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: {length}\r\n\
             Cache-Control: no-store\r\n\
             X-Content-Type-Options: nosniff\r\n\
             X-Frame-Options: DENY\r\n\
             Referrer-Policy: no-referrer\r\n\
             Cross-Origin-Resource-Policy: same-origin\r\n\
             Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; \
             script-src 'nonce-{nonce}'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n\
             Connection: close\r\n\r\n",
            status = self.status,
            length = page.len(),
        )
        .into_bytes();
        bytes.extend_from_slice(page.as_bytes());
        bytes
    }

    #[cfg(test)]
    fn page(&self) -> String {
        format!("{}{}", self.body, self.script.as_deref().unwrap_or(""))
    }
}

fn html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A value as a JavaScript literal that cannot end the script it is in.
fn script_value(value: &Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// The message a website receives. Version 1 carries a pairing (relay, id,
/// browser token and key) or an error.
const MESSAGE_TYPE: &str = "seatline-pairing";

/// Hands `standing` to the window that opened the page, addressed to its
/// website, or forwards there with it when no window did.
fn deliver(app: &str, standing: &Standing) -> Response {
    let message = json!({
        "type": MESSAGE_TYPE, "version": 1, "app": app, "relay": standing.relay,
        "id": standing.id, "token": standing.browser, "key": standing.key,
    });
    let link = format!(
        "{}/#seatline={}:{}:{}",
        standing.origin, standing.id, standing.browser, standing.key
    );
    Response {
        status: 200,
        title: "Connecting".to_owned(),
        body: format!(
            "<p id=\"state\">Connecting {} to Seatline\u{2026}</p>",
            html(&standing.origin)
        ),
        script: Some(format!(
            "(function(){{var m={},o={},l={},w=window.opener;\
             if(w){{try{{w.postMessage(m,o)}}catch(e){{}}window.close();\
             document.getElementById(\"state\").textContent=\"Connected. You can close this window.\";return}}\
             location.replace(l)}})();",
            script_value(&message),
            script_value(&Value::String(standing.origin.clone())),
            script_value(&Value::String(link)),
        )),
    }
}

/// Tells an authorized website that no pairing could be had, and why.
fn undelivered(origin: &str, reason: &str) -> Response {
    let message = json!({"type": MESSAGE_TYPE, "version": 1, "error": "relay_unavailable", "message": reason});
    Response {
        status: 502,
        title: "Not connected".to_owned(),
        body: format!("<p>{}</p>", html(reason)),
        script: Some(format!(
            "(function(){{var w=window.opener;if(w){{try{{w.postMessage({},{})}}catch(e){{}}}}}})();",
            script_value(&message),
            script_value(&Value::String(origin.to_owned())),
        )),
    }
}

/// An origin the app's grant does not name, or no such app. The page says how
/// to allow it; the website is told nothing.
fn not_authorized(connect: &Connect) -> Response {
    let relay = connect.relay.as_deref().unwrap_or("RELAY_ORIGIN");
    Response {
        status: 403,
        title: "Not authorized".to_owned(),
        body: format!(
            "<p><b>{origin}</b> is not authorized to use Seatline as <b>{app}</b>.</p>\
             <p>If you trust it, allow it once in a terminal, naming the providers it may use:</p>\
             <p><code>seatline-companion authorize {app} codex,claude,gemini,grok {origin} --relay={relay}</code></p>\
             <p>Then try again from the website.</p>",
            origin = html(&connect.origin),
            app = html(&connect.app),
            relay = html(relay),
        ),
        script: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 27183;

    fn request(lines: &[&str]) -> Request {
        Request::parse(lines.join("\r\n").as_bytes()).expect("a request head")
    }

    fn navigation(target: &str) -> Request {
        request(&[
            &format!("GET {target} HTTP/1.1"),
            "Host: 127.0.0.1:27183",
            "Sec-Fetch-Dest: document",
            "Sec-Fetch-Mode: navigate",
            "Sec-Fetch-Site: cross-site",
        ])
    }

    const TARGET: &str =
        "/connect?app=my_app&origin=https%3A%2F%2Fapp.example.com&relay=https://relay.example.com";

    #[test]
    fn a_navigation_from_a_website_names_the_app_origin_and_relay() {
        assert_eq!(
            check(&navigation(TARGET), PORT).ok(),
            Some(Connect {
                app: "my_app".into(),
                origin: "https://app.example.com".into(),
                relay: Some("https://relay.example.com".into()),
            })
        );
        let without_relay = check(
            &navigation("/connect?app=my_app&origin=https://app.example.com"),
            PORT,
        );
        assert_eq!(without_relay.ok().and_then(|connect| connect.relay), None);
    }

    #[test]
    fn only_a_page_of_its_own_is_answered() {
        for (dest, mode) in [
            ("iframe", "navigate"),
            ("empty", "cors"),
            ("script", "no-cors"),
            ("image", "no-cors"),
            ("document", "cors"),
        ] {
            let embedded = request(&[
                &format!("GET {TARGET} HTTP/1.1"),
                "Host: 127.0.0.1:27183",
                &format!("Sec-Fetch-Dest: {dest}"),
                &format!("Sec-Fetch-Mode: {mode}"),
            ]);
            assert_eq!(check(&embedded, PORT).err().map(|r| r.status()), Some(403));
        }
        // A client that says nothing is no browser; it gets an answer.
        let plain = request(&[&format!("GET {TARGET} HTTP/1.1"), "Host: localhost:27183"]);
        assert!(check(&plain, PORT).is_ok());
    }

    #[test]
    fn a_page_reached_under_another_name_is_refused() {
        for host in [
            "Host: attacker.example:27183",
            "Host: 127.0.0.1:1",
            "Host: 127.0.0.1",
        ] {
            let rebound = request(&[
                &format!("GET {TARGET} HTTP/1.1"),
                host,
                "Sec-Fetch-Dest: document",
            ]);
            assert_eq!(check(&rebound, PORT).err().map(|r| r.status()), Some(421));
        }
        let twice = request(&[
            &format!("GET {TARGET} HTTP/1.1"),
            "Host: 127.0.0.1:27183",
            "Host: attacker.example",
        ]);
        assert_eq!(check(&twice, PORT).err().map(|r| r.status()), Some(421));
    }

    #[test]
    fn malformed_links_are_refused() {
        for target in [
            "/connect?app=My-App!&origin=https://app.example.com",
            "/connect?app=my_app&origin=http://app.example.com",
            "/connect?app=my_app&origin=https://app.example.com/path",
            "/connect?app=my_app&origin=https://app.example.com&origin=https://other.example",
            "/connect?app=my_app",
            "/connect?app=my_app&origin=https://app.example.com&relay=ftp://relay",
            "http://evil.example/connect?app=my_app&origin=https://app.example.com",
        ] {
            assert_eq!(
                check(&navigation(target), PORT).err().map(|r| r.status()),
                Some(400),
                "{target}"
            );
        }
        assert_eq!(
            check(&navigation("/"), PORT).err().map(|r| r.status()),
            Some(404)
        );
        let post = request(&[&format!("POST {TARGET} HTTP/1.1"), "Host: 127.0.0.1:27183"]);
        assert_eq!(check(&post, PORT).err().map(|r| r.status()), Some(405));
        assert!(Request::parse(b"GET / HTTP/2\r\nHost: x").is_none());
        assert!(Request::parse(b"GET /  HTTP/1.1").is_none());
        assert!(Request::parse(b"GET / HTTP/1.1\r\nBad Header: x").is_none());
    }

    fn standing(origin: &str) -> Standing {
        Standing {
            origin: origin.into(),
            relay: "https://relay.example.com".into(),
            id: "a".repeat(64),
            helper: "b".repeat(64),
            browser: "c".repeat(64),
            key: "k".repeat(43),
        }
    }

    #[test]
    fn a_pairing_is_addressed_to_its_website_and_never_carries_the_helper_credential() {
        let response = deliver("my_app", &standing("https://app.example.com"));
        let page = response.page();
        assert!(page.contains(r#""token":"cccc"#));
        assert!(page.contains(r#"o="https://app.example.com""#));
        assert!(page.contains("w.postMessage(m,o)"));
        assert!(page.contains(&format!(
            "https://app.example.com/#seatline={}:{}:{}",
            "a".repeat(64),
            "c".repeat(64),
            "k".repeat(43)
        )));
        assert!(!page.contains(&"b".repeat(64)));
        let bytes = String::from_utf8(response.into_bytes()).unwrap();
        let (head, body) = bytes.split_once("\r\n\r\n").unwrap();
        for header in [
            "X-Frame-Options: DENY",
            "frame-ancestors 'none'",
            "Cache-Control: no-store",
            "Referrer-Policy: no-referrer",
        ] {
            assert!(head.contains(header), "{header}");
        }
        // The script runs under the nonce the policy names, and under no other.
        let nonce = head
            .split("'nonce-")
            .nth(1)
            .unwrap()
            .split('\'')
            .next()
            .unwrap();
        assert_eq!(nonce.len(), 32);
        assert!(body.contains(&format!("<script nonce=\"{nonce}\">")));
        assert!(
            !head.contains("Cross-Origin-Opener-Policy"),
            "the opener must survive"
        );
    }

    #[test]
    fn nothing_on_the_page_can_close_its_script_or_inject_markup() {
        let hostile = Connect {
            app: "my_app".into(),
            origin: "https://a<script>.example".into(),
            relay: Some("https://r\"><b>.example".into()),
        };
        let page = not_authorized(&hostile).page();
        assert!(!page.contains("<script>") && !page.contains("\"><b>"));
        assert_eq!(
            script_value(&Value::String("</script><script>alert(1)".into())),
            "\"\\u003c/script\\u003e\\u003cscript\\u003ealert(1)\""
        );
        // An unauthorized website is not told anything.
        assert!(not_authorized(&hostile).script.is_none());
    }

    #[test]
    fn stored_pairings_belong_to_the_grant_they_were_made_under() {
        let root = std::env::temp_dir().join(format!(
            "seatline-connect-{}",
            &config::random_token().unwrap()[..12]
        ));
        let grant = config::grant_from_args(
            "my_app",
            "codex",
            &[
                "https://app.example.com".into(),
                "--relay=https://relay.example.com".into(),
            ],
        )
        .unwrap();
        save(&root, &grant, vec![standing("https://app.example.com")]).unwrap();
        assert_eq!(load(&root, &grant).unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store_path(&root, "my_app").unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reauthorized = config::grant_from_args("my_app", "codex", &[]).unwrap();
        assert!(load(&root, &reauthorized).unwrap().is_empty());
        assert!(store_path(&root, "../escape").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
