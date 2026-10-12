# OtterHive

## 先讀

- 對外行為：`docs/specs/p1-coordination.md`（唯一規格）。
- 取捨理由：`docs/adr/0001-runtime-boundaries.md`。
- 要不要移植某功能、在哪一期：`docs/reverse-engineering/feature-map.md` 與 `docs/reverse-engineering/mcp-catalog.json`。
- 舊版行為主張：只引用 `docs/reverse-engineering/evidence-index.md` 列出的證據。

## 邊界

- 不讀寫 `~/.agend`、正式 fleet、`agend-terminal` 或 `agend-customization` 的檔案。測試與 smoke 一律用暫存的 `OTTERHIVE_HOME`。
- 不在未經 operator 授權下啟動付費的 backend CLI；backend 行為用 `tests/fixtures/backends/` 的 fake executable 測。
- 不支援的工具、欄位與值回 `unsupported_in_phase`，不靜默忽略。
- catalog 由 `scripts/research/build_mcp_catalog.py` 產生；改分期或欄位支援時改 script 再重新產生，不手改 JSON。

## 驗證

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

新行為先寫會失敗的測試，再實作。
