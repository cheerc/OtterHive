# OtterHive

## 先讀

- 對外行為：`docs/specs/p1-coordination.md`（唯一規格）。
- 要做什麼、什麼順序、怎麼驗收：`docs/plans/p1a-implementation.md`。
- 取捨理由：`docs/adr/0001-runtime-boundaries.md`。
- 要不要移植某功能、在哪一期：`docs/reverse-engineering/feature-map.md` 與 `docs/reverse-engineering/mcp-catalog.json`。
- 舊版行為主張：執行時觀測到的行為只引用 `docs/reverse-engineering/evidence-index.md` 的 REA 證據；從舊 source 讀出的主張要附 `agend-terminal@593da97c:<path>:<line>` 並標明是靜態推論。

## 邊界

- 不修改 `~/.agend`、正式 fleet、`agend-terminal` 或 `agend-customization`；舊 source 可以唯讀參考（GitHub `cheerc/agend-terminal`，commit `593da97c6f94d874f793777b4550d5df2ae4f43c`）。測試與 smoke 一律用暫存的 `OTTERHIVE_HOME`。
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
