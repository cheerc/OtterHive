//! T4 Lane A GREEN：`backend: claude`（契約 §5.1）。
//!
//! 參數組裝、協調提示、MCP 設定、skills symlink 與 resume 證據都在此。
//! `backend: command` 的完整行為延到需要它的工作包（T4 不執行 `command`／
//! `args`）；本模組只負責 `claude`。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 協調提示檔名（契約 §5.1）。
pub const PROMPT_FILE: &str = "otterhive.md";
/// MCP 設定檔名（契約 §5.1）。
pub const MCP_FILE: &str = "otterhive-mcp.json";
/// skills symlink 名稱（契約 §5.1）。
pub const SKILLS_LINK: &str = "skills";

/// `backend: claude` 的 adapter。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeBackend;

/// 組裝 `claude` 的 argv（契約 §5.1 的參數順序）。
///
/// `resume` 為 true 時以 `--continue` 取代預設的
/// `--dangerously-skip-permissions`（§5.4：有證據時以 `--continue` spawn）。
pub fn build_argv(
    executable: &str,
    model: Option<&str>,
    effort: Option<&str>,
    prompt_file: &Path,
    mcp_config: &Path,
    args: &[String],
    resume: bool,
) -> Vec<String> {
    let mut argv = vec![executable.to_owned()];
    if resume {
        argv.push("--continue".to_owned());
    } else {
        argv.push("--dangerously-skip-permissions".to_owned());
    }
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        argv.push("--effort".to_owned());
        argv.push(effort.to_owned());
    }
    argv.push("--append-system-prompt-file".to_owned());
    argv.push(prompt_file.to_string_lossy().into_owned());
    argv.push("--mcp-config".to_owned());
    argv.push(mcp_config.to_string_lossy().into_owned());
    for arg in args {
        argv.push(arg.clone());
    }
    argv
}

/// 協調提示內容（契約 §5.1）。
///
/// 至少包含 instance 名稱、可用工具、`[OTTERHIVE-MSG]` 的 drain／ack 與
/// `send` 的 `parent_id` 規則；`instructions` 原文附在其後。
pub fn coordination_prompt(
    name: &str,
    team: Option<(&str, &str)>,
    tools: &[&str],
    instructions: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("# OtterHive 協調提示\n\n");
    prompt.push_str(&format!("你是 OtterHive instance `{name}`。\n\n"));
    match team {
        Some((team_name, orchestrator)) => {
            prompt.push_str(&format!(
                "你屬於 team `{team_name}`，orchestrator 是 `{orchestrator}`。\n\n"
            ));
        }
        None => prompt.push_str("你目前不屬於任何 team。\n\n"),
    }
    prompt.push_str("## 可用的 OtterHive 工具\n\n");
    for tool in tools {
        prompt.push_str(&format!("- `{tool}`\n"));
    }
    prompt.push_str(
        "\n## 訊息規則\n\n\
         - 收到 `[OTTERHIVE-MSG]` 開頭的提示時，代表要呼叫 `inbox` drain 取訊息。\n\
         - 每則訊息處理完都要帶該訊息的 `delivery_token` 呼叫 `inbox` ack。\n\
         - 回覆訊息用 `send`，並帶上該則訊息的 `parent_id`。\n\n",
    );
    if let Some(instructions) = instructions {
        prompt.push_str("# 你的指示\n\n");
        prompt.push_str(instructions);
        if !instructions.ends_with('\n') {
            prompt.push('\n');
        }
    }
    prompt
}

/// 寫入協調提示（`<workdir>/.claude/otterhive.md`），回傳寫入路徑。
pub fn write_prompt(workdir: &Path, contents: &str) -> std::io::Result<PathBuf> {
    let path = workdir.join(".claude").join(PROMPT_FILE);
    let mut file = fs::File::create(&path)?;
    file.write_all(contents.as_bytes())?;
    Ok(path)
}

/// MCP 設定的 JSON（契約 §5.1 的結構），權限 0600。
pub fn mcp_config_value(home: &Path, bridge: &Path, token: &str) -> Value {
    json!({
        "mcpServers": {
            "otterhive": {
                "type": "stdio",
                "command": bridge.to_string_lossy(),
                "args": [],
                "env": {
                    "OTTERHIVE_HOME": home.to_string_lossy(),
                    "OTTERHIVE_INSTANCE_TOKEN": token,
                },
            }
        }
    })
}

/// 寫入 MCP 設定（`<workdir>/.claude/otterhive-mcp.json`），權限 0600。
pub fn write_mcp_config(workdir: &Path, value: &Value) -> std::io::Result<PathBuf> {
    let path = workdir.join(".claude").join(MCP_FILE);
    let encoded = serde_json::to_string_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = fs::File::create(&path)?;
    file.write_all(encoded.as_bytes())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// 建立 `<workdir>/.claude/skills` symlink（契約 §5.1）。
///
/// 回 `Ok(())` 表示由本次建立；`Err(existing)` 表示路徑已被占用且**不是**
/// OtterHive 擁有的 symlink——呼叫端據此回 warning，不覆蓋。
pub fn create_skills_link(
    workdir: &Path,
    skills_path: &Path,
    owned_target: Option<&Path>,
) -> Result<(), SkillsLinkConflict> {
    let link = workdir.join(".claude").join(SKILLS_LINK);
    match fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // 只有 journal 記錄的 target 完全一致才算 OtterHive 擁有。
            if owned_target == Some(skills_path)
                && fs::read_link(&link).ok().as_deref() == Some(skills_path)
            {
                return Ok(());
            }
            Err(SkillsLinkConflict::Occupied(link))
        }
        Ok(_) => Err(SkillsLinkConflict::Occupied(link)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = link.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(skills_path, &link).map_err(|error| match error.kind() {
                std::io::ErrorKind::AlreadyExists => SkillsLinkConflict::Occupied(link),
                _ => SkillsLinkConflict::Io(error.to_string()),
            })?;
            Ok(())
        }
        Err(error) => Err(SkillsLinkConflict::Io(error.to_string())),
    }
}

/// `.claude/skills` 路徑被占用時的結果。
#[derive(Debug)]
pub enum SkillsLinkConflict {
    /// 路徑已被其他檔案、目錄或 symlink 占用（不得覆蓋）。
    Occupied(PathBuf),
    /// 其他 I/O 錯誤。
    Io(String),
}

impl From<std::io::Error> for SkillsLinkConflict {
    fn from(error: std::io::Error) -> Self {
        SkillsLinkConflict::Io(error.to_string())
    }
}

impl std::fmt::Display for SkillsLinkConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillsLinkConflict::Occupied(path) => {
                write!(formatter, "skills 路徑 {} 已被占用，未覆蓋", path.display())
            }
            SkillsLinkConflict::Io(reason) => write!(formatter, "skills link 失敗：{reason}"),
        }
    }
}

/// resume 證據的檢查結果（契約 §5.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeEvidence {
    /// 有 `.jsonl` 含 `"type":"user"`；可以 `--continue` spawn。
    Found,
    /// 沒有任何 session 紀錄。
    None,
    /// 有其他未刪除 instance 使用同一 canonical working directory（§5.4）。
    SharedWorkingDirectory,
}

/// `<workdir>` 的專案編碼（§5.4：非 `[A-Za-z0-9-]` 換成 `-`）。
pub fn encode_project_dir(workdir: &Path) -> String {
    workdir
        .to_string_lossy()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// 檢查 resume 證據（契約 §5.4）。
pub fn resume_evidence(home: &Path, workdir: &Path) -> ResumeEvidence {
    let projects = home
        .join(".claude/projects")
        .join(encode_project_dir(workdir));
    let Ok(entries) = fs::read_dir(&projects) else {
        return ResumeEvidence::None;
    };
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jsonl")
            && fs::read_to_string(entry.path()).is_ok_and(|text| text.contains("\"type\":\"user\""))
        {
            return ResumeEvidence::Found;
        }
    }
    ResumeEvidence::None
}

/// 子程序環境（契約 §5.3）：繼承 daemon 環境並移除 `OTTERHIVE_OPERATOR` 前綴。
pub fn child_environment(
    base: &BTreeMap<String, String>,
    spec_env: &BTreeMap<String, String>,
    home: &Path,
    name: &str,
    token: &str,
    bridge: &Path,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base
        .iter()
        .filter(|(key, _)| !key.starts_with("OTTERHIVE_OPERATOR"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (key, value) in spec_env {
        // §5.3 的移除必須涵蓋 `env` 指定的變數，不能只過濾繼承來的 `base`。
        // 否則 `create_instance` 的 `env: {"OTTERHIVE_OPERATOR_…": …}` 就會把
        // operator 憑證送進子程序——契約那句是無條件的（「子程序不得看到名稱以
        // `OTTERHIVE_OPERATOR` 開頭的變數」），而 `session.rs` 的 `spawn` 層
        // 過濾用的是**繼承環境**，同樣看不到這裡指定的鍵。
        if key.starts_with("OTTERHIVE_OPERATOR") {
            continue;
        }
        env.insert(key.clone(), value.clone());
    }
    env.insert(
        "OTTERHIVE_HOME".to_owned(),
        home.to_string_lossy().into_owned(),
    );
    env.insert("OTTERHIVE_INSTANCE".to_owned(), name.to_owned());
    env.insert("OTTERHIVE_INSTANCE_TOKEN".to_owned(), token.to_owned());
    env.insert(
        "OTTERHIVE_MCP_BRIDGE".to_owned(),
        bridge.to_string_lossy().into_owned(),
    );
    env.insert("TERM".to_owned(), "xterm-256color".to_owned());
    env
}

#[cfg(test)]
mod tests {
    //! `child_environment` 的 §5.3 保證（整合層見 `tests/backend_contract.rs`）。
    use super::child_environment;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn env_of(spec_env: &[(&str, &str)]) -> BTreeMap<String, String> {
        let base: BTreeMap<String, String> = [
            ("OTTERHIVE_OPERATOR_TOKEN", "daemon-side-secret"),
            ("PATH", "/usr/bin"),
        ]
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
        let spec: BTreeMap<String, String> = spec_env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        child_environment(
            &base,
            &spec,
            Path::new("/tmp/home"),
            "worker",
            "token",
            Path::new("/tmp/bridge"),
        )
    }

    /// §5.3：子程序不得看到 `OTTERHIVE_OPERATOR` 前綴的變數——**包括 `env` 指定的**。
    #[test]
    fn spec_env_cannot_smuggle_operator_prefixed_variables() {
        let env = env_of(&[("OTTERHIVE_OPERATOR_SMUGGLED", "leaked")]);
        assert!(
            !env.keys().any(|key| key.starts_with("OTTERHIVE_OPERATOR")),
            "繁中說明：`env` 指定的 OTTERHIVE_OPERATOR 前綴變數也必須被 §5.3 移除，實際：{:?}",
            env.keys().collect::<Vec<_>>()
        );
    }

    /// 正常 `env` 仍生效（過濾不得誤傷）。
    #[test]
    fn spec_env_still_applies_ordinary_variables() {
        let env = env_of(&[("CUSTOM_FLAG", "on")]);
        assert_eq!(env.get("CUSTOM_FLAG").map(String::as_str), Some("on"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    }
}
