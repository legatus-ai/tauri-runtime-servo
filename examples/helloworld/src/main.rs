use tauri_runtime::{
  EventLoopProxy, RunEvent, Runtime, RuntimeHandle, UserEvent, WebviewDispatch, window::WindowBuilder,
};
use tauri_runtime_servo::ServoRuntime;

#[derive(Debug, Clone)]
struct AppEvent;

const PAGE: &str = r#"<!doctype html>
<html><body>
<h1 id="title">legatus IPC proof</h1>
<div id="out">waiting…</div>
<script>
window.addEventListener("DOMContentLoaded", function () {
  if (!window.__legatusInvoke) {
    document.getElementById("out").textContent = "NO BRIDGE";
    return;
  }
  window.__legatusInvoke("greet", { name: "servo" }).then(function (reply) {
    document.getElementById("out").textContent = "greet:" + reply.greeting;
  }).catch(function (error) {
    document.getElementById("out").textContent = "ERROR:" + error;
  });
});
</script>
</body></html>"#;

fn main() {
    let page = std::env::temp_dir().join("legatus-ipc-proof.html");
    std::fs::write(&page, PAGE).expect("page");
    let url = format!("file:///{}", page.to_string_lossy().replace('\\', "/"));

    let runtime = ServoRuntime::<AppEvent>::new(Default::default()).expect("runtime");
    let handle = runtime.handle();
    let proxy = runtime.create_proxy();

    // Create the window (with an attached Servo webview) from a worker thread
    // to prove cross-thread dispatch. Servo initializes on first webview.
    std::thread::spawn(move || {
        let builder = tauri_runtime_servo::ServoWindowBuilder::new().title("servo hello");
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
        println!(
            "servo runtime: window created, webview attached: {}",
            detached.webview.is_some()
        );

        // Register the IPC command, then prove Rust→JS eval with a JSON round-trip.
        if let Some(attached) = detached.webview {
            attached
                .webview
                .dispatcher
                .register_ipc_handler("greet", std::sync::Arc::new(|args| {
                    let name = args
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("stranger");
                    serde_json::json!({ "greeting": format!("hello, {name} (from rust)") })
                }))
                .expect("register greet");
            println!("servo runtime: greet handler registered");
            for probe in [
                "location.href",
                "document.readyState",
                "document.getElementById('out') && document.getElementById('out').textContent",
            ] {
                std::thread::sleep(std::time::Duration::from_secs(4));
                attached
                    .webview
                    .dispatcher
                    .eval_script_with_callback(probe, |json| {
                        println!("servo runtime: eval saw page state: {json}")
                    })
                    .expect("eval");
            }
            std::thread::sleep(std::time::Duration::from_secs(4));
        }
        proxy.send_event(AppEvent).expect("user event");
    });

    runtime.run(|event| match event {
        RunEvent::Ready => println!("servo runtime: ready"),
        RunEvent::UserEvent(_) => println!("servo runtime: user event round-trip ok"),
        RunEvent::Exit => println!("servo runtime: exit"),
        _ => {}
    });
}
