use super::env_checker::EnvConflict;
use crate::config::{atomic_write, path_is_within};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
use winreg::enums::*;
#[cfg(target_os = "windows")]
use winreg::RegKey;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub backup_path: String,
    pub timestamp: String,
    pub conflicts: Vec<EnvConflict>,
}

/// Delete environment variables with automatic backup
pub fn delete_env_vars(conflicts: Vec<EnvConflict>) -> Result<BackupInfo, String> {
    // Step 1: Create backup
    let backup_info = create_backup(&conflicts)?;

    // Step 2: Delete variables
    for conflict in &conflicts {
        match delete_single_env(conflict) {
            Ok(_) => {}
            Err(e) => {
                // If deletion fails, we keep the backup but return error
                return Err(format!(
                    "删除环境变量失败: {}. 备份已保存到: {}",
                    e, backup_info.backup_path
                ));
            }
        }
    }

    Ok(backup_info)
}

/// Create backup file before deletion
fn create_backup(conflicts: &[EnvConflict]) -> Result<BackupInfo, String> {
    // Get backup directory
    let backup_dir = get_backup_dir()?;
    fs::create_dir_all(&backup_dir).map_err(|e| format!("创建备份目录失败: {e}"))?;

    // Generate backup file name with timestamp
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let backup_file = backup_dir.join(format!("env-backup-{timestamp}.json"));

    // Create backup data
    let backup_info = BackupInfo {
        backup_path: backup_file.to_string_lossy().to_string(),
        timestamp: timestamp.clone(),
        conflicts: conflicts.to_vec(),
    };

    // Write backup file
    let json = serde_json::to_string_pretty(&backup_info)
        .map_err(|e| format!("序列化备份数据失败: {e}"))?;

    atomic_write(&backup_file, json.as_bytes()).map_err(|e| format!("写入备份文件失败: {e}"))?;

    Ok(backup_info)
}

/// Get backup directory path
fn get_backup_dir() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("无法获取用户主目录")?;
    Ok(home.join(".cc-switch").join("backups"))
}

/// 校验 backup_path 必须位于 `~/.cc-switch/backups/` 下。
///
/// `restore_from_backup` 是 IPC 命令，参数来自渲染进程。若不做路径校验，
/// 被劫持的渲染进程（或用户被诱导导入攻击者提供的"备份文件"）可以让本函数
/// 读取任意 JSON 并按其中 `source_path` 向任意可写文件追加 `export X=Y`——
/// 下次开 shell 即执行。canonicalize + `path_is_within` 双重校验阻断该路径。
fn validate_backup_path(raw: &str) -> Result<PathBuf, String> {
    let backup_dir = get_backup_dir()?;
    // canonicalize 要求路径存在；backup_dir 本身可能尚未创建，先 ensure。
    fs::create_dir_all(&backup_dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    let canonical_dir = backup_dir
        .canonicalize()
        .map_err(|e| format!("无法解析备份目录: {e}"))?;
    let candidate = Path::new(raw)
        .canonicalize()
        .map_err(|e| format!("无法解析备份文件路径: {e}"))?;

    if !path_is_within(&canonical_dir, &candidate) {
        return Err(format!(
            "备份文件必须位于 {} 下，拒绝访问: {}",
            canonical_dir.display(),
            candidate.display()
        ));
    }
    Ok(candidate)
}

/// Unix 侧允许写入的 shell rc 文件白名单（相对 home 的路径）。
///
/// `EnvConflict.source_path` 由 `env_checker` 扫描产生，理论上只包含这些文件；
/// 但 `restore_from_backup` 读取的是**磁盘上的 JSON**，攻击者可构造任意
/// `source_path`。白名单 + canonicalize 双重校验，阻断向 `/etc/profile`、
/// `~/.ssh/environment`、`~/.bash_aliases` 等非标准位置写入。
#[cfg(not(target_os = "windows"))]
const ALLOWED_SHELL_RC_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
];

/// 校验 source_path（形如 `"path:line"`）中的文件部分是否在白名单内。
///
/// 返回 canonicalize 后的真实路径。canonicalize 会解析 symlink，阻断
/// "白名单文件是指向 /etc/profile 的 symlink" 这类绕过。
#[cfg(not(target_os = "windows"))]
fn validate_shell_rc_path(source_path: &str) -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("无法获取用户主目录")?;

    // source_path 格式为 "path:line"；path 本身可能含 ':'（Windows 盘符不适用
    // 于 Unix 分支），取最后一个 ':' 之后为行号，之前为路径。
    let file_part = match source_path.rfind(':') {
        Some(idx) => &source_path[..idx],
        None => source_path,
    };

    let candidate = Path::new(file_part)
        .canonicalize()
        .map_err(|e| format!("无法解析 shell rc 路径 {file_part}: {e}"))?;

    let allowed = ALLOWED_SHELL_RC_FILES.iter().any(|name| {
        let expected = home.join(name);
        // canonicalize expected 也可能失败（文件不存在），此时用词法比较兜底。
        match expected.canonicalize() {
            Ok(canon) => canon == candidate,
            Err(_) => expected == candidate,
        }
    });

    if !allowed {
        return Err(format!(
            "拒绝写入非白名单 shell rc 文件: {}（允许: {:?}）",
            candidate.display(),
            ALLOWED_SHELL_RC_FILES
        ));
    }
    Ok(candidate)
}

/// POSIX shell 单引号引用：把任意字符串安全地变成 shell 字面量。
///
/// 单引号内所有字符都是字面量（包括 `$`、`` ` ``、`\`、`"`），唯一的例外是
/// 单引号本身——标准做法是结束当前引号、插入转义的单引号 `\'`、再重新开始。
/// 即 `foo'bar` → `'foo'\''bar'`。
///
/// 早期实现直接 `format!("export {}={}", name, value)`，值含 `;`、`$()`、
/// 反引号时会在下次 shell 启动时被解释执行。
///
/// 纯字符串变换，无平台依赖：不加 cfg 门控，使其单测在 Windows CI 上也能执行
/// （此前 `#[cfg(not(windows))]` 导致该关键安全函数在 Windows 上从未被测到）。
/// Windows 非测试构建中无调用点，故仅在该情形下允许 dead_code。
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn shell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// 校验 var_name 是合法的 shell 标识符（`[A-Za-z_][A-Za-z0-9_]*`）。
///
/// 阻断 `var_name` 含 `=`、`;`、`$`、空格等字符时的注入。
///
/// 纯校验函数，无平台依赖：不加 cfg 门控以便 Windows CI 也能执行其单测。
/// Windows 非测试构建中无调用点，故仅在该情形下允许 dead_code。
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn validate_var_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("环境变量名不能为空".to_string());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!("非法环境变量名（首字符）: {name}"));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("非法环境变量名: {name}"));
    }
    Ok(())
}

/// 扫描一行，更新"是否处于未闭合的单引号/双引号中"的状态。
///
/// 规则与 POSIX shell 一致：单引号内一切字符字面（含双引号与反斜杠），只有
/// 再遇单引号才退出；双引号内反斜杠转义下一个字符，未转义的双引号退出；
/// 引号外反斜杠转义下一个字符。注释（引号外的 `#`）之后到行尾不再影响状态。
fn quote_state_after_line(line: &str, mut in_single: bool, mut in_double: bool) -> (bool, bool) {
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            match c {
                '\\' => {
                    chars.next(); // 转义字符不参与引号判定
                }
                '"' => in_double = false,
                _ => {}
            }
            continue;
        }
        match c {
            '\\' => {
                chars.next();
            }
            '\'' => in_single = true,
            '"' => in_double = true,
            '#' => break, // 引号外注释：行尾不再影响引号状态
            _ => {}
        }
    }
    (in_single, in_double)
}

/// 提取行内 heredoc 起始定界符（`<<WORD` / `<<-WORD` / `<<'WORD'` / `<<"WORD"`）。
/// 返回 None 表示该行没有 heredoc。
fn heredoc_delimiter(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'<' {
            let mut j = i + 2;
            // 跳过可选的 '-'（strip 制表符形式）
            if j < bytes.len() && bytes[j] == b'-' {
                j += 1;
            }
            // 跳过可选引号
            let quote = if j < bytes.len() && (bytes[j] == b'\'' || bytes[j] == b'"') {
                let q = bytes[j];
                j += 1;
                Some(q)
            } else {
                None
            };
            let start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > start {
                let mut word = line[start..j].to_string();
                // 若带引号，确认闭合引号紧随其后；不闭合则视为非 heredoc
                if let Some(q) = quote {
                    if j >= bytes.len() || bytes[j] != q {
                        return None;
                    }
                }
                word.shrink_to_fit();
                return Some(word);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// 把物理行分组为"逻辑行"：引号未闭合或 heredoc 未结束时的续行并入首行组。
///
/// 返回每组的首物理行索引与该组包含的物理行（引用）。用于删除环境变量时
/// 整组移除，避免多行值（`export FOO="a\nb"`、heredoc）残留续行损坏文件（L8）。
fn group_logical_lines<'a>(lines: &[&'a str]) -> Vec<(usize, Vec<&'a str>)> {
    let mut groups: Vec<(usize, Vec<&str>)> = Vec::new();
    let mut cur_start: Option<usize> = None;
    let mut cur_lines: Vec<&str> = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut heredoc: Option<String> = None;

    for (idx, line) in lines.iter().enumerate() {
        if let Some(delim) = heredoc.clone() {
            // heredoc 体内：只判定结束定界符，不更新引号状态
            cur_lines.push(line);
            if line.trim() == delim {
                heredoc = None;
                if !in_single && !in_double {
                    groups.push((cur_start.unwrap_or(idx), std::mem::take(&mut cur_lines)));
                    cur_start = None;
                }
            }
            continue;
        }

        if cur_start.is_none() {
            cur_start = Some(idx);
        }
        cur_lines.push(line);

        // 仅逻辑行首行可能开启 heredoc
        if cur_lines.len() == 1 {
            heredoc = heredoc_delimiter(line);
        }
        let (s, d) = quote_state_after_line(line, in_single, in_double);
        in_single = s;
        in_double = d;

        if !in_single && !in_double && heredoc.is_none() {
            groups.push((cur_start.unwrap_or(idx), std::mem::take(&mut cur_lines)));
            cur_start = None;
        }
    }
    if !cur_lines.is_empty() {
        groups.push((cur_start.unwrap_or(0), cur_lines));
    }
    groups
}

/// 从 shell rc 文本中删除指定变量的赋值（整逻辑行组），返回新文本。
///
/// 纯函数、跨平台可测。只匹配行首 `export NAME=` 或 `NAME=` 形式；注释行、
/// 以 NAME 为子串的其他变量（如 `FOO_NAME=`）不受影响。多行值与 heredoc
/// 随首行整组删除。
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn filter_out_var(content: &str, var: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut keep: Vec<bool> = vec![true; lines.len()];

    for (start, group) in group_logical_lines(&lines) {
        let first = group.first().copied().unwrap_or("");
        let trimmed = first.trim_start();
        let assign = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        // 两种目标形态：`VAR=value` 赋值行（含空格），以及 `VAR<<EOF` heredoc
        // 重定向行（heredoc 组由 group_logical_lines 定界，整组删除）。
        let is_eq_assign = assign
            .find('=')
            .map(|eq| assign[..eq].trim() == var)
            .unwrap_or(false);
        let is_heredoc_target = assign.len() > var.len()
            && assign.starts_with(var)
            && assign[var.len()..].starts_with("<<");
        if is_eq_assign || is_heredoc_target {
            // 逻辑行组的物理行是连续区间，直接按 [start, start+len) 标记删除。
            for offset in 0..group.len() {
                if start + offset < keep.len() {
                    keep[start + offset] = false;
                }
            }
        }
    }

    lines
        .iter()
        .zip(keep.iter())
        .filter(|(_, k)| **k)
        .map(|(l, _)| *l)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Delete a single environment variable
#[cfg(target_os = "windows")]
fn delete_single_env(conflict: &EnvConflict) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "system" => {
            if conflict.source_path.contains("HKEY_CURRENT_USER") {
                let hkcu = RegKey::predef(HKEY_CURRENT_USER)
                    .open_subkey_with_flags("Environment", KEY_ALL_ACCESS)
                    .map_err(|e| format!("打开注册表失败: {}", e))?;

                hkcu.delete_value(&conflict.var_name)
                    .map_err(|e| format!("删除注册表项失败: {}", e))?;
            } else if conflict.source_path.contains("HKEY_LOCAL_MACHINE") {
                let hklm = RegKey::predef(HKEY_LOCAL_MACHINE)
                    .open_subkey_with_flags(
                        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
                        KEY_ALL_ACCESS,
                    )
                    .map_err(|e| format!("打开系统注册表失败 (需要管理员权限): {}", e))?;

                hklm.delete_value(&conflict.var_name)
                    .map_err(|e| format!("删除系统注册表项失败: {}", e))?;
            }
            Ok(())
        }
        "file" => Err("Windows 系统不应该有文件类型的环境变量".to_string()),
        _ => Err(format!("未知的环境变量来源类型: {}", conflict.source_type)),
    }
}

#[cfg(not(target_os = "windows"))]
fn delete_single_env(conflict: &EnvConflict) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "file" => {
            validate_var_name(&conflict.var_name)?;
            let file_path = validate_shell_rc_path(&conflict.source_path)?;

            // Read file content
            let content = fs::read_to_string(&file_path)
                .map_err(|e| format!("读取文件失败 {}: {e}", file_path.display()))?;

            // 按"逻辑行"整组删除目标变量：引号跨行 / heredoc 的续行随首行一起
            // 移除，避免早期物理行过滤残留续行导致 rc 文件语法损坏（L8）。
            let new_content = filter_out_var(&content, &conflict.var_name);

            // 原子写：避免半写状态损坏用户的 .bashrc（下次开 shell 会失败）
            atomic_write(&file_path, new_content.as_bytes())
                .map_err(|e| format!("写入文件失败 {}: {e}", file_path.display()))?;

            Ok(())
        }
        "system" => {
            // On Unix, we can't directly delete process environment variables
            Ok(())
        }
        _ => Err(format!("未知的环境变量来源类型: {}", conflict.source_type)),
    }
}

/// Restore environment variables from backup
///
/// `backup_path` 来自 IPC（渲染进程），必须校验位于 `~/.cc-switch/backups/` 下；
/// 每条 `EnvConflict.source_path` 必须命中 shell rc 白名单（Unix）或已知注册表
/// 路径（Windows）。值经 shell 单引号引用后写入，阻断 `; rm -rf ~` 类注入。
pub fn restore_from_backup(backup_path: String) -> Result<(), String> {
    let validated_path = validate_backup_path(&backup_path)?;

    // Read backup file
    let content = fs::read_to_string(&validated_path)
        .map_err(|e| format!("读取备份文件失败 {}: {e}", validated_path.display()))?;

    let backup_info: BackupInfo =
        serde_json::from_str(&content).map_err(|e| format!("解析备份文件失败: {e}"))?;

    // Restore each variable
    for conflict in &backup_info.conflicts {
        restore_single_env(conflict)?;
    }

    Ok(())
}

/// Restore a single environment variable
#[cfg(target_os = "windows")]
fn restore_single_env(conflict: &EnvConflict) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "system" => {
            // 显式校验 source_path 是已知的两个注册表路径之一，阻断任意
            // source_path 让 set_value 落到非预期位置。
            let is_hkcu = conflict.source_path.contains("HKEY_CURRENT_USER");
            let is_hklm = conflict.source_path.contains("HKEY_LOCAL_MACHINE");
            if !is_hkcu && !is_hklm {
                return Err(format!("拒绝写入未知注册表路径: {}", conflict.source_path));
            }

            if is_hkcu {
                let (hkcu, _) = RegKey::predef(HKEY_CURRENT_USER)
                    .create_subkey("Environment")
                    .map_err(|e| format!("打开注册表失败: {}", e))?;

                hkcu.set_value(&conflict.var_name, &conflict.var_value)
                    .map_err(|e| format!("恢复注册表项失败: {}", e))?;
            } else {
                let (hklm, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
                    .create_subkey(
                        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
                    )
                    .map_err(|e| format!("打开系统注册表失败 (需要管理员权限): {}", e))?;

                hklm.set_value(&conflict.var_name, &conflict.var_value)
                    .map_err(|e| format!("恢复系统注册表项失败: {}", e))?;
            }
            Ok(())
        }
        _ => Err(format!(
            "无法恢复类型为 {} 的环境变量",
            conflict.source_type
        )),
    }
}

#[cfg(not(target_os = "windows"))]
fn restore_single_env(conflict: &EnvConflict) -> Result<(), String> {
    match conflict.source_type.as_str() {
        "file" => {
            validate_var_name(&conflict.var_name)?;
            let file_path = validate_shell_rc_path(&conflict.source_path)?;

            // Read file content
            let mut content = fs::read_to_string(&file_path)
                .map_err(|e| format!("读取文件失败 {}: {e}", file_path.display()))?;

            // 幂等：若已有同名 export 行，先剔除再追加，避免重复累积。
            let target = &conflict.var_name;
            let filtered: Vec<&str> = content
                .lines()
                .filter(|line| {
                    let trimmed = line.trim_start();
                    let export_line = trimmed.strip_prefix("export ").unwrap_or(trimmed);
                    if let Some(eq_pos) = export_line.find('=') {
                        export_line[..eq_pos].trim() != target
                    } else {
                        true
                    }
                })
                .collect();
            content = filtered.join("\n");

            // Append the environment variable line, shell-quoted.
            let quoted = shell_single_quote(&conflict.var_value);
            let export_line = format!("\nexport {}={}", conflict.var_name, quoted);
            content.push_str(&export_line);

            // 原子写：避免半写状态损坏用户的 .bashrc
            atomic_write(&file_path, content.as_bytes())
                .map_err(|e| format!("写入文件失败 {}: {e}", file_path.display()))?;

            Ok(())
        }
        _ => Err(format!(
            "无法恢复类型为 {} 的环境变量",
            conflict.source_type
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backup_dir_creation() {
        let backup_dir = get_backup_dir();
        assert!(backup_dir.is_ok());
    }

    // 跨平台执行：shell_single_quote / validate_var_name 是纯函数，无 Unix 依赖，
    // 其安全语义（shell 注入防护）在任何 CI 上都必须被验证到。
    #[test]
    fn shell_single_quote_escapes_embedded_quotes() {
        assert_eq!(shell_single_quote("simple"), "'simple'");
        assert_eq!(shell_single_quote("with space"), "'with space'");
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_single_quote(""), "''");
        // 元字符在单引号内都是字面量
        assert_eq!(shell_single_quote("; rm -rf ~"), "'; rm -rf ~'");
        assert_eq!(shell_single_quote("$(whoami)"), "'$(whoami)'");
        assert_eq!(shell_single_quote("`id`"), "'`id`'");
        assert_eq!(shell_single_quote("a\\b"), "'a\\b'");
    }

    #[test]
    fn validate_var_name_rejects_injection() {
        assert!(validate_var_name("FOO").is_ok());
        assert!(validate_var_name("_foo_bar1").is_ok());
        assert!(validate_var_name("").is_err());
        assert!(validate_var_name("1FOO").is_err());
        assert!(validate_var_name("FOO;BAR").is_err());
        assert!(validate_var_name("FOO BAR").is_err());
        assert!(validate_var_name("FOO=BAR").is_err());
        assert!(validate_var_name("$FOO").is_err());
    }

    #[test]
    fn filter_out_var_removes_single_line_assignment() {
        let src = "export FOO=1\nexport BAR=2\nFOO=3\n";
        let out = filter_out_var(src, "FOO");
        assert_eq!(out, "export BAR=2");
    }

    #[test]
    fn filter_out_var_does_not_touch_substring_or_comment() {
        let src =
            "# export FOO=commented\nexport MY_FOO=keep\nexport FOOBAR=keep\nexport FOO=drop\n";
        let out = filter_out_var(src, "FOO");
        assert!(out.contains("# export FOO=commented"), "comment kept");
        assert!(out.contains("export MY_FOO=keep"), "substring var kept");
        assert!(out.contains("export FOOBAR=keep"), "prefix var kept");
        assert!(!out.contains("export FOO=drop"), "target dropped");
    }

    #[test]
    fn filter_out_var_removes_multiline_quoted_value_whole() {
        // L8 核心：多行引号值必须整组删除，不能残留续行。
        let src = "export FOO=\"line1\nline2\nline3\"\nexport BAR=after\n";
        let out = filter_out_var(src, "FOO");
        assert_eq!(out, "export BAR=after", "continuation lines must not leak");
    }

    #[test]
    fn filter_out_var_removes_heredoc_whole() {
        let src = "export FOO<<EOF\nbody1\nbody2\nEOF\nexport BAR=after\n";
        let out = filter_out_var(src, "FOO");
        assert_eq!(out, "export BAR=after", "heredoc body must not leak");
    }

    #[test]
    fn filter_out_var_keeps_unrelated_multiline_blocks() {
        // 非目标变量的多行块必须原样保留。
        let src = "export OTHER=\"a\nb\"\nexport FOO=x\n";
        let out = filter_out_var(src, "FOO");
        assert_eq!(out, "export OTHER=\"a\nb\"");
    }

    #[test]
    fn validate_backup_path_rejects_outside_dir() {
        // 构造一个明显在 backups 目录之外的路径
        let outside = if cfg!(target_os = "windows") {
            r"C:\Windows\System32\evil.json"
        } else {
            "/etc/passwd"
        };
        let result = validate_backup_path(outside);
        assert!(result.is_err(), "should reject path outside backup dir");
    }
}
