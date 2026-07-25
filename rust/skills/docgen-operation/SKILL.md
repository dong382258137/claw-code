---
name: "docgen-operation"
description: "Word和PDF文档生成操作。使用docgen（2个工具）快速创建Word文档和PDF文件。Invoke when user needs to quickly generate Word (.docx) or PDF documents from text content."
---

# 文档快速生成助手

通过 docgen（document-generator-mcp）实现 Word 和 PDF 文档的快速生成。

## 工具列表

| 工具 | 功能 | 适用场景 |
|------|------|----------|
| `gerar_documento_word` | 生成Word文档(.docx) | 需要格式化的Word文档 |
| `gerar_documento_pdf` | 生成PDF文档 | 需要不可编辑的PDF文件 |

---

## 工具详解

### 1. gerar_documento_word — 生成Word文档

```
# 基本使用
gerar_documento_word({
  nome_arquivo: "工作联系函",
  titulo_documento: "工作联系函",
  conteudo_principal: "致：五指山市南圣镇人民政府\n\n事由：关于……",
  autor: "广东泰山建设有限公司"
})

# 指定模板风格
gerar_documento_word({
  nome_arquivo: "施工报告",
  titulo_documento: "施工进度报告",
  conteudo_principal: "一、工程概况\n\n本项目位于五指山市南圣镇牙南村……",
  autor: "牙南项目部",
  formato: "word",
  template: "professional"  # professional / minimalist / corporate / default
})
```

**template 选项：**
| 值 | 风格 |
|----|------|
| `professional` | 蓝色线条，正式优雅 |
| `minimalist` | 大边距，简洁干净 |
| `corporate` | 标题大写，商务风格 |
| `default` | 标准简洁 |

**formato 选项：**
| 值 | 说明 |
|----|------|
| `word` | 仅生成 .docx |
| `pdf` | 仅生成 .pdf |
| `ambos` | 同时生成 word 和 pdf |

### 2. gerar_documento_pdf — 生成PDF文档

```
gerar_documento_pdf({
  nome_arquivo: "合同附件",
  titulo_documento: "合同附件说明",
  conteudo_principal: "本合同附件包括：\n1. 工程量清单\n2. 设计图纸\n3. 技术规范",
  autor: "牙南项目部"
})
```

---

## 标准操作流程

### 流程1：快速生成联系函

```
gerar_documento_word({
  nome_arquivo: "联系函_20260523",
  titulo_documento: "工作联系函",
  conteudo_principal: "编号：粤泰山牙南〔2026〕第XXX号\n\n" +
    "致：五指山市南圣镇人民政府\n" +
    "发自：广东泰山建设有限公司牙南项目部\n" +
    "日期：2026年5月23日\n\n" +
    "事由：关于XXX的确认函\n\n" +
    "正文内容……\n\n" +
    "特此函告！",
  autor: "广东泰山建设有限公司",
  template: "professional"
})
```

### 流程2：生成PDF报告

```
gerar_documento_pdf({
  nome_arquivo: "月报_202605",
  titulo_documento: "5月份施工进度报告",
  conteudo_principal: "一、本月完成工作\n\n" +
    "1. 挡土墙施工完成80%\n" +
    "2. 排水沟施工完成60%\n\n" +
    "二、下月计划\n\n" +
    "1. 完成剩余挡土墙\n" +
    "2. 开始道路施工",
  autor: "牙南项目部"
})
```

---

## 注意事项

1. **换行**：内容中用 `\n` 或直接换行实现段落分隔
2. **文件命名**：nome_arquivo 不带扩展名（自动添加 .docx / .pdf）
3. **模板选项**：只有 Word 文档支持 template 风格选择
4. **PDF生成**：PDF 不支持 template 参数
