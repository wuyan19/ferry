//! `file.*` 沙箱文件操作（protocol.md §4.2）。
//!
//! 载荷 ≤ 1 MiB 的同步实现：base64 解码 + 顺序 IO 都在微秒~毫秒级，
//! 不值得 spawn_blocking 的开销；调用方运行在独立请求任务中，不阻塞读循环。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::FerryError;
use crate::params::{
    DirEntry, FileDeleteParams, FileDeleteResult, FileListParams, FileListResult, FileReadParams,
    FileReadResult, FileStatParams, FileStatResult, FileWriteParams, FileWriteResult,
};
use crate::sandbox::Sandbox;

fn mtime_of(md: &std::fs::Metadata) -> f64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn type_of(md: &std::fs::Metadata) -> &'static str {
    if md.is_dir() {
        "dir"
    } else if md.is_file() {
        "file"
    } else {
        "other"
    }
}

fn hex_digest(hasher: Sha256) -> String {
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hex_digest(hasher))
}

pub fn list(sandbox: &Sandbox, p: FileListParams) -> Result<FileListResult, FerryError> {
    let dir = sandbox.resolve(&p.path)?;
    let md = std::fs::metadata(&dir).map_err(|e| map_missing(&p.path, e))?;
    if !md.is_dir() {
        return Err(FerryError::invalid_params(format!("not a directory: {:?}", p.path)));
    }
    let recursive = p.recursive.unwrap_or(false);
    let mut entries = Vec::new();
    walk(&dir, "", recursive, &mut entries)
        .map_err(|e| FerryError::io("file.list", e))?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(FileListResult { entries })
}

/// `base` 为当前子目录相对 `dir` 的路径前缀（"" = 顶层）。
fn walk(dir: &std::path::Path, base: &str, recursive: bool, out: &mut Vec<DirEntry>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let md = entry.metadata()?;
        let name = if base.is_empty() {
            entry.file_name().to_string_lossy().into_owned()
        } else {
            format!("{base}/{}", entry.file_name().to_string_lossy())
        };
        out.push(DirEntry {
            name: name.clone(),
            r#type: type_of(&md).to_string(),
            size: md.len(),
            mtime: mtime_of(&md),
        });
        if recursive && md.is_dir() {
            walk(&entry.path(), &name, recursive, out)?;
        }
    }
    Ok(())
}

pub fn stat(sandbox: &Sandbox, p: FileStatParams) -> Result<FileStatResult, FerryError> {
    let path = sandbox.resolve(&p.path)?;
    let md = std::fs::metadata(&path).map_err(|e| map_missing(&p.path, e))?;
    let sha256 = if md.is_file() {
        Some(sha256_file(&path).map_err(|e| FerryError::io("file.stat", e))?)
    } else {
        None
    };
    Ok(FileStatResult {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.path.clone()),
        r#type: type_of(&md).to_string(),
        size: md.len(),
        mtime: mtime_of(&md),
        sha256,
    })
}

pub fn write(sandbox: &Sandbox, p: FileWriteParams) -> Result<FileWriteResult, FerryError> {
    let bytes = B64
        .decode(p.content_base64.as_bytes())
        .map_err(|e| FerryError::invalid_params(format!("content_base64 is not valid base64: {e}")))?;
    let path = sandbox.resolve(&p.path)?;

    let old_len = std::fs::metadata(&path).map(|md| md.len()).unwrap_or(0);
    sandbox.check_quota(bytes.len().saturating_sub(old_len as usize) as u64)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| FerryError::io("file.write", e))?;
    }
    std::fs::write(&path, &bytes).map_err(|e| FerryError::io("file.write", e))?;
    sandbox.add_used(bytes.len() as i64 - old_len as i64);

    if let Some(mode) = p.mode {
        apply_mode(&path, mode);
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(FileWriteResult {
        bytes_written: bytes.len() as u64,
        sha256: hex_digest(hasher),
    })
}

#[cfg(unix)]
fn apply_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn apply_mode(_path: &std::path::Path, _mode: u32) {
    // Windows 无对应语义，静默忽略（协议声明 best effort）。
}

pub fn read(sandbox: &Sandbox, p: FileReadParams) -> Result<FileReadResult, FerryError> {
    let path = sandbox.resolve(&p.path)?;
    let md = std::fs::metadata(&path).map_err(|e| map_missing(&p.path, e))?;
    if !md.is_file() {
        return Err(FerryError::invalid_params(format!("not a readable file: {:?}", p.path)));
    }
    let max = p.max_bytes.unwrap_or(u64::MAX);
    let take = md.len().min(max);
    use std::io::Read;
    let mut f = std::fs::File::open(&path).map_err(|e| FerryError::io("file.read", e))?;
    let mut buf = Vec::with_capacity(take as usize);
    f.by_ref().take(take).read_to_end(&mut buf)
        .map_err(|e| FerryError::io("file.read", e))?;
    let truncated = (buf.len() as u64) < md.len();
    Ok(FileReadResult {
        content_base64: B64.encode(&buf),
        truncated,
    })
}

pub fn delete(sandbox: &Sandbox, p: FileDeleteParams) -> Result<FileDeleteResult, FerryError> {
    let path = sandbox.resolve(&p.path)?;
    let md = std::fs::metadata(&path).map_err(|e| map_missing(&p.path, e))?;
    let removed = if md.is_dir() {
        let size = dir_size(&path);
        if p.recursive.unwrap_or(false) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_dir(&path)
        }
        .map_err(|e| {
            // Windows/Unix 非空目录删除失败 → 提示补 recursive
            if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                || cfg!(windows) && e.raw_os_error() == Some(145) // ERROR_DIR_NOT_EMPTY
            {
                FerryError::invalid_params(format!(
                    "directory not empty: {:?}; pass recursive:true",
                    p.path
                ))
            } else {
                FerryError::io("file.delete", e)
            }
        })?;
        size
    } else {
        std::fs::remove_file(&path).map_err(|e| FerryError::io("file.delete", e))?;
        md.len()
    };
    sandbox.add_used(-(removed as i64));
    Ok(FileDeleteResult { deleted: true })
}

fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for entry in rd.flatten() {
                if let Ok(md) = entry.metadata() {
                    if md.is_dir() {
                        stack.push(entry.path());
                    } else {
                        total += md.len();
                    }
                }
            }
        }
    }
    total
}

/// NotFound → FILE_NOT_FOUND，其余 IO 错误透传 INTERNAL_ERROR。
fn map_missing(path: &str, e: std::io::Error) -> FerryError {
    if e.kind() == std::io::ErrorKind::NotFound {
        FerryError::file_not_found(path).with_details(json!({ "path": path, "io": e.to_string() }))
    } else {
        FerryError::io("fs", e).with_details(json!({ "path": path }))
    }
}
