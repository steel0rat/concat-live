// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Compiles the `.slint` tree into the binary and records three facts about
//! the build for Settings > About.

fn main() {
    compile_dictation();
    // Fonts and images are compiled into the binary rather than read off disk
    // at run time.
    //
    // `EmbedFiles` keeps each file exactly as it is — a PNG stays compressed,
    // a TTF stays a TTF — and hands it to the renderer from memory instead of
    // opening it. What that buys is a startup that touches no files and a
    // binary that is the whole application: five font faces, a logo and twenty
    // effect previews travel inside it, so there is no directory to ship
    // beside it and no path to get wrong.
    //
    // Not `EmbedForSoftwareRenderer`, which pre-decodes to raw pixels: that is
    // for MCUs with no filesystem, it is the only kind the software renderer
    // can read, and Skia cannot use it at all.
    //
    // On its own thread with a deep stack: the Slint compiler recurses over
    // the tree, and the tree has outgrown the megabyte a main thread gets
    // on Windows - the release build died there with a stack overflow and
    // nothing else to say. Half a gigabyte is reserved, not committed.
    let compile = std::thread::Builder::new()
        .name("slint".into())
        .stack_size(512 << 20)
        .spawn(|| {
            let config = slint_build::CompilerConfiguration::new()
                .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
            slint_build::compile_with_config("ui/app.slint", config)
        })
        .expect("could not start the Slint compiler thread");
    compile
        .join()
        .expect("the Slint compiler thread panicked")
        .expect("failed to compile ui/app.slint");

    // The text presets that ship: a folder each under text-presets/, the
    // same preset.toml a user or a community preset is. Listed here into a
    // table of include_str!s, so adding a built-in preset is adding a
    // folder and nothing in Rust names it.
    {
        let root =
            std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
                .join("text-presets");
        println!("cargo:rerun-if-changed={}", root.display());
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
            .expect("text-presets/ exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join("preset.toml"))
            .filter(|file| file.is_file())
            .collect();
        files.sort();
        let mut table = String::from("pub static BUILTIN: &[&str] = &[\n");
        for file in &files {
            println!("cargo:rerun-if-changed={}", file.display());
            table.push_str(&format!(
                "    include_str!({:?}),\n",
                file.display().to_string()
            ));
        }
        table.push_str("];\n");
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir"))
            .join("text_presets.rs");
        std::fs::write(out, table).expect("write text_presets.rs");
    }

    // The effect cards that ship: cards/<package id>.jpg, drawn on a real
    // GPU by scripts/bundle-cards.sh. A table of include_bytes!s, which a
    // shelf shows until the machine has drawn its own (#416).
    {
        let root =
            std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
                .join("cards");
        println!("cargo:rerun-if-changed={}", root.display());
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "jpg"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        let mut table = String::from("pub static CARDS: &[(&str, &[u8])] = &[\n");
        for file in &files {
            println!("cargo:rerun-if-changed={}", file.display());
            let id = file
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("a card's name");
            table.push_str(&format!(
                "    ({id:?}, include_bytes!({:?})),\n",
                file.display().to_string()
            ));
        }
        table.push_str("];\n");
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir"))
            .join("bundled_cards.rs");
        std::fs::write(out, table).expect("write bundled_cards.rs");
    }

    // The .exe's icon. The window's own is set in app.slint, but Explorer,
    // the Start menu and a pinned taskbar button read the file's resources,
    // and without one they show Windows' generic program icon. Needs the
    // Windows SDK's rc.exe, which an MSVC toolchain has.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon = "../../../assets/icons/concat.ico";
        println!("cargo:rerun-if-changed={icon}");
        // The file's name and description as Windows shows them in its
        // properties, and as code signing checks them: "Concat" in every
        // build, where the default would be the crate's lower-case name.
        winresource::WindowsResource::new()
            .set_icon(icon)
            .set("ProductName", "Concat")
            .set("FileDescription", "Concat")
            .set(
                "LegalCopyright",
                "Copyright 2026 Jareer Abdullah, AGPL-3.0-or-later",
            )
            .compile()
            .expect("could not embed the Windows icon");
    }

    // Three facts about the build that the built thing cannot ask for at run
    // time. Settings > About shows them in the block a bug report is copied
    // out of: which triple this binary is for, which profile it came out of,
    // and which compiler made it — the three questions every "cannot
    // reproduce" ends up asking.
    //
    // slint_build emits rerun-if-changed for the .slint tree, which turns off
    // cargo's rerun-on-any-change default, so the toolchain is watched by
    // hand: change rustc and this file has to run again or `Toolchain` would
    // name the old one.
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!(
        "cargo:rustc-env=BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
    println!(
        "cargo:rustc-env=BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into())
    );
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=BUILD_RUSTC={version}");
}

fn compile_dictation() {
    println!("cargo:rustc-check-cfg=cfg(concat_dictation_qa)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let source = "src/agent/dictation/apple_speech.swift";
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    println!("cargo:rerun-if-env-changed=SDKROOT");

    let tool = |arguments: &[&str]| {
        let output = std::process::Command::new("/usr/bin/xcrun")
            .args(arguments)
            .output()
            .expect("Apple dictation requires Xcode 26 or Command Line Tools 26");
        assert!(
            output.status.success(),
            "could not find the Apple dictation toolchain: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Apple toolchain path is UTF-8")
            .trim()
            .to_owned()
    };
    let compiler = std::path::PathBuf::from(tool(&["--find", "swiftc"]));
    let sdk = std::path::PathBuf::from(tool(&["--show-sdk-path"]));
    let runtime = compiler
        .parent()
        .and_then(std::path::Path::parent)
        .expect("Swift compiler is in its toolchain's bin folder")
        .join("lib/swift/macosx");
    let output = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));
    let deployment = std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "12.0".into());
    let architecture = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => panic!("unsupported Apple dictation architecture: {other:?}"),
    };
    let target = format!("{architecture}-apple-macosx{deployment}");
    let compiled = std::process::Command::new(compiler)
        .args([
            "-swift-version",
            "6",
            "-warnings-as-errors",
            "-parse-as-library",
            "-emit-library",
            "-static",
            "-O",
            "-module-name",
            "ConcatDictation",
            "-target",
            &target,
            "-sdk",
        ])
        .arg(&sdk)
        .arg("-module-cache-path")
        .arg(output.join("swift-modules"))
        .arg(source)
        .arg("-o")
        .arg(output.join("libconcat_dictation.a"))
        .output()
        .expect("could not run the Swift compiler for Apple dictation");
    assert!(
        compiled.status.success(),
        "Apple dictation requires a macOS 26 SDK and Swift 6.2 or newer: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    println!("cargo:rustc-link-search=native={}", output.display());
    println!("cargo:rustc-link-search=native={}", runtime.display());
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!(
        "cargo:rustc-link-search=framework={}",
        sdk.join("System/Library/Frameworks").display()
    );
    println!("cargo:rustc-link-lib=static=concat_dictation");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-arg=-mmacosx-version-min={deployment}");
}
