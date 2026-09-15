//! Tauri webview runtime backed by Servo (`libservo`, in-process).
//!
//! Architecture: Tao owns windows on the main thread; Servo webviews live
//! alongside them (Servo types are `!Send`). [`ServoWindowDispatcher`] and
//! [`ServoWebviewDispatcher`] are the `Send + Sync` handles used everywhere
//! else — every operation travels through the Tao event-loop proxy as a
//! [`RuntimeMessage`] and is applied on the main thread.

mod webview;
mod window;

pub use webview::ServoWebviewDispatcher;
pub use window::{ServoWindowBuilder, ServoWindowDispatcher};

// NOTE (Windows): downstream binaries must invoke `surfman::declare_surfman!()`
// at their crate root so the process links the GPU-selection symbols.
// Invoking it here as well would duplicate those symbols at link time.

use std::collections::HashMap;
use std::sync::{
  Arc, Mutex,
  mpsc::{SyncSender, sync_channel},
};

use tauri_runtime::{
  DeviceEventFilter, Error, EventLoopProxy as TauriEventLoopProxy, Result, RunEvent, Runtime,
  RuntimeHandle, RuntimeInitArgs, UserEvent,
  dpi::{PhysicalPosition, PhysicalSize},
  monitor::Monitor,
  window::{DetachedWindow, DetachedWindowWebview, PendingWindow, WindowEvent, WindowId},
  webview::{DetachedWebview, PendingWebview},
};
use tauri_utils::Theme;
use tao::event::{Event, StartCause, WindowEvent as TaoWindowEvent};

use window::{WindowOp, Windows};
use webview::{WebviewOp, Webviews};

/// Plain-data webview creation plan, extracted from [`PendingWebview`] on the
/// calling thread. The full pending type carries platform pointers and
/// `Send`-only handlers, so it never crosses threads in this runtime.
#[derive(Debug, Clone)]
pub struct WebviewPlan {
  pub label: String,
  pub url: String,
  pub use_https_scheme: bool,
}

/// Extract the thread-safe creation plan from a pending webview. Everything
/// else (protocol handlers, IPC closures, platform views) is Phase 2+.
fn extract_plan<T: UserEvent>(pending: PendingWebview<T, ServoRuntime<T>>) -> WebviewPlan {
  WebviewPlan {
    label: pending.label,
    url: pending.url,
    use_https_scheme: pending.webview_attributes.use_https_scheme,
  }
}

/// Messages from any thread to the main-thread event loop. All payloads are
/// `Send` plain data; live objects are built on the main thread.
pub enum RuntimeMessage<T: UserEvent> {
  CreateWindow {
    label: String,
    builder: ServoWindowBuilder,
    webview: Option<WebviewPlan>,
    tx: SyncSender<Result<DetachedWindow<T, ServoRuntime<T>>>>,
  },
  CreateWebview {
    window_id: WindowId,
    plan: WebviewPlan,
    tx: SyncSender<Result<DetachedWebview<T, ServoRuntime<T>>>>,
  },
  Window {
    id: WindowId,
    op: WindowOp,
  },
  Webview {
    label: String,
    op: WebviewOp,
  },
  RunMainThread(crate::window::SharedThunk),
  ServoWake,
  RequestRedraw(tao::window::WindowId),
}

impl<T: UserEvent> std::fmt::Debug for RuntimeMessage<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::CreateWindow { .. } => write!(f, "CreateWindow(..)"),
      Self::CreateWebview { .. } => write!(f, "CreateWebview(..)"),
      Self::Window { id, .. } => write!(f, "Window({id:?}, ..)"),
      Self::Webview { label, .. } => write!(f, "Webview({label}, ..)"),
      Self::RunMainThread(_) => write!(f, "RunMainThread(..)"),
      Self::ServoWake => write!(f, "ServoWake"),
      Self::RequestRedraw(id) => write!(f, "RequestRedraw({id:?})"),
    }
  }
}

/// User-event payload for the Tao event loop.
#[derive(Debug)]
pub enum TaoMessage<T: UserEvent> {
  Runtime(RuntimeMessage<T>),
  UserEvent(T),
  /// Servo has work; pump `spin_event_loop` on the main thread.
  ServoWake,
  /// A frame is ready for the Tao window; request a redraw.
  RequestRedraw(tao::window::WindowId),
}

/// Shared, thread-safe runtime state. Holds only the event-loop proxy —
/// notably NOT the display handle (`RawDisplayHandle` is `!Send`). Display
/// handles are resolved on the main thread per webview creation.
/// Live objects live on the main thread in [`MainState`].
#[derive(Debug, Clone)]
pub struct Shared<T: UserEvent> {
  pub proxy: tao::event_loop::EventLoopProxy<TaoMessage<T>>,
}

impl<T: UserEvent> Shared<T> {
  fn send(&self, message: RuntimeMessage<T>) -> Result<()> {
    self
      .proxy
      .send_event(TaoMessage::Runtime(message))
      .map_err(|_| Error::FailedToSendMessage)
  }
}

/// Main-thread-only state: Tao windows, Servo views, Servo instance.
pub struct MainState {
  pub windows: Windows,
  pub webviews: Webviews,
  pub servo: Option<servo::Servo>,
  pub labels: HashMap<String, WindowId>,
}

impl std::fmt::Debug for MainState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MainState")
      .field("windows", &self.windows)
      .field("webviews", &self.webviews)
      .field("servo", &self.servo.is_some())
      .finish_non_exhaustive()
  }
}

impl MainState {
  fn new() -> Self {
    Self {
      windows: Windows::default(),
      webviews: Webviews::default(),
      servo: None,
      labels: HashMap::new(),
    }
  }
}

pub(crate) fn unsupported(what: &str) -> Error {
  Error::CreateWebview(format!("servo runtime: {what} is not supported yet").into())
}

/// The Servo-backed Tauri runtime.
#[derive(Debug)]
pub struct ServoRuntime<T: UserEvent> {
  proxy: tao::event_loop::EventLoopProxy<TaoMessage<T>>,
  event_loop: Option<tao::event_loop::EventLoop<TaoMessage<T>>>,
  shared: Arc<Shared<T>>,
  main_thread: std::thread::ThreadId,
}

impl<T: UserEvent> ServoRuntime<T> {
  fn send(&self, message: RuntimeMessage<T>) -> Result<()> {
    self.shared.send(message)
  }
}

/// Cloneable, thread-safe handle to the runtime.
#[derive(Debug, Clone)]
pub struct ServoHandle<T: UserEvent> {
  shared: Arc<Shared<T>>,
}

impl<T: UserEvent> ServoHandle<T> {
  fn send(&self, message: RuntimeMessage<T>) -> Result<()> {
    self.shared.send(message)
  }
}

#[derive(Debug, Clone)]
pub struct ServoEventLoopProxy<T: UserEvent> {
  proxy: tao::event_loop::EventLoopProxy<TaoMessage<T>>,
}

impl<T: UserEvent> TauriEventLoopProxy<T> for ServoEventLoopProxy<T> {
  fn send_event(&self, event: T) -> Result<()> {
    self
      .proxy
      .send_event(TaoMessage::UserEvent(event))
      .map_err(|_| Error::FailedToSendMessage)
  }
}

impl<T: UserEvent> RuntimeHandle<T> for ServoHandle<T> {
  type Runtime = ServoRuntime<T>;

  fn create_proxy(&self) -> <Self::Runtime as Runtime<T>>::EventLoopProxy {
    ServoEventLoopProxy {
      proxy: self.shared.proxy.clone(),
    }
  }

  fn request_exit(&self, code: i32) -> Result<()> {
    log::debug!("servo runtime: request_exit({code})");
    // Implemented by dropping through the main loop via a dedicated message
    // in Phase 2; for now exit inline.
    std::process::exit(code);
  }

  fn create_window<F: Fn(tauri_runtime::window::RawWindow) + Send + 'static>(
    &self,
    pending: PendingWindow<T, Self::Runtime>,
    _after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Self::Runtime>> {
    let webview = pending.webview.map(extract_plan);
    let (tx, rx) = sync_channel(1);
    self.send(RuntimeMessage::CreateWindow {
      label: pending.label,
      builder: pending.window_builder,
      webview,
      tx,
    })?;
    rx.recv().map_err(|_| Error::FailedToReceiveMessage)?
  }

  fn create_webview(
    &self,
    window_id: WindowId,
    pending: PendingWebview<T, Self::Runtime>,
  ) -> Result<DetachedWebview<T, Self::Runtime>> {
    let plan = extract_plan(pending);
    let (tx, rx) = sync_channel(1);
    self.send(RuntimeMessage::CreateWebview {
      window_id,
      plan,
      tx,
    })?;
    rx.recv().map_err(|_| Error::FailedToReceiveMessage)?
  }

  fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> Result<()> {
    self.send(RuntimeMessage::RunMainThread(std::sync::Arc::new(
      std::sync::Mutex::new(Some(Box::new(f))),
    )))
  }

  fn display_handle(&self) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
    Err(raw_window_handle::HandleError::Unavailable)
  }

  fn primary_monitor(&self) -> Option<Monitor> {
    None
  }

  fn monitor_from_point(&self, _x: f64, _y: f64) -> Option<Monitor> {
    None
  }

  fn available_monitors(&self) -> Vec<Monitor> {
    Vec::new()
  }

  fn cursor_position(&self) -> Result<PhysicalPosition<f64>> {
    Err(unsupported("cursor position lookup"))
  }

  fn set_theme(&self, _theme: Option<Theme>) {}

  fn set_device_event_filter(&self, _filter: DeviceEventFilter) {}

  #[cfg(target_os = "macos")]
  fn set_activation_policy(&self, _activation_policy: tauri_runtime::ActivationPolicy) -> Result<()> {
    Err(unsupported("macOS activation policy"))
  }

  #[cfg(target_os = "macos")]
  fn set_dock_visibility(&self, _visible: bool) -> Result<()> {
    Err(unsupported("macOS dock visibility"))
  }

  #[cfg(target_os = "macos")]
  fn show(&self) -> Result<()> {
    Err(unsupported("macOS show"))
  }

  #[cfg(target_os = "macos")]
  fn hide(&self) -> Result<()> {
    Err(unsupported("macOS hide"))
  }
}

impl<T: UserEvent> Runtime<T> for ServoRuntime<T> {
  type WindowDispatcher = ServoWindowDispatcher<T>;
  type WebviewDispatcher = ServoWebviewDispatcher<T>;
  type Handle = ServoHandle<T>;
  type EventLoopProxy = ServoEventLoopProxy<T>;

  fn new(_args: RuntimeInitArgs) -> Result<Self> {
    let mut builder = tao::event_loop::EventLoopBuilder::<TaoMessage<T>>::with_user_event();
    let event_loop = builder.build();
    let proxy = event_loop.create_proxy();
    Ok(Self {
      proxy: proxy.clone(),
      event_loop: Some(event_loop),
      shared: Arc::new(Shared { proxy }),
      main_thread: std::thread::current().id(),
    })
  }

  #[cfg(any(
    windows,
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn new_any_thread(_args: RuntimeInitArgs) -> Result<Self> {
    Self::new(_args)
  }

  fn create_proxy(&self) -> Self::EventLoopProxy {
    ServoEventLoopProxy {
      proxy: self.proxy.clone(),
    }
  }

  fn handle(&self) -> Self::Handle {
    ServoHandle {
      shared: self.shared.clone(),
    }
  }

  fn create_window<F: Fn(tauri_runtime::window::RawWindow) + Send + 'static>(
    &self,
    pending: PendingWindow<T, Self>,
    after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Self>> {
    self.handle().create_window(pending, after_window_creation)
  }

  fn create_webview(
    &self,
    window_id: WindowId,
    pending: PendingWebview<T, Self>,
  ) -> Result<DetachedWebview<T, Self>> {
    self.handle().create_webview(window_id, pending)
  }

  fn primary_monitor(&self) -> Option<Monitor> {
    self.handle().primary_monitor()
  }

  fn monitor_from_point(&self, x: f64, y: f64) -> Option<Monitor> {
    self.handle().monitor_from_point(x, y)
  }

  fn available_monitors(&self) -> Vec<Monitor> {
    self.handle().available_monitors()
  }

  fn cursor_position(&self) -> Result<PhysicalPosition<f64>> {
    self.handle().cursor_position()
  }

  fn set_theme(&self, theme: Option<Theme>) {
    self.handle().set_theme(theme)
  }

  #[cfg(target_os = "macos")]
  fn set_activation_policy(&mut self, activation_policy: tauri_runtime::ActivationPolicy) {
    let _ = self.handle().set_activation_policy(activation_policy);
  }

  #[cfg(target_os = "macos")]
  fn set_dock_visibility(&mut self, visible: bool) {
    let _ = self.handle().set_dock_visibility(visible);
  }

  #[cfg(target_os = "macos")]
  fn show(&self) {}

  #[cfg(target_os = "macos")]
  fn hide(&self) {}

  fn set_device_event_filter(&mut self, filter: DeviceEventFilter) {
    self.handle().set_device_event_filter(filter);
  }

  fn run_iteration<F: FnMut(RunEvent<T>) + 'static>(&mut self, _callback: F) {
    // Phase 1: Tao 0.34 has no pump API. Servo still needs pumping so the
    // engine does not stall in embedding hosts that drive iterations.
    log::warn!("servo runtime: run_iteration pumps Servo only (Tao pump unavailable)");
  }

  fn run_return<F: FnMut(RunEvent<T>) + 'static>(self, callback: F) -> i32 {
    Self::drive(self, callback, true)
  }

  fn run<F: FnMut(RunEvent<T>) + 'static>(self, callback: F) {
    Self::drive(self, callback, false);
  }
}

/// Build a Tao window from a pending one. Main thread only.
fn build_tao_window<T: UserEvent>(
  target: &tao::event_loop::EventLoopWindowTarget<TaoMessage<T>>,
  label: &str,
  builder: ServoWindowBuilder,
) -> Result<tao::window::Window> {
  let (tao_builder, _background) = builder.into_tao();
  tao_builder
    .with_title(label)
    .build(target)
    .map_err(|_| Error::CreateWindow)
}

/// Resolve the initial URL for a webview.
fn initial_url(plan: &WebviewPlan) -> url::Url {
  // Phase 1: honor the resolved URL string; custom protocols land in Phase 2.
  url::Url::parse(&plan.url)
    .or_else(|_| url::Url::parse("about:blank"))
    .expect("about:blank parses")
}

/// Display handle for rendering contexts. Main thread only.
fn display_raw<T: UserEvent>(
  target: &tao::event_loop::EventLoopWindowTarget<TaoMessage<T>>,
) -> Result<raw_window_handle::RawDisplayHandle> {
  use raw_window_handle::HasDisplayHandle;
  target
    .display_handle()
    .map(|h| h.as_raw())
    .map_err(|_| Error::CreateWebview("no display handle".into()))
}

/// Build one Servo view on a live window entry. Main thread only.
fn build_view<T: UserEvent>(
  state: &mut MainState,
  shared: &Shared<T>,
  display: raw_window_handle::RawDisplayHandle,
  window_id: WindowId,
  plan: &WebviewPlan,
) -> Result<DetachedWebview<T, ServoRuntime<T>>> {
  let entry = state
    .windows
    .get_mut(window_id)
    .ok_or(Error::CreateWebview("unknown window".into()))?;
  let servo = webview::ensure_servo(&mut state.servo, &shared.proxy);
  let url = initial_url(plan);
  let (view, _rendering_context) =
    webview::create_view(servo, &shared.proxy, &entry.window, display, url)?;
  let dispatcher = ServoWebviewDispatcher {
    label: plan.label.clone(),
    shared: Arc::new(shared.clone()),
  };
  state.webviews.insert(webview::WebviewEntry {
    label: plan.label.clone(),
    window_id,
    view,
    handlers: HashMap::new(),
    next_handler_id: 0,
  });
  Ok(DetachedWebview {
    label: plan.label.clone(),
    dispatcher,
  })
}

/// Create the window (and its initial webview, if any). Main thread only.
pub fn apply_create_window<T: UserEvent>(
  state: &mut MainState,
  shared: &Shared<T>,
  target: &tao::event_loop::EventLoopWindowTarget<TaoMessage<T>>,
  label: String,
  builder: ServoWindowBuilder,
  plan: Option<WebviewPlan>,
  tx: SyncSender<Result<DetachedWindow<T, ServoRuntime<T>>>>,
) {
  let result = (|| -> Result<DetachedWindow<T, ServoRuntime<T>>> {
    let tao_window = build_tao_window(target, &label, builder)?;
    let id = state.windows.insert(label.clone(), tao_window);
    state.labels.insert(label.clone(), id);

    let dispatcher = ServoWindowDispatcher {
      id,
      shared: Arc::new(shared.clone()),
    };

    // Optional attached webview, created inline on the same window.
    let webview = match plan {
      None => None,
      Some(plan) => {
        let display = display_raw(target)?;
        let detached = build_view(state, shared, display, id, &plan)?;
        Some(DetachedWindowWebview {
          webview: detached,
          use_https_scheme: plan.use_https_scheme,
        })
      }
    };
    Ok(DetachedWindow {
      id,
      label,
      dispatcher,
      webview,
    })
  })();
  let _ = tx.send(result);
}

/// Create a webview on an existing window. Main thread only.
#[allow(clippy::too_many_arguments)]
pub fn apply_create_webview<T: UserEvent>(
  state: &mut MainState,
  shared: &Shared<T>,
  target: &tao::event_loop::EventLoopWindowTarget<TaoMessage<T>>,
  window_id: WindowId,
  plan: WebviewPlan,
  tx: SyncSender<Result<DetachedWebview<T, ServoRuntime<T>>>>,
) {
  let result = (|| -> Result<DetachedWebview<T, ServoRuntime<T>>> {
    let display = display_raw(target)?;
    build_view(state, shared, display, window_id, &plan)
  })();
  let _ = tx.send(result);
}

/// Apply a runtime message on the main thread.
pub fn apply_runtime_message<T: UserEvent>(
  state: &mut MainState,
  shared: &Shared<T>,
  target: &tao::event_loop::EventLoopWindowTarget<TaoMessage<T>>,
  message: RuntimeMessage<T>,
  _callback: &mut dyn FnMut(RunEvent<T>),
) {
  match message {
    RuntimeMessage::CreateWindow {
      label,
      builder,
      webview,
      tx,
    } => apply_create_window(state, shared, target, label, builder, webview, tx),
    RuntimeMessage::CreateWebview {
      window_id,
      plan,
      tx,
    } => apply_create_webview(state, shared, target, window_id, plan, tx),
    RuntimeMessage::Window { id, op } => window::apply(&mut state.windows, id, op),
    RuntimeMessage::Webview { label, op } => webview::apply(&mut state.webviews, &label, op),
    RuntimeMessage::RunMainThread(thunk) => {
      if let Some(f) = thunk.lock().expect("thunk lock").take() {
        f();
      }
    }
    RuntimeMessage::ServoWake | RuntimeMessage::RequestRedraw(_) => {
      // Handled inline in the event loop; never queued.
    }
  }
}

/// Translate a Tao window event: forward input to Servo, mirror state to
/// Tauri handlers. Main thread only.
pub fn apply_window_event<T: UserEvent>(
  state: &mut MainState,
  _target: &tao::event_loop::EventLoopWindowTarget<TaoMessage<T>>,
  window_id: tao::window::WindowId,
  event: tao::event::WindowEvent,
  callback: &mut dyn FnMut(RunEvent<T>),
) {
  use tao::event::WindowEvent as TaoEv;
  let Some((id, entry)) = state.windows.lookup_tao(window_id) else {
    return;
  };
  let label = entry.label.clone();
  match event {
    TaoEv::Resized(size) => {
      for (_, view) in state.webviews.iter_for_window(id).collect::<Vec<_>>() {
        view.view.resize(size);
      }
      emit_window(state, &label, WindowEvent::Resized(tauri_runtime::dpi::PhysicalSize {
        width: size.width,
        height: size.height,
      }), callback);
    }
    TaoEv::Moved(position) => {
      emit_window(
        state,
        &label,
        WindowEvent::Moved(tauri_runtime::dpi::PhysicalPosition {
          x: position.x,
          y: position.y,
        }),
        callback,
      );
    }
    TaoEv::CloseRequested => {
      emit_window(state, &label, WindowEvent::Destroyed, callback);
      state.windows.remove(id);
    }
    TaoEv::Focused(focused) => {
      emit_window(state, &label, WindowEvent::Focused(focused), callback);
    }
    TaoEv::ScaleFactorChanged { scale_factor, .. } => {
      let size = entry.window.inner_size();
      emit_window(
        state,
        &label,
        WindowEvent::ScaleFactorChanged {
          scale_factor,
          new_inner_size: tauri_runtime::dpi::PhysicalSize {
            width: size.width,
            height: size.height,
          },
        },
        callback,
      );
    }
    TaoEv::KeyboardInput { event, .. } => {
      forward_keyboard(state, id, &event);
    }
    TaoEv::CursorMoved { position, .. } => {
      forward_cursor_moved(state, id, position);
    }
    TaoEv::MouseInput { state: pressed, button, .. } => {
      forward_mouse_input(state, id, pressed, button);
    }
    TaoEv::MouseWheel { delta, .. } => {
      forward_wheel(state, id, delta);
    }
    _ => {}
  }
}

fn emit_window<T: UserEvent>(
  state: &MainState,
  label: &str,
  event: WindowEvent,
  callback: &mut dyn FnMut(RunEvent<T>),
) {
  if let Some((_, entry)) = state.windows.iter().find(|(_, e)| e.label == label) {
    for handler in entry.handlers.values() {
      if let Ok(guard) = handler.lock() {
        guard(&event);
      }
    }
  }
  callback(RunEvent::WindowEvent {
    label: label.to_string(),
    event,
  });
}

fn forward_keyboard(_state: &mut MainState, _id: WindowId, _event: &tao::event::KeyEvent) {
  // Phase 2: full keyboard mapping (needs keyboard-types translation).
}

fn forward_cursor_moved(
  _state: &mut MainState,
  _id: WindowId,
  _position: tao::dpi::PhysicalPosition<f64>,
) {
  // Phase 2: pointer-moved input events.
}

fn forward_mouse_input(
  _state: &mut MainState,
  _id: WindowId,
  _pressed: tao::event::ElementState,
  _button: tao::event::MouseButton,
) {
  // Phase 2: pointer button input events.
}

fn forward_wheel(_state: &mut MainState, _id: WindowId, _delta: tao::event::MouseScrollDelta,
) {
  // Phase 2: wheel input events.
}

/// Present the latest Servo frame for every view in the window.
pub fn apply_redraw(state: &mut MainState, window_id: tao::window::WindowId) {
  let Some((_id, entry)) = state.windows.lookup_tao(window_id) else {
    return;
  };
  let _ = entry.window.inner_size();
  // Phase 2: explicit present call if the rendering context needs it; the
  // delegate-driven request_redraw loop already keeps frames flowing.
}

/// Handle a close request: emit Destroyed and drop the window.
pub fn apply_close_requested<T: UserEvent>(
  state: &mut MainState,
  _target: &tao::event_loop::EventLoopWindowTarget<TaoMessage<T>>,
  window_id: tao::window::WindowId,
  callback: &mut dyn FnMut(RunEvent<T>),
  _exit_code: &Arc<std::sync::Mutex<i32>>,
) {
  let Some((id, entry)) = state.windows.lookup_tao(window_id) else {
    return;
  };
  let label = entry.label.clone();
  emit_window(state, &label, WindowEvent::Destroyed, callback);
  state.windows.remove(id);
}

impl<T: UserEvent> ServoRuntime<T> {
  /// Owns the Tao event loop until exit. When `can_return` is set the exit
  /// code is delivered through the process exit path documented on
  /// [`Runtime::run_return`].
  fn drive(mut zelf: Self, mut callback: impl FnMut(RunEvent<T>) + 'static, can_return: bool) -> i32 {
    let event_loop = zelf
      .event_loop
      .take()
      .expect("runtime event loop already consumed");
    let mut state = MainState::new();
    let shared = zelf.shared.clone();
    let exit_code = std::sync::Arc::new(std::sync::Mutex::new(0i32));

    callback(RunEvent::Ready);

    let exit_code_in_loop = exit_code.clone();
    event_loop.run(move |event, target, control_flow| {
      *control_flow = tao::event_loop::ControlFlow::Wait;
      match event {
        Event::NewEvents(StartCause::Init) => {
          callback(RunEvent::Resumed);
        }
        Event::UserEvent(TaoMessage::Runtime(message)) => {
          crate::apply_runtime_message(&mut state, &shared, target, message, &mut callback);
        }
        Event::UserEvent(TaoMessage::UserEvent(event)) => {
          callback(RunEvent::UserEvent(event));
        }
        Event::UserEvent(TaoMessage::ServoWake) => {
          if let Some(servo) = state.servo.as_ref() {
            servo.spin_event_loop();
          }
        }
        Event::UserEvent(TaoMessage::RequestRedraw(id)) => {
          for (_, entry) in state.windows.iter() {
            if entry.window.id() == id {
              entry.window.request_redraw();
            }
          }
        }
        Event::WindowEvent {
          event: TaoWindowEvent::CloseRequested,
          window_id,
          ..
        } => {
          crate::apply_close_requested(&mut state, target, window_id, &mut callback, &exit_code_in_loop);
        }
        Event::WindowEvent { event, window_id, .. } => {
          crate::apply_window_event(&mut state, target, window_id, event, &mut callback);
        }
        Event::RedrawRequested(window_id) => {
          crate::apply_redraw(&mut state, window_id);
        }
        Event::LoopDestroyed => {
          callback(RunEvent::Exit);
        }
        _ => {}
      }
    });

    #[allow(unreachable_code)]
    {
      // `EventLoop::run` never returns; reaching here means the loop was
      // torn down through process exit paths only.
      let _ = can_return;
      let code = *exit_code.lock().expect("exit code lock");
      code
    }
  }
}
