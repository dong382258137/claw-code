---
name: "pine-script"
description: "Pine Script v6 编程专家。在编写、审查或调试 Pine Script 代码时自动调用，提供类型系统、数组操作、性能优化等方面的指导。"
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
