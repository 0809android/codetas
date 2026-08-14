use super::*;
use std::collections::BTreeSet;

pub fn preview_observability_cleanup(
    directory: impl AsRef<Path>,
    now_ms: u64,
    retention_days: u16,
    max_storage_bytes: u64,
) -> ObservabilityCleanupPreview {
    let mut files = event_files(directory.as_ref()).unwrap_or_default();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let total_before = files.iter().map(|file| file.bytes).sum::<u64>();
    let current_day = now_ms / MILLIS_PER_DAY;
    let oldest_day = current_day.saturating_sub(u64::from(retention_days.saturating_sub(1)));
    let mut selected = BTreeSet::new();
    let mut remaining = total_before;
    for file in &files {
        if file.day < oldest_day && selected.insert(file.name.clone()) {
            remaining = remaining.saturating_sub(file.bytes);
        }
    }
    for file in &files {
        if remaining <= max_storage_bytes {
            break;
        }
        if selected.insert(file.name.clone()) {
            remaining = remaining.saturating_sub(file.bytes);
        }
    }
    let files = files
        .into_iter()
        .filter(|file| selected.contains(&file.name))
        .map(|file| ObservabilityStorageFile {
            name: file.name,
            bytes: file.bytes,
            day: file.day,
        })
        .collect::<Vec<_>>();
    ObservabilityCleanupPreview {
        generated_at_ms: now_ms,
        total_bytes_before: total_before,
        bytes_after: remaining,
        files,
    }
}

pub fn trash_observability_cleanup(
    directory: impl AsRef<Path>,
    preview: &ObservabilityCleanupPreview,
) -> Result<Option<ObservabilityTrashReport>, String> {
    if preview.files.is_empty() {
        return Ok(None);
    }
    let directory = directory.as_ref();
    let trash_root = directory.join(".trash");
    ensure_real_directory(&trash_root)?;
    let transaction_id = format!(
        "txn-{}-{}-{}",
        ObservationEvent::now_ms(),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let transaction = trash_root.join(&transaction_id);
    fs::create_dir(&transaction)
        .map_err(|error| format!("failed to create observability trash transaction: {error}"))?;
    secure_directory(&transaction)?;
    let manifest = ObservabilityTrashManifest {
        version: 1,
        transaction_id: transaction_id.clone(),
        created_at_ms: ObservationEvent::now_ms(),
        files: preview.files.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("failed to encode observability trash manifest: {error}"))?;
    write_new_secure_file(&transaction.join("manifest.json"), &manifest_bytes)?;

    let mut moved = Vec::<String>::new();
    for file in &preview.files {
        validate_event_file_name(&file.name)?;
        let source = directory.join(&file.name);
        let target = transaction.join(&file.name);
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| format!("observability cleanup source changed: {error}"))?;
        if !metadata.file_type().is_file() || metadata.len() != file.bytes {
            rollback_trash_moves(directory, &transaction, &moved);
            let _ = fs::remove_dir_all(&transaction);
            return Err("observability cleanup source changed after preview".into());
        }
        if let Err(error) = fs::rename(&source, &target) {
            rollback_trash_moves(directory, &transaction, &moved);
            let _ = fs::remove_dir_all(&transaction);
            return Err(format!(
                "failed to move observability segment to trash: {error}"
            ));
        }
        moved.push(file.name.clone());
    }
    Ok(Some(ObservabilityTrashReport {
        transaction_id,
        files: preview.files.len(),
        bytes: preview.files.iter().map(|file| file.bytes).sum(),
    }))
}

pub fn list_observability_trash(directory: impl AsRef<Path>) -> Vec<ObservabilityTrashEntry> {
    let trash_root = directory.as_ref().join(".trash");
    let Ok(entries) = fs::read_dir(trash_root) else {
        return Vec::new();
    };
    let mut values = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            let bytes = fs::read(entry.path().join("manifest.json")).ok()?;
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            let manifest = serde_json::from_slice::<ObservabilityTrashManifest>(&bytes).ok()?;
            validate_trash_manifest(&manifest).ok()?;
            Some(ObservabilityTrashEntry {
                transaction_id: manifest.transaction_id,
                created_at_ms: manifest.created_at_ms,
                files: manifest.files.len(),
                bytes: manifest.files.iter().map(|file| file.bytes).sum(),
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| right.created_at_ms.cmp(&left.created_at_ms));
    values
}

pub fn restore_observability_trash(
    directory: impl AsRef<Path>,
    transaction_id: &str,
) -> Result<ObservabilityTrashReport, String> {
    validate_transaction_id(transaction_id)?;
    let directory = directory.as_ref();
    let transaction = directory.join(".trash").join(transaction_id);
    let metadata = fs::symlink_metadata(&transaction)
        .map_err(|error| format!("observability trash transaction was not found: {error}"))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("observability trash transaction is not a real directory".into());
    }
    let manifest_path = transaction.join("manifest.json");
    let bytes = fs::read(&manifest_path)
        .map_err(|error| format!("failed to read observability trash manifest: {error}"))?;
    if bytes.len() > 1024 * 1024 {
        return Err("observability trash manifest exceeds 1 MiB".into());
    }
    let manifest: ObservabilityTrashManifest = serde_json::from_slice(&bytes)
        .map_err(|_| "observability trash manifest is invalid JSON".to_string())?;
    validate_trash_manifest(&manifest)?;
    if manifest.transaction_id != transaction_id {
        return Err("observability trash transaction identity mismatch".into());
    }
    for file in &manifest.files {
        if directory.join(&file.name).exists() {
            return Err(format!("restore target already exists: {}", file.name));
        }
        let metadata = fs::symlink_metadata(transaction.join(&file.name))
            .map_err(|error| format!("trashed observability segment is missing: {error}"))?;
        if !metadata.file_type().is_file() || metadata.len() != file.bytes {
            return Err("trashed observability segment changed after cleanup".into());
        }
    }
    let mut restored = Vec::<String>::new();
    for file in &manifest.files {
        if let Err(error) = fs::rename(transaction.join(&file.name), directory.join(&file.name)) {
            for name in restored.iter().rev() {
                let _ = fs::rename(directory.join(name), transaction.join(name));
            }
            return Err(format!("failed to restore observability segment: {error}"));
        }
        restored.push(file.name.clone());
    }
    fs::remove_file(manifest_path)
        .and_then(|_| fs::remove_dir(&transaction))
        .map_err(|error| {
            format!("observability data was restored but trash cleanup failed: {error}")
        })?;
    Ok(ObservabilityTrashReport {
        transaction_id: transaction_id.to_string(),
        files: manifest.files.len(),
        bytes: manifest.files.iter().map(|file| file.bytes).sum(),
    })
}

fn validate_transaction_id(value: &str) -> Result<(), String> {
    if value.len() < 8
        || value.len() > 96
        || !value.starts_with("txn-")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("invalid observability trash transaction id".into());
    }
    Ok(())
}

fn validate_event_file_name(value: &str) -> Result<(), String> {
    if value.len() > 96
        || !value.starts_with("events-")
        || !value.ends_with(".jsonl")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        return Err("invalid observability event file name".into());
    }
    Ok(())
}

pub(crate) fn validate_trash_manifest(manifest: &ObservabilityTrashManifest) -> Result<(), String> {
    if manifest.version != 1 || manifest.files.is_empty() || manifest.files.len() > 100_000 {
        return Err("unsupported observability trash manifest".into());
    }
    validate_transaction_id(&manifest.transaction_id)?;
    let mut names = BTreeSet::new();
    for file in &manifest.files {
        validate_event_file_name(&file.name)?;
        if !names.insert(file.name.as_str()) {
            return Err("observability trash manifest contains an invalid file row".into());
        }
    }
    Ok(())
}

pub(crate) fn ensure_real_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err("observability trash path is not a real directory".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| format!("failed to create observability trash: {error}"))?;
        }
        Err(error) => return Err(format!("failed to inspect observability trash: {error}")),
    }
    secure_directory(path)
}

pub(crate) fn write_new_secure_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    secure_mode(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| format!("failed to create observability trash manifest: {error}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("failed to write observability trash manifest: {error}"))
}

fn rollback_trash_moves(directory: &Path, transaction: &Path, names: &[String]) {
    for name in names.iter().rev() {
        let _ = fs::rename(transaction.join(name), directory.join(name));
    }
}
