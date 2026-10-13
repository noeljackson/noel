use pi_tools::{execute_tool, ToolRequest, ToolRuntimeOptions};

#[test]
fn read_of_a_13k_line_file_is_capped_at_50_kb_with_resume_notice() {
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

    // 50 KB is reached long before 2000 of these lines.
    let (shown, notice) = result
        .output
        .rsplit_once("\n\n")
        .expect("truncation notice");
    assert!(
        shown.len() <= 50 * 1024,
        "read cap failed: {} bytes",
        shown.len()
    );
    let lines = shown.lines().count();
    assert!(shown.ends_with(&format!(
        "line {lines}: padding to make a genuinely large file"
    )));
    assert_eq!(
        notice,
        format!(
            "[Showing lines 1-{lines} of 13000 (50.0KB limit). Use offset={} to continue.]",
            lines + 1
        )
    );
    std::fs::remove_dir_all(cwd).unwrap();
}
