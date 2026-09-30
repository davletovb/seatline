use std::process::Command;

use seatline_companion::config;

const EXE: &str = env!("CARGO_BIN_EXE_seatline-companion");

fn root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "seatline-authorize-{}",
        &config::random_token().unwrap()[..12]
    ))
}

#[allow(clippy::disallowed_methods)] // Runs the built binary, never provider execution.
fn authorize(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(EXE)
        .env("SEATLINE_DATA_DIR", root)
        .arg("authorize")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn a_mistyped_origin_fails_and_grants_nothing() {
    let root = root();
    let output = authorize(&root, &["my_app", "codex", "http://localhost:5173"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("only https://"));
    assert!(!config::app_path(&root, "my_app").unwrap().exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn provider_default_tools_are_granted_only_with_the_explicit_option() {
    let root = root();
    let plain = authorize(&root, &["my_app", "codex"]);
    assert!(plain.status.success());
    assert!(
        !config::load_grant(&root, "my_app")
            .unwrap()
            .allow_provider_default
    );
    assert!(String::from_utf8_lossy(&plain.stdout).contains("provider-default tools denied"));

    let opted_in = authorize(&root, &["my_app", "codex", "--allow-provider-default"]);
    assert!(opted_in.status.success());
    assert!(
        config::load_grant(&root, "my_app")
            .unwrap()
            .allow_provider_default
    );
    assert!(String::from_utf8_lossy(&opted_in.stdout).contains("provider-default tools allowed"));
    let _ = std::fs::remove_dir_all(root);
}
