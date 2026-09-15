//! Tauri webview runtime backed by Servo (`libservo`, in-process).
//!
//! Phase 0: crate skeleton. Phase 1 fills in the `Runtime` trait
//! implementation (Tao window + Servo webview).

/// Marker for the Servo-backed runtime once implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServoRuntime;

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // Phase 1 replaces this with runtime-construction tests.
    }
}
