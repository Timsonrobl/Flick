//! The renderer, chosen by measurement (bench/README.md): Direct2D on a
//! Direct3D 11 device, presented through a DirectComposition swap chain
//! (ARCHITECTURE.md §9).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, D2DERR_RECREATE_TARGET, HANDLE, HWND};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject};
use windows::core::Interface;
use windows_numerics::Matrix3x2;

use crate::decode::Image;

/// A call reported the device gone (`Gpu::lost`).
static LOST: AtomicBool = AtomicBool::new(false);

fn err(e: windows::core::Error) -> String {
    if [DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, D2DERR_RECREATE_TARGET].contains(&e.code()) {
        LOST.store(true, Ordering::Relaxed);
    }
    e.to_string()
}

/// Shift+F12: acts as if the device were lost, to try the recovery.
pub fn simulate_loss() {
    LOST.store(true, Ordering::Relaxed);
}

/// An event Windows sets when the device is lost (a driver update or
/// reset, a GPU hang, the adapter removed); `Gpu`'s drop sets it too.
pub struct Removed(HANDLE);

// An event handle may be waited on and set from any thread.
unsafe impl Send for Removed {}
unsafe impl Sync for Removed {}

impl Removed {
    pub fn wait(&self) {
        unsafe { WaitForSingleObject(self.0, INFINITE) };
    }
}

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Pictures: transparent pixels (GIF, PNG) show the window's background.
/// Window targets too (opaque frames; see `Surface::new`).
const BGRA_PREMULTIPLIED: D2D1_PIXEL_FORMAT = D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED };

/// One per process. Creating it is the ~170 ms driver start, so it happens on
/// a background thread from process start.
pub struct Gpu {
    d3d: ID3D11Device,
    dxgi: IDXGIDevice,
    factory: IDXGIFactory2,
    d2d: ID2D1Device,
    /// For creating bitmaps outside any window; bitmaps from one device
    /// context can be drawn by every context of the same D2D device.
    uploads: ID2D1DeviceContext,
    /// Windows' signal that the device is lost, and its registration.
    removed: Option<(Arc<Removed>, u32)>,
}

// The D3D11 device is free-threaded and the D2D factory is multi-threaded, so
// the device may be built on one thread and used on another.
unsafe impl Send for Gpu {}

impl Gpu {
    pub fn create(mark: &mut dyn FnMut(&'static str)) -> Result<Gpu, String> {
        LOST.store(false, Ordering::Relaxed);
        unsafe {
            let mut device = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .map_err(err)?;
            let d3d: ID3D11Device = device.ok_or("no D3D11 device")?;
            mark("d3d11_device");
            let dxgi: IDXGIDevice = d3d.cast().map_err(err)?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter().map_err(err)?.GetParent().map_err(err)?;
            let d2d_factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None).map_err(err)?;
            let d2d = d2d_factory.CreateDevice(&dxgi).map_err(err)?;
            let uploads = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE).map_err(err)?;
            mark("d2d_device");
            let removed = d3d.cast::<ID3D11Device4>().ok().and_then(|d3d| {
                let event = Arc::new(Removed(CreateEventW(None, true, false, None).ok()?));
                let cookie = d3d.RegisterDeviceRemovedEvent(event.0).ok()?;
                Some((event, cookie))
            });
            Ok(Gpu { d3d, dxgi, factory, d2d, uploads, removed })
        }
    }

    /// Whether the device is gone: everything made on it must be made again
    /// on a new one.
    pub fn lost(&self) -> bool {
        LOST.load(Ordering::Relaxed) || unsafe { self.d3d.GetDeviceRemovedReason() }.is_err()
    }

    /// Set when the device is lost (or this `Gpu` is dropped), for a thread
    /// to wait on.
    pub fn removed(&self) -> Option<Arc<Removed>> {
        self.removed.as_ref().map(|(event, _)| event.clone())
    }

    /// A device context for a worker thread to make and fill bitmaps with
    /// (tiles), off the UI thread.
    pub fn uploader(&self) -> Result<Uploader, String> {
        unsafe { self.d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }.map(Uploader).map_err(err)
    }

    pub fn upload(&self, image: &Image) -> Result<ID2D1Bitmap1, String> {
        unsafe {
            self.uploads
                .CreateBitmap(
                    D2D_SIZE_U { width: image.width, height: image.height },
                    Some(image.pixels.as_ptr().cast()),
                    image.width * 4,
                    &D2D1_BITMAP_PROPERTIES1 { pixelFormat: BGRA_PREMULTIPLIED, dpiX: 96.0, dpiY: 96.0, ..Default::default() },
                )
                .map_err(err)
        }
    }

    /// Copies `image` into an existing bitmap of the same size (a streaming
    /// animation reuses its few bitmaps instead of allocating per frame).
    pub fn refill(&self, bitmap: &ID2D1Bitmap1, image: &Image) -> Result<(), String> {
        let size = unsafe { bitmap.GetPixelSize() };
        if (size.width, size.height) != (image.width, image.height) {
            return Err("size differs".into());
        }
        unsafe { bitmap.CopyFromMemory(None, image.pixels.as_ptr().cast(), image.width * 4) }.map_err(err)
    }

    pub fn max_bitmap_size(&self) -> u32 {
        unsafe { self.uploads.GetMaximumBitmapSize() }
    }

    /// Really frees what was released: D3D11 destroys released objects only
    /// once its immediate context is flushed (nothing flushes it while no
    /// window draws), and Direct2D keeps a cache of scratch textures.
    pub fn release_unused(&self) {
        unsafe {
            self.d2d.ClearResources(0);
            if let Ok(context) = self.d3d.GetImmediateContext() {
                context.ClearState();
                context.Flush();
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if let Some((event, cookie)) = &self.removed {
            unsafe {
                if let Ok(d3d) = self.d3d.cast::<ID3D11Device4>() {
                    d3d.UnregisterDeviceRemoved(*cookie);
                }
                // Releases the thread waiting on it.
                let _ = SetEvent(event.0);
            }
        }
    }
}

/// Makes bitmaps on a worker thread: its own device context of the shared
/// device. Bitmaps one context makes, every context of the device draws.
pub struct Uploader(ID2D1DeviceContext);

// The device's factory is multi-threaded: Direct2D serialises calls on it,
// so a context (and its bitmaps) may be used from another thread.
unsafe impl Send for Uploader {}

impl Uploader {
    /// An empty premultiplied BGRA bitmap, filled with `copy`.
    pub fn blank(&self, (width, height): (u32, u32)) -> Result<ID2D1Bitmap1, String> {
        let properties = D2D1_BITMAP_PROPERTIES1 { pixelFormat: BGRA_PREMULTIPLIED, dpiX: 96.0, dpiY: 96.0, ..Default::default() };
        unsafe { self.0.CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &properties) }.map_err(err)
    }

    /// Copies rows of BGRA `pixels` (`pitch` bytes apart) into `bitmap` at
    /// `rect` (left, top, right, bottom).
    pub fn copy(bitmap: &ID2D1Bitmap1, rect: (u32, u32, u32, u32), pixels: &[u8], pitch: u32) -> Result<(), String> {
        let (left, top, right, bottom) = rect;
        let needed = (bottom - top).saturating_sub(1) as usize * pitch as usize + (right - left) as usize * 4;
        if pixels.len() < needed {
            return Err("pixels short of the rectangle".into());
        }
        unsafe { bitmap.CopyFromMemory(Some(&D2D_RECT_U { left, top, right, bottom }), pixels.as_ptr().cast(), pitch) }.map_err(err)
    }
}

/// A window's drawing surface: a flip-model swap chain shown as a
/// DirectComposition visual, and a Direct2D context targeting it.
pub struct Surface {
    swap_chain: IDXGISwapChain1,
    pub context: ID2D1DeviceContext,
    composition: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    pub size: (u32, u32),
    committed: bool,
    /// Diagnostics (`FLICK_GPU_SYNC=1`): an event query to wait on the GPU.
    gpu_sync: Option<(ID3D11DeviceContext, ID3D11Query)>,
    /// The Soften+Sharpen filter's effects, once that filter has been used.
    pub sharpen: Option<Sharpen>,
}

impl Surface {
    pub fn new(gpu: &Gpu, hwnd: HWND, size: (u32, u32)) -> Result<Surface, String> {
        unsafe {
            // Premultiplied alpha, though every frame is opaque: it keeps the
            // compositor drawing the window. An opaque swap chain may be put on
            // a hardware overlay plane instead, and on an NVIDIA RTX 4080 that
            // goes black on screen (not in captures) while resizing, and makes
            // page turns ~35 ms slower to reach the screen (bench/README.md).
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0.max(1),
                Height: size.1.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swap_chain = gpu.factory.CreateSwapChainForComposition(&gpu.d3d, &desc, None).map_err(err)?;
            let composition: IDCompositionDevice = DCompositionCreateDevice(&gpu.dxgi).map_err(err)?;
            let target = composition.CreateTargetForHwnd(hwnd, true).map_err(err)?;
            let visual = composition.CreateVisual().map_err(err)?;
            visual.SetContent(&swap_chain).map_err(err)?;
            target.SetRoot(&visual).map_err(err)?;
            let context = gpu.d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE).map_err(err)?;
            let gpu_sync = if std::env::var_os("FLICK_GPU_SYNC").is_some() {
                let mut query = None;
                gpu.d3d.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut query)).map_err(err)?;
                Some((gpu.d3d.GetImmediateContext().map_err(err)?, query.ok_or("no query")?))
            } else {
                None
            };
            let surface = Surface {
                swap_chain,
                context,
                composition,
                _target: target,
                _visual: visual,
                size: desc_size(&desc),
                committed: false,
                gpu_sync,
                sharpen: None,
            };
            surface.bind_target()?;
            Ok(surface)
        }
    }

    fn bind_target(&self) -> Result<(), String> {
        unsafe {
            let buffer: IDXGISurface = self.swap_chain.GetBuffer(0).map_err(err)?;
            let target = self
                .context
                .CreateBitmapFromDxgiSurface(
                    &buffer,
                    Some(&D2D1_BITMAP_PROPERTIES1 {
                        pixelFormat: BGRA_PREMULTIPLIED,
                        dpiX: 96.0,
                        dpiY: 96.0,
                        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                        ..Default::default()
                    }),
                )
                .map_err(err)?;
            self.context.SetTarget(&target);
            Ok(())
        }
    }

    pub fn resize(&mut self, size: (u32, u32)) -> Result<(), String> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return Ok(());
        }
        unsafe {
            // The buffers can't be resized while D2D still targets them.
            self.context.SetTarget(None);
            self.swap_chain.ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0)).map_err(err)?;
        }
        self.size = size;
        self.bind_target()
    }

    /// Diagnostics: blocks until the GPU has finished all submitted work and
    /// returns how long that took, if `FLICK_GPU_SYNC` is set.
    pub fn wait_for_gpu(&self) -> Option<f64> {
        let (context, query) = self.gpu_sync.as_ref()?;
        let started = std::time::Instant::now();
        unsafe {
            context.End(query);
            let mut done = windows::core::BOOL(0);
            while !done.as_bool() {
                if context.GetData(query, Some((&raw mut done).cast()), size_of_val(&done) as u32, 0).is_err() {
                    return None;
                }
            }
        }
        Some(started.elapsed().as_secs_f64() * 1000.0)
    }

    /// The compositor's current frame rate in Hz (lower than the display's
    /// maximum while variable refresh idles).
    pub fn composition_hz(&self) -> Option<f64> {
        let stats = unsafe { self.composition.GetFrameStatistics() }.ok()?;
        let rate = stats.currentCompositionRate;
        (rate.Denominator != 0).then(|| f64::from(rate.Numerator) / f64::from(rate.Denominator))
    }

    /// Draws a frame with `paint` and presents it. The first present also
    /// commits the composition visual, so the window's first visible content
    /// is this frame.
    pub fn frame(&mut self, background: D2D1_COLOR_F, paint: impl FnOnce(&ID2D1DeviceContext)) -> Result<(), String> {
        unsafe {
            self.context.BeginDraw();
            self.context.Clear(Some(&background));
            paint(&self.context);
            self.context.EndDraw(None, None).map_err(err)?;
            // Sync interval 0 and a frame latency of 1 were measured: no
            // difference to key-to-screen time (bench/README.md, milestone 2).
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok().map_err(err)?;
            if !self.committed {
                self.composition.Commit().map_err(err)?;
                self.committed = true;
            }
        }
        Ok(())
    }
}

/// The Soften+Sharpen filter: the picture resampled into window space with
/// high-quality cubic (2D affine transform effect), then sharpened there, so
/// the sharpening works on screen pixels at any zoom. Created on first use.
pub struct Sharpen {
    transform: ID2D1Effect,
    sharpen: ID2D1Effect,
}

impl Sharpen {
    /// D2D's sharpness runs 0-10; this is a light touch, like Honeyview's.
    const SHARPNESS: f32 = 2.0;

    pub fn new(context: &ID2D1DeviceContext) -> Result<Sharpen, String> {
        unsafe {
            let transform = context.CreateEffect(&CLSID_D2D12DAffineTransform).map_err(err)?;
            let mode = D2D1_2DAFFINETRANSFORM_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC.0 as u32;
            transform.SetValue(D2D1_2DAFFINETRANSFORM_PROP_INTERPOLATION_MODE.0 as u32, D2D1_PROPERTY_TYPE_ENUM, &mode.to_le_bytes()).map_err(err)?;
            let sharpen = context.CreateEffect(&CLSID_D2D1Sharpen).map_err(err)?;
            sharpen.SetValue(D2D1_SHARPEN_PROP_SHARPNESS.0 as u32, D2D1_PROPERTY_TYPE_FLOAT, &Self::SHARPNESS.to_le_bytes()).map_err(err)?;
            sharpen.SetInput(0, &transform.GetOutput().map_err(err)?, true);
            Ok(Sharpen { transform, sharpen })
        }
    }

    /// Draws `bitmap` mapped to the window by `to_window`.
    pub fn draw(&self, context: &ID2D1DeviceContext, bitmap: &ID2D1Bitmap1, to_window: Matrix3x2) -> Result<(), String> {
        unsafe {
            self.transform.SetInput(0, bitmap, true);
            let m = [to_window.M11, to_window.M12, to_window.M21, to_window.M22, to_window.M31, to_window.M32];
            let bytes: Vec<u8> = m.iter().flat_map(|v| v.to_le_bytes()).collect();
            self.transform.SetValue(D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX.0 as u32, D2D1_PROPERTY_TYPE_MATRIX_3X2, &bytes).map_err(err)?;
            context.SetTransform(&Matrix3x2::identity());
            context.DrawImage(&self.sharpen.GetOutput().map_err(err)?, None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
            Ok(())
        }
    }
}

fn desc_size(desc: &DXGI_SWAP_CHAIN_DESC1) -> (u32, u32) {
    (desc.Width, desc.Height)
}
