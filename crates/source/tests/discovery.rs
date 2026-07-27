use std::fs;

use repo_intelligence_source::discover;

#[test]
fn discovery_honors_ignore_files_and_builtin_directory_exclusions() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
    fs::create_dir_all(root.path().join("generated")).unwrap();
    fs::create_dir_all(root.path().join("ignored-by-git")).unwrap();
    fs::create_dir_all(root.path().join(".claude/worktrees/jacoco-global-coverage")).unwrap();
    fs::write(root.path().join("Keep.java"), "class Keep {}").unwrap();
    fs::write(
        root.path()
            .join(".claude/worktrees/jacoco-global-coverage/Clone.java"),
        "class Clone {}",
    )
    .unwrap();
    fs::write(
        root.path().join("node_modules/pkg/Dependency.java"),
        "class Dependency {}",
    )
    .unwrap();
    fs::write(
        root.path().join("generated/Generated.java"),
        "class Generated {}",
    )
    .unwrap();
    fs::write(root.path().join(".ignore"), "generated/\n").unwrap();
    fs::write(root.path().join(".gitignore"), "ignored-by-git/\n").unwrap();
    fs::write(
        root.path().join("ignored-by-git/Ignored.java"),
        "class Ignored {}",
    )
    .unwrap();

    // The project's own index directory must be excluded even when it holds a
    // source-shaped file, so a stale index never re-enters the graph.
    fs::create_dir_all(root.path().join(".repo-intelligence")).unwrap();
    fs::write(
        root.path().join(".repo-intelligence/Leak.java"),
        "class Leak {}",
    )
    .unwrap();

    let files = discover(root.path()).unwrap();
    let paths: Vec<_> = files
        .iter()
        .map(|file| file.relative_path.to_string_lossy().to_string())
        .collect();
    assert_eq!(paths, vec!["Keep.java"]);
}
