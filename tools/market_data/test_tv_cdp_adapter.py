"""TvCdpAdapter (TradingView 桌面版 CDP) 单测

覆盖:
1. 周期 → TradingView resolution / bar 间隔映射完整性
2. _probe_launch_exe 探测逻辑
3. _run 的 KLine 构造 (秒→毫秒, mock _eval)
4. 集成测试 (TradingView CDP 端口可用时): fetch_recent/older/after/深度边界

说明: 集成用例依赖真实 TradingView 以 --remote-debugging-port 启动, 端口不可用时自动跳过。
"""
import json
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from chanlun_py.exchanges.tv_cdp_adapter import (  # noqa: E402
    TvCdpAdapter,
    _BAR_TO_RES,
    _BAR_IV_SEC,
    _RES_TO_BAR,
    _port_ready,
)
from chanlun_py.exchanges.base import Market  # noqa: E402


# ---- 1. 映射完整性 ----

def test_bar_to_res_mapping():
    assert _BAR_TO_RES["1m"] == "1"
    assert _BAR_TO_RES["5m"] == "5"
    assert _BAR_TO_RES["1H"] == "60"
    assert _BAR_TO_RES["4H"] == "240"
    assert _BAR_TO_RES["1D"] == "D"
    assert _BAR_TO_RES["1W"] == "W"
    assert _BAR_TO_RES["1M"] == "M"
    # 双向一致
    for bar, res in _BAR_TO_RES.items():
        assert _RES_TO_BAR[res] == bar


def test_bar_iv_sec_mapping():
    assert _BAR_IV_SEC["5m"] == 300
    assert _BAR_IV_SEC["1H"] == 3600
    assert _BAR_IV_SEC["1D"] == 86400
    assert _BAR_IV_SEC["1M"] is None  # 月线不等长, 跳过间隔校验


# ---- 2. 启动探测 ----

def test_probe_launch_exe():
    from chanlun_py.exchanges.tv_cdp_adapter import _probe_launch_exe
    exe = _probe_launch_exe()
    # 本机若装有 TradingView 桌面版(MSIX/便携), 应能探测到; 否则返回 None 也可接受
    assert exe is None or os.path.isfile(exe)


# ---- 3. KLine 构造 (mock _eval, 不依赖 TradingView) ----

def _make_adapter(rows_value):
    """构造 mock 适配器: _eval 直接返回页面 JSON。"""
    a = TvCdpAdapter()
    payload = {"count": len(rows_value), "rows": rows_value,
               "sym": "OKX:BTCUSDT.P", "res": "5"}
    a._eval = lambda expr: json.loads(json.dumps(payload))  # noqa: E731
    return a


def test_kline_construction_from_rows():
    a = _make_adapter([[1791180000, 100.0, 101.0, 99.0, 100.5, 1234.5]])
    rows = a.fetch_recent("OKX:BTCUSDT.P", "5m", 1)
    assert len(rows) == 1
    k = rows[0]
    assert k.time == 1791180000 * 1000  # 秒 → 毫秒
    assert k.open == 100.0
    assert k.high == 101.0
    assert k.low == 99.0
    assert k.close == 100.5
    assert k.volume == 1234.5


def test_market_attr():
    assert TvCdpAdapter.market == Market.CRYPTO


# ---- 4. 集成测试 (TradingView CDP 运行时) ----

TV_READY = False
try:
    TV_READY = _port_ready(9333)
except Exception:
    pass

pytestmark_integration = pytest.mark.skipif(
    not TV_READY, reason="TradingView CDP 端口 9333 未就绪(需带 --remote-debugging-port 启动)"
)


@pytestmark_integration
def test_integration_fetch_recent():
    a = TvCdpAdapter()
    rows = a.fetch_recent("OKX:BTCUSDT.P", "5m", 5)
    assert len(rows) == 5
    assert rows[0].time < rows[-1].time
    assert all(k.high >= max(k.open, k.close) for k in rows)
    assert all(k.low <= min(k.open, k.close) for k in rows)


@pytestmark_integration
def test_integration_fetch_recent_deep():
    """大 limit 触发深度预加载: 5m 缓存上限 ~6133 根, 3000 根应可一次取回"""
    a = TvCdpAdapter()
    rows = a.fetch_recent("OKX:BTCUSDT.P", "5m", 3000)
    assert len(rows) == 3000
    assert rows[-1].time > rows[0].time
    # 时间戳锚定: 5m 周期, 相邻间隔 300s
    deltas = {rows[i + 1].time - rows[i].time for i in range(min(50, len(rows) - 1))}
    assert deltas == {300_000}, f"5m 间隔异常: {deltas}"


@pytestmark_integration
def test_integration_fetch_older_within_depth():
    a = TvCdpAdapter()
    recent = a.fetch_recent("OKX:BTCUSDT.P", "5m", 5)
    before = recent[0].time - 10 * 86400_000  # 10 天前, 缓存深度内
    older = a.fetch_older("OKX:BTCUSDT.P", "5m", before, 10)
    assert len(older) == 10
    assert older[-1].time <= before
    assert older[-1].time < recent[0].time


@pytestmark_integration
def test_integration_fetch_after():
    a = TvCdpAdapter()
    recent = a.fetch_recent("OKX:BTCUSDT.P", "5m", 5)
    after_ts = recent[0].time - 86400_000  # 1 天前
    after = a.fetch_after("OKX:BTCUSDT.P", "5m", after_ts)
    assert len(after) >= 1
    assert after[0].time >= after_ts


@pytestmark_integration
def test_integration_depth_limit():
    """40 天前 5m 超出缓存深度(~6000根≈21天) → 返回空(客观限制, 不抛错)"""
    a = TvCdpAdapter()
    recent = a.fetch_recent("OKX:BTCUSDT.P", "5m", 5)
    before_deep = recent[0].time - 40 * 86400_000
    older = a.fetch_older("OKX:BTCUSDT.P", "5m", before_deep, 10)
    assert isinstance(older, list)
    assert len(older) == 0
