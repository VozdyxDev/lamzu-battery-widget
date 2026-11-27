use std::{mem, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi},
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
            NIM_SETFOCUS, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
            NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
        },
        WindowsAndMessaging::*,
    },
};

use crate::{
    app::{PROJECT_NAME, wide},
    display::{Corner, Monitor},
    locale::Language,
};

pub const CALLBACK_MESSAGE: u32 = WM_APP + 4;
const ICON_ID: u32 = 1;
const LBW_ICON_RESOURCE: usize = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

const ENGLISH: u32 = 1;
const RUSSIAN: u32 = 2;
const AUTOSTART: u32 = 3;
const CLOSE: u32 = 4;
const TOP_RIGHT: u32 = 5;
const TOP_LEFT: u32 = 6;
const BOTTOM_RIGHT: u32 = 7;
const BOTTOM_LEFT: u32 = 8;
const MAIN_MONITOR: u32 = 9;
const FIRST_MONITOR: u32 = MAIN_MONITOR + 1;

pub enum Event {
    Toggle,
    Menu,
}

pub fn event(lparam: LPARAM) -> Option<Event> {
    // NOTIFYICON_VERSION_4 packs the event and icon ID into LOWORD and HIWORD.
    let packed = lparam as u32;
    if packed >> 16 != ICON_ID {
        return None;
    }
    match packed & 0xffff {
        NIN_SELECT | NIN_KEYSELECT => Some(Event::Toggle),
        WM_CONTEXTMENU => Some(Event::Menu),
        _ => None,
    }
}

pub enum Action {
    Language(Language),
    Monitor(Option<String>),
    Corner(Corner),
    ToggleAutostart,
    Close,
}

pub struct Tray {
    hwnd: HWND,
    registered: bool,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            registered: false,
        }
    }

    pub fn ensure_registered(&mut self) {
        if self.registered {
            return;
        }

        let custom_icon = application_icon();
        let icon = if custom_icon.is_null() {
            unsafe { LoadIconW(null_mut(), IDI_APPLICATION) }
        } else {
            custom_icon
        };
        if icon.is_null() {
            return;
        }
        let tip = wide(PROJECT_NAME);
        let mut tooltip = [0; 128];
        tooltip[..tip.len()].copy_from_slice(&tip);
        let data = NOTIFYICONDATAW {
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: CALLBACK_MESSAGE,
            hIcon: icon,
            szTip: tooltip,
            ..icon_data(self.hwnd)
        };
        // TaskbarCreated is also broadcast on some DPI changes when the icon still exists.
        let added = unsafe {
            Shell_NotifyIconW(NIM_ADD, &data) != 0 || Shell_NotifyIconW(NIM_MODIFY, &data) != 0
        };
        if !custom_icon.is_null() {
            unsafe { DestroyIcon(custom_icon) };
        }
        if added {
            let mut data = icon_data(self.hwnd);
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            self.registered = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) != 0 };
            if !self.registered {
                unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
            }
        }
    }

    pub fn taskbar_created(&mut self) {
        self.registered = false;
        self.ensure_registered();
    }

    pub fn unregister(&mut self) {
        if self.registered {
            unsafe { Shell_NotifyIconW(NIM_DELETE, &icon_data(self.hwnd)) };
            self.registered = false;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.unregister();
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

pub fn popup(
    hwnd: HWND,
    language: Language,
    autostart: Option<bool>,
    monitors: &[Monitor],
    selected_monitor: Option<&str>,
    corner: Corner,
) -> Option<Action> {
    let menu = PopupMenu::new()?;
    let languages = PopupMenu::new()?;
    let text = language.text();

    languages.item(ENGLISH as usize, MF_STRING, "English")?;
    languages.item(RUSSIAN as usize, MF_STRING, "Русский")?;
    let selected = match language {
        Language::English => ENGLISH,
        Language::Russian => RUSSIAN,
    };
    languages.radio(ENGLISH, RUSSIAN, selected)?;
    menu.submenu(languages, text.tray_language)?;

    let monitor_menu = PopupMenu::new()?;
    monitor_menu.item(MAIN_MONITOR as usize, MF_STRING, text.tray_monitor_main)?;
    let mut last_monitor = MAIN_MONITOR;
    let mut checked_monitor = selected_monitor.is_none().then_some(MAIN_MONITOR);
    for monitor in monitors {
        last_monitor += 1;
        let label = monitor.label.replace('&', "&&");
        monitor_menu.item(last_monitor as usize, MF_STRING, &label)?;
        if selected_monitor.is_some_and(|id| monitor.id.eq_ignore_ascii_case(id)) {
            checked_monitor = Some(last_monitor);
        }
    }
    let checked_monitor = match checked_monitor {
        Some(command) => command,
        None => {
            last_monitor += 1;
            monitor_menu.item(
                last_monitor as usize,
                MF_STRING | MF_GRAYED,
                text.tray_monitor_unavailable,
            )?;
            last_monitor
        }
    };
    monitor_menu.radio(MAIN_MONITOR, last_monitor, checked_monitor)?;
    menu.submenu(monitor_menu, text.tray_monitor)?;

    let corners = PopupMenu::new()?;
    for (command, label) in [
        (TOP_RIGHT, text.tray_top_right),
        (TOP_LEFT, text.tray_top_left),
        (BOTTOM_RIGHT, text.tray_bottom_right),
        (BOTTOM_LEFT, text.tray_bottom_left),
    ] {
        corners.item(command as usize, MF_STRING, label)?;
    }
    let selected_corner = match corner {
        Corner::TopRight => TOP_RIGHT,
        Corner::TopLeft => TOP_LEFT,
        Corner::BottomRight => BOTTOM_RIGHT,
        Corner::BottomLeft => BOTTOM_LEFT,
    };
    corners.radio(TOP_RIGHT, BOTTOM_LEFT, selected_corner)?;
    menu.submenu(corners, text.tray_corner)?;

    let autostart_flags = match autostart {
        Some(true) => MF_CHECKED,
        Some(false) => MF_UNCHECKED,
        None => MF_GRAYED,
    };
    menu.item(
        AUTOSTART as usize,
        MF_STRING | autostart_flags,
        text.tray_autostart,
    )?;
    menu.item(0, MF_SEPARATOR, "")?;
    menu.item(CLOSE as usize, MF_STRING, text.tray_close)?;

    let anchor = menu_anchor(hwnd)?;
    let command = unsafe {
        SetForegroundWindow(hwnd);
        let alignment = if GetSystemMetrics(SM_MENUDROPALIGNMENT) == 0 {
            TPM_LEFTALIGN
        } else {
            TPM_RIGHTALIGN
        };
        let command = TrackPopupMenu(
            menu.0,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | alignment,
            anchor.x,
            anchor.y,
            0,
            hwnd,
            null_mut(),
        );
        if IsWindow(hwnd) != 0 {
            PostMessageW(hwnd, WM_NULL, 0, 0);
            Shell_NotifyIconW(NIM_SETFOCUS, &icon_data(hwnd));
        }
        command as u32
    };
    match command {
        ENGLISH => Some(Action::Language(Language::English)),
        RUSSIAN => Some(Action::Language(Language::Russian)),
        MAIN_MONITOR => Some(Action::Monitor(None)),
        TOP_RIGHT => Some(Action::Corner(Corner::TopRight)),
        TOP_LEFT => Some(Action::Corner(Corner::TopLeft)),
        BOTTOM_RIGHT => Some(Action::Corner(Corner::BottomRight)),
        BOTTOM_LEFT => Some(Action::Corner(Corner::BottomLeft)),
        AUTOSTART => Some(Action::ToggleAutostart),
        CLOSE => Some(Action::Close),
        command => {
            let index = command.checked_sub(FIRST_MONITOR)? as usize;
            let monitor = monitors.get(index)?;
            Some(Action::Monitor(Some(monitor.id.clone())))
        }
    }
}

fn menu_anchor(hwnd: HWND) -> Option<POINT> {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    };
    let mut cursor = POINT::default();
    let has_cursor = unsafe { GetCursorPos(&mut cursor) } != 0;
    let mut rect = RECT::default();
    if unsafe { Shell_NotifyIconGetRect(&id, &mut rect) } < 0 {
        return has_cursor.then_some(cursor);
    }
    if has_cursor
        && (rect.left..rect.right).contains(&cursor.x)
        && (rect.top..rect.bottom).contains(&cursor.y)
    {
        Some(cursor)
    } else {
        Some(POINT {
            x: rect.left,
            y: rect.bottom,
        })
    }
}

struct PopupMenu(HMENU);

impl PopupMenu {
    fn new() -> Option<Self> {
        let menu = unsafe { CreatePopupMenu() };
        (!menu.is_null()).then(|| Self(menu))
    }

    fn item(&self, id: usize, flags: MENU_ITEM_FLAGS, label: &str) -> Option<()> {
        let label = wide(label);
        (unsafe { AppendMenuW(self.0, flags, id, label.as_ptr()) } != 0).then_some(())
    }

    fn submenu(&self, submenu: Self, label: &str) -> Option<()> {
        self.item(submenu.0 as usize, MF_POPUP, label)?;
        // The parent menu now owns the submenu and destroys it along with itself.
        mem::forget(submenu);
        Some(())
    }

    fn radio(&self, first: u32, last: u32, checked: u32) -> Option<()> {
        (unsafe { CheckMenuRadioItem(self.0, first, last, checked, MF_BYCOMMAND) } != 0)
            .then_some(())
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        unsafe { DestroyMenu(self.0) };
    }
}

fn application_icon() -> HICON {
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16);
    unsafe {
        LoadImageW(
            GetModuleHandleW(null_mut()),
            LBW_ICON_RESOURCE as *const u16,
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
    }
}
