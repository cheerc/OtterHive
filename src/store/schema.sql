-- OtterHive 內部狀態 schema。契約見 docs/specs/p1-coordination.md。
-- 欄位名 instances.pid 與 instances.pid_start 固定：測試會直接修改它們模擬身分不符。

CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);          -- schema_version = 1
CREATE TABLE instances(
  id TEXT PRIMARY KEY, name TEXT NOT NULL, spec TEXT NOT NULL,         -- spec 為 JSON
  state TEXT NOT NULL, reason TEXT, generation INTEGER NOT NULL DEFAULT 0,
  pid INTEGER, pid_start INTEGER, token_hash TEXT,
  deployment TEXT, deleted_ms INTEGER, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
CREATE UNIQUE INDEX instances_live_name ON instances(name) WHERE deleted_ms IS NULL;
CREATE TABLE teams(name TEXT PRIMARY KEY, orchestrator_id TEXT NOT NULL, description TEXT,
  accept_from TEXT NOT NULL DEFAULT '[]', deployment TEXT);
CREATE TABLE team_members(team TEXT NOT NULL, instance_id TEXT NOT NULL UNIQUE,
  PRIMARY KEY(team, instance_id));
CREATE TABLE messages(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, recipient_id TEXT NOT NULL,
  sender_kind TEXT NOT NULL, sender_id TEXT, sender_name TEXT NOT NULL, kind TEXT NOT NULL,
  body TEXT NOT NULL, correlation_id TEXT, parent_id TEXT, thread_id TEXT,
  state TEXT NOT NULL,                                                  -- unread|delivering|acked|archived
  delivery_token TEXT, lease_expires_ms INTEGER, delivery_count INTEGER NOT NULL DEFAULT 0,
  created_ms INTEGER NOT NULL, acked_ms INTEGER);
CREATE TABLE operations(id TEXT PRIMARY KEY, op TEXT NOT NULL, caller TEXT NOT NULL,
  result TEXT NOT NULL, created_ms INTEGER NOT NULL);
CREATE TABLE deployments(name TEXT PRIMARY KEY, template TEXT NOT NULL, team TEXT NOT NULL,
  residuals TEXT NOT NULL DEFAULT '[]', created_ms INTEGER NOT NULL);
CREATE TABLE journal(id INTEGER PRIMARY KEY AUTOINCREMENT, instance_id TEXT, generation INTEGER,
  kind TEXT NOT NULL, detail TEXT, at_ms INTEGER NOT NULL);