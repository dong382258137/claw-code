---
name: "drawing-reader"
description: "工程图纸自动分析工具。Invoke when user asks about CAD/DWG/DXF图纸内容、需要识别图纸中的文字(OCR)、提取尺寸标注、分析建筑构件，或上传图纸文件要求分析。"
---

# 工程图纸自动分析工具

基于 `drawing_reader.py` 实现 CAD 图纸读取、OCR 文字识别、尺寸标注提取的自动调用。

## 工具位置

- 主模块：`d:\牙南项目\drawing_reader.py`
- 图纸目录：`d:\牙南项目\02-设计图纸\`

## 三个核心类

| 类名 | 功能 | 依赖 |
|------|------|------|
| `CADReader` | 加载DWG/DXF，提取图层/实体/尺寸/建筑构件 | ezdxf |
| `OCRProcessor` | 图像OCR识别(PaddleOCR/Tesseract)，PDF页面OCR | paddleocr / pytesseract + PyMuPDF |
| `DimensionExtractor` | 从图像检测尺寸线/尺寸文字，关联尺寸与构件 | opencv-python |

高层封装：`DrawingReader` 组合上述三个类，提供一站式接口。

## 调用方式

### 方式1：完整流程（推荐）

```python
import sys
sys.path.insert(0, r"d:\牙南项目")

from drawing_reader import DrawingReader

reader = DrawingReader(r"d:\牙南项目")

# 扫描图纸文件夹
drawings = reader.scan_drawings_folder()

# 读取CAD
result = reader.read_cad("xxx.dxf")
# result = reader.read_cad("图纸路径.dxf")

# PDF分析
analysis = reader.analyze_drawing_for_project("xxx.pdf")

# OCR识别
ocr_result = reader.perform_ocr("照片路径.jpg")

# 尺寸提取
dim_result = reader.extract_dimensions_from_drawing(
    image_path="照片路径.jpg",
    cad_path="图纸路径.dxf"
)
```

### 方式2：分别调用核心类

```python
from drawing_reader import CADReader, OCRProcessor, DimensionExtractor, init_ocr

# CAD
cad = CADReader()
cad.load_file("xxx.dxf")
elements = cad.recognize_building_components()
stats = cad.get_layer_statistics()

# OCR
init_ocr("paddleocr")
ocr = OCRProcessor()
ocr.initialize()
results = ocr.process_image("xxx.jpg")

# 尺寸提取
de = DimensionExtractor()
dims = de.extract_from_image("xxx.jpg")
```

### 方式3：直接运行演示

```bash
cd d:\牙南项目
python drawing_reader.py
```

## 输出数据格式

### CAD读取结果
```json
{
  "layers": {"图层名": {"name": "...", "entity_count": N}},
  "entities": [{"entity_type": "LINE", "layer": "0", ...}],
  "dimensions": [{"value": 5000, "unit": "mm", ...}],
  "building_elements": [{"component_type": "墙体", "name": "...", ...}]
}
```

### OCR结果
```json
{
  "text": "识别文字",
  "confidence": 0.95,
  "bounding_box": [x1, y1, x2, y2],
  "position": [cx, cy]
}
```

### 尺寸提取结果
```json
{
  "dimension_lines": [{"type": "dimension_line", "orientation": "horizontal", ...}],
  "cad_dimensions": [{"value": 5000, "unit": "mm", ...}],
  "associations": [{"dimension": {...}, "element": {...}, "distance": N}]
}
```

## 使用场景与触发条件

### 场景1：用户上传/询问 CAD 图纸
- 触发词：CAD、DWG、DXF、图纸内容、图层、实体
- 操作：
  1. 使用 `CADReader.load_file()` 加载
  2. 提取图层统计、实体统计、尺寸标注
  3. 调用 `recognize_building_components()` 识别构件
  4. 呈现分析报告

### 场景2：用户需要识别图纸/照片中的文字
- 触发词：OCR、识别文字、提取文字、文字识别
- 操作：
  1. 调用 `init_ocr("paddleocr")` 初始化
  2. 使用 `OCRProcessor.process_image()` 识别
  3. 过滤低置信度结果（confidence < 0.5）
  4. 呈现识别结果

### 场景3：用户需要提取图纸尺寸
- 触发词：尺寸、标注、长度、宽度、尺寸线
- 操作：
  1. 使用 `DimensionExtractor.extract_from_image()` 从图像提取
  2. 或 `DimensionExtractor.extract_from_cad()` 从CAD提取
  3. 调用 `associate_dimensions_with_elements()` 关联构件
  4. 呈现尺寸清单

### 场景4：用户需要扫描图纸目录
- 触发词：图纸文件夹、扫描图纸、有哪些图纸
- 操作：
  1. 使用 `DrawingReader.scan_drawings_folder()`
  2. 按 PDF/DWG/DXF/图片 分类呈现

## 错误处理

```python
# CAD加载失败
if not result.get("success"):
    print(f"错误: {result.get('error')}")

# OCR失败
if not ocr_results:
    print("OCR未识别到文字，请检查图片清晰度")

# 尺寸提取为空
if not dim_result.get("success"):
    print("未检测到尺寸标注")
```

## 注意事项

1. **首次初始化 PaddleOCR** 会下载模型文件，需联网
2. **PaddleOCR 失败时**可回退到 `init_ocr("tesseract")`
3. CADReader 的 `load_file()` 支持 `.dwg` 和 `.dxf`
4. 大文件首次加载较慢，需耐心等待
5. 所有提取结果需人工核对确认
