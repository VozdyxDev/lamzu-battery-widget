use std::{io, mem::size_of, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::{LPARAM, POINT, RECT},
    Graphics::Gdi::{
        DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors,
        GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    },
    UI::{
        Shell::GetScaleFactorForMonitor,
        WindowsAndMessaging::{EDD_GET_DEVICE_INTERFACE_NAME, MONITORINFOF_PRIMARY},
    },
};

use crate::card;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl Corner {
    pub fn code(self) -> &'static str {
        match self {
            Self::TopRight => "top-right",
            Self::TopLeft => "top-left",
            Self::BottomRight => "bottom-right",
            Self::BottomLeft => "bottom-left",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "top-right" => Some(Self::TopRight),
            "top-left" => Some(Self::TopLeft),
            "bottom-right" => Some(Self::BottomRight),
            "bottom-left" => Some(Self::BottomLeft),
            _ => None,
        }
    }
}

pub struct Monitor {
    pub id: String,
    pub label: String,
    pub primary: bool,
    handle: HMONITOR,
}

impl Monitor {
    pub fn work_area(&self) -> io::Result<RECT> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(self.handle, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.rcWork)
    }

    pub fn dpi(&self) -> Option<u32> {
        let mut percent = 0;
        let status = unsafe { GetScaleFactorForMonitor(self.handle, &mut percent) };
        (status >= 0 && percent > 0).then(|| (96 * percent as u32 + 50) / 100)
    }
}

pub fn monitors() -> Vec<Monitor> {
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null_mut(),
            Some(collect_monitor),
            &mut list as *mut Vec<Monitor> as LPARAM,
        );
    }
    list.sort_by(|a, b| a.label.cmp(&b.label));
    list
}

pub fn resolve<'a>(monitors: &'a [Monitor], selected: Option<&str>) -> Option<&'a Monitor> {
    selected
        .and_then(|id| {
            monitors
                .iter()
                .find(|monitor| monitor.id.eq_ignore_ascii_case(id))
        })
        .or_else(|| monitors.iter().find(|monitor| monitor.primary))
        .or_else(|| monitors.first())
}

unsafe extern "system" fn collect_monitor(
    handle: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    list: LPARAM,
) -> i32 {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(handle, (&raw mut info).cast()) } == 0 {
        return 1;
    }

    let source = wide_text(&info.szDevice);
    let display = source.strip_prefix(r"\\.\").unwrap_or(&source);
    let device = active_device(&info.szDevice);
    let id = device
        .as_ref()
        .map(|device| wide_text(&device.DeviceID))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| format!("gdi:{source}"));
    let label = device
        .as_ref()
        .map(|device| wide_text(&device.DeviceString))
        .filter(|name| !name.is_empty())
        .map_or_else(|| display.to_owned(), |name| format!("{display} — {name}"));

    let list = unsafe { &mut *(list as *mut Vec<Monitor>) };
    list.push(Monitor {
        id,
        label,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        handle,
    });
    1
}

fn active_device(source: &[u16]) -> Option<DISPLAY_DEVICEW> {
    for index in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if unsafe {
            EnumDisplayDevicesW(
                source.as_ptr(),
                index,
                &mut device,
                EDD_GET_DEVICE_INTERFACE_NAME,
            )
        } == 0
        {
            return None;
        }
        if device.StateFlags & DISPLAY_DEVICE_ACTIVE != 0 {
            return Some(device);
        }
    }
    None
}

fn wide_text(value: &[u16]) -> String {
    let end = value.iter().position(|&ch| ch == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

pub struct CardLayout {
    pub position: POINT,
    pub width: i32,
    pub height: i32,
}

impl CardLayout {
    pub fn new(work: RECT, corner: Corner, dpi: u32) -> Self {
        let width = card::scale(card::WIDTH, dpi);
        let height = card::scale(card::HEIGHT, dpi);
        let margin = card::scale(24, dpi);
        let right = matches!(corner, Corner::TopRight | Corner::BottomRight);
        let bottom = matches!(corner, Corner::BottomRight | Corner::BottomLeft);
        Self {
            position: POINT {
                x: origin(work.left, work.right, width, margin, right),
                y: origin(work.top, work.bottom, height, margin, bottom),
            },
            width,
            height,
        }
    }
}

fn origin(start: i32, end: i32, size: i32, margin: i32, far_edge: bool) -> i32 {
    let last = (end - size).max(start);
    if far_edge {
        (last - margin).max(start)
    } else {
        (start + margin).min(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_saved_display_or_falls_back_without_discarding_the_choice() {
        let displays = [
            Monitor {
                id: "secondary".into(),
                label: "DISPLAY1".into(),
                primary: false,
                handle: null_mut(),
            },
            Monitor {
                id: "primary".into(),
                label: "DISPLAY2".into(),
                primary: true,
                handle: null_mut(),
            },
        ];
        let selected = Some("SECONDARY");
        assert_eq!(resolve(&displays, None).unwrap().id, "primary");
        assert_eq!(resolve(&displays[1..], selected).unwrap().id, "primary");
        assert_eq!(resolve(&displays, selected).unwrap().id, "secondary");
        assert!(resolve(&[], selected).is_none());
    }

    #[test]
    fn layout_keeps_all_four_corners_inside_the_work_area() {
        let work = RECT {
            left: -1600,
            top: -900,
            right: 0,
            bottom: -48,
        };
        for (corner, x, y) in [
            (Corner::TopRight, -312, -876),
            (Corner::TopLeft, -1576, -876),
            (Corner::BottomRight, -312, -218),
            (Corner::BottomLeft, -1576, -218),
        ] {
            let layout = CardLayout::new(work, corner, 96);
            assert_eq!((layout.position.x, layout.position.y), (x, y));
            assert_eq!((layout.width, layout.height), (288, 146));
            assert_eq!(Corner::parse(corner.code()), Some(corner));
        }
    }

    #[test]
    fn layout_scales_size_and_margin_at_the_target_dpi() {
        let layout = CardLayout::new(
            RECT {
                left: 1920,
                top: 0,
                right: 3840,
                bottom: 1040,
            },
            Corner::BottomRight,
            144,
        );
        assert_eq!((layout.width, layout.height), (432, 219));
        assert_eq!((layout.position.x, layout.position.y), (3372, 785));
    }

    #[test]
    fn layout_reduces_margins_when_the_card_barely_fits() {
        let layout = CardLayout::new(
            RECT {
                left: 10,
                top: 20,
                right: 300,
                bottom: 170,
            },
            Corner::TopLeft,
            96,
        );
        assert_eq!((layout.position.x, layout.position.y), (12, 24));
    }
}
