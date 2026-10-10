use pi_tools::{execute_tool, ToolRequest, ToolRuntimeOptions};

#[test]
fn read_of_a_13k_line_file_is_capped_at_2000_lines_with_resume_notice() {
    let cwd = std::env::temp_dir().join(format!("pi-read-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let content = (1..=13_000)
        .map(|i| format!("line {i}: padding to make a genuinely large file"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(cwd.join("main.rs"), content).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt
        .block_on(execute_tool(
            &cwd,
            ToolRequest::Read {
                path: "main.rs".to_string(),
                offset: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        ))
        .unwrap();

    assert!(
        result.output.chars().count() < 100_000,
        "read cap failed: {} chars",
        result.output.chars().count()
    );
    assert!(result.output.contains("showing lines 1-2000 of 13000"));
    assert!(result.output.contains("offset=2001"));
    std::fs::remove_dir_all(cwd).unwrap();
}
