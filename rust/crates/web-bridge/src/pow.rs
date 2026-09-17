//! DeepSeek 网页版 PoW（Proof-of-Work）求解器。
//!
//! 算法参考 openclaw-zero-token（MIT）`deepseek-web-client.ts`：
//! - `sha256`：`input = salt + challenge + nonce`，sha256 十六进制前导 0 bit 数
//!   >= difficulty（difficulty>1000 时取 `log2`），暴力递增 nonce。
//! - `DeepSeekHashV1`：运行官方下发的 wasm（deepseek_hash_v1.wasm.b64.txt），
//!   调用 `wasm_solve(retptr, challenge, prefix, difficulty)`，解为 f64 答案。

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

/// 服务端下发的 PoW 挑战。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PowChallenge {
    pub algorithm: String,
    pub challenge: String,
    pub difficulty: u64,
    pub salt: String,
    #[serde(default)]
    pub signature: Option<String>,
    /// DeepSeekHashV1 需要；sha256 路径忽略。
    #[serde(default)]
    pub expire_at: Option<i64>,
}

/// 挑战统一转为动态数值答案（sha256=nonce 整数，HashV1=f64）。
#[derive(Debug, Clone)]
pub struct PowAnswer {
    pub value: serde_json::Value,
}

impl PowAnswer {
    /// sha256 前导零 bit 计数（与 JS 实现逐位等价）。
    fn sha256_zero_bits(digest_hex: &str) -> u32 {
        let mut bits = 0u32;
        for c in digest_hex.chars() {
            let val = c.to_digit(16).unwrap_or(0);
            if val == 0 {
                bits += 4;
            } else {
                bits += 32 - val.leading_zeros();
                break;
            }
        }
        bits
    }

    /// 求解 sha256 类挑战：返回 nonce。
    pub fn solve_sha256(&self, ch: &PowChallenge) -> Result<u64, String> {
        let target = if ch.difficulty > 1000 {
            (ch.difficulty as f64).log2().floor() as u64
        } else {
            ch.difficulty
        };
        let mut nonce: u64 = 0;
        loop {
            let input = format!("{}{}{}", ch.salt, ch.challenge, nonce);
            let digest = Sha256::digest(input.as_bytes());
            let hex = digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            if Self::sha256_zero_bits(&hex) as u64 >= target {
                return Ok(nonce);
            }
            nonce += 1;
            if nonce > 1_000_000 {
                return Err("sha256 PoW 超时（nonce > 1e6）".to_string());
            }
        }
    }
}

/// 组装 `x-ds-pow-response` 头值：base64(JSON{...challenge, answer, target_path})。
pub fn build_pow_header(
    ch: &PowChallenge,
    answer: serde_json::Value,
    target_path: &str,
) -> Result<String, String> {
    let mut obj = serde_json::to_value(ch).map_err(|e| e.to_string())?;
    if let Some(m) = obj.as_object_mut() {
        m.insert("answer".to_string(), answer);
        m.insert("target_path".to_string(), json!(target_path));
    }
    let bytes = serde_json::to_vec(&obj).map_err(|e| e.to_string())?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        bytes,
    ))
}

// ========== DeepSeekHashV1（wasmi 运行官方 wasm） ==========

const HASHV1_WASM_B64: &str = include_str!("deepseek_hash_v1.wasm.b64.txt");

/// wasmtime 引擎跑官方 DeepSeekHashV1 wasm（行为与浏览器 V8 一致；
/// wasmi 解释器对该 wasm 的求解结果与 V8 不符，已弃用）。
struct HashV1Engine {
    store: wasmtime::Store<()>,
    alloc: wasmtime::TypedFunc<(i32, i32), i32>,
    stack_ptr: wasmtime::TypedFunc<(i32,), i32>,
    solve: wasmtime::TypedFunc<(i32, i32, i32, i32, i32, f64), ()>,
    memory: wasmtime::Memory,
}

impl HashV1Engine {
    fn new() -> Result<Self, String> {
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, HASHV1_WASM_B64)
                .map_err(|e| format!("wasm base64 解码失败: {e}"))?;
        let engine = wasmtime::Engine::default();
        let module =
            wasmtime::Module::new(&engine, &bytes).map_err(|e| format!("wasm 解析失败: {e}"))?;
        let mut store = wasmtime::Store::new(&engine, ());
        let linker = wasmtime::Linker::new(&engine);
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| format!("wasm 实例化失败: {e}"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("wasm 缺少 memory 导出")?;
        let alloc = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "__wbindgen_export_0")
            .map_err(|e| format!("wasm alloc 导出异常: {e}"))?;
        let stack_ptr = instance
            .get_typed_func::<(i32,), i32>(&mut store, "__wbindgen_add_to_stack_pointer")
            .map_err(|e| format!("wasm stack_ptr 导出异常: {e}"))?;
        let solve = instance
            .get_typed_func::<(i32, i32, i32, i32, i32, f64), ()>(&mut store, "wasm_solve")
            .map_err(|e| format!("wasm_solve 导出异常（签名不符?）: {e}"))?;
        Ok(Self {
            store,
            alloc,
            stack_ptr,
            solve,
            memory,
        })
    }

    fn call_alloc(&mut self, n: usize) -> Result<u32, String> {
        self.alloc
            .call(&mut self.store, (n as i32, 1))
            .map(|p| p as u32)
            .map_err(|e| format!("wasm alloc 失败: {e}"))
    }

    fn write_memory(&mut self, ptr: u32, data: &[u8]) -> Result<(), String> {
        self.memory
            .write(&mut self.store, ptr as usize, data)
            .map_err(|e| format!("wasm 内存写入失败: {e}"))
    }

    /// 求解 DeepSeekHashV1：`prefix = {salt}_{expire_at}_`，返回 f64 答案。
    fn solve_challenge(
        &mut self,
        target: &str,
        prefix: &str,
        difficulty: u64,
    ) -> Result<f64, String> {
        let (p_c, l_c) = (self.call_alloc(target.len())?, target.len() as u32);
        let (p_p, l_p) = (self.call_alloc(prefix.len())?, prefix.len() as u32);
        tracing::info!("HashV1 alloc: target@{p_c} len={l_c} prefix@{p_p} len={l_p}");
        self.write_memory(p_c, target.as_bytes())?;
        self.write_memory(p_p, prefix.as_bytes())?;

        // 取一段栈上 16 字节作回执区。
        let ret = self
            .stack_ptr
            .call(&mut self.store, (-16,))
            .map_err(|e| format!("栈指针失败: {e}"))? as u32;

        let start = std::time::Instant::now();
        self.solve
            .call(
                &mut self.store,
                (
                    ret as i32,
                    p_c as i32,
                    l_c as i32,
                    p_p as i32,
                    l_p as i32,
                    difficulty as f64,
                ),
            )
            .map_err(|e| format!("wasm_solve 失败: {e}"))?;
        tracing::info!("HashV1 求解耗时: {:?}", start.elapsed());

        // 与 JS 参考实现顺序一致：先读回执区，再归还栈指针。
        let status = self.read_i32(ret).ok_or("读取 status 失败")?;
        let mut ret_bytes = [0u8; 16];
        let _ = self.memory.read(&self.store, ret as usize, &mut ret_bytes);
        let answer = self
            .read_f64(ret + 8)
            .ok_or_else(|| "读取 answer 失败".to_string())?;
        let _ = self.stack_ptr.call(&mut self.store, (16,));

        tracing::info!("HashV1 结果: status={status} answer={answer} ret16={ret_bytes:02x?}");
        // 与 JS 参考一致：status == 0 表示未找到解，非 0 表示求解成功。
        if status == 0 {
            return Err("DeepSeekHashV1 未找到解".to_string());
        }
        Ok(answer)
    }

    fn read_i32(&mut self, ptr: u32) -> Option<i32> {
        let mut buf = [0u8; 4];
        self.memory.read(&self.store, ptr as usize, &mut buf).ok()?;
        Some(i32::from_le_bytes(buf))
    }

    fn read_f64(&mut self, ptr: u32) -> Option<f64> {
        let mut buf = [0u8; 8];
        self.memory.read(&self.store, ptr as usize, &mut buf).ok()?;
        Some(f64::from_le_bytes(buf))
    }
}

/// 统一求解入口：按 algorithm 分派，返回 JSON 数值答案。
pub fn solve(ch: &PowChallenge) -> Result<serde_json::Value, String> {
    match ch.algorithm.as_str() {
        "sha256" => {
            let nonce = PowAnswer::solve_sha256(&PowAnswer { value: json!(0) }, ch)?;
            Ok(json!(nonce))
        }
        "DeepSeekHashV1" => {
            let expire = ch.expire_at.ok_or("DeepSeekHashV1 缺少 expire_at")?;
            let prefix = format!("{}_{}_", ch.salt, expire);
            // 高难度下偶发求解失败，重建引擎重试（每次全新实例，避免状态残留）。
            let mut last_err = String::from("求解未尝试");
            for attempt in 1..=3 {
                match HashV1Engine::new()
                    .and_then(|mut e| e.solve_challenge(&ch.challenge, &prefix, ch.difficulty))
                {
                    Ok(answer) => {
                        tracing::info!("HashV1 求解成功（尝试 {attempt}）answer={answer}");
                        // 整数 f64 必须以整数形式入 JSON（服务端校验数字格式，
                        // 参考实现 JS 序列化无小数点）。
                        return Ok(if answer.fract() == 0.0 {
                            json!(answer as i64)
                        } else {
                            json!(answer)
                        });
                    }
                    Err(e) => {
                        last_err = e;
                        tracing::warn!("HashV1 第 {attempt} 次求解失败: {last_err}");
                    }
                }
            }
            Err(format!("DeepSeekHashV1 连续 3 次未找到解：{last_err}"))
        }
        other => Err(format!("不支持的 PoW 算法: {other}")),
    }
}

/// 便捷包装：挑战 → x-ds-pow-response 头值。
pub fn solve_to_header(ch: &PowChallenge, target_path: &str) -> Result<String, String> {
    let answer = solve(ch)?;
    build_pow_header(ch, answer, target_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha256 挑战：难度 5 应在毫秒级解出且可复现。
    #[test]
    fn sha256_pow_solves_and_verifies() {
        let ch = PowChallenge {
            algorithm: "sha256".into(),
            challenge: "abc".into(),
            difficulty: 5,
            salt: "salt".into(),
            signature: None,
            expire_at: None,
        };
        let nonce = PowAnswer { value: json!(0) }.solve_sha256(&ch).unwrap();
        let input = format!("{}{}{}", ch.salt, ch.challenge, nonce);
        let hex: String = Sha256::digest(input.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            PowAnswer::sha256_zero_bits(&hex) >= 5,
            "nonce={nonce} bits={}",
            PowAnswer::sha256_zero_bits(&hex)
        );
    }
}
