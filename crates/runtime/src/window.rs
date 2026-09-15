//! Tao-backed window management for the Servo runtime.
//!
//! Live [`tao::window::Window`] objects are owned by the main thread inside
//! [`Windows`]. [`ServoWindowDispatcher`] is the `Send + Sync` handle used
//! everywhere else; every operation travels through the event-loop proxy as
//! a [`WindowOp`] and is applied on the main thread.

use std::collections::HashMap;
use std::sync::{
  Arc, Mutex,
  mpsc::{SyncSender, sync_channel},
};

use tauri_runtime::{
  DeviceEventFilter, Error, Icon, Result, UserEvent, WindowEventId,
  dpi::{PhysicalPosition, PhysicalSize, Position, Size},
  monitor::Monitor,
  window::{
    CursorIcon, DetachedWindow, PendingWindow, RawWindow, WindowEvent, WindowId,
    WindowSizeConstraints,
  },
};
use tauri_utils::{Theme, config::Color};

/// Shareable window-event handler. The `Mutex` supplies `Sync`; invocation
/// locks briefly on the main thread.
pub type SharedWindowHandler = Arc<Mutex<Box<dyn Fn(&WindowEvent) + Send>>>;
/// Shareable one-shot main-thread closure.
pub type SharedThunk = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

use crate::{RuntimeMessage, Shared, unsupported};

/// State for one live window. Main thread only.
pub struct WindowEntry {
  pub label: String,
  pub window: tao::window::Window,
  pub handlers: HashMap<WindowEventId, SharedWindowHandler>,
  pub next_handler_id: WindowEventId,
}

impl std::fmt::Debug for WindowEntry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WindowEntry")
      .field("label", &self.label)
      .finish_non_exhaustive()
  }
}

/// All live windows, keyed by Tauri [`WindowId`]. Main thread only.
#[derive(Debug, Default)]
pub struct Windows {
  inner: HashMap<WindowId, WindowEntry>,
  by_tao_id: HashMap<tao::window::WindowId, WindowId>,
  next_id: u32,
}

impl Windows {
  pub fn insert(&mut self, label: String, window: tao::window::Window) -> WindowId {
    let id = WindowId::from(self.next_id);
    self.next_id += 1;
    self.by_tao_id.insert(window.id(), id);
    self.inner.insert(
      id,
      WindowEntry {
        label,
        window,
        handlers: HashMap::new(),
        next_handler_id: 0,
      },
    );
    id
  }

  pub fn get(&self, id: WindowId) -> Option<&WindowEntry> {
    self.inner.get(&id)
  }

  pub fn get_mut(&mut self, id: WindowId) -> Option<&mut WindowEntry> {
    self.inner.get_mut(&id)
  }

  pub fn remove(&mut self, id: WindowId) -> Option<WindowEntry> {
    let entry = self.inner.remove(&id)?;
    self.by_tao_id.retain(|_, v| *v != id);
    Some(entry)
  }

  pub fn lookup_tao(&self, id: tao::window::WindowId) -> Option<(WindowId, &WindowEntry)> {
    let ours = *self.by_tao_id.get(&id)?;
    Some((ours, &self.inner[&ours]))
  }

  pub fn iter(&self) -> impl Iterator<Item = (WindowId, &WindowEntry)> {
    self.inner.iter().map(|(id, entry)| (*id, entry))
  }
}

/// A window operation applied on the main thread. `tx` carries the result
/// back for getters; setters use `Op::Fire`-style variants without one.
pub enum WindowOp {
  ScaleFactor(SyncSender<Result<f64>>),
  InnerPosition(SyncSender<Result<PhysicalPosition<i32>>>),
  OuterPosition(SyncSender<Result<PhysicalPosition<i32>>>),
  InnerSize(SyncSender<Result<PhysicalSize<u32>>>),
  OuterSize(SyncSender<Result<PhysicalSize<u32>>>),
  IsFullscreen(SyncSender<Result<bool>>),
  IsMinimized(SyncSender<Result<bool>>),
  IsMaximized(SyncSender<Result<bool>>),
  IsFocused(SyncSender<Result<bool>>),
  IsDecorated(SyncSender<Result<bool>>),
  IsResizable(SyncSender<Result<bool>>),
  IsVisible(SyncSender<Result<bool>>),
  Title(SyncSender<Result<String>>),
  CurrentMonitor(SyncSender<Result<Option<Monitor>>>),
  PrimaryMonitor(SyncSender<Result<Option<Monitor>>>),
  OnEvent {
    handler: SharedWindowHandler,
    tx: SyncSender<WindowEventId>,
  },
  SetTitle(String),
  SetVisible(bool),
  SetFocus,
  SetResizable(bool),
  SetMinimized(bool),
  SetMaximized(bool),
  SetFullscreen(bool),
  SetDecorations(bool),
  SetAlwaysOnTop(bool),
  SetSize(Size),
  SetMinSize(Option<Size>),
  SetMaxSize(Option<Size>),
  SetPosition(Position),
  SetIcon { rgba: Vec<u8>, width: u32, height: u32 },
  SetSkipTaskbar(bool),
  SetCursorIcon(CursorIcon),
  SetCursorVisible(bool),
  SetCursorGrab(bool),
  SetIgnoreCursorEvents(bool),
  SetContentProtected(bool),
  SetEnabled(bool),
  SetFullscreenFallback,
  Close,
  Run(SharedThunk),
}

/// `Send + Sync` handle to a main-thread Tao window.
#[derive(Debug, Clone)]
pub struct ServoWindowDispatcher<T: UserEvent> {
  pub id: WindowId,
  pub shared: Arc<Shared<T>>,
}

impl<T: UserEvent> ServoWindowDispatcher<T> {
  fn send(&self, op: WindowOp) -> Result<()> {
    self
      .shared
      .proxy
      .send_event(crate::TaoMessage::Runtime(RuntimeMessage::Window {
        id: self.id,
        op,
      }))
      .map_err(|_| Error::FailedToSendMessage)
  }

  fn roundtrip<R: Send + 'static>(
    &self,
    make: impl FnOnce(SyncSender<Result<R>>) -> WindowOp,
  ) -> Result<R> {
    let (tx, rx) = sync_channel(1);
    self.send(make(tx))?;
    rx.recv().map_err(|_| Error::FailedToReceiveMessage)?
  }

  fn set(&self, op: WindowOp) -> Result<()> {
    self.send(op)
  }
}

fn tao_monitor(m: &tao::monitor::MonitorHandle) -> Monitor {
  let size = m.size();
  let position = m.position();
  Monitor {
    name: m.name().map(|n| n.to_string()),
    size: PhysicalSize {
      width: size.width,
      height: size.height,
    },
    position: PhysicalPosition {
      x: position.x,
      y: position.y,
    },
    work_area: tauri_runtime::dpi::PhysicalRect {
      position,
      size,
    },
    scale_factor: m.scale_factor(),
  }
}

fn tao_cursor_icon(icon: CursorIcon) -> tao::window::CursorIcon {
  match icon {
    CursorIcon::Default => tao::window::CursorIcon::Default,
    CursorIcon::Crosshair => tao::window::CursorIcon::Crosshair,
    CursorIcon::Hand => tao::window::CursorIcon::Hand,
    CursorIcon::Arrow => tao::window::CursorIcon::Arrow,
    CursorIcon::Move => tao::window::CursorIcon::Move,
    CursorIcon::Text => tao::window::CursorIcon::Text,
    CursorIcon::Wait => tao::window::CursorIcon::Wait,
    CursorIcon::Help => tao::window::CursorIcon::Help,
    CursorIcon::Progress => tao::window::CursorIcon::Progress,
    CursorIcon::NotAllowed => tao::window::CursorIcon::NotAllowed,
    CursorIcon::ContextMenu => tao::window::CursorIcon::ContextMenu,
    CursorIcon::Cell => tao::window::CursorIcon::Cell,
    CursorIcon::VerticalText => tao::window::CursorIcon::VerticalText,
    CursorIcon::Alias => tao::window::CursorIcon::Alias,
    CursorIcon::Copy => tao::window::CursorIcon::Copy,
    CursorIcon::NoDrop => tao::window::CursorIcon::NoDrop,
    CursorIcon::Grab => tao::window::CursorIcon::Grab,
    CursorIcon::Grabbing => tao::window::CursorIcon::Grabbing,
    CursorIcon::AllScroll => tao::window::CursorIcon::AllScroll,
    CursorIcon::ZoomIn => tao::window::CursorIcon::ZoomIn,
    CursorIcon::ZoomOut => tao::window::CursorIcon::ZoomOut,
    CursorIcon::EResize => tao::window::CursorIcon::EResize,
    CursorIcon::NResize => tao::window::CursorIcon::NResize,
    CursorIcon::NeResize => tao::window::CursorIcon::NeResize,
    CursorIcon::NwResize => tao::window::CursorIcon::NwResize,
    CursorIcon::SResize => tao::window::CursorIcon::SResize,
    CursorIcon::SeResize => tao::window::CursorIcon::SeResize,
    CursorIcon::SwResize => tao::window::CursorIcon::SwResize,
    CursorIcon::WResize => tao::window::CursorIcon::WResize,
    CursorIcon::EwResize => tao::window::CursorIcon::EwResize,
    CursorIcon::NsResize => tao::window::CursorIcon::NsResize,
    CursorIcon::NeswResize => tao::window::CursorIcon::NeswResize,
    CursorIcon::NwseResize => tao::window::CursorIcon::NwseResize,
    CursorIcon::ColResize => tao::window::CursorIcon::ColResize,
    CursorIcon::RowResize => tao::window::CursorIcon::RowResize,
    _ => tao::window::CursorIcon::Default,
  }
}

/// Apply a [`WindowOp`] against live state on the main thread.
pub fn apply(windows: &mut Windows, id: WindowId, op: WindowOp) {
  let Some(entry) = windows.get_mut(id) else {
    log::warn!("servo runtime: window op for unknown window");
    return;
  };
  if let WindowOp::OnEvent { handler, tx } = op {
    let handler_id = entry.next_handler_id;
    entry.next_handler_id += 1;
    entry.handlers.insert(handler_id, handler);
    let _ = tx.send(handler_id);
    return;
  }
  let w = &entry.window;
  match op {
    WindowOp::ScaleFactor(tx) => {
      let _ = tx.send(Ok(w.scale_factor()));
    }
    WindowOp::InnerPosition(tx) => {
      let _ = tx.send(
        w.inner_position()
          .map(|p| PhysicalPosition { x: p.x, y: p.y })
          .map_err(|_| Error::FailedToGetMonitor),
      );
    }
    WindowOp::OuterPosition(tx) => {
      let _ = tx.send(
        w.outer_position()
          .map(|p| PhysicalPosition { x: p.x, y: p.y })
          .map_err(|_| Error::FailedToGetMonitor),
      );
    }
    WindowOp::InnerSize(tx) => {
      let _ = tx.send(Ok(PhysicalSize {
        width: w.inner_size().width,
        height: w.inner_size().height,
      }));
    }
    WindowOp::OuterSize(tx) => {
      let _ = tx.send(Ok(PhysicalSize {
        width: w.outer_size().width,
        height: w.outer_size().height,
      }));
    }
    WindowOp::IsFullscreen(tx) => {
      let _ = tx.send(Ok(w.fullscreen().is_some()));
    }
    WindowOp::IsMinimized(tx) => {
      let _ = tx.send(Ok(w.is_minimized()));
    }
    WindowOp::IsMaximized(tx) => {
      let _ = tx.send(Ok(w.is_maximized()));
    }
    WindowOp::IsFocused(tx) => {
      let _ = tx.send(Ok(w.is_focused()));
    }
    WindowOp::IsDecorated(tx) => {
      let _ = tx.send(Ok(w.is_decorated()));
    }
    WindowOp::IsResizable(tx) => {
      let _ = tx.send(Ok(w.is_resizable()));
    }
    WindowOp::IsVisible(tx) => {
      let _ = tx.send(Ok(w.is_visible()));
    }
    WindowOp::Title(tx) => {
      let _ = tx.send(Ok(w.title()));
    }
    WindowOp::CurrentMonitor(tx) => {
      let _ = tx.send(Ok(w.current_monitor().map(|m| tao_monitor(&m))));
    }
    WindowOp::PrimaryMonitor(tx) => {
      let _ = tx.send(Ok(w.current_monitor().map(|m| tao_monitor(&m))));
    }
    WindowOp::OnEvent { .. } => unreachable!("handled above"),
    WindowOp::SetTitle(title) => w.set_title(&title),
    WindowOp::SetVisible(visible) => w.set_visible(visible),
    WindowOp::SetFocus => w.set_focus(),
    WindowOp::SetResizable(resizable) => w.set_resizable(resizable),
    WindowOp::SetMinimized(minimized) => w.set_minimized(minimized),
    WindowOp::SetMaximized(maximized) => w.set_maximized(maximized),
    WindowOp::SetFullscreen(fullscreen) => w.set_fullscreen(if fullscreen {
      Some(tao::window::Fullscreen::Borderless(None))
    } else {
      None
    }),
    WindowOp::SetDecorations(decorations) => w.set_decorations(decorations),
    WindowOp::SetAlwaysOnTop(always_on_top) => w.set_always_on_top(always_on_top),
    WindowOp::SetSize(size) => w.set_inner_size(size),
    WindowOp::SetMinSize(size) => {
      if let Some(size) = size {
        w.set_min_inner_size(Some(size))
      } else {
        w.set_min_inner_size::<tao::dpi::PhysicalSize<u32>>(None)
      }
    }
    WindowOp::SetMaxSize(size) => {
      if let Some(size) = size {
        w.set_max_inner_size(Some(size))
      } else {
        w.set_max_inner_size::<tao::dpi::PhysicalSize<u32>>(None)
      }
    }
    WindowOp::SetPosition(position) => w.set_outer_position(position),
    WindowOp::SetIcon { rgba, width, height } => {
      match tao::window::Icon::from_rgba(rgba, width, height) {
        Ok(tao_icon) => w.set_window_icon(Some(tao_icon)),
        Err(error) => log::warn!("servo runtime: invalid window icon: {error}"),
      }
    }
    WindowOp::SetSkipTaskbar(_skip) => {
      log::warn!("servo runtime: skip-taskbar is not available on Tao windows");
    }
    WindowOp::SetCursorIcon(icon) => w.set_cursor_icon(tao_cursor_icon(icon)),
    WindowOp::SetCursorVisible(visible) => w.set_cursor_visible(visible),
    WindowOp::SetCursorGrab(grab) => {
      if let Err(error) = w.set_cursor_grab(grab) {
        log::warn!("servo runtime: cursor grab failed: {error}");
      }
    }
    WindowOp::SetIgnoreCursorEvents(ignore) => {
      if let Err(error) = w.set_ignore_cursor_events(ignore) {
        log::warn!("servo runtime: ignore cursor events failed: {error}");
      }
    }
    WindowOp::SetContentProtected(_protected) => {
      log::warn!("servo runtime: content protection is not available on Tao windows");
    }
    WindowOp::SetEnabled(_enabled) => {
      log::warn!("servo runtime: enabled flag is not available on Tao windows");
    }
    WindowOp::SetFullscreenFallback => w.set_fullscreen(Some(tao::window::Fullscreen::Borderless(None))),
    WindowOp::Close => {
      log::debug!("servo runtime: window close requested via dispatcher");
    }
    WindowOp::Run(thunk) => {
      if let Some(f) = thunk.lock().expect("thunk lock").take() {
        f();
      }
    }
  }
}

impl<T: UserEvent> tauri_runtime::WindowDispatch<T> for ServoWindowDispatcher<T> {
  type Runtime = crate::ServoRuntime<T>;
  type WindowBuilder = ServoWindowBuilder;

  fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> Result<()> {
    self.set(WindowOp::Run(Arc::new(Mutex::new(Some(Box::new(f))))))
  }

  fn on_window_event<F: Fn(&WindowEvent) + Send + 'static>(&self, f: F) -> WindowEventId {
    // Registration needs &mut entry state; route through the loop and return
    // the id from a local counter. Ids are unique per dispatcher.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = sync_channel(1);
    let _ = self.send(WindowOp::OnEvent {
      handler: Arc::new(Mutex::new(Box::new(f))),
      tx,
    });
    let _ = rx.recv();
    id
  }

  fn scale_factor(&self) -> Result<f64> {
    self.roundtrip(WindowOp::ScaleFactor)
  }

  fn inner_position(&self) -> Result<PhysicalPosition<i32>> {
    self.roundtrip(WindowOp::InnerPosition)
  }

  fn outer_position(&self) -> Result<PhysicalPosition<i32>> {
    self.roundtrip(WindowOp::OuterPosition)
  }

  fn inner_size(&self) -> Result<PhysicalSize<u32>> {
    self.roundtrip(WindowOp::InnerSize)
  }

  fn outer_size(&self) -> Result<PhysicalSize<u32>> {
    self.roundtrip(WindowOp::OuterSize)
  }

  fn is_fullscreen(&self) -> Result<bool> {
    self.roundtrip(WindowOp::IsFullscreen)
  }

  fn is_minimized(&self) -> Result<bool> {
    self.roundtrip(WindowOp::IsMinimized)
  }

  fn is_maximized(&self) -> Result<bool> {
    self.roundtrip(WindowOp::IsMaximized)
  }

  fn is_focused(&self) -> Result<bool> {
    self.roundtrip(WindowOp::IsFocused)
  }

  fn is_decorated(&self) -> Result<bool> {
    self.roundtrip(WindowOp::IsDecorated)
  }

  fn is_resizable(&self) -> Result<bool> {
    self.roundtrip(WindowOp::IsResizable)
  }

  fn is_maximizable(&self) -> Result<bool> {
    Err(crate::unsupported("maximizable state"))
  }

  fn is_minimizable(&self) -> Result<bool> {
    Err(crate::unsupported("minimizable state"))
  }

  fn is_closable(&self) -> Result<bool> {
    Err(crate::unsupported("closable state"))
  }

  fn is_visible(&self) -> Result<bool> {
    self.roundtrip(WindowOp::IsVisible)
  }

  fn is_enabled(&self) -> Result<bool> {
    Err(crate::unsupported("enabled state"))
  }

  fn is_always_on_top(&self) -> Result<bool> {
    Err(crate::unsupported("always-on-top state"))
  }

  fn title(&self) -> Result<String> {
    self.roundtrip(WindowOp::Title)
  }

  fn current_monitor(&self) -> Result<Option<Monitor>> {
    self.roundtrip(WindowOp::CurrentMonitor)
  }

  fn monitor_from_point(&self, _x: f64, _y: f64) -> Result<Option<Monitor>> {
    Err(crate::unsupported("monitor from point"))
  }

  fn primary_monitor(&self) -> Result<Option<Monitor>> {
    self.roundtrip(WindowOp::PrimaryMonitor)
  }

  fn available_monitors(&self) -> Result<Vec<Monitor>> {
    Err(crate::unsupported("monitor enumeration on dispatcher"))
  }

  fn window_handle(
    &self,
  ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
    Err(raw_window_handle::HandleError::Unavailable)
  }

  fn theme(&self) -> Result<Theme> {
    Err(crate::unsupported("window theme query"))
  }

  fn center(&self) -> Result<()> {
    Err(crate::unsupported("window centering"))
  }

  fn request_user_attention(&self, _request_type: Option<tauri_runtime::UserAttentionType>) -> Result<()> {
    Err(crate::unsupported("user attention request"))
  }

  fn create_window<F: Fn(tauri_runtime::window::RawWindow) + Send + 'static>(
    &mut self,
    _pending: PendingWindow<T, Self::Runtime>,
    _after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Self::Runtime>> {
    Err(crate::unsupported("nested window creation"))
  }

  fn create_webview(
    &mut self,
    _pending: PendingWebview<T, Self::Runtime>,
  ) -> Result<DetachedWebview<T, Self::Runtime>> {
    Err(crate::unsupported("nested webview creation"))
  }

  fn set_resizable(&self, resizable: bool) -> Result<()> {
    self.set(WindowOp::SetResizable(resizable))
  }

  fn set_enabled(&self, enabled: bool) -> Result<()> {
    self.set(WindowOp::SetEnabled(enabled))
  }

  fn set_maximizable(&self, _maximizable: bool) -> Result<()> {
    Err(crate::unsupported("maximizable flag"))
  }

  fn set_minimizable(&self, _minimizable: bool) -> Result<()> {
    Err(crate::unsupported("minimizable flag"))
  }

  fn set_closable(&self, _closable: bool) -> Result<()> {
    Err(crate::unsupported("closable flag"))
  }

  fn set_title<S: Into<String>>(&self, title: S) -> Result<()> {
    self.set(WindowOp::SetTitle(title.into()))
  }

  fn maximize(&self) -> Result<()> {
    self.set(WindowOp::SetMaximized(true))
  }

  fn unmaximize(&self) -> Result<()> {
    self.set(WindowOp::SetMaximized(false))
  }

  fn minimize(&self) -> Result<()> {
    self.set(WindowOp::SetMinimized(true))
  }

  fn unminimize(&self) -> Result<()> {
    self.set(WindowOp::SetMinimized(false))
  }

  fn show(&self) -> Result<()> {
    self.set(WindowOp::SetVisible(true))
  }

  fn hide(&self) -> Result<()> {
    self.set(WindowOp::SetVisible(false))
  }

  fn close(&self) -> Result<()> {
    self.set(WindowOp::Close)
  }

  fn destroy(&self) -> Result<()> {
    self.set(WindowOp::Close)
  }

  fn set_decorations(&self, decorations: bool) -> Result<()> {
    self.set(WindowOp::SetDecorations(decorations))
  }

  fn set_shadow(&self, _enable: bool) -> Result<()> {
    Err(crate::unsupported("window shadow"))
  }

  fn set_always_on_bottom(&self, _always_on_bottom: bool) -> Result<()> {
    Err(crate::unsupported("always-on-bottom"))
  }

  fn set_always_on_top(&self, always_on_top: bool) -> Result<()> {
    self.set(WindowOp::SetAlwaysOnTop(always_on_top))
  }

  fn set_visible_on_all_workspaces(&self, _visible: bool) -> Result<()> {
    Err(crate::unsupported("visible-on-all-workspaces"))
  }

  fn set_background_color(&self, _color: Option<Color>) -> Result<()> {
    Err(crate::unsupported("window background color"))
  }

  fn set_content_protected(&self, protected: bool) -> Result<()> {
    self.set(WindowOp::SetContentProtected(protected))
  }

  fn set_size(&self, size: Size) -> Result<()> {
    self.set(WindowOp::SetSize(size))
  }

  fn set_min_size(&self, size: Option<Size>) -> Result<()> {
    self.set(WindowOp::SetMinSize(size))
  }

  fn set_max_size(&self, size: Option<Size>) -> Result<()> {
    self.set(WindowOp::SetMaxSize(size))
  }

  fn set_size_constraints(&self, _constraints: tauri_runtime::window::WindowSizeConstraints) -> Result<()> {
    Err(crate::unsupported("size constraints"))
  }

  fn set_position(&self, position: Position) -> Result<()> {
    self.set(WindowOp::SetPosition(position))
  }

  fn set_fullscreen(&self, fullscreen: bool) -> Result<()> {
    self.set(WindowOp::SetFullscreen(fullscreen))
  }

  #[cfg(target_os = "macos")]
  fn set_simple_fullscreen(&self, _enable: bool) -> Result<()> {
    Err(crate::unsupported("macOS simple fullscreen"))
  }

  fn set_focus(&self) -> Result<()> {
    self.set(WindowOp::SetFocus)
  }

  fn set_focusable(&self, _focusable: bool) -> Result<()> {
    Err(crate::unsupported("focusable flag"))
  }

  fn set_icon(&self, icon: Icon) -> Result<()> {
    self.set(WindowOp::SetIcon {
      rgba: icon.rgba.into_owned(),
      width: icon.width,
      height: icon.height,
    })
  }

  fn set_skip_taskbar(&self, skip: bool) -> Result<()> {
    self.set(WindowOp::SetSkipTaskbar(skip))
  }

  fn set_cursor_grab(&self, grab: bool) -> Result<()> {
    self.set(WindowOp::SetCursorGrab(grab))
  }

  fn set_cursor_visible(&self, visible: bool) -> Result<()> {
    self.set(WindowOp::SetCursorVisible(visible))
  }

  fn set_cursor_icon(&self, icon: CursorIcon) -> Result<()> {
    self.set(WindowOp::SetCursorIcon(icon))
  }

  fn set_cursor_position<Pos: Into<Position>>(&self, _position: Pos) -> Result<()> {
    Err(crate::unsupported("cursor positioning"))
  }

  fn set_ignore_cursor_events(&self, ignore: bool) -> Result<()> {
    self.set(WindowOp::SetIgnoreCursorEvents(ignore))
  }

  fn start_dragging(&self) -> Result<()> {
    Err(crate::unsupported("window dragging"))
  }

  fn start_resize_dragging(&self, _direction: tauri_runtime::ResizeDirection) -> Result<()> {
    Err(crate::unsupported("resize dragging"))
  }

  fn set_badge_count(&self, _count: Option<i64>, _desktop_filename: Option<String>) -> Result<()> {
    Err(crate::unsupported("badge count"))
  }

  fn set_badge_label(&self, _label: Option<String>) -> Result<()> {
    Err(crate::unsupported("badge label"))
  }

  fn set_overlay_icon(&self, _icon: Option<Icon>) -> Result<()> {
    Err(crate::unsupported("overlay icon"))
  }

  fn set_progress_bar(&self, _progress_state: tauri_runtime::ProgressBarState) -> Result<()> {
    Err(crate::unsupported("progress bar"))
  }

  fn set_title_bar_style(&self, _style: tauri_utils::TitleBarStyle) -> Result<()> {
    Err(crate::unsupported("title bar style"))
  }

  fn set_traffic_light_position(&self, _position: Position) -> Result<()> {
    Err(crate::unsupported("traffic light position"))
  }

  fn set_theme(&self, _theme: Option<Theme>) -> Result<()> {
    Err(crate::unsupported("window theme"))
  }
}

use tauri_runtime::{
  ResizeDirection, UserAttentionType,
  webview::{DetachedWebview, PendingWebview},
};

/// Builder translating Tauri window attributes to Tao. Plain data only (the
/// Tao builder holds raw pointers and is `!Send`), materialized on the main
/// thread at creation time.
#[derive(Debug, Clone)]
pub struct ServoWindowBuilder {
  title: String,
  position: Option<(f64, f64)>,
  inner_size: Option<(f64, f64)>,
  min_inner_size: Option<(f64, f64)>,
  max_inner_size: Option<(f64, f64)>,
  resizable: bool,
  maximizable: bool,
  minimizable: bool,
  closable: bool,
  fullscreen: bool,
  focused: bool,
  focusable: bool,
  maximized: bool,
  visible: bool,
  transparent: bool,
  decorations: bool,
  always_on_bottom: bool,
  always_on_top: bool,
  visible_on_all_workspaces: bool,
  content_protected: bool,
  skip_taskbar: bool,
  background_color: Option<Color>,
  shadow: bool,
  owner: Option<isize>,
  parent: Option<isize>,
  drag_and_drop: bool,
  theme: Option<Theme>,
  window_classname: Option<String>,
  icon_rgba: Option<(Vec<u8>, u32, u32)>,
  center: bool,
}

impl Default for ServoWindowBuilder {
  fn default() -> Self {
    Self {
      title: String::from("Servo"),
      position: None,
      inner_size: None,
      min_inner_size: None,
      max_inner_size: None,
      resizable: true,
      maximizable: true,
      minimizable: true,
      closable: true,
      fullscreen: false,
      focused: true,
      focusable: true,
      maximized: false,
      visible: true,
      transparent: false,
      decorations: true,
      always_on_bottom: false,
      always_on_top: false,
      visible_on_all_workspaces: false,
      content_protected: false,
      skip_taskbar: false,
      background_color: None,
      shadow: true,
      owner: None,
      parent: None,
      drag_and_drop: true,
      theme: None,
      window_classname: None,
      icon_rgba: None,
      center: false,
    }
  }
}

impl ServoWindowBuilder {
  pub fn into_tao(self) -> (tao::window::WindowBuilder, Option<Color>) {
    let mut builder = tao::window::WindowBuilder::new()
      .with_title(self.title)
      .with_resizable(self.resizable)
      .with_maximizable(self.maximizable)
      .with_minimizable(self.minimizable)
      .with_closable(self.closable)
      .with_fullscreen(if self.fullscreen {
        Some(tao::window::Fullscreen::Borderless(None))
      } else {
        None
      })
      .with_focused(self.focused)
      .with_focusable(self.focusable)
      .with_maximized(self.maximized)
      .with_visible(self.visible)
      .with_transparent(self.transparent)
      .with_decorations(self.decorations)
      .with_always_on_bottom(self.always_on_bottom)
      .with_always_on_top(self.always_on_top)
      .with_visible_on_all_workspaces(self.visible_on_all_workspaces)
      .with_content_protection(self.content_protected);
    if let Some((x, y)) = self.position {
      builder = builder.with_position(tao::dpi::LogicalPosition::new(x, y));
    }
    if let Some((w, h)) = self.inner_size {
      builder = builder.with_inner_size(tao::dpi::LogicalSize::new(w, h));
    }
    if let Some((w, h)) = self.min_inner_size {
      builder = builder.with_min_inner_size(tao::dpi::LogicalSize::new(w, h));
    }
    if let Some((w, h)) = self.max_inner_size {
      builder = builder.with_max_inner_size(tao::dpi::LogicalSize::new(w, h));
    }
    if self.window_classname.is_some() {
      log::warn!("servo runtime: window classname is not supported on this Tao version");
    }
    #[cfg(windows)]
    if self.owner.is_some() || self.parent.is_some() {
      log::warn!("servo runtime: owner/parent windows are not supported on this Tao version");
    }
    if let Some((rgba, width, height)) = self.icon_rgba {
      match tao::window::Icon::from_rgba(rgba, width, height) {
        Ok(icon) => {
          builder = builder.with_window_icon(Some(icon));
        }
        Err(error) => log::warn!("servo runtime: invalid builder icon: {error}"),
      }
    }
    if self.theme.is_some() {
      log::warn!("servo runtime: builder theme is not available on Tao windows");
    }
    (builder, self.background_color)
  }
}

impl tauri_runtime::window::WindowBuilderBase for ServoWindowBuilder {}

impl tauri_runtime::window::WindowBuilder for ServoWindowBuilder {
  fn new() -> Self {
    Self::default()
  }

  fn with_config(_config: &tauri_utils::config::WindowConfig) -> Self {
    // Phase 1: config-file windows use defaults; structured mapping lands
    // with the app-integration pass.
    Self::default()
  }

  fn center(mut self) -> Self {
    self.center = true;
    self
  }

  fn position(mut self, x: f64, y: f64) -> Self {
    self.position = Some((x, y));
    self
  }

  fn inner_size(mut self, width: f64, height: f64) -> Self {
    self.inner_size = Some((width, height));
    self
  }

  fn min_inner_size(mut self, min_width: f64, min_height: f64) -> Self {
    self.min_inner_size = Some((min_width, min_height));
    self
  }

  fn max_inner_size(mut self, max_width: f64, max_height: f64) -> Self {
    self.max_inner_size = Some((max_width, max_height));
    self
  }

  fn inner_size_constraints(self, _constraints: WindowSizeConstraints) -> Self {
    // Phase 1: PixelUnit mapping lands with the app-integration pass.
    self
  }

  fn prevent_overflow(self) -> Self {
    self
  }

  fn prevent_overflow_with_margin(self, _margin: tauri_runtime::dpi::Size) -> Self {
    self
  }

  fn resizable(mut self, resizable: bool) -> Self {
    self.resizable = resizable;
    self
  }

  fn maximizable(mut self, maximizable: bool) -> Self {
    self.maximizable = maximizable;
    self
  }

  fn minimizable(mut self, minimizable: bool) -> Self {
    self.minimizable = minimizable;
    self
  }

  fn closable(mut self, closable: bool) -> Self {
    self.closable = closable;
    self
  }

  fn title<S: Into<String>>(mut self, title: S) -> Self {
    self.title = title.into();
    self
  }

  fn fullscreen(mut self, fullscreen: bool) -> Self {
    self.fullscreen = fullscreen;
    self
  }

  fn focused(mut self, focused: bool) -> Self {
    self.focused = focused;
    self
  }

  fn focusable(mut self, focusable: bool) -> Self {
    self.focusable = focusable;
    self
  }

  fn maximized(mut self, maximized: bool) -> Self {
    self.maximized = maximized;
    self
  }

  fn visible(mut self, visible: bool) -> Self {
    self.visible = visible;
    self
  }

  #[cfg(any(not(target_os = "macos"), feature = "macos-private-api"))]
  fn transparent(mut self, transparent: bool) -> Self {
    self.transparent = transparent;
    self
  }

  fn decorations(mut self, decorations: bool) -> Self {
    self.decorations = decorations;
    self
  }

  fn always_on_bottom(mut self, always_on_bottom: bool) -> Self {
    self.always_on_bottom = always_on_bottom;
    self
  }

  fn always_on_top(mut self, always_on_top: bool) -> Self {
    self.always_on_top = always_on_top;
    self
  }

  fn visible_on_all_workspaces(mut self, visible_on_all_workspaces: bool) -> Self {
    self.visible_on_all_workspaces = visible_on_all_workspaces;
    self
  }

  fn content_protected(mut self, protected: bool) -> Self {
    self.content_protected = protected;
    self
  }

  fn icon(mut self, icon: Icon) -> tauri_runtime::Result<Self> {
    self.icon_rgba = Some((icon.rgba.into_owned(), icon.width, icon.height));
    Ok(self)
  }

  fn skip_taskbar(mut self, skip: bool) -> Self {
    self.skip_taskbar = skip;
    self
  }

  fn background_color(mut self, color: Color) -> Self {
    self.background_color = Some(color);
    self
  }

  fn shadow(mut self, enable: bool) -> Self {
    self.shadow = enable;
    self
  }

  #[cfg(windows)]
  fn owner(mut self, owner: windows::Win32::Foundation::HWND) -> Self {
    self.owner = Some(owner.0 as isize);
    self
  }

  #[cfg(windows)]
  fn parent(mut self, parent: windows::Win32::Foundation::HWND) -> Self {
    self.parent = Some(parent.0 as isize);
    self
  }

  #[cfg(target_os = "macos")]
  fn parent(self, _parent: *mut std::ffi::c_void) -> Self {
    self
  }

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn transient_for(self, _parent: &impl gtk::glib::IsA<gtk::Window>) -> Self {
    self
  }

  #[cfg(windows)]
  fn drag_and_drop(mut self, enabled: bool) -> Self {
    self.drag_and_drop = enabled;
    self
  }

  #[cfg(target_os = "macos")]
  fn title_bar_style(self, _style: tauri_utils::TitleBarStyle) -> Self {
    self
  }

  #[cfg(target_os = "macos")]
  fn traffic_light_position<P: Into<tauri_runtime::dpi::Position>>(self, _position: P) -> Self {
    self
  }

  #[cfg(target_os = "macos")]
  fn hidden_title(self, _hidden: bool) -> Self {
    self
  }

  #[cfg(target_os = "macos")]
  fn tabbing_identifier(self, _identifier: &str) -> Self {
    self
  }

  fn theme(mut self, theme: Option<Theme>) -> Self {
    self.theme = theme;
    self
  }

  fn has_icon(&self) -> bool {
    self.icon_rgba.is_some()
  }

  fn get_theme(&self) -> Option<Theme> {
    self.theme
  }

  fn window_classname<S: Into<String>>(mut self, window_classname: S) -> Self {
    self.window_classname = Some(window_classname.into());
    self
  }
}
