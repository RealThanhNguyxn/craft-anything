//! `fmt [--check]`: rustfmt every workspace target, as `cargo fmt --all` does, but in batches.
//!
//! `cargo fmt --all` hands rustfmt every target's root file in one command line; on Windows that
//! passed 32,767 characters and failed with "The filename or extension is too long" (os error 206).
//! The same files, with the same edition, go to rustfmt in batches that stay well under it.
//!
//! It formats the workspace members' own targets, not every local path dependency as
//! `cargo fmt --all` may: here the only other one is the patched `vendor/wgpu-hal`, which keeps
//! upstream's formatting and isn't held to ours. Like `cargo fmt`, it runs the rustfmt that
//! `RUSTFMT` names, if set.

use std::path::Path;
use std::process::Command;

/// Room for one batch's file arguments: half of Windows' 32,767-character limit, so the program,
/// its flags and any path prefix the system adds fit too.
const BATCH_CHARS: usize = 16_000;

pub fn run(root: &Path, check: bool) -> bool {
    let out = match Command::new("cargo").args(["metadata", "--no-deps", "--format-version", "1"]).current_dir(root).output() {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            eprintln!("fmt: cargo metadata failed: {}", String::from_utf8_lossy(&o.stderr).trim());
            return false;
        }
        Err(e) => {
            eprintln!("fmt: can't run cargo metadata: {e}");
            return false;
        }
    };
    let files = match targets(&out) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fmt: can't read cargo metadata: {e}");
            return false;
        }
    };
    let mut ok = true;
    for (edition, batch) in batches(&files, BATCH_CHARS) {
        let mut cmd = Command::new(std::env::var_os("RUSTFMT").unwrap_or_else(|| "rustfmt".into()));
        cmd.args(["--edition", &edition]);
        if check {
            cmd.arg("--check");
        }
        cmd.args(&batch);
        // Every batch runs, so one run lists every file that needs formatting.
        ok &= cmd.status().map(|s| s.success()).unwrap_or_else(|e| {
            eprintln!("fmt: can't run rustfmt: {e}");
            false
        });
    }
    if !ok && check {
        eprintln!("fmt: run `cargo xtask fmt` to format");
    }
    ok
}

/// Each workspace target's root file and edition, as `cargo fmt` collects them (rustfmt follows
/// their modules), without duplicates, in a stable order.
fn targets(metadata: &[u8]) -> Result<Vec<(String, String)>, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_slice(metadata)?;
    let mut files = Vec::new();
    for package in v["packages"].as_array().into_iter().flatten() {
        for target in package["targets"].as_array().into_iter().flatten() {
            if let (Some(path), Some(edition)) = (target["src_path"].as_str(), target["edition"].as_str().or(package["edition"].as_str())) {
                files.push((edition.to_string(), path.to_string()));
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Group files by edition, each batch's paths together under `limit` characters (a single longer
/// path still gets a batch of its own).
fn batches(files: &[(String, String)], limit: usize) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut len = 0;
    for (edition, path) in files {
        let cost = path.len() + 1;
        match out.last_mut() {
            Some((e, batch)) if e == edition && len + cost <= limit => batch.push(path.clone()),
            _ => {
                out.push((edition.clone(), vec![path.clone()]));
                len = 0;
            }
        }
        len += cost;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_root_is_listed_once_with_its_edition() {
        let meta = br#"{"packages":[
            {"edition":"2024","targets":[{"src_path":"/w/a/src/lib.rs","edition":"2024"},{"src_path":"/w/a/build.rs","edition":"2024"}]},
            {"edition":"2021","targets":[{"src_path":"/w/b/src/main.rs"},{"src_path":"/w/b/src/main.rs"}]}
        ]}"#;
        let t = targets(meta).unwrap();
        assert_eq!(t, [("2021".into(), "/w/b/src/main.rs".into()), ("2024".into(), "/w/a/build.rs".into()), ("2024".into(), "/w/a/src/lib.rs".into())]);
    }

    #[test]
    fn batches_stay_under_the_limit_and_keep_editions_apart() {
        let f = |e: &str, p: &str| (e.to_string(), p.to_string());
        let files = [f("2021", "aaaa"), f("2024", "bbbb"), f("2024", "cccc"), f("2024", "dddd"), f("2024", &"x".repeat(20))];
        let b = batches(&files, 10);
        assert_eq!(b.len(), 4, "{b:?}");
        assert_eq!(b[0], ("2021".into(), vec!["aaaa".into()]));
        assert_eq!(b[1].1, ["bbbb", "cccc"], "two 5-character arguments fit in 10");
        assert_eq!(b[2].1, ["dddd"]);
        assert_eq!(b[3].1, ["x".repeat(20)], "a path longer than the limit still runs on its own");
        assert!(b.iter().all(|(_, batch)| batch.len() == 1 || batch.iter().map(|p| p.len() + 1).sum::<usize>() <= 10));
        let all: Vec<String> = b.into_iter().flat_map(|(_, batch)| batch).collect();
        assert_eq!(all.len(), files.len(), "every file is formatted");
    }
}
