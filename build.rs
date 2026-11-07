use std::{env, fs, path::Path, process::Command};

fn run(command: &mut Command) {
    let status = command.status().expect("run native build tool");
    assert!(status.success(), "native build failed: {command:?}");
}

fn sources(directory: &Path, result: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).expect("read native sources") {
        let path = entry.expect("native source entry").path();
        if path.is_dir() {
            sources(&path, result);
        } else if path.extension().is_some_and(|ext| ext == "c") {
            result.push(path);
        }
    }
}

fn main() {
    if env::var_os("CARGO_FEATURE_GUI").is_none() {
        println!("cargo:rerun-if-changed=build.rs");
        return;
    }
    println!("cargo:rerun-if-changed=native");
    let packages = [
        "fontconfig",
        "harfbuzz",
        "fribidi",
        "cairo-ft",
        "libutf8proc",
        "wayland-client",
        "xkbcommon",
        "vulkan",
    ];
    let output = Command::new("pkg-config")
        .arg("--cflags")
        .args(packages)
        .output()
        .expect("pkg-config is required for native libraries");
    assert!(
        output.status.success(),
        "native development packages missing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let flags = String::from_utf8(output.stdout).expect("pkg-config flags");
    let out = env::var_os("OUT_DIR").expect("Cargo OUT_DIR");
    let out = Path::new(&out);
    for (stage, name) in [("vert", "vertex"), ("frag", "fragment")] {
        run(Command::new("glslangValidator")
            .args(["-V", "--target-env", "vulkan1.3", "--vn"])
            .arg(format!("nt_{name}_spv"))
            .arg("-o")
            .arg(out.join(format!("terminal_{stage}_spv.h")))
            .arg(format!("native/terminal.{stage}")));
    }
    let mut files = Vec::new();
    sources(Path::new("native"), &mut files);
    files.sort_unstable();
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let debug = env::var("DEBUG").expect("Cargo DEBUG") == "true";
    let archive_path = out.join("libnextty_native.a");
    let mut archive = Command::new("ar");
    archive.arg("crs").arg(&archive_path);
    for (index, file) in files.iter().enumerate() {
        let object = out.join(format!("native-{index}.o"));
        let mut cc = Command::new(&compiler);
        cc.args([
            "-std=c11",
            "-O2",
            "-fPIC",
            "-Wall",
            "-Wextra",
            "-Wno-unused-parameter",
            "-Werror",
            "-Inative",
        ])
        .args(debug.then_some("-g"))
        .arg("-I")
        .arg(out)
        .args(flags.split_whitespace())
        .arg("-c")
        .arg(file)
        .arg("-o")
        .arg(&object);
        run(&mut cc);
        archive.arg(object);
    }
    if let Err(error) = fs::remove_file(&archive_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        panic!("remove {}: {error}", archive_path.display());
    }
    run(&mut archive);
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=nextty_native");
    let output = Command::new("pkg-config")
        .arg("--libs")
        .args(packages)
        .output()
        .expect("pkg-config link flags");
    assert!(output.status.success(), "pkg-config link failed");
    for flag in String::from_utf8(output.stdout).unwrap().split_whitespace() {
        if let Some(library) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={library}");
        } else if let Some(directory) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={directory}");
        } else {
            println!("cargo:rustc-link-arg={flag}");
        }
    }
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rerun-if-env-changed=CC");
}
