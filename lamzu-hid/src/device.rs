use std::io;
use std::mem::{offset_of, size_of};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::{
    Devices::{
        DeviceAndDriverInstallation::{
            DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, MAX_DEVICE_ID_LEN,
            SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
            SPINT_ACTIVE, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
            SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW,
        },
        HumanInterfaceDevice::{
            HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData, HidD_GetHidGuid,
            HidD_GetPreparsedData, HidP_GetCaps, HidP_InitializeReportForID, HidP_Input,
            HidP_Output, PHIDP_PREPARSED_DATA,
        },
    },
    Foundation::{
        CloseHandle, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
        ERROR_IO_PENDING, ERROR_NO_MORE_ITEMS, ERROR_NO_SUCH_DEVICE,
        ERROR_NO_SUCH_DEVICE_INTERFACE, ERROR_NO_SUCH_DEVINST, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        ReadFile, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Threading::{CreateEventW, WaitForSingleObject},
    },
};
use windows_sys::core::GUID;

use crate::{ChargeState, Transport};

const USAGE_PAGE: u16 = 0xff02;
const USAGE: u16 = 2;
const REPORT_ID: u8 = 8;
const RECEIVER_PREPARE_COMMAND: u8 = 1;
const BATTERY_COMMAND: u8 = 4;
const REPORT_BYTES: usize = 17;
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const RECEIVER_PREPARE_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) struct Device {
    pub(crate) transport: Transport,
    pub(crate) path: Vec<u16>,
}

pub(crate) enum Discovery {
    Absent,
    Ambiguous,
    Found(Device),
}

pub(crate) struct Reading {
    pub(crate) raw_percent: u8,
    pub(crate) charge_state: ChargeState,
    pub(crate) millivolts: u16,
}

pub(crate) fn discover() -> io::Result<Discovery> {
    let mut guid = GUID::default();
    // SAFETY: guid is a writable HID class GUID result.
    unsafe { HidD_GetHidGuid(&mut guid) };
    // SAFETY: guid remains valid for the call and the optional parameters are null.
    let list = unsafe {
        SetupDiGetClassDevsW(
            &guid,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if list == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(io::Error::last_os_error());
    }
    let list = DeviceList(list);
    let mut cables = Vec::new();
    let mut receivers = Vec::new();
    for index in 0.. {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: list and guid are valid and interface can receive the result.
        let found =
            unsafe { SetupDiEnumDeviceInterfaces(list.0, null(), &guid, index, &mut interface) };
        if found == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_ITEMS as i32) {
                break;
            }
            return Err(error);
        }
        if interface.Flags & SPINT_ACTIVE == 0 {
            continue;
        }
        match discover_interface(&list, &interface) {
            Ok(Some(device)) => match device.transport {
                Transport::Cable => cables.push(device),
                Transport::Receiver => receivers.push(device),
            },
            Ok(None) => {}
            Err(error) if interface_disappeared(&error) => {}
            Err(error) => return Err(error),
        }
    }

    let mut preferred = if cables.is_empty() { receivers } else { cables };
    Ok(match preferred.pop() {
        None => Discovery::Absent,
        Some(device) if preferred.is_empty() => Discovery::Found(device),
        Some(_) => Discovery::Ambiguous,
    })
}

fn interface_disappeared(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(
            ERROR_DEVICE_NOT_CONNECTED
                | ERROR_FILE_NOT_FOUND
                | ERROR_NO_SUCH_DEVICE
                | ERROR_NO_SUCH_DEVICE_INTERFACE
                | ERROR_NO_SUCH_DEVINST
        )
    )
}

fn discover_interface(
    list: &DeviceList,
    interface: &SP_DEVICE_INTERFACE_DATA,
) -> io::Result<Option<Device>> {
    let mut required_bytes = 0;
    let mut info = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    // SAFETY: list and interface are valid. With a null detail buffer, SetupAPI
    // returns the required length and fills info for the enumerated interface.
    let result = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            list.0,
            interface,
            null_mut(),
            0,
            &mut required_bytes,
            &mut info,
        )
    };
    if result == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }

    let mut instance_id = [0_u16; MAX_DEVICE_ID_LEN as usize];
    // SAFETY: info was returned for this list and the buffer has the declared length.
    if unsafe {
        SetupDiGetDeviceInstanceIdW(
            list.0,
            &info,
            instance_id.as_mut_ptr(),
            MAX_DEVICE_ID_LEN,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let instance_end = instance_id
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(instance_id.len());
    let Some(transport) =
        transport_for_instance(&String::from_utf16_lossy(&instance_id[..instance_end]))
    else {
        return Ok(None);
    };

    let required_bytes_usize = required_bytes as usize;
    let path_offset = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
    if required_bytes_usize < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid HID path size",
        ));
    }
    // Allocate aligned storage that includes the variable-length path after cbSize.
    let mut detail_words = vec![0_u32; required_bytes_usize.div_ceil(size_of::<u32>())];
    detail_words[0] = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    // SAFETY: detail_words is aligned and holds at least required_bytes bytes.
    if unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            list.0,
            interface,
            detail_words.as_mut_ptr().cast(),
            required_bytes,
            null_mut(),
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: path_offset is the aligned UTF-16 field offset within the buffer.
    let path = unsafe {
        std::slice::from_raw_parts(
            detail_words
                .as_ptr()
                .cast::<u8>()
                .add(path_offset)
                .cast::<u16>(),
            (required_bytes_usize - path_offset) / size_of::<u16>(),
        )
    };
    let path_end = path
        .iter()
        .position(|&unit| unit == 0)
        .filter(|&end| end != 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HID device path"))?;
    let path = &path[..=path_end];
    if is_battery_collection(path)? {
        return Ok(Some(Device {
            transport,
            path: path.to_vec(),
        }));
    }
    Ok(None)
}

fn transport_for_instance(instance_id: &str) -> Option<Transport> {
    // These are Windows PnP instance-ID fields, independent of the opaque HID path.
    let normalized = instance_id.to_ascii_lowercase();
    let identity = normalized.strip_prefix("hid\\")?.split('\\').next()?;
    let mut parts = identity.split('&');
    if parts.next()? != "vid_3554" {
        return None;
    }
    let transport = match parts.next()? {
        "pid_f50f" => Transport::Cable,
        "pid_f50d" => Transport::Receiver,
        _ => return None,
    };
    (parts.next()? == "mi_01" && parts.next()? == "col05").then_some(transport)
}

fn is_battery_collection(path: &[u16]) -> io::Result<bool> {
    // Access zero lets us inspect the cached descriptor without reading or writing reports.
    let handle = Handle::open(path, 0, 0)?;
    let mut preparsed = 0;
    // SAFETY: handle is valid and preparsed is writable.
    if !unsafe { HidD_GetPreparsedData(handle.0, &mut preparsed) } {
        return Err(io::Error::last_os_error());
    }
    let preparsed = PreparsedData(preparsed);
    let mut caps = HIDP_CAPS::default();
    // SAFETY: preparsed was returned by HID and caps is writable.
    if unsafe { HidP_GetCaps(preparsed.0, &mut caps) } != HIDP_STATUS_SUCCESS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid HID descriptor",
        ));
    }
    if caps.UsagePage != USAGE_PAGE
        || caps.Usage != USAGE
        || usize::from(caps.InputReportByteLength) != REPORT_BYTES
        || usize::from(caps.OutputReportByteLength) != REPORT_BYTES
    {
        return Ok(false);
    }

    let mut report = [0; REPORT_BYTES];
    for report_type in [HidP_Input, HidP_Output] {
        // SAFETY: the parser checks the report ID locally using cached descriptor data.
        if unsafe {
            HidP_InitializeReportForID(
                report_type,
                REPORT_ID,
                preparsed.0,
                report.as_mut_ptr(),
                REPORT_BYTES as u32,
            )
        } != HIDP_STATUS_SUCCESS
        {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Device {
    pub(crate) fn read_battery(&self, prepare_receiver: bool) -> io::Result<Reading> {
        let handle = Handle::open(
            &self.path,
            GENERIC_READ | GENERIC_WRITE,
            FILE_FLAG_OVERLAPPED,
        )?;
        if prepare_receiver && matches!(self.transport, Transport::Receiver) {
            initialize_receiver(&handle)?;
        }
        let deadline = Instant::now() + IO_TIMEOUT;
        write_report(&handle, &battery_request(), deadline)?;

        loop {
            let mut response = [0; REPORT_BYTES];
            let count = transfer(&handle, Transfer::Read(&mut response), deadline)?;
            if count == REPORT_BYTES
                && let Some(reading) = parse_battery(&response)
            {
                return Ok(reading);
            }
        }
    }
}

fn initialize_receiver(handle: &Handle) -> io::Result<()> {
    let mut nonce = [0; 4];
    // SAFETY: system-preferred RNG takes a null provider and fills the supplied buffer.
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            nonce.as_mut_ptr(),
            nonce.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "Windows random generator failed with NTSTATUS {:#010x}",
            status as u32,
        )));
    }

    // Mouse Drive Beta sends report 08 command 01 when discovering the device.
    // On an eligible recovery attempt, send it once and wait briefly for an
    // acknowledgement before issuing the single battery request.
    let deadline = Instant::now() + RECEIVER_PREPARE_TIMEOUT;
    write_report(handle, &receiver_prepare_request(nonce), deadline)?;
    let mut response = [0; REPORT_BYTES];
    while let Ok(count) = transfer(handle, Transfer::Read(&mut response), deadline) {
        if count == REPORT_BYTES && report_matches(&response, RECEIVER_PREPARE_COMMAND) {
            break;
        }
    }
    Ok(())
}

fn write_report(handle: &Handle, report: &[u8; REPORT_BYTES], deadline: Instant) -> io::Result<()> {
    if transfer(handle, Transfer::Write(report), deadline)? != REPORT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "incomplete HID report write",
        ));
    }
    Ok(())
}

fn receiver_prepare_request(nonce: [u8; 4]) -> [u8; REPORT_BYTES] {
    let mut report = [0; REPORT_BYTES];
    report[0] = REPORT_ID;
    report[1] = RECEIVER_PREPARE_COMMAND;
    report[5] = 8;
    report[6..10].copy_from_slice(&nonce);
    report[16] = checksum(&report[..16]);
    report
}

fn battery_request() -> [u8; REPORT_BYTES] {
    let mut report = [0; REPORT_BYTES];
    report[0] = REPORT_ID;
    report[1] = BATTERY_COMMAND;
    report[16] = checksum(&report[..16]);
    report
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0x55_u8, |remaining, byte| remaining.wrapping_sub(*byte))
}

fn report_matches(report: &[u8; REPORT_BYTES], command: u8) -> bool {
    report[0] == REPORT_ID && report[1] == command && checksum(&report[..16]) == report[16]
}

fn parse_battery(report: &[u8; REPORT_BYTES]) -> Option<Reading> {
    if !report_matches(report, BATTERY_COMMAND) || report[2] != 0 {
        return None;
    }
    let raw_percent = report[6];
    let charge_state = match report[7] {
        0 => ChargeState::Discharging,
        1 => ChargeState::Charging,
        2 => ChargeState::Full,
        _ => return None,
    };
    let millivolts = u16::from_be_bytes([report[8], report[9]]);
    if raw_percent > 100 || (raw_percent == 0 && millivolts == 0 && report[7] == 0) {
        return None;
    }
    Some(Reading {
        raw_percent,
        charge_state,
        millivolts,
    })
}

enum Transfer<'a> {
    Read(&'a mut [u8; REPORT_BYTES]),
    Write(&'a [u8; REPORT_BYTES]),
}

fn transfer(handle: &Handle, operation: Transfer<'_>, deadline: Instant) -> io::Result<usize> {
    if Instant::now() >= deadline {
        return Err(timeout());
    }
    // SAFETY: optional arguments are null and the process owns the returned event.
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let event = Handle(event);
    let mut overlapped = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    // SAFETY: buffers and the event stay valid until the operation completes.
    let submitted = unsafe {
        match operation {
            Transfer::Read(buffer) => ReadFile(
                handle.0,
                buffer.as_mut_ptr(),
                REPORT_BYTES as u32,
                null_mut(),
                &mut overlapped,
            ),
            Transfer::Write(buffer) => WriteFile(
                handle.0,
                buffer.as_ptr(),
                REPORT_BYTES as u32,
                null_mut(),
                &mut overlapped,
            ),
        }
    };

    if submitted == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait_ms = remaining.as_micros().div_ceil(1_000) as u32;
        // SAFETY: the event stays open through completion and the timeout is bounded.
        let wait = unsafe { WaitForSingleObject(event.0, wait_ms) };
        if wait != WAIT_OBJECT_0 {
            let error = if wait == WAIT_TIMEOUT {
                timeout()
            } else {
                io::Error::last_os_error()
            };
            let mut ignored = 0;
            // SAFETY: join the canceled operation before reusing its OVERLAPPED or buffer.
            unsafe {
                CancelIoEx(handle.0, &overlapped);
                GetOverlappedResult(handle.0, &overlapped, &mut ignored, 1);
            }
            return Err(error);
        }
    }

    let mut transferred = 0;
    // SAFETY: the operation completed immediately or signaled its allocated event.
    if unsafe { GetOverlappedResult(handle.0, &overlapped, &mut transferred, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(transferred as usize)
}

fn timeout() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "no battery report within the HID timeout",
    )
}

struct Handle(HANDLE);

impl Handle {
    fn open(path: &[u16], access: u32, flags: u32) -> io::Result<Self> {
        debug_assert_eq!(path.last(), Some(&0));
        // SAFETY: path contains the null-terminated name and optional handles are null.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                flags,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this type owns an open file or event handle.
        unsafe { CloseHandle(self.0) };
    }
}

struct DeviceList(HDEVINFO);

impl Drop for DeviceList {
    fn drop(&mut self) {
        // SAFETY: this type owns a SetupAPI device information set.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

struct PreparsedData(PHIDP_PREPARSED_DATA);

impl Drop for PreparsedData {
    fn drop(&mut self) {
        // SAFETY: this pointer was returned by HidD_GetPreparsedData and has one owner.
        unsafe { HidD_FreePreparsedData(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BATTERY_FIXTURE: [u8; REPORT_BYTES] = [
        0x08, 0x04, 0x00, 0x00, 0x00, 0x02, 79, 1, 0x0f, 0x3c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xac,
    ];

    #[test]
    fn encodes_the_vendor_battery_request() {
        assert_eq!(
            battery_request(),
            [8, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x49]
        );
    }

    #[test]
    fn reads_battery_percentage_charge_state_and_big_endian_voltage() {
        let reading = parse_battery(&BATTERY_FIXTURE).unwrap();
        assert_eq!(reading.raw_percent, 79);
        assert!(matches!(reading.charge_state, ChargeState::Charging));
        assert_eq!(reading.millivolts, 3900);
    }

    #[test]
    fn encodes_the_vendor_receiver_prepare_request() {
        assert_eq!(
            receiver_prepare_request([1, 0x20, 0x7f, 0xfe]),
            [
                8, 1, 0, 0, 0, 8, 1, 0x20, 0x7f, 0xfe, 0, 0, 0, 0, 0, 0, 0xa6
            ],
        );
    }

    #[test]
    fn ignores_unrelated_and_corrupt_reports() {
        let mut report = BATTERY_FIXTURE;
        report[16] ^= 1;
        assert!(parse_battery(&report).is_none());

        report[1] = 3;
        report[16] = checksum(&report[..16]);
        assert!(parse_battery(&report).is_none());

        report[1] = BATTERY_COMMAND;
        report[2] = 8;
        report[16] = checksum(&report[..16]);
        assert!(parse_battery(&report).is_none());

        report[2] = 0;
        report[6] = 101;
        report[16] = checksum(&report[..16]);
        assert!(parse_battery(&report).is_none());
    }

    #[test]
    fn accepts_discharged_and_full_batteries() {
        let mut report = BATTERY_FIXTURE;
        report[6] = 0;
        report[7] = 0;
        report[8..10].copy_from_slice(&3050_u16.to_be_bytes());
        report[16] = checksum(&report[..16]);
        let empty = parse_battery(&report).unwrap();
        assert_eq!(empty.raw_percent, 0);
        assert!(matches!(empty.charge_state, ChargeState::Discharging));

        report[6] = 100;
        report[7] = 2;
        report[8..10].copy_from_slice(&4110_u16.to_be_bytes());
        report[16] = checksum(&report[..16]);
        let full = parse_battery(&report).unwrap();
        assert_eq!(full.raw_percent, 100);
        assert!(matches!(full.charge_state, ChargeState::Full));
    }

    #[test]
    fn identifies_only_the_known_runtime_devices_and_interface() {
        assert!(matches!(
            transport_for_instance("HID\\VID_3554&PID_F50D&MI_01&Col05\\7&0"),
            Some(Transport::Receiver)
        ));
        assert!(matches!(
            transport_for_instance("HID\\VID_3554&PID_F50F&MI_01&Col05\\7&0"),
            Some(Transport::Cable)
        ));
        assert!(transport_for_instance("HID\\VID_3554&PID_F50D&MI_02\\7&0").is_none());
        assert!(transport_for_instance("HID\\VID_3554&PID_F50D&MI_01&Col04\\7&0").is_none());
        assert!(transport_for_instance("HID\\VID_3554&PID_F510&MI_01&Col05\\7&0").is_none());
    }
}
