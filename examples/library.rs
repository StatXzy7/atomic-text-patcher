//! 库接口最小示例：构建计划 -> 预览 -> 提交（同进程内）。
//!
//! 运行：ATP_STATE_HOME=/tmp/atp-extra-state cargo run --example library
//!
//! 注意：库默认不强制“先预览”，命令行 `atp commit` 才会强制校验预览回执。

use std::fs;
use std::path::PathBuf;

use atomic_text_patcher::{build_plan_from_json, commit, NoCrash};

fn main() {
    let root = std::env::temp_dir().join("atp-library-example");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("README.md"), "# 演示项目\n\n版本：v1\n").unwrap();

    let plan_json = r#"{
      "ops": [
        {"type": "replace", "path": "README.md", "find": "版本：v1", "replace": "版本：v2"},
        {"type": "create", "path": "docs/intro.txt", "content": "你好，事务补丁！\n"},
        {"type": "create", "path": "notes.txt", "content": "稍后删除我\n"}
      ]
    }"#;

    let plan = build_plan_from_json(&PathBuf::from(&root), plan_json).unwrap();
    println!("{}", plan.render_preview());

    let outcome = commit(&plan, false, &NoCrash).unwrap();
    println!("提交完成：{:?}", outcome);

    assert_eq!(
        fs::read_to_string(root.join("README.md")).unwrap(),
        "# 演示项目\n\n版本：v2\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("docs/intro.txt")).unwrap(),
        "你好，事务补丁！\n"
    );
    println!("断言通过：中文与 LF 换行均按字节保留。");
}
