//! T4 Lane B：`otterhive` operator CLI（契約 §9）。
//!
//! 全域選項 `--home <dir>` 優先於 `OTTERHIVE_HOME`；operator 憑證從
//! `secrets/operator.token` 讀。除 `daemon` 外，每個指令把 `OperationResult`
//! 以 JSON 印到 stdout，exit code 依 `outcome` 映射（0 completed／queued、
//! 2 partial、3 refused、4 unsupported）；連不上 daemon 或請求格式錯誤回 1。
//!
//! `daemon` 由 Lane A 的 daemon 端提供；本檔案在 daemon 尚未實作時以
//! `-32000` 的 JSON 回應非零結束，讓呼叫端看見「還沒做」而不是假裝成功。

use std::io::Write;
use std::process::ExitCode;

use serde_json::{json, Value};

use otterhive::cli::{self, Parsed};
use otterhive::client::{self, CallOptions};
use otterhive::protocol::{Auth, ErrorCode, OpError, OperationResult, Outcome};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    match cli::parse(&argv) {
        Parsed::Daemon { home } => run_daemon(home.as_deref()),
        Parsed::Call { home, op, args } | Parsed::RawCall { home, op, args } => {
            run_call(home.as_deref(), &op, args)
        }
        Parsed::Malformed { reason } => {
            eprintln!("{reason}");
            ExitCode::from(cli::exit_code::UNREACHABLE_OR_MALFORMED as u8)
        }
        Parsed::Unknown { argv0 } => {
            eprintln!("未認得的指令：{argv0}");
            ExitCode::from(cli::exit_code::UNREACHABLE_OR_MALFORMED as u8)
        }
    }
}

/// home 解析：`--home` 優先，其次 `OTTERHIVE_HOME`，都沒有回 `None`。
fn resolve_home(explicit: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    if let Some(home) = explicit {
        return Some(home.to_path_buf());
    }
    std::env::var_os("OTTERHIVE_HOME").map(std::path::PathBuf::from)
}

/// 讀不到 home 或憑證時的 `OperationResult`（stdout 仍是合法的結果 JSON）。
fn local_failure(code: ErrorCode, message: &str) -> OperationResult {
    OperationResult {
        operation_id: String::new(),
        outcome: Outcome::Refused,
        subject: None,
        value: Value::Null,
        effects: Vec::new(),
        error: Some(OpError {
            code,
            message: message.to_owned(),
            retryable: false,
            next: None,
        }),
    }
}

fn run_call(home: Option<&std::path::Path>, op: &str, args: Value) -> ExitCode {
    let Some(home_path) = resolve_home(home) else {
        return emit(&local_failure(
            ErrorCode::InvalidArgument,
            "未設定 OTTERHIVE_HOME，也沒有 --home",
        ));
    };

    let Some(token) = cli::read_operator_token(&home_path) else {
        // 缺憑證是授權問題（refused → exit 3），不是連線問題。
        return emit(&local_failure(
            ErrorCode::Unauthorized,
            "讀不到 operator 憑證（secrets/operator.token）",
        ));
    };

    let options = CallOptions::new(&home_path, Auth::Operator(token), op, args);
    match client::call(&options) {
        Ok(result) => emit(&result),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(cli::exit_code::UNREACHABLE_OR_MALFORMED as u8)
        }
    }
}

/// `otterhive daemon`：前景執行 daemon。
///
/// daemon 的 socket 服務屬 Lane A；尚未實作時以 stderr 的
/// `unsupported_in_phase` JSON 與 exit 4 結束，語意是「這期還沒做」而不是
/// 「啟動失敗」。
fn run_daemon(home: Option<&std::path::Path>) -> ExitCode {
    let resolved = resolve_home(home);
    let message = match &resolved {
        Some(home) => format!(
            "daemon 尚未實作（T4 Lane A）；目標 home：{}",
            home.display()
        ),
        None => "daemon 尚未實作（T4 Lane A）；且未設定 OTTERHIVE_HOME 或 --home".to_owned(),
    };
    let failure = local_failure(ErrorCode::UnsupportedInPhase, &message);
    let _ = writeln!(
        std::io::stderr(),
        "{}",
        serde_json::to_string(&failure).unwrap_or_else(|_| json!({}).to_string())
    );
    ExitCode::from(cli::exit_code::UNSUPPORTED as u8)
}

/// 把結果以一行 JSON 印到 stdout，並依 outcome 回 exit code。
fn emit(result: &OperationResult) -> ExitCode {
    let encoded = match serde_json::to_string(result) {
        Ok(encoded) => encoded,
        Err(error) => {
            eprintln!("結果無法序列化：{error}");
            return ExitCode::from(cli::exit_code::UNREACHABLE_OR_MALFORMED as u8);
        }
    };
    println!("{encoded}");
    let _ = std::io::stdout().flush();
    ExitCode::from(cli::exit_code_for(result.outcome) as u8)
}
