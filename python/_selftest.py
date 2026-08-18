"""端到端自检：Rust cdylib -> Python ctypes 读。
运行: cd python && python _selftest.py
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from stockdb_rs import StockDB, StockDBError

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "fixture"))


def main():
    if not os.path.isdir(ROOT):
        print(f"[SKIP] 未找到 fixture: {ROOT}（先 `cargo run --example make_fixture`）")
        return 1
    try:
        db = StockDB(ROOT)
    except StockDBError as e:
        print(f"[FAIL] {e}")
        return 1

    closes = db.read_column("RawDailyBar", "demo600000", "close")
    print("read_column close:", closes)
    c1 = db.read_at("RawDailyBar", "demo600000", 1, "close")
    print("read_at t=1 close:", c1)
    db.close()

    ok = (
        closes == [10.2, 10.8, 11.5]
        and abs(c1 - 10.8) < 1e-9
    )
    print("[PASS]" if ok else "[FAIL] 回读值与写入值不一致")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
