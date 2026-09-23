# 4K Rust Camera

A Windows x64 realtime camera application built around the native Windows camera backend through nokhwa.

## Current realtime improvements

- Low-latency frame queue: the UI drains pending frames and processes only the newest frame, avoiding growing capture latency.
- Parallel CPU image processing for exposure, highlight recovery, shadow lift, contrast, saturation, warmth, denoise and sharpening.
- Adjustable tuning controls with a one-click reset.
- Baseline comparison: the left pane is the decoded camera frame before this application's enhancement.
- Realtime FPS plus capture/decode and enhancement timing.
- Visible camera error reporting instead of silently printing failures.
- Portable Windows x64 packaging through GitHub Actions.

## Important performance design

The realtime path deliberately does **not** run super-resolution on every 4K frame. A neural x2 super-resolution model at full 4K would add substantial memory bandwidth and inference latency and would work against the low-latency goal.

The intended AI architecture is:

1. Realtime CPU/GPU-friendly enhancement on every frame.
2. Optional lightweight ONNX super-resolution on selected small tiles or stills.
3. AI inference must be explicitly enabled and measured before being advertised as realtime.

This repository currently packages the realtime classical enhancement path. It does **not** claim that ONNX inference is active until an ONNX Runtime backend is merged and benchmarked.

## Baseline comparison

"Baseline" means the decoded camera frame received by this application. It is not a capture of the Microsoft Camera app and does not reproduce Microsoft's private processing stack.

## Build

Local Windows build:

```powershell
cargo build --release
```

GitHub Actions builds `x86_64-pc-windows-msvc` on Windows Server 2022.

Pushes to `main` produce a downloadable CI artifact. A tag such as `v0.2.0` produces a GitHub Release containing the Windows x64 ZIP.

Windows 10/11 x64. Camera permissions must be enabled in Windows Settings.

## AI model research

The lightweight SwinIR/Swin2SR ONNX family was identified on Hugging Face for optional tile/snapshot enhancement. The exact model file and license should be pinned in the release manifest before enabling inference.

## License

MIT.
