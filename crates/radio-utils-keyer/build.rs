fn main() {
    // DEX compilation only for Android targets
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("android") {
        return;
    }

    let android_home =
        std::env::var("ANDROID_HOME").expect("ANDROID_HOME must be set for Android builds");
    let out_dir = std::env::var("OUT_DIR").unwrap();

    // Java sources are vendored next to the crate; both helpers are compiled
    // into a single DEX and loaded together via InMemoryDexClassLoader.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let java_dir = std::path::PathBuf::from(&manifest_dir).join("java");

    let java_sources: Vec<std::path::PathBuf> = std::fs::read_dir(&java_dir)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", java_dir.display(), e))
        .map(|entry| {
            entry
                .unwrap_or_else(|e| {
                    panic!(
                        "Failed to read directory entry in {}: {}",
                        java_dir.display(),
                        e
                    )
                })
                .path()
        })
        .filter(|p| p.extension().is_some_and(|x| x == "java"))
        .collect();
    assert!(
        !java_sources.is_empty(),
        "No .java files found in {}",
        java_dir.display()
    );
    for src in &java_sources {
        println!("cargo:rerun-if-changed={}", src.display());
    }

    let android_jar = find_android_jar(&android_home).expect(
        "Could not find android.jar — ensure ANDROID_HOME/platforms/android-26+ is installed",
    );

    let d8_path = find_d8(&android_home)
        .expect("Could not find d8 — ensure ANDROID_HOME/build-tools/ is installed");

    // Compile every .java file in radio-utils-android/java/ into the same classes
    // directory. d8 then bundles all of them into one classes.dex.
    let classes_dir = std::path::PathBuf::from(&out_dir).join("java_classes");
    std::fs::create_dir_all(&classes_dir).expect("Failed to create classes dir");

    let mut javac = std::process::Command::new("javac");
    javac
        .args(["-source", "8", "-target", "8"])
        .arg("-cp")
        .arg(&android_jar)
        .arg("-d")
        .arg(&classes_dir);
    for src in &java_sources {
        javac.arg(src);
    }
    let output = javac
        .output()
        .expect("Failed to run javac — ensure JDK is installed and in PATH");
    if !output.status.success() {
        eprintln!("javac stderr:\n{}", String::from_utf8_lossy(&output.stderr));
        panic!("javac compilation failed");
    }

    // Collect all .class files recursively (anonymous inner classes included)
    let mut class_files = collect_class_files(&classes_dir);
    class_files.sort();
    assert!(!class_files.is_empty(), "No .class files produced by javac");

    // Convert to DEX with d8
    let dex_out_dir = std::path::PathBuf::from(&out_dir);
    let mut d8_cmd = std::process::Command::new(&d8_path);
    d8_cmd
        .arg("--min-api")
        .arg("26")
        .arg("--output")
        .arg(&dex_out_dir);
    for f in &class_files {
        d8_cmd.arg(f);
    }
    let d8_output = d8_cmd.output().expect("Failed to run d8");
    if !d8_output.status.success() {
        eprintln!("d8 stderr:\n{}", String::from_utf8_lossy(&d8_output.stderr));
        panic!("d8 failed");
    }

    let dex_path = dex_out_dir.join("classes.dex");
    assert!(dex_path.exists(), "classes.dex not produced by d8");

    // Single DEX, multiple classes (MidiHelper, UsbSerialHelper, …).
    println!(
        "cargo:rustc-env=RADIO_UTILS_KEYER_DEX={}",
        dex_path.display()
    );

    // AMidi is available from API 29 and provides native MIDI polling.
    // Explicitly add the NDK API-29 sysroot lib directory so the linker finds
    // libamidi.so even when the build targets a lower API level.
    if let Some(amidi_dir) = find_ndk_amidi_lib_dir(&android_home, &target) {
        println!("cargo:rustc-link-search=native={}", amidi_dir.display());
    }
    println!("cargo:rustc-link-lib=amidi");
}

fn collect_class_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_class_files(&path));
            } else if path.extension().is_some_and(|e| e == "class") {
                files.push(path);
            }
        }
    }
    files
}

/// Find the NDK sysroot directory that contains libamidi.so for the given target arch.
///
/// The NDK organises library stubs by API level:
///   `<ndk>/toolchains/llvm/prebuilt/<host>/sysroot/usr/lib/<arch-triple>/29/libamidi.so`
///
/// Returns the first directory containing libamidi.so at API 29+.
fn find_ndk_amidi_lib_dir(android_home: &str, target: &str) -> Option<std::path::PathBuf> {
    // Resolve the NDK root: prefer ANDROID_NDK_HOME, then the highest version
    // under $ANDROID_HOME/ndk/.
    let ndk_root = std::env::var("ANDROID_NDK_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let ndk_dir = std::path::PathBuf::from(android_home).join("ndk");
            let mut versions: Vec<_> = std::fs::read_dir(&ndk_dir)
                .ok()?
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .collect();
            // Sort by version descending (lexicographic on semver-like names works).
            versions.sort_by(|a, b| b.cmp(a));
            versions.into_iter().next()
        })?;

    // Map the Rust target triple to the Android arch-specific lib directory name.
    let arch_dir = if target.contains("aarch64") {
        "aarch64-linux-android"
    } else if target.contains("armv7") || target.contains("arm-linux") {
        "arm-linux-androideabi"
    } else if target.contains("x86_64") {
        "x86_64-linux-android"
    } else if target.contains("i686") {
        "i686-linux-android"
    } else {
        return None;
    };

    // Find the prebuilt host toolchain directory (e.g. darwin-x86_64).
    let prebuilt_root = ndk_root.join("toolchains/llvm/prebuilt");
    let host_dir = std::fs::read_dir(&prebuilt_root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())?;

    // Pick the lowest API level >= 29 that has libamidi.so.
    for api in 29..=35 {
        let lib_dir = host_dir
            .join("sysroot/usr/lib")
            .join(arch_dir)
            .join(api.to_string());
        if lib_dir.join("libamidi.so").exists() {
            return Some(lib_dir);
        }
    }
    None
}

fn find_android_jar(android_home: &str) -> Option<std::path::PathBuf> {
    for api in (26..=35).rev() {
        let path = std::path::PathBuf::from(android_home)
            .join("platforms")
            .join(format!("android-{api}"))
            .join("android.jar");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn find_d8(android_home: &str) -> Option<std::path::PathBuf> {
    let build_tools = std::path::PathBuf::from(android_home).join("build-tools");
    let Ok(entries) = std::fs::read_dir(&build_tools) else {
        return None;
    };
    let mut versions: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    // Sort numerically descending by (major, minor, patch) so "33.0.1" > "9.0.0"
    versions.sort_by(|a, b| {
        let parse = |s: &str| -> (u32, u32, u32) {
            let mut parts = s.splitn(3, '.').map(|p| p.parse::<u32>().unwrap_or(0));
            (
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
            )
        };
        parse(b).cmp(&parse(a))
    });

    for version in &versions {
        // On Windows the binary is d8.bat; on Unix it's d8
        for name in &["d8", "d8.bat", "d8.cmd"] {
            let d8 = build_tools.join(version).join(name);
            if d8.exists() {
                return Some(d8);
            }
        }
    }
    None
}
