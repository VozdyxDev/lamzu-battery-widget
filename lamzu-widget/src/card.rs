use std::{
    ffi::c_void,
    io,
    ptr::null_mut,
    slice,
    time::{Duration, SystemTime},
};

use lamzu_hid::{ChargeState, Connection, LinkCheck, Snapshot, Transport};
use windows_sys::Win32::{
    Foundation::{COLORREF, HWND, POINT, RECT, SIZE},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow},
};

use crate::{app::wide, locale::Language};

pub const WIDTH: i32 = 288;
const BASE_HEIGHT: i32 = 146;
const DETAIL_ROW_HEIGHT: i32 = 20;

const SURFACE: COLORREF = rgb(22, 29, 39);
const OUTLINE: COLORREF = rgb(65, 80, 98);
const TRACK: COLORREF = rgb(58, 71, 89);
const TEXT: COLORREF = rgb(243, 247, 252);
const MUTED: COLORREF = rgb(178, 196, 216);
const GREEN: COLORREF = rgb(104, 222, 165);
const BLUE: COLORREF = rgb(123, 195, 252);
const AMBER: COLORREF = rgb(251, 192, 104);
const YELLOW: COLORREF = rgb(255, 218, 92);
const TRANSPARENT_PIXEL: u32 = 0x00ff00ff;
const STATUS_DOT: (i32, i32, i32) = (15, 17, 12);

const fn rgb(red: u32, green: u32, blue: u32) -> COLORREF {
    red | (green << 8) | (blue << 16)
}

pub fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

#[derive(PartialEq, Eq)]
pub struct Contents {
    language: Language,
    status: &'static str,
    status_color: COLORREF,
    percent: Option<u8>,
    charge_state: Option<ChargeState>,
    millivolts: Option<u16>,
    voltage_visible: bool,
    no_battery: &'static str,
    last_check: String,
    next_check: String,
}

impl Contents {
    pub fn waiting(language: Language) -> Self {
        let text = language.text();
        Self {
            language,
            status: text.waiting,
            status_color: MUTED,
            percent: None,
            charge_state: None,
            millivolts: None,
            voltage_visible: true,
            no_battery: text.first_query,
            last_check: check_line(language, text.last_query, None),
            next_check: next_check_line(language, None),
        }
    }

    pub fn demo(language: Language) -> Self {
        let text = language.text();
        Self {
            language,
            status: text.demo,
            status_color: GREEN,
            percent: Some(78),
            charge_state: Some(ChargeState::Charging),
            millivolts: Some(4_012),
            voltage_visible: true,
            no_battery: "",
            last_check: check_line(language, text.last_query, Some(Duration::from_secs(13))),
            next_check: next_check_line(language, Some(Duration::from_secs(17))),
        }
    }

    pub fn from_poll(
        snapshot: &Snapshot,
        failed: bool,
        until_next: Option<Duration>,
        language: Language,
    ) -> Self {
        Self::from_poll_at(snapshot, failed, until_next, SystemTime::now(), language)
    }

    fn from_poll_at(
        snapshot: &Snapshot,
        failed: bool,
        until_next: Option<Duration>,
        now: SystemTime,
        language: Language,
    ) -> Self {
        let text = language.text();
        let (status, status_color) = match snapshot.connection {
            Connection::Absent => (text.absent, MUTED),
            Connection::Cable => (text.cable, GREEN),
            Connection::Receiver => match snapshot.link {
                LinkCheck::Confirmed(_) => (text.receiver_confirmed, GREEN),
                LinkCheck::NoResponse => (text.receiver_no_response, AMBER),
                _ => (text.receiver, BLUE),
            },
            Connection::Ambiguous => (text.ambiguous, AMBER),
            Connection::Unavailable => (text.unavailable, AMBER),
        };

        let transport = match (snapshot.connection, snapshot.link) {
            (Connection::Cable, _) => Some(Transport::Cable),
            (Connection::Receiver, LinkCheck::Confirmed(_)) => Some(Transport::Receiver),
            _ => None,
        };
        let visible_battery = snapshot
            .battery
            .filter(|sample| Some(sample.transport) == transport);
        let since_last = snapshot
            .last_attempt
            .and_then(|attempt| now.duration_since(attempt.completed_at).ok());
        Self {
            language,
            status,
            status_color,
            percent: visible_battery.map(|sample| sample.percent),
            charge_state: visible_battery.map(|sample| sample.charge_state),
            millivolts: visible_battery
                .map(|sample| sample.millivolts)
                .filter(|&value| value != 0),
            voltage_visible: true,
            no_battery: if visible_battery.is_some() {
                ""
            } else if failed {
                text.query_failed
            } else {
                text.battery_unknown
            },
            last_check: check_line(language, text.last_query, since_last),
            next_check: next_check_line(language, until_next),
        }
    }

    pub fn set_voltage_visible(&mut self, visible: bool) {
        self.voltage_visible = visible;
    }

    pub fn height(&self) -> i32 {
        BASE_HEIGHT + self.voltage_row_height()
    }

    fn voltage_row_height(&self) -> i32 {
        if self.voltage_visible {
            DETAIL_ROW_HEIGHT
        } else {
            0
        }
    }
}

fn voltage_line(language: Language, millivolts: Option<u16>) -> String {
    let text = language.text();
    let Some(millivolts) = millivolts else {
        return format!("{} · —", text.voltage);
    };
    let separator = match language {
        Language::English => '.',
        Language::Russian => ',',
    };
    let units = millivolts / 1_000;
    let fraction = millivolts % 1_000;
    format!(
        "{} · {units}{separator}{fraction:03} {}",
        text.voltage, text.volts
    )
}

fn next_check_line(language: Language, remaining: Option<Duration>) -> String {
    let text = language.text();
    if let Some(delay) = remaining.filter(|delay| delay.as_secs() < 60) {
        let seconds = delay.as_secs() + u64::from(delay.subsec_nanos() != 0);
        format!("{} · {seconds} {}", text.next_query, text.seconds)
    } else {
        check_line(language, text.next_query, remaining)
    }
}

fn check_line(language: Language, label: &str, duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return format!("{label} · —");
    };
    let text = language.text();
    let seconds = duration.as_secs();
    if seconds < 60 {
        return format!("{label} · {seconds} {}", text.seconds);
    }
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let minutes = minutes % 60;
    if hours == 0 {
        format!("{label} · {minutes} {}", text.minutes)
    } else {
        format!(
            "{label} · {hours} {} {minutes} {}",
            text.hours, text.minutes
        )
    }
}

pub fn paint(data: &Contents, width: i32, height: i32, dpi: u32) -> io::Result<Canvas> {
    let mut canvas = Canvas::new(width, height)?;
    canvas.fill(TRANSPARENT_PIXEL);

    let text = data.language.text();
    let s = |value| scale(value, dpi);
    canvas.round(0, 0, width, height, s(16), OUTLINE);
    canvas.round(s(1), s(1), width - s(1), height - s(1), s(15), SURFACE);
    let charging = data.charge_state == Some(ChargeState::Charging);
    if charging {
        let bolt = [(23, 14), (15, 24), (20, 24), (18, 32), (27, 21), (22, 21)]
            .map(|(x, y)| POINT { x: s(x), y: s(y) });
        canvas.polygon(&bolt, YELLOW);
    }
    canvas.text(
        text.title,
        row(34, 12, 272, 34, dpi),
        s(16),
        FW_SEMIBOLD,
        TEXT,
    );
    canvas.text(
        data.status,
        row(34, 33, 272, 53, dpi),
        s(13),
        FW_NORMAL,
        MUTED,
    );

    if let Some(percent) = data.percent {
        canvas.text(
            &format!("{percent}%"),
            row(16, 56, 101, 92, dpi),
            s(30),
            FW_SEMIBOLD,
            TEXT,
        );
        let charge_label = match data.charge_state {
            Some(ChargeState::Charging) => Some((text.charging, YELLOW)),
            Some(ChargeState::Full) => Some((text.fully_charged, MUTED)),
            _ => None,
        };
        let gauge_top = if let Some((label, color)) = charge_label {
            canvas.text(label, row(108, 55, 272, 73, dpi), s(12), FW_SEMIBOLD, color);
            76
        } else {
            69
        };
        let gauge_start = s(108);
        let gauge_end = s(272);
        canvas.round(
            gauge_start,
            s(gauge_top),
            gauge_end,
            s(gauge_top + 10),
            s(5),
            TRACK,
        );
        if percent != 0 {
            let fill_end =
                gauge_start + (gauge_end - gauge_start) * i32::from(percent.min(100)) / 100;
            let fill = if charging {
                YELLOW
            } else if percent <= 20 {
                AMBER
            } else {
                GREEN
            };
            canvas.round(
                gauge_start,
                s(gauge_top),
                fill_end.max(s(110)),
                s(gauge_top + 10),
                s(5),
                fill,
            );
        }
    } else {
        canvas.text(
            data.no_battery,
            row(16, 56, 272, 90, dpi),
            s(16),
            FW_SEMIBOLD,
            MUTED,
        );
    }
    let mut details_top = 94;
    if data.voltage_visible {
        canvas.detail_line(
            &voltage_line(data.language, data.millivolts),
            details_top,
            dpi,
        );
        details_top += DETAIL_ROW_HEIGHT;
    }
    canvas.detail_line(&data.last_check, details_top, dpi);
    canvas.detail_line(&data.next_check, details_top + DETAIL_ROW_HEIGHT, dpi);
    canvas.finalize_pixels();
    if !charging {
        let (left, top, diameter) = STATUS_DOT;
        canvas.dot(s(left), s(top), s(diameter), data.status_color);
    }
    Ok(canvas)
}

fn row(left: i32, top: i32, right: i32, bottom: i32, dpi: u32) -> RECT {
    RECT {
        left: scale(left, dpi),
        top: scale(top, dpi),
        right: scale(right, dpi),
        bottom: scale(bottom, dpi),
    }
}

pub struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u32,
    width: i32,
    height: i32,
}

impl Canvas {
    fn new(width: i32, height: i32) -> io::Result<Self> {
        let dc = unsafe { CreateCompatibleDC(null_mut()) };
        if dc.is_null() {
            return Err(io::Error::last_os_error());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels: *mut c_void = null_mut();
        let bitmap =
            unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0) };
        if bitmap.is_null() {
            let error = io::Error::last_os_error();
            unsafe { DeleteDC(dc) };
            return Err(error);
        }
        let previous = unsafe { SelectObject(dc, bitmap) };
        Ok(Self {
            dc,
            bitmap,
            previous,
            pixels: pixels.cast(),
            width,
            height,
        })
    }

    fn fill(&self, value: u32) {
        unsafe { slice::from_raw_parts_mut(self.pixels, (self.width * self.height) as usize) }
            .fill(value);
    }

    fn round(&self, left: i32, top: i32, right: i32, bottom: i32, curve: i32, color: COLORREF) {
        self.shape(color, || unsafe {
            RoundRect(self.dc, left, top, right, bottom, curve, curve);
        });
    }

    fn polygon(&self, points: &[POINT], color: COLORREF) {
        self.shape(color, || unsafe {
            Polygon(self.dc, points.as_ptr(), points.len() as i32);
        });
    }

    fn shape(&self, color: COLORREF, draw: impl FnOnce()) {
        unsafe {
            let brush = CreateSolidBrush(color);
            let previous_brush = SelectObject(self.dc, brush);
            let previous_pen = SelectObject(self.dc, GetStockObject(NULL_PEN));
            draw();
            SelectObject(self.dc, previous_pen);
            SelectObject(self.dc, previous_brush);
            DeleteObject(brush);
        }
    }

    fn text(&self, value: &str, mut bounds: RECT, size: i32, weight: FONT_WEIGHT, color: COLORREF) {
        let face = wide("Segoe UI");
        let text = wide(value);
        unsafe {
            let font = CreateFontW(
                -size,
                0,
                0,
                0,
                weight as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                DEFAULT_PITCH as u32,
                face.as_ptr(),
            );
            let previous_font = SelectObject(self.dc, font);
            SetBkMode(self.dc, TRANSPARENT as i32);
            SetTextColor(self.dc, color);
            DrawTextW(
                self.dc,
                text.as_ptr(),
                -1,
                &mut bounds,
                DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(self.dc, previous_font);
            DeleteObject(font);
        }
    }

    fn detail_line(&self, value: &str, top: i32, dpi: u32) {
        self.text(
            value,
            row(16, top, 272, top + DETAIL_ROW_HEIGHT, dpi),
            scale(12, dpi),
            FW_NORMAL,
            MUTED,
        );
    }

    fn finalize_pixels(&mut self) {
        unsafe { GdiFlush() };
        for pixel in
            unsafe { slice::from_raw_parts_mut(self.pixels, (self.width * self.height) as usize) }
        {
            *pixel = if *pixel & 0x00ffffff == TRANSPARENT_PIXEL {
                0
            } else {
                *pixel | 0xff000000
            };
        }
    }

    fn dot(&mut self, left: i32, top: i32, diameter: i32, color: COLORREF) {
        const SUBPIXELS: i32 = 4;
        const SAMPLES: u32 = (SUBPIXELS * SUBPIXELS) as u32;
        let pixels =
            unsafe { slice::from_raw_parts_mut(self.pixels, (self.width * self.height) as usize) };
        // Work in doubled subpixel coordinates so every sample is centered and
        // opposite pixels have exactly the same coverage. The card beneath is opaque.
        let radius = diameter * SUBPIXELS;
        let radius_squared = radius * radius;
        for dy in 0..diameter {
            for dx in 0..diameter {
                let mut coverage = 0;
                for sub_y in 0..SUBPIXELS {
                    let y = 2 * (dy * SUBPIXELS + sub_y) + 1 - radius;
                    for sub_x in 0..SUBPIXELS {
                        let x = 2 * (dx * SUBPIXELS + sub_x) + 1 - radius;
                        coverage += u32::from(x * x + y * y <= radius_squared);
                    }
                }

                let pixel = &mut pixels[((top + dy) * self.width + left + dx) as usize];
                let mut blended = *pixel & 0xff000000;
                for (dib_shift, color_shift) in [(0, 16), (8, 8), (16, 0)] {
                    let background = (*pixel >> dib_shift) & 0xff;
                    let foreground = (color >> color_shift) & 0xff;
                    let channel =
                        (background * (SAMPLES - coverage) + foreground * coverage + SAMPLES / 2)
                            / SAMPLES;
                    blended |= channel << dib_shift;
                }
                *pixel = blended;
            }
        }
    }

    pub fn pixels(&self) -> &[u32] {
        // The 32-bit DIB has one u32 per pixel with no row padding.
        unsafe { slice::from_raw_parts(self.pixels, (self.width * self.height) as usize) }
    }

    pub fn present_layered(&self, hwnd: HWND) -> io::Result<()> {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let origin = POINT { x: 0, y: 0 };
        if unsafe {
            UpdateLayeredWindow(
                hwnd,
                null_mut(),
                null_mut(),
                &size,
                self.dc,
                &origin,
                0,
                &blend,
                ULW_ALPHA,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use lamzu_hid::{AttemptOutcome, BatteryAttempt, BatterySample};

    use super::*;

    #[test]
    fn status_dot_has_symmetric_antialiased_edges() {
        let contents = Contents::waiting(Language::Russian);
        let canvas = paint(&contents, WIDTH, contents.height(), 96).unwrap();
        let (left, top, diameter) = STATUS_DOT;
        let pixel = |x, y| canvas.pixels()[((top + y) * WIDTH + left + x) as usize];
        let background = pixel(0, 0);
        let foreground = pixel(diameter / 2, diameter / 2);
        assert_ne!(background, foreground);

        let mut blended_edge = false;
        for y in 0..diameter {
            for x in 0..diameter {
                let color = pixel(x, y);
                assert_eq!(color >> 24, 0xff);
                assert_eq!(color, pixel(diameter - 1 - x, y));
                assert_eq!(color, pixel(x, diameter - 1 - y));
                blended_edge |= color != background && color != foreground;
            }
        }
        assert!(blended_edge);
    }

    #[test]
    fn full_charge_bar_has_no_separate_terminal() {
        let now = UNIX_EPOCH + Duration::from_secs(300);
        let card = Contents::from_poll_at(
            &Snapshot {
                connection: Connection::Receiver,
                link: LinkCheck::Confirmed(now),
                battery: Some(BatterySample {
                    percent: 100,
                    charge_state: ChargeState::Discharging,
                    millivolts: 4113,
                    measured_at: now,
                    transport: Transport::Receiver,
                }),
                ..Snapshot::default()
            },
            false,
            Some(Duration::from_secs(7199)),
            now,
            Language::Russian,
        );
        let width = WIDTH * 2;
        let canvas = paint(&card, width, card.height() * 2, 192).unwrap();
        let pixel = |x, y| canvas.pixels()[(y * 2 * width + x * 2) as usize];
        assert_eq!(pixel(270, 74), 0xff68dea5);
        for y in 69..79 {
            for x in 272..280 {
                assert_eq!(pixel(x, y), 0xff161d27);
            }
        }
    }

    #[test]
    fn voltage_uses_exact_millivolts_and_the_local_decimal_separator() {
        assert_eq!(
            voltage_line(Language::English, Some(4_168)),
            "Voltage · 4.168 V"
        );
        assert_eq!(
            voltage_line(Language::Russian, Some(4_168)),
            "Напряжение · 4,168 В"
        );
        assert_eq!(
            voltage_line(Language::English, Some(4_002)),
            "Voltage · 4.002 V"
        );
        assert_eq!(voltage_line(Language::Russian, None), "Напряжение · —");
    }

    #[test]
    fn voltage_visibility_changes_the_height_without_discarding_the_reading() {
        let mut card = Contents::demo(Language::English);
        assert_eq!(card.height(), 166);
        assert_eq!(card.millivolts, Some(4_012));
        card.set_voltage_visible(false);
        assert_eq!(card.height(), 146);
        assert_eq!(card.millivolts, Some(4_012));
        card.set_voltage_visible(true);
        assert_eq!(card.height(), 166);
    }

    #[test]
    fn time_rows_show_seconds_below_a_minute() {
        let language = Language::Russian;
        const LAST: &str = "Последний замер";
        assert_eq!(check_line(language, LAST, None), "Последний замер · —");
        assert_eq!(
            check_line(language, LAST, Some(Duration::ZERO)),
            "Последний замер · 0 сек"
        );
        assert_eq!(
            check_line(language, LAST, Some(Duration::from_millis(59_999))),
            "Последний замер · 59 сек"
        );
        assert_eq!(
            check_line(language, LAST, Some(Duration::from_secs(60))),
            "Последний замер · 1 мин"
        );
        assert_eq!(next_check_line(language, None), "До следующего запроса · —");
        assert_eq!(
            next_check_line(language, Some(Duration::ZERO)),
            "До следующего запроса · 0 сек"
        );
        assert_eq!(
            next_check_line(language, Some(Duration::from_millis(1))),
            "До следующего запроса · 1 сек"
        );
        assert_eq!(
            next_check_line(language, Some(Duration::from_secs(30))),
            "До следующего запроса · 30 сек"
        );
        assert_eq!(
            next_check_line(language, Some(Duration::from_millis(59_999))),
            "До следующего запроса · 60 сек"
        );
        assert_eq!(
            next_check_line(language, Some(Duration::from_secs(60))),
            "До следующего запроса · 1 мин"
        );
        assert_eq!(
            next_check_line(language, Some(Duration::from_secs(3_720))),
            "До следующего запроса · 1 ч 2 мин"
        );
    }

    #[test]
    fn english_uses_english_status_and_time_units() {
        let language = Language::English;
        let disconnected = Snapshot {
            connection: Connection::Absent,
            ..Snapshot::default()
        };
        let card = Contents::from_poll_at(&disconnected, false, None, UNIX_EPOCH, language);
        assert_eq!(card.status, "Mouse not found");
        assert_eq!(card.no_battery, "Battery level unknown");
        assert_eq!(card.last_check, "Last query · —");
        assert_eq!(card.next_check, "Until next query · —");

        let demo = Contents::demo(language);
        assert_eq!(demo.last_check, "Last query · 13 s");
        assert_eq!(demo.next_check, "Until next query · 17 s");
        assert_eq!(
            next_check_line(language, Some(Duration::from_secs(3_720))),
            "Until next query · 1 h 2 min"
        );
    }

    #[test]
    fn a_failed_receiver_attempt_keeps_timing_and_hides_stale_charge() {
        let finished = UNIX_EPOCH + Duration::from_secs(300);
        let battery = BatterySample {
            percent: 73,
            charge_state: ChargeState::Charging,
            millivolts: 3_960,
            measured_at: finished - Duration::from_secs(180),
            transport: Transport::Receiver,
        };
        let snapshot = Snapshot {
            connection: Connection::Receiver,
            link: LinkCheck::NoResponse,
            battery: Some(battery),
            last_attempt: Some(BatteryAttempt {
                transport: Transport::Receiver,
                completed_at: finished,
                outcome: AttemptOutcome::NoResponse,
            }),
        };
        let card = Contents::from_poll_at(
            &snapshot,
            false,
            Some(Duration::from_secs(30)),
            finished + Duration::from_secs(5),
            Language::Russian,
        );
        assert_eq!(card.last_check, "Последний замер · 5 сек");
        assert_eq!(card.next_check, "До следующего запроса · 30 сек");
        assert_eq!(card.percent, None);
        assert_eq!(card.charge_state, None);
        assert_eq!(card.millivolts, None);
    }

    #[test]
    fn a_charge_sample_is_available_only_on_its_connected_transport() {
        let time = UNIX_EPOCH + Duration::from_secs(300);
        let snapshot = Snapshot {
            connection: Connection::Cable,
            battery: Some(BatterySample {
                percent: 73,
                charge_state: ChargeState::Charging,
                millivolts: 3_960,
                measured_at: time,
                transport: Transport::Cable,
            }),
            ..Snapshot::default()
        };
        let visible = Contents::from_poll_at(&snapshot, false, None, time, Language::Russian);
        assert_eq!(visible.percent, Some(73));
        assert_eq!(visible.charge_state, Some(ChargeState::Charging));
        assert_eq!(visible.millivolts, Some(3_960));

        let missing_voltage = Snapshot {
            battery: snapshot.battery.map(|sample| BatterySample {
                millivolts: 0,
                ..sample
            }),
            ..snapshot
        };
        let card = Contents::from_poll_at(&missing_voltage, false, None, time, Language::Russian);
        assert_eq!(card.percent, Some(73));
        assert_eq!(card.millivolts, None);

        let disconnected = Snapshot {
            connection: Connection::Absent,
            ..snapshot
        };
        let card = Contents::from_poll_at(&disconnected, false, None, time, Language::Russian);
        assert_eq!(card.percent, None);
        assert_eq!(card.charge_state, None);
        assert_eq!(card.millivolts, None);

        let receiver = Snapshot {
            connection: Connection::Receiver,
            link: LinkCheck::Confirmed(time),
            ..snapshot
        };
        let card = Contents::from_poll_at(&receiver, false, None, time, Language::Russian);
        assert_eq!(card.percent, None);
        assert_eq!(card.charge_state, None);
        assert_eq!(card.millivolts, None);
    }
}
