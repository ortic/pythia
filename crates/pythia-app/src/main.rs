//! Pythia – a fast, native Oracle PL/SQL IDE.

// Release builds are GUI programs on Windows, without a console window;
// debug builds keep it for development.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ai;
mod app;
mod assist;
mod browser;
mod catalog;
mod chart_view;
mod commands;
mod compare_window;
mod connections;
mod data_edit;
mod data_search;
mod debug_setup;
mod desktop;
mod dialogs;
mod documents;
mod editor;
mod editor_menu;
mod export_tables;
mod firewall;
mod git;
mod history;
mod icons_ext;
mod import_window;
mod invalid_window;
mod jobs_window;
mod maintenance;
mod monitor_window;
mod object_editor;
mod object_files;
mod object_view;
mod profiler;
mod program_window;
mod project;
mod relations;
mod reports;
#[cfg(test)]
mod screenshots;
mod secrets;
mod serde_path;
mod settings_dialog;
mod shared;
mod source_search;
mod sql_window;
mod statistics;
mod stubs;
mod table_editor;
mod table_partitions;
mod table_parts;
#[cfg(test)]
mod test_db;
mod test_script;
mod text_diff;
mod text_file;
mod theme;
mod to_file;

use std::fs::{self, File};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pythia_config::Paths;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() -> anyhow::Result<()> {
    attach_parent_console();
    let paths = Paths::detect();
    // Before logging creates the new directories.
    let moved = paths.as_ref().map(Paths::adopt_legacy).unwrap_or_default();
    let paths = match paths {
        Ok(paths) => Some(paths),
        Err(e) => {
            report_error(&format!(
                "Pythia could not find its settings folder ({e:#}). It starts without saved settings and connections."
            ));
            None
        }
    };
    init_logging(paths.as_ref());
    install_panic_hook(paths.as_ref());
    info!(version = env!("CARGO_PKG_VERSION"), "starting Pythia");
    for problem in moved {
        warn!("settings from before the rename to Pythia: {problem}");
    }

    // Set once the app exists: a later error isn't the renderer's, e.g. the
    // window system went away (#60).
    let started = Arc::new(AtomicBool::new(false));
    // The open windows are the first Pythia's (#196); held until the end.
    let workspace = paths
        .as_ref()
        .map(|p| shared::Workspace::claim(&p.data_dir));
    let kept = workspace.as_ref().map(|w| w.path.clone());
    // Set when the adapter chosen is DirectX 12's (#491).
    let dx12 = Arc::new(AtomicBool::new(false));
    let result = run(
        eframe::Renderer::Wgpu,
        wgpu_options(true, &dx12),
        paths.clone(),
        kept.clone(),
        &started,
    )
    .or_else(|e| {
        if started.load(Ordering::SeqCst) || !dx12.load(Ordering::SeqCst) {
            return Err(e);
        }
        warn!("DirectX 12 failed ({e}), trying the other wgpu backends");
        run(
            eframe::Renderer::Wgpu,
            wgpu_options(false, &dx12),
            paths.clone(),
            kept.clone(),
            &started,
        )
    })
    .or_else(|e| {
        if started.load(Ordering::SeqCst) {
            return Err(e);
        }
        warn!("wgpu renderer failed ({e}), falling back to OpenGL");
        run(
            eframe::Renderer::Glow,
            Default::default(),
            paths.clone(),
            kept,
            &started,
        )
    });
    drop(workspace);
    if let Err(e) = &result {
        let log = paths
            .as_ref()
            .map(|p| {
                format!(
                    "\n\nDetails are in {}",
                    p.log_dir().join("pythia.log").display()
                )
            })
            .unwrap_or_default();
        report_error(&if started.load(Ordering::SeqCst) {
            format!("Pythia lost its window: the window system ended the connection ({e}).{log}")
        } else {
            format!(
                "Pythia could not open its window: neither Vulkan/DirectX/Metal nor OpenGL works here ({e}).{log}"
            )
        });
    }
    result.map_err(|e| anyhow::anyhow!("{e}"))
}

/// Reports a problem where it's seen: there may be no console (a GUI
/// program on Windows, or started from the desktop), so also in a message
/// box.
fn report_error(text: &str) {
    error!("{text}");
    eprintln!("pythia: {text}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Pythia")
        .set_description(text)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Started from a terminal, a Windows GUI program has no console of its own;
/// attaching to the terminal's shows the log output there.
fn attach_parent_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        unsafe extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
        }
        const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
        // Fails when there's no parent console, e.g. started from Explorer.
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Runs the app until its window closes; sets `started` once the renderer
/// works and the app exists.
fn run(
    renderer: eframe::Renderer,
    wgpu_options: eframe::WgpuConfiguration,
    paths: Option<Paths>,
    workspace: Option<std::path::PathBuf>,
    started: &Arc<AtomicBool>,
) -> eframe::Result {
    let started = started.clone();
    let options = eframe::NativeOptions {
        renderer,
        wgpu_options,
        viewport: egui::ViewportBuilder::default()
            .with_title("Pythia")
            .with_app_id("pythia")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(window_icon()),
        persistence_path: workspace,
        event_loop_builder: x11_on_wslg(),
        ..Default::default()
    };
    eframe::run_native(
        "pythia",
        options,
        Box::new(move |cc| {
            started.store(true, Ordering::SeqCst);
            Ok(Box::new(app::PythiaApp::new(cc, paths)))
        }),
    )
}

/// wgpu's defaults, but Pythia chooses the adapter: with `dx12`, a DirectX
/// 12 one first, else none of DirectX 12. Sets `chose_dx12` when it's
/// DirectX 12's.
fn wgpu_options(dx12: bool, chose_dx12: &Arc<AtomicBool>) -> eframe::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew, adapter_info_summary, wgpu};
    let chose_dx12 = chose_dx12.clone();
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.native_adapter_selector = Some(Arc::new(move |adapters, surface| {
        let usable: Vec<&wgpu::Adapter> = adapters
            .iter()
            .filter(|a| dx12 || a.get_info().backend != wgpu::Backend::Dx12)
            .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
            .collect();
        let infos: Vec<wgpu::AdapterInfo> = usable.iter().map(|a| a.get_info()).collect();
        let preference =
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance);
        let kinds: Vec<_> = infos.iter().map(|a| (a.backend, a.device_type)).collect();
        let Some(chosen) = choose_adapter(&kinds, preference) else {
            return Err(format!(
                "none of the {} wgpu adapters can draw the window",
                adapters.len()
            ));
        };
        let all = adapters
            .iter()
            .map(|a| format!("{{{}}}", adapter_info_summary(&a.get_info())))
            .collect::<Vec<_>>();
        info!("wgpu adapters: {}", all.join(", "));
        info!("drawing with {}", adapter_info_summary(&infos[chosen]));
        chose_dx12.store(
            infos[chosen].backend == wgpu::Backend::Dx12,
            Ordering::SeqCst,
        );
        Ok(usable[chosen].clone())
    }));
    eframe::WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

/// The adapter to draw with, as wgpu orders them: the kind of GPU
/// `preference` (`WGPU_POWER_PREF`) puts first, by default a discrete one,
/// the software rasterizer last. Of the same kind, DirectX 12 goes first,
/// Windows' own: wgpu takes Vulkan, and with Intel's Vulkan driver the
/// window flickered (#491).
fn choose_adapter(
    adapters: &[(eframe::wgpu::Backend, eframe::wgpu::DeviceType)],
    preference: eframe::wgpu::PowerPreference,
) -> Option<usize> {
    use eframe::wgpu::{Backend, DeviceType, PowerPreference};
    let kind = |device: DeviceType| match (device, preference) {
        (_, PowerPreference::None) => 0,
        (DeviceType::DiscreteGpu, PowerPreference::HighPerformance) => 1,
        (DeviceType::IntegratedGpu, PowerPreference::HighPerformance) => 2,
        (DeviceType::IntegratedGpu, PowerPreference::LowPower) => 1,
        (DeviceType::DiscreteGpu, PowerPreference::LowPower) => 2,
        (DeviceType::Other, _) => 3,
        (DeviceType::VirtualGpu, _) => 4,
        (DeviceType::Cpu, _) => 5,
    };
    adapters
        .iter()
        .enumerate()
        .min_by_key(|(_, (backend, device))| (kind(*device), *backend != Backend::Dx12))
        .map(|(i, _)| i)
}

/// Under WSLg the window opens through X11 (Xwayland). WSLg's compositor,
/// Weston 9, keeps a pointer into a client's shm pool and crashes in
/// pixman when the client grows that pool, as winit's cursor theme does for
/// each new cursor shape (microsoft/wslg#1386); Xwayland never grows one
/// (#88). `PYTHIA_WAYLAND=1` keeps Wayland.
#[cfg(target_os = "linux")]
fn x11_on_wslg() -> Option<eframe::EventLoopBuilderHook> {
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    if !use_x11(&kernel, |name| {
        std::env::var_os(name).is_some_and(|v| !v.is_empty())
    }) {
        return None;
    }
    info!("WSLg: opening the window through X11 (PYTHIA_WAYLAND=1 keeps Wayland)");
    Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }))
}

#[cfg(not(target_os = "linux"))]
fn x11_on_wslg() -> Option<eframe::EventLoopBuilderHook> {
    None
}

/// On WSL (its kernel's release names Microsoft) with both Wayland and X11
/// there, unless `PYTHIA_WAYLAND` is set.
#[cfg(target_os = "linux")]
fn use_x11(kernel: &str, set: impl Fn(&str) -> bool) -> bool {
    kernel.to_ascii_lowercase().contains("microsoft")
        && set("WAYLAND_DISPLAY")
        && set("DISPLAY")
        && !set("PYTHIA_WAYLAND")
}

/// The application icon, embedded at build time.
fn window_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon/pythia-256.png"))
        .expect("embedded icon is a valid PNG")
}

/// Logs to stderr and to `<data dir>/logs/pythia.log`. `RUST_LOG` overrides the filter.
fn init_logging(paths: Option<&Paths>) {
    let filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,wgpu=warn,naga=warn,eframe=warn"))
    };
    let file = paths.and_then(|p| {
        let dir = p.log_dir();
        fs::create_dir_all(&dir).ok()?;
        File::create(dir.join("pythia.log")).ok()
    });
    let file_layer = file.map(|f| {
        fmt::layer()
            .with_ansi(false)
            .with_writer(Mutex::new(f))
            .with_filter(filter())
    });
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter()),
        )
        .with(file_layer)
        .init();
}

/// Logs panics and writes a crash report next to the log file.
fn install_panic_hook(paths: Option<&Paths>) {
    let crash_dir = paths.map(Paths::log_dir);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        error!("panic: {panic}\n{backtrace}");
        if let Some(dir) = &crash_dir {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let _ = fs::write(
                dir.join(format!("crash-{stamp}.txt")),
                format!("{panic}\n\n{backtrace}"),
            );
        }
        default_hook(panic);
    }));
}

#[cfg(test)]
mod adapter_tests {
    use super::choose_adapter;
    use eframe::wgpu::{Backend, DeviceType, PowerPreference};

    /// As wgpu lists them on Windows: Vulkan before DirectX 12 (#491).
    const INTEL: [(Backend, DeviceType); 4] = [
        (Backend::Vulkan, DeviceType::IntegratedGpu),
        (Backend::Dx12, DeviceType::IntegratedGpu),
        (Backend::Dx12, DeviceType::Cpu),
        (Backend::Gl, DeviceType::IntegratedGpu),
    ];

    #[test]
    fn directx_12_before_vulkan_of_the_same_gpu() {
        assert_eq!(
            choose_adapter(&INTEL, PowerPreference::HighPerformance),
            Some(1)
        );
        assert_eq!(choose_adapter(&INTEL, PowerPreference::None), Some(1));
        // The software rasterizer only when there's nothing else.
        assert_eq!(
            choose_adapter(&INTEL[2..3], PowerPreference::HighPerformance),
            Some(0)
        );
        assert_eq!(choose_adapter(&[], PowerPreference::HighPerformance), None);
    }

    #[test]
    fn the_kind_of_gpu_goes_before_the_backend() {
        let laptop = [
            (Backend::Vulkan, DeviceType::IntegratedGpu),
            (Backend::Vulkan, DeviceType::DiscreteGpu),
            (Backend::Dx12, DeviceType::IntegratedGpu),
            (Backend::Dx12, DeviceType::DiscreteGpu),
        ];
        assert_eq!(
            choose_adapter(&laptop, PowerPreference::HighPerformance),
            Some(3)
        );
        assert_eq!(choose_adapter(&laptop, PowerPreference::LowPower), Some(2));
        // Without DirectX 12 (Linux, or the second try): wgpu's order.
        assert_eq!(
            choose_adapter(&laptop[..2], PowerPreference::HighPerformance),
            Some(1)
        );
        let linux = [
            (Backend::Vulkan, DeviceType::IntegratedGpu),
            (Backend::Gl, DeviceType::IntegratedGpu),
        ];
        assert_eq!(
            choose_adapter(&linux, PowerPreference::HighPerformance),
            Some(0)
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::use_x11;

    #[test]
    fn x11_only_under_wslg() {
        let wsl = "6.18.54.1-microsoft-standard-WSL2";
        let env = |names: &'static [&'static str]| move |name: &str| names.contains(&name);
        assert!(use_x11(wsl, env(&["WAYLAND_DISPLAY", "DISPLAY"])));
        // Another Linux, or WSL without one of the two, or opted out.
        assert!(!use_x11(
            "6.8.0-45-generic",
            env(&["WAYLAND_DISPLAY", "DISPLAY"])
        ));
        assert!(!use_x11(wsl, env(&["WAYLAND_DISPLAY"])));
        assert!(!use_x11(wsl, env(&["DISPLAY"])));
        assert!(!use_x11(
            wsl,
            env(&["WAYLAND_DISPLAY", "DISPLAY", "PYTHIA_WAYLAND"])
        ));
    }
}
