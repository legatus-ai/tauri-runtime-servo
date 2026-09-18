//! Engine probe: evaluate one JS snippet in a Servo page and print the JSON result.
//!
//! Usage: probe <url> "<js expression>"
//! The snippet runs against the page after load; whatever it returns is
//! serialized and printed on one line (`probe: <json>`).
//! Exit code 0 always (a JS-side failure is data, not a crash).

use tauri_runtime::{
  EventLoopProxy, RunEvent, Runtime, RuntimeHandle, UserEvent, WebviewDispatch, window::WindowBuilder,
};

#[derive(Debug, Clone)]
struct AppEvent;

surfman::declare_surfman!();

fn main() {
  let url = std::env::args()
    .nth(1)
    .unwrap_or_else(|| "about:blank".to_string());
  // A script arg starting with `@` loads the script from that file path
  // (argv cannot carry multiline JS reliably).
  fn load_arg(raw: Option<String>) -> String {
    match raw {
      Some(path) if path.starts_with('@') => std::fs::read_to_string(&path[1..])
        .unwrap_or_else(|error| panic!("probe: cannot read script file: {error}")),
      Some(inline) => inline,
      None => "location.href".to_string(),
    }
  }
  let args: Vec<String> = std::env::args().collect();
  let script = load_arg(args.get(2).cloned());
  let poll = args
    .get(3)
    .map(|raw| load_arg(Some(raw.clone())))
    .unwrap_or_default();

  let runtime = ServoRuntime::<AppEvent>::new(Default::default()).expect("runtime");
  let handle = runtime.handle();
  let proxy = runtime.create_proxy();

  std::thread::spawn(move || {
    let builder = tauri_runtime_servo::ServoWindowBuilder::new().title("servo probe");
    let mut pending =
      tauri_runtime::window::PendingWindow::new(builder, "main").expect("pending window");
    let attributes = tauri_runtime::webview::WebviewAttributes::new(
      tauri_utils::config::WebviewUrl::External(url.parse().expect("page url")),
    );
    let view =
      tauri_runtime::webview::PendingWebview::new(attributes, "main-view").expect("pending webview");
    pending.set_webview(view);
    let detached = handle
      .create_window(pending, None::<fn(tauri_runtime::window::RawWindow)>)
      .expect("create window");

    if let Some(attached) = detached.webview {
      std::thread::sleep(std::time::Duration::from_secs(6));
      let script = if poll.is_empty() {
        script
      } else {
        // Async protocol: run the setup script now, read window.__result
        // after a delay with the poll script.
        attached
          .webview
          .dispatcher
          .eval_script(script)
          .expect("setup eval");
        std::thread::sleep(std::time::Duration::from_secs(8));
        poll
      };
      attached
        .webview
        .dispatcher
        .eval_script_with_callback(script, |json| {
          println!("probe: {json}");
        })
        .expect("eval");
      std::thread::sleep(std::time::Duration::from_secs(4));
    }
    proxy.send_event(AppEvent).expect("user event");
    std::thread::sleep(std::time::Duration::from_secs(2));
    std::process::exit(0);
  });

  runtime.run(|_| {});
}

use tauri_runtime_servo::ServoRuntime;
