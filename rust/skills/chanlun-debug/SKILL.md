---
name: "chanlun-debug"
description: "缠论调试与需求分析专家。当用户遇到代码运行结果不正确、功能不满足预期、性能问题、需要调试排查、新功能需求实现时，自动调用此 Skill 提供错误案例库、需求案例库和解决方案。"
---

# 缠论调试与需求分析专家

## 核心能力

1. **错误案例库** - 收集和解决实际错误
2. **需求案例库** - 提炼和满足功能需求
3. **调试技巧** - 提供调试方法和工具
4. **性能优化** - 识别和解决性能问题

## 错误案例库

### 错误 1: Array 在 Type 中性能陷阱
**现象**: 代码运行超时
**解决**: 使用全局 array，不要在 type 中嵌套

### 错误 2: 全量重算而非增量更新
**现象**: 每根 K 线都重新计算
**解决**: 使用增量更新，记录 last confirmed index

### 错误 3: 特征序列高度不一致
**现象**: 特征序列高度与 K 线不匹配
**解决**: 使用对应笔的 high/low，不要重新计算

### 错误 4: 线段划分两种情况处理错误
**现象**: 线段划分不正确
**解决**: 区分有缺口/无缺口两种情况

### 错误 5: 图形对象过多导致超时
**现象**: `Calculation timed out` - 计算超时错误
**根本原因**: 一次性创建太多 label/line/box 等图形对象（如显示5000个分型标记）
**解决方案**:
1. **限制显示数量** - 最多显示最近 300 个对象
2. **分批处理** - 每帧最多处理 50 个，避免单次循环过大
3. **复用对象** - 使用 `label.set_xy()` 更新已有对象，而非删除重建

**代码修复**:
```pinescript
// 修复前：显示所有对象（可能超时）
int maxVisible = math.min(fxsSize, MAX_HISTORY)  // 5000
for i = 0 to maxVisible - 1
    label.new(...)  // 创建5000个label

// 修复后：限制数量+分批处理
int maxVisible = math.min(fxsSize, 300)  // 限制300个
int batchSize = math.min(50, maxVisible)  // 每批50个
int endIdx = math.min(startIdx + batchSize, fxsSize)

for i = startIdx to endIdx - 1
    if i < existingLabels
        label.set_xy(...)  // 复用已有对象
    else
        label.new(...)     // 只创建新增的对象
```

**预防措施**:
- 始终限制图形对象数量（label/line/box 等）
- 使用 `max_lines_count`、`max_labels_count` 等参数
- 采用增量渲染策略，避免一次性渲染全部历史数据

### 错误 6: L2/L3线段候选线和pending虚线不实时更新
**现象**: L2/L3线段在出现分型后不向前绘制线段，线段更新绘制后虚线不显示
**根本原因**: `barstate.islast` 渲染部分缺少候选线段和pending虚线的实时更新代码。只有L3候选线段有 `line.set_xy2()` 调用，L2候选线段、L2 pending虚线、L3 pending虚线都没有实时渲染更新。
**解决方案**: 在 `barstate.islast` 渲染块中，为所有4种线段图形对象添加实时更新：
1. L2 候选线段 `candidateXdLine` → `line.set_xy2()`
2. L2 pending虚线 `g_pendingXdLine` → `line.set_xy2()`
3. L3 候选线段 `candidateXdLineL2` → `line.set_xy2()`
4. L3 pending虚线 `g_pendingXdLineL3` → `line.set_xy2()`

**代码修复**:
```pinescript
// 修复前：只有L3候选线段有渲染
if showL3 and not na(candidateXdL2) and not na(candidateXdLineL2)
    if not na(candidateXdLineL2)
        line.set_xy2(candidateXdLineL2, candidateXdL2.endTs, candidateXdL2.endPrice)

// 修复后：4种线段图形全部实时渲染
if showXd and not na(candidateXd) and not na(candidateXdLine)
    line.set_xy2(candidateXdLine, candidateXd.endTs, candidateXd.endPrice)

if showXd and not na(g_pendingXd) and not na(g_pendingXdLine)
    line.set_xy2(g_pendingXdLine, g_pendingXd.endTs, g_pendingXd.endPrice)

if showL3 and not na(candidateXdL2) and not na(candidateXdLineL2)
    line.set_xy2(candidateXdLineL2, candidateXdL2.endTs, candidateXdL2.endPrice)

if showL3 and not na(g_pendingXdL3) and not na(g_pendingXdLineL3)
    line.set_xy2(g_pendingXdLineL3, g_pendingXdL3.endTs, g_pendingXdL3.endPrice)
```

**预防措施**:
- 每次新增图形对象（line/label/box）时，必须同时添加 `barstate.islast` 中的实时渲染代码
- 候选线和pending虚线属于"未确认"状态，其终点需要随最新K线实时更新
- 检查所有 `var line` 变量是否都有对应的渲染更新逻辑

### 错误 7: 无缺口(case1)确认线段后候选虚线不显示
**现象**: 第一种情况（无缺口）确认线段后，新的候选线段不显示虚线延伸；第二种情况（有缺口）正常显示虚线
**根本原因**: `processXianduanUnified` 中，case1 确认线段后创建新候选线段时，只删除了旧的 `resultLine` 但没有为新候选线段创建新的 `resultLine`（虚线）。而 case2 在同样场景下正确创建了新 `resultLine`。
**解决方案**: 在 case1 创建新候选线段后，添加 `resultLine := line.new(...)` 创建虚线，与 case2 保持一致。

**代码修复**:
```pinescript
// 修复前：case1 确认后没有为新候选线段创建虚线
Xianduan newXd = Xianduan.new(fxDIdx, fxDIdx, fxPrice, fxPrice, initHigh, initLow, newDir, 0, fxTs, fxTs, CASE1_STR)
nextCandXd := newXd
outFeatureSearchStartIdx := -1
// ❌ 缺少 resultLine 创建！

// 修复后：与 case2 一致，创建新候选虚线
Xianduan newXd = Xianduan.new(fxDIdx, fxDIdx, fxPrice, fxPrice, initHigh, initLow, newDir, 0, fxTs, fxTs, CASE1_STR)
nextCandXd := newXd
outFeatureSearchStartIdx := -1
resultLine := line.new(fxTs, fxPrice, fxTs, fxPrice, xloc=xloc.bar_time, color=color.new(config.lineColor, 50), width=1, style=line.style_dashed)
```

**预防措施**:
- 当创建新的候选线段（nextCandXd）时，必须同时创建对应的 resultLine 虚线
- 对比 case1 和 case2 的代码流程，确保两者行为一致
- 每次删除 resultLine 后，如果创建了新候选线段，必须重新创建 resultLine

### 错误 8: 空数组遍历导致 array.get() 索引越界
**现象**: `Error on bar 527: In 'array.get()' function. Index 0 is out of bounds, array size is 0.` 发生在渲染特征序列方框时。
**根本原因**: 渲染 L2/L3 特征序列方框时，使用 `for i = startIdx to array.size(arr) - 1` 遍历数组。当数组为空（`array.size(arr) = 0`）时，`startIdx` 计算为 `0`，循环变成 `for i = 0 to -1`，在 Pine Script v6 中会执行循环体，导致 `array.get(arr, 0)` 在空数组上越界。
而同文件中其他增量段处理循环（如 `for i = g_lastBiSegmentIdx to array.size(bisL1) - 1`）都有 `if array.size(arr) > g_lastIdx` 防护，唯独渲染循环缺少此保护。
**解决方案**: 在所有遍历数组的 `for` 循环前添加 `if array.size(arr) > 0` 防护检查。

**代码修复**:
```pinescript
// 修复前：L3特征序列框渲染 — 无空数组防护
if showL3
    int l3XdSize = array.size(xianduansL2)
    int l3StartIdx = showFeatureBoxes ? 0 : math.max(0, l3XdSize - maxFeatureBoxesWhenOff)
    for i = l3StartIdx to l3XdSize - 1
        Xianduan l3Xd = array.get(xianduansL2, i)  // ❌ l3XdSize=0 时越界

// 修复后：添加 size > 0 防护
if showL3
    int l3XdSize = array.size(xianduansL2)
    if l3XdSize > 0  // ✅ 空数组保护
        int l3StartIdx = showFeatureBoxes ? 0 : math.max(0, l3XdSize - maxFeatureBoxesWhenOff)
        for i = l3StartIdx to l3XdSize - 1
            Xianduan l3Xd = array.get(xianduansL2, i)
```

**预防措施**:
- 任何 `for i = X to array.size(arr) - 1` 的循环，如果 `X` 可能为 `0`，必须在外层添加 `if array.size(arr) > 0` 防护
- 增量段处理循环使用 `if array.size(arr) > g_lastIdx` 是正确模式
- 新增渲染循环时，始终检查数组是否可能为空（早期 bar 阶段）
- 页面加载初期（低 bar index），L2/L3 等高级别数组通常为空

### 错误 9: 线段初始方向仅依赖第一条笔的方向，缺少特征序列验证
**现象**: 线段（Xianduan）的初始方向直接取第一条笔的方向（`firstSeg.direction`），当第一条笔为逆势方向时，后续线段划分和绘制会出现方向错误。
**根本原因**: `processXianduanUnified` 中创建初始线段时（`na(nextCandXd)` 分支），简单使用 `firstSeg.direction` 作为线段方向。缠论标准做法是：根据前几条笔的特征序列上升/下降关系来确定初始线段的方向，而非单条笔的方向。
**解决方案**: 新增 `determineInitialDirection()` 函数，分析前 6 条笔的特征序列：
1. **向上特征序列**：收集向下笔的低点，检查是否递升（`seg.low >= prevLow`）
2. **向下特征序列**：收集向上笔的高点，检查是否递降（`seg.high <= prevHigh`）
3. 优先选择特征序列有效（≥2 个元素 + 趋势一致）的方向
4. 若都无效，退化为整体价格趋势判断

**代码修复**:
```pinescript
// 修复前：初始方向 = 第一条笔的方向
SegmentInfo firstSeg = array.get(segArr, 0)
Xianduan initXd = Xianduan.new(0, 0, firstSeg.startPrice, firstSeg.endPrice,
    firstSeg.high, firstSeg.low, firstSeg.direction, 0,  // ❌ 可能误导后续划分
    firstSeg.startTs, firstSeg.endTs, NONE_STR, na, na, na, 0)

// 修复后：新增 determineInitialDirection() 基于特征序列判断
determineInitialDirection(array<SegmentInfo> segArr) =>
    int sz = array.size(segArr)
    if sz < 2
        array.get(segArr, 0).direction
    else
        int checkCount = math.min(6, sz)
        // 向上特征序列（向下笔低点应递升）
        float prevUpFc = na
        int upFcCount = 0
        bool upFcRising = true
        // 向下特征序列（向上笔高点应递降）
        float prevDownFc = na
        int downFcCount = 0
        bool downFcFalling = true
        
        for i = 0 to checkCount - 1
            SegmentInfo seg = array.get(segArr, i)
            if seg.direction == "down"
                upFcCount += 1
                if not na(prevUpFc) and seg.low < prevUpFc
                    upFcRising := false
                prevUpFc := seg.low
            else
                downFcCount += 1
                if not na(prevDownFc) and seg.high > prevDownFc
                    downFcFalling := false
                prevDownFc := seg.high
        
        if upFcCount >= 2 and upFcRising and not downFcFalling
            "up"
        else if downFcCount >= 2 and downFcFalling and not upFcRising
            "down"
        else if upFcCount >= 2 and upFcRising
            "up"
        else if downFcCount >= 2 and downFcFalling
            "down"
        else
            SegmentInfo firstSeg = array.get(segArr, 0)
            SegmentInfo lastCheckSeg = array.get(segArr, checkCount - 1)
            lastCheckSeg.endPrice > firstSeg.startPrice ? "up" : "down"

// 调用处修改
string initDir = determineInitialDirection(segArr)
Xianduan initXd = Xianduan.new(0, 0, firstSeg.startPrice, firstSeg.endPrice,
    firstSeg.high, firstSeg.low, initDir, 0,  // ✅ 基于特征序列判断
    firstSeg.startTs, firstSeg.endTs, NONE_STR, na, na, na, 0)
```

**预防措施**:
- 线段方向判断不应该依赖单条笔的方向
- 始终通过特征序列的多元素趋势来验证方向选择
- 特征序列的递升/递降是缠论判断趋势的核心方法
- 当数据不足时（< 2 条笔），使用第一条笔方向作为兜底

### 错误 10: 线段 high/low 未标准化为实际高低点，导致中枢 ZG/ZD 不准确
**现象**: 线段确认后，其 `high`/`low` 字段可能不是线段范围内所有笔的真实最高/最低点，导致后续 L2/L3 中枢的 ZG（中枢高点）/ZD（中枢低点）计算偏差。
**根本原因**: 
1. **增量更新不完整**：`processXianduanUnified` 的增量更新路径（第 936-962 行）只比较 `lastSeg` 和 `curHigh/curLow`，未遍历 startBiIndex 到 endBiIndex 之间的所有段
2. **初始化不完整**：初始线段只使用 `firstSeg.high/low`
3. **确认时未标准化**：线段被 `array.push(xdArr, ...)` 确认前，未重新计算真实极值

缠论标准规定：线段的端点由破坏规则决定（`startPrice`/`endPrice`），但在分析层面构成中枢时，应使用线段内部的**实际高低点**（`high`/`low`）来定义区间。
**解决方案**: 
1. 新增 `calcStandardHL()` 函数，遍历 `startBiIndex` 到 `endBiIndex` 之间所有段，取 `max(high)` 和 `min(low)`
2. 在 `confirmPendingXd` 函数中内置标准化（pendingXd 确认时自动标准化）
3. 在 case1 无缺口的 `array.push(xdArr, nextCandXd)` 前调用标准化

**代码修复**:
```pinescript
// 新增：标准化辅助函数
calcStandardHL(Xianduan xd, array<SegmentInfo> segArr) =>
    float realH = xd.high
    float realL = xd.low
    int endIdx = math.min(xd.endBiIndex, array.size(segArr) - 1)
    if xd.startBiIndex >= 0 and endIdx >= xd.startBiIndex
        for k = xd.startBiIndex to endIdx
            SegmentInfo kSeg = array.get(segArr, k)
            realH := math.max(realH, kSeg.high)
            realL := math.min(realL, kSeg.low)
    [realH, realL]

// 修复：confirmPendingXd 内置标准化
confirmPendingXd(Xianduan pendingXd, line pendingLine, array<Xianduan> xdArr, 
    array<line> xdLinesArr, string breakType, color lineColor, array<SegmentInfo> segArr) =>
    [float stdH, float stdL] = calcStandardHL(pendingXd, segArr)
    pendingXd.high := stdH
    pendingXd.low := stdL
    pendingXd.confirmed := 1
    pendingXd.breakType := breakType
    array.push(xdArr, pendingXd)
    // ...

// 修复：case1 无缺口确认前标准化
nextCandXd.confirmed := 1
nextCandXd.breakType := CASE1_STR
nextCandXd.endTs := fxTs
nextCandXd.endPrice := fxPrice
nextCandXd.endBiIndex := fxDIdx
[float stdXH, float stdXL] = calcStandardHL(nextCandXd, segArr)
nextCandXd.high := stdXH
nextCandXd.low := stdXL
array.push(xdArr, nextCandXd)
```

**预防措施**:
- 划分层面：端点由破坏规则决定（`startPrice`/`endPrice`）
- 分析层面：用实际高低点（`high`/`low`）来定义线段的有效区间
- 任何 `array.push(xdArr, ...)` 确认线段前，必须确保 `high`/`low` 已标准化
- `confirmPendingXd` 已内置标准化，无需额外调用
- 新增确认路径时，必须添加 `calcStandardHL` 调用

## 需求案例库

### 需求 5: 安吉星BLE自动化 - 高级优化方向（Activity/Deep Link/Service分析）
**项目**: Auto.js 安吉星蓝牙钥匙自动化（Android 13 / 小米10S）
**原始需求**: 当前通过UI自动化（点击界面元素）连接安吉星蓝牙钥匙，流程较慢且依赖界面布局。用户希望探索更高效的方式绕过UI直接触发连接。
**技术分析**:
- 安吉星APP包名: `com.shanghaionstar`
- 目标BLE设备: SGM 102580 (MAC: 38:0B:3C:B7:1B:09, UUID: 0xFFF0)
- 当前方案: BLE扫描 → 唤醒解锁 → 启动APP → 逐页点击 → 连接蓝牙钥匙
- 痛点: UI操作链路长（4-5个页面跳转），依赖界面元素稳定性，容易因弹窗/网络延迟失败

**高级优化方案（按优先级排序）**:

**方案1: Activity记录脚本**
```javascript
// 记录安吉星操作流程中所有Activity切换
// 用法: 在执行连接操作前运行此脚本，记录完整Activity链
events.on("activity_changed", function(activity) {
    var cls = activity.className;
    var pkg = activity.packageName;
    log("Activity: " + pkg + "/" + cls);
    // 输出到文件供分析
    files.append("/sdcard/onstar_activities.log", 
        new Date().toLocaleTimeString() + " | " + pkg + "/" + cls + "\n");
});
```
**目的**: 找到蓝牙钥匙连接的目标Activity全路径，为直接启动做准备

**方案2: 直接启动目标Activity**
```javascript
// 跳过中间页面，直接启动蓝牙钥匙Activity
// 需要先通过方案1获取目标Activity路径
app.startActivity({
    action: "android.intent.action.MAIN",
    className: "com.shanghaionstar.xxx.BluetoothKeyActivity",  // 需要实际确认
    packageName: "com.shanghaionstar"
});
```
**风险**: Activity可能需要特定Intent参数或登录状态，可能crash
**验证方法**: 用adb shell am start命令先测试

**方案3: Deep Link分析**
```javascript
// 检查安吉星APP是否有深度链接
// 方法1: 分析AndroidManifest.xml
// 方法2: 尝试常见Deep Link模式
var deepLinks = [
    "onstar://bluetooth/key",
    "onstar://vehicle/connect",
    "onstar://key/connect",
    "shanghaionstar://bluetooth/key"
];
for (var i = 0; i < deepLinks.length; i++) {
    try {
        app.startActivity({action: "android.intent.action.VIEW", data: deepLinks[i]});
        log("✓ Deep Link有效: " + deepLinks[i]);
        break;
    } catch(e) {
        log("✗ Deep Link无效: " + deepLinks[i]);
    }
}
```
**验证方法**: 用adb shell am start -a android.intent.action.VIEW -d "onstar://xxx" 测试

**方案4: Exported Service分析**
```bash
# 使用adb检查安吉星APP的导出服务
adb shell dumpsys package com.shanghaionstar | grep -i "service"
adb shell pm dump com.shanghaionstar | findstr "Service"
```
**目的**: 找到可以直接调用的内部服务（如蓝牙连接服务），绕过UI
**风险**: 大多数Service不是exported，需要root才能访问

**实施步骤**:
1. 先运行Activity记录脚本，收集完整Activity链
2. 分析AndroidManifest.xml（用apktool反编译或adb dumpsys）
3. 尝试直接启动目标Activity
4. 测试Deep Link
5. 分析Exported Service
6. 根据分析结果选择最优方案

**当前状态**: 方案待实施，需要先收集Activity信息
**优先级**: 中（当前UI自动化方案可用，但优化后体验更好）

### 需求 1: 笔显示区分确认状态
**需求**: 已确认和未确认的笔用不同颜色
**解决**: 使用 `confirmed` 字段，渲染时动态选择颜色

### 需求 2: 线段划分后笔显示混乱
**需求**: 线段完成后笔的显示需要调整
**解决**: 检查笔是否在线段中，使用不同样式

### 需求 3: 中枢识别超时
**需求**: 中枢识别太慢
**解决**: 限制搜索窗口，使用增量搜索

### 需求 4: 显示所有历史绘图（CHANLUNV3 策略）
**原始需求**: 为什么 `CHANLUNV3中枢显示理想.pine` 中显示所有的历史绘图都没有限制呢？这个版本的代码中采用的是什么方法？我们修改 `缠论线段调试.pine` 时是不是能借鉴？
**技术分析**: 
- CHANLUNV3 版本禁用了手动内存管理（array.shift）
- 完全依赖 Pine Script 的自动内存管理机制
- 保持所有历史数据完整性
- 绘图对象与数据一一对应
- 适当设置 indicator() 的绘图限制参数（max_lines_count, max_labels_count, max_boxes_count）

**解决方案**: 
1. 移除分型显示数量限制（取消 300 个限制）
2. 移除分批处理逻辑（取消每帧 50 个限制）
3. 保持完整的历史数据显示
4. 采用对象复用 + 增量更新策略
5. 添加明确的注释说明策略变更

**核心实现**:
```pinescript
// 关键注释
// 显示所有分型，不限制数量（采用 CHANLUNV3 策略）
// Pine Script 已有完善的内存管理机制

// 核心代码
int fxsSize = array.size(L1.stdFenxings)
int existingLabels = array.size(L1.fenxingLabels)

if fxsSize > 0
    for i = 0 to fxsSize - 1
        Fenxing fx = array.get(L1.stdFenxings, i)
        // ... 处理每个分型
        
        if i < existingLabels
            // 复用现有对象
            label.set_x(l, fx.timestamp)
            // ... 更新其他属性
        else
            // 创建新对象
            label l = label.new(...)
            array.push(L1.fenxingLabels, l)

// 删除多余的对象
while array.size(L1.fenxingLabels) > fxsSize
    label.delete(array.pop(L1.fenxingLabels))
```

**使用方式**: 
- 适用于需要完整历史显示的场景
- 确保 indicator() 设置足够的绘图限制
- 依赖 Pine Script 的自动内存管理

**注意事项**: 
- 只在确认 Pine Script 内存管理稳定时使用
- 避免同时在 type 中嵌套 array（性能陷阱）
- 保持对象复用策略，不要每次都重建所有对象

## 调试技巧

```pinescript
// 调试开关
ENABLE_DEBUG = input.bool(false, "调试模式", group=GRP_SYS)

// 状态可视化
table t = table.new(position.top_right, 2, 5)
table.cell(t, 0, 0, "笔数")
table.cell(t, 1, 0, str.tostring(array.size(g_bis)))
```

## 性能优化

- 限制循环次数
- 使用 barstate.isconfirmed
- 复用图形对象
- 增量更新而非全量重算

## 工作流程

1. 用户描述问题（错误或需求）
2. 分析问题根本原因
3. 提供解决方案
4. 更新案例库
5. 给出预防措施
