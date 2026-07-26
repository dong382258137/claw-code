---
name: "pine-script"
description: "Pine Script v6 编程专家。在编写、审查或调试 Pine Script 代码时自动调用，提供类型系统、数组操作、性能优化、编译错误分析等方面的指导。"
---

# Pine Script v6 编程指南

## 核心原则

1. **严格类型系统** - v6 类型检查非常严格，禁止隐式转换
2. **引用传递规则** - array/matrix/图形对象都是引用传递，禁止使用 `ref`
3. **性能优先** - 避免在 type 中嵌套 array，避免频繁数组操作

## 类型定义规则

```pinescript
// ✅ 正确的 type 定义
type MyType
    int field1
    float field2
    array<float> arr1    // array 字段必须显式初始化

// ❌ 错误的 type 定义
type BadType
    array<float> arr = array.new<float>()  // 不能在定义时初始化
    int x = na                              // 不能用 na 给基础类型默认值
```

## 初始化规则（极易出错）

```pinescript
// ✅ 正确初始化方式
var array<float> myArr = array.new<float>()
var MyType obj = MyType.new(array.new<float>(), 0, 0.0)

// ❌ 错误初始化
var MyType obj2 = MyType.new(na, na, na)     // array不能用na
var MyType obj3 = na                          // 整个对象不能用na

// ✅ 推荐：使用创建函数封装初始化
createMyType() =>
    MyType.new(array.new<float>(0), 0, close)

var MyType obj4 = createMyType()
```

## 函数参数传递规则

| 参数类型 | 传递方式 | 修改影响 | 是否需要 `ref` |
|---------|---------|---------|---------------|
| `int/float/string/bool` | 值传递 | 函数内修改不影响外部 | 需要 `ref` 才能修改外部 |
| `array<T>` | **引用传递** | 函数内修改**直接影响**外部 | **禁止**使用 `ref` |
| `matrix<T>` | 引用传递 | 函数内修改直接影响外部 | 禁止使用 `ref` |
| `line/label/box/table` | 引用传递 | 函数内修改直接影响外部 | 禁止使用 `ref` |
| 用户 `type` | 值传递（浅拷贝） | 修改字段不影响外部 | 需要 `ref` 才能修改外部变量 |

```pinescript
// 示例验证
modifyValues(int a, ref int b, array<float> arr, MyType obj) =>
    a := a + 1                    // 不影响外部
    b := b + 1                    // 影响外部（因为ref）
    array.push(arr, 999)          // 影响外部（array是引用）
    obj.field1 := obj.field1 + 1  // 不影响外部（type是值传递）
```

## Array 操作性能规则

```pinescript
// ⚠️ 性能陷阱：array 在 type 中性能较差
type SlowType
    array<float> data

var SlowType slow = SlowType.new(array.new<float>(1000))  // 每次访问都拷贝

// ✅ 性能优化：直接使用全局 array
var array<float> fast = array.new<float>(1000)            // 无拷贝开销

// ✅ 优化：array 大小预分配
array<float> prealloc = array.new<float>(1000, 0.0)       // 预分配1000个元素

// ❌ 避免：频繁的 array.shift/array.unshift（O(n)操作）
// ✅ 代替：使用索引追踪，逻辑删除（标记无效）
```

## Tuple 返回值（推荐替代 ref）

```pinescript
// ✅ 推荐：使用 tuple 返回多个值（无副作用，易测试）
processData(array<float> prices, int count) =>
    float sum = 0.0
    for price in prices
        sum += price
    int newCount = count + 1
    float avg = sum / array.size(prices)
    [sum, avg, newCount]  // 返回 tuple

// 调用：使用 := 解包
[var sum, var avg, var count] := processData(prices, count)

// ⚠️ 注意：tuple 最多返回 7 个元素
```

## 严格类型检查（v6 最显著变化）

```pinescript
// v6 严格要求类型匹配，隐式转换被禁止或警告
int x = 10
float y = x           // ✅ 允许：int -> float（拓宽转换）
int z = y             // ❌ 错误：float -> int（需要显式转换）
int z2 = int(y)       // ✅ 正确：显式转换

// array 类型必须完全匹配
array<float> farr = array.new<float>(0)
array<int> iarr = array.new<int>(0)
farr := iarr          // ❌ 错误：类型不匹配（即使都是number）

// 字面量也有类型
var arr = array.new<float>(0)
array.push(arr, 10)         // ❌ 错误：10是int，arr是float
array.push(arr, 10.0)       // ✅ 正确：10.0是float
array.push(arr, float(10))  // ✅ 正确：显式转换
```

## 常见编译错误速查

| 错误信息 | 原因 | 解决方案 |
|---------|------|---------|
| `Cannot declare 'array' as an argument type` | 版本不是v6 | 检查 `//@version=6` |
| `The 'ref' keyword cannot be used with this type` | 对array/matrix/对象用了ref | 移除ref，直接传array |
| `Cannot use 'na' to initialize this type` | 用na初始化含array的type | 用 `array.new<T>()` 初始化array字段 |
| `Type mismatch: expected 'array<float>' but got 'array<int>'` | array元素类型不匹配 | 统一类型或使用 `array.from(float(1), 2.0)` |
| `The maximum number of arguments (7) has been reached` | tuple返回超过7个 | 封装成type返回，或减少返回值 |
| `This type cannot be used as a field in a user-defined type` | 在type中用了非法字段 | 检查字段类型（如不能用na初始化） |

## 最佳实践

### ✅ 架构设计
1. **避免在type中嵌套array**：性能差，更新复杂
2. **使用tuple而非ref**：更清晰，无副作用
3. **全局状态管理**：使用 `var` 声明全局array，函数直接操作
4. **延迟确认模式**：用 `confirmed` 标志位+计数器，避免频繁重建结构

### ✅ 性能优化
1. **预分配array大小**：`array.new<float>(1000, 0.0)` 而非频繁push
2. **批量处理**：在 `barstate.isconfirmed` 时处理，避免tick级计算
3. **避免array拷贝**：不要 `array.copy()` 除非必要，尽量用索引引用
4. **限制历史数据**：`max_bars_back()` 或定期 `array.shift()`

### ✅ 代码风格
```pinescript
// 推荐：纯函数 + 全局状态更新
updateStructure(array<Data> data, Param param) =>
    // 只读操作，返回新状态
    [newData, newIndices]

// 主循环
if barstate.isconfirmed
    [g_data, g_indices] := updateStructure(g_data, param)
    render(g_data)
```

## 官方文档参考

- **官方文档主页**: https://www.tradingview.com/pine-script-docs/en/v5/
- **v6 迁移指南**: https://www.tradingview.com/pine-script-docs/en/v5/migration_guides/To_pine_script_6.html
- **Type 系统文档**: https://www.tradingview.com/pine-script-docs/en/v5/language/Type_system.html
- **Array 参考**: https://www.tradingview.com/pine-script-docs/en/v5/concepts/Arrays.html

## 使用建议

当编写 Pine Script 代码时：
1. 首先检查 `//@version=6` 声明
2. 定义 type 时确保所有 array 字段都能正确初始化
3. 函数参数传递时注意引用 vs 值传递的区别
4. 优先使用 tuple 返回多个值
5. 遇到类型错误时，考虑显式类型转换

---

# 附录：Pine Script v6 编译错误案例库

> 合并自原 `pine-script-syntax` 技能。当用户遇到编译错误、类型错误、语法问题时参考此案例库。

## 错误 1: 类型不匹配
**错误**: `Type mismatch: expected 'array<float>' but got 'array<int>'`
**解决**: 统一类型或显式转换

## 错误 2: ref 关键字误用
**错误**: `The 'ref' keyword cannot be used with this type`
**解决**: array/matrix/图形对象禁止使用 ref

## 错误 3: 变量遮蔽
**错误**: `Shadowing variable "xxx" which exists in parent scope`
**解决**: 使用 `:=` 重新赋值而非 `=` 声明

## 错误 5: 函数内部定义嵌套函数
**错误信息**: `Syntax error at input "=>"(CE10156)`
**现象**: 在函数内部使用 `=>` 定义嵌套函数时报语法错误
**根本原因**:
- Pine Script **不支持在函数内部定义嵌套函数**
- 所有函数必须在顶层定义
- 嵌套函数定义违反了 Pine Script v6 的语法规则

**错误代码示例**:
```pinescript
// ❌ 错误：在函数内部定义嵌套函数
buildBisIncremental(LevelState state) =>
    int fxsSize = array.size(state.stdFenxings)
    if fxsSize < 2
        state
    else
        int bisSize = array.size(state.bis)

        if bisSize > 0
            // ... 处理逻辑
        else
            // ❌ 调用嵌套函数
            createNewBi()

    // ❌ 错误：在函数内部定义函数
    createNewBi() =>
        Fenxing fx1 = array.get(state.stdFenxings, fxsSize - 2)
        Fenxing fx2 = array.get(state.stdFenxings, fxsSize - 1)

        if fx1.type == fx2.type
            state
        else
            // ... 创建笔的逻辑
            Bi bi = Bi.new(...)
            array.push(state.bis, bi)
            state
```

**解决方案**:
将嵌套函数的逻辑**内联**到调用位置，或者将函数提取到顶层

**正确代码示例**:
```pinescript
// ✅ 正确：将逻辑内联到调用位置
buildBisIncremental(LevelState state) =>
    int fxsSize = array.size(state.stdFenxings)
    if fxsSize < 2
        state
    else
        int bisSize = array.size(state.bis)

        if bisSize > 0
            Bi lastBi = array.get(state.bis, bisSize - 1)
            if lastBi.endTs == fx2.timestamp
                state
            else
                // ... 处理逻辑
        else
            // ✅ 直接内联创建笔的逻辑
            Fenxing fx1 = array.get(state.stdFenxings, fxsSize - 2)
            Fenxing fx2 = array.get(state.stdFenxings, fxsSize - 1)

            if fx1.type == fx2.type
                state
            else
                bool isUp = fx1.type == "bottom" and fx2.type == "top"
                bool isDown = fx1.type == "top" and fx2.type == "bottom"

                if not (isUp or isDown)
                    state
                else if isUp and fx1.price >= fx2.price
                    state
                else if isDown and fx1.price <= fx2.price
                    state
                else if math.abs(fx2.klineIndex - fx1.klineIndex) < minBiLength
                    state
                else
                    string dir = isUp ? "up" : "down"
                    float biH = math.max(fx1.price, fx2.price)
                    float biL = math.min(fx1.price, fx2.price)
                    int confirmed = 0

                    Bi bi = Bi.new(fx1.timestamp, fx2.timestamp, fx1.price, fx2.price, biH, biL, dir, confirmed)
                    array.push(state.bis, bi)
                    state

// ✅ 或者：提取到顶层（如果需要复用）
createNewBi(LevelState state, Fenxing fx1, Fenxing fx2) =>
    if fx1.type == fx2.type
        state
    else
        bool isUp = fx1.type == "bottom" and fx2.type == "top"
        bool isDown = fx1.type == "top" and fx2.type == "bottom"

        if not (isUp or isDown)
            state
        else if isUp and fx1.price >= fx2.price
            state
        else if isDown and fx1.price <= fx2.price
            state
        else if math.abs(fx2.klineIndex - fx1.klineIndex) < minBiLength
            state
        else
            string dir = isUp ? "up" : "down"
            float biH = math.max(fx1.price, fx2.price)
            float biL = math.min(fx1.price, fx2.price)
            int confirmed = 0

            Bi bi = Bi.new(fx1.timestamp, fx2.timestamp, fx1.price, fx2.price, biH, biL, dir, confirmed)
            array.push(state.bis, bi)
            state

buildBisIncremental(LevelState state) =>
    // ... 主逻辑
    else
        // 调用顶层函数
        createNewBi(state, fx1, fx2)
```

**预防措施**:
- Pine Script 不支持嵌套函数定义
- 所有函数必须在顶层定义
- 如果逻辑需要复用，提取到顶层作为独立函数
- 如果逻辑只使用一次，直接内联到调用位置
- 避免在函数内部使用 `=>` 定义新函数

## 错误 6: 使用保留关键字作为变量名
**错误信息**: `"xxx" cannot be used as a variable or function name.(CE10150)`
**现象**: 尝试使用 Pine Script 保留关键字作为变量名或函数名时编译失败
**根本原因**:
- Pine Script 有很多保留关键字，不能用作变量名或函数名
- 常见保留关键字包括：`range`, `var`, `if`, `else`, `for`, `while`, `return`, `true`, `false` 等

**错误代码示例**:
```pinescript
// ❌ 错误：使用保留关键字 "range" 作为变量名
float range = math.abs(fx2.price - fx1.price)
int klines = fx2.klineIndex - fx1.klineIndex
float strength = klines > 0 ? range / klines : range
```

**解决方案**:
将保留关键字改为其他有意义的变量名，例如：
- `range` → `priceRange`
- `var` → `myVar`
- `if` → `condition`
- `for` → `loopIndex`

**正确代码示例**:
```pinescript
// ✅ 正确：使用非保留关键字作为变量名
float priceRange = math.abs(fx2.price - fx1.price)
int klines = fx2.klineIndex - fx1.klineIndex
float strength = klines > 0 ? priceRange / klines : priceRange
```

**预防措施**:
- 熟悉 Pine Script 保留关键字列表
- 避免使用常见编程语言关键字作为变量名
- 变量名使用更具体的描述性名称（如 `priceRange` 而不是 `range`）
- 遇到此类错误时，立即修改变量名

## 错误 7: 变量声明未初始化
**错误信息**: `Syntax error at input "end of line without line continuation"(CE10156)`
**现象**: 尝试只声明变量而不初始化时编译失败
**根本原因**:
- Pine Script v6 要求所有变量在声明时必须同时初始化
- 不能只写 `string xdDir` 而不赋值
- 对于图形对象（line/label/box/table），可以初始化为 `na`

**错误代码示例**:
```pinescript
// ❌ 错误：只声明变量而不初始化
string xdDir
int startBiIdx
float startPrice
int startTime

// ❌ 错误：图形对象未初始化
drawBi(Bi bi, int idx) =>
    line l
    if idx < array.size(biLinePool)
        l := array.get(biLinePool, idx)
```

**解决方案**:
所有变量声明时必须同时初始化：
- 基本类型（string/int/float/bool）：给一个合理的默认值
- 图形对象（line/label/box/table）：初始化为 `na`
- array/matrix：使用 `array.new<T>()` 或 `matrix.new<T>()` 初始化

**正确代码示例**:
```pinescript
// ✅ 正确：变量声明同时初始化
string xdDir = "up"
int startBiIdx = 0
float startPrice = 0.0
int startTime = 0

// ✅ 正确：图形对象初始化为 na
drawBi(Bi bi, int idx) =>
    line l = na
    if idx < array.size(biLinePool)
        l := array.get(biLinePool, idx)
```

**预防措施**:
- 养成变量声明即初始化的习惯
- 对于后续会重新赋值的变量，给一个合理的默认值
- 图形对象统一初始化为 `na`
- 使用类型推导让编译器自动推断类型（如 `var xdDir = "up"`）

## 错误 8: 代码行未完成就换行
**错误信息**: `Syntax error at input "end of line without line continuation"(CE10156)`
**现象**: 一行代码没有写完就换行了，且没有使用行延续符
**根本原因**:
- Pine Script 要求一行代码必须完整
- 如果需要换行，必须使用行延续符（但 Pine Script v6 不支持行延续符）
- 最好的做法是将代码写在同一行，或者重构为多行表达式

**错误代码示例**:
```pinescript
// ❌ 错误：一行代码没有写完就换行
bool isFeature = (xdDir == "up" and b.direction == "down") or
                (xdDir == "down" and b.direction == "up")
```

**解决方案**:
将代码写在同一行，或者使用中间变量拆分表达式：

**正确代码示例**:
```pinescript
// ✅ 正确：将代码写在同一行
bool isFeature = (xdDir == "up" and b.direction == "down") or (xdDir == "down" and b.direction == "up")

// ✅ 或者：使用中间变量拆分表达式
bool isUpFeature = xdDir == "up" and b.direction == "down"
bool isDownFeature = xdDir == "down" and b.direction == "up"
bool isFeature = isUpFeature or isDownFeature
```

**预防措施**:
- 保持代码行简洁，避免过长的单行代码
- 对于复杂表达式，使用中间变量拆分
- 不要在表达式中间换行
- 使用有意义的变量名提高可读性

## 错误 9: 访问不存在的类型字段
**错误信息**: 运行时错误或编译错误（取决于 Pine Script 版本）
**现象**: 代码尝试访问某个 type 中不存在的字段
**根本原因**:
- type 定义中没有声明该字段
- 代码使用了错误的字段名
- type 定义已更新但代码未同步更新

**错误代码示例**:
```pinescript
// type 定义中没有 high 和 low 字段
type Bi
    int startTime
    int endTime
    float startPrice
    float endPrice
    string direction

// ❌ 错误：尝试访问不存在的字段
Bi breakBi = array.get(bis, breakBiIdx)
float endPrice = xdDir == "up" ? breakBi.high : breakBi.low
```

**解决方案**:
1. 检查 type 定义，确认可用的字段
2. 使用存在的字段，或者计算需要的值
3. 如果确实需要该字段，更新 type 定义

**正确代码示例**:
```pinescript
// ✅ 正确：使用存在的字段并计算需要的值
Bi breakBi = array.get(bis, breakBiIdx)
float breakBiHigh = math.max(breakBi.startPrice, breakBi.endPrice)
float breakBiLow = math.min(breakBi.startPrice, breakBi.endPrice)
float endPrice = xdDir == "up" ? breakBiHigh : breakBiLow
```

**预防措施**:
- 编写代码前先查看 type 定义
- 使用代码补全功能避免拼写错误
- 修改 type 定义后，全局搜索所有使用该 type 的地方
- 为 type 字段添加注释说明用途

## 错误 10: 函数中修改全局变量
**错误信息**: `Cannot modify global variable "xxx" in function(CE10088)`
**现象**: 尝试在函数内部直接修改全局变量时编译失败
**根本原因**:
- Pine Script v6 不允许函数直接修改全局变量
- 对于基本类型（int/float/string/bool），需要使用 `ref` 关键字通过引用传递
- 对于 array/matrix/图形对象，它们本身就是引用传递，可以直接修改，但不需要 ref 关键字

**错误代码示例**:
```pinescript
// ❌ 错误：函数直接修改全局变量
var int biLineUsed = 0

resetPools() =>
    biLineUsed := 0  // 不能直接修改全局变量

drawBi(Bi bi, int idx) =>
    // ... 绘图逻辑
    biLineUsed += 1  // 不能直接修改全局变量
```

**解决方案**:
对于需要修改的基本类型变量，使用 `ref` 关键字通过引用传递：

**正确代码示例**:
```pinescript
// ✅ 正确：使用 ref 关键字
var int biLineUsed = 0
var int xdLineUsed = 0
var int zsBoxUsed = 0

resetPools(ref int biUsed, ref int xdUsed, ref int zsUsed) =>
    biUsed := 0
    xdUsed := 0
    zsUsed := 0

drawBi(Bi bi, int idx, ref int used) =>
    // ... 绘图逻辑
    used += 1

// 调用时传入全局变量
resetPools(biLineUsed, xdLineUsed, zsBoxUsed)
drawBi(bi, i, biLineUsed)
```

**预防措施**:
- 记住 Pine Script 的参数传递规则
- 基本类型（int/float/string/bool）默认值传递，需要 ref 才能修改外部
- array/matrix/图形对象默认引用传递，不需要 ref
- 全局变量集中管理，避免在函数中直接修改
- 使用 `g_` 前缀标识全局变量

## 错误 11: 访问不存在的局部变量
**错误信息**: `Undeclared identifier "xxx"(CE10272)`
**现象**: 尝试访问在其他作用域中定义的局部变量时编译失败
**根本原因**:
- 局部变量只在定义它的作用域内有效
- if/else/for/while 等代码块内定义的变量是块级作用域
- 函数内部定义的变量只在该函数内有效
- 不同代码块之间无法互相访问局部变量

**错误代码示例**:
```pinescript
// ❌ 错误：访问其他作用域的局部变量
if barstate.isconfirmed
    array<Fenxing> validFxs = filterValidFenxing(rawFxs)
    array<Bi> bis = buildBiFromFenxing(validFxs)
    // ... 其他逻辑

// 调试信息（在 if 块外部）
if barstate.islast and showRawFenxing
    table.cell(dbg, 1, 1, str.tostring(array.size(validFxs)))  // validFxs 不存在
    table.cell(dbg, 1, 2, str.tostring(array.size(bis)))        // bis 不存在
```

**解决方案**:
将需要跨作用域访问的变量声明为全局变量（使用 `var` 关键字）：

**正确代码示例**:
```pinescript
// ✅ 正确：使用全局变量
var array<Fenxing> g_validFxs = array.new<Fenxing>()
var array<Bi> g_bis = array.new<Bi>()
var array<Xianduan> g_xds = array.new<Xianduan>()

if barstate.isconfirmed
    array.clear(g_validFxs)
    array.clear(g_bis)
    array.clear(g_xds)

    g_validFxs := filterValidFenxing(rawFxs)
    g_bis := buildBiFromFenxing(g_validFxs)
    g_xds := buildXianduan(g_bis)

// 调试信息（可以访问全局变量）
if barstate.islast and showRawFenxing
    table.cell(dbg, 1, 1, str.tostring(array.size(g_validFxs)))
    table.cell(dbg, 1, 2, str.tostring(array.size(g_bis)))
    table.cell(dbg, 1, 3, str.tostring(array.size(g_xds)))
```

**预防措施**:
- 理解 Pine Script 的作用域规则
- 需要跨作用域访问的变量，使用 `var` 声明为全局变量
- 全局变量使用 `g_` 前缀标识
- 在每个 bar 开始时，记得清空全局数组避免数据残留
- 局部变量尽量限制在需要的作用域内使用

## 工作流程

1. 用户粘贴错误信息
2. 分析错误原因
3. 提供解决方案
4. 更新错误案例库
