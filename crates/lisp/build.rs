//! Compile the C shim and link the embedded Common Lisp image.
//!
//! ECL ships no `.pc` file, so pkg-config is not an option; `ecl-config` is the
//! supported way to discover flags and it drives both halves of the build —
//! `--cflags` for the shim compile, `--libs` for the link line.

use std::path::PathBuf;
use std::process::Command;

fn ecl_config(flag: &str) -> String {
    let out = Command::new("ecl-config")
        .arg(flag)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "could not run `ecl-config {flag}`: {e}\n\
                 Install ECL (macOS: `brew install ecl`)."
            )
        });
    if !out.status.success() {
        panic!(
            "`ecl-config {flag}` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8(out.stdout).expect("ecl-config emitted non-UTF-8")
}

fn main() {
    println!("cargo:rerun-if-changed=src/shim.c");

    let cflags = ecl_config("--cflags");
    let libs = ecl_config("--libs");

    let mut build = cc::Build::new();
    build.file("src/shim.c");
    for tok in cflags.split_whitespace() {
        if let Some(dir) = tok.strip_prefix("-I") {
            build.include(dir);
        } else if let Some(def) = tok.strip_prefix("-D") {
            // ECL's headers switch on these (e.g. -Ddarwin); the -g/-O flags in
            // the same output are cc's business, not ours.
            match def.split_once('=') {
                Some((k, v)) => build.define(k, v),
                None => build.define(def, None),
            };
        }
    }

    // `ecl.h` includes `<gc/gc.h>`, but --cflags does not mention Boehm's
    // prefix — only --libs does. Turn every library dir into its sibling
    // include dir; that recovers bdw-gc (and gmp) wherever they are installed.
    for tok in libs.split_whitespace() {
        if let Some(dir) = tok.strip_prefix("-L") {
            let include = PathBuf::from(dir).join("../include");
            if include.is_dir() {
                build.include(include);
            }
        }
    }

    build.compile("zemacs_shim");

    for tok in libs.split_whitespace() {
        if let Some(lib) = tok.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={lib}");
        } else if let Some(dir) = tok.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else if let Some(dir) = tok.strip_prefix("-Wl,-rpath,") {
            // Only reaches this crate's own test binaries — Cargo does not
            // propagate link args to dependents — but Homebrew's libecl has an
            // absolute install name, so dependents do not need it.
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        }
    }
}
