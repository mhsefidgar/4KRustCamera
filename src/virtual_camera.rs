use anyhow::{Context, Result};
use image::RgbImage;
use std::{mem::size_of, ptr, slice};
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::{
    Foundation::{CloseHandle, FreeLibrary, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Memory::{CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS, FILE_MAP_ALL_ACCESS, PAGE_READWRITE},
        Performance::QueryPerformanceCounter,
        Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    },
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const STRIDE: u32 = WIDTH * 4;
const BYTES: usize = (STRIDE * HEIGHT) as usize;
const MAPPING: &[u16] = &['L' as u16, 'o' as u16, 'c' as u16, 'a' as u16, 'l' as u16, '\\' as u16, '4' as u16, 'K' as u16, 'R' as u16, 'u' as u16, 's' as u16, 't' as u16, 'C' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'r' as u16, 'a' as u16, 'F' as u16, 'r' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'R' as u16, 'i' as u16, 'n' as u16, 'g' as u16, 0];
const MUTEX: &[u16] = &['L' as u16, 'o' as u16, 'c' as u16, 'a' as u16, 'l' as u16, '\\' as u16, '4' as u16, 'K' as u16, 'R' as u16, 'u' as u16, 's' as u16, 't' as u16, 'C' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'r' as u16, 'a' as u16, 'F' as u16, 'r' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'R' as u16, 'i' as u16, 'n' as u16, 'g' as u16, 'M' as u16, 'u' as u16, 't' as u16, 'e' as u16, 'x' as u16, 0];

#[repr(C)]
struct FrameHeader {
    sequence: u64,
    timestamp100ns: u64,
    width: u32,
    height: u32,
    stride: u32,
    reserved: u32,
}

#[repr(C)]
struct FrameRing {
    header: FrameHeader,
    pixels: [u8; BYTES],
}

pub struct VirtualCameraPublisher {
    mapping: HANDLE,
    mutex: HANDLE,
    ring: *mut FrameRing,
    sequence: u64,
}
unsafe impl Send for VirtualCameraPublisher {}
unsafe impl Sync for VirtualCameraPublisher {}

impl VirtualCameraPublisher {
    pub fn open() -> Result<Self> {
        let bytes = size_of::<FrameRing>() as u32;
        let mapping = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, ptr::null(), PAGE_READWRITE, 0, bytes, MAPPING.as_ptr())
        };
        if mapping.is_null() {
            anyhow::bail!("CreateFileMappingW failed: {}", unsafe { GetLastError() });
        }
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize) };\n        let ring = view.Value as *mut FrameRing;
        if ring.is_null() {
            unsafe { CloseHandle(mapping); }
            anyhow::bail!("MapViewOfFile failed: {}", unsafe { GetLastError() });
        }
        let mutex = unsafe { CreateMutexW(ptr::null(), 0, MUTEX.as_ptr()) };
        if mutex.is_null() {
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ring as *mut _ }); CloseHandle(mapping); }
            anyhow::bail!("CreateMutexW failed: {}", unsafe { GetLastError() });
        }
        Ok(Self { mapping, mutex, ring, sequence: 0 })
    }

    pub fn publish(&mut self, rgb: &RgbImage) -> Result<()> {
        let frame = image::imageops::resize(rgb, WIDTH, HEIGHT, image::imageops::FilterType::Triangle);
        let wait = unsafe { WaitForSingleObject(self.mutex, 100) };
        if wait != WAIT_OBJECT_0 { anyhow::bail!("virtual-camera frame mutex timeout"); }

        self.sequence = self.sequence.wrapping_add(2).max(2);
        unsafe {
            (*self.ring).header.sequence = self.sequence | 1;
            (*self.ring).header.width = WIDTH;
            (*self.ring).header.height = HEIGHT;
            (*self.ring).header.stride = STRIDE;
            (*self.ring).header.reserved = 0;
            let mut qpc = 0i64;
            QueryPerformanceCounter(&mut qpc);
            (*self.ring).header.timestamp100ns = qpc.max(0) as u64;
            let dst = slice::from_raw_parts_mut((*self.ring).pixels.as_mut_ptr(), BYTES);
            for y in 0..HEIGHT as usize {
                let src = &frame.as_raw()[(y * WIDTH as usize * 3)..((y + 1) * WIDTH as usize * 3)];
                let row = &mut dst[(y * STRIDE as usize)..((y + 1) * STRIDE as usize)];
                for x in 0..WIDTH as usize {
                    row[x * 4] = src[x * 3 + 2];
                    row[x * 4 + 1] = src[x * 3 + 1];
                    row[x * 4 + 2] = src[x * 3];
                    row[x * 4 + 3] = 255;
                }
            }
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            (*self.ring).header.sequence = self.sequence;
        }
        unsafe { ReleaseMutex(self.mutex); }
        Ok(())
    }
}

impl Drop for VirtualCameraPublisher {
    fn drop(&mut self) {
        unsafe {
            if !self.ring.is_null() { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ring as *mut _ }); }
            if !self.mutex.is_null() { CloseHandle(self.mutex); }
            if !self.mapping.is_null() { CloseHandle(self.mapping); }
        }
    }
}

pub fn call_registration(register: bool) -> Result<()> {
    let mut path = std::env::current_exe().context("current executable path")?;
    path.set_file_name("4KRustCameraVirtualCamera.dll");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let module = LoadLibraryW(wide.as_ptr());
        if module.is_null() { anyhow::bail!("LoadLibraryW failed: {}", GetLastError()); }
        let name: &[u8] = if register { b"Register4KRustCamera\0" } else { b"Unregister4KRustCamera\0" };
        let proc = GetProcAddress(module, name.as_ptr());
        if proc.is_none() { FreeLibrary(module); anyhow::bail!("virtual-camera registration export is missing"); }
        type Fn = unsafe extern "system" fn() -> i32;
        let f: Fn = std::mem::transmute(proc);
        let hr = f();
        FreeLibrary(module);
        if hr < 0 { anyhow::bail!("virtual-camera registration failed: HRESULT 0x{:08X}", hr as u32); }
    }
    Ok(())
}
