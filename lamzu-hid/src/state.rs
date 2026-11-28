use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use crate::{AttemptOutcome, BatteryAttempt, BatterySample, ReceiverInterval, Transport};

const SHORT_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);
const CACHE_LIMIT: u64 = 8_192;
const VERSION: u8 = 1;
const STATE_NAME: &str = "battery-state.json";

#[derive(Clone, Copy, Deserialize, Serialize)]
enum Gate {
    Pending,
    CooldownSince(u64),
    RecoverySince(u64),
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct Snapshot {
    version: u8,
    gate: Gate,
    battery: Option<BatterySample>,
    last_attempt: Option<BatteryAttempt>,
}

pub(crate) struct Store {
    path: PathBuf,
    snapshot: Option<Snapshot>,
    receiver_interval: ReceiverInterval,
    _lock: File,
}

impl Store {
    pub(crate) fn open() -> io::Result<Self> {
        let root = env::var_os("LOCALAPPDATA").map(PathBuf::from);
        let root = root.filter(|path| path.is_absolute()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable")
        })?;
        Self::open_at(&root.join("LamzuMonitor"), uptime_ms())
    }

    pub(crate) fn battery(&self) -> Option<BatterySample> {
        self.snapshot.and_then(|snapshot| snapshot.battery)
    }

    pub(crate) fn last_attempt(&self) -> Option<BatteryAttempt> {
        self.snapshot.and_then(|snapshot| snapshot.last_attempt)
    }

    pub(crate) fn set_receiver_interval(&mut self, interval: ReceiverInterval) {
        self.receiver_interval = interval;
    }

    pub(crate) fn eligible_now(&self, force: bool) -> bool {
        self.remaining(force).is_some_and(|time| time.is_zero())
    }

    pub(crate) fn remaining(&self, force: bool) -> Option<Duration> {
        self.remaining_at(uptime_ms(), force)
    }

    pub(crate) fn begin(&mut self, force: bool) -> io::Result<()> {
        self.begin_at(uptime_ms(), force)
    }

    pub(crate) fn finish(
        &mut self,
        attempt: BatteryAttempt,
        new_battery: Option<BatterySample>,
    ) -> io::Result<()> {
        self.finish_at(uptime_ms(), attempt, new_battery)
    }

    fn open_at(root: &Path, now: u64) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("battery-state.lock"))?;
        lock.try_lock()?;
        let path = root.join(STATE_NAME);
        let mut store = Self {
            path,
            snapshot: None,
            receiver_interval: ReceiverInterval::default(),
            _lock: lock,
        };
        let file = match File::open(&store.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(store),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(CACHE_LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() <= CACHE_LIMIT as usize {
            store.snapshot = serde_json::from_slice::<Snapshot>(&bytes)
                .ok()
                .filter(|snapshot| snapshot.version == VERSION)
                .filter(|snapshot| {
                    snapshot
                        .battery
                        .is_none_or(|battery| battery.percent <= 100)
                });
        }
        let reset = match store.snapshot.map(|snapshot| snapshot.gate) {
            Some(Gate::CooldownSince(previous)) => {
                (previous > now).then_some(Gate::CooldownSince(now))
            }
            Some(Gate::RecoverySince(previous)) => {
                (previous > now).then_some(Gate::RecoverySince(now))
            }
            // An interrupted attempt or a corrupt cache starts a short
            // recovery delay for the periodic schedule.
            Some(Gate::Pending) | None => Some(Gate::RecoverySince(now)),
        };
        if let Some(gate) = reset {
            store.save(store.with_gate(gate))?;
        }
        Ok(store)
    }

    fn eligible_at(&self, now: u64, force: bool) -> bool {
        self.remaining_at(now, force)
            .is_some_and(|time| time.is_zero())
    }

    fn remaining_at(&self, now: u64, force: bool) -> Option<Duration> {
        let Some(snapshot) = self.snapshot else {
            return Some(Duration::ZERO);
        };
        let (previous, interval) = match snapshot.gate {
            Gate::Pending => return None,
            _ if force => return Some(Duration::ZERO),
            Gate::RecoverySince(previous) => (previous, SHORT_SAMPLE_INTERVAL),
            Gate::CooldownSince(previous) => match snapshot.last_attempt {
                Some(BatteryAttempt {
                    transport: Transport::Receiver,
                    outcome: AttemptOutcome::Sampled,
                    ..
                }) => (previous, self.receiver_interval.duration()),
                _ => (previous, SHORT_SAMPLE_INTERVAL),
            },
        };
        // Uptime ignores edits to the calendar. If a reboot went undetected,
        // eligibility still requires at least the full interval of new uptime.
        let elapsed = Duration::from_millis(now.saturating_sub(previous));
        Some(interval.saturating_sub(elapsed))
    }

    fn begin_at(&mut self, now: u64, force: bool) -> io::Result<()> {
        if !self.eligible_at(now, force) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "battery query cooldown is active",
            ));
        }
        self.save(self.with_gate(Gate::Pending))
    }

    fn finish_at(
        &mut self,
        now: u64,
        attempt: BatteryAttempt,
        new_battery: Option<BatterySample>,
    ) -> io::Result<()> {
        if !matches!(
            self.snapshot.map(|snapshot| snapshot.gate),
            Some(Gate::Pending)
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no battery query is pending",
            ));
        }
        self.save(Snapshot {
            version: VERSION,
            gate: Gate::CooldownSince(now),
            battery: new_battery.or_else(|| self.battery()),
            last_attempt: Some(attempt),
        })
    }

    fn with_gate(&self, gate: Gate) -> Snapshot {
        Snapshot {
            version: VERSION,
            gate,
            battery: self.battery(),
            last_attempt: self.last_attempt(),
        }
    }

    fn save(&mut self, snapshot: Snapshot) -> io::Result<()> {
        let bytes = serde_json::to_vec(&snapshot).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("tmp");
        // The lock makes this the only writer. Remove a partial temporary
        // file left by a previous process before creating a new one.
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = File::create_new(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, &self.path)?;
        self.snapshot = Some(snapshot);
        Ok(())
    }
}

fn uptime_ms() -> u64 {
    // SAFETY: GetTickCount64 has no arguments or preconditions.
    unsafe { GetTickCount64() }
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    let source: Vec<u16> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: Both buffers are NUL-terminated UTF-16 paths and remain alive
    // throughout the call. The two files are in the same directory.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::device::{Device, Discovery, Reading};
    use crate::{ChargeState, Connection, LinkCheck, Monitor};

    const DEFAULT_RECEIVER_INTERVAL_MS: u64 = 2 * 60 * 60 * 1_000;
    const SHORT_SAMPLE_INTERVAL_MS: u64 = SHORT_SAMPLE_INTERVAL.as_millis() as u64;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            loop {
                let name = format!(
                    "lamzu-monitor-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                );
                let path = env::temp_dir().join(name);
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("temporary directory: {error}"),
                }
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn attempt(outcome: AttemptOutcome, wall_seconds: u64) -> BatteryAttempt {
        BatteryAttempt {
            transport: Transport::Receiver,
            completed_at: UNIX_EPOCH + Duration::from_secs(wall_seconds),
            outcome,
        }
    }

    #[test]
    fn monitor_calibrates_both_legacy_cache_and_new_reports() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 1_000).unwrap();
        let sampled = attempt(AttemptOutcome::Sampled, 10_000);
        store.begin_at(1_000, false).unwrap();
        store
            .finish_at(
                1_000,
                sampled,
                Some(BatterySample {
                    percent: 95,
                    charge_state: ChargeState::Discharging,
                    millivolts: 4113,
                    measured_at: sampled.completed_at,
                    transport: Transport::Receiver,
                }),
            )
            .unwrap();
        drop(store);

        let mut monitor = Monitor::from_store(Store::open_at(&root.0, 1_001).unwrap());
        assert_eq!(monitor.snapshot().battery.unwrap().percent, 100);

        let snapshot = monitor
            .sample_if_due(
                Transport::Cable,
                || {
                    Ok(Reading {
                        raw_percent: 95,
                        charge_state: ChargeState::Charging,
                        millivolts: 4113,
                    })
                },
                true,
            )
            .unwrap();
        assert_eq!(snapshot.battery.unwrap().percent, 99);
        assert_eq!(monitor.store.battery().unwrap().percent, 99);
    }

    #[test]
    fn successful_receiver_sample_waits_two_hours_and_is_preserved_on_failure() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 1_000).unwrap();
        let successful = attempt(AttemptOutcome::Sampled, 10_000);
        let sample = BatterySample {
            percent: 80,
            charge_state: ChargeState::Discharging,
            millivolts: 4_001,
            measured_at: successful.completed_at,
            transport: Transport::Receiver,
        };
        assert!(store.eligible_at(1_000, false));
        store.begin_at(1_000, false).unwrap();
        assert!(!store.eligible_at(u64::MAX, true));
        assert_eq!(store.remaining_at(u64::MAX, true), None);
        store.finish_at(1_500, successful, Some(sample)).unwrap();
        drop(store);

        let due = DEFAULT_RECEIVER_INTERVAL_MS + 1_500;
        let mut restarted = Store::open_at(&root.0, 2_000).unwrap();
        assert_eq!(restarted.battery().unwrap().percent, 80);
        assert!(!restarted.eligible_at(1_500 + SHORT_SAMPLE_INTERVAL_MS, false));
        assert!(!restarted.eligible_at(due - 1, false));
        assert_eq!(
            restarted.remaining_at(due - 1, false),
            Some(Duration::from_millis(1))
        );
        assert!(restarted.begin_at(due - 1, false).is_err());
        restarted.begin_at(due, false).unwrap();

        // An earlier wall time has no effect on the cooldown.
        let failed = attempt(AttemptOutcome::NoResponse, 9_000);
        restarted.finish_at(due, failed, None).unwrap();
        restarted.set_receiver_interval(ReceiverInterval::TenMinutes);
        assert!(!restarted.eligible_at(due + SHORT_SAMPLE_INTERVAL_MS - 1, false));
        assert!(restarted.eligible_at(due + SHORT_SAMPLE_INTERVAL_MS, false));
        drop(restarted);
        let mut restarted = Store::open_at(&root.0, due + SHORT_SAMPLE_INTERVAL_MS).unwrap();
        assert_eq!(restarted.battery().unwrap().measured_at, sample.measured_at);
        assert_eq!(
            restarted.last_attempt().unwrap().completed_at,
            failed.completed_at
        );
        restarted
            .begin_at(due + SHORT_SAMPLE_INTERVAL_MS, false)
            .unwrap();
        let retry_done = due + SHORT_SAMPLE_INTERVAL_MS + 100;
        restarted
            .finish_at(retry_done, attempt(AttemptOutcome::Failed, 9_030), None)
            .unwrap();
        restarted.set_receiver_interval(ReceiverInterval::OneMinute);
        assert!(!restarted.eligible_at(retry_done + SHORT_SAMPLE_INTERVAL_MS - 1, false));
        assert!(restarted.eligible_at(retry_done + SHORT_SAMPLE_INTERVAL_MS, false));
    }

    #[test]
    fn each_receiver_interval_uses_the_same_completion_anchor() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 1_000).unwrap();
        store.begin_at(1_000, false).unwrap();
        let completed = 1_500;
        store
            .finish_at(completed, attempt(AttemptOutcome::Sampled, 10_000), None)
            .unwrap();

        for (interval, seconds) in ReceiverInterval::ALL
            .into_iter()
            .zip([7_200, 3_600, 1_800, 600, 60, 30])
        {
            let duration = Duration::from_secs(seconds);
            assert_eq!(interval.duration(), duration);
            store.set_receiver_interval(interval);
            let due = completed + seconds * 1_000;
            assert_eq!(store.remaining_at(completed, false), Some(duration));
            assert_eq!(
                store.remaining_at(due - 1, false),
                Some(Duration::from_millis(1))
            );
            assert_eq!(store.remaining_at(due, false), Some(Duration::ZERO));
        }
    }

    #[test]
    fn interval_changes_reschedule_existing_cache_without_writing_it() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 1_000).unwrap();
        store.begin_at(1_000, false).unwrap();
        let completed = 1_500;
        let sampled = attempt(AttemptOutcome::Sampled, 10_000);
        store.finish_at(completed, sampled, None).unwrap();
        let state_path = store.path.clone();
        let original_cache = fs::read(&state_path).unwrap();
        drop(store);

        let now = completed + 45 * 60 * 1_000;
        let mut monitor = Monitor::from_store(Store::open_at(&root.0, now).unwrap());
        assert_eq!(monitor.snapshot().last_attempt, Some(sampled));
        assert_eq!(
            monitor.store.remaining_at(now, false),
            Some(Duration::from_secs(75 * 60))
        );

        monitor.set_receiver_interval(ReceiverInterval::ThirtyMinutes);
        assert_eq!(monitor.store.remaining_at(now, false), Some(Duration::ZERO));
        monitor.set_receiver_interval(ReceiverInterval::OneHour);
        assert_eq!(
            monitor.store.remaining_at(now, false),
            Some(Duration::from_secs(15 * 60))
        );
        assert_eq!(fs::read(state_path).unwrap(), original_cache);
    }

    #[test]
    fn successful_cable_sample_waits_thirty_seconds_from_completion() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 1_000).unwrap();
        let successful = BatteryAttempt {
            transport: Transport::Cable,
            ..attempt(AttemptOutcome::Sampled, 10_000)
        };
        store.begin_at(1_000, false).unwrap();
        store.finish_at(1_500, successful, None).unwrap();
        drop(store);

        let mut restarted = Store::open_at(&root.0, 2_000).unwrap();
        restarted.set_receiver_interval(ReceiverInterval::OneHour);
        assert_eq!(restarted.last_attempt(), Some(successful));
        assert_eq!(
            restarted.remaining_at(1_500 + 29_999, false),
            Some(Duration::from_millis(1))
        );
        assert!(restarted.begin_at(1_500 + 29_999, false).is_err());
        restarted.begin_at(1_500 + 30_000, false).unwrap();
    }

    #[test]
    fn interrupted_attempt_blocks_until_reopen_then_recovers() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 1_000).unwrap();
        store.begin_at(1_000, false).unwrap();
        store.set_receiver_interval(ReceiverInterval::OneHour);
        assert_eq!(store.remaining_at(u64::MAX, false), None);
        assert_eq!(store.remaining_at(u64::MAX, true), None);
        assert!(store.begin_at(1_001, true).is_err());
        assert!(Store::open_at(&root.0, 1_001).is_err());
        drop(store);

        let recovery = 2_000 + SHORT_SAMPLE_INTERVAL_MS;
        let mut store = Store::open_at(&root.0, recovery).unwrap();
        store.set_receiver_interval(ReceiverInterval::OneHour);
        assert!(!store.eligible_at(recovery + SHORT_SAMPLE_INTERVAL_MS - 1, false));
        assert!(store.eligible_at(recovery + SHORT_SAMPLE_INTERVAL_MS, false));
        store.begin_at(recovery, true).unwrap();
    }

    #[test]
    fn malformed_state_and_earlier_uptime_each_start_thirty_second_recovery() {
        let root = TestDirectory::new();
        fs::write(root.0.join(STATE_NAME), b"{invalid").unwrap();
        let store = Store::open_at(&root.0, 88_000).unwrap();
        assert!(!store.eligible_at(88_000 + SHORT_SAMPLE_INTERVAL_MS - 1, false));
        assert!(store.eligible_at(88_000 + SHORT_SAMPLE_INTERVAL_MS, false));
        drop(store);

        let rebooted = Store::open_at(&root.0, 1_000).unwrap();
        assert!(!rebooted.eligible_at(1_000 + SHORT_SAMPLE_INTERVAL_MS - 1, false));
        assert!(rebooted.eligible_at(1_000 + SHORT_SAMPLE_INTERVAL_MS, false));
    }

    #[test]
    fn explicit_refresh_bypasses_the_completed_cooldown_after_reopen() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 88_000).unwrap();
        store.begin_at(88_000, false).unwrap();
        store
            .finish_at(88_500, attempt(AttemptOutcome::Sampled, 50), None)
            .unwrap();
        drop(store);

        let mut rebooted = Store::open_at(&root.0, 1_000).unwrap();
        assert!(!rebooted.eligible_at(1_000 + DEFAULT_RECEIVER_INTERVAL_MS - 1, false));
        assert!(rebooted.eligible_at(1_000 + DEFAULT_RECEIVER_INTERVAL_MS, false));
        rebooted.begin_at(1_000, true).unwrap();
        assert!(rebooted.begin_at(1_001, true).is_err());
    }

    #[test]
    fn failed_save_does_not_start_or_clear_an_attempt() {
        let root = TestDirectory::new();
        let mut store = Store::open_at(&root.0, 1_000).unwrap();
        let temporary = store.path.with_extension("tmp");
        fs::create_dir(&temporary).unwrap();
        assert!(store.begin_at(1_000, false).is_err());
        assert!(store.eligible_at(1_000, false));
        fs::remove_dir(&temporary).unwrap();

        store.begin_at(1_000, false).unwrap();
        fs::create_dir(&temporary).unwrap();
        assert!(
            store
                .finish_at(1_500, attempt(AttemptOutcome::Failed, 50), None)
                .is_err()
        );
        assert!(!store.eligible_at(u64::MAX, true));
        fs::remove_dir(&temporary).unwrap();
        let bytes = fs::read(root.0.join(STATE_NAME)).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Snapshot>(&bytes).unwrap().gate,
            Gate::Pending
        ));
    }

    #[test]
    fn monitor_retries_a_recent_timeout_only_on_an_explicit_refresh() {
        let root = TestDirectory::new();
        let count = Cell::new(0);
        let read_timeout = || {
            count.set(count.get() + 1);
            Err(io::Error::from(io::ErrorKind::TimedOut))
        };
        let mut monitor = Monitor::from_store(Store::open_at(&root.0, uptime_ms()).unwrap());
        monitor.snapshot.connection = Connection::Receiver;

        let first = monitor
            .sample_if_due(Transport::Receiver, read_timeout, false)
            .unwrap();
        assert_eq!(first.link, LinkCheck::NoResponse);
        assert_eq!(
            first.last_attempt.unwrap().outcome,
            AttemptOutcome::NoResponse
        );
        assert!(first.battery.is_none());
        assert_eq!(count.get(), 1);

        monitor.snapshot.connection = Connection::Cable;
        monitor
            .sample_if_due(Transport::Cable, read_timeout, false)
            .unwrap();
        assert_eq!(count.get(), 1);
        let after_switch = monitor
            .sample_if_due(Transport::Cable, read_timeout, true)
            .unwrap();
        assert_eq!(
            after_switch.last_attempt.unwrap().transport,
            Transport::Cable
        );
        assert_eq!(count.get(), 2);
        drop(monitor);

        let store = Store::open_at(&root.0, uptime_ms()).unwrap();
        assert_eq!(store.last_attempt(), after_switch.last_attempt);
        let mut restarted = Monitor::from_store(store);
        restarted.snapshot.connection = Connection::Receiver;
        restarted
            .sample_if_due(Transport::Receiver, read_timeout, false)
            .unwrap();
        assert_eq!(count.get(), 2);
        assert_eq!(restarted.snapshot().last_attempt, after_switch.last_attempt);
        restarted
            .sample_if_due(Transport::Receiver, read_timeout, true)
            .unwrap();
        assert_eq!(count.get(), 3);
    }

    #[test]
    fn only_a_selected_interface_change_forces_another_sample() {
        let root = TestDirectory::new();
        let mut monitor = Monitor::from_store(Store::open_at(&root.0, uptime_ms()).unwrap());
        let count = Cell::new(0);
        let found = |path: &str| {
            Ok(Discovery::Found(Device {
                transport: Transport::Receiver,
                path: path.encode_utf16().chain([0]).collect(),
            }))
        };
        let sample = |monitor: &mut Monitor| {
            let read = || {
                count.set(count.get() + 1);
                Ok(Reading {
                    raw_percent: 67,
                    charge_state: ChargeState::Discharging,
                    millivolts: 4_000,
                })
            };
            monitor
                .sample_if_due(Transport::Receiver, read, monitor.sample_on_connect)
                .unwrap();
        };

        monitor.observe_discovery(&found("HID#RECEIVER#PORT_A"));
        sample(&mut monitor);
        assert_eq!(count.get(), 1);

        let unrelated: Vec<u16> = "hid#other#port_a".encode_utf16().collect();
        monitor.interface_removed(&unrelated);
        monitor.observe_discovery(&found("hid#receiver#port_a"));
        assert!(!monitor.sample_on_connect);
        sample(&mut monitor);
        assert_eq!(count.get(), 1);

        monitor.observe_discovery(&Err(io::Error::other("temporary discovery failure")));
        assert_eq!(monitor.snapshot().connection, Connection::Unavailable);
        monitor.observe_discovery(&found("HID#RECEIVER#PORT_A"));
        assert!(!monitor.sample_on_connect);
        sample(&mut monitor);
        assert_eq!(count.get(), 1);

        let removed: Vec<u16> = "hid#receiver#port_a".encode_utf16().collect();
        monitor.interface_removed(&removed);
        assert!(monitor.sample_on_connect);
        assert!(!monitor.receiver_ready);
        monitor.observe_discovery(&found("HID#RECEIVER#PORT_A"));
        assert_eq!(monitor.next_sample_in(), Some(Duration::ZERO));
        sample(&mut monitor);
        assert_eq!(count.get(), 2);
        assert!(!monitor.sample_on_connect);

        monitor.observe_discovery(&found("HID#RECEIVER#PORT_B"));
        assert!(monitor.sample_on_connect);
        sample(&mut monitor);
        assert_eq!(count.get(), 3);
    }

    #[test]
    fn recovering_a_cache_commit_does_not_query_the_mouse_again() {
        let root = TestDirectory::new();
        let store = Store::open_at(&root.0, uptime_ms()).unwrap();
        let temporary = store.path.with_extension("tmp");
        let mut monitor = Monitor::from_store(store);
        let read_timeout = || {
            fs::create_dir(&temporary).unwrap();
            Err(io::Error::from(io::ErrorKind::TimedOut))
        };
        assert!(
            monitor
                .sample_if_due(Transport::Receiver, read_timeout, false)
                .is_err()
        );
        let attempt = monitor.snapshot().last_attempt.unwrap();
        assert_eq!(attempt.outcome, AttemptOutcome::NoResponse);
        assert!(monitor.store.last_attempt().is_none());

        fs::remove_dir(&temporary).unwrap();
        monitor.commit_attempt().unwrap();
        assert_eq!(monitor.store.last_attempt(), Some(attempt));
        monitor
            .sample_if_due(Transport::Receiver, || panic!("unexpected query"), false)
            .unwrap();
    }
}
