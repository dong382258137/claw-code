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
            let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
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
pub fn build_pow_header(ch: &PowChallenge, answer: serde_json::Value, target_path: &str) -> Result<String, String> {
    let mut obj = serde_json::to_value(ch).map_err(|e| e.to_string())?;
    if let Some(m) = obj.as_object_mut() {
        m.insert("answer".to_string(), answer);
        m.insert("target_path".to_string(), json!(target_path));
    }
    let bytes = serde_json::to_vec(&obj).map_err(|e| e.to_string())?;
    Ok(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes))
}

// ========== DeepSeekHashV1（wasmi 运行官方 wasm） ==========

const HASHV1_WASM_B64: &str = include_str!("deepseek_hash_v1.wasm.b64.txt");

struct HashV1Engine {
    store: wasmi::Store<()>,
    alloc: wasmi::Func,
    stack_ptr: wasmi::Func,
    solve: wasmi::Func,
    memory: wasmi::Memory,
}

impl HashV1Engine {
    fn new() -> Result<Self, String> {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, HASHV1_WASM_B64)
            .map_err(|e| format!("wasm base64 解码失败: {e}"))?;
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, bytes.as_slice()).map_err(|e| format!("wasm 解析失败: {e}"))?;
        let mut store = wasmi::Store::new(&engine, ());
        let linker = wasmi::Linker::new(&engine);
        let instance = linker
            .instantiate(&mut store, &module)
            .and_then(|i| i.start(&mut store))
            .map_err(|e| format!("wasm 实例化失败: {e}"))?;
        let get = |name: &str| -> Result<wasmi::Func, String> {
            instance
                .get_export(&store, name)
                .and_then(|v| v.into_func())
                .ok_or_else(|| format!("wasm 缺少导出 {name}"))
        };
        let solve = get("wasm_solve")?;
        let alloc = get("__wbindgen_export_0")?;
        let stack_ptr = get("__wbindgen_add_to_stack_pointer")?;
        let memory = instance
            .get_export(&store, "memory")
            .and_then(|v| v.into_memory())
            .ok_or_else(|| "wasm 缺少 memory 导出".to_string())?;
        Ok(Self { store, alloc, stack_ptr, solve, memory })
    }

    fn call_alloc(&mut self, n: usize) -> Result<u32, String> {
        let mut results = [wasmi::Value::I32(0)];
        self.alloc
            .call(
                &mut self.store,
                &[wasmi::Value::I32(n as i32), wasmi::Value::I32(1)],
                &mut results,
            )
            .map_err(|e| format!("wasm alloc 失败: {e}"))?;
        match results[0] {
            wasmi::Value::I32(p) => Ok(p as u32),
            _ => Err("alloc 返回异常".to_string()),
        }
    }

    fn write_memory(&mut self, ptr: u32, data: &[u8]) {
        let _ = self.memory.write(&mut self.store, ptr as usize, data);
    }

    /// 求解 DeepSeekHashV1：`prefix = {salt}_{expire_at}_`，返回 f64 答案。
    fn solve_challenge(&mut self, target: &str, prefix: &str, difficulty: u64) -> Result<f64, String> {
        let (p_c, l_c) = (self.call_alloc(target.len())?, target.len() as u32);
        let (p_p, l_p) = (self.call_alloc(prefix.len())?, prefix.len() as u32);
        tracing::info!("HashV1 alloc: target@{p_c} len={l_c} prefix@{p_p} len={l_p}");
        self.write_memory(p_c, target.as_bytes());
        self.write_memory(p_p, prefix.as_bytes());
        // 回读校验写入
        let mut rb = vec![0u8; 16.min(l_c as usize)];
        let _ = self.memory.read(&mut self.store, p_c as usize, &mut rb);
        tracing::info!(
            "HashV1 写入回读 target前{}: {:02x?}",
            rb.len(),
            &rb[..]
        );
        let mut rbp = vec![0u8; 16.min(l_p as usize)];
        let _ = self.memory.read(&mut self.store, p_p as usize, &mut rbp);
        tracing::info!(
            "HashV1 写入回读 prefix前{}: {:02x?}",
            rbp.len(),
            &rbp[..]
        );

        // 取一段栈上 16 字节作回执区。
        let mut stack_result = [wasmi::Value::I32(0)];
        self.stack_ptr
            .call(&mut self.store, &[wasmi::Value::I32(-16)], &mut stack_result)
            .map_err(|e| format!("栈指针失败: {e}"))?;
        let ret = match stack_result[0] {
            wasmi::Value::I32(v) => v as u32,
            _ => return Err("栈指针返回异常".to_string()),
        };

        // 按 wasm 声明的函数签名动态组装参数（官方 wasm 的 difficulty
        // 参数类型可能随版本变化，i32 与 f64 均按签名适配）。
        let ty = self.solve.ty(&self.store);
        let params = ty.params().to_vec();
        let mut fixed = vec![
            wasmi::Value::I32(ret as i32),
            wasmi::Value::I32(p_c as i32),
            wasmi::Value::I32(l_c as i32),
            wasmi::Value::I32(p_p as i32),
            wasmi::Value::I32(l_p as i32),
        ];
        for (i, pt) in params.iter().enumerate() {
            if i < fixed.len() {
                continue;
            }
            match pt {
                wasmi::core::ValueType::I32 => fixed.push(wasmi::Value::I32(difficulty as i32)),
                wasmi::core::ValueType::I64 => fixed.push(wasmi::Value::I64(difficulty as i64)),
                wasmi::core::ValueType::F32 => fixed.push(wasmi::Value::F32((difficulty as f32).into())),
                wasmi::core::ValueType::F64 => fixed.push(wasmi::Value::F64(difficulty.into())),
                _ => return Err(format!("wasm_solve 含不支持参数类型 {pt:?}")),
            }
        }
        tracing::debug!("wasm_solve 签名: params={:?}", ty.params());

        let mut solve_result = vec![wasmi::Value::I32(0); ty.results().len()];
        self.solve
            .call(&mut self.store, &fixed, &mut solve_result)
            .map_err(|e| format!("wasm_solve 失败: {e}"))?;

        let mut stack_restore = [wasmi::Value::I32(0)];
        self.stack_ptr
            .call(&mut self.store, &[wasmi::Value::I32(16)], &mut stack_restore)
            .map_err(|e| format!("栈指针归还失败: {e}"))?;

        let status = self.read_i32(ret).ok_or("读取 status 失败")?;
        let mut ret_bytes = [0u8; 16];
        let _ = self.memory.read(&mut self.store, ret as usize, &mut ret_bytes);
        let answer = self.read_f64(ret + 8).ok_or_else(|| "读取 answer 失败".to_string())?;
        tracing::info!("HashV1 结果: status={status} answer={answer} ret16={ret_bytes:02x?}");
        if status != 0 {
            return Err("DeepSeekHashV1 未找到解".to_string());
        }
        Ok(answer)
    }

    fn read_i32(&mut self, ptr: u32) -> Option<i32> {
        let mut buf = [0u8; 4];
        self.memory.read(&mut self.store, ptr as usize, &mut buf).ok()?;
        Some(i32::from_le_bytes(buf))
    }

    fn read_f64(&mut self, ptr: u32) -> Option<f64> {
        let mut buf = [0u8; 8];
        self.memory.read(&mut self.store, ptr as usize, &mut buf).ok()?;
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
            let mut engine = HashV1Engine::new()?;
            let answer = engine.solve_challenge(&ch.challenge, &prefix, ch.difficulty)?;
            Ok(json!(answer))
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
        let hex: String = Sha256::digest(input.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert!(PowAnswer::sha256_zero_bits(&hex) >= 5, "nonce={nonce} bits={}", PowAnswer::sha256_zero_bits(&hex));
    }
}