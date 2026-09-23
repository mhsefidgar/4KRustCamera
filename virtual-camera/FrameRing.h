#pragma once
#include <windows.h>
#include <cstdint>

constexpr uint32_t VC_WIDTH = 1280;
constexpr uint32_t VC_HEIGHT = 720;
constexpr uint32_t VC_STRIDE = VC_WIDTH * 4;
constexpr uint32_t VC_BYTES = VC_STRIDE * VC_HEIGHT;
constexpr wchar_t VC_MAPPING_NAME[] = L"Local\\4KRustCameraFrameRing";
constexpr wchar_t VC_MUTEX_NAME[] = L"Local\\4KRustCameraFrameRingMutex";

#pragma pack(push, 1)
struct FrameHeader {
    uint64_t sequence;       // even = stable, odd = writer is updating
    uint64_t timestamp100ns; // QPC-correlated Media Foundation time
    uint32_t width;
    uint32_t height;
    uint32_t stride;
};
#pragma pack(pop)

struct FrameRing {
    FrameHeader header;
    uint8_t pixels[VC_BYTES];
};
