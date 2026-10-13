use serde_json::Value;
use std::fs;
use std::path::PathBuf;

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

pub fn fixture_dir() -> PathBuf {
    repo_root().join("tests").join("fixtures").join("ts-parity")
}

pub fn script_dir() -> PathBuf {
    repo_root().join("scripts").join("ts-parity")
}

pub fn docs_dir() -> PathBuf {
    repo_root().join("docs").join("rust-rewrite")
}

pub fn load_fixture(name: &str) -> Value {
    let path = fixture_dir().join(name);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()))
}

pub fn json_file_names(dir: PathBuf) -> Vec<String> {
    let mut names = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
        .map(|entry| entry.expect("read dir entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

pub fn fixture_script_name(fixture: &Value) -> &str {
    fixture
        .pointer("/source/script")
        .and_then(Value::as_str)
        .and_then(|script| script.strip_prefix("/ts-parity/"))
        .expect("fixture source.script must be /ts-parity/<script>")
}
