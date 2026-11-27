use std::{
    cell::RefCell,
    env,
    ffi::OsString,
    io,
    mem::{offset_of, size_of},
    ptr::null_mut,
    slice,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use lamzu_hid::{Connection, Monitor, Snapshot, detect_connection};
use windows_sys::Win32::{
    Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID,
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HMODULE, HWND, LPARAM, LRESULT,
        WPARAM,
    },
    System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
    UI::WindowsAndMessaging::*,
};

use crate::{
    card::Contents,
    display::{self, Corner},
    locale::{Language, Text},
    registry, tray,
    wallpaper::{self, Layer},
};

pub const PROJECT_NAME: &str = "LAMZU Battery Widget";
const CONTROLLER: &str = "LamzuWallpaperMonitor";
const MUTEX: &str = "Local\\LamzuWallpaperMonitor";
const UPDATED: u32 = WM_APP + 1;
const RELAYOUT: u32 = WM_APP + 2;
const LANGUAGE_CHANGED: u32 = WM_APP + 3;
const TASKBAR_REBUILT: u32 = WM_APP + 5;
const DISPLAY_TIMER: usize = 1;
const RESCAN_INTERVAL: Duration = Duration::from_secs(45);
const ERROR_RESCAN_INTERVAL: Duration = Duration::from_secs(30);

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

pub fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let text = registry::language().text();
    let Some(command) = Command::parse(&args) else {
        eprintln!("{}", text.usage);
        if !cfg!(debug_assertions) {
            message_box(text.usage, MB_ICONERROR);
        }
        std::process::exit(2);
    };

    let (result, context) = match command {
        Command::Show { demo, language } => {
            if let Some(language) = language
                && let Err(error) = registry::set_language(language)
            {
                report_error(language.text().language_error, &error);
            }
            (
                run(demo, language.is_some()),
                language.map_or(text, Language::text).startup_error,
            )
        }
        Command::Quit => (post_to_controller(WM_CLOSE), text.quit_error),
        Command::AutostartAdd => set_autostart(true, text),
        Command::AutostartRemove => set_autostart(false, text),
        Command::Help => {
            println!("{}", text.usage);
            if !cfg!(debug_assertions) {
                message_box(text.usage, MB_ICONINFORMATION);
            }
            return;
        }
    };
    if let Err(error) = result {
        report_error(context, &error);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Show {
        demo: bool,
        language: Option<Language>,
    },
    Quit,
    AutostartAdd,
    AutostartRemove,
    Help,
}

impl Command {
    fn parse(args: &[OsString]) -> Option<Self> {
        if let [arg] = args {
            match arg.to_str()? {
                "--quit" => return Some(Self::Quit),
                "--autostart" | "-a" => return Some(Self::AutostartAdd),
                "--autostart-remove" | "-arm" => return Some(Self::AutostartRemove),
                "--help" | "-h" => return Some(Self::Help),
                _ => {}
            }
        }

        let mut demo = false;
        let mut language = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.to_str()? {
                "--demo" if !demo => demo = true,
                "--lang" if language.is_none() => {
                    language = Some(Language::parse(args.next()?.to_str()?)?);
                }
                _ => return None,
            }
        }
        Some(Self::Show { demo, language })
    }
}

fn message_box(message: &str, icon: MESSAGEBOX_STYLE) {
    let title = wide(PROJECT_NAME);
    let message = wide(message);
    unsafe { MessageBoxW(null_mut(), message.as_ptr(), title.as_ptr(), MB_OK | icon) };
}

fn report_error(context: &str, error: &io::Error) -> ! {
    show_error(context, error);
    std::process::exit(1);
}

fn show_error(context: &str, error: &io::Error) {
    eprintln!("{PROJECT_NAME}: {context} {error}");
    message_box(&format!("{context}\n\n{error}"), MB_ICONERROR);
}

fn set_autostart(enabled: bool, text: &Text) -> (io::Result<()>, &'static str) {
    if enabled {
        (
            registry::add_autostart(text.autostart_path_too_long),
            text.autostart_add_error,
        )
    } else {
        (registry::remove_autostart(), text.autostart_remove_error)
    }
}

fn post_to_controller(message: u32) -> io::Result<()> {
    let hwnd = unsafe { FindWindowW(wide(CONTROLLER).as_ptr(), null_mut()) };
    if !hwnd.is_null() && unsafe { PostMessageW(hwnd, message, 0, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Singleton(HANDLE);

impl Singleton {
    fn open() -> io::Result<Option<Self>> {
        let handle = unsafe { CreateMutexW(null_mut(), 0, wide(MUTEX).as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let previous = unsafe { GetLastError() };
        let handle = Self(handle);
        if previous == ERROR_ALREADY_EXISTS {
            return Ok(None);
        }
        Ok(Some(handle))
    }
}

impl Drop for Singleton {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn run(demo: bool, language_changed: bool) -> io::Result<()> {
    let Some(_singleton) = Singleton::open()? else {
        if language_changed {
            return post_to_controller(LANGUAGE_CHANGED);
        }
        return Ok(());
    };
    let module = unsafe { GetModuleHandleW(null_mut()) };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }
    wallpaper::register_card_class(module)?;
    let class = wide(CONTROLLER);
    let options = WNDCLASSW {
        lpfnWndProc: Some(controller_proc),
        hInstance: module,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&options) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let language = registry::language();
    let state = WindowState {
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        app: RefCell::new(App {
            module,
            demo,
            language,
            visible: true,
            selected_monitor: registry::monitor(),
            corner: registry::corner(),
            monitors: display::monitors(),
            tray: None,
            menu_open: false,
            contents: if demo {
                Contents::demo(language)
            } else {
                Contents::waiting(language)
            },
            latest: None,
            layer: None,
            layer_error: false,
            poller: None,
        }),
    };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            module,
            (&raw const state).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _controller = Controller(hwnd);
    {
        let mut app = state.app.borrow_mut();
        let mut tray = tray::Tray::new(hwnd);
        tray.ensure_registered();
        app.tray = Some(tray);
        // Account for a language change made before the controller window was available.
        app.refresh_language();
    }

    let _device_notifications = if demo {
        None
    } else {
        let notifications = DeviceNotifications::register(hwnd);
        state.app.borrow_mut().poller = Some(Poller::spawn(hwnd)?);
        notifications
    };
    unsafe { SetTimer(hwnd, DISPLAY_TIMER, 1_000, None) };
    state.app.borrow_mut().draw(true);

    let mut message = MSG::default();
    loop {
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(io::Error::last_os_error()),
            0 => return Ok(()),
            _ => unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

struct Controller(HWND);

impl Drop for Controller {
    fn drop(&mut self) {
        if unsafe { IsWindow(self.0) } != 0 {
            unsafe { DestroyWindow(self.0) };
        }
    }
}

struct DeviceNotifications(HDEVNOTIFY);

impl DeviceNotifications {
    fn register(hwnd: HWND) -> Option<Self> {
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
            dbcc_classguid: GUID_DEVINTERFACE_HID,
            ..Default::default()
        };
        let handle = unsafe {
            RegisterDeviceNotificationW(
                hwnd,
                (&raw const filter).cast(),
                DEVICE_NOTIFY_WINDOW_HANDLE,
            )
        };
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for DeviceNotifications {
    fn drop(&mut self) {
        unsafe { UnregisterDeviceNotification(self.0) };
    }
}

struct WindowState {
    taskbar_created: u32,
    app: RefCell<App>,
}

struct App {
    module: HMODULE,
    demo: bool,
    language: Language,
    visible: bool,
    selected_monitor: Option<String>,
    corner: Corner,
    monitors: Vec<display::Monitor>,
    tray: Option<tray::Tray>,
    menu_open: bool,
    contents: Contents,
    latest: Option<PollResult>,
    layer: Option<Layer>,
    layer_error: bool,
    poller: Option<Poller>,
}

impl App {
    fn relayout(&mut self) {
        self.monitors = display::monitors();
        self.draw(true);
    }

    fn draw(&mut self, changed: bool) {
        if !self.visible {
            return;
        }
        if self.monitors.is_empty() {
            self.monitors = display::monitors();
        }
        let Some(monitor) = display::resolve(&self.monitors, self.selected_monitor.as_deref())
        else {
            self.layer = None;
            return;
        };
        let recreate = !self.layer.as_ref().is_some_and(Layer::is_current);
        if recreate {
            self.layer = None;
            match Layer::open(self.module) {
                Ok(layer) => self.layer = Some(layer),
                Err(error) => {
                    self.log_layer_error(&error);
                    return;
                }
            }
        }
        if let Some(layer) = &mut self.layer {
            match layer.render(&self.contents, monitor, self.corner, recreate || changed) {
                Ok(()) => self.layer_error = false,
                Err(error) => {
                    self.layer = None;
                    self.monitors = display::monitors();
                    self.log_layer_error(&error);
                }
            }
        }
    }

    fn toggle_visibility(&mut self) {
        self.visible = !self.visible;
        if self.visible {
            self.update_display();
        } else {
            self.layer = None;
        }
    }

    fn log_layer_error(&mut self, error: &io::Error) {
        if !self.layer_error {
            eprintln!("{}: {error}", self.language.text().wallpaper_error);
            self.layer_error = true;
        }
    }

    fn request(&self, event: PollRequest) {
        if let Some(poller) = &self.poller {
            poller.request(event);
        }
    }

    fn update(&mut self) {
        let Some(result) = self
            .poller
            .as_ref()
            .and_then(|poller| poller.updates.try_iter().last())
        else {
            return;
        };
        self.latest = Some(result);
        self.update_display();
    }

    fn update_display(&mut self) {
        let contents = self.latest.as_ref().and_then(|result| {
            let contents = Contents::from_poll(
                &result.snapshot,
                result.failed,
                result.time_until_sample(),
                self.language,
            );
            (contents != self.contents).then_some(contents)
        });
        let changed = contents.is_some();
        if let Some(contents) = contents {
            self.contents = contents;
        }
        self.draw(changed);
    }

    fn refresh_language(&mut self) {
        let language = registry::language();
        if language == self.language {
            return;
        }

        self.language = language;
        if self.latest.is_some() {
            self.update_display();
        } else {
            self.contents = if self.demo {
                Contents::demo(language)
            } else {
                Contents::waiting(language)
            };
            self.draw(true);
        }
    }
}

fn open_tray_menu(hwnd: HWND, state: &RefCell<App>) {
    let (language, monitor, corner) = {
        let Ok(mut app) = state.try_borrow_mut() else {
            return;
        };
        if app.menu_open {
            return;
        }
        app.menu_open = true;
        (app.language, app.selected_monitor.clone(), app.corner)
    };
    let autostart = match registry::autostart_enabled() {
        Ok(enabled) => Some(enabled),
        Err(error) => {
            eprintln!("{} {error}", language.text().autostart_read_error);
            None
        }
    };

    // The native menu pumps window messages, so leave App available while it is open.
    let selected = tray::popup(
        hwnd,
        language,
        autostart,
        &display::monitors(),
        monitor.as_deref(),
        corner,
    );
    let error = {
        let mut app = state.borrow_mut();
        app.menu_open = false;
        if unsafe { IsWindow(hwnd) } == 0 {
            return;
        }
        let text = app.language.text();
        let (result, context) = match selected {
            Some(tray::Action::Language(language)) => {
                let result = registry::set_language(language);
                if result.is_ok() {
                    app.refresh_language();
                }
                (result, text.language_error)
            }
            Some(tray::Action::Monitor(monitor)) => {
                let result = registry::set_monitor(monitor.as_deref());
                if result.is_ok() {
                    app.selected_monitor = monitor;
                    app.relayout();
                }
                (result, text.monitor_error)
            }
            Some(tray::Action::Corner(corner)) => {
                let result = registry::set_corner(corner);
                if result.is_ok() {
                    app.corner = corner;
                    app.relayout();
                }
                (result, text.corner_error)
            }
            Some(tray::Action::ToggleAutostart) => {
                let Some(enabled) = autostart else {
                    return;
                };
                set_autostart(!enabled, text)
            }
            Some(tray::Action::Close) => {
                let result = if unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) } != 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                };
                (result, text.quit_error)
            }
            None => return,
        };
        result.err().map(|error| (context, error))
    };
    if let Some((context, error)) = error {
        show_error(context, &error);
    }
}

unsafe extern "system" fn controller_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        return 1;
    }
    if message == WM_NCDESTROY {
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
    }
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
    let state = unsafe { state.as_ref() };
    // Explorer can send this during a Win32 call while the application state is borrowed.
    if state.is_some_and(|state| message != 0 && message == state.taskbar_created) {
        unsafe { PostMessageW(hwnd, TASKBAR_REBUILT, 0, 0) };
        return 0;
    }
    if state.is_some() {
        if matches!(
            message,
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_THEMECHANGED | WM_DPICHANGED
        ) {
            unsafe { PostMessageW(hwnd, RELAYOUT, 0, 0) };
            return 0;
        }
        if message == WM_DEVICECHANGE && wparam as u32 == DBT_DEVNODES_CHANGED {
            unsafe { PostMessageW(hwnd, RELAYOUT, 0, 0) };
        }
    }
    let state = state.map(|state| &state.app);
    if message == WM_DESTROY {
        if let Some(mut app) = state.and_then(|state| state.try_borrow_mut().ok()) {
            if app.menu_open {
                unsafe { EndMenu() };
            }
            if let Some(tray) = &mut app.tray {
                tray.unregister();
            }
        }
        unsafe { PostQuitMessage(0) };
        return 0;
    }

    if message == tray::CALLBACK_MESSAGE {
        if let Some(state) = state {
            match tray::event(lparam) {
                Some(tray::Event::Toggle) => {
                    if let Ok(mut app) = state.try_borrow_mut() {
                        app.toggle_visibility();
                    }
                }
                Some(tray::Event::Menu) => open_tray_menu(hwnd, state),
                None => {}
            }
        }
        return 0;
    }

    if let Some(mut app) = state.and_then(|state| state.try_borrow_mut().ok()) {
        match message {
            UPDATED => app.update(),
            RELAYOUT => app.relayout(),
            LANGUAGE_CHANGED => app.refresh_language(),
            WM_TIMER if wparam == DISPLAY_TIMER => {
                if let Some(tray) = &mut app.tray {
                    tray.ensure_registered();
                }
                app.update_display();
            }
            WM_DEVICECHANGE
                if matches!(
                    wparam as u32,
                    DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE | DBT_DEVNODES_CHANGED
                ) =>
            {
                let request = if wparam as u32 == DBT_DEVICEREMOVECOMPLETE {
                    unsafe { removed_interface(lparam) }
                        .map_or(PollRequest::Rescan, PollRequest::InterfaceRemoved)
                } else {
                    PollRequest::Rescan
                };
                app.request(request);
                return 1;
            }
            WM_POWERBROADCAST
                if matches!(wparam as u32, PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND) =>
            {
                app.request(PollRequest::Rescan);
                return 1;
            }
            TASKBAR_REBUILT => {
                if let Some(tray) = &mut app.tray {
                    tray.taskbar_created();
                }
                app.relayout();
            }
            _ => {
                drop(app);
                return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            }
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

// Windows owns the broadcast buffer for the duration of the window callback.
unsafe fn removed_interface(lparam: LPARAM) -> Option<Vec<u16>> {
    let header = unsafe { (lparam as *const DEV_BROADCAST_HDR).as_ref()? };
    if header.dbch_devicetype != DBT_DEVTYP_DEVICEINTERFACE {
        return None;
    }
    let offset = offset_of!(DEV_BROADCAST_DEVICEINTERFACE_W, dbcc_name);
    let units = (header.dbch_size as usize).checked_sub(offset)? / size_of::<u16>();
    if units == 0 || units > 32_768 {
        return None;
    }
    let name = unsafe { slice::from_raw_parts((lparam as *const u8).add(offset).cast(), units) };
    let end = name.iter().position(|&unit| unit == 0)?;
    Some(name[..end].to_vec())
}

enum PollRequest {
    Rescan,
    InterfaceRemoved(Vec<u16>),
}

struct PollResult {
    snapshot: Snapshot,
    failed: bool,
    next_sample_at: Option<Instant>,
}

impl PollResult {
    fn time_until_sample(&self) -> Option<Duration> {
        self.next_sample_at
            .map(|due| due.saturating_duration_since(Instant::now()))
    }
}

struct Poller {
    requests: Option<Sender<PollRequest>>,
    updates: Receiver<PollResult>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Poller {
    fn spawn(hwnd: HWND) -> io::Result<Self> {
        let (requests, input) = mpsc::channel();
        let (output, updates) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let hwnd = hwnd as usize;
        let thread = thread::Builder::new()
            .name("lamzu-battery".into())
            .spawn(move || {
                let mut monitor = None;
                while !worker_stopping.load(Ordering::Relaxed) {
                    let polled = poll(&mut monitor);
                    let delay = match polled.time_until_sample() {
                        Some(delay) if !polled.failed || !delay.is_zero() => {
                            delay.min(RESCAN_INTERVAL)
                        }
                        _ if polled.failed => ERROR_RESCAN_INTERVAL,
                        _ => RESCAN_INTERVAL,
                    };
                    if output.send(polled).is_err() {
                        break;
                    }
                    unsafe { PostMessageW(hwnd as HWND, UPDATED, 0, 0) };

                    match input.recv_timeout(delay) {
                        Ok(first) => {
                            for request in std::iter::once(first).chain(input.try_iter()) {
                                if let (Some(monitor), PollRequest::InterfaceRemoved(path)) =
                                    (&mut monitor, request)
                                {
                                    monitor.interface_removed(&path);
                                }
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        Ok(Self {
            requests: Some(requests),
            updates,
            stopping,
            thread: Some(thread),
        })
    }

    fn request(&self, event: PollRequest) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(event);
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn poll(monitor: &mut Option<Monitor>) -> PollResult {
    let result = match monitor {
        Some(monitor) => monitor.refresh(),
        None => Monitor::open().and_then(|opened| monitor.insert(opened).refresh_at_startup()),
    };
    let next_sample_at = monitor
        .as_ref()
        .and_then(Monitor::next_sample_in)
        .map(|delay| Instant::now() + delay);
    let (snapshot, failed) = match result {
        Ok(snapshot) => (snapshot, false),
        Err(error) => {
            eprintln!("{}: {error}", registry::language().text().poll_error);
            let snapshot = monitor.as_ref().map_or_else(
                || Snapshot {
                    connection: detect_connection().unwrap_or(Connection::Unavailable),
                    ..Snapshot::default()
                },
                Monitor::snapshot,
            );
            (snapshot, true)
        }
    };
    PollResult {
        snapshot,
        failed,
        next_sample_at,
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Language, OsString};

    fn parse(args: &[&str]) -> Option<Command> {
        Command::parse(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_widget_and_autostart_commands() {
        assert_eq!(
            parse(&[]),
            Some(Command::Show {
                demo: false,
                language: None,
            })
        );
        let russian_demo = Some(Command::Show {
            demo: true,
            language: Some(Language::Russian),
        });
        assert_eq!(parse(&["--demo", "--lang", "rU"]), russian_demo);
        assert_eq!(parse(&["--lang", "RU", "--demo"]), russian_demo);
        assert_eq!(parse(&["--autostart"]), Some(Command::AutostartAdd));
        assert_eq!(parse(&["-a"]), Some(Command::AutostartAdd));
        assert_eq!(
            parse(&["--autostart-remove"]),
            Some(Command::AutostartRemove)
        );
        assert_eq!(parse(&["-arm"]), Some(Command::AutostartRemove));
        assert_eq!(parse(&["--quit"]), Some(Command::Quit));
        assert_eq!(parse(&["--help"]), Some(Command::Help));
        assert_eq!(parse(&["-h"]), Some(Command::Help));
    }

    #[test]
    fn rejects_invalid_languages_and_conflicting_commands() {
        assert_eq!(parse(&["--lang"]), None);
        assert_eq!(parse(&["--lang", "DE"]), None);
        assert_eq!(parse(&["--lang", "EN", "--lang", "RU"]), None);
        assert_eq!(parse(&["--demo", "--demo"]), None);
        assert_eq!(parse(&["--lang", "EN", "--quit"]), None);
        assert_eq!(parse(&["--autostart", "--autostart-remove"]), None);
    }
}
