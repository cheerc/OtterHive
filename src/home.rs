use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use getrandom::fill;

/// `run/` 與 `secrets/` 只允許 owner 存取。
const DIR_MODE: u32 = 0o700;
/// socket 與 operator 憑證檔只允許 owner 讀寫。
const SECRET_MODE: u32 = 0o600;

pub const OPERATOR_TOKEN_FILE: &str = "operator.token";
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";
pub const CONFIG_FILE: &str = "otterhive.yaml";

/// 已就緒的 OtterHive home；所有路徑都從 `root` 推導。
#[derive(Clone, Debug)]
pub struct Home {
    root: PathBuf,
    run: PathBuf,
    secrets: PathBuf,
    token: PathBuf,
    lock: PathBuf,
    config: PathBuf,
}

/// `Home::initialize` 失敗的原因。
#[derive(Debug)]
pub enum HomeError {
    /// 目錄或檔案操作失敗。
    Io(io::Error),
    /// `OTTERHIVE_HOME` 指向的位置無法使用。
    InvalidHome(String),
}

impl fmt::Display for HomeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HomeError::Io(err) => write!(f, "建立 OtterHive home 失敗：{err}"),
            HomeError::InvalidHome(reason) => write!(f, "OTTERHIVE_HOME 無效：{reason}"),
        }
    }
}

impl std::error::Error for HomeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HomeError::Io(err) => Some(err),
            HomeError::InvalidHome(_) => None,
        }
    }
}

impl From<io::Error> for HomeError {
    fn from(err: io::Error) -> Self {
        HomeError::Io(err)
    }
}

/// 取得 daemon lock 失敗的原因。
#[derive(Debug)]
pub enum LockError {
    /// 同一個 run 目錄已經有 daemon 持有 lock。
    AlreadyHeld,
    /// 建立或檢查 lock 檔時的 I/O 失敗。
    Io(io::Error),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::AlreadyHeld => f.write_str("此 home 已經有 daemon 在執行"),
            LockError::Io(err) => write!(f, "操作 daemon lock 失敗：{err}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::AlreadyHeld => None,
            LockError::Io(err) => Some(err),
        }
    }
}

impl From<io::Error> for LockError {
    fn from(err: io::Error) -> Self {
        LockError::Io(err)
    }
}

/// 獨佔同一個 home 的 daemon lock；值被丟棄時自動釋放。
#[derive(Debug)]
pub struct DaemonLock {
    /// 持有 flock 的檔案句柄；欄位存在即代表鎖仍在。
    _file: std::fs::File,
    path: PathBuf,
}

impl Home {
    /// 建立（或沿用）`path` 指定的 home。
    ///
    /// 冪等：已存在的 `operator.token` 不會被覆寫，權限也會重新收斂為 0600。
    pub fn initialize(path: &Path) -> Result<Home, HomeError> {
        ensure_dir(path, DIR_MODE)?;

        let secrets = path.join("secrets");
        ensure_dir(&secrets, DIR_MODE)?;

        let run = path.join("run");
        ensure_dir(&run, DIR_MODE)?;

        let home = Home {
            root: path.to_path_buf(),
            token: secrets.join(OPERATOR_TOKEN_FILE),
            lock: run.join(DAEMON_LOCK_FILE),
            config: path.join(CONFIG_FILE),
            run,
            secrets,
        };
        home.ensure_operator_token()?;

        Ok(home)
    }

    /// 從 `OTTERHIVE_HOME` 取得 home；未設定時用 `~/.otterhive`。
    pub fn from_env() -> Result<Home, HomeError> {
        Home::initialize(&home_path()?)
    }

    /// home 根目錄。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `run/`，daemon socket 與 lock 所在位置。
    pub fn run_dir(&self) -> &Path {
        &self.run
    }

    /// `secrets/`，operator 憑證所在位置。
    pub fn secrets_dir(&self) -> &Path {
        &self.secrets
    }

    /// `secrets/operator.token`。
    pub fn operator_token_path(&self) -> &Path {
        &self.token
    }

    /// `run/daemon.lock`。
    pub fn daemon_lock_path(&self) -> &Path {
        &self.lock
    }

    /// `$OTTERHIVE_HOME/otterhive.yaml`。
    pub fn config_path(&self) -> &Path {
        &self.config
    }

    fn ensure_operator_token(&self) -> Result<(), HomeError> {
        let token_path = &self.token;

        if token_path.exists() {
            // 冪等：既有憑證不得被改寫，但權限仍需收斂為 0600。
            set_mode(token_path, SECRET_MODE)?;
            return Ok(());
        }

        let token = new_token()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(SECRET_MODE);
        #[cfg(unix)]
        let mut file = match options.open(token_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                // 另一個程序同時建立成功，沿用它的內容。
                set_mode(token_path, SECRET_MODE)?;
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };

        #[cfg(not(unix))]
        let mut file = options.open(token_path)?;

        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

impl DaemonLock {
    /// 獨佔 `run_dir`；已被持有時回 [`LockError::AlreadyHeld`]。
    pub fn acquire(run_dir: &Path) -> Result<DaemonLock, LockError> {
        ensure_dir(run_dir, DIR_MODE)?;

        let path = run_dir.join(DAEMON_LOCK_FILE);
        // 以 `create_new` 建立 lock 檔（不截斷既有檔），再靠 flock 判定獨佔性，
        // 這樣第二個 daemon 看到的是「已被持有」而不是把檔案清空。
        let file = match create_private_file(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(LockError::Io)?,
            Err(err) => return Err(LockError::Io(err)),
        };

        if !lock_is_vacant(&file).map_err(LockError::Io)? {
            return Err(LockError::AlreadyHeld);
        }

        Ok(DaemonLock { _file: file, path })
    }

    /// lock 檔路徑。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `OTTERHIVE_HOME` 或預設的 `~/.otterhive`。
pub fn home_path() -> Result<PathBuf, HomeError> {
    match std::env::var_os("OTTERHIVE_HOME") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(HomeError::InvalidHome(format!(
                    "{} 不是絕對路徑",
                    path.display()
                )));
            }
            Ok(path)
        }
        Some(_) => Err(HomeError::InvalidHome(
            "OTTERHIVE_HOME 是空字串".to_string(),
        )),
        None => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                HomeError::InvalidHome("HOME 未設定，無法決定預設的 ~/.otterhive".to_string())
            })?;
            Ok(PathBuf::from(home).join(".otterhive"))
        }
    }
}

#[cfg(unix)]
fn ensure_dir(path: &Path, mode: u32) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {
            set_mode(path, mode)?;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            if !path.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} 已存在但不是目錄", path.display()),
                ));
            }
            set_mode(path, mode)
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let current = fs::metadata(path)?.permissions().mode() & 0o777;
    if current == mode {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn ensure_dir(path: &Path, _mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// 以 `create_new` 建立 0600 的檔案，避免覆寫既有 lock。
#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(SECRET_MODE)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn lock_is_vacant(file: &std::fs::File) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;

    // LOCK_EX | LOCK_NB：成功代表沒有其他程序持有，回 false 表示已被持有。
    // 這裡取得的 flock 隨 `file` 一起被同一個 DaemonLock 持有，因此等同取得 lock。
    Ok(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0)
}

#[cfg(not(unix))]
fn lock_is_vacant(_file: &std::fs::File) -> io::Result<bool> {
    Ok(true)
}

/// 產生 256 bit 的隨機 operator token（hex 編碼）。
fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    fill(&mut bytes)
        .map_err(|err| io::Error::other(format!("無法取得系統亂數產生 operator token：{err}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
