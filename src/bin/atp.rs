//! `atp` — atomic-text-patcher 的命令行入口。
//!
//! 退出码：
//!   0 成功
//!   1 用法/其他错误
//!   2 项目根目录无效
//!   3 路径越界/符号链接逃逸
//!   4 计划格式错误
//!   5 片段歧义（不存在/多处/重叠）
//!   6 内容冲突（预览后被改 / 恢复时发现用户修改）
//!   7 目录正被其他写入会话锁定
//!   8 存在未完成事务，需要先 recover
//!   9 受控中断
//!  10 I/O 错误

use std::path::PathBuf;
use std::process::ExitCode;

use atomic_text_patcher::{
    commit, parse_ops, recover, CrashAt, Error, ErrorKind, NoCrash, Plan,
};

struct Args {
    root: Option<String>,
    plan: Option<String>,
    crash_at: Option<String>,
    help: bool,
}

fn print_help() {
    println!(
        "atp — 事务型文本补丁引擎\n\
\n\
用法：\n\
  atp --root <项目目录> preview --plan <计划.json>   预览变化（不修改目录）\n\
  atp --root <项目目录> commit  --plan <计划.json> [--crash-at 阶段]  提交同一份计划\n\
  atp --root <项目目录> recover [--crash-at 阶段]    显式恢复未完成事务\n\
\n\
计划 JSON：{{\"ops\":[{{\"type\":\"create|delete|replace\", ...}}]}}
\
受控中断阶段：after-stage / after-journal / after-switch-<序号> / recover-switch-<序号>\n\
\n\
退出码：0 成功，2 根目录无效，3 路径逃逸，4 计划非法，5 片段歧义，\n\
        6 内容冲突，7 已被锁定，8 待恢复，9 受控中断，10 I/O 错误"
    );
}

fn parse_args(argv: &[String]) -> Result<(String, Args), String> {
    let mut command: Option<String> = None;
    let mut args = Args { root: None, plan: None, crash_at: None, help: false };
    let mut iter = argv.iter();
    while let Some(token) = iter.next() {
        match token.as_str() {
            "--help" | "-h" => args.help = true,
            "--root" => args.root = Some(iter.next().ok_or("--root 缺少参数")?.clone()),
            "--plan" => args.plan = Some(iter.next().ok_or("--plan 缺少参数")?.clone()),
            "--crash-at" => args.crash_at = Some(iter.next().ok_or("--crash-at 缺少参数")?.clone()),
            other if !other.starts_with("--") && command.is_none() => command = Some(other.to_string()),
            other => return Err(format!("未知参数 {other}")),
        }
    }
    command
        .map(|cmd| (cmd, args))
        .ok_or_else(|| "缺少子命令（preview/commit/recover）".to_string())
}

fn load_plan(root: &str, plan_path: &str) -> Result<Plan, Error> {
    let text = std::fs::read_to_string(plan_path).map_err(|err| {
        Error::new(
            ErrorKind::Io,
            format!("无法读取计划文件 {plan_path}：{err}"),
        )
    })?;
    let ops = parse_ops(&text)?;
    Plan::build(&PathBuf::from(root), ops)
}

fn run() -> Result<(), Error> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (command, args) = parse_args(&argv).map_err(|msg| Error::new(ErrorKind::InvalidPlan, msg))?;
    if args.help {
        print_help();
        return Ok(());
    }
    let root = args
        .root
        .ok_or_else(|| Error::new(ErrorKind::InvalidRoot, "必须用 --root 指定项目目录"))?;

    match command.as_str() {
        "preview" => {
            let plan_path = args
                .plan
                .ok_or_else(|| Error::new(ErrorKind::InvalidPlan, "preview 需要 --plan"))?;
            let plan = load_plan(&root, &plan_path)?;
            println!("{}", plan.render_preview());
            let receipt = atomic_text_patcher::engine::Receipt::snapshot(&plan)?;
            let token = receipt.save()?;
            println!("预览完成：目标目录未发生任何写入。");
            println!("提交令牌（项目外状态目录）：{token}");
            println!("请在文件被外部修改前用同一份计划执行 commit。");
        }
        "commit" => {
            let plan_path = args
                .plan
                .ok_or_else(|| Error::new(ErrorKind::InvalidPlan, "commit 需要 --plan"))?;
            let plan = load_plan(&root, &plan_path)?;
            let outcome = match args.crash_at {
                Some(phase) => commit(&plan, true, &CrashAt(phase)),
                None => commit(&plan, true, &NoCrash),
            }?;
            println!(
                "提交成功：事务 {}，共 {} 个文件（新增 {}，修改 {}，删除 {}）",
                outcome.session_id, outcome.applied, outcome.created, outcome.modified, outcome.deleted
            );
        }
        "recover" => {
            let report = match args.crash_at {
                Some(phase) => recover(&PathBuf::from(root), &CrashAt(phase)),
                None => recover(&PathBuf::from(root), &NoCrash),
            }?;
            if report.sessions.is_empty() {
                println!("没有需要恢复的事务，目录已是一致状态。");
            } else {
                println!(
                    "恢复完成：事务 {:?}，补做 {} 个文件切换，冲突 {} 个",
                    report.sessions, report.applied, report.conflicts.len()
                );
            }
        }
        other => {
            return Err(Error::new(
                ErrorKind::InvalidPlan,
                format!("未知子命令 {other}（支持 preview/commit/recover）"),
            ))
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            eprintln!("错误：{err}");
            ExitCode::from(err.kind.exit_code() as u8)
        }
    }
}
