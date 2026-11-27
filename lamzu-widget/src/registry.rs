use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::Path,
    ptr::null_mut,
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
            ERROR_UNSUPPORTED_TYPE,
        },
        System::Registry::{
            HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
            RegSetKeyValueW,
        },
    },
    core::PCWSTR,
    w,
};

use crate::{display::Corner, locale::Language};

const SETTINGS_KEY: PCWSTR = w!("Software\\LAMZU Battery Widget");
const LANGUAGE_VALUE: PCWSTR = w!("Language");
const MONITOR_VALUE: PCWSTR = w!("Monitor");
const CORNER_VALUE: PCWSTR = w!("Corner");
const PRIMARY_MONITOR: &str = "primary";
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("LAMZU Battery Widget");
const RUN_COMMAND_LIMIT: usize = 260;
const QUOTE: u16 = b'"' as u16;

pub fn language() -> Language {
    read_setting(LANGUAGE_VALUE)
        .as_deref()
        .and_then(Language::parse)
        .unwrap_or_default()
}

pub fn set_language(language: Language) -> io::Result<()> {
    set_setting(LANGUAGE_VALUE, language.code())
}

pub fn monitor() -> Option<String> {
    read_setting(MONITOR_VALUE).filter(|id| {
        !id.eq_ignore_ascii_case(PRIMARY_MONITOR)
            && !id.trim().is_empty()
            && !id.chars().any(char::is_control)
    })
}

pub fn set_monitor(monitor: Option<&str>) -> io::Result<()> {
    set_setting(MONITOR_VALUE, monitor.unwrap_or(PRIMARY_MONITOR))
}

pub fn corner() -> Corner {
    read_setting(CORNER_VALUE)
        .as_deref()
        .and_then(Corner::parse)
        .unwrap_or_default()
}

pub fn set_corner(corner: Corner) -> io::Result<()> {
    set_setting(CORNER_VALUE, corner.code())
}

pub fn autostart_enabled() -> io::Result<bool> {
    let mut data = [0u16; RUN_COMMAND_LIMIT + 1];
    let mut bytes = size_of_val(&data) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    match status {
        ERROR_SUCCESS => {
            let bytes = bytes as usize;
            if !bytes.is_multiple_of(size_of::<u16>()) {
                return Ok(false);
            }
            // Both the CLI and setup register a quoted executable path.
            let Some([QUOTE, path @ .., QUOTE, 0]) = data.get(..bytes / size_of::<u16>()) else {
                return Ok(false);
            };
            Ok(!path.contains(&0)
                && !path.contains(&QUOTE)
                && Path::new(&OsString::from_wide(path)).is_absolute())
        }
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_UNSUPPORTED_TYPE | ERROR_MORE_DATA => {
            Ok(false)
        }
        status => Err(io::Error::from_raw_os_error(status as i32)),
    }
}

pub fn add_autostart(path_too_long: &'static str) -> io::Result<()> {
    let executable = env::current_exe()?;
    let command = autostart_command(executable.as_os_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, path_too_long))?;
    set_string(RUN_KEY, RUN_VALUE, &command)
}

pub fn remove_autostart() -> io::Result<()> {
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
    match status {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(()),
        status => Err(io::Error::from_raw_os_error(status as i32)),
    }
}

fn read_setting(name: PCWSTR) -> Option<String> {
    let mut data = [0u16; 256];
    let mut bytes = size_of_val(&data) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SETTINGS_KEY,
            name,
            RRF_RT_REG_SZ,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    let bytes = bytes as usize;
    if status != ERROR_SUCCESS || !bytes.is_multiple_of(size_of::<u16>()) {
        return None;
    }
    let text = data.get(..bytes / size_of::<u16>())?.strip_suffix(&[0])?;
    if text.contains(&0) {
        return None;
    }
    String::from_utf16(text).ok()
}

fn set_setting(name: PCWSTR, value: &str) -> io::Result<()> {
    let data: Vec<_> = value.encode_utf16().chain([0]).collect();
    set_string(SETTINGS_KEY, name, &data)
}

fn set_string(key: PCWSTR, name: PCWSTR, data: &[u16]) -> io::Result<()> {
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key,
            name,
            REG_SZ,
            data.as_ptr().cast(),
            size_of_val(data) as u32,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn autostart_command(executable: &OsStr) -> Option<Vec<u16>> {
    let mut command: Vec<_> = [QUOTE]
        .into_iter()
        .chain(executable.encode_wide())
        .chain([QUOTE])
        .collect();
    if command.len() > RUN_COMMAND_LIMIT {
        return None;
    }
    command.push(0);
    Some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_command_quotes_the_executable_path() {
        let path = r"C:\Program Files\Мышь\lamzu-widget.exe";
        let command = autostart_command(OsStr::new(path)).unwrap();
        let expected: Vec<_> = format!("\"{path}\"\0").encode_utf16().collect();
        assert_eq!(command, expected);
    }

    #[test]
    fn autostart_command_checks_the_windows_length_limit() {
        let path = "a".repeat(RUN_COMMAND_LIMIT - 2);
        let command = autostart_command(OsStr::new(&path)).unwrap();
        assert_eq!(command.len(), RUN_COMMAND_LIMIT + 1);

        let too_long = format!("{path}a");
        assert!(autostart_command(OsStr::new(&too_long)).is_none());
    }
}
