use std::{env, fs, path::PathBuf};

fn main() {
    let dir = PathBuf::from("../../docs/v1/schemas")
        .canonicalize()
        .unwrap();
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries = Vec::new();
    for path in fs::read_dir(dir).unwrap() {
        let path = path.unwrap().path();
        let file = path.file_name().unwrap().to_str().unwrap();
        if let Some(name) = file.strip_suffix(".schema.json") {
            entries.push(format!("({name:?}, include_str!({path:?}))"));
        }
    }
    entries.sort();
    let dest = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("contracts.rs");
    fs::write(dest, format!("&[{}]", entries.join(",\n"))).unwrap();
}
