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

#[test]
fn wt_path_completions_and_shell_init_work() {
    let sandbox = tempdir().unwrap();
    let repo_dir = sandbox.path().join("shell-repo");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&caisson_root).unwrap();

    init_sample_repo(&repo_dir);
    assert!(run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["config", "set", "quotas.min_host_free", "0G"],
    )
    .status
    .success());

    let add = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat/shell-nav"],
    );
    assert!(add.status.success());
    let expected_path = String::from_utf8_lossy(&add.stdout).trim().to_string();

    let path_out = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "path", "feat-shell-nav"],
    );
    assert!(path_out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&path_out.stdout).trim(),
        expected_path
    );

    let comp = Command::new(caisson_bin())
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert!(comp.status.success());
    assert!(String::from_utf8_lossy(&comp.stdout).contains("#compdef caisson"));

    let init = Command::new(caisson_bin())
        .args(["shell-init", "zsh", "--alias", "cai"])
        .output()
        .unwrap();
    assert!(init.status.success());
    let init_str = String::from_utf8_lossy(&init.stdout);
    assert!(init_str.contains("caisson wt path"));
    assert!(init_str.contains("alias cai=caisson"));
}

#[test]
fn review_regressions_prune_scope_git_failure_and_symlink_containment() {
    use std::os::unix::fs::symlink;

    let sandbox = tempdir().unwrap();
    let repo_dir = sandbox.path().join("safety-repo");
    let non_git_dir = sandbox.path().join("not-a-git-repo");
    let outside_dir = sandbox.path().join("outside-victim");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&non_git_dir).unwrap();
    fs::create_dir_all(outside_dir.join("child")).unwrap();
    fs::write(outside_dir.join("child/keep.txt"), "do-not-delete").unwrap();
    fs::create_dir_all(&caisson_root).unwrap();

    init_sample_repo(&repo_dir);
    assert!(run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["config", "set", "quotas.min_host_free", "0G"],
    )
    .status
    .success());

    // Add a clean worktree.
    let add = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat/keep-safe", "--json"],
    );
    assert!(add.status.success());
    let add_json: Value = serde_json::from_slice(&add.stdout).unwrap();
    let wt_path = PathBuf::from(add_json["path"].as_str().unwrap());

    // 1. Running `wt prune --inactive 0s --apply` outside a git repo MUST fail and NOT touch worktrees!
    let prune_outside = run_caisson(
        &non_git_dir,
        &caisson_root,
        &config_path,
        &["wt", "prune", "--inactive", "0s", "--apply", "--sync"],
    );
    assert!(!prune_outside.status.success());
    assert!(wt_path.exists());

    // 2. Plant a symlinked repo dir inside `<caisson_root>/worktrees/` pointing outside ->
    //    `wt prune --all --inactive 0s --apply` MUST NOT touch `outside-victim/child`!
    symlink(&outside_dir, caisson_root.join("worktrees/evil-link")).unwrap();

    // 3. Corrupt the worktree's `.git` pointer so git inspection fails ->
    //    BOTH `wt rm` (without `--force`) AND `wt prune --apply` MUST protect it!
    fs::write(wt_path.join(".git"), "gitdir: /nonexistent/broken/gitdir\n").unwrap();

    let rm_broken = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "rm", "feat-keep-safe"],
    );
    assert!(
        !rm_broken.status.success(),
        "wt rm must refuse when git inspection fails"
    );
    assert!(wt_path.exists());

    let prune_broken = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &[
            "wt",
            "prune",
            "--all",
            "--inactive",
            "0s",
            "--apply",
            "--sync",
            "--json",
        ],
    );
    assert!(prune_broken.status.success());
    let prune_json: Value = serde_json::from_slice(&prune_broken.stdout).unwrap();
    assert_eq!(prune_json["pruned"].as_array().unwrap().len(), 0);
    assert_eq!(
        prune_json["skipped_protected"][0]["reason"],
        "unknown-git-state"
    );
    assert!(wt_path.exists());
    // Outside symlink victim MUST remain untouched!
    assert_eq!(
        fs::read_to_string(outside_dir.join("child/keep.txt")).unwrap(),
        "do-not-delete"
    );

    // 4. `cow_copy` intermediate symlink escape MUST be blocked!
    let escape_target = sandbox.path().join("escape-target");
    fs::create_dir_all(&escape_target).unwrap();
    let repo2 = sandbox.path().join("cow-escape-repo");
    init_sample_repo(&repo2);
    symlink(&escape_target, repo2.join("vendor")).unwrap();
    fs::write(
        repo2.join(".caisson.toml"),
        "[worktree]\ncow_copy = [\"vendor/deps\"]\n",
    )
    .unwrap();
    run_git(&repo2, &["add", "vendor", ".caisson.toml"]);
    run_git(&repo2, &["commit", "-m", "add symlink vendor"]);
    let warm = caisson_root.join("cache/cow-escape-repo/vendor/deps");
    fs::create_dir_all(&warm).unwrap();
    fs::write(warm.join("x.txt"), "payload").unwrap();

    let add_escape = run_caisson(
        &repo2,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat/escape-test", "--json"],
    );
    assert!(add_escape.status.success());
    let esc_json: Value = serde_json::from_slice(&add_escape.stdout).unwrap();
    assert_eq!(esc_json["cow_clones"][0]["status"], "skipped");
    assert!(!escape_target.join("deps").exists());
}

#[test]
fn cli_agent_status_install_dismiss_uninstall_and_snippet() {
    let sandbox = tempdir().unwrap();
    let fake_home = sandbox.path().join("home");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(fake_home.join(".claude")).unwrap();
    fs::create_dir_all(fake_home.join(".codex")).unwrap();

    // 1. `agent status --json` detects claude and codex
    let st = Command::new(caisson_bin())
        .env("CAISSON_HOME", &fake_home)
        .env("CAISSON_CONFIG", &config_path)
        .args(["agent", "status", "--json"])
        .output()
        .unwrap();
    assert!(st.status.success());
    let st_json: Value = serde_json::from_slice(&st.stdout).unwrap();
    let claude = st_json
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "claude")
        .unwrap();
    assert_eq!(claude["detected"], true);
    assert_eq!(claude["installed"], false);

    // 2. `agent install claude --json` installs SKILL.md
    let inst = Command::new(caisson_bin())
        .env("CAISSON_HOME", &fake_home)
        .env("CAISSON_CONFIG", &config_path)
        .args(["agent", "install", "claude", "--json"])
        .output()
        .unwrap();
    assert!(inst.status.success());
    assert!(fake_home.join(".claude/skills/caisson/SKILL.md").is_file());

    // 3. `agent dismiss codex --json` persists dismissal
    let dis = Command::new(caisson_bin())
        .env("CAISSON_HOME", &fake_home)
        .env("CAISSON_CONFIG", &config_path)
        .args(["agent", "dismiss", "codex", "--json"])
        .output()
        .unwrap();
    assert!(dis.status.success());

    // 4. `agent uninstall claude --json` removes SKILL.md
    let uninst = Command::new(caisson_bin())
        .env("CAISSON_HOME", &fake_home)
        .env("CAISSON_CONFIG", &config_path)
        .args(["agent", "uninstall", "claude", "--json"])
        .output()
        .unwrap();
    assert!(uninst.status.success());
    assert!(!fake_home.join(".claude/skills/caisson/SKILL.md").exists());

    // 5. `agent snippet` prints drop-in AGENTS.md instructions
    let snip = Command::new(caisson_bin())
        .args(["agent", "snippet"])
        .output()
        .unwrap();
    assert!(snip.status.success());
    assert!(String::from_utf8_lossy(&snip.stdout).contains("caisson wt add"));
}

#[test]
fn wt_hydrate_clones_from_sibling_worktree_for_external_worktree_managers() {
    let sandbox = tempdir().unwrap();
    let repo_dir = sandbox.path().join("t3-demo-app");
    let external_wt_root = sandbox.path().join("t3-worktrees");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&external_wt_root).unwrap();
    fs::create_dir_all(&caisson_root).unwrap();

    init_sample_repo(&repo_dir);

    fs::write(
        repo_dir.join(".caisson.toml"),
        r#"
[worktree]
cow_copy = ["node_modules", ".venv"]
post_create = "echo hydrated:$CAISSON_BRANCH > .hook-ran"
"#,
    )
    .unwrap();
    run_git(&repo_dir, &["add", ".caisson.toml"]);
    run_git(&repo_dir, &["commit", "-m", "add caisson config"]);

    // Simulate T3 Code creating two raw git worktrees outside Caisson's own `worktrees/` dir.
    let wt_donor = external_wt_root.join("thread-1");
    let wt_new = external_wt_root.join("thread-2");
    run_git(
        &repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "t3/thread-1",
            wt_donor.to_str().unwrap(),
        ],
    );
    run_git(
        &repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "t3/thread-2",
            wt_new.to_str().unwrap(),
        ],
    );

    // Populate `node_modules` in the sibling worktree (donor), and `.venv` in main repo.
    fs::create_dir_all(wt_donor.join("node_modules/pkg")).unwrap();
    fs::write(wt_donor.join("node_modules/pkg/index.js"), "v1-sibling\n").unwrap();
    fs::create_dir_all(repo_dir.join(".venv/bin")).unwrap();
    fs::write(repo_dir.join(".venv/bin/python"), "py-main\n").unwrap();

    // 1. `--dry-run` plans both clones without writing anything or running `post_create`.
    let dry = run_caisson(
        &wt_new,
        &caisson_root,
        &config_path,
        &["wt", "hydrate", "--dry-run", "--json"],
    );
    assert!(
        dry.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_json: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_json["dry_run"], true);
    assert_eq!(dry_json["post_create_ran"], false);
    assert_eq!(dry_json["cow_clones"][0]["status"], "planned");
    assert_eq!(dry_json["cow_clones"][1]["status"], "planned");
    assert!(!wt_new.join("node_modules").exists());
    assert!(!wt_new.join(".hook-ran").exists());

    // 2. Default `wt hydrate` is clone-only (does NOT execute `post_create` unless `--run-post-create`).
    let hyd1 = run_caisson(
        &wt_new,
        &caisson_root,
        &config_path,
        &["wt", "hydrate", "--json"],
    );
    assert!(
        hyd1.status.success(),
        "hydrate failed: {}",
        String::from_utf8_lossy(&hyd1.stderr)
    );
    let hyd1_json: Value = serde_json::from_slice(&hyd1.stdout).unwrap();
    assert_eq!(hyd1_json["cow_clones"][0]["status"], "cloned");
    assert_eq!(hyd1_json["cow_clones"][1]["status"], "cloned");
    assert_eq!(hyd1_json["post_create_ran"], false);
    assert!(!wt_new.join(".hook-ran").exists());

    // Verify APFS CoW independence: mutating cloned file does not alter donor.
    fs::write(wt_new.join("node_modules/pkg/index.js"), "mutated\n").unwrap();
    assert_eq!(
        fs::read_to_string(wt_donor.join("node_modules/pkg/index.js")).unwrap(),
        "v1-sibling\n"
    );

    // 3. Re-running `wt hydrate --run-post-create` is idempotent on clones and runs main repo's hook.
    let hyd2 = run_caisson(
        &wt_new,
        &caisson_root,
        &config_path,
        &["wt", "hydrate", "--run-post-create", "--json"],
    );
    assert!(hyd2.status.success());
    let hyd2_json: Value = serde_json::from_slice(&hyd2.stdout).unwrap();
    assert_eq!(hyd2_json["cow_clones"][0]["status"], "skipped");
    assert_eq!(hyd2_json["cow_clones"][1]["status"], "skipped");
    assert_eq!(hyd2_json["post_create_ran"], true);
    assert_eq!(
        fs::read_to_string(wt_new.join(".hook-ran")).unwrap().trim(),
        "hydrated:t3/thread-2"
    );
}

#[test]
fn wt_hydrate_zero_config_auto_detects_and_prefers_matching_lockfile_donor() {
    let sandbox = tempdir().unwrap();
    let repo_dir = sandbox.path().join("zero-cfg-node-app");
    let external_wt_root = sandbox.path().join("t3-worktrees");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&external_wt_root).unwrap();
    fs::create_dir_all(&caisson_root).unwrap();

    // No `.caisson.toml` at all: only `package.json` + `pnpm-lock.yaml`.
    init_sample_repo(&repo_dir);
    fs::write(repo_dir.join("package.json"), r#"{"name":"demo"}"#).unwrap();
    fs::write(
        repo_dir.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\nv1\n",
    )
    .unwrap();
    run_git(&repo_dir, &["add", "package.json", "pnpm-lock.yaml"]);
    run_git(&repo_dir, &["commit", "-m", "add package manifest"]);

    // Donor 1 has matching v1 lockfile; Donor 2 (newer) has a different v2 lockfile.
    let wt_match = external_wt_root.join("donor-v1");
    let wt_mismatch = external_wt_root.join("donor-v2");
    let wt_target = external_wt_root.join("target-v1");
    run_git(
        &repo_dir,
        &["worktree", "add", "-b", "b-v1", wt_match.to_str().unwrap()],
    );
    run_git(
        &repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "b-v2",
            wt_mismatch.to_str().unwrap(),
        ],
    );
    run_git(
        &repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "b-target",
            wt_target.to_str().unwrap(),
        ],
    );

    fs::create_dir_all(wt_match.join("node_modules/a")).unwrap();
    fs::write(wt_match.join("node_modules/a/ver.txt"), "from-matching-v1").unwrap();

    // Make donor-v2 newer AND with a different lockfile.
    fs::write(
        wt_mismatch.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\nv2-different\n",
    )
    .unwrap();
    fs::create_dir_all(wt_mismatch.join("node_modules/a")).unwrap();
    fs::write(
        wt_mismatch.join("node_modules/a/ver.txt"),
        "from-mismatched-v2",
    )
    .unwrap();

    // Hydrate target-v1 without `.caisson.toml`: must auto-detect `node_modules` AND pick donor-v1!
    let out = run_caisson(
        &wt_target,
        &caisson_root,
        &config_path,
        &["wt", "hydrate", "--json"],
    );
    assert!(
        out.status.success(),
        "zero-config hydrate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["auto_detected"], true);
    assert_eq!(json["cow_clones"][0]["entry"], "node_modules");
    assert_eq!(json["cow_clones"][0]["status"], "cloned");
    assert_eq!(json["cow_clones"][0]["lockfile_match"], true);
    assert_eq!(
        fs::read_to_string(wt_target.join("node_modules/a/ver.txt")).unwrap(),
        "from-matching-v1"
    );

    // Now test a worktree whose lockfile differs from ALL donors: still clones, reports lockfile_match=false.
    let wt_v3 = external_wt_root.join("target-v3");
    run_git(
        &repo_dir,
        &["worktree", "add", "-b", "b-v3", wt_v3.to_str().unwrap()],
    );
    fs::write(
        wt_v3.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\nv3-brand-new\n",
    )
    .unwrap();
    let out3 = run_caisson(
        &wt_v3,
        &caisson_root,
        &config_path,
        &["wt", "hydrate", "--json"],
    );
    assert!(out3.status.success());
    let json3: Value = serde_json::from_slice(&out3.stdout).unwrap();
    assert_eq!(json3["cow_clones"][0]["status"], "cloned");
    assert_eq!(json3["cow_clones"][0]["lockfile_match"], false);
}

#[test]
fn wt_hydrate_never_runs_untrusted_branch_post_create_and_rm_respects_tier_precedence() {
    let sandbox = tempdir().unwrap();
    let repo_dir = sandbox.path().join("sec-repo");
    let caisson_root = sandbox.path().join("mock-caisson-vol");
    let config_path = sandbox.path().join("config.toml");
    fs::create_dir_all(&caisson_root).unwrap();
    init_sample_repo(&repo_dir);

    // 1. Untrusted branch modifies `.caisson.toml` with a malicious `post_create` hook.
    // Even with `--run-post-create`, `wt hydrate` MUST read `post_create` strictly from the
    // main repo checkout (which has no `post_create`), never from the target branch.
    let untrusted_wt = sandbox.path().join("untrusted-branch-wt");
    run_git(
        &repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "pr/untrusted",
            untrusted_wt.to_str().unwrap(),
        ],
    );
    fs::write(
        untrusted_wt.join(".caisson.toml"),
        "[worktree]\ncow_copy = [\"node_modules\"]\npost_create = \"echo pwned > .pwned\"\n",
    )
    .unwrap();
    let hyd = run_caisson(
        &untrusted_wt,
        &caisson_root,
        &config_path,
        &["wt", "hydrate", "--run-post-create", "--json"],
    );
    assert!(
        hyd.status.success(),
        "hydrate failed: {}",
        String::from_utf8_lossy(&hyd.stderr)
    );
    let hyd_json: Value = serde_json::from_slice(&hyd.stdout).unwrap();
    assert_eq!(hyd_json["post_create_ran"], false);
    assert!(!untrusted_wt.join(".pwned").exists());

    // 2. Deterministic tier precedence for `wt path` / `wt rm`:
    // Worktree A has branch `feat/alpha` and slug `feat-alpha`.
    // Worktree B has branch `feat-alpha` and custom slug `custom-b`.
    // Target `feat-alpha` must deterministically select exact slug (Tier 2) over exact branch (Tier 3),
    // and `wt path --json` must populate real git/disk metadata (not fabricated zeros/falses).
    let add_a = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat/alpha", "--json"],
    );
    assert!(add_a.status.success());
    let add_b = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "add", "feat-alpha", "--name", "custom-b", "--json"],
    );
    assert!(
        add_b.status.success(),
        "add_b failed: {}",
        String::from_utf8_lossy(&add_b.stderr)
    );

    // Modify a tracked file inside `feat/alpha` so `dirty` is true on disk.
    let a_json: Value = serde_json::from_slice(&add_a.stdout).unwrap();
    let a_path = PathBuf::from(a_json["path"].as_str().unwrap());
    fs::write(a_path.join("README.md"), "dirty-change\n").unwrap();

    // `wt path feat-alpha --json` must deterministically pick slug `feat-alpha` (branch `feat/alpha`)
    // and report live `dirty: true` + nonzero `disk_bytes`.
    let path_out = run_caisson(
        &repo_dir,
        &caisson_root,
        &config_path,
        &["wt", "path", "feat-alpha", "--json"],
    );
    assert!(path_out.status.success());
    let path_json: Value = serde_json::from_slice(&path_out.stdout).unwrap();
    assert_eq!(path_json["slug"], "feat-alpha");
    assert_eq!(path_json["branch"], "feat/alpha");
    assert_eq!(path_json["dirty"], true);
    assert!(path_json["disk_bytes"].as_u64().unwrap() > 0);
}
