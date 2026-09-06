use kilnr::project_lock::{self, Mode, ProjectLocks};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn provision(root: &std::path::Path, names: &[&str]) {
    fs::set_permissions(root, fs::Permissions::from_mode(0o750)).unwrap();
    project_lock::provision(
        root,
        &names.iter().map(|name| (*name).into()).collect::<Vec<_>>(),
    )
    .unwrap();
}

#[test]
fn test_project_name_validation_accepts_boundaries() {
    for name in ["a", "z9", "demo_name-2", &"a".repeat(63)] {
        assert_eq!(project_lock::validate_name(name).unwrap(), name);
    }
}

#[test]
fn test_project_name_validation_rejects_invalid_and_path_like_names() {
    for name in [
        "",
        "A",
        "-demo",
        "_demo",
        "demo.",
        "demo/name",
        "../demo",
        "demo\\name",
        &"a".repeat(64),
    ] {
        assert!(project_lock::validate_name(name).is_err(), "{name:?}");
    }
}

#[test]
fn test_project_locks_deduplicate_and_acquire_names_in_sorted_order() {
    let root = tempfile::tempdir().unwrap();
    provision(root.path(), &["zeta", "alpha"]);
    let locks = ProjectLocks::acquire(
        root.path(),
        &["zeta".into(), "alpha".into(), "zeta".into()],
        Mode::Exclusive,
        false,
    )
    .unwrap();
    assert_eq!(locks.names(), ["alpha", "zeta"]);
}

#[test]
fn test_project_locks_rejects_a_symlinked_root() {
    let base = tempfile::tempdir().unwrap();
    let target = base.path().join("target");
    fs::create_dir(&target).unwrap();
    provision(&target, &["demo"]);
    let alias = base.path().join("locks");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    assert!(ProjectLocks::acquire(&alias, &["demo".into()], Mode::Exclusive, false).is_err());
}

#[test]
fn test_project_locks_rejects_a_symlinked_intermediate_ancestor() {
    let base = tempfile::tempdir().unwrap();
    let real = base.path().join("real");
    let root = real.join("state/locks/projects");
    fs::create_dir_all(&root).unwrap();
    provision(&root, &["demo"]);
    let alias = base.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert!(ProjectLocks::acquire(
        &alias.join("state/locks/projects"),
        &["demo".into()],
        Mode::Exclusive,
        false
    )
    .is_err());
    assert!(root.join("demo.lock").is_file());
}

#[test]
fn test_project_locks_rejects_a_symlinked_lock_entry() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
    let target = root.path().join("target.lock");
    fs::write(&target, "outside lock").unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("demo.lock")).unwrap();
    assert!(ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Exclusive, false).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "outside lock");
}

#[test]
fn test_shared_lock_allows_another_shared_lock() {
    let root = tempfile::tempdir().unwrap();
    provision(root.path(), &["demo"]);
    let _first = ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Shared, false).unwrap();
    let _second = ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Shared, true).unwrap();
}

#[test]
fn test_exclusive_lock_excludes_shared_lock() {
    let root = tempfile::tempdir().unwrap();
    provision(root.path(), &["demo"]);
    {
        let _exclusive =
            ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Exclusive, false).unwrap();
        assert!(ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Shared, true).is_err());
    }
    assert!(ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Shared, true).is_ok());
}

#[test]
fn test_partial_lock_failure_releases_the_first_sorted_lock() {
    let root = tempfile::tempdir().unwrap();
    provision(root.path(), &["alpha", "zeta"]);
    let _zeta =
        ProjectLocks::acquire(root.path(), &["zeta".into()], Mode::Exclusive, false).unwrap();
    assert!(ProjectLocks::acquire(
        root.path(),
        &["zeta".into(), "alpha".into()],
        Mode::Exclusive,
        true
    )
    .is_err());
    assert!(ProjectLocks::acquire(root.path(), &["alpha".into()], Mode::Exclusive, true).is_ok());
}

#[test]
fn test_project_lock_releases_after_exception() {
    let root = tempfile::tempdir().unwrap();
    provision(root.path(), &["demo"]);
    let result = std::panic::catch_unwind(|| {
        let _lock =
            ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Exclusive, false).unwrap();
        panic!("abort lifecycle operation");
    });
    assert!(result.is_err());
    assert!(ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Shared, true).is_ok());
}

#[test]
fn test_provisioned_lock_file_is_group_writable_and_inherits_lock_root_group() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o2750)).unwrap();
    project_lock::provision(root.path(), &["demo".into()]).unwrap();
    let lock = fs::metadata(root.path().join("demo.lock")).unwrap();
    let parent = fs::metadata(root.path()).unwrap();
    assert_eq!(lock.permissions().mode() & 0o777, 0o660);
    assert_eq!(lock.uid(), parent.uid());
    assert_eq!(lock.gid(), parent.gid());
}

#[test]
fn test_lock_acquisition_requires_a_preprovisioned_entry() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
    assert!(ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Exclusive, false).is_err());
    assert!(!root.path().join("demo.lock").exists());
}

#[test]
fn test_submitter_cannot_unlink_recreate_or_split_a_held_lock_inode() {
    let root = tempfile::tempdir().unwrap();
    provision(root.path(), &["demo"]);
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o550)).unwrap();
    let _lock =
        ProjectLocks::acquire(root.path(), &["demo".into()], Mode::Exclusive, false).unwrap();
    let inode = fs::metadata(root.path().join("demo.lock")).unwrap().ino();
    assert!(fs::remove_file(root.path().join("demo.lock")).is_err());
    assert!(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.path().join("replacement"))
        .is_err());
    assert_eq!(
        fs::metadata(root.path().join("demo.lock")).unwrap().ino(),
        inode
    );
}

fn namespace_policy() -> project_lock::NamespacePolicy {
    let metadata = fs::metadata(".").unwrap();
    project_lock::NamespacePolicy {
        root_uid: metadata.uid(),
        kilnr_uid: metadata.uid(),
        kilnr_gid: metadata.gid(),
        submit_gid: metadata.gid(),
        state_traverse_group_gids: vec![],
        lock_traverse_user_uids: vec![],
    }
}

fn acl_tools() -> bool {
    cfg!(target_os = "linux")
        && std::process::Command::new("sh")
            .args([
                "-c",
                "command -v getfacl >/dev/null && command -v setfacl >/dev/null",
            ])
            .status()
            .is_ok_and(|status| status.success())
}

#[test]
fn test_submitter_cannot_replace_project_lock_namespace_through_ancestors() {
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("kilnr");
    project_lock::provision_namespace(&state, &namespace_policy()).unwrap();
    let locks = state.join("locks");
    let projects = locks.join("projects");
    for path in [&state, &locks, &projects] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o550)).unwrap();
    }
    assert!(fs::rename(&projects, locks.join("projects-replaced")).is_err());
    assert!(fs::rename(&locks, state.join("locks-replaced")).is_err());
}

#[test]
fn test_lock_namespace_removes_legacy_acls_through_validated_descriptors() {
    if !acl_tools() {
        return;
    }
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("kilnr");
    fs::create_dir_all(state.join("locks/projects")).unwrap();
    for path in [&state, &state.join("locks"), &state.join("locks/projects")] {
        let status = std::process::Command::new("setfacl")
            .args(["-m", "d:o::r-x"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }
    project_lock::provision_namespace(&state, &namespace_policy()).unwrap();
    for path in [&state, &state.join("locks"), &state.join("locks/projects")] {
        let output = std::process::Command::new("getfacl")
            .args(["-cpn"])
            .arg(path)
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&output.stdout).contains("default:"));
    }
}

#[test]
fn test_production_namespace_policy_supplies_exact_traversal_identities() {
    let policy = project_lock::NamespacePolicy {
        root_uid: 0,
        kilnr_uid: 51001,
        kilnr_gid: 51002,
        submit_gid: 51005,
        state_traverse_group_gids: vec![51006],
        lock_traverse_user_uids: vec![51003],
    };
    assert_eq!(policy.state_traverse_group_gids, [51006]);
    assert_eq!(policy.lock_traverse_user_uids, [51003]);
}

#[test]
fn test_linux_legacy_masked_acl_writers_are_removed_without_target_mutation() {
    if !acl_tools() {
        return;
    }
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("kilnr");
    fs::create_dir_all(state.join("locks/projects")).unwrap();
    for path in [&state, &state.join("locks"), &state.join("locks/projects")] {
        assert!(std::process::Command::new("setfacl")
            .args(["-m", "u:12345:rwx,m::r-x"])
            .arg(path)
            .status()
            .unwrap()
            .success());
    }
    let outside = base.path().join("outside.lock");
    fs::write(&outside, "outside target must not change").unwrap();
    std::os::unix::fs::symlink(&outside, state.join("locks/projects/demo.lock")).unwrap();
    let before = fs::read(&outside).unwrap();
    project_lock::provision_namespace(&state, &namespace_policy()).unwrap();
    assert_eq!(fs::read(outside).unwrap(), before);
    assert!(project_lock::provision(&state.join("locks/projects"), &["demo".into()]).is_err());
}

#[test]
fn test_linux_nonowner_submitter_traverses_execute_only_lock_ancestors() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("kilnr");
    project_lock::provision_namespace(&state, &namespace_policy()).unwrap();
    let locks = state.join("locks");
    let projects = locks.join("projects");
    project_lock::provision(&projects, &["demo".into()]).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o100)).unwrap();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o100)).unwrap();
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(ProjectLocks::acquire(&projects, &["demo".into()], Mode::Shared, false).is_ok());
}

#[test]
fn test_lock_namespace_provisioning_supports_clean_install_and_legacy_update() {
    for legacy in [false, true] {
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("kilnr");
        if legacy {
            fs::create_dir_all(state.join("locks")).unwrap();
            fs::set_permissions(&state, fs::Permissions::from_mode(0o710)).unwrap();
            fs::set_permissions(state.join("locks"), fs::Permissions::from_mode(0o750)).unwrap();
        }
        project_lock::provision_namespace(&state, &namespace_policy()).unwrap();
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o7777,
            0o710
        );
        assert_eq!(
            fs::metadata(state.join("locks"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o750
        );
        assert_eq!(
            fs::metadata(state.join("locks/projects"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            if cfg!(target_os = "macos") {
                0o750
            } else {
                0o2750
            }
        );
        project_lock::provision(&state.join("locks/projects"), &["demo".into()]).unwrap();
    }
}

#[test]
fn test_lock_namespace_provisioning_rejects_symlink_ancestors_without_mutation() {
    for scenario in ["state", "locks", "projects", "dangling-projects"] {
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("kilnr");
        let outside = base.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "keep").unwrap();
        match scenario {
            "state" => std::os::unix::fs::symlink(&outside, &state).unwrap(),
            "locks" => {
                fs::create_dir(&state).unwrap();
                std::os::unix::fs::symlink(&outside, state.join("locks")).unwrap();
            }
            other => {
                fs::create_dir_all(state.join("locks")).unwrap();
                let target = if other == "projects" {
                    outside.clone()
                } else {
                    base.path().join("missing")
                };
                std::os::unix::fs::symlink(target, state.join("locks/projects")).unwrap();
            }
        }
        assert!(
            project_lock::provision_namespace(&state, &namespace_policy()).is_err(),
            "{scenario}"
        );
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "keep"
        );
    }
}

#[test]
fn test_controller_home_preserves_the_hardened_state_root_contract() {
    let service = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/systemd/kilnr-controller.service"
    ))
    .unwrap();
    assert!(service.contains("Environment=HOME=/var/lib/kilnr/controller-home"));
}

#[test]
fn test_install_provisions_controller_lock_before_controller_opens_it() {
    let install = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")).unwrap();
    let controller =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops_runtime.rs")).unwrap();
    assert!(install.contains("install -o root -g kilnr -m 0660 /dev/null \"$controller_lock\""));
    let open = controller
        .find("open(\"/var/lib/kilnr/locks/controller.lock\")")
        .unwrap();
    let nearby = &controller[open.saturating_sub(250)..open + 60];
    assert!(!nearby.contains(".create(true)"));
}

#[test]
fn test_install_never_recalculates_stable_lock_ancestor_acl_masks_by_path() {
    let install = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")).unwrap();
    assert!(!install
        .lines()
        .any(|line| line.trim_start().starts_with("setfacl ")
            && (line.trim_end().ends_with(" /var/lib/kilnr")
                || line.trim_end().ends_with(" /var/lib/kilnr/locks"))));
}
