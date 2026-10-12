# OtterHive

OtterHive 是一個精簡的 Rust daemon，讓多個 AI coding agent CLI（例如 Claude Code）在各自的 PTY 中長駐、組成 team、互相送訊並由 operator 從終端操作。它從 `agend-terminal` 的實際使用方式重新建構，只移植用得到的能力。

## 狀態

尚無可用的發行版本。目前完成功能對照與行為基線（P0），正在實作 P1a：instance 生命週期、訊息、終端 attach、team 與 deployment。各期範圍見 [架構](docs/architecture.md#實作狀態)。

## 文件

| 文件 | 內容 |
|---|---|
| [架構](docs/architecture.md) | 程序、模組與實作狀態 |
| [P1a 協作契約](docs/specs/p1-coordination.md) | 首版對外行為規格 |
| [ADR 0001](docs/adr/0001-runtime-boundaries.md) | 執行期邊界與選型理由 |
| [功能對照](docs/reverse-engineering/feature-map.md) | workflow → 舊實作 → 分期 |
| [MCP catalog](docs/reverse-engineering/mcp-catalog.json) | 舊版 34 個工具的分期與欄位支援 |
| [證據索引](docs/reverse-engineering/evidence-index.md) | REA 行為證據與未驗證項目 |
