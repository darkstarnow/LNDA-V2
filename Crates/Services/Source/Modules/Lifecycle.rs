#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, HMENU};
use windows_sys::core::{PCSTR, PCWSTR, w};

use crate::{ServiceModule, service_export};

type Routine = extern "system" fn(quiesced: u8, context: *mut c_void);

static ROUTINES: Mutex<Vec<(usize, Routine, usize)>> = Mutex::new(Vec::new());

fn routines() -> MutexGuard<'static, Vec<(usize, Routine, usize)>> {
    ROUTINES.lock().unwrap_or_else(|e| e.into_inner())
}

const SUSPEND_DEADLINE: Duration = Duration::from_secs(1);

const ERROR_INVALID_PARAMETER: u32 = 87;

pub extern "system" fn RegisterAppStateChangeNotification(
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    let Some(routine) = routine else {
        return ERROR_INVALID_PARAMETER;
    };
    if registration.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    routines().push((id, routine, context as usize));
    unsafe { registration.write(id) };
    0
}

pub extern "system" fn UnregisterAppStateChangeNotification(registration: usize) {
    routines().retain(|&(id, ..)| id != registration);
}

const TITLE_PROC: PCWSTR = w!("leonida.wndproc");
const TITLE_LAYOUT: PCWSTR = w!("leonida.layout");
const TITLE_MOVING: PCWSTR = w!("leonida.moving");

pub extern "system" fn CreateWindowExW(
    ex_style: u32,
    class: PCWSTR,
    name: PCWSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (x, y, width, height) = fit(parent, x, y, width, height);
    let hwnd = unsafe {
        wm::CreateWindowExW(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    hook(hwnd);
    track_layout(hwnd, parent, width, height);
    hwnd
}

pub extern "system" fn CreateWindowExA(
    ex_style: u32,
    class: PCSTR,
    name: PCSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (x, y, width, height) = fit(parent, x, y, width, height);
    let hwnd = unsafe {
        wm::CreateWindowExA(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    hook(hwnd);
    track_layout(hwnd, parent, width, height);
    hwnd
}

fn fit(parent: HWND, x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
    let mut area = RECT::default();
    let known =
        unsafe { wm::SystemParametersInfoW(wm::SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0) };
    if parent.is_null() && known != 0 && width >= 640 && height >= 360 {
        return WindowLayout::load()
            .unwrap_or(WindowLayout {
                x: area.left + (area.right - area.left - 1280) / 2,
                y: area.top + (area.bottom - area.top - 720) / 2,
                width: 1280,
                height: 720,
            })
            .fit(&area)
            .tuple();
    }
    let fitted = fit_into(&area, width, height);
    if !parent.is_null() || known == 0 || fitted == (width, height) {
        return (x, y, width, height);
    }
    tracing::info!(width, height, to = ?fitted, "title window fitted to the screen");
    let (w, h) = fitted;
    let left = area.left + (area.right - area.left - w) / 2;
    let top = area.top + (area.bottom - area.top - h) / 2;
    (left, top, w, h)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowLayout {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl WindowLayout {
    fn parse(text: &str) -> Option<Self> {
        let values: Vec<i32> = text
            .split_ascii_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        let [x, y, width, height] = *values.as_slice() else {
            return None;
        };
        (width > 0 && height > 0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }

    fn fit(self, area: &RECT) -> Self {
        let (width, height) = fit_into(area, self.width, self.height);
        Self {
            x: self
                .x
                .clamp(area.left, area.right.saturating_sub(width).max(area.left)),
            y: self
                .y
                .clamp(area.top, area.bottom.saturating_sub(height).max(area.top)),
            width,
            height,
        }
    }

    fn tuple(self) -> (i32, i32, i32, i32) {
        (self.x, self.y, self.width, self.height)
    }

    fn path() -> Option<std::path::PathBuf> {
        leonida_core::Config::load()
            .ok()?
            .data_subdir("Window")
            .ok()
            .map(|p| p.join("Placement.txt"))
    }

    fn load() -> Option<Self> {
        Self::parse(&std::fs::read_to_string(Self::path()?).ok()?)
    }

    fn save(hwnd: HWND) {
        let stored = unsafe { wm::GetPropW(hwnd, TITLE_LAYOUT) }.cast::<Self>();
        if stored.is_null() {
            return;
        }
        let mut rect = RECT::default();
        if unsafe { wm::GetWindowRect(hwnd, &raw mut rect) } == 0 {
            return;
        }
        let layout = Self {
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        };
        unsafe {
            stored.write(layout);
        }
        if let Some(path) = Self::path()
            && let Err(error) = std::fs::write(
                path,
                format!(
                    "{} {} {} {}",
                    layout.x, layout.y, layout.width, layout.height
                ),
            )
        {
            tracing::warn!(%error, "could not save window placement");
        }
    }

    fn constrain(hwnd: HWND, position: *mut wm::WINDOWPOS) {
        unsafe {
            if !wm::GetPropW(hwnd, TITLE_MOVING).is_null() || wm::IsIconic(hwnd) != 0 {
                return;
            }
            let stored = wm::GetPropW(hwnd, TITLE_LAYOUT).cast::<Self>();
            if stored.is_null() || position.is_null() {
                return;
            }
            let layout = *stored;
            let position = &mut *position;
            if position.flags & wm::SWP_NOSIZE == 0 {
                position.cx = layout.width;
                position.cy = layout.height;
            }
            if position.flags & wm::SWP_NOMOVE == 0 {
                position.x = layout.x;
                position.y = layout.y;
            }
        }
    }
}

fn track_layout(hwnd: HWND, parent: HWND, width: i32, height: i32) {
    if !hwnd.is_null() && parent.is_null() && width >= 640 && height >= 360 {
        let mut rect = RECT::default();
        unsafe {
            if wm::GetWindowRect(hwnd, &raw mut rect) == 0 {
                return;
            }
            let layout = Box::new(WindowLayout {
                x: rect.left,
                y: rect.top,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
            });
            let stored = Box::into_raw(layout);
            if wm::SetPropW(hwnd, TITLE_LAYOUT, stored.cast()) == 0 {
                drop(Box::from_raw(stored));
            }
        }
    }
}

fn fit_into(area: &RECT, width: i32, height: i32) -> (i32, i32) {
    let (aw, ah) = (area.right - area.left, area.bottom - area.top);
    if width <= 0 || height <= 0 || aw <= 0 || ah <= 0 || (width <= aw && height <= ah) {
        return (width, height);
    }
    let scale = f64::min(
        f64::from(aw) / f64::from(width),
        f64::from(ah) / f64::from(height),
    );
    (
        (f64::from(width) * scale) as i32,
        (f64::from(height) * scale) as i32,
    )
}

fn hook(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let title = wm::GetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC);
        wm::SetPropW(hwnd, TITLE_PROC, title as *mut c_void);
        wm::SetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC, window_proc as *const () as isize);
    }
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == wm::WM_ENTERSIZEMOVE {
        unsafe {
            wm::SetPropW(hwnd, TITLE_MOVING, 1usize as *mut c_void);
        }
    }
    if message == wm::WM_WINDOWPOSCHANGING {
        WindowLayout::constrain(hwnd, lparam as *mut wm::WINDOWPOS);
    }
    if message == wm::WM_EXITSIZEMOVE {
        WindowLayout::save(hwnd);
        unsafe {
            wm::RemovePropW(hwnd, TITLE_MOVING);
        }
    }
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_CLOSE as usize {
        WindowLayout::save(hwnd);
        end_title("window closed", Box::new(|| {}));
        return 0;
    }
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_KEYMENU as usize {
        return 0;
    }
    unsafe {
        let title = wm::GetPropW(hwnd, TITLE_PROC);
        let system_layout = message == wm::WM_SYSCOMMAND
            && matches!(wparam & 0xFFF0, command if command == wm::SC_MAXIMIZE as usize || command == wm::SC_MINIMIZE as usize || command == wm::SC_RESTORE as usize);
        if system_layout {
            wm::SetPropW(hwnd, TITLE_MOVING, 1usize as *mut c_void);
        }
        if message == wm::WM_NCDESTROY {
            wm::RemovePropW(hwnd, TITLE_PROC);
            let stored = wm::RemovePropW(hwnd, TITLE_LAYOUT).cast::<WindowLayout>();
            if !stored.is_null() {
                drop(Box::from_raw(stored));
            }
            wm::RemovePropW(hwnd, TITLE_MOVING);
        }
        let title = std::mem::transmute::<*mut c_void, wm::WNDPROC>(title);
        let result = wm::CallWindowProcW(title, hwnd, message, wparam, lparam);
        if system_layout {
            wm::RemovePropW(hwnd, TITLE_MOVING);
        }
        result
    }
}

pub(crate) fn end_title(why: &'static str, then: Box<dyn FnOnce() + Send>) {
    static ENDING: AtomicBool = AtomicBool::new(false);
    if ENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let suspend = routines().clone();
    tracing::info!(routines = suspend.len(), "{why}: suspending the title");
    let then = std::sync::Arc::new(Mutex::new(Some(then)));
    let finish = move |how: &str| {
        let mut then = then.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!("{how}; ending the title");
        if let Some(then) = then.take() {
            then();
        }
        end();
    };
    let late = finish.clone();
    std::thread::spawn(move || {
        for (_, routine, context) in suspend {
            routine(1, context as *mut c_void);
        }
        finish("title suspended");
    });
    std::thread::spawn(move || {
        std::thread::sleep(SUSPEND_DEADLINE);
        late("title did not finish suspending in time");
    });
}

const RELAUNCHED_FROM: &str = "LEONIDA_RELAUNCHED_FROM";

pub(crate) fn relaunch(args: Vec<std::ffi::OsString>) {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    let mut me: HANDLE = std::ptr::null_mut();
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut me,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        command.args(&args);
        if duplicated {
            command.env(RELAUNCHED_FROM, (me as usize).to_string());
        }
        command.spawn()
    });
    match spawned {
        Ok(child) => tracing::info!(pid = child.id(), ?args, "relaunched"),
        Err(e) => tracing::warn!(%e, "relaunch failed; the title just ends"),
    }
}

pub fn wait_for_previous() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let Some(handle) = std::env::var(RELAUNCHED_FROM)
        .ok()
        .and_then(|h| h.parse::<usize>().ok())
    else {
        return;
    };
    unsafe { std::env::remove_var(RELAUNCHED_FROM) };
    let handle = handle as HANDLE;
    unsafe {
        WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
    }
}

fn end() {
    #[cfg(windows)]
    leonida_graphics::pipeline::PipelineCache::save();
    unsafe { TerminateProcess(GetCurrentProcess(), 0) };
}

pub(crate) fn module() -> ServiceModule {
    ServiceModule {
        name: "lifecycle",
        dlls: &[
            "api-ms-win-core-psm-appnotify-*",
            "ext-ms-win-rtcore-ntuser-window-*",
        ],
        exports: vec![
            service_export!(RegisterAppStateChangeNotification),
            service_export!(UnregisterAppStateChangeNotification),
            service_export!(CreateWindowExW),
            service_export!(CreateWindowExA),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_layout_restores_size_and_stays_on_the_primary_work_area() {
        let area = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let saved = WindowLayout::parse("120 80 1280 720\n").unwrap();
        assert_eq!(saved.fit(&area), saved);
        let elsewhere = WindowLayout::parse("-1920 1500 1280 720").unwrap();
        assert_eq!(elsewhere.fit(&area).tuple(), (0, 320, 1280, 720));
        let spanning = WindowLayout::parse("0 0 3840 2160").unwrap().fit(&area);
        assert_eq!(spanning.tuple(), (0, 0, 1848, 1040));
        let extreme = WindowLayout::parse("2147483647 -2147483648 1280 720")
            .unwrap()
            .fit(&area);
        assert_eq!(extreme.tuple(), (640, 0, 1280, 720));
        for text in [
            "",
            "0 0 0 720",
            "0 0 1280 -1",
            "0 0 1280 720 extra",
            "a 0 1280 720",
        ] {
            assert!(WindowLayout::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn a_later_title_resize_preserves_the_initial_window_layout() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("layout test"),
            0,
            0,
            0,
            3840,
            2160,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        let mut before = RECT::default();
        let mut after = RECT::default();
        unsafe {
            assert_ne!(wm::GetWindowRect(hwnd, &raw mut before), 0);
            wm::SetWindowPos(
                hwnd,
                null,
                -1000,
                -1000,
                3840,
                2160,
                wm::SWP_NOACTIVATE | wm::SWP_NOZORDER,
            );
            assert_ne!(wm::GetWindowRect(hwnd, &raw mut after), 0);
            assert_eq!(
                (after.left, after.top, after.right, after.bottom),
                (before.left, before.top, before.right, before.bottom)
            );
            wm::SendMessageW(hwnd, wm::WM_ENTERSIZEMOVE, 0, 0);
            wm::SetWindowPos(
                hwnd,
                null,
                120,
                80,
                640,
                360,
                wm::SWP_NOACTIVATE | wm::SWP_NOZORDER,
            );
            assert_ne!(wm::GetWindowRect(hwnd, &raw mut after), 0);
            assert_eq!(
                (
                    after.left,
                    after.top,
                    after.right - after.left,
                    after.bottom - after.top
                ),
                (120, 80, 640, 360)
            );
            wm::DestroyWindow(hwnd);
        }
    }

    extern "system" fn noop(_: u8, _: *mut c_void) {}

    #[test]
    fn registrations_come_and_go() {
        let (mut a, mut b) = (0, 0);
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut a),
            0
        );
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut b),
            0
        );
        UnregisterAppStateChangeNotification(a);
        let left: Vec<_> = routines().iter().map(|r| r.0).collect();
        assert_eq!(left, [b]);
    }

    #[test]
    fn oversized_windows_shrink_to_the_screen() {
        let area = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(fit_into(&area, 3840, 2160), (1848, 1040));
        assert_eq!(fit_into(&area, 1280, 720), (1280, 720));
        assert_eq!(
            fit_into(&area, wm::CW_USEDEFAULT, 0),
            (wm::CW_USEDEFAULT, 0)
        );
    }

    #[test]
    fn hooked_windows_still_reach_the_title_procedure() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("before"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        let mut text = [0u16; 16];
        unsafe {
            wm::SetWindowTextW(hwnd, w!("after"));
            let n = wm::GetWindowTextW(hwnd, text.as_mut_ptr(), 16);
            assert_eq!(String::from_utf16_lossy(&text[..n as usize]), "after");
            assert!(!wm::GetPropW(hwnd, TITLE_PROC).is_null());
            wm::DestroyWindow(hwnd);
        }
    }

    thread_local! {
        static COMMANDS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    extern "system" fn recording(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == wm::WM_SYSCOMMAND {
            COMMANDS.with(|c| c.borrow_mut().push(wparam & 0xFFF0));
            return 0;
        }
        unsafe { wm::DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    #[test]
    fn the_keyboard_menu_never_reaches_the_title() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("menu"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        unsafe {
            let title = wm::GetPropW(hwnd, TITLE_PROC);
            wm::SetPropW(hwnd, TITLE_PROC, recording as *const () as *mut c_void);
            let command = |c: u32, key: u8| {
                wm::SendMessageW(hwnd, wm::WM_SYSCOMMAND, c as usize, LPARAM::from(key))
            };
            command(wm::SC_KEYMENU, 0);
            command(wm::SC_KEYMENU, b' ');
            command(wm::SC_KEYMENU | 1, 0);
            command(wm::SC_MINIMIZE, 0);
            wm::SetPropW(hwnd, TITLE_PROC, title);
            wm::DestroyWindow(hwnd);
        }
        let seen = COMMANDS.with(|c| c.borrow().clone());
        assert_eq!(seen, [wm::SC_MINIMIZE as usize]);
    }
}
