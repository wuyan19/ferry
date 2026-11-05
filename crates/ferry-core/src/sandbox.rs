//! 沙箱：路径强校验 + 配额记账（protocol.md §4.2）。
//!
//! 规则：
//! - 所有 `path` 相对沙箱根、以 `/` 分隔；含 `..`、反斜杠、盘符、绝对前缀一律
//!   `SANDBOX_VIOLATION`；
//! - 解析后做物理包含检查（canonicalize 锚定最近存在的祖先），堵住符号链接逃逸；
//! - 配额为增量记账（启动时全量扫描一次），对覆盖写做差值修正。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use serde_json::json;

use crate::error::{ErrCode, FerryError};

pub struct Sandbox {
    /// canonicalize 后的根（Windows 为 `\\?\` 前缀形式），仅用于包含检查。
    canonical_root: PathBuf,
    /// 展示用的绝对路径（不带的 `\\?\` 前缀），出现在 server_ready / server.status。
    display_root: PathBuf,
    quota_bytes: u64,
    used_bytes: AtomicI64,
}

impl Sandbox {
    /// 创建（若缺）并锚定沙箱根目录；失败返回 io::Error，由 main 走启动失败路径。
    pub fn new(root: PathBuf, quota_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let display_root = std::path::absolute(&root)?;
        let canonical_root = std::fs::canonicalize(&root)?;
        let used = scan_used(&canonical_root)?;
        Ok(Self {
            canonical_root,
            display_root,
            quota_bytes,
            used_bytes: AtomicI64::new(used as i64),
        })
    }

    pub fn display_root(&self) -> &Path {
        &self.display_root
    }

    pub fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    pub fn used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Relaxed).max(0) as u64
    }

    /// 增量记账（写/删/传输完成时调用），delta 为本次净增量。
    pub fn add_used(&self, delta: i64) {
        self.used_bytes.fetch_add(delta, Ordering::Relaxed);
    }

    /// `used + additional > quota` 时返回 QUOTA_EXCEEDED。
    pub fn check_quota(&self, additional: u64) -> Result<(), FerryError> {
        let used = self.used_bytes() as i128;
        if used + additional as i128 > self.quota_bytes as i128 {
            return Err(FerryError::new(
                ErrCode::QuotaExceeded,
                format!(
                    "sandbox quota exceeded: used {} + requested {} > quota {}",
                    self.used_bytes(),
                    additional,
                    self.quota_bytes
                ),
            )
            .with_suggestion("Delete unneeded files via file.delete, then retry.")
            .with_details(json!({
                "used_bytes": self.used_bytes(),
                "quota_bytes": self.quota_bytes,
            })));
        }
        Ok(())
    }

    /// 协议相对路径 → 沙箱内物理路径。校验失败返回 SANDBOX_VIOLATION。
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, FerryError> {
        if rel.contains('\0') || rel.starts_with('/') {
            // 前导 '/' = 绝对路径，无条件拒绝
            return Err(FerryError::sandbox_violation(rel));
        }
        let mut path = self.canonical_root.clone();
        for comp in rel.split('/') {
            match comp {
                "" | "." => continue, // 容忍首尾/重复斜杠；空串即根
                ".." => return Err(FerryError::sandbox_violation(rel)),
                c if c.contains('\\') || c.contains(':') => {
                    return Err(FerryError::sandbox_violation(rel));
                }
                c => path.push(c),
            }
        }
        // 物理包含检查：符号链接指向根外时拒绝。
        match self.anchored(&path) {
            Some(c) if c.starts_with(&self.canonical_root) => Ok(path),
            _ => Err(FerryError::sandbox_violation(rel)),
        }
    }

    /// canonicalize 目标；目标不存在时锚定最近存在的祖先再拼回尾部。
    fn anchored(&self, path: &Path) -> Option<PathBuf> {
        if let Ok(c) = path.canonicalize() {
            return Some(c);
        }
        let mut probe = path.to_path_buf();
        let mut tail = Vec::new();
        while !probe.exists() {
            let name = probe.file_name()?.to_owned();
            tail.push(name);
            probe = probe.parent()?.to_owned();
        }
        let mut c = probe.canonicalize().ok()?;
        while let Some(t) = tail.pop() {
            c.push(t);
        }
        Some(c)
    }
}

/// 全量扫描目录字节用量（仅文件）。
fn scan_used(root: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let md = entry.metadata()?;
            if md.is_dir() {
                stack.push(entry.path());
            } else {
                total += md.len();
            }
        }
    }
    Ok(total)
}

/// 沙箱句柄的共享形态（Api 持有）。
pub type SharedSandbox = Arc<Sandbox>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(dir: &std::path::Path) -> Sandbox {
        Sandbox::new(dir.to_path_buf(), 1024 * 1024).unwrap()
    }

    #[test]
    fn resolves_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = sandbox(tmp.path());
        let p = sb.resolve("a/b/c.txt").unwrap();
        assert!(p.starts_with(&sb.canonical_root));
        assert!(p.ends_with("a/b/c.txt"));
        // 根本身与冗余斜杠合法
        assert!(sb.resolve("").is_ok());
        assert!(sb.resolve("./a//b.txt").is_ok());
    }

    #[test]
    fn rejects_escaping_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = sandbox(tmp.path());
        for bad in [
            "../evil",
            "a/../../evil",
            "/etc/passwd",
            "a\\b",
            "C:\\win",
            "C:/win",
            "\\\\srv\\share",
            "a\0b",
        ] {
            let err = sb.resolve(bad).unwrap_err();
            assert_eq!(err.code, ErrCode::SandboxViolation, "case {bad:?}");
        }
    }

    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sb = sandbox(tmp.path());
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("leak")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), tmp.path().join("leak")).unwrap();
        assert_eq!(sb.resolve("leak/x").unwrap_err().code, ErrCode::SandboxViolation);
    }

    #[test]
    fn tracks_quota() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().to_path_buf(), 100).unwrap();
        assert!(sb.check_quota(100).is_ok());
        assert!(sb.check_quota(101).is_err());
        sb.add_used(60);
        assert!(sb.check_quota(40).is_ok());
        assert!(sb.check_quota(41).is_err());
        sb.add_used(-10); // 覆盖写差值
        assert_eq!(sb.used_bytes(), 50);
    }
}
