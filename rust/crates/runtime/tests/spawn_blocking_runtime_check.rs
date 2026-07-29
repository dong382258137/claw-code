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
    let in_runtime = tokio::task::spawn_blocking(|| {
        Handle::try_current().is_ok()
    })
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

#[tokio::test]
async fn spawn_blocking_block_on_panics() {
    // 验证:在 spawn_blocking 内调用 Runtime::block_on 会 panic
    let result = tokio::task::spawn_blocking(|| {
        let rt = tokio::runtime::Runtime::new().expect("create runtime");
        // 这应该 panic:"Cannot start a runtime from within a runtime"
        rt.block_on(async { 42 })
    })
    .await;

    // spawn_blocking 闭包 panic → JoinError
    assert!(
        result.is_err(),
        "Runtime::block_on inside spawn_blocking should panic, got: {result:?}"
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
