use std::{
    env,
    error::Error,
    ffi::OsStr,
    fs::read_dir,
    path::{Path, PathBuf},
    process::{exit, Command},
    str,
};

const LLVM_MAJOR_VERSION: usize = 20;

fn main() {
    if let Err(error) = run() {
        eprintln!("{}", error);
        exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    // The cmake crate panic()s on failure, so we do too throughout
    // build_qwerty_mlir()
    let built_qwerty_mlir = build_qwerty_mlir();

    run_bindgen(built_qwerty_mlir)
}

struct BuiltQwertyMlir {
    include_dir: PathBuf,
    lib_dir: PathBuf,
    static_lib_names: Vec<String>,
}

fn build_qwerty_mlir() -> BuiltQwertyMlir {
    let parent_dir = PathBuf::from("..");

    println!(
        "cargo::rerun-if-changed={}",
        parent_dir.join("CMakeLists.txt").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        parent_dir.join("qwerty_mlir").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        parent_dir.join("qwerty_util").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        parent_dir.join("tweedledum").display()
    );

    let install_dir = cmake::Config::new(parent_dir).generator("Ninja").build();
    let include_dir = install_dir.join("include");
    let lib_dir = install_dir.join("lib");

    // Check if include_dir is empty
    if let None = read_dir(&include_dir).unwrap().next() {
        panic!(
            "{} is an empty directory. Expected it to contain qwerty_mlir header files",
            include_dir.display()
        );
    }

    let lib_names_starting_with = |prefix| {
        let lib_paths: Vec<_> = read_dir(&lib_dir)
            .unwrap()
            .filter_map(|dirent| {
                dirent
                    .unwrap()
                    .file_name()
                    .to_str()
                    .filter(|filename| filename.starts_with(prefix))
                    .map(|s| s.to_string())
            })
            .collect();

        if lib_paths.is_empty() {
            panic!(
                "Could not find libraries starting with {} in directory {}",
                prefix,
                lib_dir.display()
            );
        }

        lib_paths
    };

    // We have to be careful with the ordering of linker args here. We need to
    // pass a topological ordering of this dependency graph:
    //
    //     libMLIRCAPIQwerty.a
    //           |
    //           V
    //     libMLIRQwerty*.a
    //           |
    //           V
    //       libqwutil.a ----> libtweedledum.a
    //           |
    //           |   libMLIRCAPIQCirc.a
    //           |      |
    //           V      V
    //      libMLIRQCirc*.a
    //
    // We choose the following topological ordering:
    // libMLIRCAPIQwerty.a, libMLIRQwerty*.a, libqwutil.a, libtweedledum.a,
    // libMLIRCAPIQCirc.a, libMLIRQCirc*.a.

    let mut static_lib_names = lib_names_starting_with("libMLIRCAPIQwerty");
    static_lib_names.append(&mut lib_names_starting_with("libMLIRQwerty"));
    static_lib_names.append(&mut lib_names_starting_with("libqwutil"));
    static_lib_names.append(&mut lib_names_starting_with("libtweedledum"));
    static_lib_names.append(&mut lib_names_starting_with("libMLIRCAPIQCirc"));
    static_lib_names.append(&mut lib_names_starting_with("libMLIRQCirc"));

    BuiltQwertyMlir {
        include_dir,
        lib_dir,
        static_lib_names,
    }
}

fn run_bindgen(built_qwerty_mlir: BuiltQwertyMlir) -> Result<(), Box<dyn Error>> {
    let version = llvm_config("--version")?;

    if !version.starts_with(&format!("{LLVM_MAJOR_VERSION}.",)) {
        return Err(format!(
            "failed to find correct version ({LLVM_MAJOR_VERSION}.x.x) of llvm-config (found {version})"
        )
        .into());
    }

    println!("cargo:rerun-if-changed=wrapper.h");

    println!(
        "cargo:rustc-link-search={}",
        built_qwerty_mlir.lib_dir.display()
    );
    for qwerty_lib_name in built_qwerty_mlir.static_lib_names {
        if let Some(name) = parse_archive_name(&qwerty_lib_name) {
            println!("cargo:rustc-link-lib=static={name}");
        }
    }

    println!("cargo:rustc-link-search={}", llvm_config("--libdir")?);

    for entry in read_dir(llvm_config("--libdir")?)? {
        if let Some(name) = entry?.path().file_name().and_then(OsStr::to_str) {
            if name.starts_with("libMLIR") {
                if let Some(name) = parse_archive_name(name) {
                    println!("cargo:rustc-link-lib=static={name}");
                }
            }
        }
    }

    println!("cargo:rustc-link-lib=MLIR");

    for name in llvm_config("--libnames")?.trim().split(' ') {
        if let Some(name) = parse_archive_name(name) {
            println!("cargo:rustc-link-lib={name}");
        }
    }

    for flag in llvm_config("--system-libs")?.trim().split(' ') {
        let flag = flag.trim_start_matches("-l");

        if flag.starts_with('/') {
            // llvm-config returns absolute paths for dynamically linked libraries.
            let path = Path::new(flag);

            println!(
                "cargo:rustc-link-search={}",
                path.parent().unwrap().display()
            );
            println!(
                "cargo:rustc-link-lib={}",
                path.file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .trim_start_matches("lib")
            );
        } else {
            println!("cargo:rustc-link-lib={flag}");
        }
    }

    if let Some(name) = get_system_libcpp() {
        println!("cargo:rustc-link-lib={name}");
    }

    bindgen::builder()
        .header("wrapper.h")
        .clang_args(vec![
            format!("-I{}", llvm_config("--includedir")?),
            format!("-I{}", built_qwerty_mlir.include_dir.display()),
        ])
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .unwrap()
        .write_to_file(Path::new(&env::var("OUT_DIR")?).join("bindings.rs"))?;

    Ok(())
}

fn get_system_libcpp() -> Option<&'static str> {
    if cfg!(target_env = "msvc") {
        None
    } else if cfg!(target_os = "macos") {
        Some("c++")
    } else {
        Some("stdc++")
    }
}

fn llvm_config(argument: &str) -> Result<String, Box<dyn Error>> {
    let prefix = env::var(format!("MLIR_SYS_{LLVM_MAJOR_VERSION}0_PREFIX"))
        .map(|path| Path::new(&path).join("bin"))
        .unwrap_or_default();
    let llvm_config_exe = if cfg!(target_os = "windows") {
        "llvm-config.exe"
    } else {
        "llvm-config"
    };

    let call = format!(
        "{} --link-static {argument}",
        prefix.join(llvm_config_exe).display(),
    );

    Ok(str::from_utf8(
        &if cfg!(target_os = "windows") {
            Command::new("cmd").args(["/C", &call]).output()?
        } else {
            Command::new("sh").arg("-c").arg(&call).output()?
        }
        .stdout,
    )?
    .trim()
    .to_string())
}

fn parse_archive_name(name: &str) -> Option<&str> {
    if let Some(name) = name.strip_prefix("lib") {
        name.strip_suffix(".a")
    } else {
        None
    }
}
