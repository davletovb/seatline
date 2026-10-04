//! The ledger's durable representation stays unchanged. Every in-memory
//! mutation also updates the native-session and per-app indexes, including
//! rollback after a failed durable write.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    pub app: String,
    pub provider: String,
    pub native: String,
}

type NativeKey = (String, String, String);

#[derive(Clone, Default)]
pub(crate) struct Sessions {
    entries: BTreeMap<String, Session>,
    native: BTreeMap<NativeKey, BTreeSet<String>>,
    counts: BTreeMap<String, usize>,
}

impl Sessions {
    pub fn token(&self, app: &str, provider: &str, native: &str) -> Option<&String> {
        self.native
            .get(&(app.to_owned(), provider.to_owned(), native.to_owned()))?
            .first()
    }

    pub fn tokens(&self, app: &str, provider: &str, native: &str) -> impl Iterator<Item = &String> {
        self.native
            .get(&(app.to_owned(), provider.to_owned(), native.to_owned()))
            .into_iter()
            .flat_map(|tokens| tokens.iter())
    }

    pub fn app_len(&self, app: &str) -> usize {
        self.counts.get(app).copied().unwrap_or(0)
    }

    pub fn insert(&mut self, token: String, session: Session) -> Option<Session> {
        let old = self.remove(&token);
        self.native
            .entry((
                session.app.clone(),
                session.provider.clone(),
                session.native.clone(),
            ))
            .or_default()
            .insert(token.clone());
        *self.counts.entry(session.app.clone()).or_default() += 1;
        self.entries.insert(token, session);
        old
    }

    pub fn remove(&mut self, token: &str) -> Option<Session> {
        let session = self.entries.remove(token)?;
        let key = (
            session.app.clone(),
            session.provider.clone(),
            session.native.clone(),
        );
        if let Some(tokens) = self.native.get_mut(&key) {
            tokens.remove(token);
            if tokens.is_empty() {
                self.native.remove(&key);
            }
        }
        if let Some(count) = self.counts.get_mut(&session.app) {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&session.app);
            }
        }
        Some(session)
    }

    pub fn extend(&mut self, entries: impl IntoIterator<Item = (String, Session)>) {
        for (token, session) in entries {
            self.insert(token, session);
        }
    }
}

impl Deref for Sessions {
    type Target = BTreeMap<String, Session>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl Serialize for Sessions {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.entries.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Sessions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let entries = BTreeMap::<String, Session>::deserialize(deserializer)?;
        let mut sessions = Self::default();
        sessions.extend(entries);
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(app: &str, native: &str) -> Session {
        Session {
            app: app.into(),
            provider: "codex".into(),
            native: native.into(),
        }
    }

    #[test]
    fn restart_replacement_duplicates_and_rollback_keep_indexes_consistent() {
        let mut sessions = Sessions::default();
        sessions.insert("b".into(), session("first", "native"));
        sessions.insert("a".into(), session("first", "native"));
        sessions.insert("c".into(), session("second", "native"));
        let mut sessions: Sessions =
            serde_json::from_slice(&serde_json::to_vec(&sessions).unwrap()).unwrap();
        assert_eq!(sessions.token("first", "codex", "native").unwrap(), "a");
        assert_eq!(sessions.app_len("first"), 2);
        assert_eq!(sessions.token("second", "codex", "native").unwrap(), "c");
        let removed = sessions.remove("a").unwrap();
        assert_eq!(sessions.token("first", "codex", "native").unwrap(), "b");
        sessions.extend([("a".into(), removed)]);
        assert_eq!(sessions.token("first", "codex", "native").unwrap(), "a");
        sessions.insert("a".into(), session("third", "different"));
        assert_eq!(sessions.app_len("first"), 1);
        assert_eq!(sessions.app_len("third"), 1);
        assert_eq!(sessions.token("first", "codex", "native").unwrap(), "b");
    }

    #[test]
    #[ignore = "manual microbenchmark; no timing assertion"]
    fn native_lookup_benchmark() {
        let mut sessions = Sessions::default();
        for n in 0..10_000 {
            sessions.insert(
                format!("token-{n:05}"),
                session("app", &format!("native-{n}")),
            );
        }
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(sessions.iter().find(|(_, s)| {
                s.app == "app" && s.provider == "codex" && s.native == "native-9999"
            }));
        }
        let scan = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(sessions.token("app", "codex", "native-9999"));
        }
        eprintln!(
            "native session 10000 entries, 10000 lookups: scan={scan:?}, indexed={:?}",
            start.elapsed()
        );
    }
}
