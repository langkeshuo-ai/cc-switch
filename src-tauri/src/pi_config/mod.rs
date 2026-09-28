//! Thin adapter for Pi's native files.
//!
//! Pi owns account login and the active provider/model in `settings.json`.
//! CC Switch only manages explicit provider entries in `models.json`.

use crate::config::{atomic_write_private, get_home_dir};
use crate::error::AppError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

const MAX_PI_FILE_BYTES: u64 = 1024 * 1024;
const MISSING_MODELS_REVISION: &str = "missing";
static MODELS_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(test)]
static TEST_AGENT_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiNativeDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
}

pub(crate) fn get_pi_agent_dir() -> Result<PathBuf, AppError> {
    #[cfg(test)]
    if let Some(path) = TEST_AGENT_DIR
        .lock()
        .expect("lock Pi test directory")
        .clone()
    {
        return resolve_pi_agent_dir(Some(path), None, get_home_dir().join(".pi").join("agent"));
    }

    resolve_pi_agent_dir(
        crate::settings::get_pi_override_dir(),
        std::env::var_os("PI_CODING_AGENT_DIR"),
        get_home_dir().join(".pi").join("agent"),
    )
}

fn resolve_pi_agent_dir(
    settings_override: Option<PathBuf>,
    env_override: Option<std::ffi::OsString>,
    default_path: PathBuf,
) -> Result<PathBuf, AppError> {
    let (path, source) = match settings_override {
        Some(path) => (path, "Pi settings override"),
        None => match env_override {
            Some(value) if !value.is_empty() => (
                crate::settings::resolve_override_path(value.to_string_lossy().as_ref()),
                "PI_CODING_AGENT_DIR",
            ),
            _ => (default_path, "Pi default"),
        },
    };
    if !path.is_absolute() {
        return Err(AppError::InvalidInput(format!(
            "{source} must resolve to an absolute directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

pub(crate) fn get_pi_models_path() -> Result<PathBuf, AppError> {
    Ok(get_pi_agent_dir()?.join("models.json"))
}

pub(crate) fn get_pi_settings_path() -> Result<PathBuf, AppError> {
    Ok(get_pi_agent_dir()?.join("settings.json"))
}

pub(crate) fn read_pi_native_defaults() -> Result<PiNativeDefaults, AppError> {
    let path = get_pi_settings_path()?;
    if !path.exists() {
        return Ok(PiNativeDefaults::default());
    }
    let value = read_json5_value(&path, "Pi settings")?;
    let object = value.as_object().ok_or_else(|| {
        AppError::Config(format!(
            "Pi settings root must be an object: {}",
            path.display()
        ))
    })?;
    Ok(PiNativeDefaults {
        default_provider: optional_string(object, "defaultProvider", &path)?,
        default_model: optional_string(object, "defaultModel", &path)?,
        session_dir: optional_string(object, "sessionDir", &path)?,
    })
}

pub(crate) fn read_pi_native_providers() -> Result<IndexMap<String, Value>, AppError> {
    let _guard = lock_models_file()?;
    read_pi_native_providers_locked(&get_pi_models_path()?)
}

/// Pi 的"实际生效供应商"标识：settings.json 的 `defaultProvider`。
///
/// 这是 Pi CLI 唯一跟随的来源，与 CC Switch 数据库的 current provider 可能
/// 不一致。代理转发路径必须以它为准，否则会把 Pi 的流量转发到错误上游。
pub(crate) fn pi_proxy_current_provider_key() -> Option<String> {
    read_pi_native_defaults()
        .ok()
        .and_then(|defaults| defaults.default_provider)
        .filter(|key| !key.trim().is_empty())
}

/// Read the entire raw `models.json` bytes (None = file does not exist).
///
/// Used by proxy takeover to back up the original file verbatim so that a
/// later disable can restore it byte-for-byte.
pub(crate) fn read_models_document_raw() -> Result<Option<Vec<u8>>, AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_file_limited(&path, "Pi models")?))
}

/// Restore the entire `models.json` from raw bytes (atomic write, serialized
/// with every other models.json mutation via `MODELS_FILE_LOCK`).
pub(crate) fn restore_models_document_raw(raw: &[u8]) -> Result<(), AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    ensure_private_models_parent(&path)?;
    atomic_write_private(&path, raw)
}

/// Gateway route prefix for a Pi provider's `api` dialect.
///
/// Proxy takeover rewrites the provider's `baseUrl` to the local gateway;
/// the gateway then forwards transparently (Pi's request dialect equals the
/// upstream dialect, so no body transformation is needed). Dialects the
/// gateway cannot speak return `None` and takeover must be rejected.
pub(crate) fn pi_takeover_prefix_for_api(api: &str) -> Option<&'static str> {
    match api.trim() {
        "anthropic-messages" => Some("pi/anthropic"),
        "openai-completions" | "openai-responses" => Some("pi/openai"),
        _ => None,
    }
}

/// Point one provider's live `baseUrl` at the local gateway.
///
/// Only `baseUrl` is touched; every other field (including `apiKey`, which Pi
/// stores natively in `models.json`) is preserved verbatim. Fails when the
/// provider's `api` dialect has no gateway route.
pub(crate) fn apply_pi_takeover_base_url(
    provider_key: &str,
    proxy_base_url: &str,
) -> Result<(), AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    {
        let providers = providers_mut(&mut document, &path)?;
        let node = providers.get_mut(provider_key).ok_or_else(|| {
            AppError::Config(format!(
                "Pi provider '{provider_key}' is not present in models.json"
            ))
        })?;
        let api = node
            .get("api")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let prefix = pi_takeover_prefix_for_api(api).ok_or_else(|| {
            AppError::InvalidInput(format!(
                "Pi provider '{provider_key}' 的 API 格式 '{api}' 不支持本地代理接管（仅支持 anthropic-messages / openai-completions / openai-responses）"
            ))
        })?;
        let proxy_base = proxy_base_url.trim().trim_end_matches('/');
        let rewritten = format!("{proxy_base}/{prefix}");
        node.as_object_mut().ok_or_else(|| {
            AppError::Config(format!("Pi provider '{provider_key}' must be an object"))
        })?
        .insert("baseUrl".to_string(), Value::String(rewritten));
    }
    write_models_document(&path, &document, &expected_revision)
}

/// Overwrite one provider's live `baseUrl` unconditionally.
///
/// Restore/cleanup path companion of [`apply_pi_takeover_base_url`]: no dialect
/// validation, used to put back the original upstream URL from the DB profile.
/// Missing key is an error (nothing to restore onto).
pub(crate) fn set_pi_provider_base_url(
    provider_key: &str,
    base_url: &str,
) -> Result<(), AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    {
        let providers = providers_mut(&mut document, &path)?;
        let node = providers.get_mut(provider_key).ok_or_else(|| {
            AppError::Config(format!(
                "Pi provider '{provider_key}' is not present in models.json"
            ))
        })?;
        node.as_object_mut().ok_or_else(|| {
            AppError::Config(format!("Pi provider '{provider_key}' must be an object"))
        })?
        .insert(
            "baseUrl".to_string(),
            Value::String(base_url.trim().trim_end_matches('/').to_string()),
        );
    }
    write_models_document(&path, &document, &expected_revision)
}

pub(crate) fn read_pi_native_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let document = read_models_document(&path)?;
    Ok(providers(&document, &path)?.get(provider_key).cloned())
}

pub(crate) fn pi_provider_exists(provider_key: &str) -> Result<bool, AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let document = read_models_document(&path)?;
    Ok(providers(&document, &path)?.contains_key(provider_key))
}

pub(crate) fn insert_pi_provider(provider_key: &str, config: &Value) -> Result<bool, AppError> {
    validate_provider_node(provider_key, config)?;
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;

    match providers.get(provider_key) {
        Some(current) if current == config => return Ok(false),
        Some(_) => {
            return Err(AppError::InvalidInput(format!(
                "Pi provider key '{provider_key}' already exists in models.json"
            )))
        }
        None => {}
    }

    providers.insert(provider_key.to_string(), config.clone());
    write_models_document(&path, &document, &expected_revision)?;
    Ok(true)
}

pub(crate) fn replace_pi_provider(
    provider_key: &str,
    expected: &Value,
    replacement: &Value,
) -> Result<(), AppError> {
    validate_provider_node(provider_key, replacement)?;
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let current = providers.get(provider_key).ok_or_else(|| {
        AppError::Conflict(format!(
            "Pi provider '{provider_key}' is no longer present in models.json"
        ))
    })?;
    if current != expected {
        return Err(AppError::Conflict(format!(
            "Pi provider '{provider_key}' changed outside CC Switch"
        )));
    }
    if current == replacement {
        return Ok(());
    }
    providers.insert(provider_key.to_string(), replacement.clone());
    write_models_document(&path, &document, &expected_revision)
}

pub(crate) fn replace_pi_provider_if_present(
    provider_key: &str,
    replacement: &Value,
) -> Result<Option<Value>, AppError> {
    validate_provider_node(provider_key, replacement)?;
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let Some(current) = providers.get(provider_key).cloned() else {
        return Ok(None);
    };
    if current == *replacement {
        return Ok(Some(current));
    }
    providers.insert(provider_key.to_string(), replacement.clone());
    write_models_document(&path, &document, &expected_revision)?;
    Ok(Some(current))
}

pub(crate) fn remove_pi_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    remove_pi_provider_inner(provider_key, None)
}

pub(crate) fn remove_pi_provider_if_matches(
    provider_key: &str,
    expected: &Value,
) -> Result<bool, AppError> {
    remove_pi_provider_inner(provider_key, Some(expected)).map(|removed| removed.is_some())
}

fn remove_pi_provider_inner(
    provider_key: &str,
    expected: Option<&Value>,
) -> Result<Option<Value>, AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let Some(current) = providers.get(provider_key).cloned() else {
        return Ok(None);
    };
    if expected.is_some_and(|expected| current != *expected) {
        return Err(AppError::Conflict(format!(
            "Pi provider '{provider_key}' changed outside CC Switch"
        )));
    }
    providers.remove(provider_key);
    write_models_document(&path, &document, &expected_revision)?;
    Ok(Some(current))
}

pub(crate) fn restore_pi_provider_if_missing(
    provider_key: &str,
    config: &Value,
) -> Result<(), AppError> {
    let _guard = lock_models_file()?;
    let path = get_pi_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    match providers.get(provider_key) {
        Some(current) if current == config => Ok(()),
        Some(_) => Err(AppError::Conflict(format!(
            "cannot restore Pi provider '{provider_key}' because another value now owns the key"
        ))),
        None => {
            providers.insert(provider_key.to_string(), config.clone());
            write_models_document(&path, &document, &expected_revision)
        }
    }
}

/// Validate the shape CC Switch can persist as one
/// `models.json.providers.<provider_key>` node.
///
/// Provider ownership is intentionally source-based: every explicit object in
/// `models.json.providers` is manageable, including keys also built into Pi.
/// Pi's `/login` credentials live in `auth.json` and are never read here.
pub(crate) fn validate_provider_node(provider_key: &str, config: &Value) -> Result<(), AppError> {
    if provider_key.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "Pi provider key cannot be empty".to_string(),
        ));
    }
    config.as_object().ok_or_else(|| {
        AppError::InvalidInput("Pi provider configuration must be an object".to_string())
    })?;
    Ok(())
}

pub(crate) fn provider_base_url(config: &Value) -> Result<String, AppError> {
    let provider = config.as_object().ok_or_else(|| {
        AppError::InvalidInput("Pi provider configuration must be an object".to_string())
    })?;
    nonempty_string(provider.get("baseUrl"))
        .or_else(|| {
            provider
                .get("models")
                .and_then(Value::as_array)
                .and_then(|models| {
                    models
                        .iter()
                        .find_map(|model| nonempty_string(model.get("baseUrl")))
                })
        })
        .map(str::to_string)
        .ok_or_else(|| AppError::InvalidInput("Pi provider has no request URL".to_string()))
}

fn lock_models_file() -> Result<MutexGuard<'static, ()>, AppError> {
    MODELS_FILE_LOCK
        .lock()
        .map_err(|error| AppError::Config(format!("Pi models file lock is poisoned: {error}")))
}

fn read_pi_native_providers_locked(path: &Path) -> Result<IndexMap<String, Value>, AppError> {
    let document = read_models_document(path)?;
    let providers = providers(&document, path)?;
    Ok(providers
        .iter()
        .map(|(provider_key, config)| (provider_key.clone(), config.clone()))
        .collect())
}

fn read_models_document(path: &Path) -> Result<Value, AppError> {
    read_models_document_with_revision(path).map(|(document, _)| document)
}

fn read_models_document_with_revision(path: &Path) -> Result<(Value, String), AppError> {
    if !path.exists() {
        return Ok((
            Value::Object(Map::new()),
            MISSING_MODELS_REVISION.to_string(),
        ));
    }
    let bytes = read_file_limited(path, "Pi models")?;
    let revision = revision(&bytes);
    let document = parse_json5_value(path, "Pi models", bytes)?;
    Ok((document, revision))
}

fn read_json5_value(path: &Path, label: &str) -> Result<Value, AppError> {
    parse_json5_value(path, label, read_file_limited(path, label)?)
}

fn read_file_limited(path: &Path, label: &str) -> Result<Vec<u8>, AppError> {
    let file = fs::File::open(path).map_err(|error| AppError::io(path, error))?;
    let metadata = file.metadata().map_err(|error| AppError::io(path, error))?;
    if metadata.len() > MAX_PI_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "{label} file exceeds the 1 MiB limit: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PI_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::io(path, error))?;
    if bytes.len() as u64 > MAX_PI_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "{label} file exceeds the 1 MiB limit: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn parse_json5_value(path: &Path, label: &str, bytes: Vec<u8>) -> Result<Value, AppError> {
    let source = String::from_utf8(bytes).map_err(|error| {
        AppError::Config(format!(
            "{label} file must be UTF-8 ({}): {error}",
            path.display()
        ))
    })?;
    json5::from_str(&source).map_err(|error| {
        AppError::Config(format!(
            "{label} file is not valid JSON/JSONC ({}): {error}",
            path.display()
        ))
    })
}

fn providers<'a>(document: &'a Value, path: &Path) -> Result<&'a Map<String, Value>, AppError> {
    let root = document.as_object().ok_or_else(|| {
        AppError::Config(format!(
            "Pi models root must be an object: {}",
            path.display()
        ))
    })?;
    match root.get("providers") {
        None => Ok(empty_json_object()),
        Some(Value::Object(providers)) => Ok(providers),
        Some(_) => Err(AppError::Config(format!(
            "Pi models 'providers' must be an object: {}",
            path.display()
        ))),
    }
}

fn providers_mut<'a>(
    document: &'a mut Value,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, AppError> {
    let root = document.as_object_mut().ok_or_else(|| {
        AppError::Config(format!(
            "Pi models root must be an object: {}",
            path.display()
        ))
    })?;
    let value = root
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    value.as_object_mut().ok_or_else(|| {
        AppError::Config(format!(
            "Pi models 'providers' must be an object: {}",
            path.display()
        ))
    })
}

fn empty_json_object() -> &'static Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    &EMPTY
}

fn write_models_document(
    path: &Path,
    document: &Value,
    expected_revision: &str,
) -> Result<(), AppError> {
    let mut bytes =
        serde_json::to_vec_pretty(document).map_err(|source| AppError::JsonSerialize { source })?;
    bytes.push(b'\n');
    ensure_private_models_parent(path)?;
    ensure_models_revision(path, expected_revision)?;
    atomic_write_private(path, &bytes)
}

fn ensure_models_revision(path: &Path, expected_revision: &str) -> Result<(), AppError> {
    let actual_revision = match fs::File::open(path) {
        Ok(_) => revision(&read_file_limited(path, "Pi models")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            MISSING_MODELS_REVISION.to_string()
        }
        Err(error) => return Err(AppError::io(path, error)),
    };
    if actual_revision == expected_revision {
        Ok(())
    } else {
        Err(AppError::Conflict(format!(
            "Pi models.json changed outside CC Switch: {}",
            path.display()
        )))
    }
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn ensure_private_models_parent(path: &Path) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::Config(format!(
            "Pi models path has no parent directory: {}",
            path.display()
        ))
    })?;
    let created = !parent.exists();
    fs::create_dir_all(parent).map_err(|source| AppError::io(parent, source))?;

    #[cfg(not(unix))]
    let _ = created;

    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|source| AppError::io(parent, source))?;
    }

    Ok(())
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<Option<String>, AppError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(AppError::Config(format!(
            "Pi settings '{key}' must be a string: {}",
            path.display()
        ))),
    }
}

fn nonempty_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    pub(crate) struct TestAgentDir {
        _dir: Option<tempfile::TempDir>,
        previous: Option<PathBuf>,
    }

    impl TestAgentDir {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().expect("create Pi test directory");
            let agent_dir = dir.path().join("agent");
            Self::set(agent_dir, Some(dir))
        }

        pub(crate) fn at(agent_dir: &Path) -> Self {
            Self::set(agent_dir.to_path_buf(), None)
        }

        fn set(agent_dir: PathBuf, dir: Option<tempfile::TempDir>) -> Self {
            let previous = super::TEST_AGENT_DIR
                .lock()
                .expect("lock Pi test directory")
                .replace(agent_dir);
            Self {
                _dir: dir,
                previous,
            }
        }
    }

    impl Drop for TestAgentDir {
        fn drop(&mut self) {
            *super::TEST_AGENT_DIR
                .lock()
                .expect("lock Pi test directory") = self.previous.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    fn provider() -> Value {
        json!({
            "name": "Example",
            "baseUrl": "https://api.example.com/v1",
            "api": "openai-completions",
            "apiKey": "secret",
            "models": [{"id": "example-model"}]
        })
    }

    #[test]
    fn provider_node_accepts_unknown_native_fields() {
        let mut value = provider();
        value["sdkOption"] = json!({"timeout": 30});
        value["models"][0]["compat"] = json!({"supportsDeveloperRole": true});
        validate_provider_node("cc-switch-example", &value).expect("valid provider");
    }

    #[test]
    fn provider_node_ownership_depends_on_models_json_membership() {
        let mut oauth = provider();
        oauth["oauth"] = json!("anthropic");
        validate_provider_node("cc-switch-example", &oauth)
            .expect("an explicit models.json node stays manageable");
        validate_provider_node("anthropic", &json!({}))
            .expect("a built-in provider key may be explicitly configured");
        assert!(validate_provider_node("", &json!({})).is_err());
        assert!(validate_provider_node("anthropic", &json!("invalid")).is_err());
    }

    #[test]
    fn relative_agent_directory_is_rejected() {
        let error = resolve_pi_agent_dir(
            None,
            Some("relative/pi-agent".into()),
            PathBuf::from("default"),
        )
        .expect_err("relative Pi directory must be rejected");
        assert!(error.to_string().contains("absolute directory"));
    }

    #[test]
    fn settings_directory_precedes_the_environment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let settings_dir = temp.path().join("settings-agent");
        let env_dir = temp.path().join("env-agent");

        assert_eq!(
            resolve_pi_agent_dir(
                Some(settings_dir.clone()),
                Some(env_dir.into_os_string()),
                temp.path().join("default-agent"),
            )
            .expect("resolve Pi directory"),
            settings_dir
        );
    }

    #[test]
    #[serial]
    fn duplicate_provider_key_is_validation_not_a_write_conflict() {
        let _agent = test_support::TestAgentDir::new();
        insert_pi_provider("duplicate", &provider()).expect("insert provider");
        let mut replacement = provider();
        replacement["name"] = json!("Other");

        let error = insert_pi_provider("duplicate", &replacement)
            .expect_err("duplicate provider key must be rejected");
        assert!(matches!(error, AppError::InvalidInput(_)));
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn newly_created_models_file_and_agent_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let _agent = test_support::TestAgentDir::new();
        insert_pi_provider("cc-switch-private", &provider()).expect("write private models file");

        let path = get_pi_models_path().expect("models path");
        let file_mode = fs::metadata(&path)
            .expect("models metadata")
            .permissions()
            .mode()
            & 0o777;
        let directory_mode = fs::metadata(path.parent().expect("agent directory"))
            .expect("agent directory metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(file_mode, 0o600);
        assert_eq!(directory_mode, 0o700);
    }

    #[test]
    #[serial]
    fn stale_models_revision_does_not_overwrite_an_external_edit() {
        let _agent = test_support::TestAgentDir::new();
        let path = get_pi_models_path().expect("models path");
        ensure_private_models_parent(&path).expect("create agent directory");
        fs::write(&path, r#"{"providers":{"external":{"models":[]}}}"#)
            .expect("write initial models");
        let (_, stale_revision) =
            read_models_document_with_revision(&path).expect("read models revision");

        let external = r#"{"providers":{"external":{"models":[]},"pi-added":{"models":[]}}}"#;
        fs::write(&path, external).expect("edit models externally");

        let replacement = json!({"providers": {"cc-switch": provider()}});
        let error = write_models_document(&path, &replacement, &stale_revision)
            .expect_err("stale write must fail");
        assert!(matches!(error, AppError::Conflict(_)));
        assert_eq!(
            fs::read_to_string(path).expect("read external models"),
            external
        );
    }

    fn takeover_provider(api: &str, base_url: &str, api_key: &str) -> Value {
        json!({
            "name": "Takeover Target",
            "baseUrl": base_url,
            "api": api,
            "apiKey": api_key,
            "models": [{"id": "m1"}]
        })
    }

    #[test]
    fn takeover_prefix_maps_supported_dialects_only() {
        assert_eq!(
            pi_takeover_prefix_for_api("anthropic-messages"),
            Some("pi/anthropic")
        );
        assert_eq!(
            pi_takeover_prefix_for_api("openai-completions"),
            Some("pi/openai")
        );
        assert_eq!(
            pi_takeover_prefix_for_api("openai-responses"),
            Some("pi/openai")
        );
        assert_eq!(pi_takeover_prefix_for_api("google-generative-ai"), None);
        assert_eq!(pi_takeover_prefix_for_api("bedrock-converse-stream"), None);
        assert_eq!(pi_takeover_prefix_for_api(""), None);
    }

    #[test]
    #[serial]
    fn takeover_rewrites_base_url_and_preserves_other_fields_and_nodes() {
        let _agent = test_support::TestAgentDir::new();
        insert_pi_provider(
            "pi-anthropic",
            &takeover_provider("anthropic-messages", "https://anthropic.example.com", "sk-a"),
        )
        .expect("insert anthropic provider");
        insert_pi_provider(
            "pi-openai",
            &takeover_provider("openai-completions", "https://openai.example.com/v1", "sk-o"),
        )
        .expect("insert openai provider");

        apply_pi_takeover_base_url("pi-anthropic", "http://127.0.0.1:15721/")
            .expect("takeover rewrite");

        let node = read_pi_native_provider("pi-anthropic")
            .expect("read node")
            .expect("node exists");
        assert_eq!(
            node["baseUrl"],
            "http://127.0.0.1:15721/pi/anthropic",
            "baseUrl points at the gateway with the anthropic prefix"
        );
        assert_eq!(node["apiKey"], "sk-a", "apiKey must be preserved");
        assert_eq!(node["api"], "anthropic-messages", "api must be preserved");
        assert_eq!(
            node["models"][0]["id"],
            "m1",
            "models must be preserved"
        );

        let sibling = read_pi_native_provider("pi-openai")
            .expect("read sibling")
            .expect("sibling exists");
        assert_eq!(
            sibling["baseUrl"],
            "https://openai.example.com/v1",
            "other provider nodes must stay untouched"
        );
    }

    #[test]
    #[serial]
    fn openai_dialect_maps_to_openai_gateway_prefix() {
        let _agent = test_support::TestAgentDir::new();
        insert_pi_provider(
            "pi-openai",
            &takeover_provider("openai-responses", "https://openai.example.com/v1", "sk-o"),
        )
        .expect("insert provider");

        apply_pi_takeover_base_url("pi-openai", "http://127.0.0.1:15721")
            .expect("takeover rewrite");

        let node = read_pi_native_provider("pi-openai")
            .expect("read node")
            .expect("node exists");
        assert_eq!(node["baseUrl"], "http://127.0.0.1:15721/pi/openai");
    }

    #[test]
    #[serial]
    fn unsupported_dialect_is_rejected_and_models_file_untouched() {
        let _agent = test_support::TestAgentDir::new();
        insert_pi_provider(
            "pi-gemini",
            &takeover_provider("google-generative-ai", "https://gemini.example.com", "sk-g"),
        )
        .expect("insert provider");
        let before = read_models_document_raw()
            .expect("read raw")
            .expect("file exists");

        let error = apply_pi_takeover_base_url("pi-gemini", "http://127.0.0.1:15721")
            .expect_err("unsupported dialect must be rejected");
        assert!(matches!(error, AppError::InvalidInput(_)));

        let after = read_models_document_raw()
            .expect("read raw")
            .expect("file exists");
        assert_eq!(
            before, after,
            "models.json must not be modified on rejected takeover"
        );
    }

    #[test]
    #[serial]
    fn raw_restore_recovers_pre_takeover_models_document() {
        let _agent = test_support::TestAgentDir::new();
        insert_pi_provider(
            "pi-anthropic",
            &takeover_provider("anthropic-messages", "https://anthropic.example.com", "sk-a"),
        )
        .expect("insert provider");
        let backup = read_models_document_raw()
            .expect("read raw")
            .expect("file exists");

        apply_pi_takeover_base_url("pi-anthropic", "http://127.0.0.1:15721")
            .expect("takeover rewrite");
        let taken_over = read_pi_native_provider("pi-anthropic")
            .expect("read node")
            .expect("node exists");
        assert!(taken_over["baseUrl"]
            .as_str()
            .expect("baseUrl string")
            .contains("/pi/anthropic"));

        restore_models_document_raw(&backup).expect("restore raw backup");
        let node = read_pi_native_provider("pi-anthropic")
            .expect("read node")
            .expect("node exists");
        assert_eq!(
            node["baseUrl"],
            "https://anthropic.example.com",
            "restored baseUrl must equal the original upstream URL"
        );
    }

    #[test]
    #[serial]
    fn set_base_url_restores_original_upstream_without_dialect_validation() {
        let _agent = test_support::TestAgentDir::new();
        insert_pi_provider(
            "pi-openai",
            &takeover_provider("openai-completions", "https://openai.example.com/v1", "sk-o"),
        )
        .expect("insert provider");

        apply_pi_takeover_base_url("pi-openai", "http://127.0.0.1:15721").expect("takeover");
        set_pi_provider_base_url("pi-openai", "https://openai.example.com/v1/")
            .expect("restore via ssot helper");

        let node = read_pi_native_provider("pi-openai")
            .expect("read node")
            .expect("node exists");
        assert_eq!(
            node["baseUrl"],
            "https://openai.example.com/v1",
            "trailing slash normalized by the setter"
        );
    }
}
