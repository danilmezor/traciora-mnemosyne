use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, process::Command};
const DATA_HASH: &str = "bd8c145abdf3f8383276ce01dfa4ae48709bef9fef1c0711eb7c3fab4f6eb7c2";
const DATA_GZIP_HASH: &str = "e9df731445b81db880f9663b8d63246eb5954b9b9ad73225f66fc1ffe69a1086";
const SOURCE_HASH: &str = "3e99687b5c435d4b209630e2d2ebb79906c984685e78635078b672e03c89df35";
fn run(command: &mut Command) {
    let output = command.output().expect("ICU build tool unavailable");
    if !output.status.success() {
        panic!(
            "ICU78 build failed: {:?}\n{}\n{}",
            command,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}
fn main() {
    println!("cargo:rerun-if-changed=vendor/icu4c-78.2-sources.tgz");
    let root = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let archive = Path::new(&root).join("vendor/icu4c-78.2-sources.tgz");
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(&archive).expect("bundled ICU source missing"))
    );
    assert_eq!(digest, SOURCE_HASH, "pinned ICU source digest mismatch");
    println!("cargo:rerun-if-changed=vendor/electron-43.7.7-icudtl.dat.gz");
    let data_archive = Path::new(&root).join("vendor/electron-43.7.7-icudtl.dat.gz");
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&data_archive).unwrap())),
        DATA_GZIP_HASH,
        "pinned data archive mismatch"
    );
    let data = Command::new("gzip")
        .args(["-dc"])
        .arg(&data_archive)
        .output()
        .expect("gzip build tool unavailable");
    assert!(data.status.success(), "data extraction failed");
    assert_eq!(
        format!("{:x}", Sha256::digest(&data.stdout)),
        DATA_HASH,
        "pinned Electron ICU data mismatch"
    );
    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();
    assert_eq!(target,host,"ICU78 cross compilation requires an explicit target build pipeline; host library substitution is forbidden");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    assert!(matches!(os.as_str(),"macos"|"linux"),"ICU78 native MSVC/mobile build pipeline is required for this target; system ICU substitution is forbidden");
    let cargo_out = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(cargo_out.join("icudtl.dat"), &data.stdout).unwrap();
    let compiler = env::var("CXX").unwrap_or_else(|_| "c++".into());
    let compiler_version = Command::new(&compiler)
        .arg("--version")
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=CXX");
    println!("cargo:rerun-if-env-changed=SDKROOT");
    let deployment = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| {
        if target.starts_with("aarch64") {
            "11.0".into()
        } else {
            "10.13".into()
        }
    });
    let cc = env::var("CC").unwrap_or_default();
    let sdk = env::var("SDKROOT").unwrap_or_default();
    let cache_key = format!(
        "{:x}",
        Sha256::digest(
            format!("{SOURCE_HASH}:{DATA_HASH}:{target}:{compiler}:{compiler_version:?}:{deployment}:{cc}:{sdk}")
                .as_bytes()
        )
    );
    let out = cargo_out
        .ancestors()
        .nth(4)
        .expect("Cargo target directory")
        .join("mnemosyne-icu78-native")
        .join(cache_key);
    fs::create_dir_all(&out).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(out.join("build.lock"))
        .unwrap();
    lock.lock().unwrap();
    let install = out.join("install");
    let build_id =
        format!("{SOURCE_HASH}:{DATA_HASH}:symbols_mnemosyne_v4_raw_data:{target}:{deployment}");
    let stamp = install.join("build-id");
    if fs::read_to_string(&stamp).ok().as_deref() != Some(&build_id) {
        if install.exists() {
            fs::remove_dir_all(&install).unwrap();
        }
        let build = out.join("native-build");
        let configured = out.join("configured-build-id");
        if fs::read_to_string(&configured).ok().as_deref() != Some(&build_id) {
            for relative in ["icu", "native-build"] {
                let path = out.join(relative);
                if path.exists() {
                    fs::remove_dir_all(path).unwrap();
                }
            }
            run(Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&out));

            fs::create_dir_all(&build).unwrap();
            let mut configure = Command::new(out.join("icu/source/configure"));
            if os == "macos" {
                configure.env("MACOSX_DEPLOYMENT_TARGET", &deployment);
            }
            run(configure
                .current_dir(&build)
                .arg(format!("--prefix={}", install.display()))
                .args([
                    "--enable-static",
                    "--disable-shared",
                    "--disable-tests",
                    "--disable-samples",
                    "--disable-extras",
                    "--disable-icuio",
                    "--disable-dyload",
                    "--with-data-packaging=static",
                    "--with-library-suffix=mnemosyne",
                ])
                .env("CFLAGS", "-O2 -fPIC")
                .env("CXXFLAGS", "-O2 -fPIC"));
            fs::write(&configured, &build_id).unwrap();
        }
        fs::create_dir_all(build.join("lib")).unwrap();
        for directory in ["stubdata", "common", "i18n"] {
            run(Command::new("make")
                .current_dir(&build)
                .args(["-C", directory, "-j2"]));
        }
        fs::create_dir_all(install.join("lib")).unwrap();
        for (from, name) in [
            ("lib/libicui18nmnemosyne.a", "libicui18nmnemosyne.a"),
            ("lib/libicuucmnemosyne.a", "libicuucmnemosyne.a"),
            ("stubdata/libicudatamnemosyne.a", "libicudatamnemosyne.a"),
        ] {
            fs::copy(build.join(from), install.join("lib").join(name)).unwrap();
        }
        // Build intermediates have no runtime role; retain installed static libs/data.
        fs::remove_dir_all(out.join("icu")).unwrap();
        fs::remove_dir_all(build).unwrap();
        fs::write(&stamp, &build_id).unwrap();
    }
    println!(
        "cargo:rustc-link-search=native={}",
        install.join("lib").display()
    );
    for library in ["icui18nmnemosyne", "icuucmnemosyne", "icudatamnemosyne"] {
        println!("cargo:rustc-link-lib=static={library}")
    }
    println!(
        "cargo:rustc-link-lib={}",
        if os == "macos" { "c++" } else { "stdc++" }
    );
}
