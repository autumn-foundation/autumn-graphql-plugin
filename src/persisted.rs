//! Persisted operations: Automatic Persisted Queries (APQ) and trusted
//! documents.
//!
//! - **APQ** ([`PersistedQueryMode::Automatic`](crate::config::PersistedQueryMode)):
//!   clients send `extensions.persistedQuery.sha256Hash` instead of the
//!   document. On a miss the server answers `PERSISTED_QUERY_NOT_FOUND`, the
//!   client re-sends hash + document, and the server verifies the hash before
//!   caching it in a [`PersistedQueryStore`]. Saves bandwidth; not a security
//!   boundary.
//! - **Trusted documents**
//!   ([`PersistedQueryMode::Trusted`](crate::config::PersistedQueryMode)): only
//!   documents in a manifest produced at client build time may run. Anything
//!   else is refused with `OPERATION_NOT_ALLOWLISTED`. This *is* a security
//!   boundary: an attacker cannot run a query your clients never shipped.
//!
//! Manifests may be Apollo's `persisted-query-manifest.json`
//! (`{"format":"apollo-persisted-query-manifest","version":1,"operations":[{"id","body",…}]}`)
//! or a flat `{"<sha256 hex>": "<document>"}` map. Requests may name a
//! document by the APQ extension or by a top-level `documentId` (the
//! GraphQL-over-HTTP persisted-documents proposal; a `sha256:` prefix is
//! accepted).

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// A boxed, `Send` future — the shape store methods return so the trait
/// stays object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where APQ registrations live.
///
/// The in-memory [`InMemoryPersistedQueryStore`] is per process; a fleet
/// behind a load balancer shares one by implementing this trait over Redis
/// or the database (a miss is harmless — the client re-sends the document —
/// so a best-effort store is fine).
pub trait PersistedQueryStore: Send + Sync + 'static {
    /// The document registered under `hash`, if any.
    fn get<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Option<Arc<str>>>;
    /// Register `document` under `hash`. The hash has already been verified.
    fn put<'a>(&'a self, hash: &'a str, document: Arc<str>) -> BoxFuture<'a, ()>;
}

/// A bounded, least-recently-used in-memory [`PersistedQueryStore`].
pub struct InMemoryPersistedQueryStore {
    cache: Mutex<lru::LruCache<String, Arc<str>>>,
}

impl InMemoryPersistedQueryStore {
    /// A store holding at most `capacity` documents (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            cache: Mutex::new(lru::LruCache::new(capacity)),
        }
    }

    /// Number of cached documents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.lock().map_or(0, |cache| cache.len())
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for InMemoryPersistedQueryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryPersistedQueryStore")
            .field("len", &self.len())
            .finish()
    }
}

impl PersistedQueryStore for InMemoryPersistedQueryStore {
    fn get<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Option<Arc<str>>> {
        // A poisoned lock only means another request panicked mid-update;
        // the LRU is still structurally sound, so keep serving from it.
        let found = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(hash)
            .cloned();
        Box::pin(std::future::ready(found))
    }

    fn put<'a>(&'a self, hash: &'a str, document: Arc<str>) -> BoxFuture<'a, ()> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .put(hash.to_owned(), document);
        Box::pin(std::future::ready(()))
    }
}

/// Lower-case hex SHA-256 of `text` — the APQ document hash.
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether `id` looks like a SHA-256 hex digest.
#[must_use]
pub fn is_sha256_hex(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A manifest could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The file could not be read.
    #[error("cannot read persisted-query manifest {path}: {source}")]
    Io {
        /// The manifest path.
        path: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The file is not a manifest in a supported format.
    #[error("invalid persisted-query manifest: {0}")]
    Invalid(String),
}

/// The allow-list of trusted documents.
///
/// Documents are addressable two ways: by the manifest's operation `id`
/// (what a client sends), and by the SHA-256 of their text (so a client that
/// sends the full document of a trusted operation is accepted too).
#[derive(Debug, Clone, Default)]
pub struct TrustedDocuments {
    by_id: HashMap<String, Arc<str>>,
    by_hash: HashMap<String, Arc<str>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ManifestFile {
    Apollo {
        format: String,
        version: u32,
        operations: Vec<ManifestOperation>,
    },
    Flat(HashMap<String, String>),
}

#[derive(Deserialize)]
struct ManifestOperation {
    id: String,
    body: String,
}

impl TrustedDocuments {
    /// An empty allow-list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a document, addressed by its SHA-256.
    #[must_use]
    pub fn with_document(mut self, document: impl Into<String>) -> Self {
        let document: String = document.into();
        let hash = sha256_hex(&document);
        self.insert(hash, document);
        self
    }

    /// Add a document under an explicit manifest `id`.
    pub fn insert(&mut self, id: impl Into<String>, document: impl Into<String>) {
        let document: Arc<str> = Arc::from(document.into());
        self.by_hash
            .insert(sha256_hex(&document), Arc::clone(&document));
        self.by_id
            .insert(normalize_id(&id.into()).to_owned(), document);
    }

    /// Parse a manifest (Apollo format or a flat `{id: document}` map).
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] for malformed JSON, an unknown
    /// Apollo manifest format/version, or a flat entry whose SHA-256 id does
    /// not match its document.
    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        let parsed: ManifestFile =
            serde_json::from_str(text).map_err(|e| ManifestError::Invalid(e.to_string()))?;
        let mut documents = Self::new();
        match parsed {
            ManifestFile::Apollo {
                format,
                version,
                operations,
            } => {
                if format != "apollo-persisted-query-manifest" || version != 1 {
                    return Err(ManifestError::Invalid(format!(
                        "unsupported manifest format {format:?} version {version}"
                    )));
                }
                for op in operations {
                    documents.insert(op.id, op.body);
                }
            }
            ManifestFile::Flat(map) => {
                for (id, body) in map {
                    let id = normalize_id(&id).to_ascii_lowercase();
                    if is_sha256_hex(&id) && sha256_hex(&body) != id {
                        return Err(ManifestError::Invalid(format!(
                            "entry {id} does not hash to its id"
                        )));
                    }
                    documents.insert(id, body);
                }
            }
        }
        Ok(documents)
    }

    /// Load a manifest file.
    ///
    /// # Errors
    ///
    /// See [`from_json`](Self::from_json); plus [`ManifestError::Io`].
    pub fn from_file(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_json(&text)
    }

    /// Merge another allow-list into this one.
    pub fn extend(&mut self, other: Self) {
        self.by_id.extend(other.by_id);
        self.by_hash.extend(other.by_hash);
    }

    /// The document with manifest id (or SHA-256) `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Arc<str>> {
        let id = normalize_id(id);
        self.by_id
            .get(id)
            .or_else(|| self.by_hash.get(&id.to_ascii_lowercase()))
    }

    /// Whether a document with this exact text is trusted.
    #[must_use]
    pub fn contains_document(&self, document: &str) -> bool {
        self.by_hash.contains_key(&sha256_hex(document))
    }

    /// Number of distinct trusted documents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Whether the allow-list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

/// Strip the `sha256:` prefix the persisted-documents proposal allows.
fn normalize_id(id: &str) -> &str {
    id.strip_prefix("sha256:").unwrap_or(id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const DOC: &str = "{ notes { id } }";

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_sha256_hex(&sha256_hex(DOC)));
        assert!(!is_sha256_hex("xyz"));
    }

    #[tokio::test]
    async fn lru_store_evicts_least_recently_used() {
        let store = InMemoryPersistedQueryStore::new(2);
        store.put("a", Arc::from("A")).await;
        store.put("b", Arc::from("B")).await;
        assert_eq!(store.get("a").await.as_deref(), Some("A"));
        store.put("c", Arc::from("C")).await;
        assert_eq!(store.get("b").await, None, "b was least recently used");
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
        assert!(InMemoryPersistedQueryStore::new(0).is_empty());
    }

    #[test]
    fn apollo_manifest_is_addressable_by_id_and_by_hash() {
        let manifest = format!(
            r#"{{"format":"apollo-persisted-query-manifest","version":1,
                "operations":[{{"id":"notes-v1","name":"Notes","type":"query","body":"{DOC}"}}]}}"#
        );
        let docs = TrustedDocuments::from_json(&manifest).unwrap();
        assert_eq!(docs.get("notes-v1").map(|d| &**d), Some(DOC));
        assert_eq!(docs.get(&sha256_hex(DOC)).map(|d| &**d), Some(DOC));
        assert!(docs.contains_document(DOC));
        assert!(!docs.contains_document("{ other }"));
        assert_eq!(docs.len(), 1);
    }

    #[test]
    fn flat_manifest_is_verified() {
        let hash = sha256_hex(DOC);
        let docs = TrustedDocuments::from_json(&format!(r#"{{"sha256:{hash}":"{DOC}"}}"#)).unwrap();
        assert!(docs.get(&format!("sha256:{hash}")).is_some());
        assert!(docs.get(&hash.to_ascii_uppercase()).is_some());

        let wrong = sha256_hex("{ other }");
        assert!(TrustedDocuments::from_json(&format!(r#"{{"{wrong}":"{DOC}"}}"#)).is_err());
    }

    #[test]
    fn bad_manifests_are_rejected() {
        assert!(TrustedDocuments::from_json("[]").is_err());
        assert!(
            TrustedDocuments::from_json(
                r#"{"format":"apollo-persisted-query-manifest","version":2,"operations":[]}"#
            )
            .is_err()
        );
        assert!(TrustedDocuments::from_file(Path::new("/nonexistent/manifest.json")).is_err());
    }

    #[test]
    fn documents_can_be_added_in_code_and_merged() {
        let mut docs = TrustedDocuments::new().with_document(DOC);
        let mut other = TrustedDocuments::new();
        other.insert("custom", "{ x }");
        docs.extend(other);
        assert_eq!(docs.len(), 2);
        assert!(docs.get("custom").is_some());
        assert!(!docs.is_empty());
    }
}
