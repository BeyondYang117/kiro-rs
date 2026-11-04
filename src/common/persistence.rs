/// 在原文件同目录写入并同步临时文件，再原子替换；失败时保留原文件。
pub(crate) fn atomic_write(path: &std::path::Path, json: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    // 保留符号链接，替换其目标文件。
    let target = if path.exists() {
        fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let filename = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("路径不是文件"))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        filename.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        if let Ok(metadata) = fs::metadata(&target) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(json)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &target)?;
        #[cfg(unix)]
        if let Err(error) = fs::File::open(parent).and_then(|directory| directory.sync_all()) {
            tracing::warn!("文件已替换，但目录同步失败: {error}");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("原子写入文件失败: {}", path.display()))
}
