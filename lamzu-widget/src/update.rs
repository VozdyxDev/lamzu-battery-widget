use std::{
    ffi::c_void,
    fs::{self, File},
    io::{self, Read, Write},
    mem::{size_of, size_of_val},
    path::Path,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};

use serde::Deserialize;
use windows_sys::{
    Win32::{
        Networking::WinHttp::*,
        Security::Cryptography::{
            BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash,
            BCryptFinishHash, BCryptHashData,
        },
    },
    core::BOOL,
    w,
};

const API_HOST: &str = "api.github.com";
const ASSET_HOST: &str = "github.com";
const REPOSITORY: &str = "/VozdyxDev/lamzu-battery-widget";
const API_HEADERS: &str =
    "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2026-03-10\r\n";
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;
const CHECK_TIMEOUT: Duration = Duration::from_secs(25);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug)]
pub enum Check {
    Current,
    NoRelease,
    Available(Release),
}

#[derive(Debug)]
pub struct Release {
    pub version: String,
    installer: Asset,
    portable: Asset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageKind {
    Installer,
    Portable,
}

impl PackageKind {
    const fn filename(self) -> &'static str {
        match self {
            Self::Installer => "LAMZU-Battery-Widget-windows-x64-setup.exe",
            Self::Portable => "LAMZU-Battery-Widget-windows-x64.zip",
        }
    }
}

#[derive(Debug)]
struct Asset {
    path: String,
    size: u64,
    sha256: [u8; 32],
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

pub fn check(current_version: &str) -> io::Result<Check> {
    let current = stable_version(current_version)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid installed version"))?;
    let client = Client::new(CHECK_TIMEOUT)?;
    let path = format!("/repos{REPOSITORY}/releases/latest");
    let response = client.get(API_HOST, &path, Some(API_HEADERS))?;
    match response.status()? {
        404 => return Ok(Check::NoRelease),
        status => require_ok(status)?,
    }

    let mut metadata = Vec::new();
    response
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut metadata)?;
    if metadata.len() as u64 > MAX_METADATA_BYTES {
        return Err(invalid("release metadata is too large"));
    }
    parse_release(&metadata, current)
}

fn parse_release(metadata: &[u8], current: [u64; 3]) -> io::Result<Check> {
    let release: GitHubRelease = serde_json::from_slice(metadata)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if release.draft || release.prerelease {
        return Ok(Check::NoRelease);
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .ok_or_else(|| invalid("invalid release tag"))?;
    let remote = stable_version(version).ok_or_else(|| invalid("invalid release tag"))?;
    if remote <= current {
        return Ok(Check::Current);
    }

    Ok(Check::Available(Release {
        version: version.to_owned(),
        installer: asset(&release, PackageKind::Installer)?,
        portable: asset(&release, PackageKind::Portable)?,
    }))
}

fn stable_version(version: &str) -> Option<[u64; 3]> {
    let mut components = version.split('.');
    let mut version = [0; 3];
    for number in &mut version {
        let component = components.next()?;
        if component.len() > 1 && component.starts_with('0') {
            return None;
        }
        if !component.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *number = component.parse().ok()?;
    }
    components.next().is_none().then_some(version)
}

fn asset(release: &GitHubRelease, kind: PackageKind) -> io::Result<Asset> {
    let filename = kind.filename();
    let mut matches = release.assets.iter().filter(|asset| asset.name == filename);
    let asset = matches
        .next()
        .ok_or_else(|| invalid(format!("release is missing {filename}")))?;
    if matches.next().is_some() || asset.state != "uploaded" {
        return Err(invalid(format!("invalid release asset {filename}")));
    }
    let path = format!(
        "{REPOSITORY}/releases/download/{}/{filename}",
        release.tag_name
    );
    if asset.browser_download_url != format!("https://{ASSET_HOST}{path}") {
        return Err(invalid(format!("unexpected download URL for {filename}")));
    }
    if asset.size == 0 || asset.size > MAX_ASSET_BYTES {
        return Err(invalid(format!(
            "invalid release asset size for {filename}"
        )));
    }
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(parse_sha256)
        .ok_or_else(|| invalid(format!("invalid SHA-256 digest for {filename}")))?;
    Ok(Asset {
        path,
        size: asset.size,
        sha256,
    })
}

fn parse_sha256(value: &str) -> Option<[u8; 32]> {
    let value = value.strip_prefix("sha256:")?;
    let mut digest = [0; 32];
    if value.len() != digest.len() * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

impl Release {
    /// Write the verified package to a new file in the caller's staging directory.
    pub fn download(&self, kind: PackageKind, destination: &Path) -> io::Result<()> {
        let asset = match kind {
            PackageKind::Installer => &self.installer,
            PackageKind::Portable => &self.portable,
        };
        let mut file = File::create_new(destination)?;
        let result = (|| {
            let client = Client::new(DOWNLOAD_TIMEOUT)?;
            let response = asset_response(&client, &asset.path)?;
            copy_verified(response, &mut file, asset)?;
            file.sync_all()
        })();
        drop(file);
        if result.is_err() {
            let _ = fs::remove_file(destination);
        }
        result
    }
}

fn asset_response<'a>(client: &'a Client, path: &str) -> io::Result<Response<'a>> {
    let mut response = client.get(ASSET_HOST, path, None)?;
    for _ in 0..MAX_REDIRECTS {
        let status = response.status()?;
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            require_ok(status)?;
            return Ok(response);
        }
        let location = response.location()?;
        let (host, path) =
            asset_redirect(&location).ok_or_else(|| invalid("unexpected asset redirect"))?;
        drop(response);
        response = client.get(host, path, None)?;
    }
    require_ok(response.status()?)?;
    Ok(response)
}

fn asset_redirect(location: &str) -> Option<(&str, &str)> {
    let url = location.strip_prefix("https://")?;
    let slash = url.find('/')?;
    let (host, path) = url.split_at(slash);
    if !matches!(
        host,
        "release-assets.githubusercontent.com" | "objects.githubusercontent.com"
    ) || path.starts_with("//")
        || !path
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'#' | b'\\'))
    {
        return None;
    }
    Some((host, path))
}

fn copy_verified(mut input: impl Read, mut output: impl Write, asset: &Asset) -> io::Result<()> {
    let hash = Sha256::new()?;
    let mut buffer = [0; 32 * 1024];
    let mut size = 0;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > asset.size {
            return Err(invalid("download exceeds the published file size"));
        }
        let chunk = &buffer[..read];
        hash.update(chunk)?;
        output.write_all(chunk)?;
    }
    if size != asset.size {
        return Err(invalid("download does not match the published file size"));
    }
    if hash.finish()? != asset.sha256 {
        return Err(invalid(
            "download does not match the published SHA-256 digest",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct Sha256(BCRYPT_HASH_HANDLE);

impl Sha256 {
    fn new() -> io::Result<Self> {
        let mut handle = null_mut();
        // SAFETY: CNG allocates the hash object. The out parameter is writable,
        // and the Windows 10 algorithm pseudo-handle needs no cleanup.
        cng_status(unsafe {
            BCryptCreateHash(
                BCRYPT_SHA256_ALG_HANDLE,
                &mut handle,
                null_mut(),
                0,
                null(),
                0,
                0,
            )
        })?;
        Ok(Self(handle))
    }

    fn update(&self, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: The hash handle is live and this call only receives 32 KiB chunks.
        cng_status(unsafe { BCryptHashData(self.0, bytes.as_ptr(), bytes.len() as u32, 0) })
    }

    fn finish(self) -> io::Result<[u8; 32]> {
        let mut digest = [0; 32];
        // SAFETY: The live hash handle uses SHA-256 and the output buffer is 32 bytes.
        cng_status(unsafe {
            BCryptFinishHash(self.0, digest.as_mut_ptr(), digest.len() as u32, 0)
        })?;
        Ok(digest)
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        // SAFETY: The handle was successfully created and is uniquely owned.
        unsafe { BCryptDestroyHash(self.0) };
    }
}

fn cng_status(status: i32) -> io::Result<()> {
    if status < 0 {
        return Err(io::Error::other(format!(
            "Windows SHA-256 error {status:#010x}"
        )));
    }
    Ok(())
}

struct InternetHandle(*mut c_void);

impl InternetHandle {
    fn open(handle: *mut c_void) -> io::Result<Self> {
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    fn set_option(&self, option: u32, value: u32) -> io::Result<()> {
        // SAFETY: The live handle receives a documented DWORD option value.
        winhttp(unsafe {
            WinHttpSetOption(
                self.0,
                option,
                (&raw const value).cast(),
                size_of::<u32>() as u32,
            )
        })
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        // SAFETY: This handle was successfully opened and is uniquely owned.
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

struct Client {
    session: InternetHandle,
    deadline: Instant,
}

impl Client {
    fn new(timeout: Duration) -> io::Result<Self> {
        // SAFETY: The user agent is NUL-terminated. WinHTTP selects the system proxy.
        let session = InternetHandle::open(unsafe {
            WinHttpOpen(
                w!("LAMZU Battery Widget"),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            )
        })?;
        // SAFETY: The session is live. All four phase timeouts are finite milliseconds.
        winhttp(unsafe { WinHttpSetTimeouts(session.0, 5_000, 5_000, 10_000, 10_000) })?;
        session.set_option(WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, 10_000)?;
        session.set_option(
            WINHTTP_OPTION_REDIRECT_POLICY,
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        )?;
        Ok(Self {
            session,
            deadline: Instant::now() + timeout,
        })
    }

    fn get(&self, host: &str, path: &str, headers: Option<&str>) -> io::Result<Response<'_>> {
        self.check_deadline()?;
        let host = crate::app::wide(host);
        let path = crate::app::wide(path);
        // SAFETY: The session is live and the validated host is NUL-terminated UTF-16.
        let connection = InternetHandle::open(unsafe {
            WinHttpConnect(
                self.session.0,
                host.as_ptr(),
                INTERNET_DEFAULT_HTTPS_PORT,
                0,
            )
        })?;
        // SAFETY: The connection is live and the validated path is NUL-terminated UTF-16.
        // The exact query string of a signed CDN redirect must reach the server unchanged.
        let request = InternetHandle::open(unsafe {
            WinHttpOpenRequest(
                connection.0,
                w!("GET"),
                path.as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE | WINHTTP_FLAG_ESCAPE_DISABLE,
            )
        })?;
        request.set_option(
            WINHTTP_OPTION_DISABLE_FEATURE,
            WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_AUTHENTICATION,
        )?;
        let headers = headers.map(crate::app::wide);
        let (header, length) = headers
            .as_ref()
            .map_or((null(), 0), |text| (text.as_ptr(), (text.len() - 1) as u32));
        // SAFETY: The request is live. The header pointer and length describe a live
        // UTF-16 buffer or no headers. This GET request has no body.
        winhttp(unsafe { WinHttpSendRequest(request.0, header, length, null(), 0, 0, 0) })?;
        self.check_deadline()?;
        // SAFETY: The request has been sent, and the reserved argument is null.
        winhttp(unsafe { WinHttpReceiveResponse(request.0, null_mut()) })?;
        self.check_deadline()?;
        Ok(Response {
            request,
            _connection: connection,
            client: self,
        })
    }

    fn check_deadline(&self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "GitHub request timed out",
            ));
        }
        Ok(())
    }
}

struct Response<'a> {
    request: InternetHandle,
    _connection: InternetHandle,
    client: &'a Client,
}

impl Response<'_> {
    fn status(&self) -> io::Result<u32> {
        let mut status = 0_u32;
        let mut length = size_of::<u32>() as u32;
        // SAFETY: The live response has been received and NUMBER writes one DWORD.
        winhttp(unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&raw mut status).cast(),
                &mut length,
                null_mut(),
            )
        })?;
        Ok(status)
    }

    fn location(&self) -> io::Result<String> {
        let mut value = [0_u16; 8_192];
        let mut length = size_of_val(&value) as u32;
        // SAFETY: The live response has been received. The byte length describes
        // the writable UTF-16 buffer and receives the actual length excluding NUL.
        winhttp(unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                WINHTTP_QUERY_LOCATION,
                null(),
                value.as_mut_ptr().cast(),
                &mut length,
                null_mut(),
            )
        })?;
        let length = length as usize;
        if !length.is_multiple_of(size_of::<u16>()) || length >= size_of_val(&value) {
            return Err(invalid("invalid asset redirect header"));
        }
        String::from_utf16(&value[..length / size_of::<u16>()])
            .map_err(|_| invalid("invalid asset redirect header"))
    }
}

impl Read for Response<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.client.check_deadline()?;
        if buffer.is_empty() {
            return Ok(0);
        }
        let capacity = buffer.len().min(u32::MAX as usize) as u32;
        let mut read = 0;
        // SAFETY: The live response has been received. The output buffer is valid
        // for the requested byte count and the DWORD read parameter is writable.
        winhttp(unsafe {
            WinHttpReadData(
                self.request.0,
                buffer.as_mut_ptr().cast(),
                capacity,
                &mut read,
            )
        })?;
        self.client.check_deadline()?;
        Ok(read as usize)
    }
}

fn winhttp(ok: BOOL) -> io::Result<()> {
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn require_ok(status: u32) -> io::Result<()> {
    if status != 200 {
        return Err(io::Error::other(format!("GitHub returned HTTP {status}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const ABC_SHA256: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn fixture(tag: &str) -> Value {
        let assets = [PackageKind::Installer, PackageKind::Portable].map(|kind| {
            let filename = kind.filename();
            json!({
                "name": filename,
                "state": "uploaded",
                "size": 3,
                "digest": ABC_SHA256,
                "browser_download_url": format!("https://{ASSET_HOST}{REPOSITORY}/releases/download/{tag}/{filename}"),
            })
        });
        json!({ "tag_name": tag, "draft": false, "prerelease": false, "assets": assets })
    }

    fn parse(metadata: &Value, current: [u64; 3]) -> io::Result<Check> {
        parse_release(&serde_json::to_vec(metadata).unwrap(), current)
    }

    #[test]
    fn accepts_only_greater_numeric_stable_versions() {
        let metadata = fixture("v1.10.0");
        let Check::Available(release) = parse(&metadata, [1, 9, 9]).unwrap() else {
            panic!("a greater numeric version should be available");
        };
        assert_eq!(release.version, "1.10.0");
        assert!(matches!(parse(&metadata, [1, 10, 0]), Ok(Check::Current)));
        assert!(matches!(parse(&metadata, [2, 0, 0]), Ok(Check::Current)));

        for field in ["draft", "prerelease"] {
            let mut metadata = metadata.clone();
            metadata[field] = json!(true);
            assert!(matches!(parse(&metadata, [0, 0, 0]), Ok(Check::NoRelease)));
        }
        for tag in [
            "1.2.3",
            "v1.2.3-beta.1",
            "v1.2.3+abc",
            "v01.2.3",
            "v1.2",
            "v1.2.3.4",
            "v18446744073709551616.2.3",
            "v1.+2.3",
            "v1.２.3",
        ] {
            assert!(parse(&fixture(tag), [0, 0, 0]).is_err(), "{tag}");
        }
    }

    #[test]
    fn requires_both_canonical_uploaded_assets_with_bounded_sizes_and_digests() {
        let metadata = fixture("v1.2.3");
        for (field, value) in [
            ("name", json!("another.exe")),
            ("state", json!("starter")),
            ("size", json!(0)),
            ("size", json!(MAX_ASSET_BYTES + 1)),
            ("digest", Value::Null),
            ("digest", json!("md5:abcd")),
            ("digest", json!(format!("sha256:{}", "zz".repeat(32)))),
            (
                "browser_download_url",
                json!("http://github.com/releases/download/installer.exe"),
            ),
            (
                "browser_download_url",
                json!("https://example.com/installer.exe"),
            ),
        ] {
            let mut invalid = metadata.clone();
            invalid["assets"][0][field] = value;
            assert!(parse(&invalid, [1, 2, 2]).is_err(), "{field}");
        }
        let mut duplicate = metadata.clone();
        duplicate["assets"]
            .as_array_mut()
            .unwrap()
            .push(metadata["assets"][0].clone());
        assert!(parse(&duplicate, [1, 2, 2]).is_err());
        let mut missing = metadata;
        missing["assets"].as_array_mut().unwrap().pop();
        assert!(parse(&missing, [1, 2, 2]).is_err());
        assert!(matches!(parse(&missing, [1, 2, 3]), Ok(Check::Current)));
    }

    #[test]
    fn redirects_stay_on_known_https_asset_hosts() {
        let url = "https://release-assets.githubusercontent.com/github-production-release-asset/path?x=%2F&y=a+b";
        assert_eq!(
            asset_redirect(url),
            Some((
                "release-assets.githubusercontent.com",
                "/github-production-release-asset/path?x=%2F&y=a+b"
            ))
        );
        for url in [
            "http://release-assets.githubusercontent.com/file",
            "https://release-assets.githubusercontent.com.example.com/file",
            "https://release-assets.githubusercontent.com:443/file",
            "https://release-assets.githubusercontent.com@example.com/file",
            "https://release-assets.githubusercontent.com//example.com/file",
            "https://release-assets.githubusercontent.com/file#fragment",
            "https://release-assets.githubusercontent.com/file\r\n",
            "/relative/file",
        ] {
            assert_eq!(asset_redirect(url), None, "{url}");
        }
    }

    #[test]
    fn verifies_the_exact_size_and_sha256_before_accepting_a_package() {
        let Check::Available(release) = parse(&fixture("v1.2.3"), [1, 2, 2]).unwrap() else {
            panic!("expected a test release");
        };
        let mut output = Vec::new();
        copy_verified(&b"abc"[..], &mut output, &release.portable).unwrap();
        assert_eq!(output, b"abc");
        for invalid in [b"ab".as_slice(), b"abcd", b"abd"] {
            let error = copy_verified(invalid, io::sink(), &release.portable).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }
}
