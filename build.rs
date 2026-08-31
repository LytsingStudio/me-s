use std::{
    env, fs,
    fs::File,
    io,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

const PYTHON_VERSION: &str = "3.12.13";
const PYTHON_RELEASE: &str = "20260718";

struct PythonDistribution {
    target: &'static str,
    sha256: &'static str,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ME_PYTHON_RUNTIME_ARCHIVE");
    println!("cargo:rerun-if-env-changed=ME_BUILD_OFFLINE");
    let target = env::var("TARGET").expect("Cargo did not provide TARGET");
    prepare_python_runtime(&target);

    println!("cargo:rerun-if-changed=src/camoufox_window_control_macos.m");
    if !target.ends_with("apple-darwin") {
        return;
    }

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not provide OUT_DIR"))
        .join("camoufox_window_control_macos.dylib");
    let compile = Command::new("clang")
        .args([
            "-dynamiclib",
            "-fobjc-arc",
            "-framework",
            "AppKit",
            "-framework",
            "Foundation",
            "-arch",
            "arm64",
            "-arch",
            "x86_64",
            "-mmacosx-version-min=11.0",
            "-Os",
            "-Wl,-dead_strip",
            "-Wl,-install_name,@rpath/camoufox_window_control_macos.dylib",
            "-o",
        ])
        .arg(&output)
        .arg("src/camoufox_window_control_macos.m")
        .status()
        .expect("failed to run clang for the macOS Camoufox window-control bridge");
    assert!(
        compile.success(),
        "failed to compile the macOS Camoufox window-control bridge"
    );

    let sign = Command::new("codesign")
        .args(["-s", "-", "--force"])
        .arg(&output)
        .status()
        .expect("failed to run codesign for the macOS Camoufox window-control bridge");
    assert!(
        sign.success(),
        "failed to sign the macOS Camoufox window-control bridge"
    );
    generate_bridge_module(&output);
}

fn generate_bridge_module(dylib: &Path) {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output = manifest.join(".build/generated/camoufox_bridge.rs");
    fs::create_dir_all(output.parent().unwrap())
        .expect("failed to create the generated bridge module directory");
    let bytes = fs::read(dylib).expect("failed to read the compiled Camoufox bridge");
    let mut source = String::from("#[rustfmt::skip]\npub static BYTES: &[u8] = &[\n");
    for line in bytes.chunks(32) {
        source.push_str("    ");
        source.push_str(
            &line
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
        source.push_str(",\n");
    }
    source.push_str("];\n");
    let temporary = output.with_extension(format!("rs-{}", std::process::id()));
    fs::write(&temporary, source).expect("failed to write the generated Camoufox bridge module");
    fs::rename(&temporary, &output)
        .expect("failed to install the generated Camoufox bridge module");
}

fn prepare_python_runtime(target: &str) {
    let distribution = python_distribution(target).unwrap_or_else(|| {
        panic!("me has no embedded Python 3.12 distribution for target {target}")
    });
    let asset = format!(
        "cpython-{PYTHON_VERSION}+{PYTHON_RELEASE}-{}-install_only_stripped.tar.gz",
        distribution.target
    );
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let archive = env::var_os("ME_PYTHON_RUNTIME_ARCHIVE")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join(".build/python").join(&asset));
    if sha256(&archive).as_deref() != Some(distribution.sha256) {
        if env::var_os("ME_PYTHON_RUNTIME_ARCHIVE").is_some() {
            panic!(
                "ME_PYTHON_RUNTIME_ARCHIVE {} does not match the pinned SHA-256 {}",
                archive.display(),
                distribution.sha256
            );
        }
        if matches!(env::var("ME_BUILD_OFFLINE").as_deref(), Ok("1")) {
            panic!(
                "embedded Python runtime cache {} is missing or invalid while offline",
                archive.display()
            );
        }
        download_python(&archive, &asset, distribution.sha256);
    }
    println!("cargo:rerun-if-changed={}", archive.display());
    println!(
        "cargo:rustc-env=ME_EMBEDDED_PYTHON_ARCHIVE={}",
        archive.display()
    );
    println!(
        "cargo:rustc-env=ME_EMBEDDED_PYTHON_ID=cpython-{PYTHON_VERSION}+{PYTHON_RELEASE}-{target}"
    );
    println!(
        "cargo:rustc-env=ME_EMBEDDED_PYTHON_SHA256={}",
        distribution.sha256
    );
}

fn python_distribution(target: &str) -> Option<PythonDistribution> {
    Some(match target {
        "aarch64-apple-darwin" => PythonDistribution {
            target: "aarch64-apple-darwin",
            sha256: "9a1e9e06175c10efd8378b904b07fa21bd791ab3345d7cdffeb4a76c9ff55903",
        },
        "x86_64-apple-darwin" => PythonDistribution {
            target: "x86_64-apple-darwin",
            sha256: "8e6b7e6533bdf746287008edf91102e7bee0a6ca1d24f16c4514237cafd706c5",
        },
        "x86_64-unknown-linux-gnu" => PythonDistribution {
            target: "x86_64-unknown-linux-gnu",
            sha256: "5854aa6ec71cad00334d5065633c210b2e7feb40956767a59a91791cadcf0b79",
        },
        "aarch64-unknown-linux-gnu" => PythonDistribution {
            target: "aarch64-unknown-linux-gnu",
            sha256: "f226576b91491ffa5739aa85726521e9031f4d87f80627d64ed348ac77cb31e9",
        },
        "x86_64-pc-windows-gnu" | "x86_64-pc-windows-msvc" => PythonDistribution {
            target: "x86_64-pc-windows-msvc",
            sha256: "0d422a1439ec308e03f47df551bc30f5994727c456e414b026d202bcda9b7c1c",
        },
        _ => return None,
    })
}

fn download_python(path: &Path, asset: &str, expected_sha256: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap_or_else(|error| {
        panic!(
            "failed to create embedded Python cache {}: {error}",
            path.parent().unwrap().display()
        )
    });
    let temporary = path.with_extension(format!("download-{}", std::process::id()));
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/{PYTHON_RELEASE}/{}",
        asset.replace('+', "%2B")
    );
    println!(
        "cargo:warning=downloading pinned Python {PYTHON_VERSION} runtime for the current target"
    );
    let status = Command::new("curl")
        .args(["--fail", "--location", "--retry", "3", "--output"])
        .arg(&temporary)
        .arg(&url)
        .status()
        .unwrap_or_else(|error| panic!("failed to run curl while downloading {url}: {error}"));
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        panic!("failed to download embedded Python runtime from {url}");
    }
    let actual = sha256(&temporary).unwrap_or_else(|| {
        let _ = fs::remove_file(&temporary);
        panic!("failed to hash downloaded Python runtime")
    });
    if actual != expected_sha256 {
        let _ = fs::remove_file(&temporary);
        panic!(
            "downloaded Python runtime SHA-256 mismatch: expected {expected_sha256}, received {actual}"
        );
    }
    if path.exists() {
        fs::remove_file(path).unwrap_or_else(|error| {
            panic!(
                "failed to replace stale Python runtime cache {}: {error}",
                path.display()
            )
        });
    }
    fs::rename(&temporary, path).unwrap_or_else(|error| {
        let _ = fs::remove_file(&temporary);
        panic!(
            "failed to store embedded Python runtime cache {}: {error}",
            path.display()
        )
    });
}

fn sha256(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut DigestWriter(&mut digest)).ok()?;
    Some(format!("{:x}", digest.finalize()))
}

struct DigestWriter<'a>(&'a mut Sha256);

impl io::Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
