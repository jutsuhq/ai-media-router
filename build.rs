//! Embeds every providers/*.toml and providers/fixtures/<key>/*.json so adding a provider needs no code change.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn entries(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut found: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| keep(p))
        .collect();
    found.sort();
    found
}

fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    entries(dir, |p| p.extension().is_some_and(|x| x == ext))
}

fn main() {
    println!("cargo:rerun-if-changed=providers");
    let dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("providers");
    let specs: String = files(&dir, "toml")
        .iter()
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            format!("    ({name:?}, include_str!({:?})),\n", p.display().to_string())
        })
        .collect();

    let fixtures: String = entries(&dir.join("fixtures"), Path::is_dir)
        .iter()
        .flat_map(|key_dir| {
            let key = key_dir.file_name().unwrap().to_string_lossy().into_owned();
            files(key_dir, "json").into_iter().map(move |p| {
                let name = p.file_stem().unwrap().to_string_lossy();
                format!("    ({key:?}, {name:?}, include_str!({:?})),\n", p.display().to_string())
            })
        })
        .collect();

    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("builtin_specs.rs");
    fs::write(
        out,
        format!(
            "pub static BUILTIN_SPECS: &[(&str, &str)] = &[\n{specs}];\n\
             pub static BUILTIN_FIXTURES: &[(&str, &str, &str)] = &[\n{fixtures}];\n"
        ),
    )
    .unwrap();
}
