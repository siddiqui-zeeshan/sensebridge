//! SenseBridge: the window, the tray icon, and the engine they control.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod settings;

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use sensebridge_engine::{realtime, usbipctl, Config, Engine, Event};
use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewWindow, WindowEvent};

use settings::{Response, Settings};

const TRAY_ID: &str = "main";
/// Pages the interface may open in the browser.
const LINKS: [(&str, &str); 3] = [
    ("driver", usbipctl::DOWNLOAD_URL),
    ("project", env!("CARGO_PKG_REPOSITORY")),
    ("saxense", "https://apps.sdore.me/SAxense"),
];

struct AppState {
    engine: Mutex<Option<Engine>>,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    /// The engine's latest state, for a window that opens after it was reported.
    last_state: Arc<Mutex<Option<Event>>>,
}

impl AppState {
    fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Settings {
        let mut settings = self.settings.lock().unwrap();
        change(&mut settings);
        *settings = settings.clone().sanitised();
        if let Err(error) = settings.save(&self.settings_path) {
            eprintln!("could not save settings: {error}");
        }
        settings.clone()
    }
}

#[derive(Serialize)]
struct Snapshot {
    settings: Settings,
    running: bool,
    state: Option<Event>,
    open_at_sign_in: bool,
    version: &'static str,
}

fn tray_tooltip(event: &Event) -> Option<&'static str> {
    let Event::State { state, reason, .. } = event else { return None };
    Some(match (state.as_str(), reason.as_str()) {
        ("ready", _) => "SenseBridge: ready",
        ("waiting", "wired") => "SenseBridge: controller is on a cable",
        ("waiting", _) => "SenseBridge: waiting for the controller",
        ("no_driver", _) => "SenseBridge: driver needed",
        ("attaching", _) => "SenseBridge: connecting to Windows",
        ("stopped", _) => "SenseBridge: off",
        _ => "SenseBridge",
    })
}

fn start_engine(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut engine = state.engine.lock().unwrap();
    if engine.is_some() {
        return;
    }
    let settings = state.settings.lock().unwrap().clone();
    let config = Config {
        gain: settings.strength,
        buffer: settings.response.buffer(),
        ..Config::default()
    };
    let (app, last_state) = (app.clone(), Arc::clone(&state.last_state));
    *engine = Some(Engine::start(
        config,
        Arc::new(move |event| {
            if let Some(tooltip) = tray_tooltip(&event) {
                *last_state.lock().unwrap() = Some(event.clone());
                if let Some(tray) = app.tray_by_id(TRAY_ID) {
                    let _ = tray.set_tooltip(Some(tooltip));
                }
            }
            let _ = app.emit("engine", &event);
        }),
    ));
}

fn stop_engine(app: &AppHandle) {
    let engine = app.state::<AppState>().engine.lock().unwrap().take();
    drop(engine); // stops, detaches, and reports "stopped"
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn snapshot(state: State<AppState>) -> Snapshot {
    Snapshot {
        settings: state.settings.lock().unwrap().clone(),
        running: state.engine.lock().unwrap().is_some(),
        state: state.last_state.lock().unwrap().clone(),
        open_at_sign_in: autostart::enabled(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command(async)]
fn start(app: AppHandle) {
    start_engine(&app);
}

#[tauri::command(async)]
fn stop(app: AppHandle) {
    stop_engine(&app);
}

#[tauri::command]
fn set_strength(state: State<AppState>, value: f32) {
    let settings = state.update_settings(|s| s.strength = value);
    if let Some(engine) = state.engine.lock().unwrap().as_ref() {
        engine.set_gain(settings.strength);
    }
}

#[tauri::command]
fn set_response(state: State<AppState>, value: Response) {
    state.update_settings(|s| s.response = value);
    if let Some(engine) = state.engine.lock().unwrap().as_ref() {
        engine.set_buffer(value.buffer());
    }
}

#[tauri::command]
fn set_start_on_launch(state: State<AppState>, value: bool) {
    state.update_settings(|s| s.start_on_launch = value);
}

#[tauri::command]
fn set_close_to_tray(state: State<AppState>, value: bool) {
    state.update_settings(|s| s.close_to_tray = value);
}

#[tauri::command]
fn set_open_at_sign_in(value: bool) -> Result<bool, String> {
    autostart::set(value).map_err(|error| error.to_string())?;
    Ok(autostart::enabled())
}

/// Buzz the controller. Returns false when there is no controller to buzz.
#[tauri::command]
fn test_haptics(state: State<AppState>) -> bool {
    state.engine.lock().unwrap().as_ref().is_some_and(Engine::test_haptics)
}

#[tauri::command]
fn open_link(name: String) {
    if let Some((_, url)) = LINKS.iter().find(|(known, _)| *known == name) {
        let _ = Command::new("explorer.exe").arg(url).spawn();
    }
}

#[tauri::command]
fn minimize_window(window: WebviewWindow) {
    let _ = window.minimize();
}

/// The window's close button: to the tray, or quit, as the user chose.
#[tauri::command]
fn close_window(app: AppHandle, window: WebviewWindow, state: State<AppState>) {
    if state.settings.lock().unwrap().close_to_tray {
        let _ = window.hide();
    } else {
        app.exit(0);
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open SenseBridge", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("SenseBridge")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    realtime::boost();
    let start_minimized = std::env::args().any(|arg| arg == autostart::MINIMIZED_FLAG);

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .invoke_handler(tauri::generate_handler![
            snapshot,
            start,
            stop,
            set_strength,
            set_response,
            set_start_on_launch,
            set_close_to_tray,
            set_open_at_sign_in,
            test_haptics,
            open_link,
            minimize_window,
            close_window,
        ])
        .setup(move |app| {
            // A settings.json beside the exe makes the app portable; otherwise settings
            // live in the user's profile.
            let beside_exe = std::env::current_exe()
                .ok()
                .and_then(|exe| Some(exe.parent()?.join("settings.json")))
                .filter(|path| path.is_file());
            let settings_path = match beside_exe {
                Some(path) => path,
                None => app.path().app_config_dir()?.join("settings.json"),
            };
            let settings = Settings::load(&settings_path);
            let start_now = settings.start_on_launch;
            app.manage(AppState {
                engine: Mutex::new(None),
                settings: Mutex::new(settings),
                settings_path,
                last_state: Arc::default(),
            });
            build_tray(app.handle())?;
            if start_now {
                start_engine(app.handle());
            }
            if !start_minimized {
                show_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Alt+F4 and the taskbar's Close take the same route as the close button.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                if state.settings.lock().unwrap().close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("SenseBridge failed to start");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            stop_engine(app); // detach the virtual controller before the process goes
        }
    });
}
