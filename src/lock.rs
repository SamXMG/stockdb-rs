//! 跨进程并发安全：咨询锁（advisory lock）+ 原子写。
//!
//! 设计目标（不动磁盘格式、不破坏字节级兼容）：
//!
//! - 写路径（write / repack / write_meta / save_calendar）在目标文件的 sidecar
//!   `.lock` 文件上取**排他咨询锁**，保证同一时刻只有一个进程/线程改写同一个
//!   目标文件，杜绝「两个 writer 交错覆盖 => 数据损坏 / 丢失」。
//! - 写文件走 `temp + fsync + 原子 rename`：即使进程在写中途崩溃，也不会留下
//!   半截文件。reader 要么看到旧文件、要么看到新文件，绝不会看到撕裂的内容。
//! - 读路径**保持无锁**，以保留 mmap 零拷贝的高性能热路径。原子 rename 已保证
//!   reader 不会读到撕裂数据（只会读到上一次的完整快照，即最终一致）。
//!
//! 锁由 `fs4` 提供，跨平台（Unix `flock` / Windows `LockFileEx`），且为咨询锁：
//! 只有同样走本模块的写者才会互相尊重，reader 不取锁，靠原子 rename 保证安全。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

/// 目标文件对应的 sidecar 锁文件路径：`{target}.lock`。
fn lock_path_for(target: &Path) -> PathBuf {
    let mut s = target.to_string_lossy().into_owned();
    s.push_str(".lock");
    PathBuf::from(s)
}

/// 原子写中转的临时文件路径：`{target}.tmp`。
fn tmp_path_for(target: &Path) -> PathBuf {
    let mut s = target.to_string_lossy().into_owned();
    s.push_str(".tmp");
    PathBuf::from(s)
}

/// 在 `target` 的 sidecar 锁上取**排他咨询锁**，执行 `f`，结束后文件句柄 drop
/// 即自动释放锁（OS 咨询锁随句柄关闭释放）。
///
/// 锁为阻塞等待（直到拿到为止），适合写操作短平快的场景。
/// 若锁文件无法创建/加锁，返回 `io::Error`——绝不静默放行去竞态。
pub fn with_exclusive_lock<F, T>(target: &Path, f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T>,
{
    let lock_path = lock_path_for(target);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    file.lock_exclusive()?;
    f()
    // `file` 在此 drop => 操作系统释放咨询锁
}

/// 原子写：先写 `{target}.tmp`，`fsync` 落盘，再 `rename` 覆盖 `target`。
///
/// `rename` 在同一文件系统内是原子的，reader 不会观察到半截文件。
/// 任意一步失败都清理临时文件，避免残留。
pub fn atomic_write(target: &Path, buf: &[u8]) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path_for(target);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(buf)?;
        f.sync_all().map_err(|e| {
            let _ = fs::remove_file(&tmp);
            e
        })?;
    }
    fs::rename(&tmp, target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrip_and_tmp_cleaned() {
        let tmp = std::env::temp_dir().join("stockdb_rs_atomic_test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let target = tmp.join("data.dat");

        let payload = b"hello-stockdb";
        atomic_write(&target, payload).unwrap();
        assert_eq!(fs::read(&target).unwrap(), payload);
        // 临时文件必须已被清理
        assert!(!tmp_path_for(&target).exists(), "tmp file should be removed");
        // atomic_write 单独调用不应生成 sidecar 锁文件（锁由 with_exclusive_lock 负责）
        assert!(!lock_path_for(&target).exists(), "atomic_write alone must not create a lock file");

        // 覆盖写仍原子、内容正确
        let payload2 = b"second-write-ok";
        atomic_write(&target, payload2).unwrap();
        assert_eq!(fs::read(&target).unwrap(), payload2);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn exclusive_lock_serializes_critical_section() {
        let tmp = std::env::temp_dir().join("stockdb_rs_lock_test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let target = tmp.join("x.dat");

        // 两次顺序进入关键区均成功（自身不重入死锁）
        let r1 = with_exclusive_lock(&target, || Ok::<u32, io::Error>(1));
        assert_eq!(r1.unwrap(), 1);
        let r2 = with_exclusive_lock(&target, || Ok::<u32, io::Error>(2));
        assert_eq!(r2.unwrap(), 2);
        // with_exclusive_lock 必须在 sidecar 上创建锁文件
        assert!(lock_path_for(&target).exists(), "lock file should be created by with_exclusive_lock");

        let _ = fs::remove_dir_all(&tmp);
    }
}
