use std::{io, ptr::null_mut};

use windows_sys::Win32::{
    Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::MapWindowPoints,
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_PER_MONITOR_AWARE,
            GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetWindowDpiAwarenessContext,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::*,
    },
};

use crate::{
    app::{PROJECT_NAME, wide},
    card,
    composition::Composition,
    display::{CardLayout, Corner, Monitor},
};

const SPAWN_WORKER: u32 = 0x052C;
const CARD_CLASS: &str = "LamzuWallpaperCard";

struct Desktop {
    parent: HWND,
    icons: HWND,
    worker: HWND,
}

impl Desktop {
    fn is_raised(&self) -> bool {
        !self.icons.is_null()
    }

    fn discover() -> io::Result<Self> {
        let progman = unsafe { GetShellWindow() };
        if progman.is_null() {
            return Err(io::Error::other("Explorer desktop is unavailable"));
        }

        // This Explorer message creates the wallpaper worker and makes the
        // icon layer transparent on builds using the raised desktop layout.
        let mut response = 0;
        let result = unsafe {
            SendMessageTimeoutW(
                progman,
                SPAWN_WORKER,
                0x0D,
                1,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                700,
                &mut response,
            )
        };
        let message_error = (result == 0).then(io::Error::last_os_error);

        let raised = unsafe { GetWindowLongPtrW(progman, GWL_EXSTYLE) }
            & WS_EX_NOREDIRECTIONBITMAP as isize
            != 0;
        if raised {
            let icons = find_child(progman, null_mut(), "SHELLDLL_DefView");
            let worker = find_child(progman, null_mut(), "WorkerW");
            if !icons.is_null() && !worker.is_null() {
                return Ok(Self {
                    parent: progman,
                    icons,
                    worker,
                });
            }
        } else {
            let mut search = ClassicSearch {
                shell_pid: 0,
                worker: null_mut(),
            };
            unsafe {
                GetWindowThreadProcessId(progman, &mut search.shell_pid);
                EnumWindows(
                    Some(find_classic_worker),
                    &mut search as *mut ClassicSearch as LPARAM,
                );
            }
            if !search.worker.is_null() {
                return Ok(Self {
                    parent: search.worker,
                    icons: null_mut(),
                    worker: search.worker,
                });
            }
        }
        Err(message_error
            .unwrap_or_else(|| io::Error::other("Explorer wallpaper layer is unavailable")))
    }
}

struct ClassicSearch {
    shell_pid: u32,
    worker: HWND,
}

unsafe extern "system" fn find_classic_worker(hwnd: HWND, search: LPARAM) -> i32 {
    let search = unsafe { &mut *(search as *mut ClassicSearch) };
    if find_child(hwnd, null_mut(), "SHELLDLL_DefView").is_null() {
        return 1;
    }
    let worker = find_child(null_mut(), hwnd, "WorkerW");
    let mut worker_pid = 0;
    if !worker.is_null()
        && unsafe { GetWindowThreadProcessId(worker, &mut worker_pid) } != 0
        && worker_pid == search.shell_pid
    {
        search.worker = worker;
        return 0;
    }
    1
}

fn find_child(parent: HWND, after: HWND, class: &str) -> HWND {
    unsafe { FindWindowExW(parent, after, wide(class).as_ptr(), null_mut()) }
}

struct Context(DPI_AWARENESS_CONTEXT);

impl Context {
    fn match_window(hwnd: HWND) -> Self {
        let context = unsafe { GetWindowDpiAwarenessContext(hwnd) };
        Self(unsafe { SetThreadDpiAwarenessContext(context) })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

pub fn register_card_class(module: HMODULE) -> io::Result<()> {
    let class = wide(CARD_CLASS);
    let options = WNDCLASSW {
        lpfnWndProc: Some(card_proc),
        hInstance: module,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&options) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

unsafe extern "system" fn card_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_ERASEBKGND => 1,
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

pub struct Layer {
    desktop: Desktop,
    card: HWND,
    composition: Option<Composition>,
}

impl Layer {
    pub fn open(module: HMODULE) -> io::Result<Self> {
        let desktop = Desktop::discover()?;
        let _context = Context::match_window(desktop.parent);
        let class = wide(CARD_CLASS);
        let name = wide(PROJECT_NAME);
        // Progman's raised desktop does not compose GDI/layered child surfaces.
        let surface_style = if desktop.is_raised() {
            WS_EX_NOREDIRECTIONBITMAP
        } else {
            WS_EX_LAYERED
        };
        let card = unsafe {
            CreateWindowExW(
                surface_style | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_NOPARENTNOTIFY,
                class.as_ptr(),
                name.as_ptr(),
                WS_CHILD | WS_DISABLED,
                0,
                0,
                0,
                0,
                desktop.parent,
                null_mut(),
                module,
                null_mut(),
            )
        };
        if card.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut layer = Self {
            desktop,
            card,
            composition: None,
        };
        if layer.desktop.is_raised() {
            layer.composition = Some(Composition::new(card)?);
        }
        Ok(layer)
    }

    pub fn is_current(&self) -> bool {
        unsafe {
            GetParent(self.card) == self.desktop.parent
                && IsWindow(self.desktop.worker) != 0
                && (!self.desktop.is_raised()
                    || (IsWindow(self.desktop.icons) != 0
                        && GetWindow(self.card, GW_HWNDPREV) == self.desktop.icons
                        && precedes(self.card, self.desktop.worker)))
        }
    }

    pub fn render(
        &mut self,
        data: &card::Contents,
        monitor: &Monitor,
        corner: Corner,
        changed: bool,
    ) -> io::Result<()> {
        let _context = Context::match_window(self.desktop.parent);
        let awareness = unsafe {
            GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(self.desktop.parent))
        };
        // A DPI-virtualized Explorer parent expects sizes in its own coordinate space.
        let monitor_dpi = if awareness == DPI_AWARENESS_PER_MONITOR_AWARE {
            monitor.dpi()
        } else {
            None
        };
        let dpi = monitor_dpi.unwrap_or_else(|| unsafe { GetDpiForWindow(self.card) }.max(96));
        let CardLayout {
            mut position,
            width,
            height,
        } = CardLayout::new(monitor.work_area()?, corner, dpi, data.height());
        // A secondary display's scale can change while the tray window stays on the primary.
        let mut current = RECT::default();
        if !changed
            && unsafe { GetWindowRect(self.card, &mut current) } != 0
            && (current.left, current.top, current.right, current.bottom)
                == (
                    position.x,
                    position.y,
                    position.x + width,
                    position.y + height,
                )
        {
            return Ok(());
        }
        unsafe { MapWindowPoints(null_mut(), self.desktop.parent, &mut position, 1) };

        let frame = card::paint(data, width, height, dpi)?;
        if self.desktop.is_raised() && !precedes(self.desktop.icons, self.desktop.worker) {
            // Explorer can raise its opaque wallpaper worker after a desktop transition.
            if unsafe {
                SetWindowPos(
                    self.desktop.worker,
                    HWND_BOTTOM,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if unsafe {
            SetWindowPos(
                self.card,
                self.desktop.icons,
                position.x,
                position.y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        match &mut self.composition {
            Some(composition) => composition.present(frame.pixels(), width, height),
            None => frame.present_layered(self.card),
        }
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        drop(self.composition.take());
        unsafe { DestroyWindow(self.card) };
    }
}

fn precedes(mut window: HWND, other: HWND) -> bool {
    while !window.is_null() {
        window = unsafe { GetWindow(window, GW_HWNDNEXT) };
        if window == other {
            return true;
        }
    }
    false
}
