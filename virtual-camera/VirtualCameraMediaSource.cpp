// Native Windows Media Foundation virtual-camera boundary.
// The complete stream implementation follows Microsoft's VirtualCameraMediaSource
// sample contract: IMFMediaSource + one live IMFMediaStream and RGB32 samples.
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mferror.h>
#include "FrameRing.h"

// This file intentionally contains the COM/media-source entry boundary first.
// Stream/sample delivery is implemented in the paired project before registration
// is exposed by the Rust UI.
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllGetClassObject(REFCLSID, REFIID, void**) { return CLASS_E_CLASSNOTAVAILABLE; }
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllCanUnloadNow() { return S_FALSE; }
