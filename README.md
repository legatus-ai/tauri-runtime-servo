# tauri-runtime-servo

Legatus-owned Tauri webview runtime backed by the **Servo engine** (in-process
`libservo`). Our own browser rendering engine integration that we control —
no dependency on archived embedding shims.

## Why this exists

- `versotile-org/verso` (the browser) is **archived** (Oct 2025, read-only).
- `tauri-runtime-verso` (external `versoview` process model) is stalled on
  Servo churn and carries the limitations of a sidecar architecture.
- Tauri keeps the `Runtime` trait open for custom runtimes. We implement it
  directly against `libservo`, in-process: one engine, every OS, no sidecar.

## Status

Phase 0 — scaffold. The `Runtime` trait surface (~130 methods across
`Runtime`, `RuntimeHandle`, `WindowDispatch`, `WebviewDispatch`) is being
implemented against `tauri-runtime 2.11.x`.

## Roadmap

- **Phase 1** — compilable `Runtime` stub: Tao window + blank Servo view.
- **Phase 2** — navigation + IPC invoke bridge (Tauri commands round-trip).
- **Phase 3** — app-compat gauntlet against the real desktop UI
  (contenteditable composer, Tailwind CSS coverage, SSE streaming,
  persistence, file dialogs).
- **Phase 4** — owned Servo patch series where the engine gaps, conformance
  CI, upstream contributions back to Servo.

## Layout

- `crates/runtime` — the `tauri_runtime::Runtime` implementation.
- `examples/helloworld` — minimal app proving window + render + IPC.
