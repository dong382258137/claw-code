---
name: "docxtpl-operation"
description: "文档模板生成操作。使用docxtpl-mcp进行Jinja2模板渲染、PDF/PPT解析。当需要从模板批量生成文档、解析PDF或PPT时使用。DOCX解析用word-cli，Excel解析用excel-operation。"
---

# 文档模板生成助手

通过 docxtpl-mcp 实现基于 Jinja2 模板的文档生成、PDF/PPT 解析操作。

> **职责边界**：
> - **本技能负责**：Jinja2 模板渲染、模板管理、PDF 解析、PPT 解析
> - **word-cli 负责**：DOCX 创建/编辑/解析（`get-text`/`get-tables`/`info`）
> - **excel-operation 负责**：Excel 读取查询

## 工具列表

### 模板管理类
| 工具 | 功能 |
|------|------|
| `list_templates` | 列出所有可用模板 |
| `validate_template` | 验证模板并提取变量 |
| `preview_template` | 预览模板渲染效果 |
| `get_template_schema` | 获取模板完整字段结构 |
| `generate_sample_data` | 生成模板示例数据 |

### 文档生成类
| 工具 | 功能 |
|------|------|
| `generate_document` | 从模板生成Word文档（Jinja2 渲染） |
| `list_documents` | 列出所有已生成的文档 |
| `delete_document` | 删除已生成的文档 |

### 文档解析类（本技能独有）
| 工具 | 功能 |
|------|------|
| `parse_pdf_document` | 解析PDF文档 |
| `parse_ppt_document` | 解析PowerPoint文档 |
| `extract_text_from_document` | 快速提取文本（跨格式：DOCX/PDF/Excel） |
| `get_document_metadata` | 获取文档元数据 |

---

## 工具详解

### 模板管理

#### list_templates — 列出模板

```
list_templates({})
```

返回所有可用模板文件列表（模板存放在 templates/ 目录下）。

#### validate_template — 验证模板

```
validate_template({ template_name: "联系函模板.docx" })
```

返回模板中的所有 Jinja2 变量，用于确认模板字段是否正确。

#### preview_template — 预览模板

```
preview_template({
  template_name: "施工日记模板.docx",
  sample_data: { "日期": "2026-05-23", "天气": "晴", "温度": "25-32℃" }
})
```

用示例数据渲染模板，预览效果，不生成正式文件。

#### get_template_schema — 获取模板字段结构

```
get_template_schema({ template_name: "联系函模板.docx" })
```

返回完整字段结构，包含所有必填和可选字段。

#### generate_sample_data — 生成示例数据

```
generate_sample_data({ template_name: "联系函模板.docx", locale: "zh" })
```

自动为模板生成示例数据（中文），方便了解模板使用方式。

### 文档生成

#### generate_document — 从模板生成文档

```
generate_document({
  template_name: "联系函模板.docx",
  context_data: {
    "编号": "粤泰山牙南〔2026〕第001号",
    "日期": "2026年5月23日",
    "事由": "关于设计变更图纸的确认函",
    "正文内容": "我项目部自2026年1月27日起..."
  },
  output_name: "联系函_20260523"
})
```

使用 Jinja2 模板 + 数据生成 Word 文档。模板中的 `{{ 变量名 }}` 会被替换为对应的数据。

#### list_documents — 列出已生成文档

```
list_documents({})
```

#### delete_document — 删除已生成文档

```
delete_document({ document_id: "doc_xxx" })
```

### 文档解析

#### parse_pdf_document — 解析PDF

```
# 解析全部页面
parse_pdf_document({ file_path: "D:\\牙南项目\\合同\\合同文件.pdf" })

# 解析指定页面范围
parse_pdf_document({ file_path: "D:\\牙南项目\\合同\\合同文件.pdf", pages: "1-5" })
```

#### extract_text_from_document — 快速提取文本

```
# 快速提取文档文本（无结构分析）
extract_text_from_document({ file_path: "D:\\牙南项目\\05-联系函\\联系函.docx" })
```

#### get_document_metadata — 获取文档元数据

```
get_document_metadata({ file_path: "D:\\牙南项目\\05-联系函\\联系函.docx" })
```

返回：作者、创建时间、修改时间、页数等元数据。

#### parse_ppt_document — 解析PowerPoint

```
# 解析PPTX文档
parse_ppt_document({ file_path: "D:\\牙南项目\\11-其他\\汇报材料.pptx" })

# 包含图片信息
parse_ppt_document({
  file_path: "D:\\牙南项目\\11-其他\\汇报材料.pptx",
  include_images: true
})
```

---

## 标准操作流程

### 流程1：从模板生成文档

```
1. list_templates({})
   → 查看可用模板列表
2. validate_template({ template_name: "目标模板.docx" })
   → 了解模板需要哪些字段
3. generate_document({
     template_name: "目标模板.docx",
     context_data: { 字段1: "值1", 字段2: "值2" }
   })
   → 生成文档
```

### 流程2：解析PDF合同文件

```
1. get_document_metadata({ file_path: "合同.pdf" })
   → 了解文档基本信息
2. parse_pdf_document({ file_path: "合同.pdf" })
   → 提取所有内容
```

### 流程3：批量生成施工日记

```
1. generate_sample_data({ template_name: "施工日记模板.docx", locale: "zh" })
   → 查看示例数据格式
2. generate_document({
     template_name: "施工日记模板.docx",
     context_data: {
       日期: "2026-05-23",
       天气: "晴",
       温度: "25-32℃",
       施工内容: "挡土墙砌筑",
       人员: "管理人员2人，工人8人",
       机械: "挖掘机1台"
     }
   })
   → 生成当日施工日记
```

---

## 注意事项

1. **模板存放**：模板文件需放在 templates/ 目录下，模板名不包括路径
2. **模板格式**：使用 Jinja2 语法，变量用 `{{ 变量名 }}`
3. **输出文档**：生成的文档默认存放在 outputs/ 目录
4. **解析大文件**：PDF 和 PPT 可使用 pages/slides 参数限定范围
5. **中文支持**：parse_sample_data 的 locale="zh" 生成中文示例数据
