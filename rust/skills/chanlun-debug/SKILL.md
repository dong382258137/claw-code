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

> 详见 [`error-cases.md`](./error-cases.md)。以下为索引表，遇到问题时先对照索引定位，再读详情。

| 编号 | 标题 | 核心问题 |
|------|------|---------|
| 错误 1 | Array 在 Type 中性能陷阱 | 在 type 中嵌套 array 导致超时 |
| 错误 2 | 全量重算而非增量更新 | 每根 K 线都重新计算 |
| 错误 3 | 特征序列高度不一致 | 特征序列高度与 K 线不匹配 |
| 错误 4 | 线段划分两种情况处理错误 | 未区分有缺口/无缺口 |
| 错误 5 | 图形对象过多导致超时 | 一次性创建太多 label/line/box |
| 错误 6 | L2/L3候选线和pending虚线不实时更新 | barstate.islast 渲染缺失 |
| 错误 7 | 无缺口(case1)确认线段后候选虚线不显示 | case1 缺少 resultLine 创建 |
| 错误 8 | 空数组遍历导致 array.get() 索引越界 | for 循环缺少空数组防护 |
| 错误 9 | 线段初始方向仅依赖第一条笔方向 | 缺少特征序列验证 |
| 错误 10 | 线段 high/low 未标准化 | 导致中枢 ZG/ZD 不准确 |

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
