use std::{
    cell::RefCell,
    env, io,
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
    wallpaper::{self, Layer},
};

pub const PROJECT_NAME: &str = "LAMZU Battery Widget";
const CONTROLLER: &str = "LamzuWallpaperMonitor";
const MUTEX: &str = "Local\\LamzuWallpaperMonitor";
const UPDATED: u32 = WM_APP + 1;
const RELAYOUT: u32 = WM_APP + 2;
const DISPLAY_TIMER: usize = 1;
const RESCAN_INTERVAL: Duration = Duration::from_secs(45);
const ERROR_RESCAN_INTERVAL: Duration = Duration::from_secs(30);

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

pub fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [] => run(false),
        [arg] if arg == "--demo" => run(true),
        [arg] if arg == "--quit" => quit(),
        [arg] if arg == "--help" || arg == "-h" => {
            println!("lamzu-widget [--demo | --quit]");
            Ok(())
        }
        _ => {
            eprintln!("Использование: lamzu-widget [--demo | --quit]");
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("{PROJECT_NAME}: {error}");
        let title = wide(PROJECT_NAME);
        let message = wide(&format!("Не удалось запустить виджет.\n\n{error}"));
        unsafe {
            MessageBoxW(
                null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            )
        };
        std::process::exit(1);
    }
}

fn quit() -> io::Result<()> {
    let hwnd = unsafe { FindWindowW(wide(CONTROLLER).as_ptr(), null_mut()) };
    if !hwnd.is_null() && unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) } == 0 {
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

fn run(demo: bool) -> io::Result<()> {
    let Some(_singleton) = Singleton::open()? else {
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
    let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    let state = RefCell::new(App {
        module,
        taskbar_created,
        contents: if demo {
            Contents::demo()
        } else {
            Contents::waiting()
        },
        latest: None,
        layer: None,
        layer_error: false,
        poller: None,
    });
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

    let _device_notifications = if demo {
        None
    } else {
        let notifications = DeviceNotifications::register(hwnd);
        state.borrow_mut().poller = Some(Poller::spawn(hwnd)?);
        notifications
    };
    unsafe { SetTimer(hwnd, DISPLAY_TIMER, 1_000, None) };
    state.borrow_mut().draw(true);

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

struct App {
    module: HMODULE,
    taskbar_created: u32,
    contents: Contents,
    latest: Option<PollResult>,
    layer: Option<Layer>,
    layer_error: bool,
    poller: Option<Poller>,
}

impl App {
    fn draw(&mut self, changed: bool) {
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
        if (recreate || changed)
            && let Some(layer) = &mut self.layer
        {
            match layer.render(&self.contents) {
                Ok(()) => self.layer_error = false,
                Err(error) => {
                    self.layer = None;
                    self.log_layer_error(&error);
                }
            }
        }
    }

    fn log_layer_error(&mut self, error: &io::Error) {
        if !self.layer_error {
            eprintln!("Слой обоев: {error}");
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
            let contents =
                Contents::from_poll(&result.snapshot, result.failed, result.time_until_sample());
            (contents != self.contents).then_some(contents)
        });
        let changed = contents.is_some();
        if let Some(contents) = contents {
            self.contents = contents;
        }
        self.draw(changed);
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
    if message == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return 0;
    }

    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
    if let Some(state) = unsafe { state.as_ref() }
        && let Ok(mut app) = state.try_borrow_mut()
    {
        match message {
            UPDATED => app.update(),
            RELAYOUT => app.draw(true),
            WM_TIMER if wparam == DISPLAY_TIMER => app.update_display(),
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
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_THEMECHANGED | WM_DPICHANGED => {
                unsafe { PostMessageW(hwnd, RELAYOUT, 0, 0) };
            }
            message if message == app.taskbar_created && message != 0 => {
                unsafe { PostMessageW(hwnd, RELAYOUT, 0, 0) };
            }
            _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
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
            eprintln!("Проверка мыши: {error}");
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
