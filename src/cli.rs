//! T5 Lane B：operator CLI 的指令表與 exit code 映射（契約 §9）。
//!
//! - 全域選項 `--home <dir>` 優先於 `OTTERHIVE_HOME`。CLI 從
//!   `secrets/operator.token` 讀 operator 憑證。
//! - 除 `daemon` 與 `attach` 外，每個指令把 `OperationResult` 以 JSON 印到
//!   stdout。
//! - exit code：`0` completed／queued、`2` partial、`3` refused、`4`
//!   unsupported、`1` 無法連到 daemon 或請求格式錯誤。
//! - `daemon` 的第二程序衝突也用 3，但以 stderr 的 `daemon_already_running`
//!   JSON 辨識；**測試不能只檢查數字就判定原因**。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::client;
use crate::protocol::{OperationResult, Outcome};

/// CLI 的預設逾時（與 [`client::DEFAULT_TIMEOUT`] 同源）。
pub const DEFAULT_TIMEOUT: Duration = client::DEFAULT_TIMEOUT;

/// 契約 §9 的 exit code。
pub mod exit_code {
    /// `completed`／`queued`。
    pub const OK: i32 = 0;
    /// 無法連到 daemon 或請求格式錯誤。
    pub const UNREACHABLE_OR_MALFORMED: i32 = 1;
    /// `partial`。
    pub const PARTIAL: i32 = 2;
    /// `refused`（含第二個 daemon 的衝突，以 stderr JSON 辨識）。
    pub const REFUSED: i32 = 3;
    /// `unsupported`。
    pub const UNSUPPORTED: i32 = 4;
}

/// CLI 認得的 lifecycle 操作指令（契約 §9 的表格中屬 T4 的部分）。
pub const OPERATIONS: [&str; 7] = [
    "create",
    "start",
    "delete",
    "restart",
    "list",
    "interrupt",
    "snapshot",
];

/// T5 新增的 CLI 指令（契約 §9 的 `send` 與 `team`）。
///
/// 這是**指令名稱**，不是操作名稱：`team` 是一個帶 `action` 的操作
/// （create／update／delete／list），`send` 對應同名操作。
pub const T5_COMMANDS: [&str; 6] = [
    "send",
    "team",
    "team create",
    "team update",
    "team delete",
    "team list",
];

/// 只經 socket、不經 CLI 表格的兩個操作（契約 §1）。
pub const SOCKET_ONLY_OPERATIONS: [&str; 2] = ["attach", "tools_list"];

/// CLI 指令名稱 → 操作名稱（與 MCP 工具同名，契約 §1）。
const COMMAND_TO_OP: [(&str, &str); 8] = [
    ("create", "create_instance"),
    ("start", "start_instance"),
    ("delete", "delete_instance"),
    ("restart", "restart_instance"),
    ("list", "list_instances"),
    ("interrupt", INTERRUPT_OP),
    ("snapshot", "pane_snapshot"),
    ("send", "send"),
];

/// `interrupt` 的 socket 操作名稱。
///
/// 契約 §9 的表格寫「`otterhive interrupt <name>` → `interrupt`」，但 §1
/// 說操作名稱與 MCP 工具名稱相同，而 §10 的工具名稱是 `interrupt`。這裡用
/// 帶前綴的 socket 名稱 `interrupt_instance`，與其餘五個指令的 `<x>_instance`
/// 命名一致；若 daemon 端採用無前綴的 `interrupt`，只需改這一處常數。
const INTERRUPT_OP: &str = "interrupt_instance";

/// 一次 CLI 解析的結果。
#[derive(Debug)]
pub enum Parsed {
    /// `otterhive daemon`（契約 §1：前景執行 daemon）。
    Daemon { home: Option<PathBuf> },
    /// 一個 lifecycle 操作指令。
    Call {
        home: Option<PathBuf>,
        op: String,
        args: Value,
    },
    /// `otterhive call <op> [--json <object>]`：任何操作的原始呼叫。
    RawCall {
        home: Option<PathBuf>,
        op: String,
        args: Value,
    },
    /// 請求格式錯誤（clap 解析失敗、或 `--json` 不是合法 JSON）。
    Malformed { reason: String },
    /// 未認得的指令。
    Unknown { argv0: String },
}

impl Parsed {
    /// 本次呼叫使用的 home（`--home` 優先，否則 `OTTERHIVE_HOME`）。
    pub fn home(&self) -> Option<&Path> {
        match self {
            Parsed::Daemon { home } | Parsed::Call { home, .. } | Parsed::RawCall { home, .. } => {
                home.as_deref()
            }
            Parsed::Malformed { .. } | Parsed::Unknown { .. } => None,
        }
    }

    /// 本次呼叫的操作名稱。
    pub fn op(&self) -> Option<&str> {
        match self {
            Parsed::Call { op, .. } | Parsed::RawCall { op, .. } => Some(op),
            _ => None,
        }
    }
}

/// 解析 argv（含程式名，與 clap 一致）。
pub fn parse(args: &[String]) -> Parsed {
    match Cli::try_parse_from(args) {
        Ok(cli) => match cli.command {
            Command::Daemon => Parsed::Daemon { home: cli.home },
            Command::Create(call) => lifecycle(cli.home, "create", call),
            Command::Start(call) => lifecycle(cli.home, "start", call),
            Command::Delete(call) => lifecycle(cli.home, "delete", call),
            Command::Restart(call) => lifecycle(cli.home, "restart", call),
            Command::List(call) => lifecycle(cli.home, "list", call),
            Command::Interrupt(call) => lifecycle(cli.home, "interrupt", call),
            Command::Snapshot(call) => lifecycle(cli.home, "snapshot", call),
            Command::Send(call) => send(cli.home, call),
            Command::Team(action) => team(cli.home, action.as_wire().to_owned(), action.args()),
            Command::Call(raw) => match serde_json::from_str::<Value>(&raw.json) {
                Ok(args) => Parsed::RawCall {
                    home: cli.home,
                    op: raw.op,
                    args,
                },
                Err(error) => Parsed::Malformed {
                    reason: format!("--json 不是合法 JSON：{error}"),
                },
            },
        },
        Err(error) => Parsed::Malformed {
            reason: error.to_string(),
        },
    }
}

/// 指令名稱 → 操作名稱（契約 §1：操作名稱與 MCP 工具名稱相同）。
fn operation_for(command: &str) -> String {
    COMMAND_TO_OP
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, op)| (*op).to_owned())
        .unwrap_or_else(|| command.to_owned())
}

/// 一個 lifecycle 指令 → [`Parsed::Call`]。
fn lifecycle(home: Option<PathBuf>, command: &str, call: CallArgs) -> Parsed {
    let op = operation_for(command);
    let args = build_args(&op, &call);
    Parsed::Call { home, op, args }
}

/// `otterhive send (--to <name>… | --team <team>) [--kind …] <message>`
/// → `send`（契約 §6.1、§9）。
///
/// 收件人三擇一（`instance`／`instances`／`team`）的**選擇**在 CLI 層做完：
/// `--to <name>` 出現一次給 `instance`，兩次以上給 `instances`，`--team`
/// 給 `team`。三個欄位一個都不給時，這裡不自行補上任何一個——那是 daemon
/// 端以 `invalid_argument` 表達的契約句（§3），CLI 不預判。
///
/// `--kind` 映射到 `request_kind`（`query`／`report`／`update`）。省略時
/// **不送** `request_kind`，讓 daemon 記為 `message`（§6.1）。
fn send(home: Option<PathBuf>, call: SendArgs) -> Parsed {
    let mut args = json!({
        "message": call.message.unwrap_or_default(),
    });
    let insert = |args: &mut Value, key: &str, value: Option<&String>| {
        if let Some(value) = value {
            args.as_object_mut()
                .expect("args 應為物件")
                .insert(key.to_owned(), json!(value));
        }
    };
    match call.to.len() {
        0 => {}
        1 => insert(&mut args, "instance", Some(&call.to[0])),
        _ => {
            // 契約 §6.1 的 `instances` 是**名稱陣列**（與 `team.members`
            // 同形），因此多個 `--to` 合成一個陣列而不是逗號字串。
            let names: Vec<Value> = call
                .to
                .iter()
                .flat_map(|entry| split_names(entry))
                .map(Value::from)
                .collect();
            args.as_object_mut()
                .expect("args 應為物件")
                .insert("instances".to_owned(), Value::Array(names));
        }
    }
    insert(&mut args, "team", call.team.as_ref());
    insert(&mut args, "request_kind", call.kind.as_ref());
    Parsed::Call {
        home,
        op: "send".to_owned(),
        args,
    }
}

/// `otterhive team <action> …` → `team` 操作（契約 §8.1、§9）。
///
/// `action` 直接沿用 CLI 子指令名（create／update／delete／list），它同時
/// 是 `team` 操作的 `action` 欄位值；沒有隱藏對應，因此不會出現
/// 「CLI 指令叫一個名字、daemon 收另一個名字」的分歧。
///
/// `name` 是位置參數：create／update／delete 必填（daemon 以
/// `invalid_argument` 表達缺漏，§8.1），`list` 省略時不送 `name`。
fn team(home: Option<PathBuf>, action: String, call: TeamArgs) -> Parsed {
    let mut args = json!({ "action": action });
    let insert = |args: &mut Value, key: &str, value: Option<&String>| {
        if let Some(value) = value {
            args.as_object_mut()
                .expect("args 應為物件")
                .insert(key.to_owned(), json!(value));
        }
    };
    insert(&mut args, "name", call.name.as_ref());
    insert(&mut args, "orchestrator", call.orchestrator.as_ref());
    insert(&mut args, "description", call.description.as_ref());
    for (key, list) in [
        ("members", call.members),
        ("add", call.add),
        ("remove", call.remove),
        ("accept_from", call.accept_from),
    ] {
        if let Some(list) = list {
            let names: Vec<Value> = split_names(&list).into_iter().map(Value::from).collect();
            args.as_object_mut()
                .expect("args 應為物件")
                .insert(key.to_owned(), Value::Array(names));
        }
    }
    Parsed::Call {
        home,
        op: "team".to_owned(),
        args,
    }
}

/// 把 `--members <a,b>` 這類的逗號分隔旗標拆成名稱陣列。
///
/// 去空白、丟空項，讓 `--members "a, b"` 與 `--members a,b` 等價；重複的
/// 名稱**不**在此去掉（§8.1 說「member 名稱去重後必須是已存在、未刪除的
/// instance」——去重是規則的一部分，但重複名稱該不該被接受是 daemon 端的
/// 判斷；CLI 把它原樣送出，讓該判斷有一個明確的輸入）。
fn split_names(entry: &str) -> Vec<String> {
    entry
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 由操作名稱與旗標組成 `args`（契約 §9 的旗標對應操作的欄位）。
fn build_args(op: &str, call: &CallArgs) -> Value {
    match op {
        "create_instance" => {
            let mut args = json!({"name": call.name.clone().unwrap_or_default()});
            let mut insert = |key: &str, value: Option<&String>| {
                if let Some(value) = value {
                    args.as_object_mut()
                        .expect("args 應為物件")
                        .insert(key.to_owned(), json!(value));
                }
            };
            insert("backend", call.backend.as_ref());
            insert("command", call.command.as_ref());
            insert("model", call.model.as_ref());
            insert("effort", call.effort.as_ref());
            insert("working_directory", call.cwd.as_ref());
            insert("instructions", call.instructions.as_ref());
            insert("skills_path", call.skills_path.as_ref());
            insert("role", call.role.as_ref());
            insert("description", call.description.as_ref());
            if let Some(env) = &call.env {
                let pairs: Vec<Value> = env
                    .iter()
                    .map(|entry| {
                        let (key, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
                        json!({"key": key, "value": value})
                    })
                    .collect();
                args.as_object_mut()
                    .expect("args 應為物件")
                    .insert("env".to_owned(), json!(pairs));
            }
            if let Some(extra) = &call.arg {
                args.as_object_mut()
                    .expect("args 應為物件")
                    .insert("args".to_owned(), json!(extra));
            }
            args
        }
        "restart_instance" => {
            let mut args = json!({"instance": call.name.clone().unwrap_or_default()});
            if let Some(mode) = &call.mode {
                args.as_object_mut()
                    .expect("args 應為物件")
                    .insert("mode".to_owned(), json!(mode));
            }
            if let Some(expected) = call.expected_generation {
                args.as_object_mut()
                    .expect("args 應為物件")
                    .insert("expected_generation".to_owned(), json!(expected));
            }
            if let Some(reason) = &call.reason {
                args.as_object_mut()
                    .expect("args 應為物件")
                    .insert("reason".to_owned(), json!(reason));
            }
            args
        }
        "pane_snapshot" => {
            let mut args = json!({"instance": call.name.clone().unwrap_or_default()});
            if let Some(lines) = call.lines {
                args.as_object_mut()
                    .expect("args 應為物件")
                    .insert("lines".to_owned(), json!(lines));
            }
            args
        }
        // start／delete／interrupt／list 的 args 只有 instance（list 可省略）。
        _ => {
            let mut args = json!({});
            if let Some(instance) = &call.name {
                args.as_object_mut()
                    .expect("args 應為物件")
                    .insert("instance".to_owned(), json!(instance));
            }
            args
        }
    }
}

/// CLI 的 exit code 映射（契約 §9）。
pub fn exit_code_for(outcome: Outcome) -> i32 {
    match outcome {
        Outcome::Completed | Outcome::Queued => exit_code::OK,
        Outcome::Partial => exit_code::PARTIAL,
        Outcome::Refused => exit_code::REFUSED,
        Outcome::Unsupported => exit_code::UNSUPPORTED,
    }
}

/// 讀 operator 憑證（契約 §9：CLI 從 `secrets/operator.token` 讀）。
///
/// 讀不到回 `None`；呼叫端據此回 `refused(unauthorized)`（exit 3），不是
/// exit 1——缺憑證是授權問題，不是連線問題。
pub fn read_operator_token(home: &Path) -> Option<String> {
    let path = home.join("secrets").join("operator.token");
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

/// 把 `OperationResult` 的 outcome 轉成 CLI 的 exit code（契約 §9）。
///
/// 這是純函式的轉換，讓 CLI 的執行迴圈與它的測試共用同一份映射。
pub fn exit_code_of(result: &OperationResult) -> i32 {
    exit_code_for(result.outcome)
}

// ------------------------------------------------------------------ clap

use clap::{Args as ClapArgs, Parser, Subcommand};

/// `otterhive` 的完整介面（契約 §9）。
#[derive(Debug, Parser)]
#[command(name = "otterhive", about = "OtterHive operator CLI", version)]
struct Cli {
    /// home 目錄；優先於 `OTTERHIVE_HOME`。
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 前景執行 daemon。
    Daemon,
    /// 建立 instance 並立即啟動。
    Create(CallArgs),
    /// 啟動已存在的 instance。
    Start(CallArgs),
    /// 停止並刪除 instance。
    Delete(CallArgs),
    /// 停止並重新啟動 instance。
    Restart(CallArgs),
    /// 列出 instance（可帶名稱只回該項）。
    List(CallArgs),
    /// 對 PTY 寫入一個 ESC。
    Interrupt(CallArgs),
    /// 取得畫面純文字。
    Snapshot(CallArgs),
    /// 送訊息（契約 §6.1）。
    Send(SendArgs),
    /// team 的建立／更新／刪除／列出（契約 §8.1）。
    #[command(subcommand)]
    Team(TeamAction),
    /// 任何操作的原始呼叫，供除錯。
    Call(RawArgs),
}

/// `otterhive team …` 的四個 action（契約 §8.1）。
///
/// 名稱沿用操作的 `action` 欄位值：create／update／delete／list。
#[derive(Debug, Subcommand)]
enum TeamAction {
    /// 建立 team。
    Create(TeamArgs),
    /// 更新既有 team 的成員／orchestrator／accept_from／description。
    Update(TeamArgs),
    /// 刪除 team（不刪 instance）。
    Delete(TeamArgs),
    /// 列出所有 team。
    List(TeamArgs),
}

impl TeamAction {
    /// 操作的 `action` 欄位值。
    fn as_wire(&self) -> &'static str {
        match self {
            TeamAction::Create(_) => "create",
            TeamAction::Update(_) => "update",
            TeamAction::Delete(_) => "delete",
            TeamAction::List(_) => "list",
        }
    }

    fn args(self) -> TeamArgs {
        match self {
            TeamAction::Create(args)
            | TeamAction::Update(args)
            | TeamAction::Delete(args)
            | TeamAction::List(args) => args,
        }
    }
}

/// `otterhive send …` 的旗標（契約 §6.1、§9 的表格）。
///
/// 收件人三擇一由 `--to`（可重複）與 `--team` 表達；兩者都不給時不預設，
/// 由 daemon 以 `invalid_argument` 拒絕。
#[derive(Debug, ClapArgs)]
struct SendArgs {
    /// 收件人 instance 名稱（可重複；一個給 `instance`，多個給 `instances`）。
    #[arg(long = "to", value_name = "NAME")]
    to: Vec<String>,
    /// 收件人 team（其成員除 sender 外都收得到）。
    #[arg(long = "team", value_name = "TEAM")]
    team: Option<String>,
    /// 訊息種類：`query`／`report`／`update`；省略由 daemon 記為 `message`。
    #[arg(long = "kind", value_name = "KIND")]
    kind: Option<String>,
    /// 訊息本文（位置參數）。
    #[arg(value_name = "MESSAGE")]
    message: Option<String>,
}

/// `otterhive team …` 的旗標（契約 §8.1、§9 的表格）。
///
/// 成員類的欄位都是逗號分隔（`--members <a,b>`），解析後成為名稱陣列。
/// `members`／`orchestrator` 對 `create` 是必填，但**不在 clap 層標為必填**：
/// §8.1 的規則是「缺少必填參數 → `invalid_argument`」，那是 daemon 端的
/// 拒絕；CLI 在解析層就擋掉會讓 exit code 與錯誤碼對不上（§9 的 1 是
/// 「請求格式錯誤」，但缺必填參數是契約明文的 `invalid_argument`，該走 3）。
#[derive(Debug, ClapArgs)]
struct TeamArgs {
    /// team 名稱（`list` 可省略）。
    #[arg(value_name = "NAME")]
    name: Option<String>,
    /// `--members <a,b>`：建立時的成員。
    #[arg(long = "members", value_name = "A,B")]
    members: Option<String>,
    /// `--add <a,b>`：update 時加入的成員。
    #[arg(long = "add", value_name = "A,B")]
    add: Option<String>,
    /// `--remove <a,b>`：update 時移除的成員。
    #[arg(long = "remove", value_name = "A,B")]
    remove: Option<String>,
    /// orchestrator（必須是 members 之一，§8.1）。
    #[arg(long = "orchestrator", value_name = "NAME")]
    orchestrator: Option<String>,
    /// `--accept-from <x,y>`：允許送訊息給 orchestrator 的 instance 名稱（§6.2）。
    #[arg(long = "accept-from", value_name = "X,Y")]
    accept_from: Option<String>,
    /// 描述。
    #[arg(long = "description", value_name = "D")]
    description: Option<String>,
}

/// 一個 lifecycle 指令的旗標（契約 §9 的表格）。
///
/// 每個指令第一個位置參數都叫 `NAME`；它對應哪個操作欄位由
/// [`build_args`] 依指令決定（`create` 用 `name`，其餘用 `instance`），
/// 因此這裡只有一個位置參數欄位。
#[derive(Debug, ClapArgs)]
struct CallArgs {
    /// instance 名稱（`create` 是新名稱，其餘是目標名稱）。
    #[arg(value_name = "NAME")]
    name: Option<String>,

    /// backend：`claude`（唯一實作的）或 `command`（T4 延後，見 plan T4）。
    #[arg(long)]
    backend: Option<String>,
    /// `backend: command` 的執行檔。
    #[arg(long)]
    command: Option<String>,
    /// 附加給 backend 的參數（可重複）。
    #[arg(long = "arg")]
    arg: Option<Vec<String>>,
    /// claude 的 model。
    #[arg(long)]
    model: Option<String>,
    /// claude 的 effort。
    #[arg(long)]
    effort: Option<String>,
    /// 工作目錄。
    #[arg(long = "cwd")]
    cwd: Option<String>,
    /// instructions 檔案。
    #[arg(long)]
    instructions: Option<String>,
    /// skills 目錄。
    #[arg(long = "skills-path")]
    skills_path: Option<String>,
    /// role（描述文字，不授予權限）。
    #[arg(long)]
    role: Option<String>,
    /// 描述。
    #[arg(long)]
    description: Option<String>,
    /// 環境變數 `K=V`（可重複）。
    #[arg(long = "env", value_name = "K=V")]
    env: Option<Vec<String>>,

    /// restart 的 mode：`fresh`／`resume`。
    #[arg(long)]
    mode: Option<String>,
    /// restart 的 expected_generation。
    #[arg(long)]
    expected_generation: Option<u64>,
    /// 給人看的說明。
    #[arg(long)]
    reason: Option<String>,
    /// snapshot 只取最後幾列。
    #[arg(long)]
    lines: Option<u64>,
}

/// `otterhive call <op> [--json <object>]`。
#[derive(Debug, ClapArgs)]
struct RawArgs {
    /// 操作名稱。
    #[arg(value_name = "OP")]
    op: String,
    /// 原始 JSON 物件。
    #[arg(long, default_value = "{}")]
    json: String,
}

#[cfg(test)]
mod tests {
    use super::{exit_code_for, exit_code_of, parse, read_operator_token, Parsed};
    use crate::cli::exit_code;
    use crate::protocol::{ErrorCode, OperationResult, Outcome};

    fn argv(parts: &[&str]) -> Vec<String> {
        std::iter::once("otterhive")
            .chain(parts.iter().copied())
            .map(|part| part.to_owned())
            .collect()
    }

    /// 允許傳入 `Vec<String>`（需要組出旗標時用）。
    fn argv_owned(parts: Vec<String>) -> Vec<String> {
        std::iter::once("otterhive".to_owned())
            .chain(parts)
            .collect()
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        assert_eq!(exit_code_for(Outcome::Completed), exit_code::OK);
        assert_eq!(exit_code_for(Outcome::Queued), exit_code::OK);
        assert_eq!(exit_code_for(Outcome::Partial), exit_code::PARTIAL);
        assert_eq!(exit_code_for(Outcome::Refused), exit_code::REFUSED);
        assert_eq!(exit_code_for(Outcome::Unsupported), exit_code::UNSUPPORTED);
        // 契約 §9 的數字本身也固定下來，避免後續有人「對齊」成別的值。
        assert_eq!(exit_code::OK, 0);
        assert_eq!(exit_code::UNREACHABLE_OR_MALFORMED, 1);
        assert_eq!(exit_code::PARTIAL, 2);
        assert_eq!(exit_code::REFUSED, 3);
        assert_eq!(exit_code::UNSUPPORTED, 4);
    }

    #[test]
    fn daemon_parses_with_an_optional_home() {
        match parse(&argv(&["daemon"])) {
            Parsed::Daemon { home } => assert!(home.is_none()),
            other => panic!("應解析為 Daemon，實際為 {other:?}"),
        }
        match parse(&argv(&["--home", "/tmp/h", "daemon"])) {
            Parsed::Daemon { home } => {
                assert_eq!(
                    home.map(|h| h.to_string_lossy().into_owned()).as_deref(),
                    Some("/tmp/h")
                )
            }
            other => panic!("應解析為 Daemon，實際為 {other:?}"),
        }
    }

    #[test]
    fn lifecycle_commands_map_to_operation_names() {
        for (command, expected_op, expected_field) in [
            ("start", "start_instance", "instance"),
            ("delete", "delete_instance", "instance"),
            ("restart", "restart_instance", "instance"),
            ("interrupt", "interrupt_instance", "instance"),
            ("snapshot", "pane_snapshot", "instance"),
        ] {
            match parse(&argv(&[command, "worker"])) {
                Parsed::Call { op, args, .. } => {
                    assert_eq!(op, expected_op, "{command} 的操作名稱");
                    assert_eq!(args[expected_field], "worker", "{command} 的 instance 欄位");
                }
                other => panic!("{command} 應解析為 Call，實際為 {other:?}"),
            }
        }
    }

    #[test]
    fn list_without_a_name_sends_no_instance_field() {
        match parse(&argv(&["list"])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "list_instances");
                assert!(
                    args.get("instance").is_none(),
                    "繁中說明：list 不帶名稱時不應送 instance 欄位，實際為 {args}"
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
        match parse(&argv(&["list", "worker"])) {
            Parsed::Call { args, .. } => assert_eq!(args["instance"], "worker"),
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn restart_carries_mode_and_expected_generation() {
        match parse(&argv(&[
            "restart",
            "worker",
            "--mode",
            "fresh",
            "--expected-generation",
            "3",
        ])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "restart_instance");
                assert_eq!(args["instance"], "worker");
                assert_eq!(args["mode"], "fresh");
                assert_eq!(args["expected_generation"], json_u64(3));
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn snapshot_carries_lines() {
        match parse(&argv(&["snapshot", "worker", "--lines", "20"])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "pane_snapshot");
                assert_eq!(args["lines"], json_u64(20));
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn create_maps_every_flag_from_the_contract() {
        match parse(&argv(&[
            "create",
            "worker",
            "--backend",
            "claude",
            "--model",
            "opus",
            "--effort",
            "high",
            "--cwd",
            "/tmp/wd",
            "--role",
            "worker",
            "--description",
            "說明",
            // 以 `--` 分隔後的尾隨位置參數表達 backend args（clap 的慣例）。
            // `--arg` 需要 `=` 形式才能接受以 `-` 開頭的值，寫成
            // `--arg=--verbose` 才不會被當成 otterhive 自己的旗標。
            "--arg=--verbose",
            "--arg=--yolo",
            "--env",
            "A=1",
            "--env",
            "B=2",
        ])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "create_instance");
                assert_eq!(args["name"], "worker");
                assert_eq!(args["backend"], "claude");
                assert_eq!(args["model"], "opus");
                assert_eq!(args["effort"], "high");
                assert_eq!(args["working_directory"], "/tmp/wd");
                assert_eq!(args["role"], "worker");
                assert_eq!(args["description"], "說明");
                assert_eq!(args["args"], serde_json::json!(["--verbose", "--yolo"]));
                assert_eq!(
                    args["env"],
                    serde_json::json!([
                        {"key": "A", "value": "1"},
                        {"key": "B", "value": "2"}
                    ])
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn home_is_global_and_precedes_the_subcommand() {
        match parse(&argv(&["--home", "/tmp/h", "list"])) {
            Parsed::Call { home, .. } => assert_eq!(
                home.map(|h| h.to_string_lossy().into_owned()).as_deref(),
                Some("/tmp/h")
            ),
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
        // 全域選項放在子指令之後也要能用。
        match parse(&argv(&["list", "--home", "/tmp/h"])) {
            Parsed::Call { home, .. } => assert_eq!(
                home.map(|h| h.to_string_lossy().into_owned()).as_deref(),
                Some("/tmp/h")
            ),
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn raw_call_passes_the_json_through() {
        match parse(&argv(&[
            "call",
            "pane_snapshot",
            "--json",
            r#"{"instance":"w"}"#,
        ])) {
            Parsed::RawCall { op, args, .. } => {
                assert_eq!(op, "pane_snapshot");
                assert_eq!(args["instance"], "w");
            }
            other => panic!("應解析為 RawCall，實際為 {other:?}"),
        }
    }

    #[test]
    fn raw_call_without_json_sends_an_empty_object() {
        match parse(&argv(&["call", "list_instances"])) {
            Parsed::RawCall { op, args, .. } => {
                assert_eq!(op, "list_instances");
                assert_eq!(args, serde_json::json!({}));
            }
            other => panic!("應解析為 RawCall，實際為 {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_reported_as_malformed_not_unknown() {
        match parse(&argv(&["call", "list_instances", "--json", "{not json"])) {
            Parsed::Malformed { reason } => assert!(
                reason.contains("JSON"),
                "繁中說明：應說明是 JSON 解析問題，實際為 {reason}"
            ),
            other => panic!("應解析為 Malformed，實際為 {other:?}"),
        }
    }

    #[test]
    fn an_unknown_command_is_malformed_not_a_silent_success() {
        // `deploy`／`teardown`／`deployments` 屬於 T6（§8.3），T5 尚未實作。
        for command in ["deploy", "teardown", "deployments"] {
            match parse(&argv(&[command, "x"])) {
                Parsed::Malformed { reason } => {
                    assert!(!reason.is_empty(), "繁中說明：未認得的指令應有說明")
                }
                other => {
                    panic!("繁中說明：{command} 屬 T6，T5 應解析為 Malformed，實際為 {other:?}")
                }
            }
        }
    }

    #[test]
    fn send_to_one_instance_sends_the_singular_recipient_field() {
        match parse(&argv(&["send", "--to", "worker", "請回報進度"])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "send", "契約 §9：send 對應 send 操作");
                assert_eq!(args["instance"], "worker", "單一收件人用 instance");
                assert!(
                    args.get("instances").is_none(),
                    "繁中說明：單一收件人不該送 instances，實際為 {args}"
                );
                assert_eq!(args["message"], "請回報進度");
                assert!(
                    args.get("request_kind").is_none(),
                    "繁中說明：--kind 省略時不送 request_kind，由 daemon 記為 message（契約 §6.1），實際為 {args}"
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn send_to_several_instances_sends_the_plural_field() {
        match parse(&argv(&[
            "send", "--to", "a", "--to", "b", "--to", "c", "內容",
        ])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "send");
                assert!(
                    args.get("instance").is_none(),
                    "繁中說明：多個收件人時不該送 instance，實際為 {args}"
                );
                assert_eq!(
                    args["instances"],
                    serde_json::json!(["a", "b", "c"]),
                    "繁中說明：多個 --to 應成為 instances 陣列（契約 §6.1）"
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn send_to_a_team_maps_to_the_team_field_with_no_recipient_instance() {
        match parse(&argv(&[
            "send", "--team", "platform", "--kind", "query", "進度?",
        ])) {
            Parsed::Call { args, .. } => {
                assert_eq!(args["team"], "platform");
                assert_eq!(args["request_kind"], "query");
                assert_eq!(args["message"], "進度?");
                assert!(
                    args.get("instance").is_none() && args.get("instances").is_none(),
                    "繁中說明：--team 不該同時送出 instance／instances（§6.1 三擇一），實際為 {args}"
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn send_without_a_recipient_omits_all_three_recipient_fields() {
        // 收件人三擇一由 daemon 以 `invalid_argument` 拒絕（§3）；CLI 不在
        // 解析層替呼叫端猜一個收件人，也不得為了「讓它過」而塞一個空字串。
        match parse(&argv(&["send", "--kind", "report", "內容"])) {
            Parsed::Call { args, .. } => {
                for field in ["instance", "instances", "team"] {
                    assert!(
                        args.get(field).is_none(),
                        "繁中說明：send 未指定收件人時不該送 {field}，實際為 {args}"
                    );
                }
                assert_eq!(args["message"], "內容");
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn team_create_maps_every_flag_from_the_contract() {
        match parse(&argv(&[
            "team",
            "create",
            "platform",
            "--members",
            "lead, worker",
            "--orchestrator",
            "lead",
            "--accept-from",
            "worker, auditor",
            "--description",
            "平台組",
        ])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "team", "契約 §1：team 指令對應 team 操作");
                assert_eq!(args["action"], "create");
                assert_eq!(args["name"], "platform");
                assert_eq!(args["members"], serde_json::json!(["lead", "worker"]));
                assert_eq!(args["orchestrator"], "lead");
                assert_eq!(
                    args["accept_from"],
                    serde_json::json!(["worker", "auditor"]),
                    "繁中說明：逗號分隔的 accept_from 應成為名稱陣列（契約 §6.2）"
                );
                assert_eq!(args["description"], "平台組");
                assert!(
                    args.get("add").is_none() && args.get("remove").is_none(),
                    "繁中說明：create 不該送 update 專屬欄位，實際為 {args}"
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn team_update_sends_add_and_remove_as_name_arrays() {
        match parse(&argv(&[
            "team",
            "update",
            "platform",
            "--add",
            "sre",
            "--remove",
            "old, older",
            "--accept-from",
            "sre",
        ])) {
            Parsed::Call { args, .. } => {
                assert_eq!(args["action"], "update");
                assert_eq!(args["name"], "platform");
                assert_eq!(args["add"], serde_json::json!(["sre"]));
                assert_eq!(args["remove"], serde_json::json!(["old", "older"]));
                assert_eq!(args["accept_from"], serde_json::json!(["sre"]));
                assert!(
                    args.get("members").is_none(),
                    "繁中說明：update 不該送 create 專屬的 members，實際為 {args}"
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn team_delete_and_list_map_to_their_actions() {
        match parse(&argv(&["team", "delete", "platform"])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "team");
                assert_eq!(args["action"], "delete");
                assert_eq!(args["name"], "platform");
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
        match parse(&argv(&["team", "list"])) {
            Parsed::Call { op, args, .. } => {
                assert_eq!(op, "team");
                assert_eq!(args["action"], "list");
                assert!(
                    args.get("name").is_none(),
                    "繁中說明：team list 不帶名稱時不應送 name，實際為 {args}"
                );
            }
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn the_five_contract_team_and_send_commands_all_parse() {
        // 契約 §9 的表格逐字列出這五個指令。這個測試是「表格與 CLI 一致」的
        // 對照：任一個被漏掉或改名都會在這裡紅。
        for (parts, expected_action, expected_name) in [
            (
                vec![
                    "team",
                    "create",
                    "n",
                    "--members",
                    "a",
                    "--orchestrator",
                    "a",
                ],
                Some("create"),
                Some("n"),
            ),
            (vec!["team", "update", "n"], Some("update"), Some("n")),
            (vec!["team", "delete", "n"], Some("delete"), Some("n")),
            (vec!["team", "list"], Some("list"), None),
        ] {
            match parse(&argv(&parts)) {
                Parsed::Call { op, args, .. } => {
                    assert_eq!(op, "team");
                    assert_eq!(args["action"], expected_action.unwrap_or_default());
                    if let Some(name) = expected_name {
                        assert_eq!(args["name"], name);
                    }
                }
                other => panic!("{parts:?} 應解析為 Call，實際為 {other:?}"),
            }
        }
        match parse(&argv(&["send", "--to", "a", "hi"])) {
            Parsed::Call { op, .. } => assert_eq!(op, "send"),
            other => panic!("應解析為 Call，實際為 {other:?}"),
        }
    }

    #[test]
    fn home_is_global_for_the_t5_commands_too() {
        let cases: [&[&str]; 3] = [
            &["send", "--to", "a", "hi"],
            &["team", "list"],
            &[
                "team",
                "create",
                "n",
                "--members",
                "a",
                "--orchestrator",
                "a",
            ],
        ];
        for parts in cases {
            let mut with_flag = vec!["--home".to_owned(), "/tmp/h".to_owned()];
            with_flag.extend(parts.iter().map(|part| (*part).to_owned()));
            match parse(&argv_owned(with_flag)) {
                Parsed::Call { home, .. } => assert_eq!(
                    home.map(|h| h.to_string_lossy().into_owned()).as_deref(),
                    Some("/tmp/h"),
                    "繁中說明：--home 對 T5 的指令同樣是全域選項（{parts:?}）"
                ),
                other => panic!("{parts:?} 應解析為 Call，實際為 {other:?}"),
            }
        }
    }

    #[test]
    fn exit_code_of_uses_the_operation_result_outcome() {
        let result = OperationResult::completed("op-1", serde_json::json!({}));
        assert_eq!(
            exit_code_of(&result),
            exit_code::OK,
            "繁中說明：completed 應對應 exit 0（契約 §9）"
        );
        let refused = OperationResult::refused("op-2", ErrorCode::TeamAclDenied, "跨 team");
        assert_eq!(
            exit_code_of(&refused),
            exit_code::REFUSED,
            "繁中說明：refused 應對應 exit 3（契約 §9）"
        );
    }

    #[test]
    fn operator_token_is_read_from_secrets() {
        let temp = std::env::temp_dir().join(format!("otterhive-cli-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join("secrets")).expect("應可建立暫存 secrets");
        std::fs::write(temp.join("secrets/operator.token"), "tok-abc\n").expect("應可寫入 token");

        assert_eq!(
            read_operator_token(&temp).as_deref(),
            Some("tok-abc"),
            "繁中說明：應讀到 token 並去掉行尾空白"
        );

        std::fs::remove_file(temp.join("secrets/operator.token")).expect("應可刪除");
        assert!(
            read_operator_token(&temp).is_none(),
            "繁中說明：token 不存在時回 None（呼叫端據此回 unauthorized）"
        );
        let _ = std::fs::remove_dir_all(&temp);
    }

    fn json_u64(value: u64) -> serde_json::Value {
        serde_json::json!(value)
    }
}
