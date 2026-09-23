#pragma once
#include <windows.h>
#include <cstdint>

// Fixed-size 720p transport keeps the virtual-camera component bounded. The Rust
// app downsamples its latest processed frame before publishing. This is deliberate:
// Frame Server clients commonly negotiate 720p/1080p even when the source is 4K.
constexpr uint32_t VC_WIDTH = 1280;
constexpr uint32_t VC_HEIGHT = 720;
constexpr uint32_t VC_STRIDE = VC_WIDTH * 4;
constexpr uint32_t VC_BYTES = VC_STRIDE * VC_HEIGHT;
constexpr wchar_t VC_MAPPING_NAME[] = L"Local\\4KRustCameraFrameRing";

struct FrameHeader { uint64_t sequence; uint64_t timestamp100ns; uint32_t width; uint32_t height; uint32_t stride; };
struct FrameRing { FrameHeader header; uint8_t pixels[VC_BYTES]; };
