//! T4 Lane A GREEN：daemon 授權（契約 §2）。
//!
//! - operator 以 `secrets/operator.token` 認證；instance 以每個 generation
//!   一組的 token 認證，舊 generation 的 token 一律失效。
//! - 呼叫端不能自報 sender／role／team：sender 一律由憑證推導。
//! - 權限只來自 operator、`managers`、team 的 `orchestrator` 與「對自己」；
//!   `role` 是描述文字，不授予任何權限。

use sha2::{Digest, Sha256};

/// 依憑證推導出的呼叫端身分（契約 §2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// operator（`secrets/operator.token`）。
    Operator,
    /// instance 呼叫端；`id` 為 `InstanceId`。
    Instance(String),
}

impl Caller {
    /// `operations.caller` 的記錄形式（契約 §3：`operator` 或
    /// `instance:<InstanceId>`）。與 generation 無關，因此 restart 後以新
    /// token 重送仍視為同一呼叫端。
    pub fn record_key(&self) -> String {
        match self {
            Caller::Operator => "operator".to_owned(),
            Caller::Instance(id) => format!("instance:{id}"),
        }
    }

    /// instance 呼叫端的身分 ID（operator 回 `None`）。
    pub fn instance_id(&self) -> Option<&str> {
        match self {
            Caller::Operator => None,
            Caller::Instance(id) => Some(id),
        }
    }
}

/// token 的雜湊形式（SQLite 只存雜湊，不存明文）。
pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 產生一個 instance token（每個 generation 一組）。
///
/// 與 `home` 層的 operator token 同樣取 32 bytes 的系統亂數、十六進位編碼，
/// 避免 token 形狀與 operator 憑證不同。
pub fn new_instance_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|random_error| {
        std::io::Error::other(format!("無法取得系統亂數：{random_error}"))
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 一次授權判斷（契約 §2 的權限矩陣）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// 允許。
    Allow,
    /// 不允許（`unauthorized`）。
    Deny,
}

/// 契約 §2 矩陣中的動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// create／start／delete instance、team 異動、deployment（含其 list）。
    /// 只有 operator。
    Manage,
    /// restart：任何（operator）／manager／只限自己。
    Restart,
    /// interrupt：任何／manager／自己 team 的成員／instance 否。
    Interrupt,
    /// pane_snapshot：任何／manager／自己 team 的成員與自己／只限自己。
    PaneSnapshot,
    /// list_instances／team list：四種身分都可。
    List,
}

/// 被操作對象的資訊（由 store／設定查出，不來自請求）。
#[derive(Debug, Clone, Default)]
pub struct Subject {
    /// 被操作的 instance 名稱（若有）。
    pub instance_name: Option<String>,
    /// 被操作 instance 所屬的 team 與該 team 的 orchestrator 名稱。
    pub team: Option<(String, String)>,
    /// 呼叫端 instance 的名稱（operator 為 `None`）。
    pub caller_name: Option<String>,
    /// 呼叫端 instance 名稱在設定檔的 `managers` 中。
    pub caller_is_manager: bool,
}

impl Subject {
    /// 呼叫端 instance 是否就是被操作 instance（restart／snapshot 自己）。
    pub fn is_self(&self) -> bool {
        match (&self.caller_name, &self.instance_name) {
            (Some(caller), Some(target)) => caller == target,
            _ => false,
        }
    }

    /// 呼叫端是否為被操作 instance 所屬 team 的 orchestrator。
    pub fn caller_is_orchestrator(&self) -> bool {
        match (&self.caller_name, &self.team) {
            (Some(caller), Some((_, orchestrator))) => caller == orchestrator,
            _ => false,
        }
    }
}

/// 依契約 §2 的矩陣判斷 `caller` 是否可對 `action` 執行。
pub fn authorize(caller: &Caller, action: Action, subject: &Subject) -> Permission {
    let allow = |ok: bool| {
        if ok {
            Permission::Allow
        } else {
            Permission::Deny
        }
    };
    match caller {
        // operator 對矩陣中所有動作都有權（create／start／delete／team／
        // deployment／restart／interrupt／snapshot／list）。
        Caller::Operator => Permission::Allow,
        Caller::Instance(_) => {
            let is_manager = subject.caller_is_manager;
            match action {
                Action::List => Permission::Allow,
                Action::Manage => Permission::Deny,
                Action::Restart => allow(is_manager || subject.is_self()),
                Action::Interrupt => allow(is_manager || subject.caller_is_orchestrator()),
                Action::PaneSnapshot => {
                    allow(is_manager || subject.caller_is_orchestrator() || subject.is_self())
                }
            }
        }
    }
}
