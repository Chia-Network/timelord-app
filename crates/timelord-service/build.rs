use std::process::Command;
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
fn main() {
    println!("cargo:rerun-if-env-changed=TIMELORD_BUILD_REVISION");
    let revision = std::env::var("TIMELORD_BUILD_REVISION")
        .ok()
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=TIMELORD_BUILD_REVISION={revision}");
    if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &reference,
        ])
    {
        println!("cargo:rerun-if-changed={path}");
    }
}
