"""
通达信/同花顺 本地文件行情适配器 (market_data 接入层)

设计目标 (用户规则: 源头控制 — 在上游数据源拿数据, 不在界面猜):
- 用户本地的 通达信(vipdoc) / 同花顺(history) 数据文件是 A股/期货 K线的权威上游。
- 直接解析本地二进制文件, 零网络、零外部进程, 不依赖 akshare 反爬接口。
- 与现有 DataFetcher 适配器注册中心对接(ExchangeAdapter 接口), 非 crypto 市场
  本地文件优先, 文件缺失时自动回退 akshare。

支持格式 (格式公开稳定, 已实证/文档化):
1. 日线 .day  (vipdoc/<sh|sz|bj|ot>/lday/<code>.day, 32 字节小端每记录):
    日期int(YYYYMMDD) | 开int | 高int | 低int | 收int(价格×100) |
    成交额float | 成交量int | 保留4字节
    —— 2026-10-05 用真实 000001.day (400 记录) 实证: 价格/日期/量额与平安银行日K一致。
2. 分钟线 .lc1/.lc5 (vipdoc/<sh|sz|bj|ot>/fzline) / .lc15/.lc30/.lc60
   (vipdoc/<sh|sz|bj|ot>/minline), 32 字节小端每记录:
    日期int(YYYYMMDD) | 时间int(HHMM, 如 930/935/1000) |
    开float | 高float | 低float | 收float | 成交额float | 成交量int
    —— 遵循公开文档格式(价格 float 直接存储, 与 .day 的 ×100 int 不同)。
    (本机 fzline 目录暂无分钟数据, 解析用合成样本单测覆盖, 待用户下载后实盘核对。)

输出:
- ExchangeAdapter 接口: fetch_recent / fetch_older / fetch_after / fetch_tickers
  / fetch_instrument / get_last_price / start_realtime / stop_realtime
- 独立 CLI (标准 OHLCV JSON, 供脚本/后续 TradingView CDP 摄取共用同一输出格式):
    python -m chanlun_py.exchanges.tdx_adapter --symbol 000001 --bar 1D
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import logging
import os
import struct
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

        def _with_fallback(self, *a, **kw):
            raise NotImplementedError

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

# ===== 二进制格式常量 =====

_DAY_RECORD = struct.Struct("<IIIIIfII")     # 32 字节: 日期/开/高/低/收(×100)/额float/量/保留
_MIN_RECORD = struct.Struct("<IIfffffI")     # 32 字节: 日期/时间/开/高/低/收(float)/额float/量
_DAY_SCALE = 100.0                           # 日线 OHLC 整数存储, ×100 还原

# 分钟周期 → 通达信分钟文件扩展名
_MIN_FILE_EXTS = {
    "1m": "lc1", "5m": "lc5", "15m": "lc15",
    "30m": "lc30", "1H": "lc60",
}

# 周期 → (通达信子目录, 扩展名)  日线=lday, 1m/5m=fzline, 15m+/60m=minline
_TDX_DIR_EXT = {
    "1D": ("lday", "day"), "2D": ("lday", "day"), "5D": ("lday", "day"),
    "1W": ("lday", "day"), "1M": ("lday", "day"),
    "1m": ("fzline", "lc1"), "5m": ("fzline", "lc5"),
    "15m": ("minline", "lc15"), "30m": ("minline", "lc30"),
    "1H": ("minline", "lc60"),
}
# 同花顺: 日线=day 目录, 分钟一律 minline 目录
_THS_DIR_EXT = {
    "1D": ("day", "day"), "2D": ("day", "day"), "5D": ("day", "day"),
    "1W": ("day", "day"), "1M": ("day", "day"),
    "1m": ("minline", "lc1"), "5m": ("minline", "lc5"),
    "15m": ("minline", "lc15"), "30m": ("minline", "lc30"),
    "1H": ("minline", "lc60"),
}

# 周期 → 毫秒间隔(与 csv_data.BAR_INTERVAL_MS 对齐, 避免重复导入循环)
_BAR_MS = {
    "1m": 60_000, "3m": 180_000, "5m": 300_000,
    "15m": 900_000, "30m": 1_800_000,
    "1H": 3_600_000, "2H": 7_200_000, "4H": 14_400_000,
    "1D": 86_400_000,
    "1W": 7 * 24 * 3_600_000,
    "1M": 30 * 24 * 3_600_000,
}

# 日线聚合周期: (bar, 聚合方式) — "n" 按连续 N 根聚合, "week"/"month" 按日历聚合
_DAILY_BARS = {"1D": "n1", "2D": "n2", "5D": "n5", "1W": "week", "1M": "month"}
_MINUTE_AGG_BARS = {"3m": "1m", "2H": "1m", "4H": "1m"}  # 无独立文件, 从 1m 聚合

# 安装路径探测: 环境变量优先, 然后常见安装位置
_TDX_PROBE_DIRS = [
    "D:\\TradingPlatforms\\TDX",
    "C:\\new_tdx", "C:\\tdx", "C:\\zd_tsone", "C:\\tdxw",
    "D:\\tdx", "D:\\TdxW", "D:\\zd_tsone", "E:\\tdx",
]
_THS_PROBE_DIRS = [
    "D:\\同花顺", "C:\\同花顺", "D:\\ths", "C:\\ths",
    "D:\\TradingPlatforms\\THS",
]
# 同花顺数据根下: history/<sh|sz>/day 或 userdata
_THS_DATA_SUBDIRS = ("history", "userdata")


# ===== 时间戳锚定 (与 akshare_adapter 口径一致) =====

def date_int_to_ts_ms(d: int) -> int:
    """YYYYMMDD → 毫秒时间戳, 日K锚定 15:00 本地时间 (CST = 07:00 UTC)。

    与 akshare_adapter._date_to_ts_ms 完全一致, 保证同源切换不改变时间戳口径,
    使 validate_klines 的 a_stock 1D 偏移对齐(7h)继续成立。
    """
    y, m, dd = d // 10000, (d // 100) % 100, d % 100
    return int(_dt.datetime(y, m, dd, 15, 0, 0).timestamp() * 1000)


def min_time_to_ts_ms(date_int: int, hhmm: int) -> int:
    """YYYYMMDD + HHMM → 毫秒时间戳(保留原始时分, 与 akshare 分钟口径一致)。"""
    y, m, dd = date_int // 10000, (date_int // 100) % 100, date_int % 100
    hh, mm = hhmm // 100, hhmm % 100
    if hh > 23 or mm > 59:
        logger.warning(f"分钟时间字段异常: date={date_int} hhmm={hhmm}, 跳过")
        return 0
    return int(_dt.datetime(y, m, dd, hh, mm).timestamp() * 1000)


# ===== 文件解析 (纯函数, 可独立测试) =====

def parse_day_file(path: str) -> List[KLine]:
    """解析通达信/同花顺日线 .day 文件 → List[KLine] (时间升序)。"""
    with open(path, "rb") as f:
        data = f.read()
    n = len(data) // 32
    klines: List[KLine] = []
    for i in range(n):
        date, o, h, l, c, _amt, vol, _resv = _DAY_RECORD.unpack_from(data, i * 32)
        if date <= 0:
            continue
        klines.append(KLine(
            time=date_int_to_ts_ms(date),
            open=o / _DAY_SCALE,
            high=h / _DAY_SCALE,
            low=l / _DAY_SCALE,
            close=c / _DAY_SCALE,
            volume=float(vol),
        ))
    return klines


def parse_min_file(path: str) -> List[KLine]:
    """解析通达信分钟线 .lc1/.lc5/.lc15/.lc30/.lc60 → List[KLine] (时间升序)。

    格式: 32 字节小端 {日期int, 时间int(HHMM), 开/高/低/收 float, 额float, 量int}。
    解析后做基本健全性检查; 若绝大多数记录 OHLC 不合法(格式假设错误),
    记录告警(帮助排查不同版本通达信的字节布局差异)。
    """
    with open(path, "rb") as f:
        data = f.read()
    n = len(data) // 32
    klines: List[KLine] = []
    bad = 0
    for i in range(n):
        date, hhmm, o, h, l, c, _amt, vol = _MIN_RECORD.unpack_from(data, i * 32)
        ts = min_time_to_ts_ms(date, hhmm)
        if ts <= 0 or date <= 0:
            bad += 1
            continue
        k = KLine(time=ts, open=o, high=h, low=l, close=c, volume=float(vol))
        if not (h >= max(o, c) and l <= min(o, c) and o > 0 and c > 0):
            bad += 1
            continue
        klines.append(k)
    if n and bad > n * 0.8:
        logger.warning(
            f"分钟文件解析异常率高: {path} {bad}/{n} 记录不合法, "
            f"可能该版本通达信分钟格式与 {_MIN_RECORD.format} 布局不一致"
        )
    return klines


def _aggregate_bars(klines: List[KLine], mode: str) -> List[KLine]:
    """按模式聚合日线: n1/n2/n5(连续N根) | week | month → List[KLine]。"""
    if mode == "n1" or len(klines) <= 1:
        return klines
    groups: List[List[KLine]] = []
    if mode in ("n2", "n5"):
        n = int(mode[1:])
        groups = [klines[i:i + n] for i in range(0, len(klines), n)]
    elif mode == "week":
        cur_key = None
        for k in klines:
            dt = _dt.datetime.fromtimestamp(k.time / 1000)
            key = dt.isocalendar()[:2]  # (iso_year, iso_week)
            if key != cur_key:
                groups.append([])
                cur_key = key
            groups[-1].append(k)
    elif mode == "month":
        cur_key = None
        for k in klines:
            dt = _dt.datetime.fromtimestamp(k.time / 1000)
            key = (dt.year, dt.month)
            if key != cur_key:
                groups.append([])
                cur_key = key
            groups[-1].append(k)
    out: List[KLine] = []
    for g in groups:
        out.append(KLine(
            time=g[-1].time,               # 聚合K线时间戳 = 组内最后交易日 15:00
            open=g[0].open,
            high=max(x.high for x in g),
            low=min(x.low for x in g),
            close=g[-1].close,
            volume=sum(x.volume for x in g),
        ))
    return out


def _aggregate_minute(klines: List[KLine], bar: str) -> List[KLine]:
    """按周期桶聚合分钟K线 (3m/2H/4H 等无独立文件的周期, 从 1m 聚合)。

    桶 = ts // interval_ms (起始对齐), 空桶(午休)不产生K线。
    """
    interval_ms = _BAR_MS[bar]
    out: List[KLine] = []
    for k in klines:
        bucket = k.time // interval_ms
        if out and out[-1].time // interval_ms == bucket:
            last = out[-1]
            last.high = max(last.high, k.high)
            last.low = min(last.low, k.low)
            last.close = k.close
            last.volume += k.volume
        else:
            out.append(KLine(
                time=bucket * interval_ms,
                open=k.open, high=k.high, low=k.low, close=k.close, volume=k.volume,
            ))
    return out


# ===== 安装目录探测 =====

def _find_data_dirs(install_dir: Optional[str] = None) -> Dict[str, str]:
    """探测通达信/同花顺数据目录, 返回 {kind: root} 有序映射。

    kind ∈ {"tdx"(vipdoc根), "ths"(history/userdata根)}。环境变量
    TDX_INSTALL_DIR / THS_INSTALL_DIR 优先, 然后常见安装路径。
    """
    roots: List[str] = []
    if install_dir:
        roots.append(install_dir)
    for var in ("TDX_INSTALL_DIR", "THS_INSTALL_DIR"):
        env = os.environ.get(var)
        if env:
            roots.append(env)
    roots += _TDX_PROBE_DIRS + _THS_PROBE_DIRS
    cand: Dict[str, str] = {}
    seen = set()
    for r in roots:
        r = r.strip().strip('"').strip("'")
        if not r or r in seen:
            continue
        seen.add(r)
        if not os.path.isdir(r):
            continue
        vp = os.path.join(r, "vipdoc")
        if os.path.isdir(vp):
            cand.setdefault("tdx", vp)
        elif os.path.isdir(os.path.join(r, "sz")) or os.path.isdir(os.path.join(r, "sh")):
            # 传入的即 vipdoc 根 (含 sz/sh/bj 市场子目录), 直接接受
            cand.setdefault("tdx", r)
        for sub in _THS_DATA_SUBDIRS:
            hist = os.path.join(r, sub)
            if os.path.isdir(hist):
                cand.setdefault("ths", hist)
    return cand


# 通达信 6位代码 → vipdoc 市场子目录
def _code_to_market_dir(code: str) -> str:
    if code.startswith("688") or code.startswith(("60", "68", "90")):
        return "sh"
    if code.startswith(("0", "3")):
        return "sz"
    if code.startswith(("4", "8")):
        return "bj"
    return "sh"


# ===== 适配器 =====

class TdxFileAdapter(ExchangeAdapter):
    """通达信/同花顺 本地文件适配器 (A股 + 国内期货)。

    本地文件优先, 文件缺失时惰性回退 akshare(网络), 保证不在本地数据的品种
    仍可用。目录探测一次完成并缓存。
    """

    def __init__(self, market: Market, install_dir: Optional[str] = None,
                 enable_akshare_fallback: bool = True):
        if market not in (Market.A_STOCK, Market.CN_FUTURES):
            raise ValueError(f"TdxFileAdapter 不支持市场 {market}")
        self.market = market
        self.enable_akshare_fallback = enable_akshare_fallback
        self._install_dir = install_dir
        self._data_roots: Dict[str, str] = {}
        self._file_cache: Dict[tuple, tuple] = {}   # (path, size, mtime) -> klines
        self._last_prices: Dict[str, float] = {}
        self._akshare = None
        self._probed = False

    # ---- 目录/文件解析 ----

    def _ensure_probed(self):
        if not self._probed:
            self._data_roots = _find_data_dirs(self._install_dir)
            self._probed = True
            if self._data_roots:
                logger.info(
                    f"TdxFileAdapter 本地数据根: {list(self._data_roots.values())}"
                )

    def _candidate_files(self, symbol: str, bar: str) -> List[str]:
        """候选本地文件路径: 通达信 vipdoc + 同花顺 history。

        目录-扩展名映射见 _TDX_DIR_EXT / _THS_DIR_EXT; 期货(ot)做主力/连续命名探测。
        """
        self._ensure_probed()
        files: List[str] = []
        if self.market == Market.A_STOCK:
            mdir = _code_to_market_dir(symbol)
            names = [f"{symbol}"]
        else:  # CN_FUTURES: ot 目录, 主力/连续命名探测
            mdir = "ot"
            names = [f"{symbol}"]
            if not symbol.endswith("0"):
                names += [f"{symbol}0", f"{symbol}00", f"{symbol}0000"]
        for kind, root in self._data_roots.items():
            dir_ext = (_THS_DIR_EXT if kind == "ths" else _TDX_DIR_EXT).get(bar)
            if dir_ext is None:
                continue
            subdir, ext = dir_ext
            base = os.path.join(root, mdir, subdir)
            if kind == "ths":
                # 同花顺根下还有 history|userdata 两层
                base = os.path.join(root, "history" if os.path.isdir(
                    os.path.join(root, "history", mdir, subdir)) else "userdata",
                    mdir, subdir)
            if not os.path.isdir(base):
                continue
            for nm in names:
                files.append(os.path.join(base, f"{nm}.{ext}"))
        return files

    def _load_file(self, path: str) -> List[KLine]:
        """按 (路径, 大小, mtime) 缓存解析结果, 避免同轮多次重复读盘。"""
        try:
            st = os.stat(path)
        except OSError:
            return []
        key = (path, st.st_size, st.st_mtime_ns)
        if key in self._file_cache:
            return self._file_cache[key]
        ext = path.rsplit(".", 1)[-1]
        klines = parse_day_file(path) if ext == "day" else parse_min_file(path)
        if len(self._file_cache) > 256:
            self._file_cache.clear()
        self._file_cache[key] = klines
        return klines

    def _load_local(self, symbol: str, bar: str) -> List[KLine]:
        """读取本地文件 → 原始K线(未聚合)。无文件返回 []。"""
        if bar in _DAILY_BARS:
            src_bar = "1D"
        elif bar in _MIN_FILE_EXTS:
            src_bar = bar
        elif bar in _MINUTE_AGG_BARS:
            src_bar = _MINUTE_AGG_BARS[bar]   # 3m/2H/4H 从 1m 聚合
        else:
            return []
        for p in self._candidate_files(symbol, src_bar):
            if os.path.isfile(p):
                klines = self._load_file(p)
                if klines:
                    return klines
        return []

    def _resolve(self, symbol: str, bar: str) -> List[KLine]:
        """解析 symbol+bar → 聚合完成的K线; 本地缺失返回 []。"""
        if bar in _DAILY_BARS:
            daily = self._load_local(symbol, "1D")
            return _aggregate_bars(daily, _DAILY_BARS[bar]) if daily else []
        if bar in _MIN_FILE_EXTS:
            return self._load_local(symbol, bar)
        if bar in _MINUTE_AGG_BARS:
            raw = self._load_local(symbol, _MINUTE_AGG_BARS[bar])
            return _aggregate_minute(raw, bar) if raw else []
        return []

    # ---- akshare 回退 ----

    def _get_akshare(self):
        if self._akshare is None:
            from .akshare_adapter import AkshareAdapter
            self._akshare = AkshareAdapter(self.market)
        return self._akshare

    def _with_fallback(self, symbol: str, bar: str, local_fn, remote_fn):
        """本地优先, 空数据时回退 akshare。返回 (klines, 来源标签)。"""
        klines = local_fn()
        if klines:
            return klines, "tdx_local"
        if self.enable_akshare_fallback:
            try:
                k = remote_fn()
                if k:
                    logger.info(
                        f"本地无 [{self.market}/{symbol}/{bar}], 回退 akshare "
                        f"({len(k)}根)"
                    )
                    return k, "akshare"
            except Exception as e:
                logger.warning(f"akshare 回退失败 [{symbol}/{bar}]: {e}")
        return [], "none"

    # ---- ExchangeAdapter 接口 ----

    def fetch_recent(self, symbol: str, bar: str, limit: int = 100) -> List[KLine]:
        def local():
            ks = self._resolve(symbol, bar)
            return ks[-limit:] if limit > 0 else ks
        klines, _src = self._with_fallback(
            symbol, bar, local,
            lambda: self._get_akshare().fetch_recent(symbol, bar, limit),
        )
        if klines:
            self._last_prices[symbol] = klines[-1].close
        return klines

    def fetch_older(self, symbol: str, bar: str, before_ts: int,
                    limit: int = 100) -> List[KLine]:
        def local():
            ks = [k for k in self._resolve(symbol, bar) if k.time < before_ts]
            return ks[-limit:] if limit > 0 else ks
        klines, _src = self._with_fallback(
            symbol, bar, local,
            lambda: self._get_akshare().fetch_older(symbol, bar, before_ts, limit),
        )
        return klines

    def fetch_after(self, symbol: str, bar: str, after_ts: int) -> List[KLine]:
        def local():
            ks = [k for k in self._resolve(symbol, bar) if k.time > after_ts]
            ks.sort(key=lambda k: k.time)
            return ks
        klines, _src = self._with_fallback(
            symbol, bar, local,
            lambda: self._get_akshare().fetch_after(symbol, bar, after_ts),
        )
        return klines

    def fetch_tickers(self) -> List[TickerInfo]:
        """行情列表: 本地已有 .day 文件的代码(离线可见), 无本地数据时回退 akshare。"""
        self._ensure_probed()
        codes: List[str] = []
        if self.market == Market.A_STOCK:
            for _kind, root in self._data_roots.items():
                for mdir in ("sh", "sz", "bj"):
                    ldirs = [os.path.join(root, sub, mdir, "day") for sub in _THS_DATA_SUBDIRS]
                    ldirs.append(os.path.join(root, mdir, "lday"))
                    for ld in ldirs:
                        if not os.path.isdir(ld):
                            continue
                        for fn in sorted(os.listdir(ld)):
                            if fn.endswith(".day"):
                                codes.append(fn[:-4])
        else:
            for _kind, root in self._data_roots.items():
                base = os.path.join(root, "ot", "lday")
                if os.path.isdir(base):
                    for fn in sorted(os.listdir(base)):
                        if fn.endswith(".day"):
                            codes.append(fn[:-4])
        if codes:
            return [TickerInfo(symbol=c, name="", last_price=0.0,
                               change_pct=0.0, volume=0.0, amount=0.0) for c in codes]
        if self.enable_akshare_fallback:
            try:
                return self._get_akshare().fetch_tickers()
            except Exception as e:
                logger.warning(f"akshare tickers 回退失败: {e}")
        return []

    def start_realtime(self, symbol: str, bar: str, on_kline_cb) -> None:
        # 本地文件实时由 DataFeed.start_poll 轮询 fetch_recent 驱动(非 crypto 默认路径),
        # 与 akshare 行为一致: 需在通达信/同花顺客户端运行时文件才会更新。
        raise NotImplementedError("TdxFileAdapter.start_realtime: 本地文件由 REST 轮询驱动")

    def stop_realtime(self) -> None:
        pass

    def fetch_instrument(self, symbol: str) -> InstrumentInfo:
        # 与 akshare 适配器默认规格一致
        if self.market == Market.A_STOCK:
            return InstrumentInfo(symbol=symbol, lot_size=100, price_tick=0.01,
                                  contract_multiplier=1, margin_rate=1.0,
                                  limit_up=0, limit_down=0)
        return InstrumentInfo(symbol=symbol, lot_size=1, price_tick=0.2,
                              contract_multiplier=10, margin_rate=0.1,
                              limit_up=0, limit_down=0)

    def get_last_price(self, symbol: str) -> float:
        if symbol in self._last_prices:
            return self._last_prices[symbol]
        try:
            klines = self.fetch_recent(symbol, "1D", 1)
            if klines:
                return klines[-1].close
        except Exception as e:
            logger.warning(f"TdxFileAdapter.get_last_price 失败 [{symbol}]: {e}")
        return 0.0


# ===== 独立 CLI: 标准 OHLCV JSON 输出 =====

def _klines_to_json(symbol: str, bar: str, market: str, source: str,
                    klines: List[KLine]) -> str:
    payload = {
        "symbol": symbol,
        "bar": bar,
        "market": market,
        "source": source,
        "count": len(klines),
        "klines": [
            {"time": k.time, "open": k.open, "high": k.high,
             "low": k.low, "close": k.close, "volume": k.volume}
            for k in klines
        ],
    }
    return json.dumps(payload, ensure_ascii=False)


def main(argv: Optional[List[str]] = None) -> int:
    """CLI: python -m chanlun_py.exchanges.tdx_adapter [--symbol 000001] [--bar 1D] ..."""
    ap = argparse.ArgumentParser(description="通达信/同花顺本地K线解析 → 标准 OHLCV JSON")
    ap.add_argument("--symbol", default="000001", help="股票代码 (默认 000001)")
    ap.add_argument("--bar", default="1D",
                    choices=sorted(set(_DAILY_BARS) | set(_MIN_FILE_EXTS) | set(_MINUTE_AGG_BARS)),
                    help="K线周期")
    ap.add_argument("--market", default="a_stock", choices=("a_stock", "cn_futures"))
    ap.add_argument("--limit", type=int, default=0, help="只输出最近 N 根 (0=全部)")
    ap.add_argument("--no-fallback", action="store_true", help="禁用 akshare 网络回退")
    args = ap.parse_args(argv)

    adapter = TdxFileAdapter(Market(args.market),
                             enable_akshare_fallback=not args.no_fallback)
    klines = adapter.fetch_recent(args.symbol, args.bar, args.limit) if args.limit > 0 \
        else adapter._resolve(args.symbol, args.bar)
    source = "tdx_local" if klines else ("akshare" if args.limit > 0 else "none")
    if adapter._resolve(args.symbol, args.bar):
        source = "tdx_local"
    print(_klines_to_json(args.symbol, args.bar, args.market, source, klines))
    return 0


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    raise SystemExit(main())
