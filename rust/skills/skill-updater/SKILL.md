---
name: "skill-updater"
description: "技能自动更新器。监听错误发生、识别新技能需求、自动更新技能文件和错误档案。"
---

# Skill Updater - 技能自动更新器

## 核心功能

自动维护技能系统和错误档案，确保 AI 从历史错误中持续学习。

### 触发条件（硬性）

当满足以下**任一条件**时，必须执行更新：

1. 代码执行返回非零退出码（exit code ≠ 0）
2. 用户明确指出错误或问题（消息包含"错误"、"bug"、"问题"、"不工作"等关键词）
3. Shell 命令执行失败（RunCommand 返回 error）
4. 完成了一项 ≥3 次工具调用的任务
5. 配置文件（mcp.json、project_rules.md、tasks.json）被修改后

### 自检机制

每周一 9:00 AM 自动检查：
- error-archive.md 最后修改时间是否距今超过 7 天 → 如超过，提醒用户
- 是否有未归档的错误（检查最近 7 天的 shell 执行失败记录）
- 技能目录是否与 skill-updater 索引一致

---

## 错误档案 (Error Archive)

### 档案位置

当前项目根目录下的 `.trae\documents\error-archive.md`

> 注意：此路径随项目不同而变化。始终在**当前 IDE 打开的项目目录**中查找 `.trae\documents\error-archive.md`。

### 写入规则

所有 Type 5 类型的错误**必须**追加到 error-archive.md。

错误类型分类：
- **Type 1**: 代码逻辑错误（由开发者在源码中修复）
- **Type 2**: 依赖版本冲突（记录冲突包名和版本要求）
- **Type 3**: 环境配置问题（记录缺失的环境变量或配置项）
- **Type 4**: 网络/IO 超时问题（记录超时场景和替代方案）
- **Type 5**: 系统级错误（Shell解析、权限、缓存损坏等）→ 必须入档案

### 读取规则

在执行以下操作前，**必须**检索 error-archive.md：

1. **配置修改前** — 确认 Trae 版本路径 (.trae-cn vs .trae)
2. **MCP 注册** — 写入 `%APPDATA%\Trae CN\User\mcp.json`
3. **Shell 命令** — 避免 PowerShell 多行 -c；复杂脚本写入 .py 文件
4. **pip install** — 先检查已安装；使用国内镜像
5. **文件写入** — `.trae-cn` 目录优先用 MCP Write 工具
6. **系统错误入档**: 非 Pine Script 的配置/系统错误写入 error-archive.md

### 操作前强制检查清单

#### 1. 配置路径操作
```
□ 确认 Trae 版本 (中文版 .trae-cn / 国际版 .trae)
□ 技能路径: %USERPROFILE%\.trae-cn\skills\
□ MCP 路径: %APPDATA%\Trae CN\User\mcp.json
□ 操作前先 Read 现有配置确认格式
```
**相关错误**: E01, E02

#### 2. Shell 命令执行
```
□ 避免带引号的路径开头 (PowerShell 解析错误)
□ 多行 Python 代码写入 .py 文件，不要用 -c
□ 优先使用默认 python 而非绝对路径
□ 已验证: & "path\to\python.exe" args 格式可用
```
**相关错误**: E03, E07

#### 3. pip 安装依赖
```
□ 先检查是否已安装: python -c "import <pkg>"
□ 如已安装则跳过
□ 使用国内镜像: -i https://pypi.tuna.tsinghua.edu.cn/simple
□ 安装前 pip check 检查冲突
```
**相关错误**: E04, E05

#### 4. 文件迁移
```
□ 迁移前记录源目录完整文件列表
□ 迁移后执行源/目标比对
□ 检查 __pycache__ 残留
□ 检查同名技能在项目级和全局的覆盖关系
```
**相关错误**: E06, E09, E10

#### 5. 模型/大文件下载
```
□ 设置 HF_ENDPOINT=https://hf-mirror.com
□ 先检查本地缓存: ~/.cache/huggingface/
□ 使用重试机制，预期 5 次重试
```
**相关错误**: E08

#### 6. 路径写入权限
```
□ .trae-cn 目录写入优先使用 Write 工具/MCP
□ Shell Copy-Item 可能被沙箱拦截
□ 写入优先级: Write工具 > DesktopCommander MCP > Shell
```
**相关错误**: E06

### 已知错误快速索引

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

## 技能更新机制

### 进化技能更新

当 Hermes 进化引擎自动生成新技能（`evolution-*` 目录），此技能负责：

1. 扫描当前项目的 `.trae/skills/evolution-*/` 目录（项目级）和用户级 `%USERPROFILE%\.trae-cn\skills\evolution-*/` 目录
2. 验证技能文件格式正确
3. 更新技能索引
4. 标注技能来源和创建时间

### 技能弃用

当旧版技能被新版替代时：
- 将旧技能目录重命名为 `.deprecated-*`
- 在新技能中添加 "替代" 引用
- 保留 30 天后自动清理

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

每周/重大操作后执行：
1. 统计错误类型的分布变化
2. 识别高频率错误 → 升级预防等级
3. 归档已彻底解决的错误
4. 更新预防检查清单

---

## 与其他技能的协同

### 错误档案全生命周期管理（本技能统一负责）

本技能同时承担**操作前预防检查**和**操作后错误归档**双重职责：
- 操作前：读取 `.trae/documents/error-archive.md`，按错误类型分类预防
- 操作后：将新错误追加到档案，更新预防检查清单

### 与 hermes_evolution 协同
进化引擎创建技能 → skill-updater 验证并更新索引 → IDE AI 自动加载

---

## 更新日志

### v1.1.0 (2026-05-15)
- ✅ 错误档案路径改为项目相对路径（不再硬编码）
- ✅ 支持跨项目移植
- ✅ 与进化引擎协同

### v1.0.0 (2026-05-13)
- ✅ 实现错误档案自动更新
- ✅ 技能文件扫描和索引
- ✅ 错误记录格式标准化
