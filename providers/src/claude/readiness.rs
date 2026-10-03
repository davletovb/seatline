//! Only account/authentication inputs from Claude's mixed global state file
//! affect readiness. Startup counters, history and UI state do not.

use crate::readiness::Key;
use seatline_core::discovery::FileStamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, Read};
use std::path::PathBuf;

// The mixed state file may exceed the credential-file limit. Stream past
// unrelated fields without retaining them, but keep total work bounded.
const STATE_BYTES: u64 = 16 * 1024 * 1024;
const ACCOUNT_BYTES: usize = 1024 * 1024;

#[derive(Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct AccountConfig {
    oauth_account: Value,
    primary_api_key: Value,
    api_key_helper: Value,
    custom_api_key_responses: Value,
    #[serde(rename = "userID")]
    user_id: Value,
    has_available_subscription: Value,
    organization_type: Value,
    organization_uuid: Value,
    organization_rate_limit_tier: Value,
}

// Neither Debug nor Serialize: account paths/material remain private.
struct Watched {
    path: PathBuf,
    stamp: Option<FileStamp>,
    digest: Option<u64>,
}

#[derive(Default)]
pub(super) struct AccountFile(RefCell<Option<Watched>>);

impl AccountFile {
    pub(super) fn invalidate(&self) {
        self.0.borrow_mut().take();
    }

    pub(super) fn watch(&self, key: Key, path: PathBuf) -> Option<Key> {
        let stamp = FileStamp::read(&path).ok()?;
        if let Some(previous) = self.0.borrow().as_ref() {
            if previous.path == path && previous.stamp == stamp {
                return Some(key.with_projection(path, previous.digest));
            }
        }
        let digest = if stamp.is_some() {
            let file = std::fs::File::open(&path).ok()?;
            if file.metadata().ok()?.len() > STATE_BYTES {
                return None;
            }
            let mut reader = BufReader::new(file.take(STATE_BYTES + 1));
            let account: AccountConfig = serde_json::from_reader(&mut reader).ok()?;
            if reader.get_ref().limit() == 0 {
                return None;
            }
            let bytes = serde_json::to_vec(&account).ok()?;
            if bytes.len() > ACCOUNT_BYTES || FileStamp::read(&path).ok()? != stamp {
                return None;
            }
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::fs::canonicalize(&path).ok()?.hash(&mut hasher);
            bytes.hash(&mut hasher);
            Some(hasher.finish())
        } else {
            None
        };
        *self.0.borrow_mut() = Some(Watched {
            path: path.clone(),
            stamp,
            digest,
        });
        Some(key.with_projection(path, digest))
    }
}
