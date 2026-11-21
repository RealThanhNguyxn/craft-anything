//! A named chat must not run an AI the person turned off (`ai off` says nothing uses it).
//!
//! The test gives the binary its own config, home and `PATH`, and a stand-in `claude` that writes
//! a marker if it is started at all, so neither the person's settings nor an installed agent can
//! decide the result.
#![cfg(unix)]

use craft_anything_platform::process::{Output, Process};
use std::path::Path;
use std::time::Duration;

fn cli(args: &[&str], config: &Path, home: &Path, bin: &Path) -> Output {
    let path = format!("{}:/usr/bin:/bin", bin.display());
    Process::new(env!("CARGO_BIN_EXE_craft-anything-cli"))
        .args(args)
        .env_clear()
        .env("CRAFT_ANYTHING_CONFIG_DIR", config)
        .env("HOME", home)
        .env("PATH", path)
        .timeout(Duration::from_secs(60))
        .run()
        .unwrap()
}

#[test]
fn a_named_chat_does_not_start_a_turned_off_agent() {
    use std::os::unix::fs::PermissionsExt;
    let config = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let marker = config.path().join("ran.txt");
    let program = bin.path().join("claude");
    std::fs::write(&program, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n", marker.display())).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        config.path().join("config.toml"),
        format!("[[ai.backends]]\nname = \"Fake\"\nkind = \"agent_cli\"\nmodel = \"claude\"\nenabled = false\ncommand = \"{}\"\n", program.display()),
    )
    .unwrap();

    let out = cli(&["chat", "hi", "--backend", "Fake"], config.path(), home.path(), bin.path());
    let log = format!("{}{}", out.stdout_text(), out.stderr_text());
    assert!(!out.success, "the chat must be refused before it starts the agent:\n{log}");
    assert!(log.contains("turned off"), "the refusal must say why:\n{log}");
    assert!(!marker.exists(), "the turned-off agent ran:\n{log}");
}
