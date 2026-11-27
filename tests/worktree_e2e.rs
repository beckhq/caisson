use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

fn caisson_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_caisson"))
}

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "tag.forceSignAnnotated=false",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Caisson Test")
        .env("GIT_AUTHOR_EMAIL", "test@caisson.invalid")
        .env("GIT_COMMITTER_NAME", "Caisson Test")
        .env("GIT_COMMITTER_EMAIL", "test@caisson.invalid")
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_sample_repo(repo_dir: &Path) {
    fs::create_dir_all(repo_dir).unwrap();
    run_git(repo_dir, &["init", "-b", "main"]);
    fs::write(repo_dir.join("README.md"), "# sample repo\n").unwrap();
    fs::write(repo_dir.join(".gitignore"), "node_modules/\n.hook-ran\n").unwrap();
    run_git(repo_dir, &["add", "."]);
    run_git(repo_dir, &["commit", "-m", "initial commit"]);
}

fn run_caisson(cwd: &Path, root: &Path, cfg: &Path, args: &[&str]) -> Output {
    Command::new(caisson_bin())
        .current_dir(cwd)
        .env("CAISSON_ROOT", root)
        .env("CAISSON_CONFIG", cfg)
        .args(args)
        .output()
        .expect("caisson binary should execute")
}

#[test]
fn worktree_full_lifecycle_cow_post_create_rm_and_gc() {
    let sandbox = tempdir().unwrap();
    let repo_dir = sandbox.path().join("demo-app");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&caisson_root).unwrap();

    init_sample_repo(&repo_dir);

    // Seed `.caisson.toml` with `cow_copy` and `post_create`, and put a `node_modules` tree in main repo.
    fs::write(
        repo_dir.join(".caisson.toml"),
        r#"
[worktree]
cow_copy = ["node_modules"]
post_create = "echo $CAISSON_BRANCH > .hook-ran"
"#,
    )
    .unwrap();
    run_git(&repo_dir, &["add", ".caisson.toml"]);
    run_git(&repo_dir, &["commit", "-m", "add caisson config"]);

    let nm = repo_dir.join("node_modules/leftpad");
    fs::create_dir_all(&nm).unwrap();
    fs::write(nm.join("index.js"), "module.exports = 'v1';\n").unwrap();

    // Relax min_host_free so test never depends on host disk free space.
    let set_free = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["config", "set", "quotas.min_host_free", "0G"],
    );
    assert!(
        set_free.status.success(),
        "{}",
        String::from_utf8_lossy(&set_free.stderr)
    );

    // 1. `wt add` with CoW cloning + post_create hook + JSON output
    let add_out = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat/agent-one", "--json"],
    );
    assert!(
        add_out.status.success(),
        "wt add failed: {}",
        String::from_utf8_lossy(&add_out.stderr)
    );
    let add_json: Value = serde_json::from_slice(&add_out.stdout).unwrap();
    assert_eq!(add_json["slug"], "feat-agent-one");
    assert_eq!(add_json["post_create_ran"], true);
    assert_eq!(add_json["cow_clones"][0]["status"], "cloned");

    let wt_path = PathBuf::from(add_json["path"].as_str().unwrap());
    assert!(wt_path.exists());
    assert_eq!(
        fs::read_to_string(wt_path.join(".hook-ran"))
            .unwrap()
            .trim(),
        "feat/agent-one"
    );
    let cloned_js = wt_path.join("node_modules/leftpad/index.js");
    assert_eq!(
        fs::read_to_string(&cloned_js).unwrap(),
        "module.exports = 'v1';\n"
    );
    // Verify Copy-on-Write independence: editing cloned node_modules does not affect main repo.
    fs::write(&cloned_js, "module.exports = 'mutated';\n").unwrap();
    assert_eq!(
        fs::read_to_string(nm.join("index.js")).unwrap(),
        "module.exports = 'v1';\n"
    );

    // 2. `wt add --no-cow` skips cow_copy
    let add2 = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat/no-cow", "--no-cow", "--json"],
    );
    assert!(add2.status.success());
    let add2_json: Value = serde_json::from_slice(&add2.stdout).unwrap();
    let wt2_path = PathBuf::from(add2_json["path"].as_str().unwrap());
    assert!(!wt2_path.join("node_modules").exists());

    // 3. `wt ls --json` shows both worktrees
    let ls_out = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "ls", "--json"],
    );
    assert!(ls_out.status.success());
    let ls_json: Value = serde_json::from_slice(&ls_out.stdout).unwrap();
    assert_eq!(ls_json.as_array().unwrap().len(), 2);

    // 4. Make wt1 dirty -> `wt rm` without `--force` MUST refuse
    fs::write(wt_path.join("uncommitted.txt"), "wip\n").unwrap();
    let rm_refused = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "rm", "feat-agent-one"],
    );
    assert!(!rm_refused.status.success());
    assert!(
        String::from_utf8_lossy(&rm_refused.stderr).contains("dirty or has unpushed"),
        "stderr: {}",
        String::from_utf8_lossy(&rm_refused.stderr)
    );
    assert!(wt_path.exists());

    // 5. `wt rm --force --sync` removes cleanly and prunes git worktree metadata
    let rm_ok = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "rm", "feat-agent-one", "--force", "--sync", "--json"],
    );
    assert!(
        rm_ok.status.success(),
        "wt rm failed: {}",
        String::from_utf8_lossy(&rm_ok.stderr)
    );
    assert!(!wt_path.exists());

    // Remove the second clean worktree asynchronously, then verify `caisson gc` drains trash & prunes.
    let rm2_ok = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "rm", "feat-no-cow"],
    );
    assert!(rm2_ok.status.success());
    assert!(!wt2_path.exists());

    // Drop a synthetic stale entry in `.trash/` and verify `caisson gc` cleans it + empty repo dir.
    let stale_trash = caisson_root.join(".trash/stale-item");
    fs::create_dir_all(&stale_trash).unwrap();
    fs::write(stale_trash.join("data.bin"), vec![1u8; 4096]).unwrap();

    let gc_out = run_caisson(&repo_dir, &caisson_root, &config_path, &["gc", "--json"]);
    assert!(gc_out.status.success());
    let gc_json: Value = serde_json::from_slice(&gc_out.stdout).unwrap();
    assert!(gc_json["trash"]["entries_removed"].as_u64().unwrap() >= 1);
    assert!(!stale_trash.exists());
}

#[test]
fn soft_quotas_block_excess_worktrees_and_prune_respects_dirty_and_unpushed() {
    let sandbox = tempdir().unwrap();
    let repo_dir = sandbox.path().join("quota-repo");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&caisson_root).unwrap();

    init_sample_repo(&repo_dir);

    // Configure tight per-repo quota = 2, min_host_free = 0G.
    for (k, v) in [
        ("quotas.max_worktrees_per_repo", "2"),
        ("quotas.min_host_free", "0G"),
    ] {
        let out = run_caisson(
            &repo_dir,
            &caisson_root,
            &config_path,
            &["config", "set", k, v],
        );
        assert!(out.status.success());
    }

    // Create 2 worktrees (clean + unpushed).
    let wt1_out = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "branch-clean", "--json"],
    );
    assert!(wt1_out.status.success());
    let wt1_json: Value = serde_json::from_slice(&wt1_out.stdout).unwrap();
    let wt1_path = PathBuf::from(wt1_json["path"].as_str().unwrap());

    let wt2_out = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "branch-unpushed", "--json"],
    );
    assert!(wt2_out.status.success());
    let wt2_json: Value = serde_json::from_slice(&wt2_out.stdout).unwrap();
    let wt2_path = PathBuf::from(wt2_json["path"].as_str().unwrap());

    // Make a local commit on `branch-unpushed` so it is unpushed relative to `main`.
    fs::write(wt2_path.join("feature.txt"), "new work\n").unwrap();
    run_git(&wt2_path, &["add", "feature.txt"]);
    run_git(&wt2_path, &["commit", "-m", "local unpushed commit"]);

    // 3rd worktree MUST be rejected by `max_worktrees_per_repo` with structured JSON payload.
    let wt3_blocked = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "branch-third", "--json"],
    );
    assert!(!wt3_blocked.status.success());
    let err_json: Value = serde_json::from_slice(&wt3_blocked.stdout).unwrap();
    assert_eq!(
        err_json["quota_violation"]["rule"],
        "max_worktrees_per_repo"
    );
    assert!(err_json["quota_violation"]["suggested_command"]
        .as_str()
        .unwrap()
        .contains("caisson wt prune"));

    // `--force` bypasses the soft quota with a warning.
    let wt3_forced = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "branch-third", "--force", "--json"],
    );
    assert!(wt3_forced.status.success());
    let wt3_json: Value = serde_json::from_slice(&wt3_forced.stdout).unwrap();
    assert!(!wt3_json["warnings"].as_array().unwrap().is_empty());

    // Make `branch-third` dirty.
    let wt3_path = PathBuf::from(wt3_json["path"].as_str().unwrap());
    fs::write(wt3_path.join("dirty.txt"), "dirty").unwrap();

    // `wt prune --inactive 0s` (dry-run by default): should select `branch-clean` only,
    // protecting `branch-unpushed` and `branch-third` (dirty), and delete NOTHING yet!
    let prune_dry = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "prune", "--inactive", "0s", "--json"],
    );
    assert!(prune_dry.status.success());
    let dry_json: Value = serde_json::from_slice(&prune_dry.stdout).unwrap();
    assert_eq!(dry_json["dry_run"], true);
    assert_eq!(dry_json["pruned"].as_array().unwrap().len(), 1);
    assert_eq!(dry_json["pruned"][0]["slug"], "branch-clean");
    assert_eq!(dry_json["skipped_protected"].as_array().unwrap().len(), 2);
    assert!(wt1_path.exists());

    // `wt prune --inactive 0s --apply --sync`: removes `branch-clean`, keeps dirty + unpushed!
    let prune_apply = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &[
            "wt",
            "prune",
            "--inactive",
            "0s",
            "--apply",
            "--sync",
            "--json",
        ],
    );
    assert!(prune_apply.status.success());
    assert!(!wt1_path.exists());
    assert!(wt2_path.exists());
    assert!(wt3_path.exists());

    // `wt prune --inactive 0s --include-dirty --include-unpushed --apply --sync` removes remaining two.
    let prune_all = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &[
            "wt",
            "prune",
            "--inactive",
            "0s",
            "--include-dirty",
            "--include-unpushed",
            "--apply",
            "--sync",
            "--json",
        ],
    );
    assert!(prune_all.status.success());
    assert!(!wt2_path.exists());
    assert!(!wt3_path.exists());
}

#[test]
fn cli_clone_subcommand_clones_file_on_apfs() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("a.txt");
    let dst = dir.path().join("b.txt");
    fs::write(&src, "apfs-clone-cli").unwrap();

    let out = Command::new(caisson_bin())
        .args([
            "clone",
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(fs::read_to_string(&dst).unwrap(), "apfs-clone-cli");
}

#[test]
fn worktree_from_ref_custom_slug_warm_cache_and_multi_repo_ls_all() {
    let sandbox = tempdir().unwrap();
    let repo_a = sandbox.path().join("repo-alpha");
    let repo_b = sandbox.path().join("repo-beta");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&caisson_root).unwrap();

    init_sample_repo(&repo_a);
    run_git(&repo_a, &["tag", "v1"]);
    fs::write(repo_a.join("README.md"), "# v2 content\n").unwrap();
    fs::write(
        repo_a.join(".caisson.toml"),
        "[worktree]\ncow_copy = [\"vendor\"]\n",
    )
    .unwrap();
    run_git(&repo_a, &["add", "."]);
    run_git(&repo_a, &["commit", "-m", "v2"]);

    // Seed warm cache for repo-alpha under `<root>/cache/repo-alpha/vendor`
    let warm_vendor = caisson_root.join("cache/repo-alpha/vendor");
    fs::create_dir_all(&warm_vendor).unwrap();
    fs::write(warm_vendor.join("lib.txt"), "warm-cached-dep").unwrap();

    init_sample_repo(&repo_b);

    let set_free = run_caisson(
        &repo_a,
        &caisson_root,
        &config_path,
        &["config", "set", "quotas.min_host_free", "0G"],
    );
    assert!(set_free.status.success());

    // Branch from tag `v1` with custom `--name` slug
    let add_a = run_caisson(
        &repo_a,
        &caisson_root,
        &config_path,
        &[
            "wt",
            "add",
            "hotfix/v1-patch",
            "--from",
            "v1",
            "--name",
            "custom-hotfix",
            "--json",
        ],
    );
    assert!(
        add_a.status.success(),
        "{}",
        String::from_utf8_lossy(&add_a.stderr)
    );
    let add_a_json: Value = serde_json::from_slice(&add_a.stdout).unwrap();
    assert_eq!(add_a_json["slug"], "custom-hotfix");
    let wt_a_path = PathBuf::from(add_a_json["path"].as_str().unwrap());
    // Checked out at v1, not v2!
    assert_eq!(
        fs::read_to_string(wt_a_path.join("README.md")).unwrap(),
        "# sample repo\n"
    );
    // Cloned from warm cache!
    assert_eq!(
        fs::read_to_string(wt_a_path.join("vendor/lib.txt")).unwrap(),
        "warm-cached-dep"
    );

    // Add a worktree for repo_b too
    let add_b = run_caisson(
        &repo_b,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat/beta-work", "--json"],
    );
    assert!(add_b.status.success());

    // `wt ls` inside repo_a returns 1; `wt ls --all` returns 2 across both repos!
    let ls_local = run_caisson(
        &repo_a,
        &caisson_root,
        &config_path,
        &["wt", "ls", "--json"],
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&ls_local.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let ls_all = run_caisson(
        &repo_a,
        &caisson_root,
        &config_path,
        &["wt", "ls", "--all", "--json"],
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&ls_all.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
