"""TdxFileAdapter (通达信/同花顺本地文件) 单测

覆盖:
1. .day 合成二进制解析 (价格×100 int 布局, 15:00 本地锚定)
2. .lc5/.lc1 合成二进制解析 (float 布局, HHMM 时间)
3. 日线聚合 (2D/5D/1W/1M)
4. 分钟聚合 (从 1m → 3m/2H/4H)
5. 代码→市场目录映射
6. 适配器 fetch_recent/older/after 本地优先 (临时安装目录)
7. 真实 000001.day 集成校验 (文件存在时)
"""
import os
import struct
import sys
import tempfile
from datetime import datetime

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from chanlun_py.exchanges.tdx_adapter import (  # noqa: E402
    TdxFileAdapter,
    _code_to_market_dir,
    _aggregate_bars,
    _aggregate_minute,
    parse_day_file,
    parse_min_file,
)
from chanlun_py.exchanges.base import Market  # noqa: E402
from chanlun_py.types import KLine  # noqa: E402

DAY_RECORD = struct.Struct("<IIIIIfII")
MIN_RECORD = struct.Struct("<IIfffffI")


def _make_day_file(path, rows):
    """rows: [(date, o, h, l, c, amount, vol)] 价格按 ×100 int"""
    with open(path, "wb") as f:
        for date, o, h, l, c, amount, vol in rows:
            # round 而非 int: 避免 10.03*100=1002.999... 被截断成 1002
            f.write(DAY_RECORD.pack(date, round(o * 100), round(h * 100),
                                    round(l * 100), round(c * 100), amount, vol, 0))


def _make_min_file(path, rows):
    """rows: [(date, hhmm, o, h, l, c, amount, vol)] 价格 float"""
    with open(path, "wb") as f:
        for date, hhmm, o, h, l, c, amount, vol in rows:
            f.write(MIN_RECORD.pack(date, hhmm, o, h, l, c, amount, vol))


# ---- 1. .day 解析 ----

def test_parse_day_file():
    with tempfile.TemporaryDirectory() as td:
        p = os.path.join(td, "000001.day")
        _make_day_file(p, [
            (20240102, 10.00, 10.07, 9.97, 10.07, 89238.0, 10945),
            (20240103, 10.07, 10.20, 10.06, 10.11, 97745.0, 8119),
            (20240104, 10.11, 10.15, 10.01, 10.03, 91230.0, 10002),
        ])
        ks = parse_day_file(p)
        assert len(ks) == 3
        k = ks[0]
        # 价格 ×100 还原
        assert k.open == pytest.approx(10.00)
        assert k.high == pytest.approx(10.07)
        assert k.low == pytest.approx(9.97)
        assert k.close == pytest.approx(10.07)
        assert k.volume == pytest.approx(10945.0)
        # 15:00 本地锚定 (与 akshare 口径一致): 2024-01-02 15:00
        dt = datetime.fromtimestamp(k.time / 1000)
        assert (dt.year, dt.month, dt.day, dt.hour, dt.minute) == (2024, 1, 2, 15, 0)
        # 升序
        assert all(ks[i].time < ks[i + 1].time for i in range(len(ks) - 1))


def test_parse_day_file_skips_zero_date():
    with tempfile.TemporaryDirectory() as td:
        p = os.path.join(td, "x.day")
        # 40 字节 → 1 条有效 + 尾部残留(不足32字节被忽略)
        _make_day_file(p, [(20240102, 10.0, 10.1, 9.9, 10.0, 1.0, 1)])
        with open(p, "ab") as f:
            f.write(b"\x00" * 40)
        ks = parse_day_file(p)
        assert len(ks) == 1


# ---- 2. 分钟文件解析 ----

def test_parse_min_file_lc5():
    with tempfile.TemporaryDirectory() as td:
        p = os.path.join(td, "000001.lc5")
        _make_min_file(p, [
            (20240102, 935, 10.00, 10.05, 9.98, 10.04, 20000.0, 500),
            (20240102, 940, 10.04, 10.10, 10.01, 10.08, 30000.0, 700),
        ])
        ks = parse_min_file(p)
        assert len(ks) == 2
        k = ks[0]
        assert k.open == pytest.approx(10.00)
        assert k.high == pytest.approx(10.05)
        dt = datetime.fromtimestamp(k.time / 1000)
        assert (dt.year, dt.month, dt.day, dt.hour, dt.minute) == (2024, 1, 2, 9, 35)
        k2 = ks[1]
        dt2 = datetime.fromtimestamp(k2.time / 1000)
        assert (dt2.hour, dt2.minute) == (9, 40)


def test_parse_min_file_hlc_sanity():
    """OHLC 不合法的记录被过滤 (高<低等)。"""
    with tempfile.TemporaryDirectory() as td:
        p = os.path.join(td, "bad.lc1")
        _make_min_file(p, [
            (20240102, 930, 10.0, 10.5, 9.5, 10.2, 1.0, 1),    # 合法
            (20240102, 931, 10.0, 9.0, 11.0, 10.0, 1.0, 1),    # 高<低 → 过滤
        ])
        ks = parse_min_file(p)
        assert len(ks) == 1


# ---- 3. 日线聚合 ----

def _daily_klines():
    # 4 个交易日: 周一~周四 (2024-01-01 是周一)
    return [KLine(time=datetime(2024, 1, d, 15, 0).timestamp() * 1000,
                  open=10 + d, high=10.5 + d, low=9.5 + d, close=10.2 + d,
                  volume=1000.0 * d)
            for d in (1, 2, 3, 4)]


def test_aggregate_n2_n5():
    ks = _daily_klines()
    n2 = _aggregate_bars(ks, "n2")
    assert len(n2) == 2
    assert n2[0].open == ks[0].open
    assert n2[0].high == max(ks[0].high, ks[1].high)
    assert n2[0].low == min(ks[0].low, ks[1].low)
    assert n2[0].close == ks[1].close
    assert n2[0].volume == pytest.approx(ks[0].volume + ks[1].volume)
    assert n2[0].time == ks[1].time   # 聚合K线时间戳=组内最后交易日
    n5 = _aggregate_bars(ks, "n5")
    assert len(n5) == 1
    assert n5[0].close == ks[-1].close


def test_aggregate_week_month():
    # 跨两周: 2024-01-01(周一) ~ 2024-01-08(周一)
    ks = [KLine(time=datetime(2024, 1, d, 15, 0).timestamp() * 1000,
                open=10.0, high=11.0, low=9.0, close=10.5, volume=100.0)
          for d in (1, 2, 3, 4, 5, 8)]  # 前5个是同一周, 1-08 是下一周
    wk = _aggregate_bars(ks, "week")
    assert len(wk) == 2
    assert wk[0].close == ks[4].close
    assert wk[1].open == ks[5].open
    # 跨月: 2024-01-31, 2024-02-01
    ks_m = [KLine(time=datetime(2024, m, d, 15, 0).timestamp() * 1000,
                  open=10.0, high=11.0, low=9.0, close=10.5, volume=100.0)
            for m, d in ((1, 31), (2, 1), (2, 2))]
    mo = _aggregate_bars(ks_m, "month")
    assert len(mo) == 2
    assert mo[1].open == ks_m[1].open


# ---- 4. 分钟聚合 ----

def test_aggregate_minute_3m_and_4h():
    # 1m 合成: 09:30~09:33 + 跨 4h
    base = datetime(2024, 1, 2, 9, 30).timestamp() * 1000
    ks = [KLine(time=base + i * 60_000, open=10.0, high=10.0 + i * 0.01,
                low=9.9, close=10.0 + i * 0.01, volume=100.0) for i in range(4)]
    m3 = _aggregate_minute(ks, "3m")
    assert len(m3) == 2  # 09:30-09:32 / 09:33 各自成桶
    assert m3[0].high == pytest.approx(10.02)
    assert m3[0].volume == pytest.approx(300.0)
    assert m3[0].time % 180_000 == 0
    # 4h: 4 根 1m 合成 1 根
    h4 = _aggregate_minute(ks, "4H")
    assert len(h4) == 1
    assert h4[0].close == ks[-1].close


# ---- 5. 代码→目录 ----

def test_code_to_market_dir():
    assert _code_to_market_dir("600519") == "sh"
    assert _code_to_market_dir("688981") == "sh"
    assert _code_to_market_dir("000001") == "sz"
    assert _code_to_market_dir("300750") == "sz"
    assert _code_to_market_dir("830000") == "bj"
    assert _code_to_market_dir("432001") == "bj"


# ---- 6. 适配器本地优先 (临时安装目录) ----

@pytest.fixture()
def fake_tdx(tmp_path):
    """构造临时通达信目录: vipdoc/sz/lday/000001.day + vipdoc/sz/fzline/000001.lc1/.lc5"""
    vp = tmp_path / "vipdoc"
    sz = vp / "sz"
    lday = sz / "lday"
    fz = sz / "fzline"
    lday.mkdir(parents=True)
    fz.mkdir(parents=True)
    _make_day_file(str(lday / "000001.day"), [
        (20240102, 10.00, 10.07, 9.97, 10.07, 89238.0, 10945),
        (20240103, 10.07, 10.20, 10.06, 10.11, 97745.0, 8119),
        (20240104, 10.11, 10.15, 10.01, 10.03, 91230.0, 10002),
    ])
    _make_min_file(str(fz / "000001.lc1"), [
        (20240102, 930, 10.00, 10.05, 9.98, 10.04, 20000.0, 500),
        (20240102, 931, 10.04, 10.10, 10.01, 10.08, 30000.0, 700),
    ])
    _make_min_file(str(fz / "000001.lc5"), [
        (20240102, 935, 10.00, 10.05, 9.98, 10.04, 20000.0, 500),
        (20240102, 940, 10.04, 10.10, 10.01, 10.08, 30000.0, 700),
    ])
    return str(vp)


def test_adapter_fetch_recent_local(fake_tdx):
    a = TdxFileAdapter(Market.A_STOCK, install_dir=fake_tdx,
                       enable_akshare_fallback=False)
    ks = a.fetch_recent("000001", "1D", 2)
    assert len(ks) == 2
    assert ks[-1].close == pytest.approx(10.03)
    # 5m 走独立 lc5 文件 (通达信分钟线不做从1m聚合)
    ks_m = a.fetch_recent("000001", "5m", 10)
    assert len(ks_m) == 2
    assert ks_m[-1].close == pytest.approx(10.08)


def test_adapter_fetch_older_after(fake_tdx):
    a = TdxFileAdapter(Market.A_STOCK, install_dir=fake_tdx,
                       enable_akshare_fallback=False)
    mid = datetime(2024, 1, 2, 15, 0).timestamp() * 1000
    older = a.fetch_older("000001", "1D", mid + 1, 10)
    assert len(older) == 1 and older[0].time == mid
    after = a.fetch_after("000001", "1D", mid)
    assert len(after) == 2
    assert after[0].time > mid


def test_adapter_missing_symbol_no_fallback(fake_tdx):
    a = TdxFileAdapter(Market.A_STOCK, install_dir=fake_tdx,
                       enable_akshare_fallback=False)
    assert a.fetch_recent("600519", "1D", 5) == []
    # 本地已有代码可枚举 (fake 目录的 000001), 不崩溃
    ts = a.fetch_tickers()
    assert ts and any(t.symbol == "000001" for t in ts)


def test_adapter_tickers(fake_tdx):
    a = TdxFileAdapter(Market.A_STOCK, install_dir=fake_tdx,
                       enable_akshare_fallback=False)
    ts = a.fetch_tickers()
    assert any(t.symbol == "000001" for t in ts)


# ---- 7. 真实通达信数据集成 (文件存在时) ----

@pytest.mark.skipif(
    not os.path.isfile(r"D:\TradingPlatforms\TDX\vipdoc\sz\lday\000001.day"),
    reason="本机无通达信本地日线数据",
)
def test_real_tdx_000001():
    a = TdxFileAdapter(Market.A_STOCK, enable_akshare_fallback=False)
    ks = a.fetch_recent("000001", "1D", 400)
    assert len(ks) >= 100
    # 平安银行 2024 年初价格区间合理性
    assert all(3.0 < k.close < 25.0 for k in ks)
    assert all(k.high >= max(k.open, k.close) for k in ks)
    assert all(k.low <= min(k.open, k.close) for k in ks)
