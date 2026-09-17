//! Legatus IPC bridge (v1): JavaScript-to-Rust commands over an `ipc://` scheme.
//!
//! Page side (injected init script):
//!   `window.__legatusInvoke(cmd, args)` fetches
//!   `ipc://legatus/<cmd>?payload=<urlencoded-json>` and resolves the JSON reply.
//!
//! Rust side ([`IpcProtocolHandler`]): a Servo [`ProtocolHandler`] registered
//! for the `ipc` scheme at engine build time. It dispatches to the registered
//! [`IpcHandler`] in-process and answers with the JSON result.
//!
//! Why a protocol handler instead of delegate interception: Servo applies
//! mixed-content checks to embedder-intercepted responses, and `ipc://` is not
//! a trustworthy scheme, so `fetch("ipc://...")` from any secure page dies
//! with "Blocked as mixed content". A registered protocol with
//! `is_secure() == true` is explicitly exempted ("this only works for
//! bypassing mixed content checks right now"), and `is_fetchable() == true`
//! lets page `fetch()` read the body.
//!
//! Arguments travel in the URL query because v1 only issues GET fetches.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use servo::protocol_handler::{
  DoneChannel, FetchContext, HttpStatus, ProtocolHandler, Request, ResourceFetchTiming,
  Response, ResponseBody,
};

/// URL scheme carrying IPC calls.
pub const IPC_SCHEME: &str = "ipc";

/// Bootstraps `window.__legatusInvoke` in every page before its own scripts.
pub const INIT_SCRIPT: &str = r#"(function () {
  if (window.__legatusInvoke) return;
  window.__legatusInvoke = function (cmd, args) {
    var payload = encodeURIComponent(JSON.stringify(args ?? null));
    return fetch("ipc://legatus/" + encodeURIComponent(cmd) + "?payload=" + payload).then(
      function (response) {
        return response.json();
      },
    );
  };
})();"#;

/// A command handler: JSON in, JSON out. Must be fast — it runs on the main
/// thread inside the resource-load interception.
pub type IpcHandler = Arc<dyn Fn(serde_json::Value) -> serde_json::Value + Send + Sync>;

/// Registry shared between the delegate (main thread) and registration calls.
pub type IpcRegistry = Arc<Mutex<HashMap<String, IpcHandler>>>;

pub fn registry() -> IpcRegistry {
  Arc::new(Mutex::new(HashMap::new()))
}

/// Servo protocol handler serving `ipc://legatus/<cmd>?payload=<json>`.
/// Holds a clone of the shared dispatch table, so commands registered after
/// engine build (Tauri `invoke_handler` entries) go live immediately.
pub struct IpcProtocolHandler {
  dispatch: IpcRegistry,
}

impl IpcProtocolHandler {
  pub fn new(dispatch: IpcRegistry) -> Self {
    Self { dispatch }
  }
}

impl ProtocolHandler for IpcProtocolHandler {
  fn load<'a>(
    &'a self,
    request: &'a mut Request,
    _done_chan: &mut DoneChannel,
    _context: &FetchContext,
  ) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
    let url = request.current_url();
    let inner = url.as_url();
    let cmd = inner.path().trim_start_matches('/').to_string();
    let payload: serde_json::Value = inner
      .query_pairs()
      .find(|(key, _)| key == "payload")
      .and_then(|(_, value)| serde_json::from_str(&percent_decode(&value)).ok())
      .unwrap_or(serde_json::Value::Null);

    let reply = match self.dispatch.lock().ok().and_then(|map| map.get(&cmd).cloned()) {
      Some(handler) => handler(payload),
      None => serde_json::json!({ "error": format!("unknown IPC command: {cmd}") }),
    };
    let body = serde_json::to_vec(&reply).unwrap_or_else(|_| b"null".to_vec());

    let mut response = Response::new(url, ResourceFetchTiming::new(request.timing_type()));
    *response.body.lock() = ResponseBody::Done(body);
    if let Ok(content_type) = "application/json".parse() {
      response.headers.insert(http::header::CONTENT_TYPE, content_type);
    }
    response.status = HttpStatus::default();
    Box::pin(std::future::ready(response))
  }

  fn is_fetchable(&self) -> bool {
    true
  }

  fn is_secure(&self) -> bool {
    // Bypasses mixed-content blocking for `fetch("ipc://...")`. The responses
    // never touch the network; the handler runs in-process.
    true
  }
}

fn percent_decode(input: &str) -> String {
  let mut out = Vec::with_capacity(input.len());
  let bytes = input.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%' && i + 2 < bytes.len() {
      if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
        out.push(h << 4 | l);
        i += 3;
        continue;
      }
    }
    out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
    i += 1;
  }
  String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
  match byte {
    b'0'..=b'9' => Some(byte - b'0'),
    b'a'..=b'f' => Some(byte - b'a' + 10),
    b'A'..=b'F' => Some(byte - b'A' + 10),
    _ => None,
  }
}
