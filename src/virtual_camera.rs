use anyhow::{Context, Result};
use image::RgbImage;
use rayon::prelude::*;
use std::{mem::size_of, ptr, slice, path::PathBuf};
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::{
    Foundation::{CloseHandle, FreeLibrary, GetLastError, LocalFree, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH},
        Memory::{CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS, FILE_MAP_ALL_ACCESS, PAGE_READWRITE},
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject, GetExitCodeProcess},
    },
    Security::{SECURITY_ATTRIBUTES, Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1}},
    UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
};

const WIDTH: u32 = 3840;
const HEIGHT: u32 = 2160;
const STRIDE: u32 = WIDTH * 4;
const BYTES: usize = (STRIDE * HEIGHT) as usize;
const MAPPING: &[u16] = &['G' as u16, 'l' as u16, 'o' as u16, 'b' as u16, 'a' as u16, 'l' as u16, '\\' as u16, '4' as u16, 'K' as u16, 'R' as u16, 'u' as u16, 's' as u16, 't' as u16, 'C' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'r' as u16, 'a' as u16, 'F' as u16, 'r' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'R' as u16, 'i' as u16, 'n' as u16, 'g' as u16, 0];
const MUTEX: &[u16] = &['G' as u16, 'l' as u16, 'o' as u16, 'b' as u16, 'a' as u16, 'l' as u16, '\\' as u16, '4' as u16, 'K' as u16, 'R' as u16, 'u' as u16, 's' as u16, 't' as u16, 'C' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'r' as u16, 'a' as u16, 'F' as u16, 'r' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'R' as u16, 'i' as u16, 'n' as u16, 'g' as u16, 'M' as u16, 'u' as u16, 't' as u16, 'e' as u16, 'x' as u16, 0];

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
    qpc_frequency: i64,
}
unsafe impl Send for VirtualCameraPublisher {}
unsafe impl Sync for VirtualCameraPublisher {}

impl VirtualCameraPublisher {
    pub fn open() -> Result<Self> {
        let bytes = size_of::<FrameRing>() as u32;
        let mut sd = ptr::null_mut();
        let sddl: Vec<u16> = "D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;IU)".encode_utf16().chain(std::iter::once(0)).collect();
        let sd_ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, ptr::null_mut()) != 0 };
        if !sd_ok { anyhow::bail!("could not create virtual-camera shared-memory security descriptor: {}", unsafe { GetLastError() }); }
        let mut sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd as *mut _, bInheritHandle: 0 };
        let mapping = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, &mut sa, PAGE_READWRITE, 0, bytes, MAPPING.as_ptr()) };
        unsafe { if !sd.is_null() { LocalFree(sd as _); } }
        if mapping.is_null() {
            anyhow::bail!("CreateFileMappingW failed: {}", unsafe { GetLastError() });
        }
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize) };
        let ring = view.Value as *mut FrameRing;
        if ring.is_null() {
            unsafe { CloseHandle(mapping); }
            anyhow::bail!("MapViewOfFile failed: {}", unsafe { GetLastError() });
        }
        let mutex = unsafe { CreateMutexW(ptr::null(), 0, MUTEX.as_ptr()) };
        if mutex.is_null() {
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ring as *mut _ }); CloseHandle(mapping); }
            anyhow::bail!("CreateMutexW failed: {}", unsafe { GetLastError() });
        }
        let mut qpc_frequency = 0i64;
        unsafe { QueryPerformanceFrequency(&mut qpc_frequency); }
        if qpc_frequency <= 0 {
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ring as *mut _ }); CloseHandle(mutex); CloseHandle(mapping); }
            anyhow::bail!("QueryPerformanceFrequency failed");
        }
        Ok(Self { mapping, mutex, ring, sequence: 0, qpc_frequency })
    }

    pub fn publish(&mut self, rgb: &RgbImage) -> Result<()> {
        let frame = if rgb.width() == WIDTH && rgb.height() == HEIGHT { rgb.clone() } else { image::imageops::resize(rgb, WIDTH, HEIGHT, image::imageops::FilterType::Triangle) };
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
            (*self.ring).header.timestamp100ns = ((qpc.max(0) as i128) * 10_000_000i128 / self.qpc_frequency as i128) as u64;
            let dst = slice::from_raw_parts_mut((*self.ring).pixels.as_mut_ptr(), BYTES);
            let src = frame.as_raw();
            dst.par_chunks_mut(STRIDE as usize).enumerate().for_each(|(y, row)| {
                let src_row = &src[(y * WIDTH as usize * 3)..((y + 1) * WIDTH as usize * 3)];
                for x in 0..WIDTH as usize {
                    row[x * 4] = src_row[x * 3 + 2];
                    row[x * 4 + 1] = src_row[x * 3 + 1];
                    row[x * 4 + 2] = src_row[x * 3];
                    row[x * 4 + 3] = 255;
                }
            });
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

pub fn check_registration_support() -> Result<()> {
    let mut path = std::env::current_exe().context("current executable path")?;
    path.set_file_name("4KRustCameraVirtualCamera.dll");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let module = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH);
        if module.is_null() {
            anyhow::bail!("LoadLibraryExW failed: {} (DLL path: {})", GetLastError(), path.display());
        }
        let proc = GetProcAddress(module, b"Check4KRustCameraSupport\0".as_ptr());
        if proc.is_none() { FreeLibrary(module); anyhow::bail!("virtual-camera support probe export is missing"); }
        type Fn = unsafe extern "system" fn() -> i32;
        let f: Fn = std::mem::transmute(proc);
        let hr = f();
        FreeLibrary(module);
        if hr < 0 { anyhow::bail!("virtual-camera API unsupported/unavailable: HRESULT 0x{:08X}", hr as u32); }
    }
    Ok(())
}

fn elevate_regsvr32(register: bool) -> Result<()> {
    let mut dll = std::env::current_exe().context("current executable path")?;
    dll.set_file_name("4KRustCameraVirtualCamera.dll");
    if !dll.is_file() {
        anyhow::bail!("virtual-camera DLL not found: {}", dll.display());
    }
    let system = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let regsvr = system.join("System32").join("regsvr32.exe");
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = regsvr.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let args_text = if register {
        format!("/s \"{}\"", dll.display())
    } else {
        format!("/s /u \"{}\"", dll.display())
    };
    let args: Vec<u16> = args_text.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = args.as_ptr();
        info.nShow = 0;
        if ShellExecuteExW(&mut info) == 0 {
            anyhow::bail!("elevated COM registration was not started: {}", GetLastError());
        }
        if info.hProcess.is_null() {
            anyhow::bail!("elevated COM registration returned no process handle");
        }
        let wait = WaitForSingleObject(info.hProcess, 30_000);
        if wait != WAIT_OBJECT_0 {
            CloseHandle(info.hProcess);
            anyhow::bail!("elevated COM registration timed out");
        }
        let mut exit_code = 1u32;
        if GetExitCodeProcess(info.hProcess, &mut exit_code) == 0 {
            let e = GetLastError();
            CloseHandle(info.hProcess);
            anyhow::bail!("could not read elevated registration exit code: {}", e);
        }
        CloseHandle(info.hProcess);
        if exit_code != 0 {
            anyhow::bail!("elevated regsvr32 failed with exit code {}", exit_code);
        }
    }
    Ok(())
}

pub fn call_registration(register: bool) -> Result<()> {\n    // The FrameServer loads the media source out-of-process, so its COM class\n    // must be registered system-wide. Perform that privileged operation first.\n    elevate_regsvr32(register)?;
    let mut path = std::env::current_exe().context("current executable path")?;
    path.set_file_name("4KRustCameraVirtualCamera.dll");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let module = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH);
        if module.is_null() {
            let code = GetLastError();
            anyhow::bail!("LoadLibraryExW failed: {} (DLL path: {})", code, path.display());
        }
        let name: &[u8] = if register { b"Register4KRustCamera\0" } else { b"Unregister4KRustCamera\0" };
        let proc = GetProcAddress(module, name.as_ptr());
        if proc.is_none() { FreeLibrary(module); anyhow::bail!("virtual-camera registration export is missing"); }
        type Fn = unsafe extern "system" fn() -> i32;
        let f: Fn = std::mem::transmute(proc);
        let hr = f();
        FreeLibrary(module);
        if hr < 0 {
            let code = hr as u32;
            let detail = match code {
                0x80004001 => "E_NOTIMPL: Windows virtual-camera API is not available/supported on this Windows build (MFCreateVirtualCamera requires Windows 11 build 22000+).",
                0x80070005 => "E_ACCESSDENIED: Windows camera privacy/access policy denied virtual-camera registration.",
                _ => "",
            };
            if detail.is_empty() {
                anyhow::bail!("virtual-camera registration failed: HRESULT 0x{:08X}", code);
            } else {
                anyhow::bail!("virtual-camera registration failed: HRESULT 0x{:08X} — {}", code, detail);
            }
        }
    }
    Ok(())
}
