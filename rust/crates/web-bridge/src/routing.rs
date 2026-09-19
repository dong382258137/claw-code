//! 混合分工路由:把「纯推理回合」交给网页后端(零 token),其余保持 API 后端。
//!
//! # 设计动机
//! 网页版(deepseek-web)与 API 端优劣势互补:
//! - 网页端:零 token、无需计费,但工具协议弱(```claw 代码块 + `parse_reply`
//!   文本模拟,`WebApiClient` 整段往返),长程工具密集型任务不如 API 端稳;
//! - API 端:原生 tool-calling、prefix cache、`report_usage` 完整,但计入 token。
//!
//! 因此按「轮次档位」做优先级分配(见项目记忆「混合分工」方向):
//! - [`TriageTier::Trivial`] 主回复(纯推理、零工具、只注入核心工具)→ **网页**;
//! - [`TriageTier::Standard`] 主回复(全量注入、需断工具)→ **API**;
//! - 分诊请求(triage,`with_model(deepseek-v4-flash)`,零工具)→ **网页**;
//! - 子智能体请求(`with_model_and_capability`,可能带工具白名单)→ **API**。
//!
//! # 分诊识别
//! trait [`runtime::ApiClient::with_model`] 的生产环境**唯一调用方**是
//! `ConversationRuntime::triage_turn_llm`(conversation.rs:3435,传 `TRIAGE_MODEL`);
//! 子智能体走 `with_model_and_capability`(不触发本 trait 的 with_model)。因此
//! [`RoutingApiClient::with_model`] 对所有结果统一标记 `route_web_all = true`,
//! 返回的 webs 路由客户端会把分诊请求也送往网页,不殃及带工具的子智能体。

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use runtime::triage::TriageTier;
use runtime::{ApiClient, ApiRequest, AssistantEvent, RuntimeError, StatusEmitter};

/// 在 API 后端与网页后端之间按轮次档位分发的委托客户端。
///
/// 由 CLI 层(`app.rs::build_api_client`)构造:当主后端是 API(Anthropic)且
/// 检测到本机已登录网页凭证时,自动包一层,把纯推理回合路由到网页零 token。
///
/// `web` 用 `Arc<Mutex>` 共享,使 `with_model`(分诊)产出的子路由客户端能复用
/// 同一网页实例与其已注入的 StatusEmitter。
pub struct RoutingApiClient {
    api: Box<dyn ApiClient>,
    /// 共享的网页后端(同一实例,跨主路由/分诊复用,mutex 串行化访问)。
    web: Arc<Mutex<Box<dyn ApiClient>>>,
    /// 该实例的所有请求是否都路由到网页(分诊专用)。
    route_web_all: bool,
}

impl RoutingApiClient {
    /// 构造主路由客户端:`api` 为主 API 后端,`web` 为已 box 的网页后端。
    #[must_use]
    pub fn new(api: Box<dyn ApiClient>, web: Box<dyn ApiClient>) -> Self {
        Self {
            api,
            web: Arc::new(Mutex::new(web)),
            route_web_all: false,
        }
    }

    /// 判定本次请求是否应路由到网页后端。
    fn should_route_to_web(&self, request: &ApiRequest) -> bool {
        if self.route_web_all {
            return true;
        }
        // 主回复:Trivial 档(纯推理、零工具)→ 网页;Standard(全量、需工具)→ API。
        matches!(request.triage_tier, Some(TriageTier::Trivial))
    }
}

impl ApiClient for RoutingApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if self.should_route_to_web(&request) {
            let mut web = self
                .web
                .lock()
                .map_err(|_| RuntimeError::new("web lock poisoned".to_string()))?;
            web.stream(request)
        } else {
            self.api.stream(request)
        }
    }

    fn stream_async<'a>(
        &'a mut self,
        request: ApiRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<AssistantEvent>, RuntimeError>> + Send + 'a,
        >,
    > {
        if self.should_route_to_web(&request) {
            // 网页后端本质是同步 round_trip(WebApiClient::stream 一次性取回整段
            // 回复),无需在 await 上持锁:短暂持 std Mutex、同步调用、立即释放。
            let output = match self.web.lock() {
                Ok(mut guard) => guard.stream(request),
                Err(_) => Err(RuntimeError::new("web lock poisoned".to_string())),
            };
            Box::pin(async move { output })
        } else {
            self.api.stream_async(request)
        }
    }

    /// 分诊:经 `with_model` 新建的 client 一律路由网页(见模块头注释)。
    fn with_model(&self, model: &str) -> Result<Box<dyn ApiClient>, String> {
        let sub_api = self.api.with_model(model)?;
        Ok(Box::new(Self {
            api: sub_api,
            web: self.web.clone(),
            route_web_all: true,
        }))
    }

    /// 子智能体(可能带工具白名单)→ 保持 API 端,维持其原生工具能力。
    fn with_model_and_capability(
        &self,
        model: &str,
        capability: runtime::multi_agent::SubagentCapability,
    ) -> Result<Box<dyn ApiClient>, String> {
        self.api.with_model_and_capability(model, capability)
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.api.set_reasoning_effort(effort);
    }

    fn set_thinking_mode(&mut self, thinking: Option<bool>) {
        self.api.set_thinking_mode(thinking);
    }

    fn thinking_mode(&self) -> Option<bool> {
        self.api.thinking_mode()
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.api.reasoning_effort()
    }

    fn set_status_emitter(&mut self, emitter: StatusEmitter) {
        // 同时注入 api 与共享 web:Standard/子智能体回合需 API 的 emitter,
        // 网页回合由共享实例内部 emitter 驱动 TUI 显示。
        self.api.set_status_emitter(emitter.clone());
        if let Ok(mut web) = self.web.lock() {
            web.set_status_emitter(emitter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ConversationMessage, RequestKind, SystemPromptSplit};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 记录 `stream` 是否被调用的假后端。
    #[derive(Clone)]
    struct Spy(Arc<AtomicBool>);

    impl Spy {
        fn new() -> (Self, Arc<AtomicBool>) {
            let flag = Arc::new(AtomicBool::new(false));
            (Self(flag.clone()), flag)
        }
    }

    impl ApiClient for Spy {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.0.store(true, Ordering::SeqCst);
            Ok(vec![AssistantEvent::MessageStop])
        }

        fn with_model(&self, _model: &str) -> Result<Box<dyn ApiClient>, String> {
            Ok(Box::new(Spy(self.0.clone())))
        }
    }

    fn request(tier: Option<TriageTier>) -> ApiRequest {
        ApiRequest {
            system_prompt: SystemPromptSplit::from_sections(vec!["sys".into()]),
            messages: vec![ConversationMessage::user_text("hi")],
            request_kind: RequestKind::Main,
            triage_tier: tier,
        }
    }

    #[test]
    fn standard_routes_to_api() {
        let (api, called_api) = Spy::new();
        let (web, called_web) = Spy::new();
        let mut router = RoutingApiClient::new(Box::new(api), Box::new(web));
        let _ = router.stream(request(Some(TriageTier::Standard)));
        assert!(called_api.load(Ordering::SeqCst), "Standard 应走 API");
        assert!(!called_web.load(Ordering::SeqCst), "Standard 不应走网页");
    }

    #[test]
    fn trivial_routes_to_web() {
        let (api, called_api) = Spy::new();
        let (web, called_web) = Spy::new();
        let mut router = RoutingApiClient::new(Box::new(api), Box::new(web));
        let _ = router.stream(request(Some(TriageTier::Trivial)));
        assert!(!called_api.load(Ordering::SeqCst), "Trivial 不应走 API");
        assert!(called_web.load(Ordering::SeqCst), "Trivial 应走网页");
    }

    #[test]
    fn triage_all_routes_to_web() {
        let (api, _called_api) = Spy::new();
        let (web, called_web) = Spy::new();
        let router = RoutingApiClient::new(Box::new(api), Box::new(web));
        // with_model(deepseek-v4-flash) → route_web_all=true:分诊请求(tier=None)也走网页。
        let mut sub = router.with_model("deepseek-v4-flash").unwrap();
        let _ = sub.stream(request(None));
        assert!(called_web.load(Ordering::SeqCst), "分诊请求应走网页");
    }
}
