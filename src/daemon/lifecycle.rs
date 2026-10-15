//! T4 Lane A GREEN：instance lifecycle 的規格檢查與狀態（契約 §4.1）。
//!
//! 本模組只做「任何變更之前」的驗證與狀態列舉；真正的 start／stop／reap 走
//! `crate::procs` 的證據式停止核心，編排見 `crate::daemon`。

use std::path::{Path, PathBuf};

use serde_json::Value;

/// 一個 instance 的 lifecycle 狀態（契約 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    Stopped,
    Starting,
    Running,
    Stopping,
    /// 附原因（`exited_during_startup`／`exec_failed`／`exited`／`daemon_restarted`）。
    Failed,
    RecoveryRequired,
}

impl InstanceState {
    /// SQLite `instances.state` 的字串形式。
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Stopped => "stopped",
            InstanceState::Starting => "starting",
            InstanceState::Running => "running",
            InstanceState::Stopping => "stopping",
            InstanceState::Failed => "failed",
            InstanceState::RecoveryRequired => "recovery_required",
        }
    }

    /// 由 SQLite 的字串解析（未知值視為 `stopped`，不讓壞資料讓 daemon 崩潰）。
    ///
    /// 刻意不實作 `FromStr`：這不是會失敗的解析（未知值有明確的退路），
    /// 命名 `from_state` 以免被誤認為標準的 `from_str`。
    pub fn from_state(value: &str) -> Self {
        match value {
            "starting" => InstanceState::Starting,
            "running" => InstanceState::Running,
            "stopping" => InstanceState::Stopping,
            "failed" => InstanceState::Failed,
            "recovery_required" => InstanceState::RecoveryRequired,
            _ => InstanceState::Stopped,
        }
    }
}

/// 規格檢查失敗的原因（契約 §3 的對應錯誤碼）。
#[derive(Debug)]
pub enum SpecError {
    /// `invalid_argument`：欄位內容不符（§4.1 的各種檢查）。
    InvalidArgument(String),
    /// `unsupported_in_phase`：本期不支援的 backend。
    UnsupportedInPhase { backend: String, message: String },
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::InvalidArgument(reason) => {
                write!(formatter, "instance 規格不符契約：{reason}")
            }
            SpecError::UnsupportedInPhase { backend, message } => {
                write!(formatter, "backend {backend} 在 P1a 不支援：{message}")
            }
        }
    }
}

/// 檢查後的 instance 規格（契約 §4.1 的 `InstanceSpec`）。
#[derive(Debug, Clone)]
pub struct ValidatedSpec {
    pub name: String,
    pub backend: String,
    /// `command` 欄位保留但 T4 不執行（backend 固定為 claude；契約 §5.1 的
    /// `command` 是「覆寫可執行檔」，T4 讓它覆寫 `claude`）。
    pub command: Option<String>,
    pub args: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub working_directory: PathBuf,
    pub explicit_working_directory: bool,
    pub instructions: Option<PathBuf>,
    pub instructions_text: Option<String>,
    pub skills_path: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub description: Option<String>,
    pub role: Option<String>,
}

/// 名稱規則（契約 §4.1）：1–64 字元、只含 `[A-Za-z0-9_-]`、不得為 `operator`。
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(String::from("name 不得為空"));
    }
    if name.chars().count() > 64 {
        return Err(String::from("name 不得超過 64 字元"));
    }
    if name == "operator" {
        return Err(String::from("name 不得為 operator（保留名）"));
    }
    if let Some(bad) = name
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-'))
    {
        return Err(format!("name 不得含 {bad:?}，只允許 [A-Za-z0-9_-]"));
    }
    Ok(())
}

/// 從 `create_instance` 的 args 驗證規格（契約 §4.1；任何變更之前完成）。
///
/// `home` 用於未給 `working_directory` 時的預設路徑。
pub fn validate_spec(args: &Value, home: &Path) -> Result<ValidatedSpec, SpecError> {
    let object = args
        .as_object()
        .ok_or_else(|| SpecError::InvalidArgument(String::from("args 必須是物件")))?;

    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| SpecError::InvalidArgument(String::from("name 必填且須為字串")))?
        .to_owned();
    validate_name(&name).map_err(SpecError::InvalidArgument)?;

    let backend = object
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or("claude")
        .to_owned();
    if backend != "claude" {
        // T4 只實作 claude；明確 command 回 unsupported_in_phase（plan T4）。
        return Err(SpecError::UnsupportedInPhase {
            backend: backend.clone(),
            message: String::from("backend command 延到需要它的工作包，P1a 只支援 claude"),
        });
    }

    let command = object
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let args_list = match object.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    SpecError::InvalidArgument(String::from("args 的每一項都必須是字串"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(SpecError::InvalidArgument(String::from(
                "args 必須是字串陣列",
            )))
        }
    };
    let model = optional_string(object, "model")?;
    let effort = optional_string(object, "effort")?;
    let description = optional_string(object, "description")?;
    let role = optional_string(object, "role")?;

    let env = match object.get("env") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(map)) => {
            let mut env = Vec::new();
            for (key, value) in map {
                let text = value
                    .as_str()
                    .ok_or_else(|| SpecError::InvalidArgument(format!("env.{key} 必須是字串")))?;
                env.push((key.clone(), text.to_owned()));
            }
            env.sort();
            env
        }
        Some(_) => return Err(SpecError::InvalidArgument(String::from("env 必須是物件"))),
    };

    // 路徑欄位：必須絕對、存在、型別正確（契約 §4.1）。
    let explicit_working_directory = object
        .get("working_directory")
        .and_then(Value::as_str)
        .is_some();
    let working_directory = match object.get("working_directory") {
        None | Some(Value::Null) => home.join("workspace").join(&name),
        Some(Value::String(raw)) => {
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                return Err(SpecError::InvalidArgument(format!(
                    "working_directory 必須是絕對路徑：{raw}"
                )));
            }
            if !path.exists() {
                return Err(SpecError::InvalidArgument(format!(
                    "working_directory 不存在：{raw}"
                )));
            }
            if !path.is_dir() {
                return Err(SpecError::InvalidArgument(format!(
                    "working_directory 必須是目錄：{raw}"
                )));
            }
            path
        }
        Some(_) => {
            return Err(SpecError::InvalidArgument(String::from(
                "working_directory 必須是字串",
            )))
        }
    };

    let instructions_path = match object.get("instructions") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                return Err(SpecError::InvalidArgument(format!(
                    "instructions 必須是絕對路徑：{raw}"
                )));
            }
            if !path.is_file() {
                return Err(SpecError::InvalidArgument(format!(
                    "instructions 必須是可讀的檔案：{raw}"
                )));
            }
            Some(path)
        }
        Some(_) => {
            return Err(SpecError::InvalidArgument(String::from(
                "instructions 必須是字串",
            )))
        }
    };
    let instructions_text = instructions_path
        .as_ref()
        .map(|path| {
            std::fs::read_to_string(path).map_err(|error| {
                SpecError::InvalidArgument(format!(
                    "instructions 不可讀（{}）：{error}",
                    path.display()
                ))
            })
        })
        .transpose()?;

    let skills_path = match object.get("skills_path") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                return Err(SpecError::InvalidArgument(format!(
                    "skills_path 必須是絕對路徑：{raw}"
                )));
            }
            if !path.exists() {
                return Err(SpecError::InvalidArgument(format!(
                    "skills_path 不存在：{raw}"
                )));
            }
            if !path.is_dir() {
                return Err(SpecError::InvalidArgument(format!(
                    "skills_path 必須是目錄：{raw}"
                )));
            }
            Some(path)
        }
        Some(_) => {
            return Err(SpecError::InvalidArgument(String::from(
                "skills_path 必須是字串",
            )))
        }
    };

    Ok(ValidatedSpec {
        name,
        backend,
        command,
        args: args_list,
        model,
        effort,
        working_directory,
        explicit_working_directory,
        instructions: instructions_path,
        instructions_text,
        skills_path,
        env,
        description,
        role,
    })
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, SpecError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(SpecError::InvalidArgument(format!("{key} 必須是字串"))),
    }
}

/// 由 `ValidatedSpec` 產生存進 SQLite 的 spec JSON（契約 §4.1 的欄位集）。
pub fn spec_json(spec: &ValidatedSpec) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("name".to_owned(), Value::String(spec.name.clone()));
    object.insert("backend".to_owned(), Value::String(spec.backend.clone()));
    if let Some(command) = &spec.command {
        object.insert("command".to_owned(), Value::String(command.clone()));
    }
    object.insert(
        "args".to_owned(),
        Value::Array(spec.args.iter().cloned().map(Value::String).collect()),
    );
    for (key, value) in [
        ("model", &spec.model),
        ("effort", &spec.effort),
        ("description", &spec.description),
        ("role", &spec.role),
    ] {
        if let Some(text) = value {
            object.insert(key.to_owned(), Value::String(text.clone()));
        }
    }
    object.insert(
        "working_directory".to_owned(),
        Value::String(spec.working_directory.to_string_lossy().into_owned()),
    );
    if let Some(path) = &spec.instructions {
        object.insert(
            "instructions".to_owned(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    if let Some(path) = &spec.skills_path {
        object.insert(
            "skills_path".to_owned(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    if !spec.env.is_empty() {
        let mut env = serde_json::Map::new();
        for (key, value) in &spec.env {
            env.insert(key.clone(), Value::String(value.clone()));
        }
        object.insert("env".to_owned(), Value::Object(env));
    }
    Value::Object(object)
}
