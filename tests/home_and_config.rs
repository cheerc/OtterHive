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

/// 寫入設定內容並回傳路徑，讓每個案例各自擁有獨立檔案。
fn write_config(directory: &Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = directory.join(name);
    fs::write(&path, contents).expect("寫入測試用 otterhive.yaml");
    path
}

#[test]
fn member_level_ignored_fields_are_collected_into_template() {
    let temp = tempdir().expect("建立暫存目錄");
    let config_path = write_config(
        temp.path(),
        "member-ignored.yaml",
        "templates:\n  team-a:\n    orchestrator: lead\n    instances:\n      lead:\n        role: coordinator\n        tmux_pane: '%3'\n        legacy_note: 舊版備註\n",
    );

    let config = Config::load(&config_path).expect("成員層的不生效欄位不應讓載入失敗");
    let template = config
        .templates
        .get("team-a")
        .expect("載入後應保留 team-a template");

    // 成員層的不生效欄位必須回報，且帶完整點分路徑以指回來源。
    for ignored_field in ["tmux_pane", "legacy_note"] {
        let expected_path = format!("templates.team-a.instances.lead.{ignored_field}");
        assert!(
            template.ignored_fields.contains(&expected_path),
            "成員層的 ignored field 應回報 {expected_path}，實際為 {:?}",
            template.ignored_fields
        );
    }

    // template 層級的 ignored field 仍要一併保留，兩者不可互相覆蓋。
    assert!(
        !template
            .ignored_fields
            .iter()
            .any(|path| path == "templates.team-a.role"),
        "成員欄位不得被誤報為 template 層級欄位：{:?}",
        template.ignored_fields
    );
    assert_eq!(
        template
            .instances
            .get("lead")
            .map(|member| member.role.as_deref()),
        Some(Some("coordinator")),
        "成員本身仍應正確解析 role"
    );
}

#[test]
fn orchestrator_must_be_one_of_the_template_members() {
    let temp = tempdir().expect("建立暫存目錄");

    // orchestrator 指向不存在的成員。
    let not_a_member = write_config(
        temp.path(),
        "orchestrator-missing.yaml",
        "templates:\n  team-a:\n    orchestrator: ghost\n    instances:\n      lead:\n        role: coordinator\n",
    );
    let error =
        Config::load(&not_a_member).expect_err("orchestrator 不在 instances 之中時應被拒絕");
    let message = error.to_string();
    assert!(
        message.contains("orchestrator") && message.contains("ghost"),
        "錯誤訊息應指出 orchestrator 與其值，實際訊息：{message}"
    );

    // 完全缺少 orchestrator。
    let absent = write_config(
        temp.path(),
        "orchestrator-absent.yaml",
        "templates:\n  team-a:\n    instances:\n      lead:\n        role: coordinator\n",
    );
    let error = Config::load(&absent).expect_err("缺少 orchestrator 時應被拒絕");
    assert!(
        error.to_string().contains("orchestrator"),
        "錯誤訊息應指出缺少 orchestrator，實際訊息：{error}"
    );

    // orchestrator 是合法成員時應通過。
    let valid = write_config(
        temp.path(),
        "orchestrator-valid.yaml",
        "templates:\n  team-a:\n    orchestrator: lead\n    instances:\n      lead:\n        role: coordinator\n",
    );
    let config = Config::load(&valid).expect("orchestrator 是成員之一時應載入成功");
    assert_eq!(
        config
            .templates
            .get("team-a")
            .and_then(|template| template.orchestrator.as_deref()),
        Some("lead")
    );
}

#[test]
fn template_member_names_must_follow_instance_name_rules() {
    let temp = tempdir().expect("建立暫存目錄");

    for (file_name, member_name, reason) in [
        ("member-operator.yaml", "operator", "不得為保留名 operator"),
        ("member-dot.yaml", "bad.name", "不得含小數點"),
        ("member-space.yaml", "bad name", "不得含空白"),
        ("member-slash.yaml", "bad/name", "不得含斜線"),
    ] {
        let config_path = write_config(
            temp.path(),
            file_name,
            &format!(
                "templates:\n  team-a:\n    orchestrator: {member_name}\n    instances:\n      {member_name}:\n        role: coordinator\n"
            ),
        );
        let error = Config::load(&config_path)
            .expect_err(&format!("成員名稱違反 §4.1 規則時應被拒絕：{reason}"));
        let message = error.to_string();
        assert!(
            message.contains("instances"),
            "錯誤訊息應指出問題出在 instances 之下，實際訊息：{message}"
        );
        assert!(
            message.contains(member_name),
            "錯誤訊息應指出無效的成員名稱 {member_name}，實際訊息：{message}"
        );
    }

    // 65 個字元超出上限。
    let too_long = "a".repeat(65);
    let config_path = write_config(
        temp.path(),
        "member-too-long.yaml",
        &format!(
            "templates:\n  team-a:\n    orchestrator: {too_long}\n    instances:\n      {too_long}:\n        role: coordinator\n"
        ),
    );
    let error = Config::load(&config_path).expect_err("成員名稱超過 64 字元時應被拒絕");
    assert!(
        error.to_string().contains("64"),
        "錯誤訊息應指出 64 字元上限，實際訊息：{error}"
    );

    // 剛好 64 字元應通過。
    let at_limit = "a".repeat(64);
    let config_path = write_config(
        temp.path(),
        "member-at-limit.yaml",
        &format!(
            "templates:\n  team-a:\n    orchestrator: {at_limit}\n    instances:\n      {at_limit}:\n        role: coordinator\n"
        ),
    );
    Config::load(&config_path).expect("成員名稱剛好 64 字元應可載入");

    // 保留底線與連字元的合法名稱應通過。
    let config_path = write_config(
        temp.path(),
        "member-legal.yaml",
        "templates:\n  team-a:\n    orchestrator: lead_1\n    instances:\n      lead_1:\n        role: coordinator\n",
    );
    Config::load(&config_path).expect("含底線與數字的名稱應可載入");
}
