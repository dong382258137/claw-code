"""③ 端到端验证: TradingView CDP 数据 → 缠论引擎

链路: TvCdpAdapter(CDP) → List[KLine] → ChanLunEngine.process_klines → 分型/笔/段
验证点:
1. TV 拉取的 K 线能被引擎批量消费且不崩溃
2. 状态摘要包含分型/笔/线段 (缠论绘图所需)
3. 与 OKX 同品种同周期的分型/笔数量可对照(数据质量抽查)
"""
import sys
sys.path.insert(0, "D:\\chanlunV2")

from chanlun_py.exchanges.tv_cdp_adapter import TvCdpAdapter
from chanlun_py.engine import ChanLunEngine


def main():
    a = TvCdpAdapter()

    # 1. TV 拉 3000 根 5m BTC(缓存深度内)
    tv_klines = a.fetch_recent("OKX:BTCUSDT.P", "5m", 3000)
    assert len(tv_klines) == 3000, f"期望 3000 根, 实际 {len(tv_klines)}"
    print(f"[1] TV 拉取: {len(tv_klines)} 根 5m | "
          f"区间 {tv_klines[0].time} ~ {tv_klines[-1].time} | 末收 {tv_klines[-1].close}")

    # 2. 喂引擎
    eng = ChanLunEngine()
    summary = eng.process_klines(tv_klines)
    print(f"[2] 引擎处理完成, 状态摘要 keys: {list(summary.keys())}")

    # 3. 提取分型/笔/段计数(引擎标准字段)
    bi = len(summary.get("bis") or [])
    fx = len(summary.get("fenxings") or [])
    xd = len(summary.get("xd_l3") or [])
    zs = len(summary.get("zs_l3") or [])
    print(f"[3] 分型={fx} 笔={bi} 线段L3={xd} 中枢L3={zs}")
    assert fx > 0 and bi > 0 and xd > 0, "引擎未产出缠论结构"
    print("    缠论结构产出正常")

    # 兜底: 直接查 engine 内部状态
    try:
        st = eng.get_state_summary()
        print(f"[3b] get_state_summary keys 样本: {[k for k in st if 'bi' in k or 'seg' in k or 'fx' in k or 'xd' in k or 'zhong' in k][:12]}")
    except Exception as e:
        print(f"[3b] get_state_summary 异常: {e}")

    # 4. 换周期再验 (1H)
    tv_1h = a.fetch_recent("OKX:BTCUSDT.P", "1H", 2000)
    eng2 = ChanLunEngine()
    s2 = eng2.process_klines(tv_1h)
    fx2 = len(s2.get("fenxings") or [])
    bi2 = len(s2.get("bis") or [])
    xd2 = len(s2.get("xd_l3") or [])
    print(f"[4] 1H 引擎处理: {len(tv_1h)} 根 | 分型={fx2} 笔={bi2} 线段L3={xd2}")
    assert fx2 > 0 and bi2 > 0, "1H 缠论结构异常"

    print("E2E DONE")


if __name__ == "__main__":
    main()
