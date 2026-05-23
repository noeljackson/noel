use pi_parity::{
    docs_dir, fixture_dir, fixture_script_name, json_file_names, load_fixture, repo_root,
    script_dir,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

const EXPECTED_FIXTURES: &[&str] = &[
    "agent-tool-loop.json",
    "anthropic-api-key.json",
    "anthropic-claude-code-oauth.json",
    "anthropic-tools.json",
    "auth-discovery.json",
    "bedrock-claude-opus-4.6.json",
    "bedrock-tools.json",
    "cli-contract.json",
    "cloudflare-ai-gateway-kimi.json",
    "github-copilot-gpt-5.4.json",
    "google-gemini-2.5-pro.json",
    "google-tool-image-routing.json",
    "google-tools.json",
    "local-tools.json",
    "mistral-devstral.json",
    "mistral-tool-id.json",
    "mistral-tools.json",
    "model-catalog.json",
    "openai-codex-chatgpt-oauth.json",
    "openai-responses-tool-id.json",
    "openai-responses-tools.json",
    "openrouter-kimi.json",
    "session-export.json",
    "settings.json",
    "slash-commands.json",
    "tui-transcripts.json",
];

const EXPECTED_SCRIPTS: &[&str] = &[
    "agent-tool-loop-fixture.ts",
    "anthropic-oauth-fixture.ts",
    "auth-discovery-fixture.ts",
    "bedrock-fixture.ts",
    "cli-contract-fixture.ts",
    "cloudflare-fixture.ts",
    "codex-oauth-fixture.ts",
    "copilot-fixture.ts",
    "google-fixture.ts",
    "local-tools-fixture.ts",
    "mistral-fixture.ts",
    "model-catalog-fixture.ts",
    "openai-tools-fixture.ts",
    "openrouter-fixture.ts",
    "session-export-fixture.ts",
    "settings-fixture.ts",
    "slash-command-fixture.ts",
    "tui-transcript-fixture.ts",
];

#[test]
fn fixture_and_script_inventory_is_declared() {
    let fixtures = json_file_names(fixture_dir());
    assert_eq!(fixtures, EXPECTED_FIXTURES);

    let mut scripts = fs::read_dir(script_dir())
        .expect("read script dir")
        .map(|entry| entry.expect("read dir entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.ends_with("-fixture.ts"))
        .collect::<Vec<_>>();
    scripts.sort();
    assert_eq!(scripts, EXPECTED_SCRIPTS);
}

#[test]
fn fixtures_have_source_metadata_and_payloads() {
    for fixture_name in EXPECTED_FIXTURES {
        let fixture = load_fixture(fixture_name);
        let source = fixture
            .get("source")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{fixture_name} missing source object"));
        assert_eq!(
            source.get("repository").and_then(Value::as_str),
            Some("https://github.com/earendil-works/pi")
        );
        assert_eq!(source.get("ref").and_then(Value::as_str), Some("main"));
        let script = fixture_script_name(&fixture);
        assert!(
            EXPECTED_SCRIPTS.contains(&script),
            "{fixture_name} references undeclared script {script}"
        );
        let object = fixture.as_object().expect("fixture root object");
        assert!(
            object.keys().any(|key| key != "source"),
            "{fixture_name} must contain data beyond source metadata"
        );
    }
}

#[test]
fn every_script_output_is_committed() {
    let mut by_script: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for fixture_name in EXPECTED_FIXTURES {
        let fixture = load_fixture(fixture_name);
        by_script
            .entry(fixture_script_name(&fixture).to_string())
            .or_default()
            .insert(fixture_name.to_string());
    }

    let expected = [
        (
            "agent-tool-loop-fixture.ts",
            fixture_set(&["agent-tool-loop.json"]),
        ),
        (
            "anthropic-oauth-fixture.ts",
            fixture_set(&[
                "anthropic-api-key.json",
                "anthropic-claude-code-oauth.json",
                "anthropic-tools.json",
            ]),
        ),
        (
            "auth-discovery-fixture.ts",
            fixture_set(&["auth-discovery.json"]),
        ),
        (
            "bedrock-fixture.ts",
            fixture_set(&["bedrock-claude-opus-4.6.json", "bedrock-tools.json"]),
        ),
        (
            "cli-contract-fixture.ts",
            fixture_set(&["cli-contract.json"]),
        ),
        (
            "cloudflare-fixture.ts",
            fixture_set(&["cloudflare-ai-gateway-kimi.json"]),
        ),
        (
            "codex-oauth-fixture.ts",
            fixture_set(&["openai-codex-chatgpt-oauth.json"]),
        ),
        (
            "copilot-fixture.ts",
            fixture_set(&["github-copilot-gpt-5.4.json"]),
        ),
        (
            "google-fixture.ts",
            fixture_set(&[
                "google-gemini-2.5-pro.json",
                "google-tool-image-routing.json",
                "google-tools.json",
            ]),
        ),
        ("local-tools-fixture.ts", fixture_set(&["local-tools.json"])),
        (
            "mistral-fixture.ts",
            fixture_set(&[
                "mistral-devstral.json",
                "mistral-tool-id.json",
                "mistral-tools.json",
            ]),
        ),
        (
            "model-catalog-fixture.ts",
            fixture_set(&["model-catalog.json"]),
        ),
        (
            "openai-tools-fixture.ts",
            fixture_set(&[
                "openai-responses-tool-id.json",
                "openai-responses-tools.json",
            ]),
        ),
        (
            "openrouter-fixture.ts",
            fixture_set(&["openrouter-kimi.json"]),
        ),
        (
            "session-export-fixture.ts",
            fixture_set(&["session-export.json"]),
        ),
        ("settings-fixture.ts", fixture_set(&["settings.json"])),
        (
            "slash-command-fixture.ts",
            fixture_set(&["slash-commands.json"]),
        ),
        (
            "tui-transcript-fixture.ts",
            fixture_set(&["tui-transcripts.json"]),
        ),
    ]
    .into_iter()
    .map(|(script, fixtures)| (script.to_string(), fixtures))
    .collect::<BTreeMap<_, _>>();

    assert_eq!(by_script, expected);
}

fn fixture_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

#[test]
fn provider_request_headers_are_redacted() {
    for fixture_name in EXPECTED_FIXTURES {
        let fixture = load_fixture(fixture_name);
        let Some(headers) = fixture
            .pointer("/request/headers")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (name, value) in headers {
            let Some(value) = value.as_str() else {
                continue;
            };
            let header = name.to_ascii_lowercase();
            if header.contains("authorization") || header.contains("api-key") {
                assert!(
                    value.contains("<redacted>"),
                    "{fixture_name} header {name} must be redacted"
                );
            }
        }
    }
}

#[test]
fn committed_fixtures_are_referenced_by_rust_parity_tests() {
    let expected = [
        ("agent-tool-loop.json", "crates/pi-core/src/lib.rs"),
        ("anthropic-api-key.json", "crates/pi-ai/src/lib.rs"),
        (
            "anthropic-claude-code-oauth.json",
            "crates/pi-ai/src/lib.rs",
        ),
        ("anthropic-tools.json", "crates/pi-ai/src/lib.rs"),
        ("auth-discovery.json", "crates/pi-config/src/lib.rs"),
        ("bedrock-claude-opus-4.6.json", "crates/pi-ai/src/lib.rs"),
        ("bedrock-tools.json", "crates/pi-ai/src/lib.rs"),
        ("cli-contract.json", "crates/pi-cli/src/main.rs"),
        ("cloudflare-ai-gateway-kimi.json", "crates/pi-ai/src/lib.rs"),
        ("github-copilot-gpt-5.4.json", "crates/pi-ai/src/lib.rs"),
        ("google-gemini-2.5-pro.json", "crates/pi-ai/src/lib.rs"),
        ("google-tool-image-routing.json", "crates/pi-ai/src/lib.rs"),
        ("google-tools.json", "crates/pi-ai/src/lib.rs"),
        ("local-tools.json", "crates/pi-tools/src/lib.rs"),
        ("mistral-devstral.json", "crates/pi-ai/src/lib.rs"),
        ("mistral-tool-id.json", "crates/pi-ai/src/lib.rs"),
        ("mistral-tools.json", "crates/pi-ai/src/lib.rs"),
        ("model-catalog.json", "crates/pi-config/src/lib.rs"),
        ("openai-codex-chatgpt-oauth.json", "crates/pi-ai/src/lib.rs"),
        ("openai-responses-tool-id.json", "crates/pi-ai/src/lib.rs"),
        ("openai-responses-tools.json", "crates/pi-ai/src/lib.rs"),
        ("openrouter-kimi.json", "crates/pi-ai/src/lib.rs"),
        ("session-export.json", "crates/pi-core/src/lib.rs"),
        ("settings.json", "crates/pi-config/src/lib.rs"),
        ("slash-commands.json", "crates/pi-tui/src/lib.rs"),
        ("tui-transcripts.json", "crates/pi-tui/src/lib.rs"),
    ]
    .into_iter()
    .collect::<BTreeMap<_, _>>();

    assert_eq!(
        expected.keys().copied().collect::<Vec<_>>(),
        EXPECTED_FIXTURES
    );

    for (fixture_name, source_path) in expected {
        let path = repo_root().join(source_path);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        assert!(
            source.contains(fixture_name),
            "{source_path} must reference {fixture_name}"
        );
    }
}

#[test]
fn parity_docs_track_every_fixture_and_non_parity_decisions() {
    let status =
        fs::read_to_string(docs_dir().join("parity-status.md")).expect("read parity status doc");
    for fixture_name in EXPECTED_FIXTURES {
        assert!(
            status.contains(fixture_name),
            "parity-status.md must mention {fixture_name}"
        );
    }

    let non_parity = fs::read_to_string(docs_dir().join("non-parity-register.md"))
        .expect("read non-parity register");
    assert!(non_parity.contains("## Active Non-Parity Decisions"));
}
