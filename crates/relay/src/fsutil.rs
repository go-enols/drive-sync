//! 文件落盘工具（中继持久化共用）。

use std::path::Path;

use drive_core::util::random_hex;

/// 原子写：同目录临时文件 + rename。
///
/// Windows 上 `rename` 不能覆盖已存在文件，退化为「删除目标后重命名」
/// （窗口极小，原型可接受；崩溃时最多留下孤儿临时文件，由启动清理回收）。
pub(crate) async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("目标路径没有父目录"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("object");
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}", random_hex(4)));
    tokio::fs::write(&tmp_path, data).await?;
    match tokio::fs::rename(&tmp_path, path).await {
        Ok(()) => Ok(()),
        Err(first_error) => {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(first_error);
                }
            }
            match tokio::fs::rename(&tmp_path, path).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    Err(error)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_atomic_creates_and_overwrites() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("index.json");
        write_atomic(&path, b"first").await.expect("首次写入");
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"first");
        write_atomic(&path, b"second").await.expect("覆盖写入");
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"second");
        // 不应残留临时文件。
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.contains(".tmp-"), "残留临时文件: {name}");
        }
    }
}
