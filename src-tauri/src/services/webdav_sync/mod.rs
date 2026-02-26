//! WebDAV 同步协议 v2
//!
//! 与上游 GUI 兼容的同步协议：远端路径 `{root}/v2/{profile}/`，
//! manifest 使用 BTreeMap 存储 artifacts，仅同步 db.sql + skills.zip。

mod archive;

use std::collections::BTreeMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use crate::database::Database;
use crate::error::AppError;
use crate::services::webdav;
use crate::settings::{
    get_webdav_sync_settings, update_webdav_sync_status, WebDavSyncSettings, WebDavSyncStatus,
};

use self::archive::{zip_skills_ssot, restore_skills_zip, SkillsBackup};

// ---------------------------------------------------------------------------
// i18n 辅助
// ---------------------------------------------------------------------------

fn localized(key: &'static str, zh: impl Into<String>, en: impl Into<String>) -> AppError {
    AppError::localized(key, zh, en)
}

fn io_context_localized(
    _key: &'static str,
    zh: impl Into<String>,
    en: impl Into<String>,
    source: std::io::Error,
) -> AppError {
    let zh_msg = zh.into();
    let en_msg = en.into();
    AppError::IoContext {
        context: format!("{zh_msg} ({en_msg})"),
        source,
    }
}

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

const PROTOCOL_FORMAT: &str = "cc-switch-webdav-sync";
const PROTOCOL_VERSION: u32 = 2;
const REMOTE_DB_SQL: &str = "db.sql";
const REMOTE_SKILLS_ZIP: &str = "skills.zip";
const REMOTE_MANIFEST: &str = "manifest.json";

const MAX_DEVICE_NAME_LEN: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024; // 1 MB
const MAX_SYNC_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024; // 512 MB

// ---------------------------------------------------------------------------
// 公共类型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDecision {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebDavSyncSummary {
    pub decision: SyncDecision,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Manifest 类型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncManifest {
    format: String,
    version: u32,
    device_name: String,
    created_at: String,
    artifacts: BTreeMap<String, ArtifactMeta>,
    snapshot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactMeta {
    sha256: String,
    size: u64,
}

// ---------------------------------------------------------------------------
// 本地快照
// ---------------------------------------------------------------------------

struct LocalSnapshot {
    db_sql: Vec<u8>,
    skills_zip: Vec<u8>,
    manifest_bytes: Vec<u8>,
    manifest_hash: String,
}

// ---------------------------------------------------------------------------
// 公共 API（同步包装）
// ---------------------------------------------------------------------------

pub struct WebDavSyncService;

impl WebDavSyncService {
    pub fn check_connection() -> Result<(), AppError> {
        run_http(check_connection())
    }

    pub fn upload() -> Result<WebDavSyncSummary, AppError> {
        run_http(upload())
    }

    pub fn download() -> Result<WebDavSyncSummary, AppError> {
        run_http(download())
    }
}

// ---------------------------------------------------------------------------
// 异步核心
// ---------------------------------------------------------------------------

async fn check_connection() -> Result<(), AppError> {
    let settings = load_webdav_settings()?;
    let auth = webdav::auth_from_credentials(&settings.username, &settings.password);
    webdav::test_connection(&settings.base_url, &auth).await?;
    let dir_segments = remote_dir_segments(&settings);
    webdav::ensure_remote_directories(&settings.base_url, &dir_segments, &auth).await?;
    Ok(())
}

async fn upload() -> Result<WebDavSyncSummary, AppError> {
    let mut settings = load_webdav_settings()?;
    let auth = webdav::auth_from_credentials(&settings.username, &settings.password);

    let dir_segments = remote_dir_segments(&settings);
    webdav::ensure_remote_directories(&settings.base_url, &dir_segments, &auth).await?;

    let snapshot = build_local_snapshot(&settings)?;

    // 上传 artifacts
    let db_url = build_artifact_url(&settings, REMOTE_DB_SQL)?;
    webdav::put_bytes(&db_url, &auth, snapshot.db_sql, "application/sql").await?;

    let skills_url = build_artifact_url(&settings, REMOTE_SKILLS_ZIP)?;
    webdav::put_bytes(&skills_url, &auth, snapshot.skills_zip, "application/zip").await?;

    // 上传 manifest（最后上传，确保 artifacts 已就绪）
    let manifest_url = build_artifact_url(&settings, REMOTE_MANIFEST)?;
    webdav::put_bytes(
        &manifest_url,
        &auth,
        snapshot.manifest_bytes,
        "application/json",
    )
    .await?;

    // 获取 etag（best-effort，不影响上传结果）
    let etag = match webdav::head_etag(&manifest_url, &auth).await {
        Ok(e) => e,
        Err(e) => {
            log::debug!("[WebDAV] Failed to fetch ETag after upload: {e}");
            None
        }
    };

    persist_sync_success_best_effort(&mut settings, &snapshot.manifest_hash, etag);

    Ok(WebDavSyncSummary {
        decision: SyncDecision::Upload,
        message: "WebDAV upload completed".to_string(),
    })
}

async fn download() -> Result<WebDavSyncSummary, AppError> {
    let mut settings = load_webdav_settings()?;
    let auth = webdav::auth_from_credentials(&settings.username, &settings.password);

    // 下载 manifest
    let manifest_url = build_artifact_url(&settings, REMOTE_MANIFEST)?;
    let (manifest_bytes, etag) = webdav::get_bytes(&manifest_url, &auth, Some(MAX_MANIFEST_BYTES))
        .await?
        .ok_or_else(|| localized(
            "webdav.sync.remote_empty",
            "远端没有可下载的同步数据",
            "No downloadable sync data found on the remote",
        ))?;

    let manifest: SyncManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| AppError::Json {
            path: REMOTE_MANIFEST.to_string(),
            source: e,
        })?;
    validate_manifest_compat(&manifest)?;

    let manifest_hash = sha256_hex(&manifest_bytes);

    // 下载 artifacts
    let db_sql = download_and_verify(&settings, &auth, REMOTE_DB_SQL, &manifest.artifacts).await?;
    let skills_zip =
        download_and_verify(&settings, &auth, REMOTE_SKILLS_ZIP, &manifest.artifacts).await?;

    // 应用快照（带 skills 备份回滚）
    apply_snapshot(&db_sql, &skills_zip)?;

    persist_sync_success_best_effort(&mut settings, &manifest_hash, etag);

    Ok(WebDavSyncSummary {
        decision: SyncDecision::Download,
        message: "WebDAV download completed".to_string(),
    })
}

// ---------------------------------------------------------------------------
// 设置加载 / 验证
// ---------------------------------------------------------------------------

fn load_webdav_settings() -> Result<WebDavSyncSettings, AppError> {
    let settings = get_webdav_sync_settings()
        .ok_or_else(|| localized(
            "webdav.sync.not_configured",
            "未配置 WebDAV 同步",
            "WebDAV sync is not configured",
        ))?;
    if !settings.enabled {
        return Err(localized(
            "webdav.sync.not_enabled",
            "WebDAV 同步未启用",
            "WebDAV sync is not enabled",
        ));
    }
    settings.validate()?;
    Ok(settings)
}

// ---------------------------------------------------------------------------
// 远端路径
// ---------------------------------------------------------------------------

fn remote_dir_segments(settings: &WebDavSyncSettings) -> Vec<String> {
    let mut segments = Vec::new();
    segments.extend(webdav::path_segments(&settings.remote_root).map(str::to_string));
    segments.push(format!("v{PROTOCOL_VERSION}"));
    segments.extend(webdav::path_segments(&settings.profile).map(str::to_string));
    segments
}

fn build_artifact_url(settings: &WebDavSyncSettings, file_name: &str) -> Result<String, AppError> {
    let mut segments = remote_dir_segments(settings);
    segments.extend(webdav::path_segments(file_name).map(str::to_string));
    webdav::build_remote_url(&settings.base_url, &segments)
}

// ---------------------------------------------------------------------------
// 本地快照构建
// ---------------------------------------------------------------------------

fn build_local_snapshot(_settings: &WebDavSyncSettings) -> Result<LocalSnapshot, AppError> {
    let tmp = tempdir().map_err(|e| io_context_localized(
        "webdav.sync.snapshot_tmpdir_failed",
        "创建 WebDAV 快照临时目录失败",
        "Failed to create temporary directory for WebDAV snapshot",
        e,
    ))?;

    // 导出 DB
    let db_sql = Database::init()?.export_sql_string()?.into_bytes();

    // 打包 skills
    let skills_zip_path = tmp.path().join(REMOTE_SKILLS_ZIP);
    zip_skills_ssot(&skills_zip_path)?;
    let skills_zip =
        std::fs::read(&skills_zip_path).map_err(|e| AppError::io(&skills_zip_path, e))?;

    // 构建 artifacts map
    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        REMOTE_DB_SQL.to_string(),
        ArtifactMeta {
            sha256: sha256_hex(&db_sql),
            size: db_sql.len() as u64,
        },
    );
    artifacts.insert(
        REMOTE_SKILLS_ZIP.to_string(),
        ArtifactMeta {
            sha256: sha256_hex(&skills_zip),
            size: skills_zip.len() as u64,
        },
    );

    let snapshot_id = compute_snapshot_id(&artifacts);
    let device_name = detect_system_device_name().unwrap_or_else(|| "Unknown Device".to_string());

    let manifest = SyncManifest {
        format: PROTOCOL_FORMAT.to_string(),
        version: PROTOCOL_VERSION,
        device_name,
        created_at: Utc::now().to_rfc3339(),
        artifacts,
        snapshot_id,
    };

    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| AppError::JsonSerialize { source: e })?;
    let manifest_hash = sha256_hex(&manifest_bytes);

    Ok(LocalSnapshot {
        db_sql,
        skills_zip,
        manifest_bytes,
        manifest_hash,
    })
}

// ---------------------------------------------------------------------------
// Manifest 验证
// ---------------------------------------------------------------------------

fn validate_manifest_compat(manifest: &SyncManifest) -> Result<(), AppError> {
    if manifest.format != PROTOCOL_FORMAT {
        return Err(localized(
            "webdav.sync.manifest_format_incompatible",
            format!("远端 manifest 格式不兼容: {}", manifest.format),
            format!("Remote manifest format is incompatible: {}", manifest.format),
        ));
    }
    if manifest.version != PROTOCOL_VERSION {
        return Err(localized(
            "webdav.sync.manifest_version_incompatible",
            format!(
                "远端 manifest 协议版本不兼容: v{} (本地 v{PROTOCOL_VERSION})",
                manifest.version
            ),
            format!(
                "Remote manifest protocol version is incompatible: v{} (local v{PROTOCOL_VERSION})",
                manifest.version
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Artifact 下载 + 校验
// ---------------------------------------------------------------------------

async fn download_and_verify(
    settings: &WebDavSyncSettings,
    auth: &webdav::WebDavAuth,
    artifact_name: &str,
    artifacts: &BTreeMap<String, ArtifactMeta>,
) -> Result<Vec<u8>, AppError> {
    let meta = artifacts.get(artifact_name).ok_or_else(|| {
        localized(
            "webdav.sync.manifest_missing_artifact",
            format!("manifest 中缺少 artifact: {artifact_name}"),
            format!("Manifest missing artifact: {artifact_name}"),
        )
    })?;

    validate_artifact_size_limit(artifact_name, meta.size)?;

    let url = build_artifact_url(settings, artifact_name)?;
    let (bytes, _) = webdav::get_bytes(&url, auth, Some(MAX_SYNC_ARTIFACT_BYTES))
        .await?
        .ok_or_else(|| localized(
            "webdav.sync.remote_missing_artifact",
            format!("远端缺少 artifact 文件: {artifact_name}"),
            format!("Remote artifact file missing: {artifact_name}"),
        ))?;

    // 先检查大小（快速），再检查 hash（昂贵）
    if bytes.len() as u64 != meta.size {
        return Err(localized(
            "webdav.sync.artifact_size_mismatch",
            format!(
                "artifact {artifact_name} 大小不匹配 (expected: {}, got: {})",
                meta.size,
                bytes.len(),
            ),
            format!(
                "Artifact {artifact_name} size mismatch (expected: {}, got: {})",
                meta.size,
                bytes.len(),
            ),
        ));
    }

    let actual_hash = sha256_hex(&bytes);
    if actual_hash != meta.sha256 {
        return Err(localized(
            "webdav.sync.artifact_hash_mismatch",
            format!(
                "artifact {artifact_name} SHA256 校验失败 (expected: {}..., got: {}...)",
                meta.sha256.get(..8).unwrap_or(&meta.sha256),
                actual_hash.get(..8).unwrap_or(&actual_hash),
            ),
            format!(
                "Artifact {artifact_name} SHA256 verification failed (expected: {}..., got: {}...)",
                meta.sha256.get(..8).unwrap_or(&meta.sha256),
                actual_hash.get(..8).unwrap_or(&actual_hash),
            ),
        ));
    }

    Ok(bytes)
}

fn validate_artifact_size_limit(name: &str, size: u64) -> Result<(), AppError> {
    if size > MAX_SYNC_ARTIFACT_BYTES {
        let max_mb = MAX_SYNC_ARTIFACT_BYTES / 1024 / 1024;
        return Err(localized(
            "webdav.sync.artifact_too_large",
            format!("artifact {name} 超过下载上限（{max_mb} MB）"),
            format!("Artifact {name} exceeds download limit ({max_mb} MB)"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 快照应用（带 skills 备份回滚）
// ---------------------------------------------------------------------------

fn apply_snapshot(db_sql: &[u8], skills_zip: &[u8]) -> Result<(), AppError> {
    let sql_str = std::str::from_utf8(db_sql)
        .map_err(|e| localized(
            "webdav.sync.sql_not_utf8",
            format!("SQL 非 UTF-8: {e}"),
            format!("SQL is not valid UTF-8: {e}"),
        ))?;

    let skills_backup = SkillsBackup::backup_current_skills()?;

    // 先替换 skills，再导入数据库；若导入失败则回滚 skills，避免"半恢复"。
    restore_skills_zip(skills_zip)?;

    if let Err(db_err) = Database::init()?.import_sql_string(sql_str) {
        if let Err(rollback_err) = skills_backup.restore() {
            return Err(localized(
                "webdav.sync.db_import_and_rollback_failed",
                format!("导入数据库失败: {db_err}; 同时回滚 Skills 失败: {rollback_err}"),
                format!("Database import failed: {db_err}; skills rollback also failed: {rollback_err}"),
            ));
        }
        return Err(db_err);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 同步状态持久化
// ---------------------------------------------------------------------------

fn persist_sync_success(
    settings: &mut WebDavSyncSettings,
    manifest_hash: &str,
    etag: Option<String>,
) -> Result<(), AppError> {
    let status = WebDavSyncStatus {
        last_sync_at: Some(Utc::now().timestamp()),
        last_error: None,
        last_error_source: None,
        last_remote_etag: etag,
        last_local_manifest_hash: Some(manifest_hash.to_string()),
        last_remote_manifest_hash: Some(manifest_hash.to_string()),
    };
    settings.status = status.clone();
    update_webdav_sync_status(status)
}

/// 尽力持久化同步状态，失败时仅记录日志
fn persist_sync_success_best_effort(
    settings: &mut WebDavSyncSettings,
    manifest_hash: &str,
    etag: Option<String>,
) -> bool {
    match persist_sync_success(settings, manifest_hash, etag) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("持久化同步状态失败（非致命）: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot ID 计算
// ---------------------------------------------------------------------------

fn compute_snapshot_id(artifacts: &BTreeMap<String, ArtifactMeta>) -> String {
    let combined: String = artifacts
        .iter()
        .map(|(name, meta)| format!("{name}:{}", meta.sha256))
        .collect::<Vec<_>>()
        .join("|");
    sha256_hex(combined.as_bytes())
}

// ---------------------------------------------------------------------------
// 设备名检测
// ---------------------------------------------------------------------------

fn detect_system_device_name() -> Option<String> {
    let env_name = ["CC_SWITCH_DEVICE_NAME", "COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| normalize_device_name(&value));

    if env_name.is_some() {
        return env_name;
    }

    let output = std::process::Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let hostname = String::from_utf8(output.stdout).ok()?;
    normalize_device_name(&hostname)
}

fn normalize_device_name(raw: &str) -> Option<String> {
    let compact = raw
        .chars()
        .fold(String::with_capacity(raw.len()), |mut acc, ch| {
            if ch.is_whitespace() {
                acc.push(' ');
            } else if !ch.is_control() {
                acc.push(ch);
            }
            acc
        });
    let normalized = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return None;
    }

    let limited = trimmed
        .chars()
        .take(MAX_DEVICE_NAME_LEN)
        .collect::<String>();
    if limited.is_empty() {
        None
    } else {
        Some(limited)
    }
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    format!("{hash:x}")
}

fn run_http<F, T>(future: F) -> Result<T, AppError>
where
    F: std::future::Future<Output = Result<T, AppError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| localized(
            "webdav.sync.runtime_create_failed",
            format!("创建异步运行时失败: {e}"),
            format!("Failed to create async runtime: {e}"),
        ))?;
    runtime.block_on(future)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_settings() -> WebDavSyncSettings {
        WebDavSyncSettings {
            enabled: true,
            base_url: "https://dav.example.com/remote.php/dav/files/demo/".to_string(),
            remote_root: "cc switch-sync/team a".to_string(),
            profile: "default profile".to_string(),
            username: "demo".to_string(),
            password: "secret".to_string(),
            auto_sync: false,
            status: WebDavSyncStatus::default(),
        }
    }

    #[test]
    fn remote_dir_segments_uses_v2() {
        let mut settings = sample_settings();
        settings.normalize();
        let segments = remote_dir_segments(&settings);
        assert!(
            segments.contains(&"v2".to_string()),
            "segments should contain v2: {segments:?}"
        );
        assert!(
            !segments.contains(&"v1".to_string()),
            "segments should not contain v1: {segments:?}"
        );
    }

    #[test]
    fn build_artifact_url_encodes_path_segments() {
        let mut settings = sample_settings();
        settings.normalize();
        let url = build_artifact_url(&settings, REMOTE_MANIFEST).expect("build artifact url");
        assert_eq!(
            url,
            "https://dav.example.com/remote.php/dav/files/demo/cc%20switch-sync/team%20a/v2/default%20profile/manifest.json"
        );
        assert!(
            !url.contains("//cc"),
            "url should not contain duplicated slash: {url}"
        );
    }

    #[test]
    fn snapshot_id_is_stable() {
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "db.sql".to_string(),
            ArtifactMeta {
                sha256: "aaa".to_string(),
                size: 1,
            },
        );
        artifacts.insert(
            "skills.zip".to_string(),
            ArtifactMeta {
                sha256: "bbb".to_string(),
                size: 2,
            },
        );
        let id1 = compute_snapshot_id(&artifacts);
        let id2 = compute_snapshot_id(&artifacts);
        assert_eq!(id1, id2);
    }

    #[test]
    fn snapshot_id_changes_with_artifacts() {
        let mut artifacts_a = BTreeMap::new();
        artifacts_a.insert(
            "db.sql".to_string(),
            ArtifactMeta {
                sha256: "aaa".to_string(),
                size: 1,
            },
        );
        artifacts_a.insert(
            "skills.zip".to_string(),
            ArtifactMeta {
                sha256: "bbb".to_string(),
                size: 2,
            },
        );

        let mut artifacts_b = artifacts_a.clone();
        artifacts_b.get_mut("db.sql").unwrap().sha256 = "ccc".to_string();

        assert_ne!(
            compute_snapshot_id(&artifacts_a),
            compute_snapshot_id(&artifacts_b)
        );
    }

    #[test]
    fn validate_manifest_compat_ok() {
        let manifest = SyncManifest {
            format: PROTOCOL_FORMAT.to_string(),
            version: PROTOCOL_VERSION,
            device_name: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            artifacts: BTreeMap::new(),
            snapshot_id: "id".to_string(),
        };
        assert!(validate_manifest_compat(&manifest).is_ok());
    }

    #[test]
    fn validate_manifest_compat_wrong_format() {
        let manifest = SyncManifest {
            format: "wrong-format".to_string(),
            version: PROTOCOL_VERSION,
            device_name: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            artifacts: BTreeMap::new(),
            snapshot_id: "id".to_string(),
        };
        assert!(validate_manifest_compat(&manifest).is_err());
    }

    #[test]
    fn validate_manifest_compat_wrong_version() {
        let manifest = SyncManifest {
            format: PROTOCOL_FORMAT.to_string(),
            version: 999,
            device_name: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            artifacts: BTreeMap::new(),
            snapshot_id: "id".to_string(),
        };
        assert!(validate_manifest_compat(&manifest).is_err());
    }

    #[test]
    fn validate_artifact_size_limit_ok() {
        assert!(validate_artifact_size_limit("db.sql", 1024).is_ok());
    }

    #[test]
    fn validate_artifact_size_limit_exceeded() {
        assert!(validate_artifact_size_limit("db.sql", MAX_SYNC_ARTIFACT_BYTES + 1).is_err());
    }

    #[test]
    fn normalize_device_name_trims() {
        assert_eq!(
            normalize_device_name("  my-host  "),
            Some("my-host".to_string())
        );
    }

    #[test]
    fn normalize_device_name_empty() {
        assert_eq!(normalize_device_name(""), None);
        assert_eq!(normalize_device_name("   "), None);
    }

    #[test]
    fn normalize_device_name_truncates() {
        let long = "a".repeat(100);
        let result = normalize_device_name(&long).unwrap();
        assert_eq!(result.chars().count(), MAX_DEVICE_NAME_LEN);
    }

    #[test]
    fn normalize_device_name_collapses_whitespace() {
        assert_eq!(
            normalize_device_name("  Mac  Book  Pro  "),
            Some("Mac Book Pro".to_string())
        );
    }

    #[test]
    fn normalize_device_name_truncates_by_chars_not_bytes() {
        // 中文字符每个 3 bytes，80 个中文 = 240 bytes
        let long_cn = "测".repeat(80);
        let result = normalize_device_name(&long_cn).unwrap();
        assert_eq!(result.chars().count(), MAX_DEVICE_NAME_LEN);
    }

    #[test]
    fn detect_system_device_name_returns_some() {
        // 在 CI/本地环境中应该总能获取到设备名
        let name = detect_system_device_name();
        assert!(name.is_some(), "should detect a device name");
    }

    #[test]
    fn manifest_serialization_uses_device_name_only() {
        let manifest = SyncManifest {
            format: PROTOCOL_FORMAT.to_string(),
            version: PROTOCOL_VERSION,
            device_name: "My MacBook".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            artifacts: BTreeMap::new(),
            snapshot_id: "snap-1".to_string(),
        };
        let value = serde_json::to_value(&manifest).expect("serialize manifest");
        assert!(
            value.get("deviceName").is_some(),
            "manifest should contain deviceName"
        );
        assert!(
            value.get("deviceId").is_none(),
            "manifest should not contain deviceId"
        );
    }
}
