//! 网页版"工具调用协议":增强提示的渲染 + 网页回复的解析。
//!
//! 设计参考 Cuckoo Code(零 Token 桌面 Agent,参考实现位于
//! `references/cuckoo-code`):协议不是裸命令/裸 JSON,而是"专属语言标签 +
//! JS 函数调用"。模型在回复中输出一个 ```claw 代码块,用 `await` 调用白名单
//! 工具函数(如 `await read("a.js")`),本地解析函数调用并映射到 claw 工具执行,
//! 结果经 `<|tool_result|>` 渲染回对话让模型继续。
//!
//! ## 防误判(借鉴 cuckoo `js-detector`)
//! - ```claw 代码块:无条件视为工具脚本(专属标签);
//! - ```js / ```javascript 代码块:仅当整条回复只含代码块且代码命中
//!   `await 白名单函数(` 时才执行,避免把普通回答里的示例代码当工具执行;
//! - ```bash / ```sh / ```shell:兼容旧协议,转 bash 工具(命令经危险预检);
//! - 其余文本:最终回复。

use runtime::{ContentBlock, ConversationMessage, MessageRole, SystemPromptSplit};

/// 工具调用围栏标记(旧协议,调试/测试保留)。
pub const TOOL_CALLS_FENCE: &str = "```tool_calls";
/// 专属工具代码块语言标签(claw 主协议)。
pub const CLAW_FENCE: &str = "```claw";

/// 渲染进增强提示末尾的协议说明(告诉网页版模型如何申请执行本地操作)。
pub const PROTOCOL_SUFFIX: &str = "\n\n[claw 工具协议]\n\
你在代替 claw 在用户的电脑上执行任务。如果需要实际操作电脑(读写文件、\
搜索代码、执行命令等),请在回复中输出一个 claw 代码块,用 await 调用工具函数,\
例如:\n\
```claw\nconst text = await read(\"path/to/file\");\n\
await write(\"path/to/file\", text.replace(\"a\", \"b\"));\n```\n\
可用的函数:read(path)、write(path, content)、edit(path, old, new, replaceAll?)、\
glob(pattern, searchPath?)、grep(pattern, options?)、bash(command)、\
todoWrite(todos)、webFetch(url)。\n\
规则:\n\
- 参数必须是字面量(字符串/数字/布尔),不要用变量名或模板字符串;\n\
- 执行结果会以新消息返回,请根据结果继续操作,直到任务完成;\n\
- 只是展示给你的代码内容请放普通代码块,不要使用 claw;\n\
- 任务全部完成后,用纯文字总结结果,不要再输出代码块。";

/// 把一个 `ApiRequest` 渲染成发给网页的"增强提示"文本。
///
/// 结构:claw 系统提示 → 可用工具清单 → 对话记录(带 `<|tool_result|>` 标记)→ 协议。
#[must_use]
pub fn render_enhanced_prompt(
    system: &SystemPromptSplit,
    messages: &[ConversationMessage],
    tool_names: &str,
) -> String {
    let mut out = String::new();
    out.push_str("[claw instructions]\n");
    for section in system
        .static_sections
        .iter()
        .chain(system.dynamic_sections.iter())
    {
        if !section.is_empty() {
            out.push_str(section);
            out.push('\n');
        }
    }
    out.push_str("\n[available tools]\n");
    if tool_names.is_empty() {
        out.push_str("(none)");
    } else {
        out.push_str(tool_names);
    }
    out.push_str("\n\n[conversation]\n");
    for msg in messages {
        let role = match msg.role {
            MessageRole::User => "<|user|>",
            MessageRole::Assistant => "<|assistant|>",
            MessageRole::Tool => "<|tool_result|>",
            MessageRole::System => "<|system|>",
        };
        let body = msg
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::ToolResult {
                    tool_name, output, ..
                } => Some(format!("[{tool_name}]\n{output}")),
                ContentBlock::Thinking { .. } | ContentBlock::ToolUse { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!("{role} {body}\n"));
    }
    out.push_str(PROTOCOL_SUFFIX);
    out
}

/// 解析结果。
#[derive(Debug)]
pub struct ParseOutcome {
    /// 围栏之外的文本(最终回复)。
    pub text: String,
    /// 解析出的工具调用。
    pub calls: Vec<WebToolCall>,
    /// 是否存在未闭合的围栏或调用(模型输出被截断)。
    pub truncated: bool,
    /// 解析失败的工具调用描述(参数非字面量等),供提示模型重写。
    pub unparsed: Vec<String>,
}

/// 网页回复中解析出的单个工具调用。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WebToolCall {
    pub name: String,
    #[serde(default = "empty_args")]
    pub args: serde_json::Value,
}

fn empty_args() -> serde_json::Value {
    serde_json::json!({})
}

/// 解析网页回复。
#[must_use]
pub fn parse_reply(reply: &str) -> ParseOutcome {
    let mut outcome = ParseOutcome {
        text: String::new(),
        calls: Vec::new(),
        truncated: false,
        unparsed: Vec::new(),
    };

    let lines: Vec<&str> = reply.split('\n').collect();
    let mut in_block = false;
    let mut lang = String::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut rest: Vec<&str> = Vec::new();
    let mut saw_fence_open = false;

    // 逐行扫描围栏;记录未闭合(截断)。
    let flush = |outcome: &mut ParseOutcome, lang: &str, code: &str, only_fences: bool| {
        let code = code.trim();
        if code.is_empty() {
            return;
        }
        let l = lang.to_ascii_lowercase();
        match l.as_str() {
            "claw" => {
                let (calls, unparsed) = extract_calls_from_code(code);
                outcome.calls.extend(calls);
                outcome.unparsed.extend(unparsed);
            }
            "js" | "javascript" if only_fences && contains_tool_call(code) => {
                // 仅当整条回复只含代码块且命中工具函数调用才执行(防误判)。
                let (calls, unparsed) = extract_calls_from_code(code);
                outcome.calls.extend(calls);
                outcome.unparsed.extend(unparsed);
            }
            "bash" | "sh" | "shell" => {
                outcome.calls.push(WebToolCall {
                    name: "bash".to_string(),
                    args: serde_json::json!({ "command": code }),
                });
            }
            // 旧协议:tool_calls 围栏内的 JSON 数组(调试/测试保留)
            "tool_calls" => {
                if let Ok(calls) = serde_json::from_str::<Vec<WebToolCall>>(code) {
                    outcome.calls.extend(calls);
                } else {
                    outcome
                        .unparsed
                        .push("tool_calls 块 JSON 解析失败".to_string());
                }
            }
            _ => {}
        }
    };

    for line in &lines {
        let trimmed = line.trim();
        if !in_block {
            if let Some(rest_open) = trimmed.strip_prefix("```") {
                lang = rest_open
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                in_block = true;
                buf.clear();
                saw_fence_open = true;
            } else {
                rest.push(line);
            }
        } else if trimmed.starts_with("```") {
            // 闭合围栏
            flush(&mut outcome, &lang, &buf.join("\n"), false);
            in_block = false;
            lang.clear();
        } else {
            buf.push(line);
        }
    }
    // 未闭合:要么是截断的真实输出,要么是纯文本最后几行被误判。
    if in_block {
        if saw_fence_open {
            outcome.truncated = true;
        }
        // 未闭合的块不执行(视为截断)。
        rest.extend(buf);
    }

    // 整条回复是否"只含代码块"(围栏之外的可见文本为空;用于 js 块防误判)。
    let mut only_fences = true;
    {
        let mut in_fence = false;
        for line in reply.split('\n') {
            let t = line.trim();
            if t.starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if !in_fence && !t.is_empty() {
                only_fences = false;
                break;
            }
        }
    }
    // 若存在 js 块却未被 flush(仅在整条仅含代码块分支)——上方的 flush 使用的是
    // only_fences=false,这里需对 js 块重新按 only_fences 判定。
    // 简化:上面 flush 对 js 用 only_fences=false 会跳过;补一遍 js 块(仅在 only_fences 时)。
    if only_fences {
        // 重新解析整个回复中的 js 块(仅当只含代码块)
        let (js_calls, js_unparsed) = extract_js_blocks_only(reply);
        outcome.calls.extend(js_calls);
        outcome.unparsed.extend(js_unparsed);
    }

    outcome.text = rest.join("\n").trim().to_string();
    outcome
}

/// 整条回复只含代码块时,单独提取其中的 js/javascript 块并解析。
fn extract_js_blocks_only(reply: &str) -> (Vec<WebToolCall>, Vec<String>) {
    let mut calls = Vec::new();
    let mut unparsed = Vec::new();
    let lines: Vec<&str> = reply.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if let Some(rest_open) = t.strip_prefix("```") {
            let lang = rest_open
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if lang == "js" || lang == "javascript" {
                let mut buf = Vec::new();
                i += 1;
                while i < lines.len() && !lines[i].trim().starts_with("```") {
                    buf.push(lines[i]);
                    i += 1;
                }
                let code = buf.join("\n").trim().to_string();
                if contains_tool_call(&code) {
                    let (c, u) = extract_calls_from_code(&code);
                    calls.extend(c);
                    unparsed.extend(u);
                }
            }
        }
        i += 1;
    }
    (calls, unparsed)
}

/// 工具函数白名单调用特征检测:`await read(` 等(字符串扫描,无 regex 依赖)。
fn contains_tool_call(code: &str) -> bool {
    const FNS: &[&str] = &[
        "read",
        "write",
        "edit",
        "glob",
        "grep",
        "bash",
        "todoWrite",
        "webFetch",
    ];
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i + 6 <= chars.len() {
        // 找 "await"
        if chars[i..i + 5] != ['a', 'w', 'a', 'i', 't'] {
            i += 1;
            continue;
        }
        let mut j = i + 5;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        for f in FNS {
            let fc: Vec<char> = f.chars().collect();
            if j + fc.len() <= chars.len() && &chars[j..j + fc.len()] == fc.as_slice() {
                let mut k = j + fc.len();
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if k < chars.len() && chars[k] == '(' {
                    return true;
                }
            }
        }
        i = j;
    }
    false
}

/// 从一段 claw/js 代码中提取全部工具函数调用。
///
/// 通过括号配对找到每个 `await fn(` 的实参区间,再把 JS 对象字面量参数
/// 宽松转换为 JSON。解析失败(变量名/模板字符串等非字面量)记入 `unparsed`。
pub fn extract_calls_from_code(code: &str) -> (Vec<WebToolCall>, Vec<String>) {
    let mut calls = Vec::new();
    let mut unparsed = Vec::new();
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    let n = chars.len();
    while i < n {
        // 找 `await` 或函数名起点
        if !(chars[i] == 'a'
            && chars.get(i + 1) == Some(&'w')
            && chars.get(i + 2) == Some(&'a')
            && chars.get(i + 3) == Some(&'i')
            && chars.get(i + 4) == Some(&'t'))
        {
            i += 1;
            continue;
        }
        // 跳过空白,读函数名
        let mut j = i + 5;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        let name_start = j;
        while j < n && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '$') {
            j += 1;
        }
        let name: String = chars[name_start..j].iter().collect();
        if !is_whitelisted(&name) {
            i = j;
            continue;
        }
        let mut k = j;
        while k < n && chars[k].is_whitespace() {
            k += 1;
        }
        if k >= n || chars[k] != '(' {
            i = j;
            continue;
        }
        // 括号配对找实参区间
        let mut depth = 0usize;
        let mut m = k;
        let mut in_str = false;
        let mut str_q = ' ';
        let mut esc = false;
        while m < n {
            let c = chars[m];
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == str_q {
                    in_str = false;
                }
            } else {
                match c {
                    '"' | '\'' => {
                        in_str = true;
                        str_q = c;
                    }
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            m += 1;
        }
        if m >= n {
            // 括号未闭合:调用被截断
            unparsed.push(format!("{name}(...) 括号未闭合(疑似截断)"));
            break;
        }
        let args_src: String = chars[k + 1..m].iter().collect();
        match map_call(&name, &args_src) {
            Ok(call) => calls.push(call),
            Err(e) => unparsed.push(format!("{name} 参数解析失败:{e}")),
        }
        i = m + 1;
    }
    (calls, unparsed)
}

fn is_whitelisted(name: &str) -> bool {
    matches!(
        name,
        "read" | "write" | "edit" | "glob" | "grep" | "bash" | "todoWrite" | "webFetch"
    )
}

/// 位置参数若为对象字面量则取其中同名键(兼容 `read({path: "x"})` 写法)。
fn as_key(value: serde_json::Value, key: &str) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => map
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Object(map)),
        other => other,
    }
}

/// 把 `fnName(argsSrc)` 的实参按位置映射为 claw 工具调用。
///
/// 参数是位置参数(`await read("a.js")`),先按顶层逗号切分,每个实参必须
/// 是可解析的字面量(字符串/数字/布尔/null);变量/模板字符串 → Err。
fn map_call(name: &str, args_src: &str) -> Result<WebToolCall, String> {
    let args = split_top_level_args(args_src);
    let get = |i: usize| -> Result<serde_json::Value, String> {
        let src = args
            .get(i)
            .cloned()
            .ok_or_else(|| format!("缺少第 {} 个参数", i + 1))?;
        js_object_to_json(&src).map_err(|e| format!("参数 {i}:{e}"))
    };
    let tc = match name {
        "read" => WebToolCall {
            name: "read_file".to_string(),
            args: serde_json::json!({ "path": as_key(get(0)?, "path") }),
        },
        "write" => WebToolCall {
            name: "write_file".to_string(),
            args: serde_json::json!({
                "path": as_key(get(0)?, "path"),
                "content": as_key(get(1)?, "content"),
            }),
        },
        "edit" => WebToolCall {
            name: "edit_file".to_string(),
            args: serde_json::json!({
                "path": as_key(get(0)?, "path"),
                "old_string": as_key(get(1)?, "old"),
                "new_string": as_key(get(2)?, "new"),
                "replace_all": args
                    .get(3)
                    .and_then(|s| js_object_to_json(s).ok())
                    .unwrap_or(serde_json::json!(false)),
            }),
        },
        "glob" => {
            let mut map = serde_json::Map::new();
            map.insert("pattern".to_string(), as_key(get(0)?, "pattern"));
            if let Some(p) = args.get(1) {
                if let Ok(v) = js_object_to_json(p) {
                    map.insert("path".to_string(), as_key(v, "searchPath"));
                }
            }
            WebToolCall {
                name: "glob_search".to_string(),
                args: serde_json::Value::Object(map),
            }
        }
        "grep" => {
            let mut map = serde_json::Map::new();
            map.insert("pattern".to_string(), as_key(get(0)?, "pattern"));
            if let Some(opt) = args.get(1) {
                if let Ok(serde_json::Value::Object(o)) = js_object_to_json(opt) {
                    if let Some(p) = o.get("path") {
                        map.insert("path".to_string(), p.clone());
                    }
                }
            }
            WebToolCall {
                name: "grep_search".to_string(),
                args: serde_json::Value::Object(map),
            }
        }
        "bash" => WebToolCall {
            name: "bash".to_string(),
            args: serde_json::json!({ "command": as_key(get(0)?, "command") }),
        },
        "todoWrite" => WebToolCall {
            name: "TodoWrite".to_string(),
            args: serde_json::json!({ "todos": as_key(get(0)?, "todos") }),
        },
        "webFetch" => WebToolCall {
            name: "WebFetch".to_string(),
            args: serde_json::json!({ "url": as_key(get(0)?, "url") }),
        },
        _ => return Err(format!("unknown tool fn {name}")),
    };
    Ok(tc)
}

/// 按顶层逗号切分实参(忽略字符串与嵌套括号内的逗号)。
fn split_top_level_args(src: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut str_q = ' ';
    let mut esc = false;
    let mut cur = String::new();
    for c in src.chars() {
        if in_str {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == str_q {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                in_str = true;
                str_q = c;
                cur.push(c);
            }
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

/// 把 JS 对象字面量宽松转换为 JSON。
///
/// 支持:对象(裸键/引号键)、双/单引号字符串(含转义)、数字、布尔、null、数组。
/// 变量名、模板字符串等非字面量 → Err(提示模型改写字面量)。
pub fn js_object_to_json(src: &str) -> Result<serde_json::Value, String> {
    let mut p = JsParser {
        chars: src.chars().collect(),
        pos: 0,
    };
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(format!("存在无法解析的尾部内容(位置 {})", p.pos));
    }
    Ok(v)
}

struct JsParser {
    chars: Vec<char>,
    pos: usize,
}

impl JsParser {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<serde_json::Value, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') | Some('\'') => Ok(serde_json::Value::String(self.parse_string()?)),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(format!("参数含非字面量内容(位置 {})", self.pos)),
        }
    }

    fn parse_object(&mut self) -> Result<serde_json::Value, String> {
        self.pos += 1; // '{'
        let mut map = serde_json::Map::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    return Ok(serde_json::Value::Object(map));
                }
                Some(',') => {
                    self.pos += 1;
                }
                _ => {
                    let key = self.parse_key()?;
                    self.skip_ws();
                    if self.peek() != Some(':') {
                        return Err(format!("对象键 {key} 后缺少 ':'"));
                    }
                    self.pos += 1;
                    let value = self.parse_value()?;
                    map.insert(key, value);
                }
            }
        }
    }

    fn parse_key(&mut self) -> Result<String, String> {
        self.skip_ws();
        match self.peek() {
            Some('"') | Some('\'') => self.parse_string(),
            Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = self.pos;
                while self.pos < self.chars.len()
                    && (self.chars[self.pos].is_alphanumeric()
                        || self.chars[self.pos] == '_'
                        || self.chars[self.pos] == '$')
                {
                    self.pos += 1;
                }
                Ok(self.chars[start..self.pos].iter().collect())
            }
            _ => Err(format!("无法解析对象键(位置 {})", self.pos)),
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let q = self.peek().ok_or("字符串未闭合")?;
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            self.pos += 1;
            if c == q {
                return Ok(out);
            }
            if c == '\\' {
                let e = self.chars.get(self.pos);
                self.pos += 1;
                match e {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some('\'') => out.push('\''),
                    Some('/') => out.push('/'),
                    Some(other) => out.push(*other),
                    None => return Err("字符串转义未完成".to_string()),
                }
            } else {
                out.push(c);
            }
        }
        Err("字符串未闭合".to_string())
    }

    fn parse_number(&mut self) -> Result<serde_json::Value, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_digit() || self.chars[self.pos] == '.')
        {
            self.pos += 1;
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
            .map(serde_json::Value::from)
            .map_err(|_| format!("数字解析失败: {s}"))
    }

    fn parse_array(&mut self) -> Result<serde_json::Value, String> {
        self.pos += 1; // '['
        let mut arr = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(']') => {
                    self.pos += 1;
                    return Ok(serde_json::Value::Array(arr));
                }
                Some(',') => {
                    self.pos += 1;
                }
                _ => {
                    arr.push(self.parse_value()?);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::ContentBlock as CB;

    fn msg(role: MessageRole, block: CB) -> ConversationMessage {
        ConversationMessage {
            role,
            blocks: vec![block],
            usage: None,
        }
    }

    fn empty_system() -> SystemPromptSplit {
        SystemPromptSplit {
            static_sections: vec!["L0 instructions".to_string()],
            dynamic_sections: vec![],
        }
    }

    #[test]
    fn render_includes_roles_and_protocol() {
        let prompt = render_enhanced_prompt(
            &empty_system(),
            &[
                msg(
                    MessageRole::User,
                    CB::Text {
                        text: "hello".to_string(),
                    },
                ),
                msg(
                    MessageRole::Tool,
                    CB::ToolResult {
                        tool_use_id: "x".to_string(),
                        tool_name: "read_file".to_string(),
                        output: "file-content".to_string(),
                        is_error: false,
                    },
                ),
            ],
            "read_file, bash",
        );
        assert!(prompt.contains("<|user|> hello"));
        assert!(prompt.contains("<|tool_result|> [read_file]\nfile-content"));
        assert!(prompt.contains("read_file, bash"));
        // 主协议:claw 代码块
        assert!(prompt.contains(CLAW_FENCE));
    }

    #[test]
    fn parse_claw_block_maps_functions() {
        let reply = "我先读文件。\n```claw\nconst t = await read(\"a.js\");\nawait write(\"a.js\", \"hello content\");\n```\n已完成。";
        let out = parse_reply(reply);
        assert_eq!(out.text, "我先读文件。\n已完成。");
        assert!(!out.truncated);
        assert!(out.unparsed.is_empty(), "unparsed: {:?}", out.unparsed);
        assert_eq!(out.calls.len(), 2);
        assert_eq!(out.calls[0].name, "read_file");
        assert_eq!(out.calls[0].args["path"], "a.js");
        assert_eq!(out.calls[1].name, "write_file");
        assert_eq!(out.calls[1].args["path"], "a.js");
        assert_eq!(out.calls[1].args["content"], "hello content");
    }

    #[test]
    fn parse_claw_bare_keys_and_single_quotes() {
        let reply = "```claw\nawait read({path: 'x/y.js'});\n```";
        let out = parse_reply(reply);
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].name, "read_file");
        assert_eq!(out.calls[0].args["path"], "x/y.js");
    }

    #[test]
    fn js_block_in_plain_answer_is_not_executed() {
        // 普通回答里的 js 示例代码不得执行
        let reply = "代码示例:\n```js\nconst x = 1;\nconsole.log(x);\n```\n这就是例子。";
        let out = parse_reply(reply);
        assert!(out.calls.is_empty());
        assert!(out.unparsed.is_empty());
    }

    #[test]
    fn js_block_tool_call_in_fence_only_reply_is_executed() {
        let reply = "```js\nawait read(\"a.js\");\n```";
        let out = parse_reply(reply);
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].name, "read_file");
    }

    #[test]
    fn truncated_fence_is_detected() {
        let reply = "读文件:\n```claw\nawait read(\"a.js\");";
        let out = parse_reply(reply);
        assert!(out.truncated, "未闭合围栏应判定为截断");
        assert!(out.calls.is_empty());
    }

    #[test]
    fn unparsed_expression_is_reported() {
        let reply = "```claw\nconst v = 1;\nawait write(\"a.js\", someVar);\n```";
        let out = parse_reply(reply);
        assert!(out.calls.is_empty(), "变量参数应判定为不可解析");
        assert!(
            out.unparsed.iter().any(|u| u.contains("write")),
            "应报告 write 调用参数解析失败: {:?}",
            out.unparsed
        );
    }

    #[test]
    fn bash_block_still_supported() {
        let reply = "```bash\ncat Cargo.toml\n```";
        let out = parse_reply(reply);
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].name, "bash");
    }

    #[test]
    fn tool_calls_fence_still_supported() {
        let reply = "```tool_calls\n[{\"name\":\"read_file\",\"args\":{\"path\":\"/tmp/a\"}}]\n```";
        let out = parse_reply(reply);
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].name, "read_file");
        assert_eq!(out.calls[0].args["path"], "/tmp/a");
    }

    #[test]
    fn plain_reply_no_tools() {
        let out = parse_reply("最终答复,无工具");
        assert_eq!(out.text, "最终答复,无工具");
        assert!(out.calls.is_empty());
        assert!(!out.truncated);
    }
}
