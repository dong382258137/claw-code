---
name: "excel-operation"
description: "Excel文件读取和查询操作。使用excel-mcp-server（3个工具）进行Excel文件内容读取、工作表列表查询、数据查询。Invoke when user needs to read Excel files, query data, or list sheets."
---

# Excel文件操作助手

通过 excel-mcp-server 实现 Excel 文件（.xlsx/.xls）的读取和查询操作。

## 工具列表

| 工具 | 功能 | 适用场景 |
|------|------|----------|
| `read_excel_file` | 读取Excel文件全部内容 | 查看整个工作表数据 |
| `list_excel_sheets` | 获取工作表列表 | 了解文件结构 |
| `query_excel_data` | 查询Excel数据 | 按条件筛选数据 |

---

## 工具详解

### 1. read_excel_file — 读取Excel内容

读取指定工作表的全部数据，返回表格形式。

```
# 读取默认工作表（第一个）
read_excel_file({ file_path: "D:\\牙南项目\\06-报账资料\\报账单.xlsx" })

# 指定工作表名称
read_excel_file({ file_path: "D:\\牙南项目\\06-报账资料\\报账单.xlsx", sheet_name: "Sheet1" })

# 指定工作表索引（0=第一个）
read_excel_file({ file_path: "D:\\牙南项目\\06-报账资料\\报账单.xlsx", sheet_name: 0 })

# 限制读取行数（大文件时）
read_excel_file({ file_path: "D:\\牙南项目\\06-报账资料\\报账单.xlsx", nrows: 50 })
```

### 2. list_excel_sheets — 获取工作表列表

查看 Excel 文件中有哪些工作表。

```
list_excel_sheets({ file_path: "D:\\牙南项目\\02-设计图纸\\图纸会审\\图纸会审记录.xlsx" })
```

返回示例：
```json
["Sheet1", "Sheet2", "汇总表"]
```

### 3. query_excel_data — 查询Excel数据

按查询条件筛选数据。支持 SQL 风格的 WHERE 查询。

```
# 查询所有数据
query_excel_data({ file_path: "D:\\牙南项目\\06-报账资料\\报账单.xlsx" })

# 按条件筛选
query_excel_data({
  file_path: "D:\\牙南项目\\06-报账资料\\报账单.xlsx",
  sheet_name: "Sheet1",
  query: "金额 > 1000"
})

# 指定工作表查询
query_excel_data({
  file_path: "D:\\牙南项目\\06-报账资料\\报账单.xlsx",
  sheet_name: "人工费",
  query: "工种 = '钢筋工'"
})
```

---

## 标准操作流程

### 流程1：查看Excel文件内容

```
1. list_excel_sheets({ file_path: "文件.xlsx" })
   → 了解有哪些工作表
2. read_excel_file({ file_path: "文件.xlsx", sheet_name: "目标表" })
   → 读取数据
```

### 流程2：筛选特定数据

```
1. read_excel_file({ file_path: "数据.xlsx" })
   → 查看列名（第一行通常是表头）
2. query_excel_data({ file_path: "数据.xlsx", query: "列名 = '条件'" })
   → 筛选出需要的数据
```

---

## 注意事项

1. **只读操作**：excel-mcp-server 仅支持读取，不支持写入/修改
2. **文件路径**：使用绝对路径，如 `D:\\牙南项目\\文件.xlsx`（双反斜杠）
3. **编码**：支持中文列名和内容
4. **大文件**：nrows 参数可限制读取行数，避免返回数据过多
5. **多工作表**：先 list_excel_sheets 再 read_excel_file 指定 sheet_name
