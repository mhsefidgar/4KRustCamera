#pragma once
#include <windows.h>
#include <cstdint>

constexpr uint32_t VC_WIDTH = 1280;
constexpr uint32_t VC_HEIGHT = 720;
constexpr uint32_t VC_STRIDE = VC_WIDTH * 4;
constexpr uint32_t VC_BYTES = VC_STRIDE * VC_HEIGHT;
constexpr wchar_t VC_MAPPING_NAME[] = L"Global\\4KRustCameraFrameRing";
constexpr wchar_t VC_MUTEX_NAME[] = L"Global\\4KRustCameraFrameRingMutex";

struct FrameHeader {
    uint64_t sequence;
    uint64_t timestamp100ns;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t reserved;
};

struct FrameRing {
    FrameHeader header;
    uint8_t pixels[VC_BYTES];
};
