use std::{env, path::Path, process::Command};

const PACKAGES: [&str; 8] = [
    "fontconfig",
    "harfbuzz",
    "fribidi",
    "cairo-ft",
    "libutf8proc",
    "wayland-client",
    "xkbcommon",
    "vulkan",
];

fn run(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    assert!(status.success(), "native build failed: {command:?}");
}

fn pkg_config(mode: &str) -> String {
    let output = Command::new("pkg-config")
        .arg(mode)
        .args(PACKAGES)
        .output()
        .expect("pkg-config is required for native libraries");
    assert!(
        output.status.success(),
        "native development packages missing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("pkg-config flags")
}

fn main() {
    if env::var_os("CARGO_FEATURE_GUI").is_none() {
        println!("cargo:rerun-if-changed=build.rs");
        return;
    }
    println!("cargo:rerun-if-changed=native");
    let flags = pkg_config("--cflags");
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
    let mut native = cc::Build::new();
    native
        .std("c11")
        .opt_level(2)
        .warnings_into_errors(true)
        .flag("-Wno-unused-parameter")
        .include(out)
        .files(["native/window.c", "native/font.c", "native/render.c"]);
    for flag in flags.split_whitespace() {
        native.flag(flag);
    }
    for protocol in [
        "cursor-shape",
        "fractional-scale",
        "presentation-time",
        "primary-selection",
        "tablet-v2",
        "text-input",
        "viewporter",
        "xdg-shell",
    ] {
        let xml = format!("native/protocols/{protocol}.xml");
        let source = out.join(format!("{protocol}-protocol.c"));
        run(Command::new("wayland-scanner")
            .arg("private-code")
            .arg(&xml)
            .arg(&source));
        native.file(source);
        if protocol != "tablet-v2" {
            run(Command::new("wayland-scanner")
                .arg("client-header")
                .arg(xml)
                .arg(out.join(format!("{protocol}-client.h"))));
        }
    }
    native.compile("nextty_native");
    for flag in pkg_config("--libs").split_whitespace() {
        if let Some(library) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={library}");
        } else if let Some(directory) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={directory}");
        } else {
            println!("cargo:rustc-link-arg={flag}");
        }
    }
    println!("cargo:rustc-link-lib=m");
}
