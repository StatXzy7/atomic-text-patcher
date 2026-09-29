//! atp-demo：真实业务演示程序（terminal，kind=terminal 的 start 目标）。
//!
//! 模式：
//!   atp-demo                    编排整个演示，并自行清理临时目录
//!   atp-demo crash --root DIR --plan FILE --phase PHASE
//!                               子进程：执行 commit 并在 PHASE 硬中断（SIGABRT）
//!
//! 任何断言失败都以非零退出码结束。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use atomic_text_patcher::{
    build_plan_from_json, commit, parse_ops, recover, CrashAt, ErrorKind, NoCrash, Plan,
};

fn plan_json() -> &'static str {
    r##"{
  "ops": [
    {"type": "replace", "path": "src/配置.txt", "find": "版本 = 1", "replace": "版本 = 2"},
    {"type": "replace", "path": "src/配置.txt", "find": "language: en", "replace": "language: zh"},
    {"type": "create",  "path": "docs/说明.md", "content": "# 说明\r\n\r\n事务补丁演示。\r\n"},
    {"type": "delete",  "path": "old/废弃.txt"}
  ]
}"##
}

fn seed(root: &Path) {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("old")).unwrap();
    fs::write(
        root.join("src/配置.txt"),
        "# 项目配置\nlanguage: en\n版本 = 1\n".as_bytes(),
    )
    .unwrap();
    fs::write(root.join("old/废弃.txt"), "此文件将被删除\n".as_bytes()).unwrap();
    fs::write(root.join("二进制.dat"), vec![0x89u8, b'X', 0x00, 0xff, b'\n']).unwrap();
}

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

fn check(label: &str, cond: bool) {
    if cond {
        println!("  [断言通过] {label}");
    } else {
        println!("  [断言失败] {label}");
        std::process::exit(1);
    }
}

fn build(root: &Path) -> Plan {
    build_plan_from_json(root, plan_json()).unwrap_or_else(|err| panic!("计划构建失败：{err}"))
}

fn assert_final(root: &Path) {
    let main = fs::read(root.join("src/配置.txt")).unwrap();
    check("替换后内容正确（中文/UTF-8 与 LF 保持）", main == "# 项目配置\nlanguage: zh\n版本 = 2\n".as_bytes());
    let doc = fs::read(root.join("docs/说明.md")).unwrap();
    check("新建文件保留 CRLF 换行风格", doc == "# 说明\r\n\r\n事务补丁演示。\r\n".as_bytes());
    check("删除生效", !root.join("old/废弃.txt").exists());
    check("未修改文件字节不变", fs::read(root.join("二进制.dat")).unwrap() == vec![0x89u8, b'X', 0x00, 0xff, b'\n']);
    let journals = root.join(".atp-state/journals");
    check("事务证据目录已清理", !journals.exists() || fs::read_dir(&journals).unwrap().next().is_none());
}

fn demo_preview(root: &Path) {
    println!("\n[1/5] 预览（preview）——目标目录不得有任何变化");
    println!("输入计划 JSON：\n{}", plan_json());
    let before = tree_summary(root);
    let plan = build(root);
    println!("{}", plan.render_preview());
    let receipt = atomic_text_patcher::engine::Receipt::snapshot(&plan).unwrap();
    receipt.save().unwrap();
    let after = tree_summary(root);
    check("预览前后目录摘要完全一致", before == after);
    check("状态（回执）写在项目目录之外", !root.join(".atp-state").exists());
}

fn demo_rejections(root: &Path) {
    println!("\n[2/5] 歧义替换拒绝（片段不存在 / 出现多处）");
    let miss = r#"{"ops":[{"type":"replace","path":"src/配置.txt","find":"NOT-THERE","replace":"x"}]}"#;
    check("片段不存在 -> Ambiguous", build_plan_from_json(root, miss).unwrap_err().kind == ErrorKind::Ambiguous);
    let dup = r#"{"ops":[{"type":"replace","path":"src/配置.txt","find":"\n","replace":"X"}]}"#;
    check("片段出现多处 -> Ambiguous", build_plan_from_json(root, dup).unwrap_err().kind == ErrorKind::Ambiguous);
    let overlap = r#"{"ops":[
      {"type":"replace","path":"src/配置.txt","find":"版本 = 1","replace":"A"},
      {"type":"replace","path":"src/配置.txt","find":"= 1\n","replace":"B"}]}"#;
    check("同文件重叠替换 -> Ambiguous", build_plan_from_json(root, overlap).unwrap_err().kind == ErrorKind::Ambiguous);

    println!("\n[3/5] 路径逃逸拒绝");
    let escape = r#"{"ops":[{"type":"delete","path":"../../etc/hostname"}]}"#;
    check("../ 越界路径 -> UnsafePath", build_plan_from_json(root, escape).unwrap_err().kind == ErrorKind::UnsafePath);
}

fn demo_stale_conflict(root: &Path) {
    println!("\n[4/5] 过期计划：预览后被外部编辑，整批拒绝");
    let plan = build(root);
    let receipt = atomic_text_patcher::engine::Receipt::snapshot(&plan).unwrap();
    receipt.save().unwrap();
    let mut bytes = fs::read(root.join("src/配置.txt")).unwrap();
    bytes.extend_from_slice("\n# 外部编辑器改动".as_bytes());
    fs::write(root.join("src/配置.txt"), bytes).unwrap();
    let err = commit(&plan, true, &NoCrash).unwrap_err();
    check("过期提交 -> Conflict", err.kind == ErrorKind::Conflict);
    check("整批回滚：待删除文件仍在", root.join("old/废弃.txt").exists());
    check("整批回滚：新文件未落地", !root.join("docs/说明.md").exists());
}

fn demo_crash_recover(work: &Path, phase: &str) {
    println!("\n[5/5] 写入阶段 {phase} 注入硬中断（子进程 SIGABRT）后显式恢复");
    let root = work.join(phase.replace('-', "_"));
    seed(&root);
    std::env::set_var("ATP_STATE_HOME", work.join("external-state"));
    let plan_path = root.join("plan.json");
    fs::write(&plan_path, plan_json()).unwrap();

    let exe = std::env::current_exe().expect("无法定位演示可执行文件");
    let status = Command::new(exe)
        .args(["crash", "--root"])
        .arg(&root)
        .arg("--plan")
        .arg(&plan_path)
        .arg("--phase")
        .arg(phase)
        .status()
        .expect("无法启动中断子进程");
    println!("子进程（硬中断）退出状态：{status:?}");
    check("子进程确实非正常结束", !status.success());
    // flock 由内核释放：父进程恢复前必须能拿到锁。
    check("中断后没有半套不可读文件（src/配置.txt 仍为旧版或新版）", {
        let p = root.join("src/配置.txt");
        let b = fs::read(&p).unwrap();
        b == "# 项目配置\nlanguage: en\n版本 = 1\n".as_bytes()
            || b == "# 项目配置\nlanguage: zh\n版本 = 2\n".as_bytes()
    });

    let report = recover(&root, &NoCrash).expect("恢复应当成功");
    println!("恢复报告：sessions={:?} 补做切换={} 冲突={}", report.sessions, report.applied, report.conflicts.len());
    assert_final(&root);
    println!("恢复后目录内容摘要：");
    for (path, fp) in &tree_summary(&root) {
        println!("  {path:>20}  {fp}");
    }
}

fn child_crash(args: &[String]) -> ExitCode {
    let mut root = None;
    let mut plan_file = None;
    let mut phase = None;
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        match token.as_str() {
            "--root" => root = iter.next().cloned(),
            "--plan" => plan_file = iter.next().cloned(),
            "--phase" => phase = iter.next().cloned(),
            other => panic!("crash 子模式未知参数 {other}"),
        }
    }
    let root = PathBuf::from(root.expect("--root"));
    let text = fs::read_to_string(plan_file.expect("--plan")).unwrap();
    let ops = parse_ops(&text).unwrap();
    let plan = Plan::build(&root, ops).unwrap();
    let result = commit(&plan, false, &CrashAt(phase.expect("--phase")));
    eprintln!("子进程 commit 结果：{result:?}");
    ExitCode::from(if result.is_ok() { 20 } else { 0 })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s.as_str()) == Some("crash") {
        return child_crash(&args[1..]);
    }

    let work = std::env::temp_dir().join(format!(
        "atp-demo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let root = work.join("project");
    seed(&root);
    std::env::set_var("ATP_STATE_HOME", work.join("external-state"));

    println!("=== atomic-text-patcher 自动演示 ===");
    println!("项目目录：{}", root.display());

    demo_preview(&root);
    demo_rejections(&root);
    demo_stale_conflict(&root);

    // 硬中断 + 恢复，覆盖 journal 写出后与部分文件切换后两个阶段。
    demo_crash_recover(&work, "after-journal");
    demo_crash_recover(&work, "after-switch-1");

    // 再演示一次无中断的正常提交（恢复路径之外的 happy path 收尾）。
    let happy = work.join("happy");
    seed(&happy);
    std::env::set_var("ATP_STATE_HOME", work.join("external-state").join("happy"));
    let plan = build(&happy);
    let outcome = commit(&plan, false, &NoCrash).unwrap();
    println!("\n[收尾] 正常提交：{} 个文件（新增 {} 修改 {} 删除 {}）",
        outcome.applied, outcome.created, outcome.modified, outcome.deleted);
    assert_final(&happy);

    let _ = fs::remove_dir_all(&work);
    println!("\n全部输入、操作、结果与断言均已打印；演示成功，临时目录已清理。");
    ExitCode::from(0)
}
