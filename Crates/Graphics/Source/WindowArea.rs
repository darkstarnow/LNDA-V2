use windows::Win32::Foundation::{E_INVALIDARG, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::BOOL;

pub struct WindowArea;

struct Display {
    name: String,
    area: RECT,
}

impl WindowArea {
    pub fn background() -> bool {
        std::env::var("LEONIDA_BACKGROUND_WINDOW").as_deref() == Ok("1")
    }

    pub fn current() -> windows::core::Result<[i32; 4]> {
        let area = match std::env::var_os("LEONIDA_DISPLAY") {
            Some(name) if !name.is_empty() => {
                let name = name.to_string_lossy();
                let displays = Self::displays()?;
                match Self::named(&displays, &name) {
                    Some(area) => area,
                    None => {
                        tracing::error!(display = %name, "requested display is unavailable; refusing to open a window on another display");
                        return Err(windows::core::Error::from_hresult(E_INVALIDARG));
                    }
                }
            }
            _ => {
                let mut area = RECT::default();
                unsafe {
                    SystemParametersInfoW(
                        SPI_GETWORKAREA,
                        0,
                        Some((&raw mut area).cast()),
                        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                    )?;
                }
                area
            }
        };
        if area.right <= area.left || area.bottom <= area.top {
            return Err(windows::core::Error::from_hresult(E_INVALIDARG));
        }
        Ok([area.left, area.top, area.right, area.bottom])
    }

    fn named(displays: &[Display], name: &str) -> Option<RECT> {
        displays
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
            .map(|d| d.area)
    }

    fn displays() -> windows::core::Result<Vec<Display>> {
        unsafe extern "system" fn visit(
            monitor: HMONITOR,
            _: HDC,
            _: *mut RECT,
            data: LPARAM,
        ) -> BOOL {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if unsafe { GetMonitorInfoW(monitor, &raw mut info.monitorInfo) }.as_bool() {
                let end = info
                    .szDevice
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(info.szDevice.len());
                let displays = unsafe { &mut *(data.0 as *mut Vec<Display>) };
                displays.push(Display {
                    name: String::from_utf16_lossy(&info.szDevice[..end]),
                    area: info.monitorInfo.rcWork,
                });
            }
            true.into()
        }
        let mut displays = Vec::new();
        unsafe {
            EnumDisplayMonitors(None, None, Some(visit), LPARAM(&raw mut displays as isize))
                .ok()?;
        }
        Ok(displays)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_matches_device_names_and_never_substitutes_a_missing_display() {
        let displays = WindowArea::displays().unwrap();
        assert!(!displays.is_empty());
        for display in &displays {
            assert_eq!(
                WindowArea::named(&displays, &display.name.to_ascii_lowercase()),
                Some(display.area)
            );
            assert!(display.area.right > display.area.left);
            assert!(display.area.bottom > display.area.top);
        }
        assert_eq!(
            WindowArea::named(&displays, "missing-display-for-test"),
            None
        );
    }
}
