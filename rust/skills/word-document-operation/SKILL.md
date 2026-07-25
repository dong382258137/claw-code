---
name: "word-document-operation"
description: "Word文档操作。使用word-wrapper（6个合并工具）进行文档创建、编辑、格式化、布局、批注和实时编辑。Invoke when user needs to create, edit, or manipulate Word documents (.docx)."
---

# Word文档操作助手

通过 word-wrapper（word-mcp-live 的 80→6 合并包装器）实现 Word 文档的完整操作。

## 工具架构

```
word-wrapper（主力，6个合并工具）
  底层调用 word-mcp-live 的 80+ 个细粒度工具
├── word_create    → 文档创建/复制/读取/查询
├── word_edit      → 内容编辑（段落/标题/表格/图片）
├── word_format    → 格式化（文字格式/表格格式/样式）
├── word_layout    → 页面布局（页眉页脚/页码/分节符）
├── word_annotate  → 批注/脚注/超链接/修订
└── word_live      → 实时编辑（需Word正在运行）
```

**使用方式说明：**
- **前5个工具**（word_create / word_edit / word_format / word_layout / word_annotate）：直接操作 .docx 文件，**不需要Word运行**
- **word_live**（实时编辑）：需要先打开 Word 文档，适合微调格式、撤销/保存等操作

---

## 合并工具详解

### 1. word_create — 文档创建与信息查询

```
# 创建新文档
word_create({ action: "create", filename: "D:\\项目\\联系函.docx", title: "工作联系函" })

# 复制文档
word_create({ action: "copy", source_filename: "模板.docx", destination_filename: "输出.docx" })

# 读取文档文本内容
word_create({ action: "get_text", filename: "文档.docx" })
word_create({ action: "get_text", filename: "文档.docx", show_revisions: true })  # 显示修订标记

# 获取文档信息（页数、段落数等）
word_create({ action: "get_info", filename: "文档.docx" })

# 获取文档大纲（标题结构）
word_create({ action: "get_outline", filename: "文档.docx" })

# 列出目录中的文档
word_create({ action: "list", directory: "D:\\牙南项目\\联系函" })

# 合并多个文档
word_create({ action: "merge", filename: "合并后.docx", source_filenames: ["文档1.docx", "文档2.docx"] })
```

### 2. word_edit — 内容编辑

```
# 添加段落
word_edit({ action: "add_paragraph", filename: "文档.docx", text: "这是正文内容", style: "Normal" })

# 添加标题
word_edit({ action: "add_heading", filename: "文档.docx", text: "第一章 工程概况", level: 1 })

# 添加表格
word_edit({
  action: "add_table",
  filename: "文档.docx",
  rows: 3, cols: 4,
  data: [["序号", "项目", "单位", "数量"], ["1", "土方", "m³", "500"]]
})

# 添加图片
word_edit({ action: "add_picture", filename: "文档.docx", image_path: "D:\\照片\\现场.jpg" })

# 添加分页符
word_edit({ action: "add_page_break", filename: "文档.docx" })

# 搜索替换
word_edit({ action: "search_replace", filename: "文档.docx", find_text: "旧文字", replace_text: "新文字" })

# 添加目录
word_edit({ action: "add_toc", filename: "文档.docx", text: "目 录" })

# 删除段落
word_edit({ action: "delete_paragraph", filename: "文档.docx", text: "要删除的段落文字" })

# 插入标题附近的文字（before/after）
word_edit({ action: "insert_near_text", filename: "文档.docx", target_text: "工程概况", header_title: "新增小节", position: "after" })

# 插入列表
word_edit({ action: "insert_list", filename: "文档.docx", list_items: ["项1", "项2", "项3"], bullet_type: "bullet" })
```

### 3. word_format — 格式化操作

```
# 格式化文字（加粗/斜体/字号/字体/颜色）
word_format({ action: "format_text", filename: "文档.docx", paragraph_index: 0, bold: true, font_size: 14, font_name: "宋体" })

# 格式化表格（边框/表头/着色）
word_format({ action: "format_table", filename: "文档.docx", table_index: 0 })

# 创建自定义样式
word_format({ action: "create_style", filename: "文档.docx", style_name: "我的样式", bold: true, font_name: "宋体", font_size: 12 })

# 设置表格单元格底色
word_format({ action: "table_cell_shading", filename: "文档.docx", table_index: 0, row_index: 0, col_index: 0, color: "4472C4" })

# 表格交替行颜色
word_format({ action: "table_alternating_rows", filename: "文档.docx", table_index: 0 })

# 高亮表格表头
word_format({ action: "table_header_highlight", filename: "文档.docx", table_index: 0 })

# 合并单元格
word_format({ action: "merge_cells", filename: "文档.docx", table_index: 0 })

# 设置列宽
word_format({ action: "column_width", filename: "文档.docx", table_index: 0, col_index: 0 })

# 设置单元格对齐
word_format({ action: "cell_alignment", filename: "文档.docx", table_index: 0, row_index: 0, col_index: 0 })
```

### 4. word_layout — 页面布局

```
# 页面设置（方向/大小/页边距）
word_layout({ action: "page_layout", filename: "文档.docx", orientation: "landscape" })
# orientation: "portrait"(纵向) / "landscape"(横向)

# 页眉页脚
word_layout({ action: "header_footer", filename: "文档.docx", header_text: "牙南印象项目", footer_text: "第1页" })

# 页码
word_layout({ action: "page_numbers", filename: "文档.docx" })

# 分节符
word_layout({ action: "section_break", filename: "文档.docx", break_type: "new_page" })
# break_type: new_page / continuous / even_page / odd_page

# 段落间距
word_layout({ action: "paragraph_spacing", filename: "文档.docx" })

# 书签
word_layout({ action: "bookmark", filename: "文档.docx" })

# 水印
word_layout({ action: "watermark", filename: "文档.docx", text: "草稿" })
```

### 5. word_annotate — 批注/脚注/超链接/修订

```
# 添加批注
word_annotate({ action: "add_comment", filename: "文档.docx", target_text: "需要批注的文字", comment_text: "请确认此数据" })

# 获取所有批注
word_annotate({ action: "get_comments", filename: "文档.docx" })

# 添加脚注
word_annotate({ action: "add_footnote", filename: "文档.docx", footnote_text: "详见合同附件" })

# 添加超链接
word_annotate({ action: "add_hyperlink", filename: "文档.docx", target_text: "点击查看", url: "https://example.com" })

# 获取修订列表
word_annotate({ action: "get_tracked_changes", filename: "文档.docx" })

# 接受所有修订
word_annotate({ action: "accept_changes", filename: "文档.docx" })

# 拒绝修订
word_annotate({ action: "reject_changes", filename: "文档.docx" })

# 修订模式：替换
word_annotate({ action: "track_replace", filename: "文档.docx", old_text: "原文字", new_text: "新文字" })

# 修订模式：插入
word_annotate({ action: "track_insert", filename: "文档.docx", insert_text: "插入内容" })

# 修订模式：删除
word_annotate({ action: "track_delete", filename: "文档.docx", old_text: "要删除的文字" })
```

### 6. word_live — Word实时编辑（需Word正在运行）

此工具通过 COM 接口操作正在运行的 Word 应用程序，**必须先打开 Word 文档**。

```
# 实时读取文档内容
word_live({ action: "get_text", filename: "文档.docx" })

# 获取文档信息
word_live({ action: "get_info", filename: "文档.docx" })

# 列出Word中打开的所有文档
word_live({ action: "list_open" })

# 保存文档
word_live({ action: "save", filename: "文档.docx" })

# 另存为
word_live({ action: "save", filename: "文档.docx", save_as: "D:\\备份\\文档_新版本.docx" })

# 撤销操作
word_live({ action: "undo", filename: "文档.docx" })
word_live({ action: "undo", filename: "文档.docx", times: 3 })  # 撤销3次

# 截图Word文档
word_live({ action: "screenshot", filename: "文档.docx" })

# 实时插入文本
word_live({ action: "insert_text", filename: "文档.docx", text: "新增内容" })

# 实时替换文本
word_live({ action: "replace_text", filename: "文档.docx", find_text: "旧文字", replace_text: "新文字" })
```

---

## 标准操作流程

### 流程1：创建文档并添加内容

```
1. word_create({ action: "create", filename: "报告.docx", title: "施工进度报告" })
2. word_edit({ action: "add_heading", filename: "报告.docx", text: "一、工程概况", level: 1 })
3. word_edit({ action: "add_paragraph", filename: "报告.docx", text: "本项目位于五指山市..." })
4. word_edit({ action: "add_table", filename: "报告.docx", rows: 4, cols: 3,
     data: [["序号","项目","进度"],["1","基础","80%"],["2","主体","60%"]] })
5. word_format({ action: "table_header_highlight", filename: "报告.docx", table_index: 0 })
```

### 流程2：编辑已有文档

```
1. word_create({ action: "get_text", filename: "联系函.docx" })
   → 读取当前内容，了解文档结构
2. word_edit({ action: "search_replace", filename: "联系函.docx",
     find_text: "[日期]", replace_text: "2026年5月22日" })
3. word_edit({ action: "search_replace", filename: "联系函.docx",
     find_text: "[金额]", replace_text: "捌佰叁拾叁万元" })
4. word_create({ action: "get_text", filename: "联系函.docx" })
   → 验证替换结果
```

### 流程3：生成正式文档（不含实时编辑）

```
1. word_create({ action: "create", filename: "施工方案.docx", title: "施工方案" })
2. word_edit({ action: "add_heading", filename: "施工方案.docx", text: "编制依据", level: 1 })
3. word_edit({ action: "add_paragraph", filename: "施工方案.docx", text: "1. 施工合同\n2. 施工图纸..." })
4. word_edit({ action: "add_toc", filename: "施工方案.docx" })
5. word_layout({ action: "header_footer", filename: "施工方案.docx",
     header_text: "牙南印象项目", footer_text: "第1页" })
6. word_create({ action: "get_info", filename: "施工方案.docx" })
   → 确认文档生成成功
```

### 流程4：使用Word实时编辑微调（需Word打开文档）

```
# 先用 Word 打开文档
1. word_live({ action: "get_text", filename: "联系函.docx" })
   → 读取当前文档内容
2. word_live({ action: "insert_text", filename: "联系函.docx", text: "\n补充说明：..." })
3. word_live({ action: "save", filename: "联系函.docx" })
4. word_live({ action: "screenshot", filename: "联系函.docx" })
   → 截图确认效果
5. 如有问题 → word_live({ action: "undo", filename: "联系函.docx" })
```

---

## 常见工程文档操作示例

### 联系函
```
1. word_create({ action: "create", filename: "联系函.docx", title: "工作联系函" })
2. word_edit({ action: "add_paragraph", filename: "联系函.docx",
     text: "致：五指山市南圣镇人民政府", style: "Normal" })
3. word_edit({ action: "add_paragraph", filename: "联系函.docx",
     text: "发自：广东泰山建设有限公司牙南项目部", style: "Normal" })
4. word_edit({ action: "add_paragraph", filename: "联系函.docx",
     text: "事由：关于XXX的确认函", style: "Normal" })
5. word_edit({ action: "add_paragraph", filename: "联系函.docx",
     text: "正文内容...", style: "Normal" })
```

### 施工日记
```
1. word_create({ action: "create", filename: "施工日记_20260522.docx", title: "施工日记" })
2. word_edit({ action: "add_heading", filename: "施工日记_20260522.docx",
     text: "施工日记", level: 1 })
3. word_edit({ action: "add_table", filename: "施工日记_20260522.docx", rows: 8, cols: 2,
     data: [["日期","2026年5月22日"],["天气","晴"],["温度","25-32℃"],
            ["施工内容","挡土墙施工"],["人员","管理人员5人，工人15人"],
            ["材料","水泥10吨"],["质量","合格"],["安全","无事故"]] })
```

---

## 注意事项

1. **word_live 需要Word运行**：使用前先通过 word_live({ action: "list_open" }) 确认Word已打开
2. **文件路径**：使用绝对路径，避免中文或空格导致的路径问题。例如 `D:\\牙南项目\\文档.docx`
3. **表索引**：table_index 从 0 开始（第一个表格是 0，第二个是 1）
4. **段落索引**：paragraph_index 从 0 开始
5. **修订模式**：如果需要记录修改痕迹，使用 word_annotate 的 track_replace/track_insert/track_delete
6. **合并文档**：word_create 的 merge 会按 source_filenames 的顺序依次拼接
