---
name: "chart-operation"
description: "图表生成操作。使用@antv/mcp-server-chart（22种图表+表格工具）生成各类数据可视化图表。Invoke when user needs to create charts, visualize data, or generate reports with graphs."
---

# 图表生成助手

通过 @antv/mcp-server-chart 实现 22 种图表和电子表格的生成。所有图表工具共享统一的参数风格。

## 工具总览（22种图表 + 1种表格）

### 比较类 — 展示数值对比
| 工具 | 最适合 | 示例场景 |
|------|--------|----------|
| `generate_bar_chart` | 横向对比多类别 | 各班组施工进度对比 |
| `generate_column_chart` | 纵向对比多类别 | 月度产值对比 |
| `generate_radar_chart` | 多维指标对比 | 质量/安全/进度综合评分 |
| `generate_boxplot_chart` | 数据分布对比 | 材料强度检测分布 |
| `generate_violin_chart` | 分布密度对比 | 不同标号混凝土强度分布 |

### 趋势类 — 展示变化趋势
| 工具 | 最适合 | 示例场景 |
|------|--------|----------|
| `generate_line_chart` | 连续时间趋势 | 每日施工进度曲线 |
| `generate_area_chart` | 面积累积趋势 | 累计完成工程量 |
| `generate_dual_axes_chart` | 双轴对比趋势 | 产值与成本双轴对比 |

### 分布类 — 展示数据分布
| 工具 | 最适合 | 示例场景 |
|------|--------|----------|
| `generate_scatter_chart` | 两个变量关系 | 混凝土龄期与强度关系 |
| `generate_histogram_chart` | 频率分布 | 材料到场时间分布 |
| `generate_boxplot_chart` | 离群值检测 | 成本异常检测 |

### 流程类 — 展示流程和转化
| 工具 | 最适合 | 示例场景 |
|------|--------|----------|
| `generate_funnel_chart` | 漏斗转化 | 报账审批各环节通过率 |
| `generate_flow_diagram` | 数据流向 | 施工流程图 |
| `generate_sankey_chart` | 流量分布 | 材料采购到消耗全流程 |
| `generate_fishbone_diagram` | 因果分析 | 质量事故根因分析 |

### 层级与关系类
| 工具 | 最适合 | 示例场景 |
|------|--------|----------|
| `generate_treemap_chart` | 层级占比 | 工程预算各科目占比 |
| `generate_mind_map` | 思维导图 | 施工方案结构梳理 |
| `generate_venn_chart` | 交集关系 | 各单位职责重叠分析 |

### 特殊指标类
| 工具 | 最适合 | 示例场景 |
|------|--------|----------|
| `generate_waterfall_chart` | 累计增减 | 成本增加/减少分解 |
| `generate_liquid_chart` | 单一完成率 | 进度完成率仪表盘 |
| `generate_word_cloud_chart` | 文本权重 | 安全隐患关键词分析 |
| `generate_district_map` | 地理区域 | 各地块施工进度分布 |

### 表格类
| 工具 | 最适合 | 示例场景 |
|------|--------|----------|
| `generate_spreadsheet` | 数据表格/透视表 | 显示结构化数据 |

---

## 工具详解

### 1. 柱状图/条形图/折线图（最常用）

三种最通用的图表，共享相同的数据格式和通用参数。

```
# 通用参数（适用于所有图表）
{
  data: [...],             // 数据数组（必填）
  title: "图表标题",        // 标题（可选）
  width: 600,              // 宽度（可选，默认600）
  height: 400,             // 高度（可选，默认400）
  theme: "default",        // 主题：default / academy / dark
  style: {                 // 样式配置（可选）
    backgroundColor: "#fff",
    palette: ["#FF4D4F", "#2EBB59"],
    texture: "default"     // 或 "rough"（手绘风格）
  }
}
```

#### generate_bar_chart — 横向条形图

```
generate_bar_chart({
  data: [
    { category: "土建", value: 85 },
    { category: "安装", value: 62 },
    { category: "园林", value: 45 }
  ],
  title: "各专业施工进度",
  axisYTitle: "完成率(%)"
})
```

#### generate_column_chart — 纵向柱状图

```
generate_column_chart({
  data: [
    { category: "1月", value: 120 },
    { category: "2月", value: 95 },
    { category: "3月", value: 150, group: "计划" },
    { category: "3月", value: 135, group: "实际" }
  ],
  title: "月度产值对比"
})
```

#### generate_line_chart — 折线图

```
generate_line_chart({
  data: [
    { category: "1月", value: 30 },
    { category: "2月", value: 55 },
    { category: "3月", value: 72 }
  ],
  title: "累计完成百分比"
})

# 多系列折线
generate_line_chart({
  data: [
    { category: "1月", value: 30, group: "计划" },
    { category: "1月", value: 28, group: "实际" },
    { category: "2月", value: 55, group: "计划" },
    { category: "2月", value: 50, group: "实际" }
  ],
  title: "计划 vs 实际进度"
})
```

### 2. 饼图/环形图/雷达图

#### generate_area_chart — 面积图

```
generate_area_chart({
  data: [
    { category: "Q1", value: 200 },
    { category: "Q2", value: 450 },
    { category: "Q3", value: 700 },
    { category: "Q4", value: 900 }
  ],
  title: "累计完成工程量"
})
```

#### generate_radar_chart — 雷达图

```
generate_radar_chart({
  data: [
    { metric: "进度", value: 85, group: "本月" },
    { metric: "质量", value: 92, group: "本月" },
    { metric: "安全", value: 95, group: "本月" },
    { metric: "成本", value: 78, group: "本月" },
    { metric: "管理", value: 88, group: "本月" }
  ],
  title: "项目管理综合评分"
})
```

#### generate_dual_axes_chart — 双轴图

```
generate_dual_axes_chart({
  data: [
    { category: "1月", value: 30, count: 50 },
    { category: "2月", value: 55, count: 80 }
  ],
  title: "产值与成本双轴对比"
})
```

### 3. 流程/因果分析图

#### generate_funnel_chart — 漏斗图

```
generate_funnel_chart({
  data: [
    { category: "申请", value: 100 },
    { category: "审核", value: 85 },
    { category: "批准", value: 62 },
    { category: "付款", value: 48 }
  ],
  title: "报账审批漏斗"
})
```

#### generate_flow_diagram — 流程图

```
generate_flow_diagram({
  data: [
    { source: "材料进场", target: "取样检测", value: "100%" },
    { source: "取样检测", target: "合格入库", value: "95%" },
    { source: "取样检测", target: "退场", value: "5%" },
    { source: "合格入库", target: "施工使用", value: "100%" }
  ],
  title: "材料管理流程"
})
```

#### generate_fishbone_diagram — 鱼骨图

```
generate_fishbone_diagram({
  data: [
    { category: "人员", cause: "培训不足", effect: "质量问题" },
    { category: "材料", cause: "材料不合格", effect: "质量问题" },
    { category: "机械", cause: "设备老化", effect: "质量问题" },
    { category: "方法", cause: "工艺不当", effect: "质量问题" }
  ],
  title: "质量问题根因分析"
})
```

#### generate_sankey_chart — 桑基图

```
generate_sankey_chart({
  data: [
    { source: "预算", target: "人工费", value: 300 },
    { source: "预算", target: "材料费", value: 500 },
    { source: "材料费", target: "钢筋", value: 200 },
    { source: "材料费", target: "水泥", value: 150 }
  ],
  title: "预算分配流向"
})
```

### 4. 分布/统计类

#### generate_scatter_chart — 散点图

```
generate_scatter_chart({
  data: [
    { x: 3, y: 25, group: "C25" },
    { x: 7, y: 32, group: "C30" },
    { x: 14, y: 38, group: "C30" },
    { x: 28, y: 42, group: "C25" }
  ],
  title: "混凝土龄期与强度关系"
})
```

#### generate_histogram_chart — 直方图

```
generate_histogram_chart({
  data: [
    { value: 25, count: 3 },
    { value: 28, count: 8 },
    { value: 30, count: 12 },
    { value: 32, count: 7 }
  ],
  title: "材料强度分布"
})
```

#### generate_boxplot_chart — 箱线图

```
generate_boxplot_chart({
  data: [
    { category: "C25", min: 20, q1: 25, median: 30, q3: 35, max: 40 },
    { category: "C30", min: 25, q1: 30, median: 35, q3: 40, max: 45 }
  ],
  title: "混凝土强度箱线图"
})
```

#### generate_violin_chart — 提琴图

```
generate_violin_chart({
  data: [
    { category: "班组A", value: 85, group: "质量" },
    { category: "班组A", value: 90 },
    { category: "班组B", value: 78 },
    { category: "班组B", value: 82 }
  ],
  title: "各班组质量评分分布"
})
```

### 5. 层级/特殊图表

#### generate_treemap_chart — 矩形树图

```
generate_treemap_chart({
  data: [
    {
      name: "预算", value: 833,
      children: [
        { name: "人工", value: 250 },
        { name: "材料", value: 350 },
        { name: "机械", value: 150 },
        { name: "其他", value: 83 }
      ]
    }
  ],
  title: "工程预算构成"
})
```

#### generate_mind_map — 思维导图

```
generate_mind_map({
  data: [
    { from: "项目管理", to: "进度" },
    { from: "项目管理", to: "质量" },
    { from: "项目管理", to: "成本" },
    { from: "进度", to: "计划编制" },
    { from: "进度", to: "进度跟踪" }
  ],
  title: "项目管理体系"
})
```

#### generate_waterfall_chart — 瀑布图

```
generate_waterfall_chart({
  data: [
    { category: "预算", value: 833 },
    { category: "人工增加", value: 50 },
    { category: "材料节省", value: -30 },
    { category: "变更增加", value: 80 },
    { category: "最终", isTotal: true }
  ],
  title: "成本增减分解"
})
```

#### generate_liquid_chart — 液态图

```
generate_liquid_chart({
  data: [{ value: 0.72 }],
  title: "施工进度"
})
```

### 6. 通用参数说明

所有图表支持的通用参数：

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `data` | array | 必填 | 图表数据 |
| `title` | string | "" | 图表标题 |
| `width` | number | 600 | 图表宽度 |
| `height` | number | 400 | 图表高度 |
| `theme` | string | "default" | 主题：default / academy / dark |
| `style.backgroundColor` | string | - | 背景色，如 "#fff" |
| `style.palette` | array | - | 配色方案 |
| `style.texture` | string | "default" | 纹理：default / rough |
| `axisXTitle` | string | "" | X轴标题 |
| `axisYTitle` | string | "" | Y轴标题 |

---

## 标准操作流程

### 流程1：生成施工进度对比图

```
generate_column_chart({
  data: [
    { category: "基础", value: 100, group: "计划" },
    { category: "基础", value: 100, group: "实际" },
    { category: "主体", value: 80, group: "计划" },
    { category: "主体", value: 65, group: "实际" },
    { category: "装修", value: 40, group: "计划" },
    { category: "装修", value: 25, group: "实际" }
  ],
  title: "各分项工程进度对比",
  theme: "default"
})
```

### 流程2：生成成本分析瀑布图

```
generate_waterfall_chart({
  data: [
    { category: "合同金额", value: 8330534.28 },
    { category: "人工调整", value: 120000 },
    { category: "材料涨价", value: 85000 },
    { category: "变更增加", value: 200000 },
    { category: "费用节约", value: -50000 },
    { category: "预计总成本", isTotal: true }
  ],
  title: "工程成本增减分解"
})
```

### 流程3：生成质量问题根因分析

```
generate_fishbone_diagram({
  data: [
    { category: "人", cause: "技术交底不到位", effect: "质量问题" },
    { category: "机", cause: "搅拌机计量不准", effect: "质量问题" },
    { category: "料", cause: "砂石含泥量高", effect: "质量问题" },
    { category: "法", cause: "养护时间不足", effect: "质量问题" },
    { category: "环", cause: "温度过高影响", effect: "质量问题" }
  ],
  title: "混凝土质量缺陷根因分析"
})
```

### 流程4：生成预算分布树图

```
generate_treemap_chart({
  data: [{
    name: "项目总预算",
    value: 833,
    children: [
      { name: "人工费", value: 250 },
      { name: "材料费", value: 350,
        children: [
          { name: "钢筋", value: 120 },
          { name: "水泥", value: 80 },
          { name: "砂石", value: 60 }
        ]
      },
      { name: "机械费", value: 150 },
      { name: "管理费", value: 83 }
    ]
  }],
  title: "工程预算构成"
})
```

---

## 常用参数模板

### 深色主题（适合投影展示）

```
{
  theme: "dark",
  title: "项目进度总览",
  width: 800,
  height: 500
}
```

### 手绘风格

```
{
  style: { texture: "rough", palette: ["#FF6B6B", "#4ECDC4", "#45B7D1"] },
  title: "手绘风格图表"
}
```

### 学术风格

```
{
  theme: "academy",
  title: "分析报告",
  width: 700,
  height: 450
}
```

---

## 注意事项

1. **数据格式**：每个图表的 data 数组结构不同，请按各工具说明构造数据
2. **中文字体**：主题自带中文字体支持
3. **颜色自定义**：通过 style.palette 可自定义配色
4. **手绘风格**：style.texture = "rough" 可生成手绘/涂鸦风格
5. **响应式**：通过 width/height 控制图表尺寸
6. **分组数据**：多系列数据用 group 字段区分
