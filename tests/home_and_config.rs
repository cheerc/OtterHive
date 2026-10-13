use std::fs;
use std::path::Path;

use otterhive::config::Config;
use otterhive::home::{DaemonLock, Home, LockError};
use tempfile::tempdir;

#[test]
fn second_daemon_lock_on_same_home_is_refused() {
    let temp = tempdir().expect("建立暫存 home 目錄");
    let run_dir = temp.path().join("run");
    fs::create_dir(&run_dir).expect("建立 daemon run 目錄");

    let first = DaemonLock::acquire(&run_dir).expect("取得第一個 daemon lock");
    let second = DaemonLock::acquire(&run_dir);
    assert!(
        matches!(second, Err(LockError::AlreadyHeld)),
        "同一個 run 目錄的第二個 lock 應回 AlreadyHeld，實際結果：{second:?}"
    );

    drop(first);
    let after_release = DaemonLock::acquire(&run_dir);
    assert!(
        after_release.is_ok(),
        "釋放第一個 lock 後應可重新取得 lock，實際錯誤：{:?}",
        after_release.err()
    );
}

#[test]
fn home_directories_and_secrets_have_owner_only_permissions() {
    let temp = tempdir().expect("建立暫存目錄");
    let home_path = temp.path().join("otterhive-home");
    let home = Home::initialize(&home_path).expect("初始化暫存 OtterHive home");

    assert_eq!(home.root(), Path::new(&home_path));
    assert_mode(home.run_dir(), 0o700, "run/ 應只有 owner 可存取");
    assert_mode(home.secrets_dir(), 0o700, "secrets/ 應只有 owner 可存取");
    assert_mode(
        home.operator_token_path(),
        0o600,
        "operator.token 應只有 owner 可讀寫",
    );

    let token_before = fs::read(home.operator_token_path()).expect("讀取初始化後的 operator token");
    let initialized_again = Home::initialize(&home_path).expect("再次初始化同一個暫存 home");
    let token_after = fs::read(initialized_again.operator_token_path())
        .expect("讀取重複初始化後的 operator token");

    assert_eq!(
        token_after, token_before,
        "重複初始化不得重建或改變既有 operator token"
    );
    assert_mode(
        initialized_again.operator_token_path(),
        0o600,
        "重複初始化後 operator.token 仍應只有 owner 可讀寫",
    );
}

#[cfg(unix)]
fn assert_mode(path: &Path, expected: u32, reason: &str) {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).expect("取得檔案或目錄權限中繼資料");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        expected,
        "{reason}：{}",
        path.display()
    );
}

#[cfg(not(unix))]
fn assert_mode(_path: &Path, _expected: u32, _reason: &str) {
    panic!("此權限測試依賴 Unix 權限位元，非 Unix 平台不在 P1a 驗證範圍");
}

#[test]
fn config_accepts_legacy_template_format_and_reports_ignored_fields() {
    let temp = tempdir().expect("建立暫存目錄");
    let config_path = temp.path().join("otterhive.yaml");
    fs::write(
        &config_path,
        "templates:\n  legacy:\n    orchestrator: lead\n    instances:\n      lead:\n        role: coordinator\n    source_repo: https://example.invalid/repo.git\n    project_id: legacy-project\n    github_login: legacy-user\nmanagers:\n  - operator-helper\n",
    )
    .expect("寫入 legacy template 設定檔");

    let config = Config::load(&config_path).expect("載入合法 legacy template 設定");
    assert_eq!(config.managers, vec!["operator-helper"]);

    let template = config
        .templates
        .get("legacy")
        .expect("載入後應保留 legacy template");
    for ignored_field in ["source_repo", "project_id", "github_login"] {
        assert!(
            template
                .ignored_fields
                .iter()
                .any(|path| path.contains(ignored_field)),
            "legacy template 的 ignored fields 應回報 {ignored_field}，實際為 {:?}",
            template.ignored_fields
        );
    }
}

#[test]
fn config_rejects_unknown_top_level_keys() {
    let temp = tempdir().expect("建立暫存目錄");
    let config_path = temp.path().join("otterhive.yaml");
    fs::write(&config_path, "channel: slack\n").expect("寫入含未知頂層鍵的設定檔");

    let error = Config::load(&config_path).expect_err("channel 是不允許的頂層鍵，載入設定應被拒絕");
    let message = error.to_string();
    assert!(
        message.contains("channel"),
        "錯誤訊息應指出被拒絕的頂層鍵 channel，實際訊息：{message}"
    );
    assert!(
        message.contains("templates") && message.contains("managers"),
        "錯誤訊息應列出允許的頂層鍵 templates 與 managers，實際訊息：{message}"
    );

    let missing_path = temp.path().join("missing.yaml");
    let empty = Config::load(&missing_path).expect("設定檔不存在時應視為空設定");
    assert!(empty.managers.is_empty(), "空設定的 managers 應為空");
    assert!(empty.templates.is_empty(), "空設定的 templates 應為空");
}
