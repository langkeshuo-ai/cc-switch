//! 敏感 JSON 存储文件的静态加密封装。
//!
//! 与 `settings.rs` 中凭据字段的加密取舍一致：
//! - Windows：整份文件内容经 DPAPI（绑定当前用户）加密，密文带
//!   `ccswitch-dpapi-v1:` 前缀；
//! - 非 Windows：依赖 0o600 文件权限，内容保持明文（macOS Keychain /
//!   Linux libsecret 属独立增强，不在本轮范围）。
//!
//! 读取时无前缀一律视为历史明文，实现平滑迁移、无需一次性 migration。
//! 当前消费方：`codex_oauth_auth.json` / `xai_oauth_auth.json` /
//! `copilot_auth.json`（均含长效 refresh_token，为本地文件，不参与云同步）。

use std::path::Path;

/// 落盘前加封：Windows 走 DPAPI，失败或非 Windows 原样返回明文。
pub(crate) fn seal_content(plain: &str) -> String {
    #[cfg(windows)]
    {
        if let Some(sealed) = crate::settings::dpapi_protect(plain) {
            return sealed;
        }
    }
    plain.to_string()
}

/// 读取后解封：带 DPAPI 前缀则解密，否则按历史明文原样返回。
pub(crate) fn unseal_content(raw: &str) -> Result<String, String> {
    #[cfg(windows)]
    {
        if raw.starts_with(crate::settings::DPAPI_MARKER) {
            return crate::settings::dpapi_unprotect(raw).ok_or_else(|| {
                "凭据存储解密失败（DPAPI：文件可能由其他用户或系统加密）".to_string()
            });
        }
    }
    Ok(raw.to_string())
}

/// 读取文件并解封为明文字符串。解密失败以 `InvalidData` 报错。
pub(crate) fn read_sealed_to_string(path: &Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    unseal_content(&raw)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_unseal_round_trip() {
        let plain = r#"{"access_token":"secret-token"}"#;
        let sealed = seal_content(plain);
        #[cfg(windows)]
        assert!(
            sealed.starts_with(crate::settings::DPAPI_MARKER),
            "sealed content should carry the DPAPI marker on Windows"
        );
        assert_eq!(unseal_content(&sealed).unwrap(), plain);
    }

    #[test]
    fn unseal_plaintext_passthrough_for_legacy_files() {
        assert_eq!(unseal_content("{\"a\":1}").unwrap(), "{\"a\":1}");
    }
}
