//! Content-addressed storage for successful layout artifacts.
//!
//! Callers provide one serializable input value containing every input that can
//! affect placement or routing, plus an algorithm namespace that must change
//! whenever implementation behavior changes. The cache deliberately does not
//! infer those inputs from mutable process state.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const CACHE_FORMAT: &str = "legion-of-bom.layout-cache.v1";

/// A filesystem-backed content-addressed cache.
#[derive(Debug, Clone)]
pub struct LayoutCache {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    format: String,
    key: String,
    payload_sha256: String,
    payload: Value,
}

impl LayoutCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Compute the address for the complete behavior-affecting input value.
    ///
    /// `algorithm` is a caller-owned versioned namespace such as
    /// `"pathfinder-layout-v3"`; bump it when code changes can alter output.
    pub fn key<T: Serialize>(algorithm: &str, inputs: &T) -> Result<String, serde_json::Error> {
        let mut value = serde_json::to_value(inputs)?;
        canonicalize(&mut value);
        let bytes = serde_json::to_vec(&(CACHE_FORMAT, algorithm, value))?;
        Ok(hex_sha256(&bytes))
    }

    /// Read and validate a cached artifact. Corrupt, truncated, tampered, and
    /// old-format entries are safe misses; filesystem access errors remain loud.
    pub fn get<A: DeserializeOwned>(&self, key: &str) -> io::Result<Option<A>> {
        let path = self.path(key);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let envelope: Envelope = match serde_json::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(_) => return Ok(None),
        };
        if envelope.format != CACHE_FORMAT || envelope.key != key {
            return Ok(None);
        }
        let mut payload = envelope.payload;
        canonicalize(&mut payload);
        let payload_bytes = match serde_json::to_vec(&payload) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
        if hex_sha256(&payload_bytes) != envelope.payload_sha256 {
            return Ok(None);
        }
        Ok(serde_json::from_value(payload).ok())
    }

    /// Atomically store a successful artifact under `key`.
    ///
    /// The API intentionally has no failure-entry method: a failed or partial
    /// route must never become indistinguishable from a reusable success.
    pub fn put_success<A: Serialize>(&self, key: &str, artifact: &A) -> io::Result<()> {
        let mut payload = serde_json::to_value(artifact).map_err(invalid_data)?;
        canonicalize(&mut payload);
        let payload_bytes = serde_json::to_vec(&payload).map_err(invalid_data)?;
        let envelope = Envelope {
            format: CACHE_FORMAT.to_owned(),
            key: key.to_owned(),
            payload_sha256: hex_sha256(&payload_bytes),
            payload,
        };
        let bytes = serde_json::to_vec(&envelope).map_err(invalid_data)?;
        fs::create_dir_all(&self.root)?;

        let final_path = self.path(key);
        let temp_path = self.root.join(format!(".{key}.{}.tmp", std::process::id()));
        fs::write(&temp_path, bytes)?;
        match fs::rename(&temp_path, &final_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&temp_path);
                Err(e)
            }
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(canonicalize),
        Value::Object(map) => {
            let old = std::mem::take(map);
            let mut fields: Vec<_> = old.into_iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, mut value) in fields {
                canonicalize(&mut value);
                map.insert(name, value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Debug, Serialize)]
    struct Inputs {
        circuit: String,
        options: HashMap<String, usize>,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Artifact {
        board: String,
        score: f64,
    }

    fn temp_cache(test: &str) -> LayoutCache {
        let root = std::env::temp_dir().join(format!(
            "lob-layout-cache-{test}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        LayoutCache::new(root)
    }

    #[test]
    fn successful_hit_reproduces_exact_artifact() {
        let cache = temp_cache("hit");
        let inputs = Inputs {
            circuit: "netlist-v1".into(),
            options: HashMap::from([("iterations".into(), 6), ("grid_um".into(), 250)]),
        };
        let key = LayoutCache::key("router-v1", &inputs).unwrap();
        let artifact = Artifact {
            board: "(kicad_pcb exact bytes)".into(),
            score: 12.5,
        };

        cache.put_success(&key, &artifact).unwrap();

        assert_eq!(cache.get::<Artifact>(&key).unwrap(), Some(artifact));
        let _ = fs::remove_dir_all(cache.root);
    }

    #[test]
    fn changed_input_or_algorithm_misses() {
        let cache = temp_cache("stale");
        let mut inputs = Inputs {
            circuit: "netlist-v1".into(),
            options: HashMap::from([("iterations".into(), 6)]),
        };
        let old = LayoutCache::key("router-v1", &inputs).unwrap();
        cache
            .put_success(
                &old,
                &Artifact {
                    board: "old".into(),
                    score: 1.0,
                },
            )
            .unwrap();

        inputs.options.insert("iterations".into(), 7);
        let changed_input = LayoutCache::key("router-v1", &inputs).unwrap();
        let changed_code = LayoutCache::key("router-v2", &inputs).unwrap();
        assert_ne!(old, changed_input);
        assert_ne!(changed_input, changed_code);
        assert_eq!(cache.get::<Artifact>(&changed_input).unwrap(), None);
        assert_eq!(cache.get::<Artifact>(&changed_code).unwrap(), None);
        let _ = fs::remove_dir_all(cache.root);
    }

    #[test]
    fn object_field_order_does_not_change_key() {
        let a = serde_json::json!({"circuit": "x", "options": {"a": 1, "b": 2}});
        let b = serde_json::json!({"options": {"b": 2, "a": 1}, "circuit": "x"});
        assert_eq!(
            LayoutCache::key("router-v1", &a).unwrap(),
            LayoutCache::key("router-v1", &b).unwrap()
        );
    }

    #[test]
    fn corruption_is_a_safe_miss() {
        let cache = temp_cache("corrupt");
        let key = LayoutCache::key("router-v1", &"input").unwrap();
        cache
            .put_success(
                &key,
                &Artifact {
                    board: "good".into(),
                    score: 2.0,
                },
            )
            .unwrap();
        fs::write(cache.path(&key), b"{truncated").unwrap();

        assert_eq!(cache.get::<Artifact>(&key).unwrap(), None);
        let _ = fs::remove_dir_all(cache.root);
    }
}
