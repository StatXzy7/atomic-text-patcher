//! 集成测试：正常批量修改、预览零副作用、过期计划、歧义/重叠拒绝、
//! 多阶段注入中断后的恢复结果，以及目录内容摘要。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use atomic_text_patcher::{
    build_plan_from_json, commit, parse_ops, recover, CrashAction, CrashHook, ErrorKind,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn ensure_global_state_home() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let dir = std::env::temp_dir().join(format!(
            "atp-it-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Global, OUTSIDE every project root; keyed per root by hash.
        std::env::set_var("ATP_STATE_HOME", &dir);
    });
}

fn temp_root(tag: &str) -> PathBuf {
    ensure_global_state_home();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let base = std::env::temp_dir().join(format!(
        "atp-it-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    base
}

/// sha-like but dependency free: fnv digest summary of the whole tree.
fn tree_summary(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().unwrap() == ".atp-state" {
                continue;
            }
            if path.is_dir() {
                walk(&path, base, out);
            } else if path.is_file() {
                let bytes = fs::read(&path).unwrap();
                let rel = path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                out.insert(rel, atomic_text_patcher::hash::content_fingerprint(&bytes));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

struct OnceCrash {
    phase: String,
    count: std::cell::Cell<u32>,
}
impl CrashHook for OnceCrash {
    fn at(&self, phase: &str) -> CrashAction {
        if phase == self.phase && self.count.get() == 0 {
            self.count.set(1);
            CrashAction::Simulated
        } else {
            CrashAction::Continue
        }
    }
}

const PLAN: &str = r#"{
  "ops": [
    {"type": "replace", "path": "src/main.txt", "find": "版本：v1", "replace": "版本：v2"},
    {"type": "replace", "path": "src/main.txt", "find": "中文", "replace": "漢字"},
    {"type": "create", "path": "docs/新建.txt", "content": "第一行\r\n第二行 CRLF\r\n"},
    {"type": "delete", "path": "obsolete.txt"}
  ]
}"#;

fn seed(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.txt"), "标题：中文\n版本：v1\n").unwrap();
    fs::write(root.join("obsolete.txt"), "待删除\n").unwrap();
    fs::write(root.join("untouched.dat"), vec![0u8, 1, 2, 255, b'\n']).unwrap();
}

fn assert_final_state(root: &Path) {
    assert_eq!(
        fs::read(root.join("src/main.txt")).unwrap(),
        "标题：漢字\n版本：v2\n".as_bytes()
    );
    assert_eq!(
        fs::read(root.join("docs/新建.txt")).unwrap(),
        "第一行\r\n第二行 CRLF\r\n".as_bytes()
    );
    assert!(!root.join("obsolete.txt").exists());
    // Unchanged file keeps identical bytes.
    assert_eq!(fs::read(root.join("untouched.dat")).unwrap(), vec![0u8, 1, 2, 255, b'\n']);
    assert!(!root.join(".atp-state").exists() || dir_is_empty_after_recovery(root));
}

fn dir_is_empty_after_recovery(root: &Path) -> bool {
    // journals must be gone once the transaction finishes.
    let journals = root.join(".atp-state/journals");
    !journals.exists() || fs::read_dir(journals).unwrap().next().is_none()
}

#[test]
fn batch_modifies_creates_deletes_and_preserves_bytes() {
    let root = temp_root("batch");
    seed(&root);
    let plan = build_plan_from_json(&root, PLAN).unwrap();
    let outcome = commit(&plan, false, &OnceCrash { phase: "never".into(), count: 0.into() }).unwrap();
    assert_eq!(outcome.applied, 3);
    assert_eq!(outcome.created, 1);
    assert_eq!(outcome.deleted, 1);
    assert_final_state(&root);
}

#[test]
fn preview_has_no_side_effects_in_target_directory() {
    let root = temp_root("preview");
    seed(&root);
    let before = tree_summary(&root);
    let plan = build_plan_from_json(&root, PLAN).unwrap();
    let text = plan.render_preview();
    assert!(text.contains("补丁预览"));
    // Snapshot + saving receipt is part of the CLI preview flow too.
    let receipt = atomic_text_patcher::engine::Receipt::snapshot(&plan).unwrap();
    receipt.save().unwrap();
    let after = tree_summary(&root);
    assert_eq!(before, after, "预览不得改动目标目录");
    // State really lives outside the project.
    assert!(!root.join(".atp-state").exists());
    let state_home = std::env::var("ATP_STATE_HOME").unwrap();
    assert!(
        !PathBuf::from(&state_home).starts_with(&root),
        "外部状态目录不得位于项目根内"
    );
    assert!(PathBuf::from(&state_home).join("roots").read_dir().unwrap().next().is_some());
}

#[test]
fn stale_plan_conflict_when_file_changed_after_preview() {
    let root = temp_root("stale");
    seed(&root);
    let plan = build_plan_from_json(&root, PLAN).unwrap();
    let receipt = atomic_text_patcher::engine::Receipt::snapshot(&plan).unwrap();
    receipt.save().unwrap();

    // External edit after preview.
    let mut changed = fs::read(root.join("src/main.txt")).unwrap();
    changed.extend_from_slice("// 用户手工新增\n".as_bytes());
    fs::write(root.join("src/main.txt"), changed).unwrap();

    let err = commit(&plan, true, &OnceCrash { phase: "x".into(), count: 0.into() }).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    // Whole batch rejected: delete must not have happened.
    assert!(root.join("obsolete.txt").exists());
    assert!(!root.join("docs/新建.txt").exists());
}

#[test]
fn missing_and_multiple_snippet_rejected() {
    let root = temp_root("ambiguous");
    seed(&root);

    let missing = r#"{"ops":[{"type":"replace","path":"src/main.txt","find":"不存在的片段","replace":"x"}]}"#;
    let err = build_plan_from_json(&root, missing).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Ambiguous);

    let multiple = r#"{"ops":[{"type":"replace","path":"src/main.txt","find":"\n","replace":"X"}]}"#;
    let err = build_plan_from_json(&root, multiple).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Ambiguous);
}

#[test]
fn overlapping_replacements_rejected() {
    let root = temp_root("overlap");
    seed(&root);
    let json = r#"{"ops":[
      {"type":"replace","path":"src/main.txt","find":"版本：v1","replace":"A"},
      {"type":"replace","path":"src/main.txt","find":"：v1\n","replace":"B"}
    ]}"#;
    let err = build_plan_from_json(&root, json).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Ambiguous);
}

#[test]
fn path_traversal_and_symlink_escape_rejected() {
    let root = temp_root("escape");
    seed(&root);
    let outside = root.parent().unwrap().join("atp-outside-secret");
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "机密\n").unwrap();

    let traversal = r#"{"ops":[{"type":"delete","path":"../atp-outside-secret/secret.txt"}]}"#;
    let err = build_plan_from_json(&root, traversal).unwrap_err();
    assert_eq!(err.kind, ErrorKind::UnsafePath);

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let via_link = r#"{"ops":[{"type":"delete","path":"link/secret.txt"}]}"#;
        let err = build_plan_from_json(&root, via_link).unwrap_err();
        assert_eq!(err.kind, ErrorKind::UnsafePath);
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), "机密\n".as_bytes());
    }
}

#[test]
fn interrupted_at_each_switch_phase_then_recovered() {
    let root = temp_root("phases");
    seed(&root);
    let phases = ["after-stage", "after-journal", "after-switch-0", "after-switch-1", "after-switch-2"];
    for phase in phases {
        // Fresh project copy per phase.
        let case = root.join(phase.replace('-', "_"));
        fs::create_dir_all(&case).unwrap();
        seed(&case);

        let plan = build_plan_from_json(&case, PLAN).unwrap();
        let crash = OnceCrash { phase: phase.into(), count: 0.into() };
        let err = commit(&plan, false, &crash).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Interrupted, "phase {phase} should interrupt");

        // Per-file atomicity: every file is exactly old or exactly new, never torn.
        let main_old = "标题：中文\n版本：v1\n".as_bytes();
        let main_new = "标题：漢字\n版本：v2\n".as_bytes();
        let main_bytes = fs::read(case.join("src/main.txt")).unwrap();
        assert!(
            main_bytes == main_old || main_bytes == main_new,
            "phase {phase}: main.txt torn or corrupted"
        );
        let created = case.join("docs/新建.txt");
        let created_new = "第一行\r\n第二行 CRLF\r\n".as_bytes();
        assert!(
            !created.exists() || fs::read(&created).unwrap() == created_new,
            "phase {phase}: created file torn"
        );

        let report = recover(&case, &OnceCrash { phase: "never".into(), count: 0.into() }).unwrap();
        if phase == "after-stage" {
            // Crash was before the journal existed: nothing was switched and
            // recovery clears the orphan staging directory; directory all-old.
            assert!(report.sessions.is_empty(), "after-stage leaves no journal");
            assert_eq!(
                fs::read_to_string(case.join("src/main.txt")).unwrap(),
                "标题：中文
版本：v1
"
            );
            continue;
        }
        assert!(report.sessions.len() >= 1, "phase {phase} should recover a session");
        assert_final_state(&case);
        println!("阶段 {phase} 恢复后目录摘要：{:?}", tree_summary(&case));
    }
}

#[test]
fn recovery_conflict_preserves_scene_when_user_edits() {
    let root = temp_root("recover-conflict");
    seed(&root);
    let plan = build_plan_from_json(&root, PLAN).unwrap();
    let err = commit(&plan, false, &OnceCrash { phase: "after-switch-0".into(), count: 0.into() }).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Interrupted);

    // User edits a file that was already switched to new content.
    fs::write(root.join("src/main.txt"), "用户在中断后乱改的内容\n").unwrap();

    let err = recover(&root, &OnceCrash { phase: "x".into(), count: 0.into() }).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    // Scene preserved: journal, stage and backup remain.
    let journals = root.join(".atp-state/journals");
    let mut found_journal = false;
    for entry in fs::read_dir(&journals).unwrap().flatten() {
        assert!(entry.path().join("journal.json").is_file());
        assert!(entry.path().join("backup").read_dir().unwrap().next().is_some());
        found_journal = true;
    }
    assert!(found_journal);
    assert_eq!(
        fs::read_to_string(root.join("src/main.txt")).unwrap(),
        "用户在中断后乱改的内容\n"
    );
}

#[test]
fn new_commit_rejected_while_transaction_pending() {
    let root = temp_root("pending-blocks");
    seed(&root);
    let plan = build_plan_from_json(&root, PLAN).unwrap();
    let err = commit(&plan, false, &OnceCrash { phase: "after-journal".into(), count: 0.into() }).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Interrupted);

    let other = r##"{"ops":[{"type":"create","path":"x.txt","content":"x"}]}"##;
    let plan2 = build_plan_from_json(&root, other).unwrap();
    let err = commit(&plan2, false, &OnceCrash { phase: "z".into(), count: 0.into() }).unwrap_err();
    assert_eq!(err.kind, ErrorKind::PendingRecovery);
}

#[test]
fn empty_ops_and_unknown_type_are_invalid_plans() {
    let root = temp_root("badplan");
    seed(&root);
    let err = parse_ops(r##"{"ops":[]}"##).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidPlan);
    let err = build_plan_from_json(&root, r##"{"ops":[{"type":"frobnicate","path":"a"}]}"##).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidPlan);
}
