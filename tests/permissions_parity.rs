use kilnr::permissions;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;

#[test]
fn test_normalize_build_metadata_repairs_only_managed_files() {
    let root = tempfile::tempdir().unwrap();
    let builds = root.path().join("builds");
    let build = builds.join("20260828-demo-abc");
    fs::create_dir_all(&build).unwrap();
    let managed = [
        "job.json",
        "status.json",
        "runtime.json",
        "pipeline.mk",
        "status.lock",
    ];
    for name in managed {
        let path = build.join(name);
        fs::write(&path, "{}\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o650)).unwrap();
    }
    let unrelated = build.join("custom-output");
    fs::write(&unrelated, "keep\n").unwrap();
    fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o650)).unwrap();
    permissions::normalize_build_metadata(&builds).unwrap();
    for name in managed {
        assert_eq!(
            fs::metadata(build.join(name)).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
    assert_eq!(
        fs::metadata(unrelated).unwrap().permissions().mode() & 0o777,
        0o650
    );
}

#[test]
fn test_normalize_build_metadata_rejects_a_managed_symlink() {
    let root = tempfile::tempdir().unwrap();
    let builds = root.path().join("builds");
    let build = builds.join("20260828-demo-abc");
    fs::create_dir_all(&build).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, "{}\n").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o650)).unwrap();
    std::os::unix::fs::symlink(&outside, build.join("job.json")).unwrap();
    assert!(permissions::normalize_build_metadata(&builds)
        .unwrap_err()
        .to_string()
        .contains("unsafe"));
    assert_eq!(
        fs::metadata(outside).unwrap().permissions().mode() & 0o777,
        0o650
    );
}

#[test]
fn test_repository_ref_acl_normalization_replaces_inherited_other_access() {
    if !cfg!(target_os = "linux")
        || Command::new("sh")
            .args([
                "-c",
                "command -v getfacl >/dev/null && command -v setfacl >/dev/null",
            ])
            .status()
            .map_or(true, |status| !status.success())
    {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let jobs = root.path().join("demo.git/refs/kilnr/jobs");
    fs::create_dir_all(&jobs).unwrap();
    let loose = jobs.join("20260828-demo-abc");
    fs::write(&loose, "deadbeef\n").unwrap();
    let uid = fs::metadata(root.path()).unwrap().uid();
    permissions::normalize_repository_refs(&root.path().join("demo.git"), uid).unwrap();
    let directory_acl = Command::new("getfacl")
        .args(["-cpn", jobs.to_str().unwrap()])
        .output()
        .unwrap();
    let loose_acl = Command::new("getfacl")
        .args(["-cpn", loose.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&directory_acl.stdout).contains("default:other::---"));
    assert!(String::from_utf8_lossy(&loose_acl.stdout).contains("other::---"));
    assert_eq!(
        fs::metadata(jobs).unwrap().permissions().mode() & 0o777,
        0o770
    );
    assert_eq!(
        fs::metadata(loose).unwrap().permissions().mode() & 0o777,
        0o660
    );
}
