//! Plan data model: user operations, validated per-file actions and JSON I/O.
//!
//! A *plan* is a batch of [`Op`]s. [`Plan::build`] normalizes and validates
//! them into per-file [`FileAction`]s that carry exact old/new byte contents,
//! so commit only has to compare bytes and swap files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};
use crate::json::Json;
use crate::pathsafe::{ensure_parents_safe, join_components, safe_components};

/// A single user-level operation against one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Create `path` with `content`; fails validation if it already exists.
    Create { path: String, content: String },
    /// Delete `path`; it must currently exist.
    Delete { path: String },
    /// Replace exactly one occurrence of `find` with `replace`.
    Replace { path: String, find: String, replace: String },
}

/// Raw operation parsed from JSON (`type` + fields).
#[derive(Debug, Clone)]
struct RawOp {
    op_type: String,
    path: String,
    content: Option<String>,
    find: Option<String>,
    replace: Option<String>,
}

/// Resolved action for one target file after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAction {
    /// Project-relative path with forward slashes.
    pub rel_path: String,
    pub validated: Vec<String>,
    /// Exact current bytes (None means the file is newly created).
    pub old: Option<Vec<u8>>,
    /// Desired bytes (None means the file must be deleted).
    pub new: Option<Vec<u8>>,
    /// Operations in user order (for preview display).
    pub ops: Vec<String>,
}

/// A fully validated, committable plan.
#[derive(Debug, Clone)]
pub struct Plan {
    pub root: PathBuf,
    pub actions: Vec<FileAction>,
    /// User operations preserved for re-serialization.
    pub ops: Vec<Op>,
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

impl Op {
    /// Parse one operation object, e.g. `{"type":"replace","path":"a.txt",
    /// "find":"x","replace":"y"}`.
    pub fn from_json(value: &Json) -> Result<Op> {
        let obj = match value {
            Json::Obj(_) => value,
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidPlan,
                    "每个操作必须是 JSON 对象",
                ))
            }
        };
        let raw = RawOp {
            op_type: require_str(obj, "type")?.to_string(),
            path: require_str(obj, "path")?.to_string(),
            content: get_str(obj, "content").map(str::to_string),
            find: get_str(obj, "find").map(str::to_string),
            replace: obj.get("replace").map(|v| match v {
                Json::Str(s) => Ok(s.clone()),
                _ => Err(Error::new(
                    ErrorKind::InvalidPlan,
                    "字段 replace 必须是字符串",
                )),
            }).transpose()?,
        };
        match raw.op_type.as_str() {
            "create" => Ok(Op::Create {
                path: raw.path,
                content: raw.content.ok_or_else(|| {
                    Error::new(ErrorKind::InvalidPlan, "create 操作缺少 content 字段")
                })?,
            }),
            "delete" => Ok(Op::Delete { path: raw.path }),
            "replace" => Ok(Op::Replace {
                path: raw.path,
                find: raw.find.ok_or_else(|| {
                    Error::new(ErrorKind::InvalidPlan, "replace 操作缺少 find 字段")
                })?,
                replace: raw.replace.ok_or_else(|| {
                    Error::new(ErrorKind::InvalidPlan, "replace 操作缺少 replace 字段")
                })?,
            }),
            other => Err(Error::new(
                ErrorKind::InvalidPlan,
                format!("未知操作类型 {other:?}（仅支持 create/delete/replace）"),
            )),
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Op::Create { path, .. } | Op::Delete { path } | Op::Replace { path, .. } => path,
        }
    }

    pub fn to_json(&self) -> Json {
        let mut map = BTreeMap::new();
        match self {
            Op::Create { path, content } => {
                map.insert("type".into(), Json::from("create"));
                map.insert("path".into(), Json::from(path.as_str()));
                map.insert("content".into(), Json::from(content.as_str()));
            }
            Op::Delete { path } => {
                map.insert("type".into(), Json::from("delete"));
                map.insert("path".into(), Json::from(path.as_str()));
            }
            Op::Replace { path, find, replace } => {
                map.insert("type".into(), Json::from("replace"));
                map.insert("path".into(), Json::from(path.as_str()));
                map.insert("find".into(), Json::from(find.as_str()));
                map.insert("replace".into(), Json::from(replace.as_str()));
            }
        }
        Json::Obj(map)
    }
}

fn require_str<'a>(obj: &'a Json, key: &str) -> Result<&'a str> {
    get_str(obj, key)
        .ok_or_else(|| Error::new(ErrorKind::InvalidPlan, format!("操作缺少字符串字段 {key}")))
}

fn get_str<'a>(obj: &'a Json, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(|v| v.as_str())
}

/// Parse a plan JSON document: either `{"ops":[...]}` or a bare array.
pub fn parse_ops(input: &str) -> Result<Vec<Op>> {
    let doc = Json::parse(input).map_err(|msg| Error::new(ErrorKind::InvalidPlan, msg))?;
    let arr = if let Some(ops) = doc.get("ops") {
        ops.as_array().ok_or_else(|| {
            Error::new(ErrorKind::InvalidPlan, "字段 ops 必须是数组")
        })?
    } else {
        doc.as_array().ok_or_else(|| {
            Error::new(ErrorKind::InvalidPlan, "计划必须是数组或含 ops 数组的对象")
        })?
    };
    if arr.is_empty() {
        return Err(Error::new(ErrorKind::InvalidPlan, "计划不包含任何操作"));
    }
    arr.iter().map(Op::from_json).collect()
}

// ---------------------------------------------------------------------------
// Plan building / validation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Create,
    Delete,
    Replace,
}

struct Grouped {
    rel_path: String,
    components: Vec<String>,
    entries: Vec<(usize, Kind)>,
}

impl Plan {
    /// Validate `ops` against the live contents of `root`.
    ///
    /// This only reads the filesystem; it never creates or modifies files, so
    /// it is safe to use for previews.
    pub fn build(root: &Path, ops: Vec<Op>) -> Result<Plan> {
        let canon_root = root.canonicalize().map_err(|err| {
            Error::new(
                ErrorKind::InvalidRoot,
                format!("项目根目录 {} 无法访问：{err}", root.display()),
            )
        })?;
        if !canon_root.is_dir() {
            return Err(Error::new(
                ErrorKind::InvalidRoot,
                format!("项目根目录不是目录：{}", canon_root.display()),
            ));
        }

        // Group operations by normalized relative path, preserving user order.
        let mut groups: BTreeMap<String, Grouped> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for (idx, op) in ops.iter().enumerate() {
            let components = safe_components(op.path())?;
            if components.first().map(|s| s.as_str()) == Some(crate::engine::STATE_DIR) {
                return Err(Error::new(
                    ErrorKind::UnsafePath,
                    format!("路径 {} 位于引擎保留的状态目录 {} 内，禁止操作", op.path(), crate::engine::STATE_DIR),
                ));
            }
            let rel_path = components.join("/");
            let kind = match op {
                Op::Create { .. } => Kind::Create,
                Op::Delete { .. } => Kind::Delete,
                Op::Replace { .. } => Kind::Replace,
            };
            let group = groups.entry(rel_path.clone()).or_insert_with(|| {
                order.push(rel_path.clone());
                Grouped { rel_path: rel_path.clone(), components: components.clone(), entries: Vec::new() }
            });
            group.entries.push((idx, kind));
        }

        let mut actions = Vec::new();
        for rel_path in &order {
            let group = groups.get(rel_path).expect("group present");
            let action = build_action(&canon_root, &ops, group)?;
            actions.push(action);
        }

        Ok(Plan { root: canon_root, actions, ops })
    }
}

fn build_action(root: &Path, ops: &[Op], group: &Grouped) -> Result<FileAction> {
    let target = join_components(root, &group.components);
    let kinds: Vec<Kind> = group.entries.iter().map(|(_, k)| *k).collect();
    let creates = kinds.iter().filter(|k| **k == Kind::Create).count();
    let deletes = kinds.iter().filter(|k| **k == Kind::Delete).count();
    let replaces = kinds.iter().filter(|k| **k == Kind::Replace).count();
    let rel = &group.rel_path;

    let mut descriptions = Vec::new();

    let action = if creates > 0 {
        if creates > 1 || deletes + replaces > 0 {
            return Err(Error::new(
                ErrorKind::InvalidPlan,
                format!("文件 {rel} 的操作组合冲突：create 不能与其他操作同时出现，也不能重复"),
            ));
        }
        if target.exists() || target.symlink_metadata().is_ok() {
            return Err(Error::new(
                ErrorKind::InvalidPlan,
                format!("create 的目标已存在：{rel}"),
            ));
        }
        ensure_parents_safe(root, &target)?;
        let content = match &ops[group.entries[0].0] {
            Op::Create { content, .. } => content,
            _ => unreachable!(),
        };
        descriptions.push(format!("新增 {} 字节（{} 个 UTF-8 字符）", content.len(), content.chars().count()));
        FileAction {
            rel_path: rel.clone(),
            validated: group.components.clone(),
            old: None,
            new: Some(content.as_bytes().to_vec()),
            ops: descriptions,
        }
    } else if deletes > 0 {
        if deletes > 1 || replaces > 0 {
            return Err(Error::new(
                ErrorKind::InvalidPlan,
                format!("文件 {rel} 的操作组合冲突：delete 不能与 replace 同时出现，也不能重复"),
            ));
        }
        crate::pathsafe::ensure_existing_path_contained(root, &target)?;
        let old = std::fs::read(&target)
            .map_err(|err| Error::new(ErrorKind::Io, format!("读取 {rel} 失败：{err}")))?;
        descriptions.push(format!("删除 {} 字节", old.len()));
        FileAction {
            rel_path: rel.clone(),
            validated: group.components.clone(),
            old: Some(old),
            new: None,
            ops: descriptions,
        }
    } else {
        assert!(replaces > 0);
        crate::pathsafe::ensure_existing_path_contained(root, &target)?;
        let old_bytes = std::fs::read(&target)
            .map_err(|err| Error::new(ErrorKind::Io, format!("读取 {rel} 失败：{err}")))?;
        let text = std::str::from_utf8(&old_bytes).map_err(|_| {
            Error::new(
                ErrorKind::InvalidPlan,
                format!("文件 {rel} 不是合法 UTF-8 文本，引擎只处理文本文件"),
            )
        })?;

        // Resolve every replacement against the ORIGINAL snapshot.
        let mut spans: Vec<(usize, usize, &str)> = Vec::new();
        for (op_idx, _) in &group.entries {
            let (find, replace) = match &ops[*op_idx] {
                Op::Replace { find, replace, .. } => (find.as_str(), replace.as_str()),
                _ => unreachable!(),
            };
            if find.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidPlan,
                    format!("文件 {rel} 中存在空的 find 片段"),
                ));
            }
            let occurrences = find_occurrences(text, find);
            if occurrences.is_empty() {
                return Err(Error::new(
                    ErrorKind::Ambiguous,
                    format!("文件 {rel} 中找不到片段：{:?}", truncate(find)),
                ));
            }
            if occurrences.len() > 1 {
                return Err(Error::new(
                    ErrorKind::Ambiguous,
                    format!("文件 {rel} 中片段出现 {} 次，拒绝猜测位置：{:?}", occurrences.len(), truncate(find)),
                ));
            }
            let (start, end) = occurrences[0];
            spans.push((start, end, replace));
            descriptions.push(format!(
                "替换 [{}..{}] {:?} -> {:?}",
                start,
                start + find.len(),
                truncate(find),
                truncate(replace)
            ));
        }

        // Reject overlapping spans in the original text.
        for i in 0..spans.len() {
            for j in (i + 1)..spans.len() {
                let (a_start, a_end, _) = spans[i];
                let (b_start, b_end, _) = spans[j];
                if a_start < b_end && b_start < a_end {
                    return Err(Error::new(
                        ErrorKind::Ambiguous,
                        format!("文件 {rel} 中存在互相重叠的替换区间 [{a_start}..{a_end}) 与 [{b_start}..{b_end})"),
                    ));
                }
            }
        }

        // Apply tail-to-head so earlier byte offsets stay valid.
        let mut new_bytes = old_bytes.clone();
        for (start, end, replacement) in spans.into_iter().rev() {
            new_bytes.splice(start..end, replacement.as_bytes().iter().copied());
        }

        FileAction {
            rel_path: rel.clone(),
            validated: group.components.clone(),
            old: Some(old_bytes),
            new: Some(new_bytes),
            ops: descriptions,
        }
    };

    Ok(action)
}

/// All byte offsets where `needle` occurs in `haystack`.
fn find_occurrences(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let needle_bytes = needle.as_bytes();
    let hay = haystack.as_bytes();
    if needle_bytes.is_empty() || hay.len() < needle_bytes.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0;
    while from + needle_bytes.len() <= hay.len() {
        if let Some(relative) = find_subslice(&hay[from..], needle_bytes) {
            let start = from + relative;
            out.push((start, start + needle_bytes.len()));
            from = start + 1;
        } else {
            break;
        }
    }
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn truncate(s: &str) -> String {
    let limit = 24;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= limit {
        s.replace('\n', "\\n").replace('\r', "\\r")
    } else {
        let head: String = chars[..limit].iter().collect();
        format!("{}…", head.replace('\n', "\\n").replace('\r', "\\r"))
    }
}

// ---------------------------------------------------------------------------
// Preview rendering and serialization
// ---------------------------------------------------------------------------

impl Plan {
    /// Human-readable preview; says nothing about side effects (there are
    /// none during preview).
    pub fn render_preview(&self) -> String {
        let mut out = String::new();
        out.push_str("== 补丁预览（不会修改目标目录）==\n");
        out.push_str(&format!("项目根目录：{}\n", self.root.display()));
        out.push_str(&format!("涉及文件数：{}\n", self.actions.len()));
        for action in &self.actions {
            let kind = match (&action.old, &action.new) {
                (None, Some(_)) => "新增",
                (Some(_), None) => "删除",
                (Some(a), Some(b)) => {
                    if a == b {
                        "未变化"
                    } else {
                        "修改"
                    }
                }
                (None, None) => "非法",
            };
            out.push_str(&format!("\n[{kind}] {}\n", action.rel_path));
            for line in &action.ops {
                out.push_str(&format!("  - {line}\n"));
            }
        }
        out
    }

    /// Serialize the original user operations to JSON (for plan files).
    pub fn ops_json(&self) -> String {
        let arr = Json::Arr(self.ops.iter().map(Op::to_json).collect());
        let mut doc = BTreeMap::new();
        doc.insert("ops".to_string(), arr);
        Json::Obj(doc).emit_pretty()
    }

    pub fn action_count(&self) -> usize {
        self.actions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atp-plan-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replace_requires_unique_match() {
        let root = temp_root("dup");
        fs::write(root.join("a.txt"), "x x x\n").unwrap();
        let ops = vec![Op::Replace {
            path: "a.txt".into(),
            find: "x".into(),
            replace: "y".into(),
        }];
        let err = Plan::build(&root, ops).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Ambiguous);

        let ops = vec![Op::Replace {
            path: "a.txt".into(),
            find: "zzz".into(),
            replace: "y".into(),
        }];
        let err = Plan::build(&root, ops).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Ambiguous);
    }

    #[test]
    fn overlapping_replacements_rejected() {
        let root = temp_root("overlap");
        fs::write(root.join("a.txt"), "abcdef\n").unwrap();
        let ops = vec![
            Op::Replace { path: "a.txt".into(), find: "abcd".into(), replace: "1".into() },
            Op::Replace { path: "a.txt".into(), find: "cdef".into(), replace: "2".into() },
        ];
        let err = Plan::build(&root, ops).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Ambiguous);
    }

    #[test]
    fn multiple_disjoint_replacements_apply() {
        let root = temp_root("disjoint");
        fs::write(root.join("a.txt"), "alpha BETA gamma\n").unwrap();
        let ops = vec![
            Op::Replace { path: "a.txt".into(), find: "alpha".into(), replace: "Α".into() },
            Op::Replace { path: "a.txt".into(), find: "BETA".into(), replace: "β".into() },
        ];
        let plan = Plan::build(&root, ops).unwrap();
        assert_eq!(plan.actions[0].new.as_deref(), Some("Α β gamma\n".as_bytes()));
    }
}
