use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=NDS_SOURCE_COMMIT");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    let commit = std::env::var("NDS_SOURCE_COMMIT")
        .ok()
        .or_else(|| {
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into());
    assert!(
        commit == "unknown"
            || (commit.len() == 40 && commit.bytes().all(|c| c.is_ascii_hexdigit())),
        "NDS_SOURCE_COMMIT must be a full commit SHA"
    );
    println!("cargo:rustc-env=NDS_BUILD_COMMIT={commit}");
}
