//! Path containment checks.
//!
//! Every path supplied in a plan must be a *relative* path whose lexical
//! components stay inside the project root and whose final resolution (after
//! following symlinks) is still inside the real (canonicalized) root. This
//! blocks `../escape`, absolute paths, symlink farms and directory junctions
//! created by the user.

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};

/// Lexically split `input` into safe components.
///
/// Returns [`ErrorKind::UnsafePath`] for absolute paths, empty paths,
/// `.`/`..` components, NUL bytes and platform-reserved names.
pub fn safe_components(input: &str) -> Result<Vec<String>> {
    if input.is_empty() {
        return Err(Error::new(ErrorKind::UnsafePath, "路径为空"));
    }
    if input.contains('\0') {
        return Err(Error::new(ErrorKind::UnsafePath, format!("路径包含 NUL：{input:?}")));
    }
    let path = Path::new(input);
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part
                    .to_str()
                    .ok_or_else(|| Error::new(ErrorKind::UnsafePath, format!("路径不是合法 UTF-8：{input:?}")))?;
                if text == "." || text == ".." || text.is_empty() {
                    return Err(Error::new(ErrorKind::UnsafePath, format!("非法路径段：{input:?}")));
                }
                components.push(text.to_string());
            }
            Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::new(
                    ErrorKind::UnsafePath,
                    format!("路径必须是项目根目录内的相对路径：{input:?}"),
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(Error::new(ErrorKind::UnsafePath, format!("路径未指向任何文件：{input:?}")));
    }
    Ok(components)
}

/// Join validated components onto `root` without touching the filesystem.
pub fn join_components(root: &Path, components: &[String]) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in components {
        out.push(part);
    }
    out
}

/// Verify that an *existing* target path resolves inside canonical root.
///
/// `target` must already be the result of [`join_components`]. A symlink as
/// the final component is rejected outright: patches only ever edit regular
/// files, never replace one link with another.
pub fn ensure_existing_path_contained(root_canon: &Path, target: &Path) -> Result<()> {
    let canon = target.canonicalize().map_err(|err| {
        Error::new(
            ErrorKind::Io,
            format!("无法解析路径 {}：{err}", target.display()),
        )
    })?;
    if !canon.starts_with(root_canon) {
        return Err(Error::new(
            ErrorKind::UnsafePath,
            format!(
                "路径 {} 通过符号链接逃逸到项目根目录之外：{}",
                target.display(),
                canon.display()
            ),
        ));
    }
    let meta = target.symlink_metadata().map_err(|err| {
        Error::new(ErrorKind::Io, format!("无法读取 {} 的元数据：{err}", target.display()))
    })?;
    if meta.file_type().is_symlink() {
        return Err(Error::new(
            ErrorKind::UnsafePath,
            format!("拒绝操作符号链接本身：{}", target.display()),
        ));
    }
    if !meta.is_file() {
        return Err(Error::new(
            ErrorKind::InvalidPlan,
            format!("目标不是普通文件：{}", target.display()),
        ));
    }
    Ok(())
}

/// Walk parent components of a not-yet-existing file and refuse symlink
/// directories (which would place the new file outside the root).
pub fn ensure_parents_safe(root_canon: &Path, target: &Path) -> Result<()> {
    let parent = match target.parent() {
        Some(p) => p,
        None => return Ok(()),
    };
    let mut current = root_canon.to_path_buf();
    if parent == current {
        return Ok(());
    }
    let relative = parent.strip_prefix(root_canon).map_err(|_| {
        Error::new(ErrorKind::Io, "父路径不在项目根目录下（内部错误）")
    })?;
    for component in relative.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            if current.symlink_metadata().is_ok() {
                let canon = current.canonicalize().map_err(|err| {
                    Error::new(
                        ErrorKind::Io,
                        format!("无法解析目录 {}：{err}", current.display()),
                    )
                })?;
                if !canon.starts_with(root_canon) {
                    return Err(Error::new(
                        ErrorKind::UnsafePath,
                        format!("目录 {} 经符号链接逃逸出项目根", current.display()),
                    ));
                }
                if !canon.is_dir() {
                    return Err(Error::new(
                        ErrorKind::InvalidPlan,
                        format!("路径中段不是目录：{}", current.display()),
                    ));
                }
            }
        }
    }
    Ok(())
}
