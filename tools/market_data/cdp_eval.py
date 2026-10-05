"""CDP Runtime.evaluate 通用求值器 — 用于实证调研 TradingView 页面数据提取路径。

用法: python cdp_eval.py <page_ws_url> "<js_expression>"
输出: JSON (含 result.result.value 或异常信息)
"""
import json
import sys
import websocket


def main():
    ws_url, expr = sys.argv[1], sys.argv[2]
    origin = sys.argv[3] if len(sys.argv) > 3 else "devtools://devtools"
    ws = websocket.create_connection(ws_url, timeout=60, origin=origin)

    def send(method, params=None, mid=1):
        ws.send(json.dumps({"id": mid, "method": method, "params": params or {}}))
        while True:
            msg = json.loads(ws.recv())
            if msg.get("id") == mid:
                return msg

    send("Runtime.enable")
    r = send("Runtime.evaluate", {
        "expression": expr,
        "returnByValue": True,
        "awaitPromise": True,
    })
    print(json.dumps(r, ensure_ascii=False))
    ws.close()


if __name__ == "__main__":
    main()
