//! Build script. When the `tesseract` feature is on, compile minimal **static** Leptonica +
//! Tesseract from the vendored `third_party/` submodules and link them into the core. With
//! the feature off (the default for `cargo test`/`check`) this is a no-op, so the pure-Rust
//! crate stays fast to build.
//!
//! The build is deliberately minimal: every Leptonica image codec is OFF (distillPDF decodes
//! images in Rust and feeds Tesseract raw RGB via `SetImage`), and Tesseract is LSTM-only with
//! no training tools / tests / graphics. See `benchmarking/ocr/TESSERACT_SPIKE.md` for the
//! flags' provenance and the measured size/time. The CMake flags are facts (clean-room).

fn main() {
    if std::env::var("CARGO_FEATURE_TESSERACT").is_err() {
        return; // feature off → nothing to build
    }
    #[cfg(feature = "tesseract")]
    build_tesseract();
}

#[cfg(feature = "tesseract")]
fn build_tesseract() {
    use std::path::Path;

    for sub in ["third_party/leptonica", "third_party/tesseract"] {
        if !Path::new(sub).join("CMakeLists.txt").exists() {
            panic!(
                "vendored source missing: {sub}/CMakeLists.txt. Run \
                 `git submodule update --init --depth 1 {sub}`."
            );
        }
        println!("cargo:rerun-if-changed={sub}/CMakeLists.txt");
    }

    // --- Leptonica: static, NO image codecs (we feed raw RGB) ---------------
    let lept = cmake::Config::new("third_party/leptonica")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_PROG", "OFF")
        // Leptonica defaults SW_BUILD=ON on Windows, which does `find_package(SW REQUIRED)`
        // for the Software Network package manager and fails on a clean MSVC runner. Force it
        // off (it's already off on macOS/Linux) so the from-source build works everywhere.
        .define("SW_BUILD", "OFF")
        .define("ENABLE_ZLIB", "OFF")
        .define("ENABLE_PNG", "OFF")
        .define("ENABLE_JPEG", "OFF")
        .define("ENABLE_TIFF", "OFF")
        .define("ENABLE_WEBP", "OFF")
        .define("ENABLE_OPENJPEG", "OFF")
        .define("ENABLE_GIF", "OFF")
        .cflag("-ffunction-sections")
        .cflag("-fdata-sections")
        .build();

    // --- Tesseract: static, LSTM-only ---------------------------------------
    let lept_cmake = lept.join("lib").join("cmake").join("leptonica");
    let tess = cmake::Config::new("third_party/tesseract")
        .define("BUILD_SHARED_LIBS", "OFF")
        // Same Windows quirk as Leptonica: Tesseract defaults SW_BUILD=ON on Windows and
        // does `find_package(SW REQUIRED)`, which fails on a clean MSVC runner.
        .define("SW_BUILD", "OFF")
        .define("DISABLED_LEGACY_ENGINE", "ON")
        .define("BUILD_TRAINING_TOOLS", "OFF")
        .define("BUILD_TESTS", "OFF")
        .define("GRAPHICS_DISABLED", "ON")
        .define("USE_OPENCL", "OFF")
        .define("OPENMP_BUILD", "OFF")
        .define("DISABLE_CURL", "ON")
        .define("DISABLE_ARCHIVE", "ON")
        .define("Leptonica_DIR", lept_cmake.to_str().unwrap())
        .define("CMAKE_PREFIX_PATH", lept.to_str().unwrap())
        .cxxflag("-ffunction-sections")
        .cxxflag("-fdata-sections")
        .build();

    // Link the produced static libs. Their filenames vary by platform/version
    // (libtesseract.a on unix vs e.g. tesseract55.lib / leptonica-1.85.0.lib on MSVC), so
    // discover the real names rather than hard-coding them. Order matters for GNU ld:
    // tesseract depends on leptonica, so name it first.
    for dir in [&tess, &lept] {
        println!("cargo:rustc-link-search=native={}/lib", dir.display());
    }
    let lib_dirs = [tess.join("lib"), lept.join("lib")];
    for substr in ["tesseract", "leptonica"] {
        match find_static_lib(&lib_dirs, substr) {
            Some(stem) => println!("cargo:rustc-link-lib=static={stem}"),
            None => panic!("could not find the built {substr} static lib in {lib_dirs:?}"),
        }
    }

    // C++ runtime + platform extras.
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target.as_str() {
        "macos" | "ios" => println!("cargo:rustc-link-lib=dylib=c++"),
        "windows" => { /* MSVC links the C++ runtime automatically */ }
        _ => {
            println!("cargo:rustc-link-lib=dylib=stdc++");
            println!("cargo:rustc-link-lib=dylib=pthread");
            println!("cargo:rustc-link-lib=dylib=m");
        }
    }
}

/// Find a built static library whose filename contains `substr` in any of `dirs`, and return
/// the `rustc-link-lib=static=` stem: `.a`/`.lib` extension stripped, plus a leading `lib`
/// prefix on the unix archive form (`libtesseract.a` -> `tesseract`). Handles the version
/// suffixes MSVC adds (`tesseract55.lib`, `leptonica-1.85.0.lib`).
#[cfg(feature = "tesseract")]
fn find_static_lib(dirs: &[std::path::PathBuf], substr: &str) -> Option<String> {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for ent in entries.flatten() {
            let path = ent.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !name.contains(substr) {
                continue;
            }
            if let Some(stem) = name.strip_suffix(".lib") {
                return Some(stem.to_string()); // MSVC: link by the exact stem
            }
            if let Some(stem) = name.strip_suffix(".a") {
                return Some(stem.strip_prefix("lib").unwrap_or(stem).to_string());
            }
        }
    }
    None
}
