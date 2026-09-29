# atomic-text-patcher

一个**零第三方依赖**的 Rust 文本补丁引擎（edition 2021，Rust 1.75+，Linux x86_64）。
它让本地开发工具把一批关联修改（新增 / 删除 / 精确文本替换）安全地应用到某个项目目录：
先预览，再由用户用**同一份计划**提交；提交中断后可用显式恢复入口把目录补到一致状态。

## 特性

- **三类操作**：`create` 新增、`delete` 删除、`replace` 精确替换文本片段。
- **只预览不改动**：`preview` 对目标目录零写入；预览回执存放在项目目录之外
  （`ATP_STATE_HOME` / `XDG_STATE_HOME` / 系统临时目录）。
- **过期计划整批拒绝**：提交时重新读取每个文件并与预览快照逐字节比对，
  预览后被外部编辑（含变为符号链接/被创建/被删除）则拒绝整批，指出冲突路径。
- **绝不猜测位置**：替换片段必须在原文中**恰好出现一次**；找不到或出现多处都拒绝。
  同一文件多个替换若在原文上区间重叠也拒绝。
- **事务原子性**：新内容先落到同文件系统的 journal 暂存区（`rename(2)` 原子替换），
  逐文件切换，任意单个文件永远是“整旧”或“整新”，不会留下半个文件。
- **显式恢复**：`atp recover` 扫描 `.atp-state/journals/`，roll-forward 到整新；
  若恢复时发现用户又改了相关文件，**保留 journal/暂存/备份现场**并报告冲突，不覆盖未知内容。
- **受控中断注入**：`--crash-at <阶段>` 在指定阶段 `abort()`（SIGABRT），
  用于真实验证恢复；库接口可用 `CrashHook` 注入不杀进程的模拟中断。
- **并发互斥**：项目根对应一把 `flock(2)` 排他写锁（非阻塞，冲突返回退出码 7），
  锁文件在项目目录之外，内核在进程死亡时自动释放。
- **路径安全**：拒绝绝对路径、`..` 越界、保留目录 `.atp-state`、
  最终组件为符号链接、以及经符号链接目录（目录联接）解析到根目录之外的路径。
- **字节保真**：未修改文件逐字节不变；中文 UTF-8、LF/CRLF 换行风格原样保留。
- **无外部服务**，纯本地 POSIX；**新增第三方依赖数为 0**（限制为不超过两个）。

## 计划文件格式

```json
{
  "ops": [
    {"type": "replace", "path": "src/config.txt", "find": "版本 = 1", "replace": "版本 = 2"},
    {"type": "create",  "path": "docs/intro.md", "content": "# 说明\r\n\r\nCRLF 示例\r\n"},
    {"type": "delete",  "path": "old/废弃.txt"}
  ]
}
```

同一文件的多个 `replace` 允许并存，但必须在原文件快照中各自唯一且区间不重叠；
替换在原快照上定位、从后往前应用。

## 命令行

```bash
# 1) 预览（目标目录零写入）
atp --root ./my-project preview --plan plan.json

# 2) 用同一份计划提交；预览后文件被外部修改 -> rc=6
atp --root ./my-project commit  --plan plan.json

# 3) 显式恢复未完成事务（中断后）
atp --root ./my-project recover

# 受控中断演示：在第 1 个文件切换后立即 SIGABRT（随后 recover）
atp --root ./my-project commit --plan plan.json --crash-at after-switch-0
```

可注入阶段：`after-stage`、`after-journal`、`after-switch-<i>`、`recover-switch-<i>`。

### 退出码

| 退出码 | 含义 |
| ---: | --- |
| 0 | 成功 |
| 2 | 项目根目录无效 |
| 3 | 路径越界 / 符号链接逃逸 / 写入保留目录 |
| 4 | 计划格式错误（JSON、未知操作、操作组合冲突等） |
| 5 | 片段歧义：不存在、出现多处或区间重叠 |
| 6 | 内容冲突（预览后被改；恢复时发现用户修改） |
| 7 | 目录正被其他写入会话锁定 |
| 8 | 存在未完成事务，需先 `recover` |
| 9 | 受控中断（模拟） |
| 10 | I/O 错误 |

## 库接口

```rust
use std::path::Path;
use atomic_text_patcher::{build_plan_from_json, commit, recover, NoCrash};

let root = Path::new("./my-project");
let plan = build_plan_from_json(root, include_str!("../plan.json"))?;
println!("{}", plan.render_preview());           // 只读
let outcome = commit(&plan, /*verify_preview*/ false, &NoCrash)?;
let report  = recover(root, &NoCrash)?;          // 中断后显式恢复
```

`commit` 的第二参数 `verify_preview`：库内直接调用可传 `false`；
命令行固定传 `true`，强制要求与最近一次 preview 匹配的回执（含计划内容哈希）。

运行最小库示例：

```bash
cargo run --example library
```

## 事务与恢复设计

```
项目根/.atp-state/journals/<session>/
  journal.json      # prepared|committed|conflict，逐文件 old/new 哈希与长度
  stage/NNNN.bin    # 新内容（create/modify）
  backup/NNNN.bin   # 旧内容备份（modify/delete）
```

1. 提交持排他锁，拒绝存在的未完成 journal，并清理“journal 写出前”死亡留下的暂存孤儿目录；
2. 重新读取全部目标文件并与计划中的旧字节比较（过期即 `Conflict`）；
3. 新内容写 `stage/`、旧内容写 `backup/`，fsync 后原子写出 `journal.json (prepared)`；
4. 按原顺序 `rename` 切换每个目标（modify/create 用 `rename(2)` 覆盖，delete 直接移除）；
5. 全部切换后 manifest 标 `committed` 并删除会话目录。

恢复时逐文件分类为“旧 / 新 / 未知”：全新则收尾清理，部分旧则按序补齐，
出现“未知”即把 journal 标为 `conflict`、保留全部证据并返回冲突，绝不覆盖。

## 测试与自动演示

```bash
cargo test          # 单元 + 10 个集成测试（含多阶段中断恢复、目录摘要）
cargo run --bin atp-demo
```

根目录 `atk-demo.json` 供服务器自动化录屏：`setup` 用 cargo 构建 release 产物，
`start` 运行自结束的真实业务演示 `atp-demo`（打印输入计划、每步操作、结果和断言，
并在 `after-journal` 与 `after-switch-1` 两个阶段用子进程 SIGABRT 注入后执行恢复），
整体在 1 秒内完成，失败以非零退出。

## 目录布局

```
src/error.rs     错误分类与稳定退出码
src/json.rs      自带的小型严格 JSON 解析/序列化
src/hash.rs      FNV-1a 128（状态目录键、内容指纹）
src/pathsafe.rs  路径组件校验与符号链接/联接逃逸检测
src/lock.rs      flock(2) 共享/排他锁
src/plan.rs      操作模型、唯一性定位、重叠检测、预览渲染
src/engine.rs    journal、暂存/备份、提交、恢复、崩溃注入
src/bin/atp.rs   命令行入口
examples/library.rs
demo/main.rs     atp-demo（自动演示程序）
tests/integration.rs
```
