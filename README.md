# 4K Rust Camera

A Windows x64 realtime camera application built around Windows Media Foundation through nokhwa, with a low-latency enhancement pipeline and a side-by-side baseline comparison.

## Improvements

- Realtime capture on the native Windows Media Foundation backend.
- Parallel image pipeline: exposure, tone mapping, contrast, saturation, warmth, shadow lift, highlight recovery, denoise and edge-aware sharpening.
- User-adjustable tuning parameters.
- Bounded frame queue so processing cannot accumulate unbounded latency.
- Baseline-vs-enhanced comparison mode.
- FPS and per-frame capture/enhancement timing.
- Optional lightweight AI model package for offline detail experiments without putting a large neural network into the realtime 4K path.

## Lightweight model

The release package includes models/swin2SR-lightweight-x2-64.onnx, sourced from the Hugging Face ONNX model collection. The model is intentionally not executed on every 4K frame: its small tile-oriented super-resolution architecture is better suited to optional detail/snapshot work than full-resolution realtime processing.

The release uses the lightweight SwinIR/Swin2SR family listed by Hugging Face.

## Baseline comparison

The left pane is the decoded camera frame before this application's enhancement. This is a reproducible camera-pipeline baseline; it is not a claim that it reproduces the Microsoft Camera app's private processing stack.

## Build

Run cargo build --release for a local build. GitHub Actions builds x86_64-pc-windows-msvc and packages a portable ZIP.

Windows 10/11 x64. Camera permission must be enabled in Windows Settings.

## License

MIT.
