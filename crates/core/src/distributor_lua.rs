//! Distributor API clients ([`crate::mouser`], [`crate::jlcpcb`]) as Lua
//! scripts, loaded from disk at runtime — same bounded-host-API shape
//! [`crate::panel_lua`] already uses: sandboxed stdlib, a narrow function
//! contract validated at load time, serde-table marshaling at the boundary,
//! no `include_str!`.
//!
//! What stays host-side, same split as `panel_lua`'s "verified hardware data
//! stays Rust": real network I/O ([`lua_http_request`]) and real HMAC
//! signing ([`lua_hmac_sha256_base64`]) — a script gets narrow, purpose-built
//! primitives, never a raw socket or an `os`/`io` library. What's
//! swappable per script is the *source-specific* part every distributor
//! does differently: request shape, auth header, and how a JSON response
//! maps onto our types. So a new distributor (or a different API for one we
//! already have) is a new `.lua` file, not a Rust recompile.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;

use crate::tools::find_upward;

/// Errors loading or calling a distributor script.
#[derive(Debug, thiserror::Error)]
pub enum DistributorScriptError {
    #[error("distributor script {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: mlua::Error,
    },
    #[error("distributor script {path} must define a global function `{function}`")]
    MissingFn { path: PathBuf, function: String },
    #[error("distributor script {path}: {function}() call failed: {source}")]
    Call {
        path: PathBuf,
        function: String,
        #[source]
        source: mlua::Error,
    },
    #[error(
        "no distributor script directory found (set LOB_DISTRIBUTOR_SCRIPTS_DIR, \
         or run inside a checkout with assets/distributors)"
    )]
    NoScriptDir,
}

/// A loaded, ready-to-call distributor API script.
pub struct DistributorScript {
    lua: mlua::Lua,
    path: PathBuf,
}

impl DistributorScript {
    /// Loads a distributor script from disk, checking every name in
    /// `required_fns` is defined as a global function. Sandboxed to
    /// `table`/`string`/`math` plus the host primitives below — no
    /// filesystem, process, or real network library reachable directly.
    pub fn load(path: &Path, required_fns: &[&str]) -> Result<Self, DistributorScriptError> {
        let lua = mlua::Lua::new_with(
            mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH,
            mlua::LuaOptions::default(),
        )
        .map_err(|source| DistributorScriptError::Load {
            path: path.to_path_buf(),
            source,
        })?;

        install_host_fns(&lua, path)?;

        let src = std::fs::read_to_string(path).map_err(|e| DistributorScriptError::Load {
            path: path.to_path_buf(),
            source: mlua::Error::RuntimeError(format!("read {}: {e}", path.display())),
        })?;
        lua.load(src)
            .set_name(path.to_string_lossy())
            .exec()
            .map_err(|source| DistributorScriptError::Load {
                path: path.to_path_buf(),
                source,
            })?;

        for f in required_fns {
            if !lua
                .globals()
                .get::<mlua::Value>(*f)
                .is_ok_and(|v| v.is_function())
            {
                return Err(DistributorScriptError::MissingFn {
                    path: path.to_path_buf(),
                    function: (*f).to_string(),
                });
            }
        }

        Ok(DistributorScript {
            lua,
            path: path.to_path_buf(),
        })
    }

    /// Calls the global function `function(req)` and deserializes its return
    /// value. `req` and the return value are marshaled the same way
    /// `panel_lua`'s `layout(spec)` is — a single typed argument, a single
    /// typed (possibly `nil`/absent) result.
    pub fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        function: &str,
        req: &Req,
    ) -> Result<Resp, DistributorScriptError> {
        use mlua::LuaSerdeExt;

        let err = |source: mlua::Error| DistributorScriptError::Call {
            path: self.path.clone(),
            function: function.to_string(),
            source,
        };

        let f: mlua::Function = self.lua.globals().get(function).map_err(err)?;
        let req_value = self.lua.to_value(req).map_err(err)?;
        let result: mlua::Value = f.call(req_value).map_err(err)?;
        self.lua.from_value(result).map_err(err)
    }
}

/// Real HTTP + crypto primitives every distributor script gets — the part
/// that must stay host-side (real sockets, real HMAC) no matter which script
/// is loaded.
fn install_host_fns(lua: &mlua::Lua, path: &Path) -> Result<(), DistributorScriptError> {
    let wrap = |source: mlua::Error| DistributorScriptError::Load {
        path: path.to_path_buf(),
        source,
    };
    let globals = lua.globals();

    globals
        .set(
            "http_request",
            lua.create_function(lua_http_request).map_err(wrap)?,
        )
        .map_err(wrap)?;
    globals
        .set(
            "json_encode",
            lua.create_function(lua_json_encode).map_err(wrap)?,
        )
        .map_err(wrap)?;
    globals
        .set(
            "json_decode",
            lua.create_function(lua_json_decode).map_err(wrap)?,
        )
        .map_err(wrap)?;
    globals
        .set(
            "hmac_sha256_base64",
            lua.create_function(lua_hmac_sha256_base64).map_err(wrap)?,
        )
        .map_err(wrap)?;
    globals
        .set(
            "unix_timestamp",
            lua.create_function(|_, ()| Ok(unix_timestamp()))
                .map_err(wrap)?,
        )
        .map_err(wrap)?;
    globals
        .set(
            "nonce",
            lua.create_function(|_, ()| Ok(nonce())).map_err(wrap)?,
        )
        .map_err(wrap)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct HttpRequestSpec {
    method: String,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Serialize)]
struct HttpResponseOut {
    status: u16,
    body: String,
}

/// `http_request({method, url, headers, body}) -> {status, body}` — the one
/// primitive that actually opens a socket. A script never sees a raw
/// connection, only this narrow request/response shape (`ureq`, the same
/// HTTP client every other stage in this crate already uses).
fn lua_http_request(lua: &mlua::Lua, spec: mlua::Value) -> mlua::Result<mlua::Value> {
    use mlua::LuaSerdeExt;
    let spec: HttpRequestSpec = lua.from_value(spec)?;

    let mut req = match spec.method.to_ascii_uppercase().as_str() {
        "GET" => ureq::get(&spec.url),
        "POST" => ureq::post(&spec.url),
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "unsupported HTTP method {other}"
            )))
        }
    };
    for (k, v) in &spec.headers {
        req = req.set(k, v);
    }

    let result = match &spec.body {
        Some(body) => req.send_string(body),
        None => req.call(),
    };
    let (status, body) = match result {
        Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(e @ ureq::Error::Transport(_)) => {
            return Err(mlua::Error::RuntimeError(format!(
                "http request failed: {e}"
            )))
        }
    };

    lua.to_value(&HttpResponseOut { status, body })
}

/// `json_encode(table) -> string` — a script needs the exact wire bytes it's
/// about to send (e.g. to HMAC-sign them), so encoding is a call the script
/// makes itself rather than something `http_request` does implicitly.
fn lua_json_encode(lua: &mlua::Lua, value: mlua::Value) -> mlua::Result<String> {
    use mlua::LuaSerdeExt;
    let json: serde_json::Value = lua.from_value(value)?;
    serde_json::to_string(&json).map_err(|e| mlua::Error::RuntimeError(e.to_string()))
}

/// `json_decode(string) -> table`.
fn lua_json_decode(lua: &mlua::Lua, s: String) -> mlua::Result<mlua::Value> {
    use mlua::LuaSerdeExt;
    let json: serde_json::Value =
        serde_json::from_str(&s).map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
    lua.to_value(&json)
}

/// `hmac_sha256_base64(secret, message) -> string` — real crypto (the same
/// `hmac`/`sha2` crates [`crate::jlcpcb`] used directly before this
/// migration), not something a script could get right in bare Lua.
fn lua_hmac_sha256_base64(
    _lua: &mlua::Lua,
    (secret, message): (String, String),
) -> mlua::Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
    mac.update(message.as_bytes());
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A per-process-unique 32-hex-char nonce (time-nanos + counter — the kind of
/// server this signs requests for only needs uniqueness within its timestamp
/// window, not cryptographic randomness).
fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:016x}{counter:016x}")
}

/// Loads `<distributor_script_dir()>/<name>`, checking `required_fns` are
/// defined. The one entry point [`crate::mouser::MouserClient`] and
/// [`crate::jlcpcb::JlcpcbClient`] use to find their script.
pub fn load_named_script(
    name: &str,
    required_fns: &[&str],
) -> Result<DistributorScript, DistributorScriptError> {
    let dir = distributor_script_dir().ok_or(DistributorScriptError::NoScriptDir)?;
    DistributorScript::load(&dir.join(name), required_fns)
}

/// Where distributor scripts live, highest precedence first: an explicit
/// `LOB_DISTRIBUTOR_SCRIPTS_DIR` override, then walking up from the working
/// directory looking for this checkout's own `assets/distributors` (the dev
/// loop the "no recompile" goal is for — edit the script, rerun `lob`, no
/// `cargo build`), then the same global `~/.local/share/legion-of-bom` tree
/// [`crate::parts::default_parts_dir`]/[`crate::panel_lua::panel_script_dir`]
/// already use for user data.
pub fn distributor_script_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LOB_DISTRIBUTOR_SCRIPTS_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Some(dir) = find_upward("assets/distributors") {
        return Some(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    let global = base.join("legion-of-bom").join("distributors");
    global.is_dir().then_some(global)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempfile_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lob-distributor-lua-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[derive(Serialize)]
    struct Req {
        n: i64,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Resp {
        doubled: i64,
    }

    #[test]
    fn a_minimal_script_round_trips_a_call() {
        let dir = tempfile_dir();
        let path = dir.join("double.lua");
        std::fs::write(
            &path,
            r#"
            function double(req)
                return { doubled = req.n * 2 }
            end
            "#,
        )
        .unwrap();
        let script = DistributorScript::load(&path, &["double"]).unwrap();
        let resp: Resp = script.call("double", &Req { n: 21 }).unwrap();
        assert_eq!(resp, Resp { doubled: 42 });
    }

    #[test]
    fn missing_required_function_fails_at_load_not_call() {
        let dir = tempfile_dir();
        let path = dir.join("incomplete.lua");
        std::fs::write(&path, "function only_this() end").unwrap();
        assert!(matches!(
            DistributorScript::load(&path, &["double"]),
            Err(DistributorScriptError::MissingFn { .. })
        ));
    }

    #[test]
    fn sandbox_blocks_filesystem_and_os_access() {
        let dir = tempfile_dir();
        let path = dir.join("escape.lua");
        std::fs::write(
            &path,
            r#"
            function double(req)
                io.open("/etc/passwd", "r")
                return { doubled = 0 }
            end
            "#,
        )
        .unwrap();
        let script = DistributorScript::load(&path, &["double"]).unwrap();
        let result: Result<Resp, _> = script.call("double", &Req { n: 1 });
        assert!(result.is_err(), "io library must not be reachable");
    }

    #[test]
    fn hmac_signing_is_reachable_and_correct() {
        let dir = tempfile_dir();
        let path = dir.join("sign.lua");
        std::fs::write(
            &path,
            r#"
            function double(req)
                local sig = hmac_sha256_base64("topsecret", "hello")
                return { doubled = #sig }
            end
            "#,
        )
        .unwrap();
        let script = DistributorScript::load(&path, &["double"]).unwrap();
        let resp: Resp = script.call("double", &Req { n: 0 }).unwrap();
        // Base64 of a 32-byte SHA-256 digest is always 44 chars (with padding).
        assert_eq!(resp.doubled, 44);
    }

    #[test]
    fn an_empty_lua_table_deserializes_as_an_empty_vec() {
        let dir = tempfile_dir();
        let path = dir.join("empty_list.lua");
        std::fs::write(&path, "function double(req) return {} end").unwrap();
        let script = DistributorScript::load(&path, &["double"]).unwrap();
        let resp: Vec<i64> = script.call("double", &Req { n: 0 }).unwrap();
        assert_eq!(resp, Vec::<i64>::new());
    }

    #[test]
    fn nil_deserializes_as_none() {
        let dir = tempfile_dir();
        let path = dir.join("nil.lua");
        std::fs::write(&path, "function double(req) return nil end").unwrap();
        let script = DistributorScript::load(&path, &["double"]).unwrap();
        let resp: Option<Resp> = script.call("double", &Req { n: 0 }).unwrap();
        assert_eq!(resp, None);
    }

    #[test]
    fn json_encode_decode_round_trip_is_reachable() {
        let dir = tempfile_dir();
        let path = dir.join("json.lua");
        std::fs::write(
            &path,
            r#"
            function double(req)
                local s = json_encode({ a = req.n })
                local t = json_decode(s)
                return { doubled = t.a * 2 }
            end
            "#,
        )
        .unwrap();
        let script = DistributorScript::load(&path, &["double"]).unwrap();
        let resp: Resp = script.call("double", &Req { n: 7 }).unwrap();
        assert_eq!(resp.doubled, 14);
    }
}
