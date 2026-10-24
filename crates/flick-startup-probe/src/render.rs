//! Renderers under test. Each draws the decoded image fitted into the window
//! on black, with linear-quality scaling, and presents it.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;
use windows::core::Interface;

use crate::decode::Image;

pub trait Renderer {
    /// Work that can happen before the window is shown.
    fn prepare(&mut self, _image: Option<&Image>) -> Result<(), String> {
        Ok(())
    }
    fn draw(&mut self, image: Option<&Image>) -> Result<(), String>;
}

/// `image` fitted into `window` (never upscaled), centred: (x, y, w, h).
pub fn fit_rect(image: (u32, u32), window: (u32, u32)) -> (f32, f32, f32, f32) {
    let (iw, ih) = (image.0 as f32, image.1 as f32);
    let (ww, wh) = (window.0 as f32, window.1 as f32);
    let scale = f32::min(1.0, f32::min(ww / iw, wh / ih));
    let (w, h) = ((iw * scale).round(), (ih * scale).round());
    (((ww - w) / 2.0).floor(), ((wh - h) / 2.0).floor(), w, h)
}

fn err(e: windows::core::Error) -> String {
    e.to_string()
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect) };
    ((rect.right - rect.left).max(1) as u32, (rect.bottom - rect.top).max(1) as u32)
}

const BLACK: D2D1_COLOR_F = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
const BGRA_IGNORE: D2D1_PIXEL_FORMAT = D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE };

fn dest(image: &Image, window: (u32, u32)) -> D2D_RECT_F {
    let (x, y, w, h) = fit_rect((image.width, image.height), window);
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

/// GDI with HALFTONE scaling: CPU only, no device to create. The whole frame
/// is composed off-screen once and copied to the window in a single blit,
/// so the screen never shows a half-drawn (black) frame.
pub struct Gdi {
    hwnd: HWND,
    frame: Option<Frame>,
}

/// A memory DC holding the composed window contents.
struct Frame {
    dc: HDC,
    bitmap: HBITMAP,
    size: (u32, u32),
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}

impl Gdi {
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd, frame: None }
    }

    fn compose(&self, image: Option<&Image>) -> Result<Frame, String> {
        let window = client_size(self.hwnd);
        unsafe {
            let screen = GetDC(Some(self.hwnd));
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, window.0 as i32, window.1 as i32);
            ReleaseDC(Some(self.hwnd), screen);
            SelectObject(dc, bitmap.into());
            let frame = Frame { dc, bitmap, size: window };
            let hdc = dc;
            let _ = PatBlt(hdc, 0, 0, window.0 as i32, window.1 as i32, BLACKNESS);
            if let Some(image) = image {
                let (x, y, w, h) = fit_rect((image.width, image.height), window);
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: image.width as i32,
                        biHeight: -(image.height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetStretchBltMode(hdc, HALFTONE);
                let _ = SetBrushOrgEx(hdc, 0, 0, None);
                let lines = StretchDIBits(
                    hdc,
                    x as i32,
                    y as i32,
                    w as i32,
                    h as i32,
                    0,
                    0,
                    image.width as i32,
                    image.height as i32,
                    Some(image.pixels.as_ptr().cast()),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                if lines == 0 {
                    return Err("StretchDIBits failed".into());
                }
            }
            let _ = GdiFlush();
            Ok(frame)
        }
    }
}

impl Renderer for Gdi {
    fn prepare(&mut self, image: Option<&Image>) -> Result<(), String> {
        self.frame = Some(self.compose(image)?);
        Ok(())
    }

    fn draw(&mut self, image: Option<&Image>) -> Result<(), String> {
        let window = client_size(self.hwnd);
        if self.frame.as_ref().is_none_or(|f| f.size != window) {
            self.prepare(image)?;
        }
        let frame = self.frame.as_ref().ok_or("no frame")?;
        unsafe {
            let hdc = GetDC(Some(self.hwnd));
            let copied = BitBlt(hdc, 0, 0, window.0 as i32, window.1 as i32, Some(frame.dc), 0, 0, SRCCOPY);
            let _ = GdiFlush();
            ReleaseDC(Some(self.hwnd), hdc);
            copied.map_err(err)
        }
    }
}

/// Direct2D `ID2D1HwndRenderTarget`: the classic one-call D2D setup.
pub struct D2dHwnd {
    target: ID2D1HwndRenderTarget,
    window: (u32, u32),
}

impl D2dHwnd {
    pub fn new(hwnd: HWND, mark: &mut dyn FnMut(&'static str)) -> Result<Self, String> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).map_err(err)?;
            mark("d2d_factory");
            let window = client_size(hwnd);
            let props = D2D1_RENDER_TARGET_PROPERTIES { pixelFormat: BGRA_IGNORE, dpiX: 96.0, dpiY: 96.0, ..Default::default() };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: window.0, height: window.1 },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let target = factory.CreateHwndRenderTarget(&props, &hwnd_props).map_err(err)?;
            mark("render_target");
            Ok(Self { target, window })
        }
    }
}

impl Renderer for D2dHwnd {
    fn draw(&mut self, image: Option<&Image>) -> Result<(), String> {
        unsafe {
            let bitmap = match image {
                Some(image) => Some(
                    self.target
                        .CreateBitmap(
                            D2D_SIZE_U { width: image.width, height: image.height },
                            Some(image.pixels.as_ptr().cast()),
                            image.width * 4,
                            &D2D1_BITMAP_PROPERTIES { pixelFormat: BGRA_IGNORE, dpiX: 96.0, dpiY: 96.0 },
                        )
                        .map_err(err)?,
                ),
                None => None,
            };
            self.target.BeginDraw();
            self.target.Clear(Some(&BLACK));
            if let (Some(bitmap), Some(image)) = (&bitmap, image) {
                self.target.DrawBitmap(bitmap, Some(&dest(image, self.window)), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
            }
            self.target.EndDraw(None, None).map_err(err)
        }
    }
}

/// A Direct3D 11 device with a Direct2D device on top. Creating it is where
/// the ~170 ms of driver start goes, so the handover backends create it on a
/// background thread from process start.
pub struct GpuDevice {
    d3d: ID3D11Device,
    dxgi: IDXGIDevice,
    d2d: ID2D1Device,
}

// The D3D11 device is free-threaded and the D2D factory is created
// multi-threaded, so the device may be built on one thread and used on another.
unsafe impl Send for GpuDevice {}

pub fn create_gpu_device(mark: &mut dyn FnMut(&'static str)) -> Result<GpuDevice, String> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
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
        let d3d = device.ok_or("no D3D11 device")?;
        mark("d3d11_device");
        let dxgi: IDXGIDevice = d3d.cast().map_err(err)?;
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None).map_err(err)?;
        let d2d = factory.CreateDevice(&dxgi).map_err(err)?;
        mark("d2d_device");
        Ok(GpuDevice { d3d, dxgi, d2d })
    }
}

/// How the swap chain reaches the window.
#[derive(Clone, Copy)]
pub enum Attach {
    /// `CreateSwapChainForHwnd`: the swap chain replaces the window's
    /// GDI-drawn redirection surface.
    Hwnd,
    /// DirectComposition: the swap chain is a visual composited above the
    /// redirection surface, which stays visible until the visual is committed.
    Composition,
}

/// Direct2D drawing into a DXGI flip-model swap chain on a Direct3D 11
/// device: the native path a non-wgpu Flick would use.
pub struct D2dDxgi {
    swap_chain: IDXGISwapChain1,
    context: ID2D1DeviceContext,
    window: (u32, u32),
    /// Kept alive while the visual is shown; committed after the first Present.
    composition: Option<(windows::Win32::Graphics::DirectComposition::IDCompositionDevice, windows::Win32::Graphics::DirectComposition::IDCompositionTarget, windows::Win32::Graphics::DirectComposition::IDCompositionVisual, bool)>,
}

impl D2dDxgi {
    /// Everything on the calling thread, in order.
    pub fn new(hwnd: HWND, mark: &mut dyn FnMut(&'static str)) -> Result<Self, String> {
        let gpu = create_gpu_device(mark)?;
        Self::attach(hwnd, &gpu, Attach::Hwnd, mark)
    }

    pub fn attach(hwnd: HWND, gpu: &GpuDevice, how: Attach, mark: &mut dyn FnMut(&'static str)) -> Result<Self, String> {
        use windows::Win32::Graphics::DirectComposition::{DCompositionCreateDevice, IDCompositionDevice};
        unsafe {
            let factory: IDXGIFactory2 = gpu.dxgi.GetAdapter().map_err(err)?.GetParent().map_err(err)?;
            let window = client_size(hwnd);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: window.0,
                Height: window.1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let (swap_chain, composition) = match how {
                Attach::Hwnd => (factory.CreateSwapChainForHwnd(&gpu.d3d, hwnd, &desc, None, None).map_err(err)?, None),
                Attach::Composition => {
                    let swap_chain = factory.CreateSwapChainForComposition(&gpu.d3d, &desc, None).map_err(err)?;
                    let device: IDCompositionDevice = DCompositionCreateDevice(&gpu.dxgi).map_err(err)?;
                    let target = device.CreateTargetForHwnd(hwnd, true).map_err(err)?;
                    let visual = device.CreateVisual().map_err(err)?;
                    visual.SetContent(&swap_chain).map_err(err)?;
                    target.SetRoot(&visual).map_err(err)?;
                    (swap_chain, Some((device, target, visual, false)))
                }
            };
            mark("swap_chain");

            let context = gpu.d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE).map_err(err)?;
            let surface: IDXGISurface = swap_chain.GetBuffer(0).map_err(err)?;
            let target = context
                .CreateBitmapFromDxgiSurface(
                    &surface,
                    Some(&D2D1_BITMAP_PROPERTIES1 {
                        pixelFormat: BGRA_IGNORE,
                        dpiX: 96.0,
                        dpiY: 96.0,
                        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                        ..Default::default()
                    }),
                )
                .map_err(err)?;
            context.SetTarget(&target);
            mark("d2d_context");
            Ok(Self { swap_chain, context, window, composition })
        }
    }
}

impl Renderer for D2dDxgi {
    fn draw(&mut self, image: Option<&Image>) -> Result<(), String> {
        unsafe {
            let bitmap = match image {
                Some(image) => Some(
                    self.context
                        .CreateBitmap(
                            D2D_SIZE_U { width: image.width, height: image.height },
                            Some(image.pixels.as_ptr().cast()),
                            image.width * 4,
                            &D2D1_BITMAP_PROPERTIES1 { pixelFormat: BGRA_IGNORE, dpiX: 96.0, dpiY: 96.0, ..Default::default() },
                        )
                        .map_err(err)?,
                ),
                None => None,
            };
            self.context.BeginDraw();
            self.context.Clear(Some(&BLACK));
            if let (Some(bitmap), Some(image)) = (&bitmap, image) {
                self.context.DrawBitmap(bitmap, Some(&dest(image, self.window)), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
            }
            self.context.EndDraw(None, None).map_err(err)?;
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok().map_err(err)?;
            // The visual goes live only now, already holding a frame.
            if let Some((device, _, _, committed)) = &mut self.composition
                && !*committed
            {
                device.Commit().map_err(err)?;
                *committed = true;
            }
            Ok(())
        }
    }
}
