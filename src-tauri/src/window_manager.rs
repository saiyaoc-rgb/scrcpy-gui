use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    Landscape,
    Portrait,
}

impl Orientation {
    pub fn from_dimensions(width: i32, height: i32) -> Self {
        if width >= height {
            Orientation::Landscape
        } else {
            Orientation::Portrait
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl WindowRect {
    pub fn is_valid(&self) -> bool {
        self.width >= 120 && self.height >= 120
    }

    pub fn orientation(&self) -> Orientation {
        Orientation::from_dimensions(self.width, self.height)
    }

    pub fn clamp_to_work_area(
        self,
        work_left: i32,
        work_top: i32,
        work_right: i32,
        work_bottom: i32,
    ) -> Self {
        let max_w = (work_right - work_left).max(120);
        let max_h = (work_bottom - work_top).max(120);
        let width = self.width.min(max_w);
        let height = self.height.min(max_h);
        let x = self.x.clamp(work_left, (work_right - width).max(work_left));
        let y = self.y.clamp(work_top, (work_bottom - height).max(work_top));
        Self { x, y, width, height }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceWindowLayout {
    pub landscape: Option<WindowRect>,
    pub portrait: Option<WindowRect>,
    pub last_orientation: Option<Orientation>,
}

impl DeviceWindowLayout {
    pub fn get(&self, orientation: Orientation) -> Option<WindowRect> {
        match orientation {
            Orientation::Landscape => self.landscape,
            Orientation::Portrait => self.portrait,
        }
    }

    pub fn set(&mut self, orientation: Orientation, rect: WindowRect) {
        match orientation {
            Orientation::Landscape => self.landscape = Some(rect),
            Orientation::Portrait => self.portrait = Some(rect),
        }
        self.last_orientation = Some(orientation);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowLayoutStore {
    pub enabled: bool,
    pub devices: HashMap<String, DeviceWindowLayout>,
}

pub struct WindowManagerState {
    store: Arc<Mutex<WindowLayoutStore>>,
    active_watchers: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    storage_path: PathBuf,
}

impl WindowManagerState {
    pub fn new() -> Self {
        let storage_path = resolve_portable_storage_path();
        let store = load_store(&storage_path).unwrap_or(WindowLayoutStore {
            enabled: true,
            devices: HashMap::new(),
        });
        Self {
            store: Arc::new(Mutex::new(store)),
            active_watchers: Arc::new(Mutex::new(HashMap::new())),
            storage_path,
        }
    }

    pub fn initial_scrcpy_window_args(&self, device_serial: &str) -> Vec<String> {
        let Ok(guard) = self.store.lock() else {
            return Vec::new();
        };
        if !guard.enabled {
            return Vec::new();
        }
        let Some(layout) = guard.devices.get(device_serial) else {
            return Vec::new();
        };
        let preferred = layout
            .last_orientation
            .and_then(|o| layout.get(o))
            .or(layout.landscape)
            .or(layout.portrait);

        let Some(rect) = preferred.filter(|r| r.is_valid()) else {
            return Vec::new();
        };

        vec![
            format!("--window-x={}", rect.x),
            format!("--window-y={}", rect.y),
            format!("--window-width={}", rect.width),
            format!("--window-height={}", rect.height),
        ]
    }

    pub fn start_tracking_pid(&self, device_serial: String, scrcpy_pid: u32) {
        if let Ok(mut watchers) = self.active_watchers.lock() {
            if let Some(prev) = watchers.remove(&device_serial) {
                prev.store(false, Ordering::SeqCst);
            }
            let running = Arc::new(AtomicBool::new(true));
            watchers.insert(device_serial.clone(), running.clone());

            let store = self.store.clone();
            let storage_path = self.storage_path.clone();

            thread::spawn(move || {
                #[cfg(target_os = "windows")]
                run_windows_tracker(device_serial, scrcpy_pid, running, store, storage_path);

                #[cfg(not(target_os = "windows"))]
                {
                    let _ = (device_serial, scrcpy_pid, running, store, storage_path);
                }
            });
        }
    }

    pub fn stop_tracking(&self, device_serial: &str) {
        if let Ok(mut watchers) = self.active_watchers.lock() {
            if let Some(flag) = watchers.remove(device_serial) {
                flag.store(false, Ordering::SeqCst);
            }
        }
    }
}

fn resolve_portable_storage_path() -> PathBuf {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(dir) = exe_path.parent() {
            return dir.join("window_layouts.json");
        }
    }
    PathBuf::from("window_layouts.json")
}

fn load_store(path: &PathBuf) -> Option<WindowLayoutStore> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_store(path: &PathBuf, store: &WindowLayoutStore) {
    if let Ok(json) = serde_json::to_string_pretty(store) {
        let _ = fs::write(path, json);
    }
}

const POLL_INTERVAL_MS: u64 = 150;
const DRAG_DEBOUNCE_MS: u64 = 400;
const ORIENTATION_SETTLE_MS: u64 = 120;
const RESTORE_COOLDOWN_MS: u64 = 350;

#[tauri::command]
pub fn get_saved_window_layouts(
    state: tauri::State<'_, WindowManagerState>,
) -> Result<WindowLayoutStore, String> {
    let guard = state.store.lock().map_err(|e| e.to_string())?;
    Ok(guard.clone())
}

#[tauri::command]
pub fn set_window_persistence_enabled(
    enabled: bool,
    state: tauri::State<'_, WindowManagerState>,
) -> Result<(), String> {
    let mut guard = state.store.lock().map_err(|e| e.to_string())?;
    guard.enabled = enabled;
    save_store(&state.storage_path, &guard);
    Ok(())
}

#[tauri::command]
pub fn reset_device_window_layout(
    device_serial: String,
    state: tauri::State<'_, WindowManagerState>,
) -> Result<(), String> {
    let mut guard = state.store.lock().map_err(|e| e.to_string())?;
    guard.devices.remove(&device_serial);
    save_store(&state.storage_path, &guard);
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_windows_tracker(
    device_serial: String,
    scrcpy_pid: u32,
    running: Arc<AtomicBool>,
    store: Arc<Mutex<WindowLayoutStore>>,
    storage_path: PathBuf,
) {
    use win32_helpers::{apply_window_rect, find_window_by_pid, get_window_rect, is_window_minimized};

    let mut hwnd: isize = 0;
    let mut last_seen_rect: Option<WindowRect> = None;
    let mut last_orientation: Option<Orientation> = None;
    let mut pending_save_rect: Option<(WindowRect, Instant)> = None;
    let mut restore_cooldown_until: Option<Instant> = None;
    let mut initial_restore_done = false;
    let mut missing_ticks: u32 = 0;

    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));

        if hwnd == 0 {
            if let Some(found) = find_window_by_pid(scrcpy_pid) {
                hwnd = found;
                missing_ticks = 0;
            } else {
                missing_ticks += 1;
                if missing_ticks > 100 {
                    break;
                }
                continue;
            }
        }

        let Some(current_rect) = get_window_rect(hwnd) else {
            break;
        };

        if is_window_minimized(hwnd) || !current_rect.is_valid() {
            continue;
        }

        let persistence_enabled = store.lock().map(|g| g.enabled).unwrap_or(false);
        if !persistence_enabled {
            last_seen_rect = Some(current_rect);
            last_orientation = Some(current_rect.orientation());
            pending_save_rect = None;
            continue;
        }

        let current_orientation = current_rect.orientation();
        let now = Instant::now();

        if !initial_restore_done {
            initial_restore_done = true;
            last_orientation = Some(current_orientation);
            if let Some(saved) = store
                .lock()
                .ok()
                .and_then(|g| g.devices.get(&device_serial).and_then(|l| l.get(current_orientation)))
            {
                if apply_window_rect(hwnd, saved) {
                    last_seen_rect = Some(saved);
                    restore_cooldown_until = Some(now + Duration::from_millis(RESTORE_COOLDOWN_MS));
                    continue;
                }
            }
            last_seen_rect = Some(current_rect);
            continue;
        }

        if let Some(until) = restore_cooldown_until {
            if now < until {
                last_seen_rect = Some(current_rect);
                last_orientation = Some(current_orientation);
                continue;
            }
            restore_cooldown_until = None;
        }

        if last_orientation != Some(current_orientation) {
            pending_save_rect = None;
            last_orientation = Some(current_orientation);

            thread::sleep(Duration::from_millis(ORIENTATION_SETTLE_MS));

            let saved_target = store
                .lock()
                .ok()
                .and_then(|g| g.devices.get(&device_serial).and_then(|l| l.get(current_orientation)));

            if let Some(target_rect) = saved_target {
                if apply_window_rect(hwnd, target_rect) {
                    last_seen_rect = Some(target_rect);
                    restore_cooldown_until =
                        Some(Instant::now() + Duration::from_millis(RESTORE_COOLDOWN_MS));
                    continue;
                }
            }

            if let Some(settled) = get_window_rect(hwnd).filter(|r| r.is_valid()) {
                last_seen_rect = Some(settled);
                if let Ok(mut guard) = store.lock() {
                    guard
                        .devices
                        .entry(device_serial.clone())
                        .or_default()
                        .set(current_orientation, settled);
                    save_store(&storage_path, &guard);
                }
            }
            continue;
        }

        if last_seen_rect != Some(current_rect) {
            last_seen_rect = Some(current_rect);
            pending_save_rect = Some((current_rect, now));
        } else if let Some((rect_to_save, changed_at)) = pending_save_rect {
            if now.duration_since(changed_at) >= Duration::from_millis(DRAG_DEBOUNCE_MS) {
                pending_save_rect = None;
                if let Ok(mut guard) = store.lock() {
                    guard
                        .devices
                        .entry(device_serial.clone())
                        .or_default()
                        .set(rect_to_save.orientation(), rect_to_save);
                    save_store(&storage_path, &guard);
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod win32_helpers {
    use super::WindowRect;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
    };

    struct FindCtx {
        target_pid: u32,
        found_hwnd: HWND,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut FindCtx);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == ctx.target_pid {
            ctx.found_hwnd = hwnd;
            return BOOL(0);
        }
        BOOL(1)
    }

    pub fn find_window_by_pid(pid: u32) -> Option<isize> {
        let mut ctx = FindCtx {
            target_pid: pid,
            found_hwnd: HWND(std::ptr::null_mut()),
        };
        unsafe {
            let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut FindCtx as isize));
        }
        if ctx.found_hwnd.0.is_null() {
            None
        } else {
            Some(ctx.found_hwnd.0 as isize)
        }
    }

    pub fn get_window_rect(hwnd: isize) -> Option<WindowRect> {
        let mut r = RECT::default();
        unsafe { GetWindowRect(HWND(hwnd as *mut _), &mut r).ok()? };
        Some(WindowRect {
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
        })
    }

    pub fn is_window_minimized(hwnd: isize) -> bool {
        unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() }
    }

    pub fn apply_window_rect(hwnd: isize, rect: WindowRect) -> bool {
        let handle = HWND(hwnd as *mut _);
        let clamped = clamp_to_nearest_monitor(handle, rect);
        unsafe {
            SetWindowPos(
                handle,
                None,
                clamped.x,
                clamped.y,
                clamped.width,
                clamped.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .is_ok()
        }
    }

    fn clamp_to_nearest_monitor(hwnd: HWND, rect: WindowRect) -> WindowRect {
        unsafe {
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if hmon.0.is_null() {
                return rect;
            }
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut info).as_bool() {
                let w = info.rcWork;
                return rect.clamp_to_work_area(w.left, w.top, w.right, w.bottom);
            }
        }
        rect
    }
      }
