//! 验证 tokio::task::spawn_blocking 闭包内 Handle::try_current() 的行为。
//!
//! 用于确认 subagent_dispatcher.rs 中 "Cannot start a runtime from within a runtime"
//! panic 的根因:spawn_blocking 闭包是否保留 runtime 上下文。

use tokio::runtime::Handle;

#[tokio::test]
async fn spawn_blocking_try_current_returns_ok() {
    // 在 async 上下文中,Handle::try_current() 应返回 Ok
    assert!(Handle::try_current().is_ok());

    // spawn_blocking 闭包内是否仍能检测到 runtime?
    let in_runtime = tokio::task::spawn_blocking(|| Handle::try_current().is_ok())
        .await
        .expect("spawn_blocking should not panic");

    // 关键断言:如果 spawn_blocking 保留 runtime 上下文,这里为 true
    // 这就是 client.stream() 内部 runtime.block_on 触发 panic 的根因
    assert!(
        in_runtime,
        "Handle::try_current() returned Err inside spawn_blocking — \
         the nested-runtime panic must come from a different path"
    );
}

#[tokio::test]
async fn os_thread_try_current_returns_err() {
    // 在独立 OS 线程上,Handle::try_current() 应返回 Err
    let in_runtime = std::thread::spawn(|| Handle::try_current().is_ok())
        .join()
        .expect("OS thread should not panic");

    assert!(
        !in_runtime,
        "Handle::try_current() should return Err on a fresh OS thread"
    );
}

/// 验证 tokio 1.50 中 `spawn_blocking` 闭包内新建 runtime 并 `block_on` 的行为。
///
/// **结论(实测 tokio 1.50)**:**不** panic,`block_on(async { 42 })` 正常返回 42。
/// 原因:`spawn_blocking` 闭包运行在独立的 blocking 线程池线程上,该线程的
/// `Context.runtime` 恒为 `EnterRuntime::NotEntered`(context.rs:101 默认值),
/// 因此 `Runtime::block_on` 内部的 `try_enter` 检查通过,不会触发
/// "Cannot start a runtime from within a runtime" panic。
///
/// 这印证了 `subagent_dispatcher.rs::dispatch_impl`(Epic 3b)的设计:同步
/// `client.stream()`(内部 `self.runtime.block_on`,streaming.rs:420)被隔离在
/// `std::thread::spawn` 的独立 OS 线程内调用,该线程同样 `NotEntered`,故
/// block_on 安全,不会嵌套 panic。
///
/// 真正会 panic 的场景是:在**同一个已进入的 runtime 工作线程**上嵌套 block_on
/// (此时 `Context.runtime` 为 `Entered`)。生产代码用 `stream_async`(路径 A,
/// conversation.rs:3869)或 `Handle::try_current().is_ok()` 守卫(conversation.rs:3617)
/// 规避该场景。
#[tokio::test]
async fn spawn_blocking_block_on_does_not_panic() {
    let result = tokio::task::spawn_blocking(|| {
        let rt = tokio::runtime::Runtime::new().expect("create runtime");
        // tokio 1.50:blocking 线程 NotEntered,不 panic,正常返回 42
        rt.block_on(async { 42 })
    })
    .await;

    let value = result.expect(
        "tokio 1.50: Runtime::block_on inside spawn_blocking should succeed \
                 (blocking thread Context.runtime = NotEntered), got nested runtime panic",
    );
    assert_eq!(
        value, 42,
        "block_on(async {{ 42 }}) should resolve to 42 inside spawn_blocking"
    );
}

/// 验证真正的嵌套 panic:在**已进入的 runtime 工作线程**上创建新 runtime 并 block_on。
///
/// 这是生产代码 `Handle::try_current().is_ok()` 守卫要规避的场景
/// (conversation.rs:3617 / 3074)。`#[tokio::test]` 的 async 测试体运行在已进入的
/// runtime 工作线程上,此处新建 runtime 并 block_on 应 panic。
#[tokio::test]
async fn nested_block_on_in_runtime_worker_panics() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt = tokio::runtime::Runtime::new().expect("create runtime");
        rt.block_on(async { 42 })
    }));

    assert!(
        result.is_err(),
        "Runtime::block_on on an already-entered runtime worker thread should panic \
         (nested runtime), got: {result:?}"
    );
}

/// 模拟 claw-shell 的 current_thread + LocalSet 环境
#[test]
fn current_thread_runtime_spawn_blocking_block_on() {
    use tokio::task::LocalSet;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create current_thread runtime");
    let local = LocalSet::new();

    let result = local.block_on(&rt, async {
        // 在 current_thread + LocalSet 上下文中 spawn_blocking
        tokio::task::spawn_blocking(|| {
            let inner_rt = tokio::runtime::Runtime::new().expect("create inner runtime");
            inner_rt.block_on(async { 42 })
        })
        .await
    });

    eprintln!("current_thread + LocalSet spawn_blocking block_on result: {result:?}");
}

/// 模拟 claw-shell 的 current_thread + LocalSet 环境 — 直接在 async 上下文调用 block_on
#[test]
fn current_thread_runtime_direct_block_on_panics() {
    use tokio::task::LocalSet;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create current_thread runtime");
    let local = LocalSet::new();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        local.block_on(&rt, async {
            // 直接在 async 上下文创建新 runtime 并 block_on — 应该 panic
            let inner_rt = tokio::runtime::Runtime::new().expect("create inner runtime");
            inner_rt.block_on(async { 42 })
        })
    }));

    eprintln!("current_thread + LocalSet direct block_on result: {result:?}");
}
