use std::process::Command;
use tempfile::TempDir;

#[test]
fn update_help_and_invalid_options_work_outside_a_repository_without_network() {
    let temp = TempDir::new().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_git-multibox"))
            .args(args)
            .current_dir(temp.path())
            .env("PATH", "")
            .output()
            .unwrap()
    };
    let help = run(&["update", "-h"]);
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("git multibox update [--check] [--force]")
    );
    for args in [
        &["update", "--unknown"][..],
        &["update", "extra"],
        &["update", "--check", "--force"],
    ] {
        let output = run(args);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("update"), "{error}");
        assert!(
            !error.contains("curl"),
            "invalid options must fail before networking: {error}"
        );
    }
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
