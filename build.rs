use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    error::Error,
    ffi::OsStr,
    fs::{self, File, read_dir},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio, exit},
    str,
};

/// Logical name passed to bindgen for the in-memory wrapper. Bindgen needs a
/// `header_name` for diagnostics; this string never touches disk.
const WRAPPER_NAME: &str = "wrapper.h";

const LLVM_MAJOR_VERSION: usize = 22;

fn main() {
    if let Err(error) = run() {
        eprintln!("{}", error);
        exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");

    // The cmake crate panic()s on failure, so we do too throughout
    // build_qwerty_mlir()
    let built_qwerty_mlir = build_qwerty_mlir();

    for rerun_if_changed_entry in built_qwerty_mlir.rerun_if_changed.iter() {
        println!(
            "cargo::rerun-if-changed={}",
            rerun_if_changed_entry.display()
        );
    }

    println!(
        "cargo::metadata=bin_dir={}",
        built_qwerty_mlir.bin_dir.display()
    );

    run_bindgen(built_qwerty_mlir)
}

struct BuiltQwertyMlir {
    rerun_if_changed: Vec<PathBuf>,
    include_dir: PathBuf,
    lib_dir: PathBuf,
    bin_dir: PathBuf,
    static_lib_names: Vec<String>,
    mlir_deps_graph: HashMap<String, Vec<String>>,
}

fn build_qwerty_mlir() -> BuiltQwertyMlir {
    let parent_dir = PathBuf::from("..");

    let rerun_if_changed = vec![
        parent_dir.join("CMakeLists.txt"),
        parent_dir.join("qwerty_mlir"),
        parent_dir.join("qwerty_util"),
        parent_dir.join("tweedledum"),
    ];

    let install_dir = cmake::Config::new(parent_dir)
        .generator("Ninja")
        // Hide a wall of warnings that are from LLVM, not us
        // TODO: remove this so we don't miss useful warnings
        .configure_arg("-Wno-dev")
        .define("DUMP_MLIR_DEPS", "ON")
        .build();
    let include_dir = install_dir.join("include");
    let lib_dir = install_dir.join("lib");
    let bin_dir = install_dir.join("bin");
    let mlir_deps_tsv_path = install_dir.join("lib").join("mlir-deps.tsv");

    // Check if include_dir is empty
    for (nonempty_dir, contents_summary) in [
        (&include_dir, "header files"),
        (&bin_dir, "debugging executables"),
    ] {
        if let None = read_dir(nonempty_dir).unwrap().next() {
            panic!(
                "{} is an empty directory. Expected it to contain qwerty_mlir {}",
                include_dir.display(),
                contents_summary
            );
        }
    }

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

    let mut static_lib_names = lib_names_starting_with(&lib_dir, "libMLIRCAPIQwerty");
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libMLIRQwerty"));
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libqwutil"));
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libtweedledum"));
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libMLIRCAPIUtils"));
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libMLIRCAPIQCirc"));
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libMLIRCAPICCirc"));
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libMLIRQCirc"));
    static_lib_names.append(&mut lib_names_starting_with(&lib_dir, "libMLIRCCirc"));

    // For an explanation of what mlir-deps.tsv is, see CMakeLists.txt in the
    // parent repository.
    let mut mlir_deps_graph = HashMap::<String, Vec<String>>::new();
    let mlir_deps_tsv_fp = File::open(mlir_deps_tsv_path).unwrap();
    for mlir_deps_line_res in BufReader::new(mlir_deps_tsv_fp).lines() {
        let mlir_deps_line = mlir_deps_line_res.unwrap();
        let mut cols: Vec<String> = mlir_deps_line
            .trim()
            .split('\t')
            .map(String::from)
            .collect();
        let depender = cols.remove(0);
        mlir_deps_graph.insert(depender, cols);
    }

    BuiltQwertyMlir {
        rerun_if_changed,
        include_dir,
        lib_dir,
        bin_dir,
        static_lib_names,
        mlir_deps_graph,
    }
}

fn run_bindgen(built_qwerty_mlir: BuiltQwertyMlir) -> Result<(), Box<dyn Error>> {
    let link_mode = detect_link_mode();

    if !cfg!(feature = "no-version-check") {
        let version = llvm_config("--version", &link_mode)?;
        if !version.starts_with(&format!("{LLVM_MAJOR_VERSION}.")) {
            return Err(format!(
                "failed to find correct version ({LLVM_MAJOR_VERSION}.x.x) of llvm-config (found {version})"
            )
            .into());
        }
    }

    println!(
        "cargo:rustc-link-search={}",
        built_qwerty_mlir.lib_dir.display()
    );
    for qwerty_lib_name in built_qwerty_mlir.static_lib_names {
        if let Some(name) = parse_static_lib_name(&qwerty_lib_name) {
            println!("cargo:rustc-link-lib=static={name}");
        }
    }

    let directory = llvm_config("--libdir", &link_mode)?;
    println!("cargo:rustc-link-search={directory}");

    let mlir_lib_names: HashSet<_> =
        lib_names_starting_with(llvm_config("--libdir", &link_mode)?, "libMLIR")
            .iter()
            .filter_map(|s| parse_static_lib_name(s).map(str::to_string))
            .collect();
    for mlir_lib_name in toposort(&built_qwerty_mlir.mlir_deps_graph) {
        if mlir_lib_names.contains(&mlir_lib_name) {
            println!("cargo:rustc-link-lib=static={mlir_lib_name}");
        }
    }

    for name in llvm_config("--libnames", &link_mode)?.split(' ') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        match link_mode {
            LinkMode::Static => {
                if let Some(name) = parse_static_lib_name(name) {
                    println!("cargo:rustc-link-lib={name}");
                }
            }
            LinkMode::Shared => {
                if let Some(name) = parse_shared_lib_name(name) {
                    println!("cargo:rustc-link-lib={name}");
                }
            }
        }
    }

    for flag in llvm_config("--system-libs", &link_mode)?.split(' ') {
        let flag = flag.trim().trim_start_matches("-l");

        if flag.is_empty() {
            continue;
        }

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

    let include_dir = llvm_config("--includedir", &link_mode)?;
    let qwerty_include_dir = built_qwerty_mlir.include_dir.display().to_string();
    let wrapper_contents = generate_wrapper_contents(&include_dir, &qwerty_include_dir)?;
    let clang_args = {
        let mut args = vec![
            format!("-I{include_dir}"),
            format!("-I{qwerty_include_dir}"),
        ];
        fix_macos_sysroot(&mut args)?;
        args
    };

    bindgen::builder()
        .header_contents(WRAPPER_NAME, &wrapper_contents)
        .clang_args(clang_args)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .unwrap()
        .write_to_file(Path::new(&env::var("OUT_DIR")?).join("bindings.rs"))?;

    Ok(())
}

fn lib_names_starting_with<P: AsRef<Path>>(dir: P, prefix: &str) -> Vec<String> {
    let dir_path = dir.as_ref();
    let lib_paths: Vec<_> = read_dir(dir_path)
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
            dir_path.display()
        );
    }

    lib_paths
}

#[derive(Clone, Copy)]
enum LinkMode {
    Static,
    Shared,
}

/// Detect whether to link LLVM/MLIR statically or as shared libraries.
///
/// Checks in order:
/// 1. `MLIR_SYS_LINK_SHARED=1` env var forces shared
/// 2. Whether static libraries exist in the lib directory
/// 3. Falls back to `llvm-config --shared-mode`
fn detect_link_mode() -> LinkMode {
    if let Ok(val) = env::var("MLIR_SYS_LINK_SHARED")
        && val == "1"
    {
        return LinkMode::Shared;
    }

    // Try static first — use --libnames which actually checks for libraries.
    if try_llvm_config("--libnames", "--link-static").is_ok() {
        return LinkMode::Static;
    }

    // Static failed, try shared.
    if try_llvm_config("--libnames", "--link-shared").is_ok() {
        return LinkMode::Shared;
    }

    // Default to static (will produce a clear error later).
    LinkMode::Static
}

fn get_system_libcpp() -> Option<&'static str> {
    if env::var("CARGO_CFG_TARGET_ENV").ok()? == "msvc" {
        None
    } else if env::var("CARGO_CFG_TARGET_VENDOR").ok()? == "apple" {
        Some("c++")
    } else {
        Some("stdc++")
    }
}

fn llvm_config_command() -> Command {
    let prefix = env::var_os(format!("MLIR_SYS_{LLVM_MAJOR_VERSION}0_PREFIX"))
        .map(|path| Path::new(&path).join("bin"))
        .unwrap_or_default();

    Command::new(prefix.join(if cfg!(target_os = "windows") {
        "llvm-config.exe"
    } else {
        "llvm-config"
    }))
}

fn try_llvm_config(argument: &str, link_flag: &str) -> Result<String, Box<dyn Error>> {
    let mut command = llvm_config_command();
    command.arg(link_flag).arg(argument).stderr(Stdio::null());
    run_command(command)
}

fn llvm_config(argument: &str, link_mode: &LinkMode) -> Result<String, Box<dyn Error>> {
    let mut command = llvm_config_command();

    let link_flag = match link_mode {
        LinkMode::Static => "--link-static",
        LinkMode::Shared => "--link-shared",
    };

    command.arg(link_flag);

    // --ignore-libllvm only applies to static linking.
    if matches!(link_mode, LinkMode::Static) {
        command.arg("--ignore-libllvm");
    }

    command.arg(argument).stderr(Stdio::inherit());
    run_command(command)
}

/// Because of Apple's innovative approach to software engineering, our vanilla
/// libclang does not know where the sysroot is, and thus #including trivial C
/// standard library headers will fail during bindgen. We have to fix this
/// manually.
fn fix_macos_sysroot(clang_args: &mut Vec<String>) -> Result<(), Box<dyn Error>> {
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "macos" {
        let mut command = Command::new("xcrun");
        command.args(["--show-sdk-path"]);
        let sdk_path = run_command(command)?;
        clang_args.push(format!("--sysroot={sdk_path}"));
    }
    Ok(())
}

fn run_command(mut command: Command) -> Result<String, Box<dyn Error>> {
    let output = command
        .output()
        .map_err(|error| format!("failed to run `{command:?}`: {error}"))?;

    if !output.status.success() {
        return Err(format!("failed to run `{command:?}`: {}", output.status).into());
    }

    Ok(str::from_utf8(&output.stdout)?.trim().into())
}

fn parse_static_lib_name(name: &str) -> Option<&str> {
    if let Some(name) = name.strip_prefix("lib") {
        name.strip_suffix(".a")
    } else {
        None
    }
}

fn toposort(graph: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut sorted = Vec::new();
    let mut vertex_indegrees: HashMap<String, usize> = HashMap::new();
    let mut zero_indegree_queue = VecDeque::new();

    for (depender, dependees) in graph {
        vertex_indegrees.entry(depender.to_string()).or_insert(0);
        for dependee in dependees {
            vertex_indegrees
                .entry(dependee.to_string())
                .and_modify(|indegree| *indegree += 1)
                .or_insert(1);
        }
    }

    for (libname, indegree) in &vertex_indegrees {
        if *indegree == 0 {
            zero_indegree_queue.push_back(libname.to_string());
        }
    }

    while let Some(ref libname) = zero_indegree_queue.pop_front() {
        sorted.push(libname.to_string());

        for dependee in &graph[libname] {
            let indegree: &mut usize = vertex_indegrees.get_mut(dependee).unwrap();
            *indegree -= 1;
            if *indegree == 0 {
                zero_indegree_queue.push_back(dependee.to_string());
            }
        }
    }

    sorted
}

fn parse_shared_lib_name(name: &str) -> Option<&str> {
    let name = name.strip_prefix("lib").unwrap_or(name);

    // Handle libFoo.so, libFoo.so.22, libFoo.dylib
    if let Some(pos) = name.find(".so") {
        Some(&name[..pos])
    } else if let Some(name) = name.strip_suffix(".dylib") {
        Some(name)
    } else {
        None
    }
}

/// Walk `{includedir}/mlir-c/` and build an in-memory list of `#include`s
/// covering every header. Returned as a `String` so it can be passed to
/// bindgen via `header_contents`, avoiding any on-disk wrapper file. A
/// disk-backed wrapper would have its mtime rewritten on every build,
/// which interacts badly with bindgen's `CargoCallbacks::rerun-if-changed`
/// tracking and forces cargo to rebuild this crate (and everything that
/// depends on it) on every invocation.
fn generate_wrapper_contents(
    include_dir: &str,
    qwerty_include_dir: &str,
) -> Result<String, Box<dyn Error>> {
    let mlir_c_dir = Path::new(include_dir).join("mlir-c");
    let mut headers = Vec::new();
    collect_headers(&mlir_c_dir, &mlir_c_dir, &mut headers)?;
    headers.sort();

    let mut content = String::new();
    for header in &headers {
        content.push_str(&format!("#include <mlir-c/{header}>\n"));
    }

    headers.clear();
    let qwerty_capi_dir = Path::new(qwerty_include_dir).join("CAPI");
    collect_headers(&qwerty_capi_dir, &qwerty_capi_dir, &mut headers)?;
    for header in &headers {
        content.push_str(&format!("#include <CAPI/{header}>\n"));
    }

    Ok(content)
}

fn collect_headers(
    base: &Path,
    dir: &Path,
    headers: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            // Skip Bindings/ (Python bindings, not relevant for Rust FFI)
            if path.file_name().and_then(OsStr::to_str) == Some("Bindings") {
                continue;
            }
            collect_headers(base, &path, headers)?;
        } else if path.extension().and_then(OsStr::to_str) == Some("h") {
            let relative = path.strip_prefix(base)?;
            headers.push(relative.to_string_lossy().into_owned());
        }
    }
    Ok(())
}
