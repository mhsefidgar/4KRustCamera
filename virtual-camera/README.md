# 4K Rust Camera Virtual Camera

Windows 11 virtual cameras are Media Foundation custom media-source components. This directory contains the native component boundary used by the Rust application.

The Rust process publishes processed frames through a local shared-memory ring. The Media Foundation source reads the newest frame and exposes RGB32 to Frame Server. The installer registers the COM media-source DLL and the Rust UI creates/removes the virtual-camera device.

This component targets Windows 11 22H2+ and requires the Windows SDK 10.0.22000.0 or newer.
