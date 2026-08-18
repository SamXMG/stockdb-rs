# stockdb-rs 并发写安全改造 — 概览

## 目标
消灭当前唯一会**真丢数据**的硬伤：多进程/多线程并发写同一 `.dat` 时 last-writer-wins 或相互覆盖损坏。
约束：**不改磁盘格式、不破坏字节级兼容（参考实现为 Python `stockdb`）**。

## 方案
1. **咨询锁（advisory lock）**：新增 `src/lock.rs`，用 `fs4`(0.13) 在每个目标文件的 sidecar `.lock` 上取排他锁（`lock_exclusive`，跨平台 Unix flock / Windows LockFileEx）。
2. **原子写**：`atomic_write` 先写 `{target}.tmp` → `sync_all` 落盘 → `rename` 覆盖。rename 原子，reader 不会看到半截文件；崩溃也不留残留。
3. **写路径加锁**：
   - `write`：整段在**日历锁**内（merge 磁盘日历 → `ensure` 算 `t` → 写 `.dat` → 回写日历），`.dat` 另在自身锁内原子写。日历锁贯穿 ensure→持久化，保证并发时 `t` 索引稳定、不同进程不因各写 `calendar.json` 而丢失交易日。
   - `repack` / `write_meta` / `save_calendar`：同样加 sidecar 锁 + 原子写。
4. **`TradingCalendar::merge`**：合并磁盘上其他进程已 ensure 的日期，防止 save_calendar 互相覆盖丢失。
5. **读路径保持无锁**（高性能热路径），靠原子 rename 保证最终一致、不撕裂。

## 文件
- 新增 `src/lock.rs`、`tests/concurrency.rs`
- 改 `src/lib.rs`（write/repack/write_meta/save_calendar + `mod lock`）、`src/calendar.rs`（merge）、`Cargo.toml`（fs4）、`README.md`（特性说明）

## 验证状态（已编译 + 已测试 ✅）
环境：Windows 11 + Rust **1.97.1** stable-x86_64-pc-windows-msvc（MSVC 工具链，用户本机安装）。
- `cargo build` → **PASS**（BUILD_EXIT=0）。仅 2 个**预存** warning（`lib.rs:94` 生命周期写法、`ingest_bridge.rs:74` unreachable pattern），非本次改动引入。
- `cargo test` → 库内 **8/8 单元测试通过**，含：
  - 并发安全：`lock::atomic_write_roundtrip_and_tmp_cleaned`、`lock::exclusive_lock_serializes_critical_section`、`lock::exclusive_lock_creates_sidecar`（锁文件由 `with_exclusive_lock` 的 `create(true)` 生成）。
  - 字节级兼容（未因加锁回归）：`layout::record_lens_match_python`、`view` qfq 系列（含 `qfq_at_zero_lookahead` 严格前视隔离）、`minute` 系列。
  - `tests/concurrency.rs`（集成）**2/2 通过**：`write_is_atomic_and_produces_valid_file`、`overwrite_then_repack_stays_valid`。
- `tests/align_with_python.rs` / `minute_align.rs` / `view_align.rs` / `write_align.rs` **尚未实跑（环境前置未齐，非代码错）**：4 个文件里的 `SCREENER` 硬编码绝对路径（`/home/honor/Git/LIANGHUA/Screener`）**已改为相对路径** `concat!(env!("CARGO_MANIFEST_DIR"), "/../Screener")`（取 crate 同级 Screener，跨平台、不依赖 cwd）。改后本机实测失败点已从"路径 NotFound"前移到 `Store::open` 找不到 `testdata/`——证明路径修复生效。真正跑通还差两项前置（见下）。
- 曾踩坑并修正：`fs4` 0.13 把 `FileExt` 从 crate 根挪到 `fs4::fs_std::FileExt`，已改 `src/lock.rs`；并修了一处测试断言错误（`atomic_write` 不创建 `.lock`，锁由 `with_exclusive_lock` 负责）。

### 跑通跨语言对齐测试的前置（路径已相对化，仅差两项）
1. **Python `stockdb` 引擎可得**：`from stockdb import engine` 需在 `python3` 环境可 import。本机当前**不可 import**（系统 python3 3.13.14 无该包、Screener 里也无；stockdb 是 stockdb-rs 所复刻的原始 Python 引擎，需另行提供/安装，或把它所在的目录加入 `PYTHONPATH`）。
2. **`testdata/` 已生成**：对齐测试默认读 `CARGO_MANIFEST_DIR/testdata`（可用 `TESTDATA=/abs/path` 覆盖）。该目录需由上述 Python `stockdb` 引擎落盘生成（含 `calendar.json` 及各表 `.dat`，样例票 600000/000001/300750 等）。`minute_align.rs` 另需 `tests/gen_testdata.py` 生成 `minute/` 块。
3. `python3` 本机已在 PATH（3.13.14），此项已满足。

> 注：路径改为相对后，只要把 Python `stockdb` 引擎装好 + `testdata/` 生成，4 个对齐测试即可在任意机器（含本机 Windows）真正执行，验证 Rust 写/读与 Python 引擎的字节级一致。

## 已知取舍
- 日历锁在单次 `write` 内持续持有 → 不同 code 的并发写会在日历锁上串行（吞吐换正确性）；单进程顺序 ingest 无影响。
- 读者可能看到上一次完整快照（最终一致），不会看到撕裂；如需强一致读可在此基础上加共享锁（代码中已留注释位）。
