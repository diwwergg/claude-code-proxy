use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn models_advertise_copilot_namespace_without_gpt_fast_duplicates() {
    let mut cmd = Command::cargo_bin("claude-code-proxy").unwrap();
    cmd.arg("models")
        .assert()
        .success()
        .stdout(predicate::str::contains("github-copilot:gpt-5.4"))
        .stdout(predicate::str::contains("gpt-5.4-fast").not())
        .stdout(predicate::str::contains("gpt-5.6-sol-fast").not());
}

#[test]
fn github_copilot_help_exposes_auth_copy_and_models() {
    let mut cmd = Command::cargo_bin("claude-code-proxy").unwrap();
    cmd.args(["github-copilot", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("auth"))
        .stdout(predicate::str::contains("copy"))
        .stdout(predicate::str::contains("models"));
}

#[test]
fn github_copilot_copy_sources_parse_without_reaching_io() {
    for source in ["vscode", "opencode"] {
        let mut cmd = Command::cargo_bin("claude-code-proxy").unwrap();
        cmd.args(["github-copilot", "copy", source, "--help"])
            .assert();
    }
}

#[test]
fn github_copilot_copy_rejects_unknown_source_at_cli_layer() {
    let mut cmd = Command::cargo_bin("claude-code-proxy").unwrap();
    cmd.args(["github-copilot", "copy", "unknown"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value"));
}

#[test]
fn github_copilot_import_remains_a_compatibility_alias() {
    let mut cmd = Command::cargo_bin("claude-code-proxy").unwrap();
    cmd.args(["github-copilot", "import", "unknown"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value"));
}
