use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// 設定檔只允許這兩個頂層鍵。
pub const ALLOWED_TOP_LEVEL_KEYS: [&str; 2] = ["templates", "managers"];

/// 錯誤訊息中列舉允許頂層鍵時使用的文字。
pub const ALLOWED_TOP_LEVEL_TEXT: &str = "templates、managers";

/// 單一 instance 成員在 template 中可用的欄位（契約 §8.2）。
pub const MEMBER_KNOWN_KEYS: [&str; 10] = [
    "role",
    "backend",
    "command",
    "model",
    "effort",
    "args",
    "instructions",
    "skills_path",
    "env",
    "working_directory",
];

/// 成員名稱規則（契約 §4.1）：1–64 字元，只含 `[A-Za-z0-9_-]`，不得為 `operator`。
const RESERVED_NAME: &str = "operator";

/// 解析後的成員，包含成員本身與它不生效的欄位路徑。
struct ParsedMember {
    member: TemplateMember,
    ignored_fields: Vec<String>,
}

/// `templates.<name>` 會採用的頂層鍵；其餘鍵不生效但要列入 ignored fields。
pub const TEMPLATE_KNOWN_KEYS: [&str; 2] = ["orchestrator", "instances"];

/// daemon 讀到的 `$OTTERHIVE_HOME/otterhive.yaml` 內容。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// 可管理此 fleet 的 operator 清單。
    pub managers: Vec<String>,
    /// template 名稱到 template 定義。
    pub templates: BTreeMap<String, Template>,
}

/// 舊 fleet.yaml 格式的 deployment template。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Template {
    /// 負責協調的成員名稱。
    pub orchestrator: Option<String>,
    /// 成員名稱到成員定義。
    pub instances: BTreeMap<String, TemplateMember>,
    /// 不生效但需回報的欄位路徑。
    pub ignored_fields: Vec<String>,
}

/// `templates.<name>.instances.<member>` 的定義。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateMember {
    pub role: Option<String>,
    pub backend: Option<String>,
    pub command: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub args: Option<Vec<String>>,
    pub instructions: Option<String>,
    pub skills_path: Option<String>,
    pub env: BTreeMap<String, String>,
    pub working_directory: Option<String>,
}

/// 載入設定檔失敗的原因。
#[derive(Debug)]
pub enum ConfigError {
    /// 設定檔不存在時等同空設定，因此此變體只涵蓋真正的讀取失敗。
    Io(io::Error),
    /// YAML 格式錯誤。
    Parse { path: String, message: String },
    /// 頂層不是對應到 `templates`／`managers` 的物件。
    NotAMapping { path: String },
    /// 出現不允許的頂層鍵。
    UnknownTopLevelKey { key: String },
    /// 結構或欄位型別不符合契約。
    Invalid { message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(err) => write!(f, "讀取 otterhive.yaml 失敗：{err}"),
            ConfigError::Parse { path, message } => {
                write!(f, "otterhive.yaml 格式錯誤（{path}）：{message}")
            }
            ConfigError::NotAMapping { path } => {
                write!(f, "otterhive.yaml 的 {path} 必須是物件")
            }
            ConfigError::UnknownTopLevelKey { key } => write!(
                f,
                "otterhive.yaml 不允許的頂層鍵 {key}；允許的頂層鍵只有 {ALLOWED_TOP_LEVEL_TEXT}"
            ),
            ConfigError::Invalid { message } => {
                write!(f, "otterhive.yaml 不符合契約：{message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl Config {
    /// 載入 `path`；檔案不存在時回傳空設定。
    ///
    /// 只接受 `templates` 與 `managers` 兩個頂層鍵，其他頂層鍵一律拒絕。
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        // 檔案不存在等同空設定。
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => return Err(ConfigError::Io(err)),
        };

        let document: serde_yaml_ng::Value =
            serde_yaml_ng::from_slice(&raw).map_err(|err| ConfigError::Parse {
                path: path.display().to_string(),
                message: err.to_string(),
            })?;

        let mapping = match document {
            // 空白檔案等同空設定。
            serde_yaml_ng::Value::Null => return Ok(Config::default()),
            serde_yaml_ng::Value::Mapping(mapping) => mapping,
            _ => {
                return Err(ConfigError::NotAMapping {
                    path: "整份設定檔".to_string(),
                })
            }
        };

        let mut managers = Vec::new();
        let mut templates = BTreeMap::new();

        for (key, value) in mapping {
            let key = match key.as_str() {
                Some(key) => key.to_string(),
                None => {
                    return Err(ConfigError::Invalid {
                        message: "頂層鍵必須是字串".to_string(),
                    })
                }
            };

            match key.as_str() {
                "managers" => managers = parse_managers(&value)?,
                "templates" => templates = parse_templates(&value)?,
                _ => return Err(ConfigError::UnknownTopLevelKey { key }),
            }
        }

        Ok(Config {
            managers,
            templates,
        })
    }
}

fn parse_managers(value: &serde_yaml_ng::Value) -> Result<Vec<String>, ConfigError> {
    let sequence = match value {
        serde_yaml_ng::Value::Null => return Ok(Vec::new()),
        serde_yaml_ng::Value::Sequence(sequence) => sequence,
        _ => {
            return Err(ConfigError::Invalid {
                message: "managers 必須是字串陣列".to_string(),
            })
        }
    };

    sequence
        .iter()
        .map(|entry| match entry.as_str() {
            Some(name) => Ok(name.to_string()),
            None => Err(ConfigError::Invalid {
                message: "managers 的每一項都必須是字串".to_string(),
            }),
        })
        .collect()
}

fn parse_templates(
    value: &serde_yaml_ng::Value,
) -> Result<BTreeMap<String, Template>, ConfigError> {
    let mapping = match value {
        serde_yaml_ng::Value::Null => return Ok(BTreeMap::new()),
        serde_yaml_ng::Value::Mapping(mapping) => mapping,
        _ => {
            return Err(ConfigError::Invalid {
                message: "templates 必須是物件".to_string(),
            })
        }
    };

    let mut templates = BTreeMap::new();
    for (key, value) in mapping {
        let name = match key.as_str() {
            Some(name) => name.to_string(),
            None => {
                return Err(ConfigError::Invalid {
                    message: "templates 的鍵必須是字串".to_string(),
                })
            }
        };
        let template = parse_template(&name, value)?;
        templates.insert(name, template);
    }
    Ok(templates)
}

fn parse_template(name: &str, value: &serde_yaml_ng::Value) -> Result<Template, ConfigError> {
    let mapping = match value {
        serde_yaml_ng::Value::Mapping(mapping) => mapping,
        serde_yaml_ng::Value::Null => return Err(invalid_template(name, "template 必須是物件")),
        _ => return Err(invalid_template(name, "template 必須是物件")),
    };

    let mut template = Template::default();
    for (key, value) in mapping {
        let key = match key.as_str() {
            Some(key) => key.to_string(),
            None => return Err(invalid_template(name, "template 的鍵必須是字串")),
        };
        match key.as_str() {
            "orchestrator" => {
                template.orchestrator = value.as_str().map(str::to_string);
                if template.orchestrator.is_none() {
                    return Err(invalid_template(name, "orchestrator 必須是字串"));
                }
            }
            "instances" => {
                let (instances, member_ignored) = parse_instances(name, value)?;
                template.instances = instances;
                // 成員層級的不生效欄位也要回報（契約 §8.2）。
                template.ignored_fields.extend(member_ignored);
            }
            other => {
                // 其他鍵（例如 source_repo、project_id、github_login）不生效，
                // 但必須回報，讓 deploy 結果能列出 ignored_template_fields。
                template
                    .ignored_fields
                    .push(format!("templates.{name}.{other}"));
            }
        }
    }

    // 契約 §8.2：orchestrator 必須是 instances 其中之一；缺漏或不符回 invalid_argument。
    let orchestrator = template.orchestrator.as_deref().ok_or_else(|| {
        invalid_template(name, "orchestrator 必須存在，且必須是 instances 其中之一")
    })?;
    if !template.instances.contains_key(orchestrator) {
        return Err(invalid_template(
            name,
            &format!("orchestrator {orchestrator} 不在 instances 的成員之中"),
        ));
    }

    Ok(template)
}

fn parse_instances(
    template_name: &str,
    value: &serde_yaml_ng::Value,
) -> Result<(BTreeMap<String, TemplateMember>, Vec<String>), ConfigError> {
    let mapping = match value {
        serde_yaml_ng::Value::Mapping(mapping) => mapping,
        // 契約 §8.2：instances 必須是非空物件。
        _ => return Err(invalid_template(template_name, "instances 必須是非空物件")),
    };
    if mapping.is_empty() {
        return Err(invalid_template(template_name, "instances 必須是非空物件"));
    }

    let mut instances = BTreeMap::new();
    let mut ignored_fields = Vec::new();
    for (key, value) in mapping {
        let member_name = match key.as_str() {
            Some(member_name) => member_name.to_string(),
            None => {
                return Err(invalid_template(
                    template_name,
                    "instances 的成員名稱必須是字串",
                ))
            }
        };
        // 契約 §8.2：member key 遵守 §4.1 名稱規則。
        validate_member_name(template_name, &member_name)?;
        let parsed = parse_member(template_name, &member_name, value)?;
        ignored_fields.extend(parsed.ignored_fields);
        instances.insert(member_name, parsed.member);
    }
    Ok((instances, ignored_fields))
}

/// 契約 §8.2：member key 遵守 §4.1 名稱規則。
fn validate_member_name(template_name: &str, member_name: &str) -> Result<(), ConfigError> {
    let valid_charset = !member_name.is_empty()
        && member_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    let too_long = member_name.chars().count() > 64;
    if !valid_charset {
        return Err(invalid_member(
            template_name,
            member_name,
            "成員名稱只能含 [A-Za-z0-9_-] 且長度為 1–64",
        ));
    }
    if too_long {
        return Err(invalid_member(
            template_name,
            member_name,
            "成員名稱長度不得超過 64 字元",
        ));
    }
    if member_name == RESERVED_NAME {
        return Err(invalid_member(
            template_name,
            member_name,
            "成員名稱不得為 operator",
        ));
    }
    Ok(())
}

fn parse_member(
    template_name: &str,
    member_name: &str,
    value: &serde_yaml_ng::Value,
) -> Result<ParsedMember, ConfigError> {
    let mapping = match value {
        serde_yaml_ng::Value::Mapping(mapping) => mapping,
        serde_yaml_ng::Value::Null => {
            return Err(invalid_member(
                template_name,
                member_name,
                "成員 spec 必須是物件",
            ))
        }
        _ => {
            return Err(invalid_member(
                template_name,
                member_name,
                "成員 spec 必須是物件",
            ))
        }
    };

    let mut member = TemplateMember::default();
    let mut ignored_fields = Vec::new();
    for (key, value) in mapping {
        let key = match key.as_str() {
            Some(key) => key.to_string(),
            None => {
                return Err(invalid_member(
                    template_name,
                    member_name,
                    "成員 spec 的鍵必須是字串",
                ))
            }
        };
        match key.as_str() {
            "role" => member.role = optional_string(value),
            "backend" => member.backend = optional_string(value),
            "command" => member.command = optional_string(value),
            "model" => member.model = optional_string(value),
            "effort" => member.effort = optional_string(value),
            "instructions" => member.instructions = optional_string(value),
            "skills_path" => member.skills_path = optional_string(value),
            "working_directory" => member.working_directory = optional_string(value),
            "args" => member.args = optional_string_list(value),
            "env" => member.env = parse_env(value)?,
            other => ignored_fields.push(format!(
                "templates.{template_name}.instances.{member_name}.{other}"
            )),
        }
    }

    Ok(ParsedMember {
        member,
        ignored_fields,
    })
}

fn parse_env(value: &serde_yaml_ng::Value) -> Result<BTreeMap<String, String>, ConfigError> {
    match value {
        serde_yaml_ng::Value::Null => Ok(BTreeMap::new()),
        serde_yaml_ng::Value::Mapping(mapping) => {
            let mut env = BTreeMap::new();
            for (key, value) in mapping {
                let key = match key.as_str() {
                    Some(key) => key.to_string(),
                    None => {
                        return Err(ConfigError::Invalid {
                            message: "env 的鍵必須是字串".to_string(),
                        })
                    }
                };
                let rendered = match value {
                    serde_yaml_ng::Value::String(text) => text.clone(),
                    serde_yaml_ng::Value::Bool(flag) => flag.to_string(),
                    serde_yaml_ng::Value::Number(number) => number.to_string(),
                    _ => {
                        return Err(ConfigError::Invalid {
                            message: "env 的值必須是純量".to_string(),
                        })
                    }
                };
                env.insert(key, rendered);
            }
            Ok(env)
        }
        _ => Err(ConfigError::Invalid {
            message: "env 必須是物件".to_string(),
        }),
    }
}

fn optional_string(value: &serde_yaml_ng::Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn optional_string_list(value: &serde_yaml_ng::Value) -> Option<Vec<String>> {
    match value {
        serde_yaml_ng::Value::Null => Some(Vec::new()),
        serde_yaml_ng::Value::Sequence(sequence) => Some(
            sequence
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

fn invalid_template(name: &str, message: &str) -> ConfigError {
    ConfigError::Invalid {
        message: format!("templates.{name}：{message}"),
    }
}

fn invalid_member(template_name: &str, member_name: &str, message: &str) -> ConfigError {
    ConfigError::Invalid {
        message: format!("templates.{template_name}.instances.{member_name}：{message}"),
    }
}

/// 設定檔允許的頂層鍵；deploy 與 daemon 的錯誤訊息共用同一份清單。
pub fn allowed_top_level_keys() -> &'static [&'static str; 2] {
    &ALLOWED_TOP_LEVEL_KEYS
}

/// 成員會採用的欄位（契約 §8.2）。
pub fn member_known_keys() -> &'static [&'static str; 10] {
    &MEMBER_KNOWN_KEYS
}

/// template 頂層會採用的鍵。
pub fn template_known_keys() -> &'static [&'static str; 2] {
    &TEMPLATE_KNOWN_KEYS
}
