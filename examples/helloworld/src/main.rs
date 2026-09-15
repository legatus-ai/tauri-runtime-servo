use tauri_runtime::{EventLoopProxy, RunEvent, Runtime, RuntimeHandle, UserEvent, window::WindowBuilder};
use tauri_runtime_servo::ServoRuntime;

// The surfman GPU-selection symbols must link into the final binary.
surfman::declare_surfman!();

#[derive(Debug, Clone)]
struct AppEvent;

fn main() {
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
            tauri_utils::config::WebviewUrl::External("https://servo.org".parse().unwrap()),
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
        proxy.send_event(AppEvent).expect("user event");
    });

    runtime.run(|event| match event {
        RunEvent::Ready => println!("servo runtime: ready"),
        RunEvent::UserEvent(_) => println!("servo runtime: user event round-trip ok"),
        RunEvent::Exit => println!("servo runtime: exit"),
        _ => {}
    });
}
