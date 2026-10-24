//! Measures Flick's cold-start floor for one renderer: process creation to
//! the real picture (screen-scale WIC decode, fitted into the window) presented.
//!
//! The decode starts on a worker thread at `main`, in parallel with window and
//! renderer setup, as the real viewer would do it. The window is created
//! hidden and shown only when the decoded picture is drawn: no placeholder
//! frames. Every stage is timed from process creation (not from `main`), so
//! DLL loading and runtime init are included. Results are written as one JSON
//! object to `--out <file>` (or stdout) when the process exits.
//!
//! Usage: flick-startup-probe [--backend wgpu-dx12|wgpu-vulkan|wgpu-gl|gdi|d2d|d2d-dxgi]
//!        [--image file.jpg] [--size WxH] [--stay] [--out file]
//!
//! `--stay` keeps the window open until it is closed, so `flick-bench
//! ext-viewer` can time the probe from the outside like any other viewer.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod decode;
mod render;
mod render_wgpu;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

use decode::Image;
use render::Renderer;

#[derive(Clone, Copy)]
enum Backend {
    Wgpu(wgpu::Backends),
    Gdi,
    D2dHwnd,
    D2dDxgi,
    /// GDI draws the first frame; a GPU device created on a background thread
    /// from process start then takes the window over (the handover test).
    GdiThen(render::Attach),
}

enum UserEvent {
    GpuReady(Result<render::GpuDevice, String>, Vec<(&'static str, f64)>),
}

struct Args {
    backend: Backend,
    image: Option<PathBuf>,
    size: (u32, u32),
    stay: bool,
    /// Keep DWM's window-open animation (it delays the picture by ~200 ms).
    animate: bool,
    /// Show the window cloaked and uncloak it once the picture is drawn, so
    /// it never appears blank (`--no-cloak` turns this off).
    cloak: bool,
    out: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { backend: Backend::Wgpu(wgpu::Backends::DX12), image: None, size: (1280, 800), stay: false, animate: false, cloak: true, out: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--backend" => {
                args.backend = match value()?.as_str() {
                    "wgpu-dx12" | "dx12" => Backend::Wgpu(wgpu::Backends::DX12),
                    "wgpu-vulkan" | "vulkan" => Backend::Wgpu(wgpu::Backends::VULKAN),
                    "wgpu-gl" | "gl" => Backend::Wgpu(wgpu::Backends::GL),
                    "gdi" => Backend::Gdi,
                    "d2d" => Backend::D2dHwnd,
                    "d2d-dxgi" => Backend::D2dDxgi,
                    "gdi+hwnd" => Backend::GdiThen(render::Attach::Hwnd),
                    "gdi+dcomp" => Backend::GdiThen(render::Attach::Composition),
                    other => return Err(format!("unknown backend {other}")),
                }
            }
            "--image" => args.image = Some(PathBuf::from(value()?)),
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                args.size = (w.parse().map_err(|_| "bad --size width")?, h.parse().map_err(|_| "bad --size height")?);
            }
            "--stay" => args.stay = true,
            "--animate" => args.animate = true,
            "--no-cloak" => args.cloak = false,
            "--out" => args.out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

type Decoded = (Result<Image, String>, f64);

struct Probe {
    args: Args,
    stages: Vec<(&'static str, f64)>,
    decode: Option<JoinHandle<Decoded>>,
    image: Option<Image>,
    window: Option<Arc<Window>>,
    hwnd: Option<windows::Win32::Foundation::HWND>,
    renderer: Option<Box<dyn Renderer>>,
    /// A GPU device that arrived before the first frame was drawn.
    pending_gpu: Option<render::GpuDevice>,
    handed_over: bool,
    error: Option<String>,
}

impl Probe {
    fn mark(&mut self, stage: &'static str) {
        self.stages.push((stage, ms_since_process_start()));
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        self.error = Some(error);
        event_loop.exit();
    }

    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attrs = Window::default_attributes()
            .with_title("flick-startup-probe")
            .with_decorations(false)
            .with_inner_size(PhysicalSize::new(self.args.size.0, self.args.size.1))
            // Fixed and fully on-screen: Windows' default cascade pushes large
            // windows partly off the bottom, which skews outside timing.
            .with_position(PhysicalPosition::new(0, 0))
            .with_visible(false);
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        self.window = Some(window.clone());
        self.mark("window");

        let hwnd = match window.window_handle().map_err(|e| e.to_string())?.as_raw() {
            RawWindowHandle::Win32(h) => windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _),
            _ => return Err("not a Win32 window".into()),
        };
        if !self.args.animate {
            // DWM's fade/zoom-in runs after the window is shown and keeps the
            // finished picture from appearing at full opacity for ~200 ms.
            use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
            let disable = windows::core::BOOL::from(true);
            unsafe { DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, (&raw const disable).cast(), size_of_val(&disable) as u32) }
                .map_err(|e| e.to_string())?;
        }
        let stages = &mut self.stages;
        let mut mark = |stage: &'static str| stages.push((stage, ms_since_process_start()));
        let renderer: Box<dyn Renderer> = match self.args.backend {
            Backend::Wgpu(backends) => Box::new(render_wgpu::Wgpu::new(window.clone(), backends, &mut mark)?),
            Backend::Gdi => Box::new(render::Gdi::new(hwnd)),
            Backend::D2dHwnd => Box::new(render::D2dHwnd::new(hwnd, &mut mark)?),
            Backend::D2dDxgi => Box::new(render::D2dDxgi::new(hwnd, &mut mark)?),
            Backend::GdiThen(_) => Box::new(render::Gdi::new(hwnd)),
        };
        self.hwnd = Some(hwnd);
        self.mark("renderer_ready");

        if let Some(handle) = self.decode.take() {
            let (image, decoded_at) = handle.join().map_err(|_| "decode thread panicked")?;
            self.stages.push(("decoded", decoded_at));
            self.image = Some(image?);
        }

        // Shown only now, with the real picture ready: no placeholder frames.
        // A plain show composes the window before the first draw lands (a
        // blank frame); cloaked, it is drawn while invisible, then revealed.
        let mut renderer = renderer;
        renderer.prepare(self.image.as_ref())?;
        self.mark("prepared");
        if self.args.cloak {
            set_cloak(hwnd, true)?;
        }
        window.set_visible(true);
        renderer.draw(self.image.as_ref())?;
        if self.args.cloak {
            set_cloak(hwnd, false)?;
        }
        self.mark("frame");
        self.renderer = Some(renderer);
        if let Some(gpu) = self.pending_gpu.take() {
            self.hand_over(gpu)?;
        }
        Ok(())
    }

    /// The GPU renderer takes the window over and draws the same picture.
    fn hand_over(&mut self, gpu: render::GpuDevice) -> Result<(), String> {
        let Backend::GdiThen(how) = self.args.backend else { return Ok(()) };
        let hwnd = self.hwnd.ok_or("no window yet")?;
        let stages = &mut self.stages;
        let mut mark = |stage: &'static str| stages.push((stage, ms_since_process_start()));
        let mut renderer = render::D2dDxgi::attach(hwnd, &gpu, how, &mut mark)?;
        renderer.draw(self.image.as_ref())?;
        self.mark("handover");
        self.renderer = Some(Box::new(renderer));
        self.handed_over = true;
        Ok(())
    }

    /// Whether the probe still has work to do before a non-`--stay` exit.
    fn done(&self) -> bool {
        !matches!(self.args.backend, Backend::GdiThen(_)) || self.handed_over
    }

    fn report(&mut self) -> String {
        self.stages.sort_by(|a, b| a.1.total_cmp(&b.1));
        let stages = self.stages.iter().map(|(name, ms)| format!("[\"{name}\",{ms:.3}]")).collect::<Vec<_>>().join(",");
        let (private_mb, working_set_mb) = memory_mb();
        let error = match &self.error {
            Some(e) => format!("\"{}\"", json_escape(e)),
            None => "null".into(),
        };
        let backend = match self.args.backend {
            Backend::Wgpu(b) if b == wgpu::Backends::DX12 => "wgpu-dx12",
            Backend::Wgpu(b) if b == wgpu::Backends::VULKAN => "wgpu-vulkan",
            Backend::Wgpu(_) => "wgpu-gl",
            Backend::Gdi => "gdi",
            Backend::D2dHwnd => "d2d",
            Backend::D2dDxgi => "d2d-dxgi",
            Backend::GdiThen(render::Attach::Hwnd) => "gdi+hwnd",
            Backend::GdiThen(render::Attach::Composition) => "gdi+dcomp",
        };
        format!(
            "{{\"adapter\":\"{backend}\",\"stages\":[{stages}],\"private_mb\":{private_mb:.1},\"working_set_mb\":{working_set_mb:.1},\"error\":{error}}}"
        )
    }
}

impl ApplicationHandler<UserEvent> for Probe {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.error.is_some() {
            return;
        }
        match self.init(event_loop) {
            Ok(()) if !self.args.stay && self.done() => event_loop.exit(),
            Ok(()) => {}
            Err(e) => self.fail(event_loop, e),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::GpuReady(gpu, stages) = event;
        self.stages.extend(stages);
        let result = gpu.and_then(|gpu| {
            if self.renderer.is_none() {
                self.pending_gpu = Some(gpu);
                Ok(())
            } else {
                self.hand_over(gpu)
            }
        });
        match result {
            Err(e) => self.fail(event_loop, e),
            Ok(()) if !self.args.stay && self.done() => event_loop.exit(),
            Ok(()) => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // Repaints after the first frame (e.g. the window was uncovered).
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = &mut self.renderer
                    && let Err(e) = renderer.draw(self.image.as_ref())
                {
                    self.fail(event_loop, e);
                }
            }
            _ => {}
        }
    }
}

fn set_cloak(hwnd: windows::Win32::Foundation::HWND, cloak: bool) -> Result<(), String> {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    let value = windows::core::BOOL::from(cloak);
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, (&raw const value).cast(), size_of_val(&value) as u32) }.map_err(|e| e.to_string())
}

fn json_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect(),
            c if c.is_control() => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}

fn ms_since_process_start() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let ticks = |ft: FILETIME| (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user).is_err() {
            return f64::NAN;
        }
        ticks(GetSystemTimePreciseAsFileTime()).saturating_sub(ticks(created)) as f64 / 10_000.0
    }
}

fn memory_mb() -> (f64, f64) {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) };
    if ok.is_err() {
        return (f64::NAN, f64::NAN);
    }
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    (mb(c.PrivateUsage), mb(c.WorkingSetSize))
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let main_ms = ms_since_process_start();

    // Decode starts first: it is the longest pole for real photos.
    let decode = args.image.clone().map(|path| {
        let target = args.size;
        std::thread::spawn(move || {
            let image = decode::decode_to_fit(&path, target);
            (image, ms_since_process_start())
        })
    });

    let mut probe = Probe {
        args,
        stages: vec![("main", main_ms)],
        decode,
        image: None,
        window: None,
        hwnd: None,
        renderer: None,
        pending_gpu: None,
        handed_over: false,
        error: None,
    };
    let result = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string()).and_then(|event_loop| {
        probe.mark("event_loop");
        if let Backend::GdiThen(_) = probe.args.backend {
            let proxy = event_loop.create_proxy();
            std::thread::spawn(move || {
                let mut stages = Vec::new();
                let gpu = render::create_gpu_device(&mut |stage| stages.push((stage, ms_since_process_start())));
                let _ = proxy.send_event(UserEvent::GpuReady(gpu, stages));
            });
        }
        event_loop.run_app(&mut probe).map_err(|e| e.to_string())
    });
    if let Err(e) = result {
        probe.error.get_or_insert(e);
    }

    let report = probe.report();
    match &probe.args.out {
        Some(path) => std::fs::write(path, report).expect("write --out file"),
        None => println!("{report}"),
    }
}
