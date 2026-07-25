---
name: "error-check"
description: "错误预防技能。在代码生成、文件操作、系统配置等操作前自动检索历史错误档案，防止重复出现已知错误。必须在此类操作前调用。"
---

# Error Check - 错误预防检查

## 核心职责

在执行以下操作前，自动检索历史错误档案并执行预防检查：
1. **配置修改** - MCP/skills/路径配置
2. **文件操作** - 技能创建/迁移/删除
3. **Shell 命令** - pip install/文件复制/多行命令
4. **网络下载** - 模型/依赖安装

---

## 错误档案位置

当前 IDE 打开的项目根目录下的 `.trae\documents\error-archive.md`

> 始终在**当前工作项目**中查找。优先搜索顺序：当前项目 → `D:\BCAD\AutoCAD 2014`（主项目）。

---

## 操作前强制检查项

### 1. 配置路径操作

```
□ 确认 Trae 版本 (中文版 .trae-cn / 国际版 .trae)
□ 技能路径: %USERPROFILE%\.trae-cn\skills\
□ MCP 路径: %APPDATA%\Trae CN\User\mcp.json
□ 操作前先 Read 现有配置确认格式
```

**相关错误**: E01, E02

### 2. Shell 命令执行

```
□ 避免带引号的路径开头 (PowerShell 解析错误)
□ 多行 Python 代码写入 .py 文件，不要用 -c
□ 优先使用默认 python 而非绝对路径
□ 已验证: & "path\to\python.exe" args 格式可用
```

**相关错误**: E03, E07

### 3. pip 安装依赖

```
□ 先检查是否已安装: python -c "import <pkg>"
□ 如已安装则跳过
□ 使用国内镜像: -i https://pypi.tuna.tsinghua.edu.cn/simple
□ 安装前 pip check 检查冲突
```

**相关错误**: E04, E05

### 4. 文件迁移

```
□ 迁移前记录源目录完整文件列表
□ 迁移后执行源/目标比对
□ 检查 __pycache__ 残留
□ 检查同名技能在项目级和全局的覆盖关系
```

**相关错误**: E06, E09, E10

### 5. 模型/大文件下载

```
□ 设置 HF_ENDPOINT=https://hf-mirror.com
□ 先检查本地缓存: ~/.cache/huggingface/
□ 使用重试机制，预期 5 次重试
```

**相关错误**: E08

### 6. 路径写入权限

```
□ .trae-cn 目录写入优先使用 Write 工具/MCP
□ Shell Copy-Item 可能被沙箱拦截
□ 写入优先级: Write工具 > DesktopCommander MCP > Shell
```

**相关错误**: E06

---

## 已知错误快速索引

| 操作 | 常见错误 | 解决方案 |
|------|---------|---------|
| 创建技能 | 写入 .trae 而非 .trae-cn | 确认中文版路径 |
| 注册 MCP | 写入 .trae-cn\mcp.json | 写入 AppData\Trae CN\User\mcp.json |
| pip install | 超时 | 用国内镜像 |
| pip install | protobuf 冲突 | venv 隔离 |
| Copy-Item | 沙箱拒绝 | 用 Write 工具 |
| 模型下载 | HuggingFace 超时 | 设置 HF_ENDPOINT 镜像 |
| python -c | 多行解析失败 | 写入 .py 文件 |
| 文件迁移 | 遗漏清理 | 迁移后审计 |

---

## 错误记录格式

每次新增错误时，按以下格式追加到 error-archive.md：

```markdown
## E{编号}: {简短标题}

**出现时间**: YYYY-MM-DD
**现象**: {具体表现}
**错误信息**: `{error message}`
**根本原因**: {技术分析}
**解决方案**:
1. {步骤1}
2. {步骤2}
**预防检查清单**:
□ 检查项1
□ 检查项2
```

---

## 定期分析

每周一或任何 ≥5 次工具调用的任务完成后执行：
1. 统计错误类型的分布变化
2. 识别高频率错误 → 升级预防等级
3. 归档已彻底解决的错误
4. 更新预防检查清单
