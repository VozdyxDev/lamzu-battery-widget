use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-env-changed=LAMZU_WIDGET_VERSION");
    let version = match env::var("LAMZU_WIDGET_VERSION") {
        Ok(version) => {
            let mut components = version.split('.');
            assert!(
                components.clone().count() == 3
                    && components.all(|component| {
                        (component.len() == 1 || !component.starts_with('0'))
                            && component.bytes().all(|byte| byte.is_ascii_digit())
                            && component.parse::<u64>().is_ok()
                    }),
                "LAMZU_WIDGET_VERSION must be a numeric X.Y.Z version"
            );
            version
        }
        Err(env::VarError::NotPresent) => env!("CARGO_PKG_VERSION").into(),
        Err(error) => panic!("Failed to read LAMZU_WIDGET_VERSION: {error}"),
    };
    println!("cargo:rustc-env=LAMZU_WIDGET_VERSION={version}");

    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/lbw.rc");
    println!("cargo:rerun-if-changed=assets/lbw.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let crate_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let manifest = crate_dir.join("app.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );

        let resources = compile_icon(&crate_dir);
        println!("cargo:rustc-link-arg-bins={}", resources.display());
    }
}

fn compile_icon(crate_dir: &Path) -> PathBuf {
    for variable in ["RC", "WindowsSdkVerBinPath", "WindowsSdkDir"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let rc = resource_compiler().expect(
        "Windows SDK resource compiler (rc.exe) not found. Install the Windows SDK or set RC.",
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("lbw.res");
    let status = Command::new(&rc)
        .current_dir(crate_dir.join("assets"))
        .arg("/nologo")
        .arg("/fo")
        .arg(&output)
        .arg("lbw.rc")
        .status()
        .unwrap_or_else(|error| panic!("Failed to launch {}: {error}", rc.display()));
    assert!(status.success(), "{} failed: {status}", rc.display());
    output
}

fn resource_compiler() -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC") {
        return Some(PathBuf::from(rc));
    }

    let architecture = match env::consts::ARCH {
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    };
    if let Some(bin) = env::var_os("WindowsSdkVerBinPath") {
        let rc = PathBuf::from(bin).join(architecture).join("rc.exe");
        if rc.is_file() {
            return Some(rc);
        }
    }
    if let Some(path) = env::var_os("PATH")
        && let Some(rc) = env::split_paths(&path)
            .map(|directory| directory.join("rc.exe"))
            .find(|rc| rc.is_file())
    {
        return Some(rc);
    }

    let sdk = env::var_os("WindowsSdkDir").map(PathBuf::from);
    let default_sdk = env::var_os("ProgramFiles(x86)")
        .or_else(|| env::var_os("ProgramFiles"))
        .map(|directory| PathBuf::from(directory).join("Windows Kits").join("10"));
    for root in sdk.into_iter().chain(default_sdk) {
        let bin = root.join("bin");
        if let Ok(versions) = fs::read_dir(bin)
            && let Some(rc) = versions
                .filter_map(Result::ok)
                .map(|version| version.path().join(architecture).join("rc.exe"))
                .filter(|rc| rc.is_file())
                .max()
        {
            return Some(rc);
        }
    }
    None
}
