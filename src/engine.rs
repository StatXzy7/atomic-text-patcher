//! Transactional commit engine with crash recovery.
//!
//! Durability strategy (all operations are local POSIX, no services):
//!
//! 1. Every target file is resolved to exact old/new byte vectors during
//!    [`crate::plan::Plan::build`].
//! 2. New content is staged and old content backed up inside
//!    `<root>/.atp-state/journals/<session>/` (same filesystem -> atomic
//!    `rename(2)`).
//! 3. A JSON journal manifest (`state = "prepared"`) is fsynced before any
//!    target is touched.
//! 4. Targets switch one by one via atomic rename. A crash leaves each file
//!    either old or new, never torn.
//! 5. After the switch the manifest becomes `committed`; recovery rolls
//!    forward (all-old -> all-new) or simply finishes cleanup (all-new).
//!    Any file that is neither old nor new is a user conflict: the scene is
//!    preserved and the whole batch is rejected.
//!
//! The inter-process lock file lives OUTSIDE the project tree so previews
//! never write into the target directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::os::unix::fs::OpenOptionsExt;

use crate::error::{io_err, Error, ErrorKind, Result};
use crate::hash::{content_fingerprint, digest_hex, to_hex};
use crate::json::Json;
use crate::lock::{FileLock, LockMode};
use crate::pathsafe::{ensure_parents_safe, join_components};
use crate::plan::Plan;

/// Directory inside the project holding journals, staged files and backups.
pub const STATE_DIR: &str = ".atp-state";
const JOURNALS_DIR: &str = "journals";
/// External state directory (locks, preview receipts).
const EXTERNAL_DIR_NAME: &str = "atomic-text-patcher";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashAction {
    Continue,
    /// Simulate interruption: return [`ErrorKind::Interrupted`] (tests only).
    Simulated,
    /// Kill the process immediately with SIGABRT (flock released by kernel).
    AbortProcess,
}

/// Hook consulted at well-defined commit phases; used to inject interruptions.
pub trait CrashHook {
    fn at(&self, phase: &str) -> CrashAction;
}

/// Never interrupts.
pub struct NoCrash;
impl CrashHook for NoCrash {
    fn at(&self, _phase: &str) -> CrashAction {
        CrashAction::Continue
    }
}

/// Crash when the phase equals a fixed string (used by `--crash-at`).
pub struct CrashAt(pub String);
impl CrashHook for CrashAt {
    fn at(&self, phase: &str) -> CrashAction {
        if phase == self.0 {
            CrashAction::AbortProcess
        } else {
            CrashAction::Continue
        }
    }
}

/// One per-file record persisted in the journal manifest.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub rel_path: String,
    pub components: Vec<String>,
    /// "create" | "modify" | "delete"
    pub kind: String,
    pub old_hash: Option<String>,
    pub old_len: Option<usize>,
    pub new_hash: Option<String>,
    pub new_len: Option<usize>,
    /// Session-relative path of staged new content, if any.
    pub stage_rel: Option<String>,
    /// Session-relative path of backed-up old content, if any.
    pub backup_rel: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Journal {
    pub session_id: String,
    pub root_canon: PathBuf,
    pub state: String,
    pub entries: Vec<ManifestEntry>,
}

// ---------------------------------------------------------------------------
// Paths, ids and atomic writes
// ---------------------------------------------------------------------------

fn state_dir(root: &Path) -> PathBuf {
    root.join(STATE_DIR)
}

fn journals_dir(root: &Path) -> PathBuf {
    state_dir(root).join(JOURNALS_DIR)
}

fn session_dir(root: &Path, id: &str) -> PathBuf {
    journals_dir(root).join(id)
}

fn journal_manifest(root: &Path, id: &str) -> PathBuf {
    session_dir(root, id).join("journal.json")
}

/// External base directory for locks / receipts.
///
/// Honours `ATP_STATE_HOME` (tests and demos use it); otherwise falls back to
/// `$XDG_STATE_HOME/atomic-text-patcher`, then the system temp directory.
pub fn external_base() -> PathBuf {
    if let Ok(dir) = std::env::var("ATP_STATE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join(EXTERNAL_DIR_NAME);
        }
    }
    std::env::temp_dir().join(EXTERNAL_DIR_NAME)
}

fn external_root_dir(root_canon: &Path) -> PathBuf {
    external_base().join("roots").join(digest_hex(root_canon.as_os_str().as_encoded_bytes()))
}

pub fn lock_path(root_canon: &Path) -> PathBuf {
    external_root_dir(root_canon).join("root.lock")
}

fn receipt_path(root_canon: &Path) -> PathBuf {
    external_root_dir(root_canon).join("last-preview.json")
}

/// Generate an unguessable-ish session identifier (timestamp + pid + random).
fn new_session_id() -> String {
    let mut bytes = [0u8; 8];
    // /dev/urandom is always available on Linux; fall back to clock entropy.
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        use std::io::Read as _;
        if f.read_exact(&mut bytes).is_err() {
            fill_from_clock(&mut bytes);
        }
    } else {
        fill_from_clock(&mut bytes);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("s{:016x}-{}", nanos, to_hex(&bytes))
}

fn fill_from_clock(bytes: &mut [u8]) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut value = nanos as u64 ^ std::process::id() as u64;
    for slot in bytes.iter_mut() {
        value = value.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *slot = (value >> 33) as u8;
    }
}

/// Write `data` to `path` atomically (temp file + fsync + rename).
fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| io_err(err, format!("创建目录 {}", parent.display())))?;
    }
    let tmp = path.with_extra_extension();
    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .or_else(|_| {
                fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .mode(mode)
                    .open(&tmp)
            })
            .map_err(|err| io_err(err, format!("创建临时文件 {}", tmp.display())))?;
        file.write_all(data)
            .map_err(|err| io_err(err, format!("写入 {}", tmp.display())))?;
        file.sync_all()
            .map_err(|err| io_err(err, format!("同步 {}", tmp.display())))?;
    }
    fs::rename(&tmp, path).map_err(|err| {
        io_err(err, format!("原子重命名 {} -> {}", tmp.display(), path.display()))
    })?;
    if let Ok(dir) = fs::File::open(path.parent().unwrap_or(Path::new("."))) {
        use std::os::unix::io::AsRawFd;
        // Best effort directory durability.
        unsafe { libc_fsync(dir.as_raw_fd()) };
    }
    Ok(())
}

trait ExtraExt {
    fn with_extra_extension(&self) -> PathBuf;
}

impl ExtraExt for Path {
    fn with_extra_extension(&self) -> PathBuf {
        let mut name = self
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".atp-tmp");
        self.with_file_name(name)
    }
}

extern "C" {
    fn fsync(fd: std::os::unix::io::RawFd) -> std::os::raw::c_int;
}
unsafe fn libc_fsync(fd: std::os::unix::io::RawFd) -> i32 {
    fsync(fd)
}

// ---------------------------------------------------------------------------
// Journal manifest JSON
// ---------------------------------------------------------------------------

impl ManifestEntry {
    fn to_json(&self) -> Json {
        let mut map = BTreeMap::new();
        map.insert("path".into(), Json::from(self.rel_path.as_str()));
        map.insert(
            "components".into(),
            Json::Arr(self.components.iter().map(|c| Json::from(c.as_str())).collect()),
        );
        map.insert("kind".into(), Json::from(self.kind.as_str()));
        if let Some(h) = &self.old_hash {
            map.insert("old_hash".into(), Json::from(h.as_str()));
            map.insert("old_len".into(), Json::Num(self.old_len.unwrap_or(0) as f64));
        }
        if let Some(h) = &self.new_hash {
            map.insert("new_hash".into(), Json::from(h.as_str()));
            map.insert("new_len".into(), Json::Num(self.new_len.unwrap_or(0) as f64));
        }
        if let Some(p) = &self.stage_rel {
            map.insert("stage".into(), Json::from(p.as_str()));
        }
        if let Some(p) = &self.backup_rel {
            map.insert("backup".into(), Json::from(p.as_str()));
        }
        Json::Obj(map)
    }

    fn from_json(value: &Json) -> Result<ManifestEntry> {
        let rel_path = value
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::new(ErrorKind::Io, "journal 记录缺少 path"))?
            .to_string();
        let components = value
            .get("components")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Option<Vec<String>>>()
            })
            .flatten()
            .ok_or_else(|| Error::new(ErrorKind::Io, "journal 记录缺少 components"))?;
        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("modify")
            .to_string();
        Ok(ManifestEntry {
            rel_path,
            components,
            kind,
            old_hash: string_field(value, "old_hash"),
            old_len: usize_field(value, "old_len"),
            new_hash: string_field(value, "new_hash"),
            new_len: usize_field(value, "new_len"),
            stage_rel: string_field(value, "stage"),
            backup_rel: string_field(value, "backup"),
        })
    }
}

fn string_field(value: &Json, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn usize_field(value: &Json, key: &str) -> Option<usize> {
    match value.get(key) {
        Some(Json::Num(n)) => Some(*n as usize),
        _ => None,
    }
}

impl Journal {
    fn write(&self) -> Result<()> {
        let mut entries = BTreeMap::new();
        entries.insert(
            "session".into(),
            Json::from(self.session_id.as_str()),
        );
        entries.insert("state".into(), Json::from(self.state.as_str()));
        entries.insert("root".into(), Json::from(self.root_canon.to_string_lossy().as_ref()));
        entries.insert(
            "entries".into(),
            Json::Arr(self.entries.iter().map(ManifestEntry::to_json).collect()),
        );
        let payload = Json::Obj(entries).emit_pretty();
        let path = journal_manifest(&self.root_canon, &self.session_id);
        atomic_write(&path, payload.as_bytes(), 0o600)
    }

    /// Load a journal by session id.
    pub fn load(root: &Path, session_id: &str) -> Result<Journal> {
        let path = journal_manifest(root, session_id);
        let raw = fs::read_to_string(&path)
            .map_err(|err| io_err(err, format!("读取 journal {}", path.display())))?;
        let doc = Json::parse(&raw).map_err(|msg| Error::new(ErrorKind::Io, msg))?;
        let state = doc
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("prepared")
            .to_string();
        let entries = doc
            .get("entries")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::new(ErrorKind::Io, "journal 缺少 entries"))?;
        let mut parsed = Vec::new();
        for entry in entries {
            parsed.push(ManifestEntry::from_json(entry)?);
        }
        Ok(Journal {
            session_id: session_id.to_string(),
            root_canon: root.to_path_buf(),
            state,
            entries: parsed,
        })
    }

    fn session_dir(&self) -> PathBuf {
        session_dir(&self.root_canon, &self.session_id)
    }
}

// ---------------------------------------------------------------------------
// Preview receipts: detect external edits between preview and commit
// ---------------------------------------------------------------------------

/// Metadata captured during a preview, persisted outside the project tree so
/// previews remain side-effect free with respect to the target directory.
#[derive(Debug, Clone)]
pub struct Receipt {
    pub root_canon: String,
    pub plan_digest: String,
    pub files: BTreeMap<String, String>,
}

impl Receipt {
    /// Record current content fingerprints for every file the plan touches.
    pub fn snapshot(plan: &Plan) -> Result<Receipt> {
        let mut files = BTreeMap::new();
        for action in &plan.actions {
            let target = join_components(&plan.root, &action.validated);
            let fp = match (&action.old, target.symlink_metadata()) {
                (None, Err(_)) => "absent".to_string(),
                (None, Ok(meta)) if meta.file_type().is_symlink() => {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("预览后 {} 变成了符号链接，拒绝提交", action.rel_path),
                    ))
                }
                (None, Ok(_)) => {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("预览后 {} 被外部创建，拒绝提交", action.rel_path),
                    ))
                }
                (Some(bytes), Ok(_)) => {
                    // Commit re-reads anyway; fingerprint here for the receipt.
                    content_fingerprint(bytes)
                }
                (Some(_), Err(_)) => "deleted".to_string(),
            };
            files.insert(action.rel_path.clone(), fp);
        }
        Ok(Receipt {
            root_canon: plan.root.to_string_lossy().to_string(),
            plan_digest: digest_hex(plan.ops_json().as_bytes()),
            files,
        })
    }

    /// Persist the receipt (outside the project). Returns a token string.
    pub fn save(&self) -> Result<String> {
        let root = PathBuf::from(&self.root_canon);
        let mut map = BTreeMap::new();
        map.insert("root".into(), Json::from(self.root_canon.as_str()));
        map.insert("plan_digest".into(), Json::from(self.plan_digest.as_str()));
        let mut files = BTreeMap::new();
        for (path, fp) in &self.files {
            files.insert(path.clone(), Json::from(fp.as_str()));
        }
        map.insert("files".into(), Json::Obj(files));
        let payload = Json::Obj(map).emit_pretty();
        let path = receipt_path(&root);
        atomic_write(&path, payload.as_bytes(), 0o600)?;
        Ok(hex_like_token(payload.as_bytes()))
    }

    fn load(root: &Path) -> Result<Option<Receipt>> {
        let path = receipt_path(root);
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_err(err, format!("读取预览回执 {}", path.display()))),
        };
        let doc = Json::parse(&raw).map_err(|msg| Error::new(ErrorKind::Io, msg))?;
        let mut files = BTreeMap::new();
        if let Some(obj) = doc.get("files").and_then(|v| match v {
            Json::Obj(m) => Some(m),
            _ => None,
        }) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    files.insert(k.clone(), s.to_string());
                }
            }
        }
        Ok(Some(Receipt {
            root_canon: doc
                .get("root")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            plan_digest: doc
                .get("plan_digest")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            files,
        }))
    }

    fn clear(root: &Path) {
        let _ = fs::remove_file(receipt_path(root));
    }
}

fn hex_like_token(data: &[u8]) -> String {
    digest_hex(data)
}

// ---------------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------------

/// Outcome of a successful commit or recovery.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub session_id: String,
    pub applied: usize,
    pub created: usize,
    pub modified: usize,
    pub deleted: usize,
}

fn guard_no_pending_journal(root: &Path) -> Result<()> {
    let dir = journals_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(io_err(err, format!("扫描 journal 目录 {}", dir.display()))),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        // Orphan directory left behind if we died before the manifest was
        // persisted: no evidence to recover from, remove while we hold the
        // exclusive writer lock.
        if !path.join("journal.json").is_file() {
            let _ = fs::remove_dir_all(&path);
            continue;
        }
        if let Ok(journal) = Journal::load(root, &id) {
            return Err(Error::new(
                ErrorKind::PendingRecovery,
                format!(
                    "存在未完成的事务 {}（状态 {}），请先运行 atp recover；新提交已拒绝",
                    journal.session_id, journal.state
                ),
            ));
        }
    }
    Ok(())
}

/// Commit a validated plan.
///
/// `verify_preview=true` additionally requires a current preview receipt and
/// refuses the batch if any touched path changed after the preview.
pub fn commit(plan: &Plan, verify_preview: bool, hook: &dyn CrashHook) -> Result<CommitOutcome> {
    let root = plan.root.clone();
    let _lock = FileLock::acquire(&lock_path(&root), LockMode::Exclusive, true)?;
    guard_no_pending_journal(&root)?;

    if verify_preview {
        verify_receipt_fresh(&root, plan)?;
    }

    let session_id = new_session_id();
    let sdir = session_dir(&root, &session_id);
    fs::create_dir_all(sdir.join("stage"))
        .map_err(|err| io_err(err, "创建 stage 目录"))?;
    fs::create_dir_all(sdir.join("backup"))
        .map_err(|err| io_err(err, "创建 backup 目录"))?;

    let mut entries = Vec::new();

    // --- Phase A: re-read every file and build journal records. ----------
    for (idx, action) in plan.actions.iter().enumerate() {
        let target = join_components(&root, &action.validated);
        let entry = match (&action.old, &action.new) {
            (None, Some(new_bytes)) => {
                if target.symlink_metadata().is_ok() {
                    return Err(conflict_after_preview(&action.rel_path, "已存在"));
                }
                ensure_parents_safe(&root, &target)?;
                let stage_rel = format!("stage/{:04}.bin", idx);
                atomic_write(&sdir.join(&stage_rel), new_bytes, 0o600)?;
                ManifestEntry {
                    rel_path: action.rel_path.clone(),
                    components: action.validated.clone(),
                    kind: "create".into(),
                    old_hash: None,
                    old_len: None,
                    new_hash: Some(digest_hex(new_bytes)),
                    new_len: Some(new_bytes.len()),
                    stage_rel: Some(stage_rel),
                    backup_rel: None,
                }
            }
            (Some(old_bytes), None) => {
                let current = read_existing(&root, &target, &action.rel_path)?;
                if &current != old_bytes {
                    return Err(conflict_after_preview(&action.rel_path, "内容已变化"));
                }
                let backup_rel = format!("backup/{:04}.bin", idx);
                atomic_write(&sdir.join(&backup_rel), old_bytes, 0o600)?;
                ManifestEntry {
                    rel_path: action.rel_path.clone(),
                    components: action.validated.clone(),
                    kind: "delete".into(),
                    old_hash: Some(digest_hex(old_bytes)),
                    old_len: Some(old_bytes.len()),
                    new_hash: None,
                    new_len: None,
                    stage_rel: None,
                    backup_rel: Some(backup_rel),
                }
            }
            (Some(old_bytes), Some(new_bytes)) => {
                let current = read_existing(&root, &target, &action.rel_path)?;
                if &current != old_bytes {
                    return Err(conflict_after_preview(&action.rel_path, "内容已变化"));
                }
                let stage_rel = format!("stage/{:04}.bin", idx);
                let backup_rel = format!("backup/{:04}.bin", idx);
                atomic_write(&sdir.join(&stage_rel), new_bytes, 0o600)?;
                atomic_write(&sdir.join(&backup_rel), old_bytes, 0o600)?;
                ManifestEntry {
                    rel_path: action.rel_path.clone(),
                    components: action.validated.clone(),
                    kind: "modify".into(),
                    old_hash: Some(digest_hex(old_bytes)),
                    old_len: Some(old_bytes.len()),
                    new_hash: Some(digest_hex(new_bytes)),
                    new_len: Some(new_bytes.len()),
                    stage_rel: Some(stage_rel),
                    backup_rel: Some(backup_rel),
                }
            }
            (None, None) => {
                return Err(Error::new(
                    ErrorKind::InvalidPlan,
                    format!("内部错误：{} 没有任何目标内容", action.rel_path),
                ))
            }
        };
        entries.push(entry);
    }

    crash(hook, "after-stage")?;

    // --- Phase B: persist the prepared journal. --------------------------
    let journal = Journal {
        session_id: session_id.clone(),
        root_canon: root.clone(),
        state: "prepared".into(),
        entries,
    };
    journal.write()?;
    crash(hook, "after-journal")?;

    // --- Phase C: switch every target. -----------------------------------
    let count = plan.actions.len();
    for idx in 0..count {
        apply_one_switch(&root, &journal, idx)?;
        crash(hook, &format!("after-switch-{}", idx))?;
    }
    crash(hook, "after-switch-all")?;

    // --- Phase D: mark committed. ----------------------------------------
    let mut committed = journal;
    committed.state = "committed".into();
    committed.write()?;

    let outcome = summarize(&session_id, &committed.entries);
    cleanup_session(&root, &session_id);
    Receipt::clear(&root);
    Ok(outcome)
}

fn summarize(session_id: &str, entries: &[ManifestEntry]) -> CommitOutcome {
    let mut created = 0;
    let mut modified = 0;
    let mut deleted = 0;
    for entry in entries {
        match entry.kind.as_str() {
            "create" => created += 1,
            "delete" => deleted += 1,
            _ => modified += 1,
        }
    }
    CommitOutcome {
        session_id: session_id.to_string(),
        applied: entries.len(),
        created,
        modified,
        deleted,
    }
}

fn read_existing(root: &Path, target: &Path, rel: &str) -> Result<Vec<u8>> {
    crate::pathsafe::ensure_existing_path_contained(root, target)?;
    fs::read(target).map_err(|err| Error::new(ErrorKind::Io, format!("读取 {rel}：{err}")))
}

fn conflict_after_preview(rel: &str, why: &str) -> Error {
    Error::new(
        ErrorKind::Conflict,
        format!("文件 {rel} 在预览后被外部修改（{why}），整批拒绝以避免覆盖未知内容"),
    )
}

fn crash(hook: &dyn CrashHook, phase: &str) -> Result<()> {
    match hook.at(phase) {
        CrashAction::Continue => Ok(()),
        CrashAction::Simulated => Err(Error::new(
            ErrorKind::Interrupted,
            format!("已在阶段 {phase} 注入受控中断"),
        )),
        CrashAction::AbortProcess => {
            eprintln!("!! 受控中断：在阶段 {phase} 立即终止进程（SIGABRT），journal 保持 prepared");
            // Flush stderr best effort, then abort. The kernel releases flock.
            unsafe { libc_abort() };
        }
    }
}

extern "C" {
    fn abort() -> !;
}
unsafe fn libc_abort() -> ! {
    abort()
}

// ---------------------------------------------------------------------------
// Preview freshness
// ---------------------------------------------------------------------------

fn current_fingerprint(root: &Path, components: &[String]) -> String {
    let target = join_components(root, components);
    match target.symlink_metadata() {
        Err(_) => "absent".to_string(),
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                "symlink".to_string()
            } else if meta.is_file() {
                match fs::read(&target) {
                    Ok(bytes) => content_fingerprint(&bytes),
                    Err(_) => "unreadable".to_string(),
                }
            } else {
                "non-file".to_string()
            }
        }
    }
}

fn verify_receipt_fresh(root: &Path, plan: &Plan) -> Result<()> {
    let receipt = Receipt::load(root)?.ok_or_else(|| {
        Error::new(
            ErrorKind::Conflict,
            "没有找到本次预览的回执：请先运行 preview，再用同一份计划 commit",
        )
    })?;
    if receipt.root_canon != root.to_string_lossy() {
        return Err(Error::new(
            ErrorKind::Conflict,
            "预览回执属于其他项目根目录，请重新预览",
        ));
    }
    let plan_digest = digest_hex(plan.ops_json().as_bytes());
    if receipt.plan_digest != plan_digest {
        return Err(Error::new(
            ErrorKind::Conflict,
            "提交的计划与预览时的不是同一份（内容哈希不一致），请重新预览",
        ));
    }
    for action in &plan.actions {
        let expected = receipt.files.get(&action.rel_path);
        let now = current_fingerprint(root, &action.validated);
        match expected {
            None => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!("预览回执中缺少文件 {}，计划与预览不一致", action.rel_path),
                ));
            }
            Some(recorded) if recorded == &now => {}
            Some(recorded) => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "文件 {} 在预览后发生变化（预览：{}，当前：{}），整批拒绝",
                        action.rel_path, recorded, now
                    ),
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-file atomic switch
// ---------------------------------------------------------------------------

fn apply_one_switch(root: &Path, journal: &Journal, idx: usize) -> Result<()> {
    let entry = &journal.entries[idx];
    let target = join_components(root, &entry.components);
    let sdir = journal.session_dir();

    match entry.kind.as_str() {
        "create" => {
            let stage = sdir.join(entry.stage_rel.as_deref().ok_or_else(|| {
                Error::new(ErrorKind::Io, format!("{} 缺少 stage 路径", entry.rel_path))
            })?);
            ensure_parents_safe(root, &target)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    io_err(err, format!("创建新文件目录 {}", parent.display()))
                })?;
            }
            fs::rename(&stage, &target).map_err(|err| {
                io_err(
                    err,
                    format!("落地新文件 {}（{}）", entry.rel_path, stage.display()),
                )
            })?;
        }
        "delete" => {
            // Backup was made during the prepared phase.
            fs::remove_file(&target).map_err(|err| {
                io_err(err, format!("删除目标文件 {}", entry.rel_path))
            })?;
        }
        _ => {
            let stage = sdir.join(entry.stage_rel.as_deref().ok_or_else(|| {
                Error::new(ErrorKind::Io, format!("{} 缺少 stage 路径", entry.rel_path))
            })?);
            // Atomic replace of an existing regular file (same filesystem).
            fs::rename(&stage, &target).map_err(|err| {
                io_err(err, format!("替换文件 {}", entry.rel_path))
            })?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppliedState {
    /// Target still holds the OLD content (switch for this file did not run).
    Old,
    /// Target already holds the NEW content (switch completed before crash).
    New,
    /// Target is missing in a way that does not match either expectation.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub sessions: Vec<String>,
    pub applied: usize,
    pub conflicts: Vec<String>,
}

/// Explicit recovery entry point.
///
/// Finds every journal under `<root>/.atp-state/journals`, acquires the
/// exclusive writer lock, and rolls each transaction forward so the directory
/// ends up entirely new (or was already new). User-modified files are reported
/// as conflicts with all journal evidence left in place.
pub fn recover(root: &Path, hook: &dyn CrashHook) -> Result<RecoveryReport> {
    let canon = root.canonicalize().map_err(|err| {
        Error::new(
            ErrorKind::InvalidRoot,
            format!("项目根目录 {} 无法访问：{err}", root.display()),
        )
    })?;
    let _lock = FileLock::acquire(&lock_path(&canon), LockMode::Exclusive, true)?;

    let pending = list_sessions(&canon)?;
    let mut report = RecoveryReport { sessions: Vec::new(), applied: 0, conflicts: Vec::new() };
    if pending.is_empty() {
        return Ok(report);
    }

    for session_id in pending {
        recover_one(&canon, &session_id, hook, &mut report)?;
    }
    Ok(report)
}

fn list_sessions(root: &Path) -> Result<Vec<String>> {
    let dir = journals_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_err(err, format!("扫描 {}", dir.display()))),
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        // No manifest means staging died before the journal was durable;
        // targets cannot have been touched, so the directory is just
        // garbage we can remove while holding the exclusive lock.
        if !entry.path().join("journal.json").is_file() {
            let _ = fs::remove_dir_all(entry.path());
            continue;
        }
        ids.push(entry.file_name().to_string_lossy().to_string());
    }
    ids.sort();
    Ok(ids)
}

fn recover_one(
    root: &Path,
    session_id: &str,
    hook: &dyn CrashHook,
    report: &mut RecoveryReport,
) -> Result<()> {
    let journal = Journal::load(root, session_id)?;
    let sdir = journal.session_dir();

    let mut pending_idx = Vec::new();
    let mut conflicts = Vec::new();

    for (idx, entry) in journal.entries.iter().enumerate() {
        let target = join_components(root, &entry.components);
        let state = classify_entry(&sdir, &target, entry);
        match state {
            AppliedState::New => {}
            AppliedState::Old => pending_idx.push(idx),
            AppliedState::Unknown => {
                conflicts.push(format!(
                    "{}：当前内容既不是事务前版本也不是事务后版本（疑似用户已修改）",
                    entry.rel_path
                ));
            }
        }
    }

    if !conflicts.is_empty() {
        // Preserve everything: do not touch targets, stage, backup or journal.
        let mut conflict_journal = journal;
        conflict_journal.state = "conflict".into();
        let _ = conflict_journal.write();
        report.conflicts.extend(
            conflicts
                .iter()
                .map(|c| format!("事务 {session_id} {c}")),
        );
        return Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "恢复事务 {session_id} 时发现用户修改，已保留现场（{} 仍在 {}）：\n  - {}",
                session_id,
                sdir.display(),
                conflicts.join("\n  - ")
            ),
        ));
    }

    if pending_idx.is_empty() {
        // Crash happened after switches but before cleanup.
        report.sessions.push(session_id.to_string());
        cleanup_session(root, session_id);
        return Ok(());
    }

    // Roll forward strictly in original order.
    for idx in pending_idx {
        apply_one_switch(root, &journal, idx)?;
        report.applied += 1;
        crash(hook, &format!("recover-switch-{idx}"))?;
    }
    let mut done = journal;
    done.state = "committed".into();
    done.write()?;
    report.sessions.push(session_id.to_string());
    cleanup_session(root, session_id);
    Ok(())
}

fn classify_entry(sdir: &Path, target: &Path, entry: &ManifestEntry) -> AppliedState {
    // Symlink targets are never something we would have created: conflict.
    if let Ok(meta) = target.symlink_metadata() {
        if meta.file_type().is_symlink() {
            return AppliedState::Unknown;
        }
    }

    match entry.kind.as_str() {
        "create" => {
            let stage = entry.stage_rel.as_ref().map(|p| sdir.join(p));
            match (fs::metadata(target).is_ok(), stage) {
                (false, Some(stage)) if stage.is_file() => AppliedState::Old,
                (true, _) => {
                    if matches_hash(target, entry.new_hash.as_deref(), entry.new_len) {
                        AppliedState::New
                    } else {
                        AppliedState::Unknown
                    }
                }
                _ => AppliedState::Unknown,
            }
        }
        "delete" => match fs::metadata(target) {
            Err(_) => AppliedState::New,
            Ok(_) => {
                if matches_hash(target, entry.old_hash.as_deref(), entry.old_len) {
                    AppliedState::Old
                } else {
                    AppliedState::Unknown
                }
            }
        },
        _modify => {
            // modify: old file + stage present -> Old; new content -> New.
            let stage = entry.stage_rel.as_ref().map(|p| sdir.join(p));
            let stage_present = stage.as_ref().map(|p| p.is_file()).unwrap_or(false);
            if matches_hash(target, entry.old_hash.as_deref(), entry.old_len) && stage_present {
                AppliedState::Old
            } else if matches_hash(target, entry.new_hash.as_deref(), entry.new_len) {
                AppliedState::New
            } else {
                AppliedState::Unknown
            }
        }
    }
}

fn matches_hash(target: &Path, expected: Option<&str>, expected_len: Option<usize>) -> bool {
    match (fs::read(target), expected) {
        (Ok(bytes), Some(hash)) => {
            digest_hex(&bytes) == hash
                && expected_len.map(|len| len == bytes.len()).unwrap_or(true)
        }
        _ => false,
    }
}

fn cleanup_session(root: &Path, session_id: &str) {
    let dir = session_dir(root, session_id);
    let _ = fs::remove_dir_all(&dir);
}
