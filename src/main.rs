use anyhow::Result;
use std::ffi::OsStr;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("kilnr: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let argv = std::env::args().collect::<Vec<_>>();
    let invoked = Path::new(&argv[0])
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("kilnr");
    let helpers = [
        "web",
        "controller",
        "execute",
        "enqueue",
        "cleanup",
        "rerun",
        "project-delete",
        "project-rename",
        "project-webhook-set",
        "project-lock-run",
        "git-key-add",
        "secret-set",
        "secret-set-file",
        "secret-list",
        "secret-delete",
        "notify-discord",
        "permissions",
        "project-create",
        "config-tool",
    ];
    if helpers.contains(&invoked) {
        return kilnr::ops::helper(invoked, &argv[1..]);
    }
    kilnr::ops::cli(&argv[1..])
}
