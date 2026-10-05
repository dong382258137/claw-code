"""
TradingView 桌面版 CDP 行情适配器 (market_data 接入层, ② 阶段)

设计目标 (用户规则: 源头控制 — 在上游数据源拿数据, 不在界面猜):
- TradingView 桌面版(Chromium/Electron 套壳)是加密/美股/外汇/全球指数的权威图表源。
- 通过 CDP (Chrome DevTools Protocol) 驱动真实 TradingView 页面, 直接读取图表内部
  序列数据, 无需解析页面 DOM、不依赖非公开数据流协议。
- 与现有 DataFetcher 适配器注册中心对接(ExchangeAdapter 接口), 覆盖 crypto 市场
  的补充数据源 (本地 tdx 文件覆盖 A股/期货, TradingView 覆盖加密/美股等)。

已实证的数据链路 (2026-10-05, 真实 TradingView 桌面版 3.3.0):
  window._exposed_chartWidgetCollection.activeChartWidget._value
    → .getSymbol() / .getResolution() / .setSymbol(sym) / .setResolution(res)
    → .model().mainSeries()  →  .bars()._items[] → {index, value:[time_s,o,h,l,c,v]}
    → .navigateToBar({time: ms})  历史跳转(触发远端加载, 缓存累积)
  时间戳锚定: UTC 整周期对齐 (与 OKX crypto 口径一致, validate_klines 直接可用)

输出:
- ExchangeAdapter 接口: fetch_recent / fetch_older / fetch_after / fetch_tickers
  / fetch_instrument / get_last_price / start_realtime / stop_realtime
- 独立 CLI (标准 OHLCV JSON):
    python -m chanlun_py.exchanges.tv_cdp_adapter --symbol OKX:BTCUSDT.P --bar 5m --limit 100
    python -m chanlun_py.exchanges.tv_cdp_adapter --symbol OKX:BTCUSDT.P --bar 1H \
        --mode older --before_ts 1789693200000 --limit 500

LLM 控制路径 (供自动调用方使用):
1. 检查是否可用 (拉取工具首次调用前先探测):
       python -c "import urllib.request;print(urllib.request.urlopen('http://127.0.0.1:9333/json/version',timeout=3).status)"
   期望输出 200; 失败说明 TradingView 未带调试端口运行, 走第 2 步。
2. 拉起 (端口不可用且进程未运行):
       python -m chanlun_py.exchanges.tv_cdp_adapter --launch --limit 1
   - 若报"已在运行但未开调试端口": 有未保存内容风险 → 让用户手动保存后关闭,
     或调用方确认可丢内容后加 --force-restart (自动杀进程并以 CDP 模式重启)。
3. 拉数 (示例): 见上文 CLI 两行。
4. 诊断:
   - 错误 JSON {"error": "..."} 含下一步动作提示 (关闭重试 / --force-restart 等)。
   - 页面级问题 (切换失败/返回异常): 打开 http://127.0.0.1:9333 看页面, 或重启 TradingView。

注意:
- TradingView 需以 `--remote-debugging-port=<port> --remote-allow-origins=*` 启动。
  本模块 ensure_cdp_ready 可自动拉起(默认不自动杀已运行实例, 防丢用户未保存内容;
  显式 force_restart=True 时才杀)。
- 驱动的是真实 TradingView 界面: 切换 symbol/周期/跳转历史会改变图表视口。
- volume 为 TradingView 口径, 与 OKX 可能不同(缠论以价格为主, 影响小)。
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import logging
import os
import socket
import time
import urllib.request
from typing import Dict, List, Optional

try:
    from .base import ExchangeAdapter, Market, TickerInfo, InstrumentInfo
    from ..types import KLine
    _IN_ENGINE = True
except ImportError:  # 脱离 chanlun_py 包独立运行 (CLI/测试) 时的最小回退
    from dataclasses import dataclass

    @dataclass
    class KLine:
        """原始K线 (与 chanlun_py.types.KLine 同构)"""
        time: int
        open: float
        high: float
        low: float
        close: float
        volume: float

    class Market:
        CRYPTO = "crypto"
        A_STOCK = "a_stock"
        CN_FUTURES = "cn_futures"

    @dataclass
    class TickerInfo:
        symbol: str
        name: str
        last_price: float
        change_pct: float
        volume: float
        amount: float

    @dataclass
    class InstrumentInfo:
        symbol: str
        lot_size: float
        price_tick: float
        contract_multiplier: float
        margin_rate: float
        limit_up: float
        limit_down: float

    class ExchangeAdapter:
        market = None

        def fetch_recent(self, symbol, bar, limit=100):
            raise NotImplementedError

        def fetch_older(self, symbol, bar, before_ts, limit=100):
            raise NotImplementedError

        def fetch_after(self, symbol, bar, after_ts):
            raise NotImplementedError

        def fetch_tickers(self):
            raise NotImplementedError

        def start_realtime(self, symbol, bar, on_kline_cb):
            raise NotImplementedError

        def stop_realtime(self):
            raise NotImplementedError

        def fetch_instrument(self, symbol):
            raise NotImplementedError

        def get_last_price(self, symbol):
            raise NotImplementedError

    _IN_ENGINE = False

logger = logging.getLogger(__name__)

try:
    import websocket  # websocket-client
except ImportError:  # pragma: no cover
    websocket = None

# ===== 常量 =====

DEFAULT_PORT = 9333
DEFAULT_HOST = "127.0.0.1"
_CDP_ORIGIN = "devtools://devtools"
_HTTP_TIMEOUT = 5

# 引擎周期 → TradingView resolution
_BAR_TO_RES = {
    "1m": "1", "3m": "3", "5m": "5", "15m": "15", "30m": "30",
    "1H": "60", "2H": "120", "3H": "180", "4H": "240", "6H": "360", "12H": "720",
    "1D": "D", "1W": "W", "1M": "M",
}
_RES_TO_BAR = {v: k for k, v in _BAR_TO_RES.items()}

# 周期 → 期望 bar 间隔(秒)。D/W/M 由 TradingView 按日历对齐, 间隔固定(86400/604800);
# 1M 月线不等长 → None 表示跳过间隔校验。
_BAR_IV_SEC = {
    "1m": 60, "3m": 180, "5m": 300, "15m": 900, "30m": 1800,
    "1H": 3600, "2H": 7200, "3H": 10800, "4H": 14400, "6H": 21600, "12H": 43200,
    "1D": 86400, "1W": 604800, "1M": None,
}


def _probe_launch_exe() -> Optional[str]:
    """探测 TradingView 桌面版(MSIX/WindowsApps)可执行文件路径。"""
    candidates = []
    root = r"C:\Program Files\WindowsApps"
    try:
        for d in os.listdir(root):
            if d.lower().startswith("tradingview.desktop"):
                candidates.append(os.path.join(root, d, "TradingView.exe"))
    except OSError:
        pass
    candidates += [
        os.path.expandvars(r"%LOCALAPPDATA%\Programs\TradingView\TradingView.exe"),
        os.path.expandvars(r"%LOCALAPPDATA%\TradingView\TradingView.exe"),
    ]
    for c in candidates:
        if os.path.isfile(c):
            return c
    return None


# ===== CDP 通信 (与 cdp_eval.py 同协议) =====

class CdpError(RuntimeError):
    pass


class TvCdpAdapter(ExchangeAdapter):
    """TradingView 桌面版 CDP 数据适配器。

    market=CRYPTO (TradingView 主覆盖加密/美股/外汇/全球指数)。
    symbol 约定: 直接使用 TradingView symbol 字符串, 如 "OKX:BTCUSDT.P"。
    """

    market = Market.CRYPTO

    def __init__(self, port: int = DEFAULT_PORT, host: str = DEFAULT_HOST,
                 timeout: int = 60):
        self.port = port
        self.host = host
        self.timeout = timeout
        self._page_ws: Optional[str] = None
        self._cdp_base = f"http://{host}:{port}"

    # === CDP 基础设施 ===

    def _http_json(self, path: str):
        req = urllib.request.Request(f"{self._cdp_base}{path}",
                                     headers={"Origin": _CDP_ORIGIN})
        with urllib.request.urlopen(req, timeout=_HTTP_TIMEOUT) as r:
            return json.loads(r.read().decode("utf-8"))

    def _find_page_ws(self, force: bool = False) -> str:
        """定位 TradingView 图表页面 WebSocket url。"""
        if self._page_ws and not force:
            return self._page_ws
        targets = self._http_json("/json")
        for t in targets:
            if t.get("type") == "page" and "tradingview.com/chart" in (t.get("url") or ""):
                ws = t.get("webSocketDebuggerUrl")
                if ws:
                    self._page_ws = ws
                    return ws
        raise CdpError(
            f"未找到 TradingView 图表页面 (CDP {self._cdp_base}); "
            f"请确认 TradingView 已带 --remote-debugging-port 启动且打开了图表"
        )

    def _eval(self, expr: str, page_ws: Optional[str] = None):
        """Runtime.evaluate, 返回 result.result.value (页面内返回的 JSON 字符串→对象)。"""
        if websocket is None:
            raise CdpError("缺少 websocket-client 依赖: pip install websocket-client")
        ws_url = page_ws or self._find_page_ws()
        ws = websocket.create_connection(ws_url, timeout=self.timeout, origin=_CDP_ORIGIN)
        try:
            ws.send(json.dumps({"id": 1, "method": "Runtime.evaluate", "params": {
                "expression": expr, "returnByValue": True, "awaitPromise": True,
            }}))
            while True:
                msg = json.loads(ws.recv())
                if msg.get("id") != 1:
                    continue
                if "error" in msg:
                    raise CdpError(f"CDP evaluate 错误: {msg['error']}")
                r = msg.get("result", {}).get("result", {})
                if r.get("subtype") == "error" or "exceptionDetails" in msg.get("result", {}):
                    raise CdpError(f"页面执行异常: {r.get('description', msg)}")
                val = r.get("value")
                if isinstance(val, str):
                    try:
                        return json.loads(val)
                    except (ValueError, TypeError):
                        return val
                return val
        finally:
            ws.close()

    # === 页面内 JS: 切换 + 等待稳定 + 取数 (统一入口) ===

    _DRIVER_JS = r"""
(async()=>{
  const SYM = __SYM__, RES = __RES__, IV = __IV__, BEFORE = __BEFORE__,
        AFTER = __AFTER__, LIMIT = __LIMIT__, MODE = __MODE__;
  const coll = window._exposed_chartWidgetCollection;
  const cw = coll && coll.activeChartWidget && coll.activeChartWidget._value;
  if (!cw || !cw.getSymbol) return JSON.stringify({error: 'no chart widget', href: location.href});
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  if (cw.getSymbol() !== SYM) cw.setSymbol(SYM);
  if (cw.getResolution() !== RES) cw.setResolution(RES);
  // 等待主序列就绪: 切换发生时 items 会清空再重载(实证 SOL 切换 415→0→300),
  // 以 sawEmpty(清空后重填)+ 数据非空 + bar 间隔匹配 作为新数据就绪信号。
  // 未发生切换(symbol 相同)时, n 稳定即就绪(幂等)。
  let ready = false, detail = null, sawEmpty = false, prevN = -1;
  for (let i = 0; i < 80; i++) {
    const s = cw.model().mainSeries();
    const items = s && s.bars() ? s.bars()._items : [];
    const n = items.length;
    if (n === 0) { sawEmpty = true; prevN = 0; await sleep(500); continue; }
    let symN = null; try { symN = s.symbol(); } catch (e) {}
    const bd = n > 1 ? items[n - 1].value[0] - items[n - 2].value[0] : 0;
    const ivOk = (IV === null) || (bd > 0 && Math.abs(bd - IV) <= IV * 0.05);
    if (symN === SYM && ivOk && (sawEmpty || (n === prevN && prevN > 0))) { ready = true; break; }
    if (i > 30 && !detail) detail = 'sym=' + cw.getSymbol() + '/series=' + symN +
      '/items=' + n + '/bd=' + bd + '/sawEmpty=' + sawEmpty;
    prevN = n;
    await sleep(500);
  }
  if (!ready) return JSON.stringify({error: 'switch not ready', detail});
  // recent 且缓存不足 limit 时: 跳到极远触发深度加载
  // (TV 图表缓存上限实证: 5m≈6133根/1m≈10496根; 跳远古后缓存覆盖全区间, 末根仍为最新)
  if (MODE === 'recent') {
    let curN = cw.model().mainSeries().bars()._items.length;
    if (curN < LIMIT) {
      cw.navigateToBar({time: (Date.now() / 1000 - 50 * 365 * 86400) * 1000});
      await sleep(1200);
      let lastN2 = -1, stable2 = 0;
      for (let i = 0; i < 50; i++) {
        const n2 = cw.model().mainSeries().bars()._items.length;
        if (n2 === lastN2) { stable2++; } else { stable2 = 0; lastN2 = n2; }
        if (stable2 >= 3) break;
        await sleep(800);
      }
    }
  }
  if (BEFORE > 0) { cw.navigateToBar({time: BEFORE * 1000}); await sleep(1500); }
  if (AFTER > 0) { cw.navigateToBar({time: AFTER * 1000}); await sleep(1500); }
  // 跳转后等待加载完成: 目标区间覆盖 或 first/last 连续 3 次采样稳定(深度上限达成)。
  // 注意: TV 图表缓存深度约 6000 根(5m≈21天/1m≈4天/1H≈250天), 超深目标无法覆盖,
  // 稳定后按现有缓存取数(可能为空)。
  const t0 = Date.now();
  let lastF = 0, lastL = 0, stableCnt = 0;
  for (let i = 0; i < 60; i++) {
    const items = cw.model().mainSeries().bars()._items;
    const first = items.length ? items[0].value[0] : 0;
    const last = items.length ? items[items.length - 1].value[0] : 0;
    const covered = (MODE === 'recent') ||
      (MODE === 'older' && first > 0 && first <= BEFORE) ||
      (MODE === 'after' && last >= AFTER);
    if (covered) break;
    if (first === lastF && last === lastL) { stableCnt++; if (stableCnt >= 3) break; }
    else { stableCnt = 0; lastF = first; lastL = last; }
    if (Date.now() - t0 > 30000) break;
    await sleep(700);
  }
  const items = cw.model().mainSeries().bars()._items;
  let rows;
  if (MODE === 'older') rows = items.filter(b => b.value[0] <= BEFORE).slice(-LIMIT).map(b => b.value);
  else if (MODE === 'after') rows = items.filter(b => b.value[0] >= AFTER).slice(0, LIMIT).map(b => b.value);
  else rows = items.slice(-LIMIT).map(b => b.value);
  return JSON.stringify({count: items.length, rows, sym: cw.getSymbol(), res: cw.getResolution()});
})()
"""

    def _run(self, symbol: str, bar: str, mode: str = "recent",
             before_ts: int = 0, after_ts: int = 0, limit: int = 100) -> List[KLine]:
        res = _BAR_TO_RES.get(bar)
        if res is None:
            raise CdpError(f"不支持的周期: {bar} (支持: {sorted(_BAR_TO_RES)})")
        expr = (self._DRIVER_JS
                .replace("__SYM__", json.dumps(symbol))
                .replace("__RES__", json.dumps(res))
                .replace("__IV__", "null" if _BAR_IV_SEC[bar] is None else str(_BAR_IV_SEC[bar]))
                .replace("__BEFORE__", str(before_ts))
                .replace("__AFTER__", str(after_ts))
                .replace("__LIMIT__", str(limit))
                .replace("__MODE__", json.dumps(mode)))
        data = self._eval(expr)
        if not isinstance(data, dict):
            raise CdpError(f"页面返回异常: {data}")
        if data.get("error"):
            raise CdpError(f"TradingView 页面错误: {data['error']} ({data.get('detail')})")
        if data.get("sym") != symbol or data.get("res") != res:
            raise CdpError(f"切换未生效: 期望 {symbol}/{res}, 实际 {data.get('sym')}/{data.get('res')}")
        klines: List[KLine] = []
        for row in data.get("rows", []):
            # row = [time_s, o, h, l, c, v]
            klines.append(KLine(
                time=int(row[0]) * 1000,
                open=float(row[1]),
                high=float(row[2]),
                low=float(row[3]),
                close=float(row[4]),
                volume=float(row[5]),
            ))
        return klines

    # === ExchangeAdapter 接口 ===

    def fetch_recent(self, symbol: str, bar: str, limit: int = 100) -> List[KLine]:
        return self._run(symbol, bar, mode="recent", limit=limit)

    def fetch_older(self, symbol: str, bar: str, before_ts: int,
                    limit: int = 100) -> List[KLine]:
        return self._run(symbol, bar, mode="older", before_ts=before_ts // 1000, limit=limit)

    def fetch_after(self, symbol: str, bar: str, after_ts: int) -> List[KLine]:
        return self._run(symbol, bar, mode="after", after_ts=after_ts // 1000, limit=200)

    def fetch_tickers(self) -> List[TickerInfo]:
        # TradingView 图表页面无独立行情列表; 列表数据在 watchlist, 后续可扩展
        return []

    def start_realtime(self, symbol: str, bar: str, on_kline_cb) -> None:
        # 实时数据由 TradingView 图表自身维护; 缠论引擎 WS 层另有实时链路
        raise NotImplementedError("TvCdpAdapter 不提供实时订阅 (实时走现有 WS 链路)")

    def stop_realtime(self) -> None:
        pass

    def fetch_instrument(self, symbol: str) -> InstrumentInfo:
        return InstrumentInfo(symbol=symbol, lot_size=0.001, price_tick=0.1,
                              contract_multiplier=1.0, margin_rate=0.05,
                              limit_up=0.0, limit_down=0.0)

    def get_last_price(self, symbol: str) -> float:
        rows = self.fetch_recent(symbol, "1m", limit=1)
        return rows[-1].close if rows else 0.0


# ===== 端口就绪检查 / 自动拉起 =====

def _port_ready(port: int, host: str = DEFAULT_HOST) -> bool:
    try:
        req = urllib.request.Request(f"http://{host}:{port}/json/version",
                                     headers={"Origin": _CDP_ORIGIN})
        with urllib.request.urlopen(req, timeout=_HTTP_TIMEOUT) as r:
            json.loads(r.read().decode("utf-8"))
        return True
    except Exception:
        return False


def _kill_tradingview() -> None:
    """强制结束 TradingView 进程 (仅 force_restart 路径调用, 会丢未保存内容)。"""
    if os.name != "nt":
        return
    import subprocess
    subprocess.run(["taskkill", "/IM", "TradingView.exe", "/F"],
                   capture_output=True, text=True, timeout=10)


def ensure_cdp_ready(port: int = DEFAULT_PORT, host: str = DEFAULT_HOST,
                     launch_exe: Optional[str] = None, timeout: int = 60,
                     force_restart: bool = False) -> str:
    """确保 TradingView CDP 端口可用。

    - 端口已就绪 → 直接返回。
    - 端口不可用且 launch_exe 给出:
        - TradingView 进程已在运行(未带调试端口):
            默认抛错提示(不自动杀进程, 防丢未保存内容);
            force_restart=True 时先 _kill_tradingview() 再重启。
        - 未运行 → 带 --remote-debugging-port 与 --remote-allow-origins=* 启动并轮询就绪。
    """
    if _port_ready(port, host):
        return "ready"
    if not launch_exe or not os.path.isfile(launch_exe):
        raise CdpError(
            f"CDP 端口 {port} 不可用, 且未找到 TradingView 可执行文件; "
            f"请用 --remote-debugging-port={port} --remote-allow-origins=* 启动 TradingView"
        )
    if _running_process() and not force_restart:
        raise CdpError(
            "TradingView 已在运行但未开调试端口。为避免丢失未保存内容, "
            "请先手动保存, 然后关闭 TradingView 后重试; "
            "或确认可丢未保存内容后加 --force-restart 由程序自动重启"
        )
    if _running_process():
        logger.warning("force_restart: 结束现有 TradingView 进程 (未保存内容会丢失)")
        _kill_tradingview()
        time.sleep(2)
    logger.info(f"启动 TradingView: {launch_exe} (port={port})")
    if os.name == "nt":
        import subprocess
        subprocess.Popen(
            [launch_exe, f"--remote-debugging-port={port}", "--remote-allow-origins=*"],
            creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) |
            getattr(subprocess, "DETACHED_PROCESS", 0),
        )
    else:  # pragma: no cover
        import subprocess
        subprocess.Popen([launch_exe, f"--remote-debugging-port={port}",
                          "--remote-allow-origins=*"])
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _port_ready(port, host):
            return "launched"
        time.sleep(2)
    raise CdpError(f"TradingView 启动超时 ({timeout}s), 端口 {port} 未就绪")


def _running_process() -> bool:
    if os.name != "nt":
        return False
    try:
        import subprocess
        out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq TradingView.exe"],
                             capture_output=True, text=True, timeout=10).stdout
        return "TradingView.exe" in out
    except Exception:
        return False


# ===== CLI =====

def _main(argv: Optional[List[str]] = None) -> int:
    ap = argparse.ArgumentParser(description="TradingView CDP 行情拉取 (标准 OHLCV JSON)")
    ap.add_argument("--symbol", default="OKX:BTCUSDT.P", help="TradingView symbol")
    ap.add_argument("--bar", default="5m", help="周期: 1m/3m/5m/15m/30m/1H/2H/4H/1D/1W/1M")
    ap.add_argument("--limit", type=int, default=100)
    ap.add_argument("--mode", choices=["recent", "older", "after"], default="recent")
    ap.add_argument("--before_ts", type=int, default=0, help="older 模式: 取此毫秒时间戳之前")
    ap.add_argument("--after_ts", type=int, default=0, help="after 模式: 取此毫秒时间戳之后")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--launch", action="store_true",
                    help="端口不可用时自动拉起 TradingView")
    ap.add_argument("--force-restart", action="store_true",
                    help="(仅配合 --launch) 已在运行但未开调试端口时, 自动结束进程并重启; "
                         "会丢失未保存内容")
    args = ap.parse_args(argv)

    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    try:
        if args.launch:
            ensure_cdp_ready(port=args.port, launch_exe=_probe_launch_exe(),
                             force_restart=args.force_restart)
        adapter = TvCdpAdapter(port=args.port)
        if args.mode == "older":
            rows = adapter.fetch_older(args.symbol, args.bar, args.before_ts, args.limit)
        elif args.mode == "after":
            rows = adapter.fetch_after(args.symbol, args.bar, args.after_ts)
        else:
            rows = adapter.fetch_recent(args.symbol, args.bar, args.limit)
        print(json.dumps({
            "symbol": args.symbol, "bar": args.bar, "mode": args.mode,
            "count": len(rows), "klines": [
                {"time": k.time, "open": k.open, "high": k.high, "low": k.low,
                 "close": k.close, "volume": k.volume} for k in rows
            ],
        }, ensure_ascii=False))
        return 0
    except Exception as e:
        print(json.dumps({"error": str(e)}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    raise SystemExit(_main())
