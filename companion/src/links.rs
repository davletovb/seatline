//! Linked browsers, and the background web service that keeps them connected.
//!
//! `seatline-companion link APP` makes a pairing for one browser on one of the
//! app's authorized websites and prints its link; the browser keeps it. The
//! pairing is stored in `web/APP.json` in the data directory (owner-only), so it
//! outlives restarts. `seatline-companion web`, started at login, keeps the
//! helper side of every stored link connected to the relay, as `pair` keeps its
//! one, so a linked browser finds Seatline whenever it opens the website.
//!
//! The service opens no port: its only connections are outbound, to the relay
//! and to the local broker. A link ends when the relay refuses it (expired or
//! unknown), when the app is revoked or reauthorized, or with `unlink`.
use crate::{config, web};
use aes_gcm::{Aes256Gcm, aead::KeyInit};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// Links kept for one website; linking another browser beyond this ends the oldest.
pub const MAX_LINKS: usize = 8;
/// How often the service looks for links made or removed since it last looked.
const RESCAN: Duration = Duration::from_secs(2);
/// How long a starting service waits for one that is stopping to let go.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// A link's helper side probes the relay more often than `pair` does and backs
/// off for less, so after sleep, a network change or a relay restart the website
/// finds Seatline again within seconds, not a minute.
fn link_timing() -> web::Timing {
    web::Timing {
        ping_every: Duration::from_secs(15),
        dead_after: Duration::from_secs(40),
        backoff_max: Duration::from_secs(15),
        ..web::Timing::default()
    }
}

/// One linked browser: the relay channel, both sides' credentials and the key.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub origin: String,
    pub relay: String,
    pub id: String,
    helper: String,
    browser: String,
    /// The pairing key, base64url without padding, as a pairing link carries it.
    key: String,
}

impl Link {
    /// The link the browser opens: the website, with the pairing in the fragment.
    pub fn url(&self) -> String {
        format!(
            "{}/#seatline={}:{}:{}",
            self.origin, self.id, self.browser, self.key
        )
    }
}

/// One app's links, for the grant whose token they were made under.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    token: String,
    links: Vec<Link>,
}

fn store_path(root: &Path, app: &str) -> io::Result<PathBuf> {
    // Checks that `app` is an app identifier before it names a file.
    config::app_path(root, app)?;
    Ok(root.join("web").join(format!("{app}.json")))
}

/// The links stored for `grant`. Those made under an earlier grant of the same
/// app are not its: reauthorizing an app ends its links.
fn load(root: &Path, grant: &config::Grant) -> io::Result<Vec<Link>> {
    let bytes = match std::fs::read(store_path(root, &grant.app)?) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let stored: Stored = serde_json::from_slice(&bytes)?;
    Ok(if config::same_token(&stored.token, &grant.token) {
        stored.links
    } else {
        Vec::new()
    })
}

fn save(root: &Path, grant: &config::Grant, links: Vec<Link>) -> io::Result<()> {
    let stored = Stored {
        token: grant.token.clone(),
        links,
    };
    config::write_private(
        &store_path(root, &grant.app)?,
        &serde_json::to_vec_pretty(&stored)?,
    )
}

/// Changes an app's links while holding the store's lock: `link`, `unlink` and
/// the service are separate processes.
fn change<T>(
    root: &Path,
    grant: &config::Grant,
    edit: impl FnOnce(&mut Vec<Link>) -> T,
) -> io::Result<T> {
    let mut waited = Duration::ZERO;
    let _lock = loop {
        match config::lock(root, "links.lock") {
            Ok(lock) => break lock,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock && waited < LOCK_WAIT => {
                std::thread::sleep(Duration::from_millis(20));
                waited += Duration::from_millis(20);
            }
            Err(error) => return Err(error),
        }
    };
    // A caller may hold a grant that was replaced while it waited (a link task of
    // the old grant, a `link` that was talking to the relay): its links are not
    // the current grant's, so it must not write over them.
    let current = config::load_grant(root, &grant.app)?;
    if !config::same_token(&current.token, &grant.token) {
        return Err(io::Error::other(
            "app authorization was replaced; link again",
        ));
    }
    let mut links = load(root, grant)?;
    let result = edit(&mut links);
    save(root, grant, links)?;
    Ok(result)
}

/// `value` if given, else the grant's only entry; it must be one the grant names.
fn chosen(given: Option<&str>, allowed: &[String], what: &str, app: &str) -> io::Result<String> {
    let value = match given {
        Some(value) => config::https_origin(value)?,
        None if allowed.len() == 1 => allowed[0].clone(),
        None if allowed.is_empty() => {
            return Err(io::Error::other(format!(
                "{app} is not authorized for any {what}; authorize it with one first"
            )));
        }
        None => {
            return Err(io::Error::other(format!(
                "{app} is authorized for several {what}s; name one of: {}",
                allowed.join(", ")
            )));
        }
    };
    if !allowed.contains(&value) {
        return Err(io::Error::other(format!(
            "{value} is not an authorized {what} of {app}"
        )));
    }
    Ok(value)
}

/// Links one more browser to `app`'s website: asks the relay for a pairing,
/// makes its key here and stores it for the service to keep connected.
pub async fn link(
    root: &Path,
    app: &str,
    origin: Option<&str>,
    relay: Option<&str>,
) -> io::Result<Link> {
    let grant = config::load_grant(root, app)?;
    let origin = chosen(origin, &grant.web_origins, "website", app)?;
    let relay = chosen(relay, &grant.web_relays, "relay", app)?;
    let relay_url = reqwest::Url::parse(&relay).map_err(io::Error::other)?;
    let pairing =
        web::request_pairing(&web::http_client()?, &grant.app, &relay_url, &origin).await?;
    let link = Link {
        origin,
        relay,
        id: pairing.id,
        helper: pairing.helper,
        browser: pairing.browser,
        key: URL_SAFE_NO_PAD.encode(web::new_key()?),
    };
    change(root, &grant, |links| {
        links.push(link.clone());
        let same_site = links.iter().filter(|l| l.origin == link.origin).count();
        let mut excess = same_site.saturating_sub(MAX_LINKS);
        links.retain(|l| {
            let drop = excess > 0 && l.origin == link.origin;
            excess -= usize::from(drop);
            !drop
        });
    })?;
    Ok(link)
}

/// Ends every link of `app`. Returns how many there were.
pub fn unlink(root: &Path, app: &str) -> io::Result<usize> {
    let grant = config::load_grant(root, app)?;
    change(root, &grant, std::mem::take).map(|links| links.len())
}

/// Whether the web service is running for this data directory.
pub fn service_running(root: &Path) -> bool {
    matches!(
        config::lock(root, "web-service.lock"),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
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
pub async fn serve(root: PathBuf) -> io::Result<()> {
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
    let service = Rc::new(Service {
        root,
        running: RefCell::new(HashMap::new()),
    });
    tokio::task::LocalSet::new()
        .run_until(async move {
            loop {
                service.reconcile();
                tokio::time::sleep(RESCAN).await;
            }
        })
        .await
}

struct Service {
    root: PathBuf,
    /// The links whose helper side is running, by relay channel id.
    running: RefCell<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl Service {
    /// Connects every stored link its grant still allows, and disconnects every
    /// link that is no longer stored (unlinked, revoked or reauthorized).
    fn reconcile(self: &Rc<Self>) {
        let mut wanted = HashMap::new();
        for grant in grants(&self.root) {
            for link in load(&self.root, &grant).unwrap_or_default() {
                if grant.web_origins.contains(&link.origin)
                    && grant.web_relays.contains(&link.relay)
                {
                    wanted.insert(link.id.clone(), (grant.clone(), link));
                }
            }
        }
        self.running.borrow_mut().retain(|id, task| {
            let keep = wanted.contains_key(id) && !task.is_finished();
            if !keep {
                task.abort();
            }
            keep
        });
        for (id, (grant, link)) in wanted {
            if !self.running.borrow().contains_key(&id) {
                let task = tokio::task::spawn_local(self.clone().keep(grant, link));
                self.running.borrow_mut().insert(id, task);
            }
        }
    }

    /// Keeps the helper side of `link` connected until it cannot work any more,
    /// then forgets it: the browser will have to be linked again.
    async fn keep(self: Rc<Self>, grant: config::Grant, link: Link) {
        let ended = async {
            let relay = reqwest::Url::parse(&link.relay).map_err(io::Error::other)?;
            let key = URL_SAFE_NO_PAD
                .decode(&link.key)
                .ok()
                .and_then(|key| Aes256Gcm::new_from_slice(&key).ok())
                .ok_or_else(|| io::Error::other("invalid stored pairing key"))?;
            web::run_channel(
                &self.root,
                &grant.app,
                &grant.token,
                &web::helper_endpoint(&relay, &link.id)?,
                &link.helper,
                &key,
                link_timing(),
            )
            .await
        }
        .await;
        if let Err(failure) = ended {
            eprintln!("Seatline: a link to {} ended: {failure}", link.origin);
        }
        let _ = change(&self.root, &grant, |links| {
            links.retain(|kept| kept.id != link.id)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "seatline-links-{}",
            &config::random_token().unwrap()[..12]
        ))
    }

    fn link(origin: &str, id: char) -> Link {
        Link {
            origin: origin.into(),
            relay: "https://relay.example.com".into(),
            id: id.to_string().repeat(64),
            helper: "b".repeat(64),
            browser: "c".repeat(64),
            key: "k".repeat(43),
        }
    }

    fn grant(app: &str, args: &[&str]) -> config::Grant {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        config::grant_from_args(app, "codex", &args).unwrap()
    }

    fn authorize(root: &Path, grant: &config::Grant) {
        config::write_private(
            &config::app_path(root, &grant.app).unwrap(),
            &serde_json::to_vec(grant).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_holder_of_a_replaced_grant_cannot_write_over_the_new_grants_links() {
        let root = root();
        let site = [
            "https://app.example.com",
            "--relay=https://relay.example.com",
        ];
        let old = grant("my_app", &site);
        authorize(&root, &old);
        // Reauthorized while a task of the old grant was still connected.
        let new = grant("my_app", &site);
        authorize(&root, &new);
        change(&root, &new, |links| {
            links.push(link("https://app.example.com", 'a'))
        })
        .unwrap();
        // The old task notices too late and tries to forget its link.
        let stale = change(&root, &old, |links| links.clear());
        assert!(stale.unwrap_err().to_string().contains("replaced"));
        assert_eq!(
            load(&root, &new).unwrap().len(),
            1,
            "the new grant's links survive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_link_carries_the_browser_side_only() {
        let url = link("https://app.example.com", 'a').url();
        assert_eq!(
            url,
            format!(
                "https://app.example.com/#seatline={}:{}:{}",
                "a".repeat(64),
                "c".repeat(64),
                "k".repeat(43)
            )
        );
        assert!(!url.contains(&"b".repeat(64)));
    }

    #[test]
    fn links_belong_to_the_grant_they_were_made_under_and_are_owner_only() {
        let root = root();
        let first = grant(
            "my_app",
            &[
                "https://app.example.com",
                "--relay=https://relay.example.com",
            ],
        );
        authorize(&root, &first);
        change(&root, &first, |links| {
            links.push(link("https://app.example.com", 'a'))
        })
        .unwrap();
        assert_eq!(load(&root, &first).unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store_path(&root, "my_app").unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reauthorized = grant("my_app", &[]);
        assert!(load(&root, &reauthorized).unwrap().is_empty());
        assert!(store_path(&root, "../escape").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_website_and_relay_default_to_the_only_ones_and_must_be_authorized() {
        let one = vec!["https://app.example.com".to_owned()];
        assert_eq!(
            chosen(None, &one, "website", "my_app").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            chosen(Some("https://app.example.com/"), &one, "website", "my_app").unwrap(),
            "https://app.example.com"
        );
        assert!(chosen(Some("https://other.example.com"), &one, "website", "my_app").is_err());
        assert!(chosen(None, &[], "website", "my_app").is_err());
        let two = vec![
            "https://a.example".to_owned(),
            "https://b.example".to_owned(),
        ];
        let several = chosen(None, &two, "website", "my_app").unwrap_err();
        assert!(
            several
                .to_string()
                .contains("https://a.example, https://b.example")
        );
    }

    #[test]
    fn unlink_ends_every_link_of_the_app() {
        let root = root();
        let app = grant(
            "my_app",
            &[
                "https://app.example.com",
                "--relay=https://relay.example.com",
            ],
        );
        config::write_private(
            &config::app_path(&root, "my_app").unwrap(),
            &serde_json::to_vec(&app).unwrap(),
        )
        .unwrap();
        change(&root, &app, |links| {
            links.push(link("https://app.example.com", 'a'));
            links.push(link("https://app.example.com", 'd'));
        })
        .unwrap();
        assert_eq!(unlink(&root, "my_app").unwrap(), 2);
        assert!(load(&root, &app).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
