//! Read-only telemetry for the supported LAMZU USB mouse and receiver on Windows.
//!
//! [`detect_connection`] only inspects Windows' view of the hardware. [`Monitor`]
//! schedules battery reads after successful samples, failures and USB reconnects.

#![cfg(windows)]

mod battery;
mod device;
mod state;

use std::io;
use std::time::{Duration, SystemTime};

use device::Discovery;
use serde::{Deserialize, Serialize};
use state::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    Cable,
    Receiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Absent,
    Cable,
    Receiver,
    /// Several matching interfaces of the preferred transport are present.
    Ambiguous,
    /// Windows did not return a successful inventory.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChargeState {
    Discharging,
    Charging,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatterySample {
    /// Charge level using the manufacturer's voltage calibration when available.
    pub percent: u8,
    pub charge_state: ChargeState,
    pub millivolts: u16,
    pub measured_at: SystemTime,
    pub transport: Transport,
}

impl BatterySample {
    fn calibrated(mut self) -> Self {
        self.percent =
            battery::manufacturer_percent(self.percent, self.millivolts, self.charge_state);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptOutcome {
    Sampled,
    NoResponse,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatteryAttempt {
    pub transport: Transport,
    pub completed_at: SystemTime,
    pub outcome: AttemptOutcome,
}

/// Last evidence of a radio link while the receiver is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCheck {
    Unconfirmed,
    Confirmed(SystemTime),
    NoResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub connection: Connection,
    pub battery: Option<BatterySample>,
    pub link: LinkCheck,
    pub last_attempt: Option<BatteryAttempt>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            connection: Connection::Unavailable,
            battery: None,
            link: LinkCheck::Unconfirmed,
            last_attempt: None,
        }
    }
}

/// A single per-user owner of the battery cache and request interval.
///
/// Call [`Self::refresh`] from a worker thread. It may wait for one HID response.
pub struct Monitor {
    store: Store,
    snapshot: Snapshot,
    pending_commit: Option<(BatteryAttempt, Option<BatterySample>)>,
    receiver_ready: bool,
    selected_path: Option<Vec<u16>>,
    sample_on_connect: bool,
}

impl Monitor {
    /// Open the per-user state. A second owner receives an error until the first drops.
    pub fn open() -> io::Result<Self> {
        Store::open().map(Self::from_store)
    }

    fn from_store(store: Store) -> Self {
        let snapshot = Snapshot {
            battery: store.battery().map(BatterySample::calibrated),
            last_attempt: store.last_attempt(),
            ..Snapshot::default()
        };
        Self {
            store,
            snapshot,
            pending_commit: None,
            receiver_ready: false,
            selected_path: None,
            sample_on_connect: false,
        }
    }

    /// Return the current view without touching the hardware.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
    }

    /// Sample at application startup, or as soon as a supported device is present.
    pub fn refresh_at_startup(&mut self) -> io::Result<Snapshot> {
        self.sample_on_connect = true;
        self.refresh()
    }

    /// Mark the selected HID interface removed. Useful when Windows reports a quick
    /// unplug/replug into the same port before the next inventory can observe it.
    pub fn interface_removed(&mut self, path: &[u16]) {
        if self.is_selected_interface(path) {
            self.forget_selected_device();
        }
    }

    /// Time until another sample is due for the current connection.
    /// `None` means detection or a pending cache write needs a separate wakeup.
    pub fn next_sample_in(&self) -> Option<Duration> {
        if self.pending_commit.is_none()
            && matches!(
                self.snapshot.connection,
                Connection::Cable | Connection::Receiver
            )
        {
            return self.store.remaining(self.sample_on_connect);
        }
        None
    }

    /// Discover the USB transport and sample when the current schedule is due.
    /// An unanswered request updates the snapshot. Other I/O errors are returned with
    /// the current observations still available through [`Self::snapshot`].
    pub fn refresh(&mut self) -> io::Result<Snapshot> {
        let discovery = device::discover();
        self.observe_discovery(&discovery);
        self.commit_attempt()?;

        let Discovery::Found(device) = discovery? else {
            return Ok(self.snapshot);
        };
        let prepare_receiver = device.transport == Transport::Receiver && !self.receiver_ready;
        self.sample_if_due(
            device.transport,
            || device.read_battery(prepare_receiver),
            self.sample_on_connect,
        )
    }

    fn observe_discovery(&mut self, discovery: &io::Result<Discovery>) {
        self.snapshot.connection = discovery
            .as_ref()
            .map(connection_for)
            .unwrap_or(Connection::Unavailable);
        match discovery {
            Ok(Discovery::Absent) => self.forget_selected_device(),
            Ok(Discovery::Found(device)) if !self.is_selected_interface(&device.path) => {
                self.forget_selected_device();
                self.selected_path = Some(device.path.clone());
            }
            _ => {}
        }
        self.update_link();
    }

    fn is_selected_interface(&self, path: &[u16]) -> bool {
        self.selected_path
            .as_deref()
            .is_some_and(|selected| same_path(selected, path))
    }

    fn forget_selected_device(&mut self) {
        if self.selected_path.take().is_some() {
            self.sample_on_connect = true;
            self.receiver_ready = false;
        }
    }

    fn sample_if_due(
        &mut self,
        transport: Transport,
        read_battery: impl FnOnce() -> io::Result<device::Reading>,
        force: bool,
    ) -> io::Result<Snapshot> {
        if !self.store.eligible_now(force) {
            return Ok(self.snapshot);
        }

        self.store.begin(force)?;
        self.sample_on_connect = false;
        let reading = read_battery();
        let completed_at = SystemTime::now();
        let outcome = match &reading {
            Ok(_) => AttemptOutcome::Sampled,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => AttemptOutcome::NoResponse,
            Err(_) => AttemptOutcome::Failed,
        };
        if transport == Transport::Receiver {
            self.receiver_ready = outcome == AttemptOutcome::Sampled;
        }
        let attempt = BatteryAttempt {
            transport,
            completed_at,
            outcome,
        };
        let sample = reading.as_ref().ok().map(|reading| {
            BatterySample {
                percent: reading.raw_percent,
                charge_state: reading.charge_state,
                millivolts: reading.millivolts,
                measured_at: completed_at,
                transport,
            }
            .calibrated()
        });
        self.snapshot.last_attempt = Some(attempt);
        self.snapshot.battery = sample.or(self.snapshot.battery);
        self.update_link();
        self.pending_commit = Some((attempt, sample));
        self.commit_attempt()?;

        if outcome == AttemptOutcome::Failed {
            reading?;
        }
        Ok(self.snapshot)
    }

    fn commit_attempt(&mut self) -> io::Result<()> {
        if let Some((attempt, sample)) = self.pending_commit {
            self.store.finish(attempt, sample)?;
            self.pending_commit = None;
        }
        Ok(())
    }

    fn update_link(&mut self) {
        self.snapshot.link = LinkCheck::Unconfirmed;
        if self.snapshot.connection != Connection::Receiver || self.sample_on_connect {
            return;
        }

        let last_radio_attempt = self
            .snapshot
            .last_attempt
            .filter(|attempt| attempt.transport == Transport::Receiver);
        if let Some(attempt) = last_radio_attempt {
            self.snapshot.link = match attempt.outcome {
                AttemptOutcome::Sampled => LinkCheck::Confirmed(attempt.completed_at),
                AttemptOutcome::NoResponse => LinkCheck::NoResponse,
                AttemptOutcome::Failed => LinkCheck::Unconfirmed,
            };
        } else if let Some(sample) = self
            .snapshot
            .battery
            .filter(|sample| sample.transport == Transport::Receiver)
        {
            self.snapshot.link = LinkCheck::Confirmed(sample.measured_at);
        }
    }
}

fn same_path(left: &[u16], right: &[u16]) -> bool {
    let left = left.strip_suffix(&[0]).unwrap_or(left);
    let right = right.strip_suffix(&[0]).unwrap_or(right);
    let fold_ascii = |unit| match unit {
        0x41..=0x5a => unit + 0x20,
        _ => unit,
    };
    left.iter()
        .copied()
        .map(fold_ascii)
        .eq(right.iter().copied().map(fold_ascii))
}

/// Inspect present USB interfaces without sending a HID report or accessing the cache.
pub fn detect_connection() -> io::Result<Connection> {
    device::discover().map(|discovery| connection_for(&discovery))
}

fn connection_for(discovery: &Discovery) -> Connection {
    match discovery {
        Discovery::Absent => Connection::Absent,
        Discovery::Ambiguous => Connection::Ambiguous,
        Discovery::Found(device) => match device.transport {
            Transport::Cable => Connection::Cable,
            Transport::Receiver => Connection::Receiver,
        },
    }
}
