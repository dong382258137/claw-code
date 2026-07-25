---
name: "pine-script-syntax"
description: "Pine Script v6 语法专家。当用户遇到编译错误、类型错误、语法问题、Pine Script 版本问题、函数参数问题时，自动调用此 Skill 提供错误分析和解决方案。"
---

# Pine Script v6 语法专家

## 核心能力

1. **编译错误分析** - 解读 TradingView 错误信息
2. **类型系统指导** - 解决类型不匹配问题
3. **语法规则解释** - 说明 Pine Script v6 语法

## 常见错误速查

### 错误 1: 类型不匹配
**错误**: `Type mismatch: expected 'array<float>' but got 'array<int>'`
**解决**: 统一类型或显式转换

### 错误 2: ref 关键字误用
**错误**: `The 'ref' keyword cannot be used with this type`
**解决**: array/matrix/图形对象禁止使用 ref

### 错误 3: 变量遮蔽
**错误**: `Shadowing variable "xxx" which exists in parent scope`
**解决**: 使用 `:=` 重新赋值而非 `=` 声明

### 错误 5: 函数内部定义嵌套函数
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

### 错误 6: 使用保留关键字作为变量名
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

### 错误 7: 变量声明未初始化
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

### 错误 8: 代码行未完成就换行
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

### 错误 9: 访问不存在的类型字段
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

### 错误 10: 函数中修改全局变量
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

### 错误 11: 访问不存在的局部变量
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
