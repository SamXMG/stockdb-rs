"""为 stockdb-rs 对齐测试生成 5 张未测表的样例数据 (FundFlow/IndexDaily/
Announcement/RenameEvent/DailySnapshot)。

用法: python3 tests/gen_testdata.py <testdata_root> <screener_root>
依赖本地 Screener/stockdb 的 engine + schema。
"""
import sys, os
sys.path.insert(0, sys.argv[2])
from stockdb import engine, schema
from stockdb.calendar import TradingCalendar
from dataclasses import fields as dc_fields
import json

ROOT = sys.argv[1]
SCR = sys.argv[2]
cal = TradingCalendar.load(os.path.join(ROOT, "calendar.json"))
store = engine.ColumnStore(ROOT, cal)

codes = ["600000", "000001", "300750"]

# 取日历前若干交易日做 t 锚点
dates = cal._dates[:20]
date_t = {d: cal.get_t(d) for d in dates}

def mkrow(cls, **kw):
    # 用 schema dataclass 构造, 缺字段留空(由 dataclass 默认值补)
    valid = {f.name for f in dc_fields(cls)}
    args = {k: kw[k] for k in kw if k in valid}
    return cls(**args)

# FundFlow: 3 只票各写 2 天
for code in codes:
    rows = []
    for i, d in enumerate(dates[:2]):
        rows.append(mkrow(schema.FundFlow, code=code, t=date_t[d], date=d,
            main_net=1.2e8 + i*1e6, main_pct=3.1 + i, xl_net=5e7, xl_pct=1.2,
            l_net=-3e7, l_pct=-0.8, mid_net=2e7, mid_pct=0.5,
            small_net=-9e7, small_pct=-2.1))
    store.write("FundFlow", code, rows)

# IndexDaily: 用上证指数 000001 写 (这里用 code 字段; 指数用 index_code)
for code in ["000001", "399001"]:
    rows = []
    for i, d in enumerate(dates[:3]):
        rows.append(mkrow(schema.IndexDaily, index_code=code, t=date_t[d], date=d,
            open=3000.0 + i, high=3050.0 + i, low=2980.0 + i,
            close=3020.0 + i, volume=1e8 + i*1e6, amount=2e11 + i*1e9))
    store.write("IndexDaily", code, rows)

# Announcement: 3 只票各 1 条
for code in codes:
    rows = []
    d = dates[1]
    rows.append(mkrow(schema.Announcement, code=code, ann_date=d, ann_type="业绩预告",
        title="关于2023年半年度业绩预告的公告", summary="预计净利润同比增长",
        url="http://example.com/a.pdf", t=date_t[d]))
    store.write("Announcement", code, rows)

# RenameEvent: 部分票
rows = []
d = dates[2]
rows.append(mkrow(schema.RenameEvent, code="600000", announce_date=d,
    effective_date=dates[3], old_name="上海浦发银行", new_name="浦发银行股份有限公司",
    reason="规范化全称", t=date_t[d]))
store.write("RenameEvent", "600000", rows)

# DailySnapshot: 3 只票各 1 天
for code in codes:
    d = dates[0]
    rows = [mkrow(schema.DailySnapshot, code=code, date=d, t=date_t[d],
        name="测试股份", board="主板", is_st=False, price=12.5, prev_close=12.0,
        chg_pct=4.17, vol_ratio=1.3, turnover=2.1, market_cap_yi=1000.0,
        float_cap_yi=800.0, pe=15.6, pb=1.8, chg60=10.5,
        flow_main=1e8, flow_main_pct=3.0, flow_xl=5e7, flow_xl_pct=1.5,
        flow_l=-3e7, flow_l_pct=-0.9, industry="银行", concepts="沪股通;融资融券")]
    store.write("DailySnapshot", code, rows)

print("generated:", [t for t in ["FundFlow","IndexDaily","Announcement","RenameEvent","DailySnapshot"]])
