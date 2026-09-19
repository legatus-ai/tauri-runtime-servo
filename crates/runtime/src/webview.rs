//! Servo webview management for the Tauri runtime.
//!
//! [`servo::WebView`] is `!Send` (reference counted, main-thread only), so
//! every live view is owned by the main thread inside [`Webviews`].
//! [`ServoWebviewDispatcher`] routes operations through the event-loop proxy.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{
  Arc,
  mpsc::{SyncSender, sync_channel},
};

use raw_window_handle::HasWindowHandle;
use servo::{
  InputEvent, RenderingContext, Servo, ServoBuilder, WebView, WebViewBuilder, WindowRenderingContext,
};
use embedder_traits::EventLoopWaker;
use tauri_runtime::{
  Error, Result, UserEvent, WebviewEventId,
  dpi::Rect,
  window::{WebviewEvent, WindowId},
};
use url::Url;

use crate::{RuntimeMessage, Shared, unsupported};

/// Shareable webview-event handler. The `Mutex` supplies `Sync`.
pub type SharedWebviewHandler = std::sync::Arc<std::sync::Mutex<Box<dyn Fn(&WebviewEvent) + Send>>>;

/// Per-webview state. Main thread only.
pub struct WebviewEntry {
  pub label: String,
  pub window_id: WindowId,
  pub view: WebView,
  pub handlers: HashMap<WebviewEventId, SharedWebviewHandler>,
  pub next_handler_id: WebviewEventId,
  pub ipc: crate::ipc::IpcRegistry,
}

impl std::fmt::Debug for WebviewEntry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WebviewEntry")
      .field("label", &self.label)
      .finish_non_exhaustive()
  }
}

/// All live webviews, keyed by label. Main thread only.
#[derive(Debug, Default)]
pub struct Webviews {
  inner: HashMap<String, WebviewEntry>,
}

impl Webviews {
  pub fn insert(&mut self, entry: WebviewEntry) {
    self.inner.insert(entry.label.clone(), entry);
  }

  pub fn get(&self, label: &str) -> Option<&WebviewEntry> {
    self.inner.get(label)
  }

  pub fn get_mut(&mut self, label: &str) -> Option<&mut WebviewEntry> {
    self.inner.get_mut(label)
  }

  pub fn remove(&mut self, label: &str) -> Option<WebviewEntry> {
    self.inner.remove(label)
  }

  pub fn iter_for_window(&self, window_id: WindowId) -> impl Iterator<Item = (String, &WebviewEntry)> {
    self
      .inner
      .iter()
      .filter(move |(_, entry)| entry.window_id == window_id)
      .map(|(label, entry)| (label.clone(), entry))
  }
}

/// Wakes the Tao event loop when Servo has work. Cloned into the Servo
/// instance at creation.
#[derive(Debug, Clone)]
struct ServoWaker<T: UserEvent> {
  proxy: tao::event_loop::EventLoopProxy<crate::TaoMessage<T>>,
}

impl<T: UserEvent> EventLoopWaker for ServoWaker<T> {
  fn clone_box(&self) -> Box<dyn EventLoopWaker> {
    Box::new(self.clone())
  }

  fn wake(&self) {
    let _ = self.proxy.send_event(crate::TaoMessage::ServoWake);
  }
}

/// Per-webview delegate: redraw requests. IPC is served by the `ipc://`
/// protocol handler (see [`crate::ipc`]), not by load interception — the
/// delegate declines every load so the pipeline reaches the handler.
#[derive(Clone)]
struct LegatusDelegate<T: UserEvent> {
  proxy: tao::event_loop::EventLoopProxy<crate::TaoMessage<T>>,
  window_id: tao::window::WindowId,
}

impl<T: UserEvent> std::fmt::Debug for LegatusDelegate<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LegatusDelegate")
      .field("window_id", &self.window_id)
      .finish_non_exhaustive()
  }
}

impl<T: UserEvent> servo::WebViewDelegate for LegatusDelegate<T> {
  fn notify_new_frame_ready(&self, _: WebView) {
    let _ = self.proxy.send_event(crate::TaoMessage::RequestRedraw(self.window_id));
  }

  fn load_web_resource(&self, _webview: WebView, _load: servo::WebResourceLoad) {
    // Decline everything (dropping the load falls through to default
    // handling). `ipc://` calls are served by the protocol handler
    // registered on the Servo builder, which also carries the
    // mixed-content exemption.
  }
}

/// Build the shared Servo instance on first use. Must run on the main thread.
pub fn ensure_servo<'a, T: UserEvent>(
  servo: &'a mut Option<Servo>,
  shared: &crate::Shared<T>,
) -> &'a Servo {
  if servo.is_none() {
    let mut protocols = servo::protocol_handler::ProtocolRegistry::default();
    if let Err(error) = protocols.register(
      crate::ipc::IPC_SCHEME,
      crate::ipc::IpcProtocolHandler::new(shared.ipc.clone()),
    ) {
      log::error!("servo runtime: failed to register ipc protocol: {error:?}");
    }
    // App-critical web features that Servo gates behind prefs: enable the
    // batch, then keep only what probes functional (see probe matrix).
    let mut preferences = servo::Preferences::default();
    preferences.dom_indexeddb_enabled = true;
    preferences.dom_async_clipboard_enabled = true;
    preferences.dom_notification_enabled = true;
    preferences.dom_fontface_enabled = true;
    preferences.dom_resize_observer_enabled = true;
    preferences.dom_intersection_observer_enabled = true;
    preferences.dom_cookiestore_enabled = true;
    preferences.dom_sanitizer_enabled = true;
    preferences.dom_web_animations_enabled = true;
    preferences.dom_exec_command_enabled = true;
    preferences.dom_composition_event_enabled = true;
    preferences.dom_storage_manager_api_enabled = true;
    let instance = ServoBuilder::default()
      .event_loop_waker(Box::new(ServoWaker {
        proxy: shared.proxy.clone(),
      }))
      .protocol_registry(protocols)
      .preferences(preferences)
      .build();
    instance.setup_logging();
    *servo = Some(instance);
  }
  servo.as_ref().expect("servo just created")
}

/// Create a Servo webview filling `window`. Must run on the main thread.
#[allow(clippy::too_many_arguments)]
pub fn create_view<T: UserEvent>(
  servo: &Servo,
  proxy: &tao::event_loop::EventLoopProxy<crate::TaoMessage<T>>,
  window: &tao::window::Window,
  display: raw_window_handle::RawDisplayHandle,
  url: Url,
) -> Result<(WebView, Rc<WindowRenderingContext>)> {
  // Sound: the display outlives the application; the borrowed handle is used
  // synchronously to build the rendering context.
  let display = unsafe { raw_window_handle::DisplayHandle::borrow_raw(display) };
  let window_handle = window
    .window_handle()
    .map_err(|e| Error::CreateWebview(format!("no window handle: {e}").into()))?;
  let rendering_context = Rc::new(
    WindowRenderingContext::new(display, window_handle, window.inner_size())
      .map_err(|e| Error::CreateWebview(format!("no rendering context: {e:?}").into()))?,
  );
  let _ = rendering_context.make_current();
  let delegate = Rc::new(LegatusDelegate::<T> {
    proxy: proxy.clone(),
    window_id: window.id(),
  });
  // The builder creates no content manager by default; ours carries the IPC
  // helper script into every page.
  let content_manager = Rc::new(servo::UserContentManager::new(servo));
  content_manager.add_script(Rc::new(servo::UserScript::new(
    crate::ipc::INIT_SCRIPT.to_string(),
    None,
  )));
  let view = WebViewBuilder::new(servo, rendering_context.clone())
    .url(url)
    .hidpi_scale_factor(euclid::Scale::new(window.scale_factor() as f32))
    .delegate(delegate)
    .user_content_manager(content_manager)
    .build();
  Ok((view, rendering_context))
}

/// Convert an evaluated [`JSValue`](servo::JSValue) to JSON text.
pub fn jsvalue_to_json(value: &servo::JSValue) -> serde_json::Value {
  use servo::JSValue as J;
  match value {
    J::Undefined | J::Null => serde_json::Value::Null,
    J::Boolean(b) => serde_json::Value::Bool(*b),
    J::Number(n) => serde_json::Number::from_f64(*n)
      .map(serde_json::Value::Number)
      .unwrap_or(serde_json::Value::Null),
    J::String(s) | J::Element(s) | J::ShadowRoot(s) | J::Frame(s) | J::Window(s) => {
      serde_json::Value::String(s.clone())
    }
    J::Array(items) => serde_json::Value::Array(items.iter().map(jsvalue_to_json).collect()),
    J::Object(entries) => serde_json::Value::Object(
      entries
        .iter()
        .map(|(key, item)| (key.clone(), jsvalue_to_json(item)))
        .collect(),
    ),
  }
}

/// A webview operation applied on the main thread.
pub enum WebviewOp {
  Url(SyncSender<Result<String>>),
  Navigate(Url),
  Reload,
  GoBack,
  GoForward,
  EvalScript {
    script: String,
  },
  EvalScriptWithCallback {
    script: String,
    tx: SyncSender<String>,
  },
  RegisterIpc {
    command: String,
    handler: crate::ipc::IpcHandler,
  },
  Close,
  OnEvent {
    handler: SharedWebviewHandler,
    tx: SyncSender<WebviewEventId>,
  },
  Hide,
  Show,
  SetFocus,
}

/// `Send + Sync` handle to a main-thread Servo webview.
#[derive(Debug, Clone)]
pub struct ServoWebviewDispatcher<T: UserEvent> {
  pub label: String,
  pub shared: Arc<Shared<T>>,
}

impl<T: UserEvent> ServoWebviewDispatcher<T> {
  fn send(&self, op: WebviewOp) -> Result<()> {
    self
      .shared
      .proxy
      .send_event(crate::TaoMessage::Runtime(RuntimeMessage::Webview {
        label: self.label.clone(),
        op,
      }))
      .map_err(|_| Error::FailedToSendMessage)
  }

  fn roundtrip<R: Send + 'static>(
    &self,
    make: impl FnOnce(SyncSender<Result<R>>) -> WebviewOp,
  ) -> Result<R> {
    let (tx, rx) = sync_channel(1);
    self.send(make(tx))?;
    rx.recv().map_err(|_| Error::FailedToReceiveMessage)?
  }

  pub fn navigate(&self, url: Url) -> Result<()> {
    self.send(WebviewOp::Navigate(url))
  }

  pub fn url(&self) -> Result<String> {
    self.roundtrip(WebviewOp::Url)
  }

  pub fn reload(&self) -> Result<()> {
    self.send(WebviewOp::Reload)
  }
}

/// Apply a [`WebviewOp`] against live state on the main thread.
pub fn apply(views: &mut Webviews, label: &str, op: WebviewOp) {
  let Some(entry) = views.get_mut(label) else {
    log::warn!("servo runtime: webview op for unknown webview");
    return;
  };
  if let WebviewOp::OnEvent { handler, tx } = op {
    let handler_id = entry.next_handler_id;
    entry.next_handler_id += 1;
    entry.handlers.insert(handler_id, handler);
    let _ = tx.send(handler_id);
    return;
  }
  let view = &entry.view;
  match op {
    WebviewOp::OnEvent { .. } => unreachable!("handled above"),
    WebviewOp::Url(tx) => {
      let _ = tx.send(Ok(view.url().map(|u| u.to_string()).unwrap_or_default()));
    }
    WebviewOp::Navigate(url) => view.load(url),
    WebviewOp::Reload => view.reload(),
    WebviewOp::GoBack => {
      view.go_back(1);
    }
    WebviewOp::GoForward => {
      log::warn!("servo runtime: go-forward is not exposed by Servo 0.5");
    }
    WebviewOp::EvalScript { script } => {
      view.evaluate_javascript(script, |_| {});
    }
    WebviewOp::EvalScriptWithCallback { script, tx } => {
      view.evaluate_javascript(script, move |result| {
        let json = match result {
          Ok(value) => serde_json::to_string(&jsvalue_to_json(&value))
            .unwrap_or_else(|_| "null".to_string()),
          Err(error) => serde_json::to_string(&format!("eval error: {error:?}"))
            .unwrap_or_else(|_| "\"eval error\"".to_string()),
        };
        let _ = tx.send(json);
      });
    }
    WebviewOp::RegisterIpc { command, handler } => {
      if let Ok(mut registry) = entry.ipc.lock() {
        registry.insert(command, handler);
      }
    }
    WebviewOp::Close | WebviewOp::Hide | WebviewOp::Show | WebviewOp::SetFocus => {
      log::warn!("servo runtime: webview visibility op is a no-op (single-view window)");
    }
  }
}

impl<T: UserEvent> tauri_runtime::WebviewDispatch<T> for ServoWebviewDispatcher<T> {
  type Runtime = crate::ServoRuntime<T>;

  fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> Result<()> {
    self
      .shared
      .proxy
      .send_event(crate::TaoMessage::Runtime(crate::RuntimeMessage::RunMainThread(
        std::sync::Arc::new(std::sync::Mutex::new(Some(Box::new(f)))),
      )))
      .map_err(|_| Error::FailedToSendMessage)
  }

  fn on_webview_event<F: Fn(&tauri_runtime::window::WebviewEvent) + Send + 'static>(
    &self,
    f: F,
  ) -> tauri_runtime::WebviewEventId {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = sync_channel(1);
    let _ = self.send(WebviewOp::OnEvent {
      handler: std::sync::Arc::new(std::sync::Mutex::new(Box::new(f))),
      tx,
    });
    let _ = rx.recv();
    id
  }

  fn with_webview<F: FnOnce(Box<dyn std::any::Any>) + Send + 'static>(&self, _f: F) -> Result<()> {
    Err(unsupported("platform webview access"))
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  fn open_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  fn close_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  fn is_devtools_open(&self) -> Result<bool> {
    Ok(false)
  }

  fn url(&self) -> Result<String> {
    self.roundtrip(WebviewOp::Url)
  }

  fn bounds(&self) -> Result<Rect> {
    Err(unsupported("webview bounds query"))
  }

  fn position(&self) -> Result<tauri_runtime::dpi::PhysicalPosition<i32>> {
    Err(unsupported("webview position query"))
  }

  fn size(&self) -> Result<tauri_runtime::dpi::PhysicalSize<u32>> {
    Err(unsupported("webview size query"))
  }

  fn navigate(&self, url: Url) -> Result<()> {
    self.send(WebviewOp::Navigate(url))
  }

  fn reload(&self) -> Result<()> {
    self.send(WebviewOp::Reload)
  }

  fn print(&self) -> Result<()> {
    Err(unsupported("webview printing"))
  }

  fn close(&self) -> Result<()> {
    self.send(WebviewOp::Close)
  }

  fn set_bounds(&self, _bounds: tauri_runtime::dpi::Rect) -> Result<()> {
    Err(unsupported("webview bounds"))
  }

  fn set_size(&self, _size: tauri_runtime::dpi::Size) -> Result<()> {
    Err(unsupported("webview size"))
  }

  fn set_position(&self, _position: tauri_runtime::dpi::Position) -> Result<()> {
    Err(unsupported("webview position"))
  }

  fn set_focus(&self) -> Result<()> {
    self.send(WebviewOp::SetFocus)
  }

  fn hide(&self) -> Result<()> {
    self.send(WebviewOp::Hide)
  }

  fn show(&self) -> Result<()> {
    self.send(WebviewOp::Show)
  }

  fn eval_script<S: Into<String>>(&self, script: S) -> Result<()> {
    self.send(WebviewOp::EvalScript {
      script: script.into(),
    })
  }

  fn eval_script_with_callback<S: Into<String>>(
    &self,
    script: S,
    callback: impl Fn(String) + Send + 'static,
  ) -> Result<()> {
    let (tx, rx) = sync_channel(1);
    self.send(WebviewOp::EvalScriptWithCallback {
      script: script.into(),
      tx,
    })?;
    // The callback must fire asynchronously: the completion arrives on the
    // main thread while this thread may hold no lock guarantees.
    std::thread::spawn(move || {
      let json = rx.recv().unwrap_or_else(|_| "null".to_string());
      callback(json);
    });
    Ok(())
  }

  fn reparent(&self, _window_id: WindowId) -> Result<()> {
    Err(unsupported("webview reparenting"))
  }

  fn cookies_for_url(&self, _url: Url) -> Result<Vec<tauri_runtime::Cookie<'static>>> {
    Err(unsupported("cookie store"))
  }

  fn cookies(&self) -> Result<Vec<tauri_runtime::Cookie<'static>>> {
    Err(unsupported("cookie store"))
  }

  fn set_cookie(&self, _cookie: tauri_runtime::Cookie<'_>) -> Result<()> {
    Err(unsupported("cookie store"))
  }

  fn delete_cookie(&self, _cookie: tauri_runtime::Cookie<'_>) -> Result<()> {
    Err(unsupported("cookie store"))
  }

  fn set_auto_resize(&self, _auto_resize: bool) -> Result<()> {
    Ok(())
  }

  fn set_zoom(&self, _scale_factor: f64) -> Result<()> {
    Err(unsupported("webview zoom"))
  }

  fn set_background_color(&self, _color: Option<tauri_utils::config::Color>) -> Result<()> {
    Err(unsupported("webview background color"))
  }

  fn clear_all_browsing_data(&self) -> Result<()> {
    Err(unsupported("browsing data clearing"))
  }
}

impl<T: UserEvent> ServoWebviewDispatcher<T> {
  /// Register an IPC command handler on this webview. Extension beyond the
  /// Tauri trait surface; the future Tauri-core integration calls this per
  /// `invoke_handler` entry.
  pub fn register_ipc_handler(
    &self,
    command: impl Into<String>,
    handler: crate::ipc::IpcHandler,
  ) -> Result<()> {
    self.send(WebviewOp::RegisterIpc {
      command: command.into(),
      handler,
    })
  }
}
