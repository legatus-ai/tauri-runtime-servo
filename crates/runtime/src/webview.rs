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

/// Requests a Tao redraw whenever Servo produces a frame. Goes through the
/// event-loop proxy so the delegate never touches windowing objects.
#[derive(Debug, Clone)]
struct FrameDelegate<T: UserEvent> {
  proxy: tao::event_loop::EventLoopProxy<crate::TaoMessage<T>>,
  window_id: tao::window::WindowId,
}

impl<T: UserEvent> servo::WebViewDelegate for FrameDelegate<T> {
  fn notify_new_frame_ready(&self, _: WebView) {
    let _ = self.proxy.send_event(crate::TaoMessage::RequestRedraw(self.window_id));
  }
}

/// Build the shared Servo instance on first use. Must run on the main thread.
pub fn ensure_servo<'a, T: UserEvent>(
  servo: &'a mut Option<Servo>,
  proxy: &tao::event_loop::EventLoopProxy<crate::TaoMessage<T>>,
) -> &'a Servo {
  if servo.is_none() {
    let instance = ServoBuilder::default()
      .event_loop_waker(Box::new(ServoWaker {
        proxy: proxy.clone(),
      }))
      .build();
    instance.setup_logging();
    *servo = Some(instance);
  }
  servo.as_ref().expect("servo just created")
}

/// Create a Servo webview filling `window`. Must run on the main thread.
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
  let delegate = Rc::new(FrameDelegate::<T> {
    proxy: proxy.clone(),
    window_id: window.id(),
  });
  let view = WebViewBuilder::new(servo, rendering_context.clone())
    .url(url)
    .hidpi_scale_factor(euclid::Scale::new(window.scale_factor() as f32))
    .delegate(delegate)
    .build();
  Ok((view, rendering_context))
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
    _callback: impl Fn(String) + Send + 'static,
  ) -> Result<()> {
    // Phase 2: needs the script-completion plumbing from Servo's evaluate
    // callback. Fire-and-forget for now.
    self.send(WebviewOp::EvalScript {
      script: script.into(),
    })
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
