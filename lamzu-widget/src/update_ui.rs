use std::{
    io,
    mem::size_of,
    ptr::null_mut,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::HWND,
    UI::{
        Controls::{
            TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDF_ALLOW_DIALOG_CANCELLATION, TaskDialogIndirect,
        },
        WindowsAndMessaging::{
            IDCANCEL, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_YESNO, MessageBoxW, PostMessageW,
            WM_APP,
        },
    },
};

use crate::{
    app::{PROJECT_NAME, wide},
    locale::{Language, Text},
    update::{self, Check, Release},
    update_apply,
};

pub const VERSION: &str = env!("LAMZU_WIDGET_VERSION");
pub const FINISHED: u32 = WM_APP + 6;
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckMode {
    Automatic,
    Manual,
}

pub enum Event {
    Checked(CheckMode, io::Result<Check>),
    NotificationActivated(Release),
    Prepared(io::Result<()>),
}

#[derive(Default, PartialEq, Eq)]
enum Stage {
    #[default]
    Idle,
    Checking(CheckMode),
    Confirming,
    Downloading,
}

#[derive(Default)]
enum AutomaticChecks {
    #[default]
    AtStartup,
    Scheduled(Instant),
    Paused,
}

#[derive(Default)]
pub struct Updates {
    stage: Stage,
    result: Option<Receiver<Event>>,
    automatic_checks: AutomaticChecks,
    notified_release: Option<Release>,
    notification_clicked: bool,
}

impl Updates {
    pub fn menu_label(&self, text: &Text) -> Option<&'static str> {
        match self.stage {
            Stage::Idle => None,
            Stage::Checking(_) | Stage::Confirming => Some(text.tray_checking_updates),
            Stage::Downloading => Some(text.tray_downloading_update),
        }
    }

    pub fn check(&mut self, hwnd: HWND, mode: CheckMode) -> io::Result<()> {
        if !self.schedule_check(Instant::now(), mode) {
            return Ok(());
        }
        self.start(hwnd, Stage::Checking(mode), move || {
            Event::Checked(mode, update::check(VERSION))
        })
    }

    fn schedule_check(&mut self, now: Instant, mode: CheckMode) -> bool {
        if self.stage != Stage::Idle {
            return false;
        }

        match self.automatic_checks {
            AutomaticChecks::Paused => mode == CheckMode::Manual,
            AutomaticChecks::Scheduled(due) if mode == CheckMode::Automatic && now < due => false,
            _ => {
                self.automatic_checks = AutomaticChecks::Scheduled(now + CHECK_INTERVAL);
                true
            }
        }
    }

    pub fn download(
        &mut self,
        hwnd: HWND,
        release: Release,
        language: Language,
        demo: bool,
    ) -> io::Result<()> {
        self.start(hwnd, Stage::Downloading, move || {
            Event::Prepared(update_apply::prepare_and_start(&release, language, demo))
        })
    }

    fn start(
        &mut self,
        hwnd: HWND,
        stage: Stage,
        job: impl FnOnce() -> Event + Send + 'static,
    ) -> io::Result<()> {
        let (sender, receiver) = mpsc::channel();
        let hwnd = hwnd as usize;
        thread::Builder::new()
            .name("lamzu-update".into())
            .spawn(move || {
                if sender.send(job()).is_ok() {
                    unsafe { PostMessageW(hwnd as HWND, FINISHED, 0, 0) };
                }
            })?;
        self.result = Some(receiver);
        self.stage = stage;
        Ok(())
    }

    pub fn notification_sent(&mut self, release: Release) {
        self.notified_release = Some(release);
        self.automatic_checks = AutomaticChecks::Paused;
    }

    pub fn activate_notification(&mut self) {
        self.notification_clicked = self.notified_release.is_some();
    }

    pub fn next_event(&mut self) -> Option<Event> {
        if self.stage == Stage::Idle && std::mem::take(&mut self.notification_clicked) {
            let release = self.notified_release.take()?;
            self.stage = Stage::Confirming;
            return Some(Event::NotificationActivated(release));
        }

        let event = match self.result.as_ref()?.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                let error = io::Error::other("The update worker exited unexpectedly.");
                match self.stage {
                    Stage::Idle | Stage::Confirming => return None,
                    Stage::Checking(mode) => Event::Checked(mode, Err(error)),
                    Stage::Downloading => Event::Prepared(Err(error)),
                }
            }
        };
        self.result = None;
        if matches!(
            &event,
            Event::Checked(CheckMode::Manual, Ok(Check::Available(_)))
                | Event::Checked(_, Ok(Check::Current | Check::NoRelease))
        ) {
            self.notified_release = None;
            self.notification_clicked = false;
        }
        Some(event)
    }

    pub fn reset(&mut self) {
        self.stage = Stage::Idle;
        self.result = None;
    }
}

pub fn confirm(hwnd: HWND, text: &Text, release: &Release) -> bool {
    const INSTALL_BUTTON: i32 = 100;
    let title = wide(PROJECT_NAME);
    let heading = wide(text.update_available);
    let details = format!(
        "{} · {}\n{} · {}\n\n{}",
        text.update_current_version,
        VERSION,
        text.update_new_version,
        release.version,
        text.update_description,
    );
    let content = wide(&details);
    let install = wide(text.update_install_button);
    let cancel = wide(text.update_cancel_button);
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: INSTALL_BUTTON,
            pszButtonText: install.as_ptr(),
        },
        TASKDIALOG_BUTTON {
            nButtonID: IDCANCEL,
            pszButtonText: cancel.as_ptr(),
        },
    ];
    let dialog = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: hwnd,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        pszWindowTitle: title.as_ptr(),
        pszMainInstruction: heading.as_ptr(),
        pszContent: content.as_ptr(),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: IDCANCEL,
        ..Default::default()
    };
    let mut selected = IDCANCEL;
    if unsafe { TaskDialogIndirect(&dialog, &mut selected, null_mut(), null_mut()) } >= 0 {
        return selected == INSTALL_BUTTON;
    }

    let message = wide(&format!("{}\n\n{details}", text.update_available));
    unsafe {
        MessageBoxW(
            hwnd,
            message.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_checks_run_at_launch_and_after_twenty_four_hours() {
        let mut updates = Updates::default();
        let launch = Instant::now();
        let due = launch + Duration::from_secs(86_400);

        assert!(updates.schedule_check(launch, CheckMode::Automatic));
        updates.reset();
        assert!(!updates.schedule_check(due - Duration::from_secs(1), CheckMode::Automatic));
        assert!(updates.schedule_check(due, CheckMode::Automatic));
    }

    #[test]
    fn manual_checks_run_early_and_reschedule_the_automatic_check() {
        let mut updates = Updates::default();
        let launch = Instant::now();
        let manual = launch + Duration::from_secs(60);

        assert!(updates.schedule_check(launch, CheckMode::Automatic));
        assert!(updates.schedule_check(manual, CheckMode::Manual));
        assert!(!updates.schedule_check(launch + CHECK_INTERVAL, CheckMode::Automatic));
        assert!(updates.schedule_check(manual + CHECK_INTERVAL, CheckMode::Automatic));
    }

    #[test]
    fn due_checks_wait_until_pending_update_work_finishes() {
        let mut updates = Updates::default();
        let launch = Instant::now();
        assert!(updates.schedule_check(launch, CheckMode::Automatic));
        let due = launch + CHECK_INTERVAL;

        updates.stage = Stage::Checking(CheckMode::Automatic);
        assert!(!updates.schedule_check(due, CheckMode::Manual));
        updates.stage = Stage::Downloading;
        assert!(!updates.schedule_check(due, CheckMode::Automatic));
        updates.reset();
        assert!(updates.schedule_check(due, CheckMode::Automatic));
    }

    #[test]
    fn a_paused_schedule_stays_paused_after_manual_checks_until_relaunch() {
        let mut updates = Updates::default();
        let launch = Instant::now();
        assert!(updates.schedule_check(launch, CheckMode::Automatic));

        updates.automatic_checks = AutomaticChecks::Paused;
        updates.reset();
        let due = launch + CHECK_INTERVAL;
        assert!(!updates.schedule_check(due, CheckMode::Automatic));
        assert!(updates.schedule_check(due, CheckMode::Manual));
        updates.reset();
        assert!(!updates.schedule_check(due + CHECK_INTERVAL, CheckMode::Automatic));
        assert!(Updates::default().schedule_check(due, CheckMode::Automatic));
    }
}
