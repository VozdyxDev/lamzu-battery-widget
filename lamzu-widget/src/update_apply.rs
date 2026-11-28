use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    process::{self, Child, Command},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, FILETIME, GetLastError, HANDLE,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Globalization::{CSTR_EQUAL, CompareStringOrdinal},
        Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
            SYNCHRONIZE,
        },
        System::{
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW},
            Threading::{
                CreateMutexW, GetCurrentProcess, GetProcessTimes, MUTEX_MODIFY_STATE, OpenMutexW,
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW, WaitForSingleObject,
            },
        },
    },
    w,
};
use zip::ZipArchive;

use crate::{
    locale::Language,
    update::{PackageKind, Release},
};

pub const HELPER_ARG: &str = "--apply-update-helper";

const MUTEX_NAME: *const u16 = w!("Local\\LamzuWallpaperMonitorUpdate");
const INSTALL_KEY: *const u16 =
    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\VozdyxDev.LamzuBatteryWidget_is1");
const HELPER_NAME: &str = "update-helper.exe";
const WIDGET_NAME: &str = "lamzu-widget.exe";
const INSTALLER_NAME: &str = "release.exe";
const ZIP_NAME: &str = "release.zip";
const READY_NAME: &str = "ready";
const FINISHED_NAME: &str = "finished";
const NEW_NAME: &str = "new.exe";
const BACKUP_NAME: &str = "backup.exe";
const UPDATE_PREFIX: &str = "update-";
const MAX_PATH_UNITS: usize = 32_768;
const MAX_WIDGET_BYTES: u64 = 64 * 1024 * 1024;
const HELPER_READY_WAIT: Duration = Duration::from_secs(10);
const WIDGET_EXIT_WAIT_MS: u32 = 60_000;

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Restarted,
    NeedsRestart,
}

pub fn in_progress() -> bool {
    let mutex = unsafe { OpenMutexW(MUTEX_MODIFY_STATE, 0, MUTEX_NAME) };
    if mutex.is_null() {
        return unsafe { GetLastError() } != ERROR_FILE_NOT_FOUND;
    }
    drop(unsafe { OwnedHandle::from_raw_handle(mutex) });
    true
}

pub fn prepare_and_start(release: &Release, language: Language, demo: bool) -> io::Result<()> {
    cleanup_finished();
    let executable = fs::canonicalize(env::current_exe()?)?;
    let installed = is_installed(&executable)?;
    let mut staging = Staging::new()?;
    if installed {
        release.download(
            PackageKind::Installer,
            &staging.directory.join(INSTALLER_NAME),
        )?;
    } else {
        let archive = staging.directory.join(ZIP_NAME);
        release.download(PackageKind::Portable, &archive)?;
        let adjacent = adjacent_directory(&executable, &staging.directory)?;
        fs::create_dir(&adjacent)?;
        staging.adjacent = Some(adjacent.clone());
        extract_widget(File::open(&archive)?, &adjacent.join(NEW_NAME))?;
        fs::remove_file(archive)?;
    }

    let helper = staging.directory.join(HELPER_NAME);
    io::copy(&mut File::open(&executable)?, &mut new_file(&helper)?)?;
    let mut child = Command::new(helper)
        .arg(HELPER_ARG)
        .arg(process::id().to_string())
        .arg(creation_time(unsafe { GetCurrentProcess() })?.to_string())
        .arg(&executable)
        .arg(if installed { "installed" } else { "portable" })
        .arg(language.code())
        .arg(if demo { "demo" } else { "normal" })
        .spawn()?;
    if let Err(error) = wait_until_ready(&mut child, &staging.directory.join(READY_NAME)) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    staging.handed_off = true;
    Ok(())
}

pub fn run_helper(args: &[OsString]) -> io::Result<ApplyOutcome> {
    let [pid, created, executable, package, language, mode] = args else {
        return Err(invalid("Invalid update helper arguments."));
    };
    let pid = pid
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|&pid| pid != 0 && pid != process::id())
        .ok_or_else(|| invalid("Invalid widget process ID."))?;
    let created = created
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| invalid("Invalid widget process creation time."))?;
    let language = language
        .to_str()
        .and_then(Language::parse)
        .ok_or_else(|| invalid("Invalid update helper language."))?;
    let installed = match package.to_str() {
        Some("installed") => true,
        Some("portable") => false,
        _ => return Err(invalid("Invalid update package type.")),
    };
    let demo = match mode.to_str() {
        Some("demo") => true,
        Some("normal") => false,
        _ => return Err(invalid("Invalid update helper mode.")),
    };
    let executable = Path::new(executable);
    if !executable.is_absolute() {
        return Err(invalid("The widget path must be absolute."));
    }
    let helper = fs::canonicalize(env::current_exe()?)?;
    let directory = staging_directory(&helper)?;
    let adjacent = if installed {
        None
    } else {
        Some(adjacent_directory(executable, directory)?)
    };
    let parent = open_widget(pid, created, executable)?;
    let lock = begin_update()?;
    new_file(&directory.join(READY_NAME))?;

    let result = match (wait_for_exit(&parent), adjacent.as_ref()) {
        (Ok(()), Some(path)) => apply_portable(path, executable, demo, lock),
        (Ok(()), None) => apply_installer(directory, executable, language, demo, lock),
        (Err(error), _) => Err(error),
    };
    if let Some(path) = adjacent {
        let _ = fs::remove_file(path.join(NEW_NAME));
        // A failed rollback may leave the original executable in this directory.
        let _ = fs::remove_dir(path);
    }
    let _ = fs::remove_file(directory.join(INSTALLER_NAME));
    let _ = fs::remove_file(directory.join(READY_NAME));
    let _ = new_file(&directory.join(FINISHED_NAME));
    result
}

fn begin_update() -> io::Result<OwnedHandle> {
    let mutex = unsafe { CreateMutexW(null(), 0, MUTEX_NAME) };
    if mutex.is_null() {
        return Err(io::Error::last_os_error());
    }
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let mutex = unsafe { OwnedHandle::from_raw_handle(mutex) };
    if existed {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Another widget update is in progress.",
        ));
    }
    Ok(mutex)
}

fn creation_time(process: HANDLE) -> io::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(creation.dwHighDateTime) << 32 | u64::from(creation.dwLowDateTime))
}

fn open_widget(pid: u32, created: u64, executable: &Path) -> io::Result<OwnedHandle> {
    let process = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut image = vec![0; MAX_PATH_UNITS];
    let mut units = image.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            PROCESS_NAME_WIN32,
            image.as_mut_ptr(),
            &mut units,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let parent_image =
        fs::canonicalize(PathBuf::from(OsString::from_wide(&image[..units as usize])))?;
    if creation_time(process.as_raw_handle())? != created || !equal_paths(executable, &parent_image)
    {
        return Err(invalid(
            "The update helper could not identify its widget process.",
        ));
    }
    Ok(process)
}

fn wait_for_exit(parent: &OwnedHandle) -> io::Result<()> {
    match unsafe { WaitForSingleObject(parent.as_raw_handle(), WIDGET_EXIT_WAIT_MS) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "The widget did not exit in time.",
        )),
        _ => Err(io::Error::last_os_error()),
    }
}

fn wait_until_ready(child: &mut Child, marker: &Path) -> io::Result<()> {
    let ready_start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "The update helper stopped: {status}."
            )));
        }
        if marker.is_file() {
            return Ok(());
        }
        if ready_start.elapsed() >= HELPER_READY_WAIT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "The update helper did not become ready.",
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn is_installed(executable: &Path) -> io::Result<bool> {
    let mut value = vec![0; MAX_PATH_UNITS];
    let mut bytes = (value.len() * size_of::<u16>()) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            INSTALL_KEY,
            w!("InstallLocation"),
            RRF_RT_REG_SZ,
            null_mut(),
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let length = bytes as usize / size_of::<u16>();
    let Some(value) = value
        .get(..length)
        .and_then(|units| units.strip_suffix(&[0]))
    else {
        return Err(invalid("The installer directory is invalid."));
    };
    let directory = PathBuf::from(OsString::from_wide(value));
    if !directory.is_absolute() {
        return Ok(false);
    }
    let installation = match fs::canonicalize(directory.join(WIDGET_NAME)) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(equal_paths(executable, &installation))
}

fn equal_paths(left: &Path, right: &Path) -> bool {
    let left = wide(left.as_os_str());
    let right = wide(right.as_os_str());
    unsafe { CompareStringOrdinal(left.as_ptr(), -1, right.as_ptr(), -1, 1) == CSTR_EQUAL }
}

fn apply_installer(
    directory: &Path,
    executable: &Path,
    language: Language,
    demo: bool,
    lock: OwnedHandle,
) -> io::Result<ApplyOutcome> {
    let install_result = (|| -> io::Result<_> {
        if !is_installed(executable)? {
            return Err(invalid(
                "The installed widget location changed before updating.",
            ));
        }
        let location = executable
            .parent()
            .ok_or_else(|| invalid("The installed widget has no parent directory."))?;
        // Inno Setup accepts ordinary absolute DOS paths, without the verbatim prefix.
        let location = unprefix(location);
        let mut directory_arg = OsString::from("/DIR=");
        directory_arg.push(location);
        let status = Command::new(directory.join(INSTALLER_NAME))
            .args([
                "/SP-",
                "/SILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/RESTARTEXITCODE=3010",
            ])
            .arg(directory_arg)
            .arg(match language {
                Language::English => "/LANG=en",
                Language::Russian => "/LANG=ru",
            })
            .status()?;
        match status.code() {
            Some(0) => Ok(false),
            Some(3010) => Ok(true),
            _ => Err(io::Error::other(format!(
                "The widget installer stopped: {status}."
            ))),
        }
    })();
    drop(lock);
    match install_result {
        Ok(true) => Ok(ApplyOutcome::NeedsRestart),
        Ok(false) => restart_widget(executable, demo),
        Err(error) => Err(restart_after_failure(executable, demo, error)),
    }
}

fn apply_portable(
    adjacent: &Path,
    executable: &Path,
    demo: bool,
    lock: OwnedHandle,
) -> io::Result<ApplyOutcome> {
    let replacement = adjacent.join(NEW_NAME);
    let backup = adjacent.join(BACKUP_NAME);
    let target = wide(executable.as_os_str());
    let staged = wide(replacement.as_os_str());
    let original = wide(backup.as_os_str());
    if unsafe {
        ReplaceFileW(
            target.as_ptr(),
            staged.as_ptr(),
            original.as_ptr(),
            0,
            null(),
            null(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        let recovered = backup
            .is_file()
            .then(|| restore_backup(executable, &backup));
        drop(lock);
        if let Some(Err(rollback)) = recovered {
            return Err(rollback_error(&backup, error, rollback));
        }
        return Err(restart_after_failure(executable, demo, error));
    }

    drop(lock);
    match restart_widget(executable, demo) {
        Ok(outcome) => {
            let _ = fs::remove_file(backup);
            Ok(outcome)
        }
        Err(error) => match begin_update().and_then(|_lock| restore_backup(executable, &backup)) {
            Ok(()) => Err(restart_after_failure(executable, demo, error)),
            Err(rollback) => Err(rollback_error(&backup, error, rollback)),
        },
    }
}

fn restore_backup(executable: &Path, backup: &Path) -> io::Result<()> {
    let original = wide(backup.as_os_str());
    let target = wide(executable.as_os_str());
    if unsafe {
        MoveFileExW(
            original.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rollback_error(backup: &Path, cause: io::Error, rollback: io::Error) -> io::Error {
    io::Error::other(format!(
        "{cause} Restoring the previous widget failed: {rollback}. Previous widget: {}",
        backup.display(),
    ))
}

fn restart_widget(executable: &Path, demo: bool) -> io::Result<ApplyOutcome> {
    let mut command = Command::new(executable);
    if demo {
        command.arg("--demo");
    }
    command.spawn().map(|_| ApplyOutcome::Restarted)
}

fn restart_after_failure(executable: &Path, demo: bool, error: io::Error) -> io::Error {
    match restart_widget(executable, demo) {
        Ok(_) => error,
        Err(restart) => io::Error::other(format!(
            "{error} Restarting the previous widget failed: {restart}"
        )),
    }
}

fn extract_widget(archive: impl Read + Seek, destination: &Path) -> io::Result<()> {
    let mut archive = ZipArchive::new(archive)?;
    if archive.len() != 1 {
        return Err(invalid(
            "The portable archive must contain one widget executable.",
        ));
    }
    let mut widget = archive.by_index(0)?;
    let size = widget.size();
    if widget.name_raw() != WIDGET_NAME.as_bytes()
        || !widget.is_file()
        || !(2..=MAX_WIDGET_BYTES).contains(&size)
    {
        return Err(invalid(
            "The portable archive contains an invalid widget executable.",
        ));
    }
    let mut signature = [0; 2];
    widget.read_exact(&mut signature)?;
    if &signature != b"MZ" {
        return Err(invalid("The portable widget is not a Windows executable."));
    }
    let mut file = new_file(destination)?;
    file.write_all(&signature)?;
    let copied = io::copy(&mut widget.take(size - 1), &mut file)?;
    if copied != size - 2 {
        return Err(invalid(
            "The portable widget size did not match the archive.",
        ));
    }
    file.sync_all()
}

struct Staging {
    directory: PathBuf,
    adjacent: Option<PathBuf>,
    handed_off: bool,
}

impl Staging {
    fn new() -> io::Result<Self> {
        let root = stage_root()?;
        fs::create_dir_all(&root)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        for suffix in 0..16 {
            let directory = root.join(format!("{UPDATE_PREFIX}{}-{now}-{suffix}", process::id()));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        directory,
                        adjacent: None,
                        handed_off: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Could not reserve widget update files.",
        ))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.handed_off {
            return;
        }
        if let Some(path) = &self.adjacent {
            let _ = fs::remove_file(path.join(NEW_NAME));
            let _ = fs::remove_dir(path);
        }
        remove_staged(&self.directory);
    }
}

fn stage_root() -> io::Result<PathBuf> {
    let root = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Local AppData is unavailable."))?;
    Ok(root.join("LamzuMonitor").join("updates"))
}

fn staging_directory(helper: &Path) -> io::Result<&Path> {
    let directory = helper
        .parent()
        .ok_or_else(|| invalid("Missing update helper directory."))?;
    let root = fs::canonicalize(stage_root()?)?;
    if helper.file_name() != Some(OsStr::new(HELPER_NAME))
        || !valid_stage_name(directory.file_name())
        || directory
            .parent()
            .is_none_or(|directory| !equal_paths(directory, &root))
    {
        return Err(invalid(
            "The executable is not a staged widget update helper.",
        ));
    }
    Ok(directory)
}

fn valid_stage_name(name: Option<&OsStr>) -> bool {
    name.and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(UPDATE_PREFIX))
        .is_some_and(|name| {
            let mut fields = name.split('-');
            fields
                .next()
                .is_some_and(|field| field.parse::<u32>().is_ok())
                && fields
                    .next()
                    .is_some_and(|field| field.parse::<u128>().is_ok())
                && fields
                    .next()
                    .is_some_and(|field| field.parse::<u8>().is_ok())
                && fields.next().is_none()
        })
}

fn adjacent_directory(executable: &Path, directory: &Path) -> io::Result<PathBuf> {
    let parent = executable
        .parent()
        .ok_or_else(|| invalid("The widget executable has no parent directory."))?;
    let name = directory
        .file_name()
        .ok_or_else(|| invalid("The widget update directory has no name."))?;
    let mut adjacent = OsString::from(".lamzu-widget-");
    adjacent.push(name);
    Ok(parent.join(adjacent))
}

fn new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn remove_staged(directory: &Path) {
    // Only delete the files owned by the updater.
    for file in [HELPER_NAME, INSTALLER_NAME, ZIP_NAME, READY_NAME] {
        let _ = fs::remove_file(directory.join(file));
    }
    if !directory.join(HELPER_NAME).exists() {
        let _ = fs::remove_file(directory.join(FINISHED_NAME));
        let _ = fs::remove_dir(directory);
    }
}

pub fn cleanup_finished() {
    let Ok(root) = stage_root() else {
        return;
    };
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir())
            && valid_stage_name(Some(&entry.file_name()))
            && entry.path().join(FINISHED_NAME).is_file()
        {
            remove_staged(&entry.path());
        }
    }
}

fn unprefix(path: &Path) -> OsString {
    let value: Vec<_> = path.as_os_str().encode_wide().collect();
    let strip_unc: Vec<_> = "\\\\?\\UNC\\".encode_utf16().collect();
    let strip_dos: Vec<_> = "\\\\?\\".encode_utf16().collect();
    if let Some(path) = value.strip_prefix(strip_unc.as_slice()) {
        let mut normalized = OsString::from("\\\\");
        normalized.push(OsString::from_wide(path));
        normalized
    } else {
        OsString::from_wide(value.strip_prefix(strip_dos.as_slice()).unwrap_or(&value))
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn archive(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for &(path, contents) in entries {
            writer
                .start_file(path, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap()
    }

    #[test]
    fn portable_archive_requires_one_file_named_lamzu_widget() {
        let unused = Path::new("invalid-portable-archive-must-not-create-a-file.exe");
        for entries in [
            &[("../lamzu-widget.exe", b"MZ" as &[u8])][..],
            &[(WIDGET_NAME, b"!!" as &[u8])],
            &[(WIDGET_NAME, b"MZ" as &[u8]), ("extra.txt", b"")],
        ] {
            let error = extract_widget(archive(entries), unused).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        assert!(!unused.exists());
    }
}
