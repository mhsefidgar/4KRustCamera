use std::sync::Mutex;
use std::path::PathBuf;
use image::RgbImage;
use libloading::Library;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject, WAIT_OBJECT_0};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const BYTES: usize = (WIDTH as usize) * (HEIGHT as usize) * 4;
const MAPPING: &str = "Local\\4KRustCameraFrameRing";
const MUTEX: &str = "Local\\4KRustCameraFrameRingMutex";

#[repr(C)]
struct FrameHeader {
    sequence: u64,
    timestamp100ns: u64,
    width: u32,
    height: u32,
    stride: u32,
}

pub struct VirtualPublisher {
    mapping: Mutex<Option<(HANDLE, *mut u8)>>,
    mutex: Mutex<Option<HANDLE>>,
    library: Mutex<Option<Library>>,
}
unsafe impl Send for VirtualPublisher {}
unsafe impl Sync for VirtualPublisher {}

impl VirtualPublisher {
    pub fn new() -> Self {
        Self { mapping: Mutex::new(None), mutex: Mutex::new(None), library: Mutex::new(None) }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn ensure_transport(&self) -> Result<(), String> {
        let mut mapping = self.mapping.lock().map_err(|_| "Virtual-camera mapping lock failed.".to_owned())?;
        if mapping.is_some() {
            return Ok(());
        }

        let name = Self::wide(MAPPING);
        let size = std::mem::size_of::<FrameHeader>() + BYTES;
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                size as u32,
                name.as_ptr(),
            )
        };
        if handle == 0 {
            return Err(format!("Could not create virtual-camera frame mapping: {}.", std::io::Error::last_os_error()));
        }

        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) } as *mut u8;
        if view.is_null() {
            unsafe { CloseHandle(handle); }
            return Err("Could not map virtual-camera frame buffer.".to_owned());
        }
        unsafe { std::ptr::write_bytes(view, 0, size); }
        *mapping = Some((handle, view));

        let mut mutex = self.mutex.lock().map_err(|_| "Virtual-camera mutex lock failed.".to_owned())?;
        let mutex_name = Self::wide(MUTEX);
        let mutex_handle = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
        if mutex_handle == 0 {
            return Err(format!("Could not create virtual-camera mutex: {}.", std::io::Error::last_os_error()));
        }
        *mutex = Some(mutex_handle);
        Ok(())
    }

    pub fn publish(&self, image: &RgbImage) {
        if self.ensure_transport().is_err() {
            return;
        }

        let mutex_handle = match self.mutex.lock().ok().and_then(|m| *m) {
            Some(handle) => handle,
            None => return,
        };
        if unsafe { WaitForSingleObject(mutex_handle, 5) } != WAIT_OBJECT_0 {
            return;
        }

        let mapping = match self.mapping.lock().ok().and_then(|m| *m) {
            Some(value) => value,
            None => {
                unsafe { ReleaseMutex(mutex_handle); }
                return;
            }
        };

        let (_, base) = mapping;
        let header = base as *mut FrameHeader;
        let pixels = unsafe { base.add(std::mem::size_of::<FrameHeader>()) };
        let frame = image::imageops::resize(image, WIDTH, HEIGHT, image::imageops::FilterType::Triangle);
        let sequence = unsafe { (*header).sequence.wrapping_add(1) };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64 * 10;

        unsafe {
            for (index, rgb) in frame.as_raw().chunks_exact(3).enumerate() {
                let dst = pixels.add(index * 4);
                *dst = rgb[2];
                *dst.add(1) = rgb[1];
                *dst.add(2) = rgb[0];
                *dst.add(3) = 255;
            }
            (*header).width = WIDTH;
            (*header).height = HEIGHT;
            (*header).stride = WIDTH * 4;
            (*header).timestamp100ns = timestamp;
            (*header).sequence = sequence;
        }

        unsafe { ReleaseMutex(mutex_handle); }
    }

    pub fn register(&self) -> Result<(), String> {
        self.ensure_transport()?;

        let exe = std::env::current_exe().map_err(|e| format!("Could not locate application: {e}"))?;
        let dll = exe.parent().unwrap_or_else(|| std::path::Path::new("."))
            .join("4KRustCameraVirtualCamera.dll");

        let library = unsafe { Library::new(&dll) }
            .map_err(|e| format!("Virtual-camera DLL not found at {}. Build virtual-camera/VirtualCameraMediaSource.vcxproj first. {e}", dll.display()))?;

        type Register = unsafe extern "system" fn() -> i32;
        let function: libloading::Symbol<Register> = unsafe {
            library.get(b"Register4KRustVirtualCamera\\0")
        }.map_err(|e| format!("Virtual-camera DLL is missing Register4KRustVirtualCamera: {e}"))?;

        let hr = unsafe { function() };
        if hr < 0 {
            return Err(format!("Media Foundation virtual-camera registration failed (HRESULT 0x{:08X}).", hr as u32));
        }

        *self.library.lock().map_err(|_| "Virtual-camera DLL lock failed.".to_owned())? = Some(library);
        Ok(())
    }
}
