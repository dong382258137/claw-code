---
name: word-cli-operation
description: "Word文档CLI自动化操作工具。触发词：生成Word文档、创建联系函docx、施工日记docx、报账单docx、Word文档编辑、docx操作、word-cli。替代MCP word-wrapper，支持--json结构化输出，Trae IDE和Hermes均可调用。"
version: 1.0.0
最后更新: 2026-05-31
---

# Word CLI - 智能体Word文档自动化操作

基于python-docx的命令行工具，让AI智能体通过结构化命令创建、编辑、格式化和查询Word文档。

> **本技能是文档生成的底层工具**，被以下上层技能引用：
> - `construction-document-composer` — 工程文书撰写（施工方案、报告、暂估价合规审查）
> - `correspondence-management` — 联系函编号/模板/签发/归档
> - `construction-diary-generator` — 施工日记生成
> - `cost-estimation` — 造价评估（输出造价书docx）

---

# 第一章：安装与验证

## 安装

```bash
pip install -e C:\Users\38225\word-cli
```

## 验证

```bash
word-cli --version    # 应输出 1.0.0
word-cli --help       # 列出全部27个命令
```

## 依赖

- Python 3.10+
- python-docx >= 1.1.0
- click >= 8.0.0

---

# 第二章：核心设计原则

## 1. 每次命令都是完整的事务

每条命令 = 打开文档 → 执行操作 → 保存文档。不存在"未保存"状态，命令成功即文档已保存。

## 2. 操作是追加式的

heading/paragraph/table/list等命令都是**追加**到文档末尾。如果需要插入到特定位置，使用 `insert` 命令。

## 3. JSON输出是智能体的标准接口

所有命令支持 `--json` 标志，输出结构化数据供智能体解析。**智能体调用时务必加 --json**。

## 4. 段落索引从0开始

format-text、delete、spacing等命令使用段落索引（paragraph_index），从0开始计数。可用 `get-text --json` 查看每个段落的索引。

---

# 第三章：命令完整参考

## 3.1 文档创建与管理

### create - 创建新文档

```bash
word-cli create <filename> [--title TEXT] [--json]
```

- `--title`: 可选，添加为Heading 0级标题
- 创建后文档为空（或仅含标题），后续命令追加内容

### copy - 复制文档

```bash
word-cli copy <source> <destination> [--json]
```

- 基于已有文档创建副本，适合从模板生成新文档

### list-files - 列出目录中的docx文件

```bash
word-cli list-files [--directory PATH] [--json]
```

- 默认扫描当前目录
- 自动排除临时文件（~$开头）

### merge-docs - 合并多个文档

```bash
word-cli merge-docs --sources JSON --output FILE [--json]
```

- `--sources`: JSON数组，如 `'["a.docx","b.docx"]'`
- 将多个文档内容合并到一个输出文件

---

## 3.2 内容编辑

### heading - 添加标题

```bash
word-cli heading <filename> <text> [--level 1-9] [--json]
```

- `--level`: 标题级别，1=一级标题，2=二级标题，默认1
- level 0 = 文档主标题（Title样式）

**项目文档标题层级规范**：
| 级别 | 用途 | 示例 |
|------|------|------|
| 0 | 文档标题 | "工作联系函" |
| 1 | 章节标题 | "一、事由背景" |
| 2 | 子标题 | "1.1 具体情况" |
| 3 | 小标题 | "（1）人员窝工" |

### paragraph - 添加段落

```bash
word-cli paragraph <filename> <text> [--style NAME] [--json]
```

- `--style`: Word段落样式名，如 "No Spacing"、"Normal"、"Intense Quote"
- 默认使用Normal样式

### table - 添加表格

```bash
word-cli table <filename> --rows N --cols N [--data JSON] [--no-header] [--json]
```

- `--data`: JSON二维数组，如 `'[["姓名","金额"],["张三","1000"]]'`
- 默认第一行为表头（加粗+蓝色底色），`--no-header`取消
- 不提供data时创建空表格

**PowerShell中传递JSON的注意事项**：
```powershell
# PowerShell会破坏单引号内的JSON，必须用变量
$data = '[["A","B"],["1","2"]]'
word-cli table doc.docx --rows 2 --cols 2 --data $data --json

# 或在Python脚本中调用（推荐）
```

### picture - 添加图片

```bash
word-cli picture <filename> <image_path> [--width INCHES] [--json]
```

- `--width`: 图片宽度（英寸），不指定则使用原始尺寸
- 支持PNG、JPG、BMP等常见格式

### page-break - 添加分页符

```bash
word-cli page-break <filename> [--json]
```

### list - 添加列表

```bash
word-cli list <filename> --items JSON [--ordered] [--json]
```

- `--items`: JSON数组，如 `'["项目1","项目2","项目3"]'`
- `--ordered`: 使用编号列表，默认为项目符号列表

### replace - 查找替换

```bash
word-cli replace <filename> <find_text> <replace_text> [--json]
```

- 替换段落和表格中的所有匹配文本
- 返回替换次数（occurrences）

### delete - 删除段落

```bash
word-cli delete <filename> --index N [--json]
```

- 按段落索引删除，索引从0开始
- **先用 `get-text --json` 确认索引再删除**

### insert - 在目标文本旁插入

```bash
word-cli insert <filename> --target TEXT --text TEXT [--before] [--json]
```

- `--target`: 查找的目标文本（支持部分匹配）
- `--text`: 要插入的文本
- `--before`: 插入到目标之前，默认插入到目标之后

### replace-block - 替换段落块

```bash
word-cli replace-block <filename> --target TEXT --paragraphs JSON [--json]
```

- 找到包含目标文本的段落，替换为多个新段落
- `--paragraphs`: JSON数组，如 `'["新段落1","新段落2"]'`

---

## 3.3 格式化

### format-text - 格式化段落文字

```bash
word-cli format-text <filename> --index N [--bold] [--italic] [--font NAME] [--size PT] [--color HEX] [--align left|center|right|justify] [--json]
```

| 参数 | 说明 | 示例 |
|------|------|------|
| `--index` | 段落索引（必填） | `--index 0` |
| `--bold` | 加粗 | `--bold` |
| `--italic` | 斜体 | `--italic` |
| `--font` | 字体名称 | `--font SimSun`（宋体） |
| `--size` | 字号（pt） | `--size 14`（四号字） |
| `--color` | 字体颜色（十六进制） | `--color FF0000`（红色） |
| `--align` | 对齐方式 | `--align center` |

**常用字号对照**：
| 中文字号 | pt值 |
|---------|------|
| 小二 | 18 |
| 三号 | 16 |
| 小三 | 15 |
| 四号 | 14 |
| 小四 | 12 |
| 五号 | 10.5 |

**常用中文字体**：
| 字体 | font参数 |
|------|---------|
| 宋体 | SimSun |
| 黑体 | SimHei |
| 楷体 | KaiTi |
| 仿宋 | FangSong |
| 微软雅黑 | Microsoft YaHei |

### format-table - 格式化表格

```bash
word-cli format-table <filename> --index N [--no-header] [--alternating] [--widths JSON] [--json]
```

- `--index`: 表格索引（从0开始）
- `--no-header`: 不高亮表头行
- `--alternating`: 交替行着色（奇数行浅灰色）
- `--widths`: 列宽数组（cm），如 `'[3,5,4]'`

### merge-cells - 合并单元格

```bash
word-cli merge-cells <filename> --table N --start ROW,COL --end ROW,COL [--json]
```

- `--table`: 表格索引
- `--start`/`--end`: 起止单元格，格式为"行,列"（均从0开始）

### spacing - 设置段落间距

```bash
word-cli spacing <filename> --index N [--before PT] [--after PT] [--line PT] [--json]
```

- `--before`: 段前间距
- `--after`: 段后间距
- `--line`: 行距

---

## 3.4 页面布局

### page-layout - 页面设置

```bash
word-cli page-layout <filename> [--landscape] [--margin-top CM] [--margin-bottom CM] [--margin-left CM] [--margin-right CM] [--json]
```

- `--landscape`: 横向，默认纵向
- 边距单位为厘米

**公文标准页边距**：
| 边距 | 标准值 |
|------|--------|
| 上 | 3.7cm |
| 下 | 3.5cm |
| 左 | 2.8cm |
| 右 | 2.6cm |

**普通文档页边距**：
| 边距 | 常用值 |
|------|--------|
| 上下 | 2.54cm |
| 左右 | 3.17cm |

### header-footer - 页眉页脚

```bash
word-cli header-footer <filename> [--header TEXT] [--footer TEXT] [--json]
```

- 可单独设置页眉或页脚，不需要同时设置

### page-numbers - 添加页码

```bash
word-cli page-numbers <filename> [--json]
```

- 在页脚居中位置添加自动页码

---

## 3.5 批注与超链接

### comment - 添加批注

```bash
word-cli comment <filename> --target TEXT --comment TEXT [--json]
```

- 对目标文本添加黄色高亮标记
- 在段落后插入灰色斜体批注文本 `[批注: xxx]`
- **限制**: python-docx不支持创建原生Word批注（comments part），此为替代方案

### hyperlink - 添加超链接

```bash
word-cli hyperlink <filename> --target TEXT --url URL [--json]
```

- 将文档中的目标文本转为超链接

---

## 3.6 查询

### get-text - 获取全文内容

```bash
word-cli get-text <filename> [--json]
```

- 返回所有段落的索引、样式和文本
- **用于确认段落索引，是format-text/delete/spacing的前置步骤**

### info - 获取文档元数据

```bash
word-cli info <filename> [--json]
```

- 返回：标题、作者、创建时间、修改时间、段落数、表格数、节数

### outline - 获取文档大纲

```bash
word-cli outline <filename> [--json]
```

- 仅返回标题段落（Heading样式），用于快速了解文档结构

### get-tables - 获取所有表格数据

```bash
word-cli get-tables <filename> [--json]
```

- 返回每个表格的索引、行列数和完整数据
- **用于确认表格索引，是format-table/merge-cells的前置步骤**

---

# 第四章：典型工作流

## 工作流A：生成联系函（资料员 → correspondence-management）

```bash
# Step 1: 创建文档
word-cli create "d:\牙南项目\05-联系函\20260531-LXH-停工确认-V1.docx" --title "工作联系函" --json

# Step 2: 添加正文结构
word-cli heading "file.docx" "一、事由背景" --level 1
word-cli paragraph "file.docx" "根据2026年1月27日现场会议精神..."
word-cli heading "file.docx" "二、具体情况" --level 1
word-cli paragraph "file.docx" "受此影响，我项目部自2026年1月27日起暂停现场施工。"

# Step 3: 添加费用表格
word-cli table "file.docx" --rows 4 --cols 3 --data '[["费用类型","计算公式","金额"],["人员窝工费","5人×90天×200元/天","90,000元"],["机械停滞费","2台×90天×500元/台班","90,000元"],["管理费增加","90天×500元/天","45,000元"]]'

# Step 4: 格式化
word-cli format-text "file.docx" --index 0 --bold --size 16 --align center
word-cli format-table "file.docx" --index 0 --alternating

# Step 5: 页面布局
word-cli page-layout "file.docx" --margin-top 3.7 --margin-bottom 3.5 --margin-left 2.8 --margin-right 2.6
word-cli header-footer "file.docx" --header "广东泰山建设有限公司 牙南项目部"
word-cli page-numbers "file.docx"

# Step 6: 验证
word-cli outline "file.docx" --json
word-cli info "file.docx" --json
```

## 工作流B：从模板生成文档（资料员）

```bash
# Step 1: 复制模板
word-cli copy "d:\牙南项目\04-施工方案\模板.docx" "d:\牙南项目\04-施工方案\新方案.docx"

# Step 2: 替换模板中的占位符
word-cli replace "新方案.docx" "{{项目名称}}" "牙南印象"
word-cli replace "新方案.docx" "{{日期}}" "2026年5月31日"

# Step 3: 验证替换结果
word-cli get-text "新方案.docx" --json
```

## 工作流C：修改已有文档（技术总工）

```bash
# Step 1: 查看文档结构和段落索引
word-cli get-text "file.docx" --json

# Step 2: 删除不需要的段落
word-cli delete "file.docx" --index 5

# Step 3: 在特定位置插入新内容
word-cli insert "file.docx" --target "三、施工部署" --text "3.1 总体安排" --before

# Step 4: 替换某个段落块
word-cli replace-block "file.docx" --target "旧内容" --paragraphs '["新段落1","新段落2"]'

# Step 5: 格式化新插入的内容
word-cli format-text "file.docx" --index 6 --bold --size 14
```

## 工作流D：合并多个文档（资料员）

```bash
# Step 1: 查看要合并的文件
word-cli list-files --directory "d:\牙南项目\07-施工日记" --json

# Step 2: 合并
word-cli merge-docs --sources '["日记1.docx","日记2.docx","日记3.docx"]' --output "合并日记.docx"
```

## 工作流E：造价书输出（预算员 → cost-estimation）

```bash
# Step 1: 创建造价书
word-cli create "d:\牙南项目\06-报账资料\造价书.docx" --title "新增工程造价书" --json

# Step 2: 添加工程量清单表格
word-cli table "造价书.docx" --rows 6 --cols 5 --data '[["序号","项目名称","单位","工程量","综合单价","合价"],["1","挡土墙","m","100","1361","136,100"],["2","排水沟","m","180","154","27,720"],["3","珍木栏杆","m","165","418.97","69,130"],["4","道路铺装","m²","300","377.31","113,193"],["5","电路改造","项","1","196,200","196,200"]]'

# Step 3: 格式化
word-cli format-table "造价书.docx" --index 0 --alternating
word-cli format-text "造价书.docx" --index 0 --bold --size 16 --align center

# Step 4: 页面设置
word-cli page-layout "造价书.docx" --landscape
word-cli header-footer "造价书.docx" --header "牙南印象工程 造价书"
word-cli page-numbers "造价书.docx"
```

---

# 第五章：智能体调用规范

## 5.1 调用前必做

1. **确认文件路径**：使用绝对路径，避免相对路径歧义
2. **确认文件存在**：编辑/查询前确认目标文件已创建
3. **加 --json**：智能体调用时务必加 `--json`，便于解析输出

## 5.2 段落索引获取流程

```
需要format-text/delete/spacing
    → 先调用 get-text --json
    → 从返回的paragraphs数组中找到目标段落的index
    → 使用index执行操作
```

## 5.3 表格索引获取流程

```
需要format-table/merge-cells
    → 先调用 get-tables --json
    → 从返回的tables数组中找到目标表格的table_index
    → 使用index执行操作
```

## 5.4 错误处理

| 错误信息 | 原因 | 解决方法 |
|---------|------|---------|
| `Document not found` | 文件路径错误 | 检查路径，使用绝对路径 |
| `Paragraph index N out of range` | 索引越界 | 先用get-text确认索引范围 |
| `Table index N out of range` | 表格索引越界 | 先用get-tables确认索引范围 |
| `Target text not found` | 查找文本不存在 | 先用get-text确认文本内容 |
| `Image not found` | 图片路径错误 | 检查图片路径 |

## 5.5 PowerShell中JSON参数的传递

PowerShell对引号处理有特殊规则，传递JSON参数时：

```powershell
# 方法1：使用PowerShell变量（推荐）
$data = '[["A","B"],["1","2"]]'
word-cli table doc.docx --rows 2 --cols 2 --data $data --json

# 方法2：使用Python脚本调用（最可靠）
# subprocess.run([sys.executable, "-m", "word_cli.cli", "table", ...])
```

## 5.6 调用后必做（完成检查清单）

> **无论用 CLI 命令还是 Python 脚本生成文档，生成完毕后必须逐项检查以下清单。缺少任一项 → 文档视为未完成。**

| # | 检查项 | 操作 | 说明 |
|---|--------|------|------|
| 1 | **标题格式化** | `format-text` 对每个 heading 段落设置字体和字号 | 一级标题：SimHei / 16pt / bold；二级：SimHei / 15pt / bold；三级：SimHei / 14pt / bold |
| 2 | **正文格式化** | `format-text` 对正文段落设置字体和字号 | 中文正文：SimSun / 12pt；英文/数字：Times New Roman / 12pt |
| 3 | **表格格式化** | `format-table` 对所有表格交替行着色 | `--alternating` 参数实现奇偶行区分 |
| 4 | **页面布局** | `page-layout` 设置页边距 | 公文：上3.7 下3.5 左2.8 右2.6（cm）；普通文档：上下2.54 左右3.17 |
| 5 | **页眉页脚** | `header-footer` 设置页眉/页脚 | 页眉：项目名称/公司名称；页脚留空或加页码 |
| 6 | **页码** | `page-numbers` 添加页码 | 页脚居中位置 |

### 格式化时机说明

- **CLI 直接调用模式**：在每个 `paragraph` / `heading` 后立即调用 `format-text`（段落索引已知）
- **Python 脚本模式**：在内容生成循环完成后，统一遍历段落索引批量调用 `format-text`；表格生成后立即调用 `format-table`
- **必须在 `get-text --json` 之前完成**格式化，避免索引漂移

### 批量格式化 Python 脚本模式示例

```python
# 生成内容后统一格式化
def format_all(doc):
    import subprocess, sys
    cli = [sys.executable, "-m", "word_cli.cli"]

    # 1. 先获取所有段落信息
    r = subprocess.run(cli + ["get-text", doc, "--json"], capture_output=True, text=True)
    paragraphs = json.loads(r.stdout)["paragraphs"]

    # 2. 遍历格式化
    for p in paragraphs:
        idx = p["index"]
        if p["style"].startswith("Heading"):
            level = int(p["style"].split()[-1]) if p["style"] != "Heading" else 1
            sizes = {0: 18, 1: 16, 2: 15, 3: 14}
            subprocess.run(cli + ["format-text", doc, "--index", str(idx),
                "--font", "SimHei", "--size", str(sizes.get(level, 12)), "--bold", "--json"])
        else:
            subprocess.run(cli + ["format-text", doc, "--index", str(idx),
                "--font", "SimSun", "--size", "12", "--json"])

    # 3. 格式化所有表格
    r = subprocess.run(cli + ["get-tables", doc, "--json"], capture_output=True, text=True)
    for t in json.loads(r.stdout)["tables"]:
        subprocess.run(cli + ["format-table", doc, "--index", str(t["table_index"]),
            "--alternating", "--json"])

    # 4. 页面布局 + 页码
    subprocess.run(cli + ["page-layout", doc, "--margin-top", "3.7",
        "--margin-bottom", "3.5", "--margin-left", "2.8", "--margin-right", "2.6", "--json"])
    subprocess.run(cli + ["page-numbers", doc, "--json"])
```

---

# 第六章：已知限制与替代方案

## 6.1 已知限制

| 限制 | 原因 | 替代方案 |
|------|------|---------|
| 不支持原生Word批注 | python-docx无comments part API | comment命令使用黄色高亮+括号文本替代 |
| 不支持实时编辑 | CLI模式，非COM自动化 | 需实时编辑时使用MCP word-live |
| 不支持修订追踪 | python-docx不支持track changes | 需修订追踪时使用MCP word-annotate |
| 不支持复杂排版 | python-docx限制（分栏/文本框/艺术字） | 复杂排版用模板+replace方式 |
| 每次操作重写整个文件 | python-docx的工作方式 | 大量操作时考虑合并到一次脚本调用 |

## 6.2 与MCP word-wrapper的分工

| 场景 | 推荐工具 | 原因 |
|------|---------|------|
| 从零创建文档 | **word-cli** | 稳定可靠 |
| 批量生成文档 | **word-cli** | 可脚本化 |
| 编辑已有文档（简单） | **word-cli** | replace/insert足够 |
| 实时编辑运行中的Word | **MCP word-live** | 需COM接口 |
| 复杂排版调整 | **MCP word-format** | 更精细的控制 |
| 修订追踪 | **MCP word-annotate** | python-docx不支持 |

---

# 第七章：项目特定配置

## 牙南印象项目文档规范

### 文件命名

```
[日期]-[类型]-[主题]-[版本].docx
```

| 类型标识 | 含义 |
|---------|------|
| LXH | 联系函 |
| SGTJ | 施工日记 |
| BG | 报告 |
| FA | 方案 |

### 编号规则

| 类型 | 格式 |
|------|------|
| 联系函 | 粤泰山牙南〔年份〕第XXX号 |
| 施工日记 | SG-年份-月份-日期 |

### 页面设置

| 文档类型 | 上边距 | 下边距 | 左边距 | 右边距 |
|---------|--------|--------|--------|--------|
| 联系函/公文 | 3.7cm | 3.5cm | 2.8cm | 2.6cm |
| 施工日记 | 1.5cm | 1.5cm | 2.0cm | 2.0cm |
| 普通文档 | 2.54cm | 2.54cm | 3.17cm | 3.17cm |

### 页眉页脚

| 文档类型 | 页眉 | 页脚 |
|---------|------|------|
| 联系函 | 广东泰山建设有限公司 牙南项目部 | 页码 |
| 施工日记 | （无） | 页码 |
| 报告 | 项目名称 | 页码 |

---

# 第八章：经验记录

> 小端AI技能学习模式：错与对都更新，直到完全跑通后去掉错误过程，保留正确流程

### 执行记录

| 日期 | 场景 | 执行结果 | 经验教训 | 状态 |
|------|------|----------|----------|------|
| 2026-05-31 | 全面测试27个命令 | 114通过/0失败 | comment命令需改为高亮+括号替代方案 | 已验证 |
| 2026-05-31 | PowerShell JSON传递 | 失败 | PowerShell破坏JSON引号，需用变量或Python脚本 | 已验证 |
| 2026-05-31 | 生成联系函测试 | 成功 | 工作流A完整跑通 | 已验证 |

### 已验证流程

- 27个命令全部可用，114个E2E测试通过
- 联系函生成工作流（工作流A）完整跑通
- 造价书输出工作流（工作流E）完整跑通

### 待修正问题

- comment命令无法创建原生Word批注（python-docx限制），当前使用黄色高亮+括号文本替代
- hyperlink命令的URL未写入relationships part（python-docx限制），超链接在Word中可能无法点击

---

# 参考

- `construction-document-composer` — 工程文书撰写（施工方案、报告、暂估价合规审查）
- `correspondence-management` — 联系函编号/模板/签发/归档
- `construction-diary-generator` — 施工日记生成
- `cost-estimation` — 造价评估（引用本技能输出造价书docx）
- `project-core.md` — 牙南印象项目核心规则（命名规范、编号规则）
- `document-rules.md` — 文档撰写规范（联系函模板、施工日记模板、措辞规范）
- `预算员造价计算规范.md` — 造价计算规范（单价选取优先级）

---

# 附录：MCP word-wrapper 补充能力

> 合并自原 `word-document-operation` 技能。word-cli 已覆盖 90% 文档生成场景，以下能力依赖 word-wrapper MCP（底层 word-mcp-live，80+ 工具合并为 6 个），仅在需要时启用。

## 工具架构

```
word-wrapper（6个合并工具，调用 word-mcp-live 80+ 细粒度工具）
├── word_create    → 文档创建/复制/读取/查询
├── word_edit      → 内容编辑（段落/标题/表格/图片）
├── word_format    → 格式化（文字/表格/样式）
├── word_layout    → 页面布局（页眉页脚/页码/分节符）
├── word_annotate  → 批注/脚注/超链接/修订追踪
└── word_live      → 实时编辑（需 Word 正在运行，COM 接口）
```

前 5 个工具直接操作 .docx 文件，**不需要 Word 运行**；`word_live` 需要先打开 Word 文档。

## word-cli 不支持、需调用 word-wrapper 的场景

### 1. 修订追踪（word_annotate）

```js
// 进入修订模式
word_annotate({ action: "track_replace", filename: "文档.docx", old_text: "原文字", new_text: "新文字" })
word_annotate({ action: "track_insert", filename: "文档.docx", insert_text: "插入内容" })
word_annotate({ action: "track_delete", filename: "文档.docx", old_text: "要删除的文字" })

// 接受/拒绝修订
word_annotate({ action: "accept_changes", filename: "文档.docx" })
word_annotate({ action: "reject_changes", filename: "文档.docx" })
word_annotate({ action: "get_tracked_changes", filename: "文档.docx" })
```

### 2. 实时编辑（word_live，需 Word 运行）

```js
word_live({ action: "list_open" })                                    // 列出 Word 中打开的所有文档
word_live({ action: "save", filename: "文档.docx" })                  // 保存或另存为
word_live({ action: "save", filename: "文档.docx", save_as: "备份.docx" })
word_live({ action: "undo", filename: "文档.docx", times: 3 })        // 撤销 N 次
word_live({ action: "screenshot", filename: "文档.docx" })            // 截图确认效果
word_live({ action: "insert_text", filename: "文档.docx", text: "新增内容" })
word_live({ action: "replace_text", filename: "文档.docx", find_text: "旧文字", replace_text: "新文字" })
```

### 3. 复杂排版（word_format / word_layout）

```js
// 单元格底色
word_format({ action: "table_cell_shading", filename: "文档.docx", table_index: 0, row_index: 0, col_index: 0, color: "4472C4" })

// 列宽 / 单元格对齐
word_format({ action: "column_width", filename: "文档.docx", table_index: 0, col_index: 0 })
word_format({ action: "cell_alignment", filename: "文档.docx", table_index: 0, row_index: 0, col_index: 0 })

// 自定义样式
word_format({ action: "create_style", filename: "文档.docx", style_name: "我的样式", bold: true, font_name: "宋体", font_size: 12 })

// 分节符 / 水印 / 书签
word_layout({ action: "section_break", filename: "文档.docx", break_type: "new_page" })  // new_page/continuous/even_page/odd_page
word_layout({ action: "watermark", filename: "文档.docx", text: "草稿" })
word_layout({ action: "bookmark", filename: "文档.docx" })

// 目录
word_edit({ action: "add_toc", filename: "文档.docx", text: "目 录" })
```

## 工具选择决策表

| 场景 | 推荐工具 | 原因 |
|------|---------|------|
| 从零创建文档 | **word-cli** | 稳定可靠，可脚本化 |
| 批量生成文档 | **word-cli** | 27 个命令 + --json |
| 编辑已有文档（简单替换） | **word-cli** | replace / insert / replace-block 足够 |
| 修订追踪（保留修改痕迹） | **word-wrapper** `word_annotate` | python-docx 不支持 track changes |
| 实时编辑运行中的 Word | **word-wrapper** `word_live` | 需 COM 接口 |
| 复杂排版（水印/分节/自定义样式） | **word-wrapper** `word_format`/`word_layout` | python-docx 限制 |
| 表格精细控制（单元格底色/列宽） | **word-wrapper** `word_format` | python-docx 限制 |
