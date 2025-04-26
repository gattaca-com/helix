use std::process::Command;

fn main() {
    let output = Command::new("git").args(["rev-parse", "HEAD"]).output().unwrap();
    let git_hash = String::from_utf8(output.stdout).unwrap();

    let output = Command::new("git").args(["rev-parse", "--abbrev-ref", "HEAD"]).output().unwrap();
    let git_branch = String::from_utf8(output.stdout).unwrap();

    let output = Command::new("date").args(["-u", "+%Y-%m-%dT%H:%M:%SZ"]).output().unwrap();
    let built_at = String::from_utf8(output.stdout).unwrap().trim().to_string();

    println!("cargo:rustc-env=GIT_HASH={git_hash}");
    println!("cargo:rustc-env=GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=BUILT_AT={built_at}");
}
