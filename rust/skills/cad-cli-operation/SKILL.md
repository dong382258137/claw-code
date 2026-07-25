---
name: cad-cli-operation
description: "CAD图纸CLI自动化操作。触发词：CAD图纸分析、提取图纸文字、提取尺寸标注、DWG操作、DXF操作、cad-cli、坐标数据绘制。基于ezdxf+pyautocad双引擎，支持--json结构化输出，支持从坐标文件生成DXF。"
version: 1.1.0
适用智能体: 全部（技术总工/预算员/测量员最常用）
最后更新: 2026-07-09
项目: 牙南村委会产业配套设施完善项目、食品厂宿舍项目
合同金额: 833万元
---

# CAD CLI - 智能体CAD图纸自动化操作

基于ezdxf（DXF）+ pyautocad（DWG via COM）双引擎，让AI智能体通过命令行读取、分析和导出CAD图纸。

> **本技能是CAD图纸分析的底层工具**，被以下上层技能引用：
> - `drawing-review` — 图纸审查（提取文字/尺寸/图层）
> - `cost-estimation` — 造价评估（从图纸提取工程量）
> - `construction-document-composer` — 工程文书（引用图纸数据）

---

# 第一章：安装与验证

## 安装

```bash
pip install -e C:\Users\38225\cad-cli
```

## 验证

```bash
cad-cli --version    # 应输出 1.0.0
cad-cli --help       # 列出全部20个命令
```

## 双引擎架构

| 引擎 | 文件格式 | 依赖 | 适用场景 |
|------|---------|------|---------|
| **ezdxf** | DXF | 纯Python，无需AutoCAD | 快速提取文字/尺寸/图层/表格 |
| **pyautocad** | DWG | 需AutoCAD运行 | 读取DWG、执行AutoCAD命令、导出PDF |

---

# 第二章：核心设计原则

## 1. DXF优先

DXF文件用ezdxf直接解析（无需AutoCAD），速度最快。DWG文件需要AutoCAD运行中。

## 2. 按需提取

不需要打开整个图纸，直接提取需要的实体类型（文字/尺寸/图层/表格）。

## 3. JSON输出是智能体的标准接口

所有命令支持 `--json` 标志。**智能体调用时务必加 --json**。

---

# 第三章：命令完整参考

## 3.1 DXF操作（13个命令，无需AutoCAD）

### info - 文件元数据

```bash
cad-cli info <file> [--json]
```

### layers - 图层列表

```bash
cad-cli layers <file> [--json]
```

### texts - 提取文字

```bash
cad-cli texts <file> [--layer NAME] [--json]
```

### dimensions - 提取尺寸标注

```bash
cad-cli dimensions <file> [--layer NAME] [--json]
```

### tables - 提取表格

```bash
cad-cli tables <file> [--json]
```

### entities - 列出实体

```bash
cad-cli entities <file> [--type TYPE] [--layer NAME] [--json]
```

### blocks - 块定义列表

```bash
cad-cli blocks <file> [--json]
```

### search - 搜索文字

```bash
cad-cli search <file> <keyword> [--json]
```

### stats - 统计摘要

```bash
cad-cli stats <file> [--json]
```

### export - 导出为图片

```bash
cad-cli export <file> <output> [--format svg|png] [--json]
```

### convert - 版本转换

```bash
cad-cli convert <file> <output> [--version R2013] [--json]
```

### layer-export - 导出单图层

```bash
cad-cli layer-export <file> <output> --layer NAME [--json]
```

---

## 3.2 DWG操作（7个命令，需AutoCAD运行）

```bash
cad-cli dwg-info [--json]          # AutoCAD信息
cad-cli dwg-open <file> [--json]   # 打开DWG
cad-cli dwg-texts [--json]         # 提取DWG文字
cad-cli dwg-layers [--json]        # DWG图层列表
cad-cli dwg-run <script> [--json]  # 执行AutoLISP
cad-cli dwg-save [--json]          # 保存
cad-cli dwg-export <output> [--format pdf|dwf] [--json]  # 导出PDF/DWF
```

---

# 第四章：典型工作流

## 工作流A：提取图纸文字和尺寸（技术总工）

```bash
cad-cli info "图纸.dxf" --json
cad-cli layers "图纸.dxf" --json
cad-cli texts "图纸.dxf" --json
cad-cli search "图纸.dxf" "收方" --json
cad-cli dimensions "图纸.dxf" --json
```

## 工作流B：从图纸提取工程量（预算员）

```bash
cad-cli stats "图纸.dxf" --json
cad-cli entities "图纸.dxf" --layer "挡土墙" --json
cad-cli dimensions "图纸.dxf" --layer "标注" --json
cad-cli search "图纸.dxf" "栏杆" --json
```

## 工作流C：操作DWG文件（需AutoCAD运行）

```bash
cad-cli dwg-info --json
cad-cli dwg-open "图纸.dwg" --json
cad-cli dwg-texts --json
cad-cli dwg-export "图纸.pdf" --format pdf --json
cad-cli dwg-save --json
```

## 工作流D：从坐标数据绘制DXF图纸（技术总工/测量员）

**场景**：将.dat、.csv等坐标数据文件绘制成CAD图纸，用于施工放样、测量点位可视化。

**操作步骤**：

1. **识别文件编码**（重要！.dat文件常使用GBK编码）
   ```bash
   # PowerShell尝试不同编码读取
   Get-Content -Path "坐标.dat" -Encoding Default  # GBK编码（常用）
   Get-Content -Path "坐标.dat" -Encoding UTF8     # UTF8编码
   ```

2. **编写Python脚本生成DXF**（使用ezdxf）
   ```python
   # -*- coding: utf-8 -*-
   import ezdxf
   from ezdxf.enums import TextEntityAlignment
   
   def parse_coordinate_file(file_path, encoding='gbk'):
       """解析坐标文件，格式：点号,X,Y,Z"""
       points = []
       with open(file_path, 'r', encoding=encoding) as f:
           for line in f:
               parts = line.strip().split(',')
               if len(parts) >= 3:
                   points.append({
                       'name': parts[0],
                       'x': float(parts[1]),
                       'y': float(parts[2]),
                       'z': float(parts[3]) if len(parts) >= 4 else 0.0
                   })
       return points
   
   def create_dxf_with_points(points, output_path):
       """创建DXF，绘制坐标点并连线"""
       doc = ezdxf.new()
       msp = doc.modelspace()
       
       # 创建图层
       doc.layers.new(name='坐标点', dxfattribs={'color': 2})
       doc.layers.new(name='连线', dxfattribs={'color': 3})
       doc.layers.new(name='标注', dxfattribs={'color': 7})
       doc.layers.new(name='引线', dxfattribs={'color': 4})  # 新增引线图层
       
       # 计算图纸范围，设置标注高度
       extent = max(
           max(p['x'] for p in points) - min(p['x'] for p in points),
           max(p['y'] for p in points) - min(p['y'] for p in points)
       )
       text_height = extent * 0.008  # 标注高度为范围的0.8%
       circle_radius = extent * 0.003  # 点圆半径为范围的0.3%
       leader_offset = extent * 0.02  # 引线偏移距离为范围的2%
       
       # 绘制点和连线
       polyline_points = []
       for point in points:
           x, y, z = point['x'], point['y'], point['z']
           
           # 绘制点（用圆表示）
           msp.add_circle(
               center=(x, y),
               radius=circle_radius,
               dxfattribs={'layer': '坐标点', 'color': 2}
           )
           
           # 绘制引线（从点到标注位置）
           leader_end = (x + leader_offset, y + leader_offset)
           msp.add_line(
               start=(x, y),
               end=leader_end,
               dxfattribs={'layer': '引线', 'color': 4}
           )
           
           # 添加标注文字（包含点号、坐标和高程）
           label_text = f"{point['name']}\nX={x:.3f} Y={y:.3f}\nH={z:.3f}"
           msp.add_text(
               label_text,
               dxfattribs={'layer': '标注', 'height': text_height}
           ).set_placement(leader_end, align=TextEntityAlignment.BOTTOM_LEFT)
           
           # 收集连线点
           polyline_points.append((x, y))
       
       # 绘制连线（多段线）
       if len(polyline_points) > 1:
           msp.add_lwpolyline(polyline_points, dxfattribs={'layer': '连线', 'color': 3})
       
       doc.saveas(output_path)
   
   # 使用示例
   points = parse_coordinate_file('坐标.dat', encoding='gbk')
   create_dxf_with_points(points, '坐标.dxf')
   ```

3. **验证生成的DXF文件**
   ```bash
   cad-cli info "坐标.dxf" --json      # 查看文件元数据
   cad-cli layers "坐标.dxf" --json    # 查看图层
   cad-cli stats "坐标.dxf" --json     # 查看统计信息
   ```

**关键技术点**：
- **文件编码识别**：.dat文件常使用GBK编码，UTF8会出现乱码
- **坐标格式解析**：标准格式为"点号,X,Y,Z"，支持逗号分隔
- **标注尺寸自适应**：根据坐标范围自动计算文字高度、点圆半径、引线偏移距离
- **引线标注**：从坐标点引出线段，标注包含点号、坐标(X,Y)和高程(H)，便于清晰查看
- **文字对齐方式**：ezdxf v1.0+ 使用 `TextEntityAlignment` 枚举（不能用字符串）
- **分组绘制**：可将不同类型的点分不同图层绘制，用不同颜色区分

**实际案例**：
- 食品厂宿舍项目：32个监测点（破除路面28个 + 挡土墙基础4个）
- 文件编码：GBK（Default）
- 输出：食品公司项目7.9.dxf（66实体：32圆、32文字、2多段线）

---

# 第五章：智能体调用规范

## 5.1 文件格式选择

| 格式 | 引擎 | 是否需要AutoCAD | 速度 |
|------|------|----------------|------|
| DXF | ezdxf | 否 | 快 |
| DWG | pyautocad | 是（必须运行中） | 慢 |

## 5.2 常见实体类型

| 类型 | 说明 | 提取命令 |
|------|------|---------|
| TEXT | 单行文字 | texts |
| MTEXT | 多行文字 | texts |
| LINE | 直线 | entities --type LINE |
| ARC | 圆弧 | entities --type ARC |
| CIRCLE | 圆 | entities --type CIRCLE |
| LWPOLYLINE | 轻量多段线 | entities --type LWPOLYLINE |
| INSERT | 块参照 | entities --type INSERT |
| DIMENSION | 尺寸标注 | dimensions |
| HATCH | 填充 | entities --type HATCH |
| TABLE | 表格 | tables |

## 5.3 错误处理

| 错误信息 | 原因 | 解决方法 |
|---------|------|---------|
| `not a DXF file` | 文件格式错误 | 确认是DXF文件 |
| `AutoCAD not running` | DWG操作需AutoCAD | 先启动AutoCAD |
| `layer not found` | 图层名错误 | 先用layers命令查看 |

---

# 第六章：已知限制

| 限制 | 原因 | 替代方案 |
|------|------|---------|
| DXF不支持DWG | ezdxf仅支持DXF格式 | DWG文件用dwg-*命令（需AutoCAD） |
| DWG操作需AutoCAD运行 | pyautocad通过COM连接 | 先启动AutoCAD |
| 表格提取有限 | ezdxf的TABLE支持不完整 | 复杂表格建议手动或用AutoCAD |
| 导出图片依赖matplotlib | ezdxf渲染引擎限制 | 复杂图纸用AutoCAD导出PDF |

## 常见错误及预防

| 错误现象 | 根本原因 | 预防措施 |
|---------|---------|---------|
| .dat文件读取乱码 | 文件编码非UTF8 | 使用 `encoding='gbk'` 或 PowerShell `-Encoding Default` |
| AssertionError: align | 文字对齐参数错误 | ezdxf v1.0+ 使用 `TextEntityAlignment.BOTTOM_CENTER`（不是字符串） |
| 坐标范围计算异常 | 坐标单位不一致 | 统一使用米或毫米，注意换算比例 |
| 标注文字太小/太大 | 固定文字高度 | 根据坐标范围自适应：`text_height = extent * 0.008` |

---

# 第七章：经验记录

### 执行记录

| 日期 | 场景 | 执行结果 | 经验教训 | 状态 |
|------|------|----------|----------|------|
| 2026-06-11 | 实际项目DXF测试 | 全部通过 | 挡土墙收方.dxf: 396实体, 2图层 | 已验证 |
| 2026-06-11 | search命令 | 30个匹配 | "收方"关键词搜索准确 | 已验证 |
| 2026-07-09 | 从.dat坐标文件绘制DXF | 成功生成 | 食品公司项目7.9.dxf: 66实体, 3图层 | 已验证 |

### 已验证流程

- DXF文件info/layers/texts/stats/search命令全部通过
- 实际项目图纸测试通过
- 从坐标数据文件（.dat/.csv）生成DXF图纸流程验证通过
- GBK编码坐标文件解析和绘制验证通过
- ezdxf TextEntityAlignment枚举使用正确

---

# 参考

- `word-cli` — Word文档CLI自动化
- `edge-cli` — Edge浏览器CLI自动化
- `drawing-review` — 图纸审查
- `cost-estimation` — 造价评估
