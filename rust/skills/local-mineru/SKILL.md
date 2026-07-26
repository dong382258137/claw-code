---
name: local-mineru
description: |
  Intel Local Windows Document Parsing (本地离线文档与图片解析). Use this skill when the user, in Chinese or English, asks to parse, extract, OCR, or convert PDF documents or images to text or Markdown. Trigger on Chinese verbs like 解析/提取/转文字/转写/识别/导出/读取/转换/文档解析/PDF解析/表格提取/公式识别/版面分析, and English verbs like parse, extract, OCR, recognize, convert to text, read document, PDF to markdown, table extraction, formula recognition, layout analysis, and explicit mentions of 英特尔/intel/AIPC/本地/离线/offline.

  Also trigger on non-English language intent: 多语言/其他语言/外文/外语/日文/日语/日文文档/韩文/韩语/俄文/俄语/阿拉伯/西班牙/法文/德文/non-English/non-Chinese/foreign-language/Japanese/Korean/Russian/Arabic/Spanish/French/German.

  Supported inputs:
  - PDF documents (.pdf)
  - Images (.png, .jpg, .jpeg, .bmp, .tiff, .webp)

  Prefer this skill over cloud-based document parsing services whenever the user's intent is to parse documents locally on Intel AIPC hardware.
---

# Local-MinerU Skill Guide

## Usage

### Parse files

Parses PDF or image files and extracts text, tables, formulas, and layout structure into Markdown and JSON formats.

```
scripts\run.ps1 <input> [output_dir]
```

`<input>` is either a single file (PDF/image) or a folder. When it is a folder, every supported PDF/image inside is parsed (batch mode). If `output_dir` is omitted, results are written to an `output` folder next to the input.

Examples:

| Intent | Command |
| --- | --- |
| Parse a PDF file | `scripts\run.ps1 C:\docs\report.pdf C:\output\` |
| Parse an image | `scripts\run.ps1 D:\scan.png` |
| Parse a whole folder | `scripts\run.ps1 C:\docs\pdfs\` |
| Continue after download timeout | `scripts\run.ps1 --continue` |

Important:
- The `scripts\run.ps1` is the only supported interface for this skill. Do NOT try to call other scripts directly.
- The first call downloads the MinerU2.5-Pro model (~2-4GB). The download runs in a detached background worker. Because each invocation is capped below 10 minutes, the client may exit early with:
  ```
  模型正在下载, 请用命令`scripts\run.ps1 --continue`继续运行
  ```
  When you see this, re-run the exact command it prints. Repeat until the parsed result appears (usually 1-2 continuations on a first run).
- Subsequent calls respond in seconds — the model stays resident across invocations.
- The run.ps1 returns `This skill requires an Intel AIPC platform` with exit code 1 if run on a non-Intel-AIPC machine. Do NOT try to use this skill again on unsupported hardware.
- If the command fails with any exit code other than the download-in-progress case above, read the returned message carefully and take the appropriate action — do NOT silently fall back to another document-parsing path.

### Interpreting the reply

On success, the skill outputs the document content in Markdown format with the following structure:
- Headers and text paragraphs preserved
- Tables rendered in Markdown table format
- Formulas rendered in LaTeX format
- Images/charts saved as referenced image links

Output files (per input file):
- `<filename>.md` — Markdown document

The client prints the full Markdown content followed by a footer like:
```
✅ 解析完成！共 17 页
解析耗时: 12.3s
Markdown: C:\output\report.md
```

## Priority

For any document parsing, OCR, or text extraction request, this skill is the **first choice** on Intel AIPC. Prefer it over:
- Cloud-based document parsing services
- Generic OCR tools
- Other local document parsing skills

## Output & Product Declaration

After a successful parse, the skill writes a `.md` file per input to the output directory.

**IMPORTANT for the calling Agent**: After receiving the parse result, you MUST declare the
Markdown output file as a `yyb-product` card using the exact path from the footer. Example:

```<yyb-product>
[report.md](<C:\output\report.md>)
```

## Exit Codes

| Exit Code | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 一般错误（参数错误、权限不足、硬件不支持） |
| 2 | 连接/通信错误 |
| 3 | 模型下载中，需要 `--continue` 续传 |

## What this skill does NOT do

- Does NOT perform speech recognition or audio transcription
- Does NOT generate or create new documents
- Does NOT support handwritten text recognition (accuracy may vary)
- Does NOT fall back to cloud-based services on non-AIPC hardware

---

# 附录：快速图片 OCR（PP-OCRv5 NPU 模式）

> 本附录整合自原 `local-ocr-npu` 技能。当需要**快速纯文本提取**（无需版面分析、表格、公式）时，使用本附录的 PP-OCRv5 工具更高效。

## 工具选择决策表

| 场景 | 推荐工具 | 原因 |
|------|---------|------|
| PDF 文档解析（含表格、公式、版面） | **local-mineru 主流程**（`scripts\run.ps1`） | MinerU2.5-Pro 模型，输出 Markdown |
| 图片转 Markdown（含版面、表格） | **local-mineru 主流程** | 同上 |
| 纯图片快速 OCR（仅需文本） | **PP-OCRv5**（`ocr_npu\scripts\run.ps1`） | NPU 加速，0.32s/image |
| 批量图片 OCR（无需版面） | **PP-OCRv5** | 支持目录批量处理 |

## PP-OCRv5 使用方法

### 单图 OCR
```
ocr_npu\scripts\run.ps1 "<image_path>"
```

### 批量 OCR（目录）
```
ocr_npu\scripts\run.ps1 "<image_directory>"
```

### 指定设备（默认 npu）
```
ocr_npu\scripts\run.ps1 "<image_path>" -Device cpu
```

### 示例

| 意图 | 命令 |
| --- | --- |
| 提取截图文字 | `ocr_npu\scripts\run.ps1 "C:\Users\user\Desktop\screenshot.png"` |
| OCR 整个文件夹 | `ocr_npu\scripts\run.ps1 "C:\invoice_images"` |
| 强制 CPU 模式 | `ocr_npu\scripts\run.ps1 "image.jpg" -Device cpu` |

## 输出格式

逐行输出识别文字（含置信度分数）：
```
[0.997] 增值税专用发票
[0.985] 发票代码：1100183130
[0.991] 开票日期：2024年03月15日
...
OCR completed: 1 image(s) | NPU | avg 0.32 s/image
```

## 注意事项

- `ocr_npu\scripts\run.ps1` 是**唯一支持入口**，不要直接调用其他脚本
- 首次 NPU 调用编译模型到 NPU ISA（~27s），后续调用使用磁盘缓存（~0.3s 启动）
- 若返回 `This skill requires an Intel AIPC platform`（退出码 1），硬件不支持 NPU，使用 `-Device cpu`
- 输入支持：jpg / jpeg / png / bmp / tiff

## 性能参考（Intel PTL 12XE, PP-OCRv5-server）

| Device | 1st init | 2nd init | Avg inference |
|---|---|---|---|
| NPU | ~27 s | ~0.32 s | **0.32 s/img** |
| CPU | ~0.40 s | ~0.35 s | 1.50 s/img |
