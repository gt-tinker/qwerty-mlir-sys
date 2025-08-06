use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    error::Error,
    fs::{File, read_dir},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, exit},
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

    let mlir_lib_names: HashSet<_> = lib_names_starting_with(llvm_config("--libdir")?, "libMLIR")
        .iter()
        .filter_map(|s| parse_archive_name(s).map(str::to_string))
        .collect();
    for mlir_lib_name in toposort(&built_qwerty_mlir.mlir_deps_graph) {
        if mlir_lib_names.contains(&mlir_lib_name) {
            println!("cargo:rustc-link-lib=static={mlir_lib_name}");
        }
    }

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
