use std::fs;
use std::process::Command;

#[test]
fn scan_and_search_work_with_json_output() {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(
        workspace.path().join("Dto.java"),
        "class Dto { private String name; }",
    )
    .unwrap();
    let data = tempfile::tempdir().unwrap();
    let database = data.path().join("workspace.sqlite");
    let binary = env!("CARGO_BIN_EXE_repo-intelligence");

    let scan = Command::new(binary)
        .args([
            "--database",
            database.to_str().unwrap(),
            "scan",
            workspace.path().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "{}",
        String::from_utf8_lossy(&scan.stderr)
    );

    let search = Command::new(binary)
        .args([
            "--database",
            database.to_str().unwrap(),
            "search",
            "name",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(response["status"], "success");
    assert!(!response["data"].as_array().unwrap().is_empty());
}
