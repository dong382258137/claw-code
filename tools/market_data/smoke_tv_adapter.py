"""TvCdpAdapter 端到端冒烟验证 (2 阶段)"""
import sys
sys.path.insert(0, "D:\\chanlunV2")

from chanlun_py.exchanges.tv_cdp_adapter import TvCdpAdapter, CdpError
from chanlun_py.csv_data import validate_klines

a = TvCdpAdapter()


def check(name, rows, bar, market="crypto", expect_n=None):
    n = len(rows)
    ok = expect_n is None or n >= expect_n
    print(f"[{'OK' if ok else 'FAIL'}] {name}: {n} 根 | 首={rows[0].time if rows else '-'} 末={rows[-1].time if rows else '-'} 末收={rows[-1].close if rows else '-'}")
    if rows and ok:
        vk = validate_klines(rows, bar, market=market)
        print(f"      validate_klines 通过 {len(vk)}/{n}")
    return ok and rows


# 1. 从 ETH(当前)切回 BTC 5m — 竞态修复验证
r1 = check("BTC 5m recent", a.fetch_recent("OKX:BTCUSDT.P", "5m", 5), "5m", expect_n=5)
# 2. 交叉切换 ETH 1H
r2 = check("ETH 1H recent", a.fetch_recent("OKX:ETHUSDT.P", "1H", 5), "1H", expect_n=5)
# 3. 再切回 BTC 1D
r3 = check("BTC 1D recent", a.fetch_recent("OKX:BTCUSDT.P", "1D", 5), "1D", expect_n=5)
# 4. 历史: 10 天前 BTC 5m (缓存深度内 ~6000 根, 10天=2880根安全)
if r1:
    before = r1[0].time - 10 * 86400_000
    r4 = check("BTC 5m older(10d前)", a.fetch_older("OKX:BTCUSDT.P", "5m", before, 10), "5m", expect_n=10)
# 4b. 深度边界验证: 40 天前 5m 超出缓存深度(~6000根=21天) → 允许返回 < expect
if r1:
    before_deep = r1[0].time - 40 * 86400_000
    r4b = check("BTC 5m older(40d前,超深度)", a.fetch_older("OKX:BTCUSDT.P", "5m", before_deep, 10), "5m")
# 4c. older 之后 recent 应恢复正常(末尾最新)
if r4:
    r4c = check("BTC 5m recent(older后)", a.fetch_recent("OKX:BTCUSDT.P", "5m", 5), "5m", expect_n=5)
    if r4c and r4c[-1].time < r4[-1].time:
        print("[FAIL] recent 末根早于 older 末根(缓存未回到最新)")
    else:
        print("[OK] recent 恢复最新")
# 5. 间隙补全: after 某时刻
if r1:
    after_ts = r1[0].time - 86400_000
    r5 = check("BTC 5m after(1d前)", a.fetch_after("OKX:BTCUSDT.P", "5m", after_ts), "5m", expect_n=1)
# 6. 最新价
try:
    px = a.get_last_price("OKX:BTCUSDT.P")
    print(f"[OK] get_last_price: {px}")
except Exception as e:
    print(f"[FAIL] get_last_price: {e}")

print("DONE")
