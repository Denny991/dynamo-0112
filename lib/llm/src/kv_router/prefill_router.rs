// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// 导入必要的 Rust 标准库模块
use std::sync::{Arc, OnceLock};

// 导入第三方库
use anyhow::Result;           // 错误处理库
use futures::StreamExt;       // 异步流扩展
use rand::Rng;               // 随机数生成
use tokio::sync::{OwnedSemaphorePermit, oneshot};  // Tokio 异步同步原语
use tokio_util::sync::CancellationToken;           // 取消令牌
use tracing::Instrument;      // 追踪工具

// 导入 Dynamo 运行时相关模块
use dynamo_runtime::{
    component::Endpoint,      // 端点组件
    pipeline::{
        AsyncEngine, AsyncEngineContextProvider, Context, ManyOut, Operator, PushRouter,
        RouterMode, ServerStreamingEngine, SingleIn, async_trait,
    },
    protocols::{EndpointId, annotated::Annotated, maybe_error::MaybeError},
};

// 导入本地模块
use crate::{
    discovery::ModelManager,  // 模型管理器
    kv_router::{KvPushRouter, KvRouterConfig, RouterConfigOverride},  // KV 路由器相关
    protocols::common::llm_backend::{LLMEngineOutput, PreprocessedRequest},  // LLM 后端协议
    protocols::common::preprocessor::{BootstrapInfo, PrefillResult},         // 预处理协议
    protocols::common::timing::{RequestPhase, RequestTracker, WORKER_TYPE_PREFILL},  // 请求时序跟踪
};

/// 预填充路由过程中可能出现的错误
#[derive(Debug, thiserror::Error)]
pub enum PrefillError {
    /// 预填充路由器尚未激活
    #[error("Prefill router not yet activated")]
    NotActivated,

    /// 预填充执行过程中的错误
    /// TODO: 将预填充工作器错误与预填充路由器错误分开
    #[error("Prefill execution failed: {0}")]
    PrefillError(String),

    /// 预填充响应中未找到分离参数
    #[error("No disaggregated params in prefill response: {0}")]
    NoDisaggregatedParams(String),
}

/// PrefillRouter 内部使用的路由器
#[derive(Clone)]
enum InnerPrefillRouter {
    /// 使用 KvPushRouter 的 KV 感知路由
    KvRouter(Arc<KvPushRouter>),
    /// 简单路由（轮询、随机、直接）
    /// 注意：每个工作器的指标（active_prefill_tokens，active_decode_blocks）仅在
    /// KV 路由模式下可用，因为路由器具有实际的记账功能。
    SimpleRouter(Arc<PushRouter<PreprocessedRequest, Annotated<LLMEngineOutput>>>),
}

impl InnerPrefillRouter {
    /// 生成到特定工作器的可选直接路由
    /// 对于 KvRouter，target_worker 被忽略，因为 prefill_worker_id 已经设置在请求上
    /// 对于 SimpleRouter，target_worker 通过 router.direct() 触发直接路由
    async fn generate_to_worker(
        &self,
        request: SingleIn<PreprocessedRequest>,
        target_worker: Option<u64>,
    ) -> Result<ManyOut<Annotated<LLMEngineOutput>>> {
        match (self, target_worker) {
            // KvRouter: prefill_worker_id 已经设置在请求上，KvPushRouter::select_worker 使用它
            (InnerPrefillRouter::KvRouter(router), _) => router.generate(request).await,
            (InnerPrefillRouter::SimpleRouter(router), Some(worker_id)) => {
                router.direct(request, worker_id).await
            }
            (InnerPrefillRouter::SimpleRouter(router), None) => router.generate(request).await,
        }
    }

    /// 选择下一个工作器（仅适用于非 KV 模式）
    fn select_next_worker(&self) -> Option<u64> {
        match self {
            InnerPrefillRouter::SimpleRouter(router) => router.select_next_worker(),
            InnerPrefillRouter::KvRouter(_) => None,
        }
    }

    /// 查看下一个工作器而不增加状态（仅适用于非 KV 模式）
    fn peek_next_worker(&self) -> Option<u64> {
        match self {
            InnerPrefillRouter::SimpleRouter(router) => router.peek_next_worker(),
            InnerPrefillRouter::KvRouter(_) => None,
        }
    }
}

/// PrefillRouter 是一个仅前向操作符，位于迁移和解码路由器之间
/// 它可以选择在路由到解码之前调用预填充工作器，从预填充响应中提取 disaggregated_params
/// 并将它们注入到解码请求中
///
/// 模式：
/// - 仅查询：存在 `query_instance_id` 注解 → 不执行而返回工作器 ID
/// - 预路由：设置了 `prefill_worker_id`/`decode_worker_id` → 路由到指定工作器
/// - 正常：根据 KV 缓存状态由路由器确定工作器 ID
pub struct PrefillRouter {
    prefill_router: OnceLock<InnerPrefillRouter>,  // 预填充路由器实例
    model_manager: Arc<ModelManager>,              // 模型管理器
    endpoint_id: OnceLock<EndpointId>,             // 端点 ID
    cancel_token: CancellationToken,               // 取消令牌
    router_mode: RouterMode,                       // 路由模式
    enforce_disagg: bool,                          // 是否强制分离
    /// 用于查找工作器监视器以进行预填充客户端注册的模型名称
    model_name: String,
}

impl PrefillRouter {
    /// 创建一个禁用的预填充路由器，永远不会激活（仅传递）
    pub fn disabled(
        model_manager: Arc<ModelManager>,  // 模型管理器
        router_mode: RouterMode,          // 路由模式
        enforce_disagg: bool,             // 是否强制分离
    ) -> Arc<Self> {
        Arc::new(Self {
            prefill_router: OnceLock::new(),  // 初始化空的预填充路由器
            model_manager,
            endpoint_id: OnceLock::new(),     // 初始化空的端点 ID
            cancel_token: CancellationToken::new(),  // 创建取消令牌
            router_mode,
            enforce_disagg,
            model_name: String::new(), // 禁用路由器不使用此字段
        })
    }

    /// 创建一个新的预填充路由器实例
    pub fn new(
        activation_rx: oneshot::Receiver<Endpoint>,  // 激活接收器
        model_manager: Arc<ModelManager>,           // 模型管理器
        router_mode: RouterMode,                   // 路由模式
        kv_cache_block_size: u32,                  // KV 缓存块大小
        kv_router_config: Option<KvRouterConfig>,   // KV 路由器配置
        enforce_disagg: bool,                      // 是否强制分离
        model_name: String,                        // 模型名称
    ) -> Arc<Self> {
        let prefill_router = OnceLock::new();      // 创建空的预填充路由器锁
        let cancel_token = CancellationToken::new();  // 创建取消令牌

        let router = Arc::new(Self {
            prefill_router,
            model_manager: model_manager.clone(),   // 克隆模型管理器
            endpoint_id: OnceLock::new(),           // 初始化端点 ID 锁
            cancel_token: cancel_token.clone(),     // 克隆取消令牌
            router_mode,
            enforce_disagg,
            model_name,
        });

        // 启动后台任务等待激活
        let router_clone = router.clone();  // 克隆路由器用于后台任务
        tokio::spawn(async move {
            tokio::select! {  // 等待激活或取消
                result = activation_rx => {  // 等待激活接收器结果
                    let Ok(endpoint) = result else {
                        tracing::debug!("Prefill router activation channel closed without receiving endpoint");
                        return;
                    };

                    if let Err(e) = router_clone.activate(
                        endpoint,           // 端点
                        model_manager,      // 模型管理器
                        kv_cache_block_size,  // KV 缓存块大小
                        kv_router_config,   // KV 路由器配置
                    ).await {
                        tracing::error!(error = %e, "Failed to activate prefill router");
                    }
                }
                _ = cancel_token.cancelled() => {  // 如果取消令牌被触发
                    tracing::debug!("Prefill router activation cancelled");
                }
            }
        });

        router
    }

    /// 使用提供的端点激活预填充路由器
    async fn activate(
        &self,
        endpoint: Endpoint,                           // 端点
        model_manager: Arc<ModelManager>,            // 模型管理器
        kv_cache_block_size: u32,                   // KV 缓存块大小
        kv_router_config: Option<KvRouterConfig>,    // KV 路由器配置
    ) -> Result<()> {
        tracing::info!(
            router_mode = ?self.router_mode,
            "Activating prefill router"
        );

        // 存储 endpoint_id 供后续 build_bootstrap_info 使用
        let _ = self.endpoint_id.set(endpoint.id());

        // 为该端点启动运行时配置监视器（需要 get_disaggregated_endpoint）
        // 这必须在创建路由器之前完成，以便引导信息可用
        model_manager
            .get_or_create_runtime_config_watcher(&endpoint)
            .await?;

        // 根据路由模式创建内部路由器
        let inner_router = if self.router_mode.is_kv_routing() {
            // 使用端点创建 KV 选择器（这是一个预填充路由器）
            let kv_chooser = model_manager
                .kv_chooser_for(
                    &endpoint,
                    kv_cache_block_size,
                    kv_router_config,
                    WORKER_TYPE_PREFILL,  // 工作器类型为预填充
                )
                .await?;

            // 从 kv_chooser 提取客户端以确保共享状态
            let client = kv_chooser.client().clone();

            // 在分离模式下为 TTFT 指标清理注册预填充客户端与工作器监视器
            if let Some(monitor) = model_manager.get_worker_monitor(&self.model_name) {
                monitor.set_prefill_client(client.clone());
            }

            // 使用共享客户端为预填充构建 KV 模式的 PushRouter
            let push_router = PushRouter::<PreprocessedRequest, Annotated<LLMEngineOutput>>::from_client_with_threshold(
                client,
                RouterMode::KV,  // 路由模式为 KV
                None, // busy_threshold
                None, // worker_monitor
            )
            .await?;

            // 将其包装在 KvPushRouter 中
            InnerPrefillRouter::KvRouter(Arc::new(KvPushRouter::new(push_router, kv_chooser)))
        } else {
            // 为简单路由器创建客户端
            let client = endpoint.client().await?;

            // 在分离模式下为 TTFT 指标清理注册预填充客户端与工作器监视器
            if let Some(monitor) = model_manager.get_worker_monitor(&self.model_name) {
                monitor.set_prefill_client(client.clone());
            }

            // 使用前端的路由模式创建简单的推送路由器
            // 注意：每个工作器的指标（active_prefill_tokens，active_decode_blocks）仅在
            // KV 路由模式下可用，因为路由器具有实际的记账功能。
            let push_router = PushRouter::<PreprocessedRequest, Annotated<LLMEngineOutput>>::from_client_with_threshold(
                client,
                self.router_mode,  // 使用当前路由器模式
                None, // busy_threshold
                None, // worker_monitor
            )
            .await?;

            InnerPrefillRouter::SimpleRouter(Arc::new(push_router))
        };

        // 设置路由器（如果已设置则忽略错误）
        let _ = self.prefill_router.set(inner_router);

        tracing::info!(
            router_mode = ?self.router_mode,
            "Prefill router activated successfully"
        );

        Ok(())
    }

    /// 为分离服务构建 bootstrap_info
    /// 如果提供了 preselected_worker（GAIE 第二阶段），则直接使用它
    /// 否则，查询最佳工作器（KV 模式）或选择下一个工作器（非 KV 模式）
    async fn build_bootstrap_info(
        &self,
        req: &PreprocessedRequest,        // 预处理请求
        preselected_worker: Option<u64>,  // 预选工作器
    ) -> Option<(u64, u32, BootstrapInfo)> {
        let endpoint_id = self.endpoint_id.get()?;    // 获取端点 ID
        let prefill_router = self.prefill_router.get()?;  // 获取预填充路由器

        // 工作器选择逻辑
        let (worker_id, dp_rank) = if let Some(id) = preselected_worker {
            // 使用预选工作器
            let dp_rank = req.routing.as_ref().and_then(|r| r.dp_rank).unwrap_or(0);  // 获取数据并行排名
            tracing::debug!(
                worker_id = id,
                dp_rank = dp_rank,
                "Using pre-selected prefill worker for bootstrap"
            );
            (id, dp_rank)
        } else if self.router_mode.is_kv_routing() {
            // KV 模式：使用 find_best_match 方法
            let kv_router = match prefill_router {
                InnerPrefillRouter::KvRouter(r) => r,  // 获取 KV 路由器
                _ => return None,  // 如果不是 KV 路由器则返回 None
            };
            // 从路由提示中提取 LORA 名称
            let lora_name = req.routing.as_ref().and_then(|r| r.lora_name.clone());
            match async {
                // 在 KV 路由器中查找最佳匹配
                kv_router
                    .chooser
                    .find_best_match(None, &req.token_ids, None, false, lora_name)
                    .await
            }
            .instrument(tracing::info_span!("kv_find_best_match"))  // 添加追踪跨度
            .await
            {
                Ok((worker, _overlap)) => (worker.worker_id, worker.dp_rank),  // 返回工作器 ID 和 DP 排名
                Err(_) => return None,  // 错误则返回 None
            }
        } else {
            // 非 KV 模式：使用 PushRouter 的有状态选择
            // 我们使用 peek_next_worker 而不是 select_next_worker 来避免双重递增计数器
            // 如果我们回退到原始路径
            let worker_id = prefill_router.peek_next_worker()?;  // 查看下一个工作器
            (worker_id, 0)  // 返回工作器 ID 和默认 DP 排名 0
        };

        // 从 ModelManager 获取引导信息（适用于任何模式）
        let endpoint = self
            .model_manager
            .get_disaggregated_endpoint(endpoint_id, worker_id)?;  // 获取分离端点
        let host = endpoint.bootstrap_host?;  // 获取引导主机
        let port = endpoint.bootstrap_port?;  // 获取引导端口

        let bootstrap_room: u64 = rand::rng().random();  // 生成随机引导房间号

        tracing::info!(
            worker_id = worker_id,
            dp_rank = dp_rank,
            bootstrap_host = %host,
            bootstrap_port = port,
            bootstrap_room = bootstrap_room,
            router_mode = ?self.router_mode,
            "Built bootstrap_info upfront before prefill"
        );

        // 返回工作器 ID、DP 排名和引导信息
        Some((
            worker_id,
            dp_rank,
            BootstrapInfo {
                bootstrap_host: host,      // 引导主机
                bootstrap_port: port,      // 引导端口
                bootstrap_room,           // 引导房间号
            },
        ))
    }

    /// 使用给定的路由器执行预填充并提取结构化结果
    ///
    /// 当指定 target_worker 时，使用直接路由（用于带有引导优化的非 KV 模式）
    ///
    /// 如果提供了 `phase_permit`，它将在接收到第一个输出后被丢弃，
    /// 允许后续的 `set_phase` 调用继续进行。这在引导优化路径中使用
    /// 以确保 `record_worker_full` 在阶段更改之前完成
    ///
    /// 返回 (PrefillResult, Option<(worker_id, dp_rank)>)
    async fn execute_prefill(
        router: Option<InnerPrefillRouter>,      // 内部预填充路由器
        request: SingleIn<PreprocessedRequest>,  // 单输入预处理请求
        target_worker: Option<u64>,              // 目标工作器
        phase_permit: Option<OwnedSemaphorePermit>,  // 阶段许可
    ) -> Result<(PrefillResult, Option<(u64, u32)>), PrefillError> {
        // 检查路由器是否已激活
        let router = router.ok_or(PrefillError::NotActivated)?;
        
        // 生成预填充响应
        let mut prefill_response = router
            .generate_to_worker(request, target_worker)  // 使用目标工作器生成
            .await
            .map_err(|e| PrefillError::PrefillError(e.to_string()))?;  // 映射错误

        // 现在丢弃阶段许可 - 路由已完成，在 select_worker 中调用了 record_worker_full
        // 这样可以在不等待预填充输出的情况下解除对主任务中 set_phase(Decode) 的阻塞
        drop(phase_permit);

        // 获取第一个输出
        let Some(first_output) = prefill_response.next().await else {
            return Err(PrefillError::PrefillError(
                "Prefill router returned no output (stream ended)".to_string(),  // 预填充路由器未返回输出
            ));
        };

        // 从第一个输出中提取提示词标记详情
        let mut prompt_tokens_details = first_output
            .data
            .as_ref()
            .and_then(|o| o.completion_usage.as_ref())  // 获取完成使用情况
            .and_then(|u| u.prompt_tokens_details.clone());  // 克隆提示词标记详情

        // 遍历剩余输出以获取提示词标记详情
        while let Some(next) = prefill_response.next().await {
            if let Some(o) = next.data.as_ref()
                && prompt_tokens_details.is_none()  // 如果还没有提示词标记详情
            {
                prompt_tokens_details = o
                    .completion_usage
                    .as_ref()
                    .and_then(|u| u.prompt_tokens_details.clone());  // 从后续输出中获取提示词标记详情
            }
        }

        // 检查第一个输出是否有错误
        if let Some(err) = first_output.err() {
            return Err(PrefillError::PrefillError(format!(
                "Prefill router returned error in output: {err:?}",  // 预填充路由器在输出中返回错误
            )));
        }

        // 检查第一个输出是否有数据
        let Some(output) = &first_output.data else {
            return Err(PrefillError::NoDisaggregatedParams(
                "Prefill router output has no data field".to_string(),  // 预填充路由器输出没有数据字段
            ));
        };

        // 检查分离参数是否存在
        let Some(disaggregated_params) = output.disaggregated_params.clone() else {
            return Err(PrefillError::NoDisaggregatedParams(
                "Prefill router output missing disaggregated_params".to_string(),  // 预填充路由器输出缺少分离参数
            ));
        };

        // 从分离参数中提取预填充工作器 ID 和 DP 排名
        let prefill_worker_info =
            disaggregated_params
                .get("worker_id")  // 从 worker_id 字段获取
                .and_then(|worker_id_json| {
                    // 获取预填充工作器 ID
                    let worker_id = worker_id_json
                        .get("prefill_worker_id")  // 从预填充工作器 ID 字段获取
                        .and_then(|v| v.as_u64())?;  // 转换为 u64
                    // 获取预填充 DP 排名
                    let dp_rank = worker_id_json
                        .get("prefill_dp_rank")  // 从预填充 DP 排名字段获取
                        .and_then(|v| v.as_u64())  // 转换为 u64
                        .map(|r| r as u32)  // 转换为 u32
                        .unwrap_or(0);  // 默认值为 0
                    Some((worker_id, dp_rank))  // 返回工作器 ID 和 DP 排名
                });
                
        // 返回预填充结果和工作器信息
        Ok((
            PrefillResult {
                disaggregated_params,      // 分离参数
                prompt_tokens_details,     // 提示词标记详情
            },
            prefill_worker_info,          // 预填充工作器信息
        ))
    }

    /// 将预填充作为后台任务启动
    ///
    /// 当指定 target_worker 时，使用直接路由（用于带有引导优化的非 KV 模式）
    ///
    /// `phase_permit` 被传递给生成的任务并在第一个输出之后被丢弃，
    /// 允许主任务的 `set_phase(Decode)` 继续进行
    fn spawn_prefill_task(
        &self,
        prefill_request: SingleIn<PreprocessedRequest>,  // 预填充请求
        target_worker: Option<u64>,                      // 目标工作器
        phase_permit: OwnedSemaphorePermit,              // 阶段许可
    ) {
        let router = self.prefill_router.get().cloned();  // 获取路由器的克隆
        
        // 捕获当前跨度以将追踪上下文传播到生成的任务
        let span = tracing::Span::current();

        tokio::spawn(
            async move {
                // 执行预填充并处理结果
                match Self::execute_prefill(
                    router,              // 路由器
                    prefill_request,     // 预填充请求
                    target_worker,       // 目标工作器
                    Some(phase_permit),  // 阶段许可
                )
                .await
                {
                    Ok(_) => {
                        tracing::debug!("Prefill background task completed");  // 预填充后台任务完成
                    }
                    Err(e) => {
                        tracing::warn!("Prefill background task error: {e:?}");  // 预填充后台任务错误
                    }
                }
            }
            .instrument(span),  // 为异步块添加追踪跨度
        );
    }

    /// 调用预填充路由器并提取结构化的预填充结果、工作器 ID 和 DP 排名
    ///
    /// 这是同步预填充路径 - 我们在继续之前等待预填充完成
    /// 不需要阶段许可，因为 `record_worker` 在我们返回之前完成
    ///
    /// 返回 (PrefillResult, Option<(worker_id, dp_rank)>)
    async fn call_prefill(
        &self,
        request: SingleIn<PreprocessedRequest>,  // 请求
    ) -> Result<(PrefillResult, Option<(u64, u32)>), PrefillError> {
        // 对于 call_prefill 路径，路由由路由器本身处理（不需要直接路由）
        // 不需要阶段许可 - 我们在更改阶段之前等待完成
        Self::execute_prefill(self.prefill_router.get().cloned(), request, None, None).await
    }
}

impl Drop for PrefillRouter {
    /// 当 PrefillRouter 被丢弃时，取消后台激活任务
    fn drop(&mut self) {
        tracing::debug!("Dropping PrefillRouter, cancelling background activation task");  // 记录调试信息
        self.cancel_token.cancel();  // 取消激活任务
    }
}

#[async_trait]
impl
    Operator<
        SingleIn<PreprocessedRequest>,
        ManyOut<Annotated<LLMEngineOutput>>,
        SingleIn<PreprocessedRequest>,
        ManyOut<Annotated<LLMEngineOutput>>,
    > for PrefillRouter
{
    /// 生成函数实现预填充路由逻辑
    async fn generate(
        &self,
        request: SingleIn<PreprocessedRequest>,                                // 输入请求
        next: ServerStreamingEngine<PreprocessedRequest, Annotated<LLMEngineOutput>>,  // 下一个引擎
    ) -> Result<ManyOut<Annotated<LLMEngineOutput>>> {
        // 在保留上下文的同时提取请求数据
        let (mut req, context) = request.into_parts();      // 解构请求为 req 和 context
        let request_id = context.id().to_string();          // 获取请求 ID
        let engine_ctx = context.context();                 // 获取引擎上下文

        // 保存原始的 max_tokens 用于解码
        let original_max_tokens = req.stop_conditions.max_tokens;

        // 如果预填充路由器未激活，则直接跳转到解码
        if self.prefill_router.get().is_none() {
            if self.enforce_disagg {  // 如果强制分离模式
                return Err(anyhow::anyhow!(PrefillError::NotActivated));  // 返回错误
            }
            // 生成解码请求
            return next.generate(context.map(|_| req)).await;
        }

        // 确保在分离模式下路由决策存在跟踪器
        // 如果上游 DeltaGenerator 没有提供，则创建一个
        if req.tracker.is_none() {
            req.tracker = Some(Arc::new(RequestTracker::new()));  // 创建新的请求跟踪器
        }
        let tracker = req.tracker.as_ref().unwrap();  // 获取跟踪器引用
        let prefill_phase_permit = tracker.set_phase(RequestPhase::Prefill).await;  // 设置预填充阶段

        // 准备 max_tokens = 1 的预填充请求（在设置跟踪器后克隆）
        let mut prefill_req = req.clone();  // 克隆请求
        prefill_req.stop_conditions.max_tokens = Some(1);  // 设置最大令牌数为 1

        // 尝试 build_bootstrap_info 优化：如果我们能预先获得引导信息，
        // 在后台生成预填充并立即进入解码
        let preselected_worker = prefill_req
            .routing
            .as_ref()
            .and_then(|r| r.prefill_worker_id);  // 获取预选的预填充工作器 ID

        // 异步执行预填充逻辑
        let prefill_result = async {
            if let Some((worker_id, dp_rank, bootstrap_info)) = self
                .build_bootstrap_info(&prefill_req, preselected_worker)  // 构建引导信息
                .await
            {
                // 引导优化路径：在后台生成预填充
                // 我们成功使用了查看的工作器，因此现在必须推进路由器状态
                // 以确保下一个请求获得不同的工作器
                if !self.router_mode.is_kv_routing()  // 如果不是 KV 路由模式
                    && let Some(router) = self.prefill_router.get()  // 获取路由器
                {
                    router.select_next_worker();  // 选择下一个工作器
                }

                let routing = prefill_req.routing_mut();  // 获取路由的可变引用
                routing.prefill_worker_id = Some(worker_id);  // 设置预填充工作器 ID
                routing.dp_rank = Some(dp_rank);  // 设置 DP 排名
                prefill_req.bootstrap_info = Some(bootstrap_info.clone());  // 设置引导信息

                let prefill_context = Context::with_id(prefill_req, request_id.clone());  // 创建预填充上下文
                engine_ctx.link_child(prefill_context.context());  // 链接到子上下文

                // 将阶段许可传递给生成的任务 - 它在第一个输出后被丢弃（record_worker_full 完成）
                // 这允许下面的 set_phase(Decode) 仅在预填充路由完成后继续
                self.spawn_prefill_task(prefill_context, Some(worker_id), prefill_phase_permit);

                Ok((None, Some(worker_id), Some(bootstrap_info)))  // 返回结果
            } else {
                // 原始预填充路径：等待预填充完成
                tracing::debug!("Using original prefill path");  // 记录调试信息

                // 在调用 call_prefill 之前丢弃阶段许可 - 我们等待完成
                // 因此与下面的 set_phase(Decode) 没有竞争条件
                drop(prefill_phase_permit);  // 丢弃许可

                let prefill_context = Context::with_id(prefill_req, request_id.clone());  // 创建预填充上下文
                engine_ctx.link_child(prefill_context.context());  // 链接到子上下文

                let result = self.call_prefill(prefill_context).await;  // 调用预填充

                result.map(|(result, worker_info)| {
                    (Some(result), worker_info.map(|(id, _)| id), None)  // 映射结果
                })
            }
        }
        .instrument(tracing::info_span!("prefill_routing"))  // 添加追踪跨度
        .await;

        // 如果在预填充期间被取消则中止
        if engine_ctx.is_stopped() || engine_ctx.is_killed() {
            tracing::debug!("Abort entering decode after context is stopped or killed");  // 记录调试信息
            return Err(anyhow::anyhow!(
                "Context id {} is stopped or killed",  // 上下文 ID {} 已停止或被杀死
                engine_ctx.id()
            ));
        }

        // 处理预填充结果
        match prefill_result {
            Ok((maybe_prefill_result, _prefill_worker_id, bootstrap_info)) => {
                tracing::debug!("Prefill completed, proceeding to decode");  // 预填充完成，继续解码

                // 为解码请求设置阶段为解码
                // 在引导路径中，这会阻塞直到生成的预填充任务丢弃其许可
                // （在第一个输出/record_worker_full 完成后），确保路由的正确阶段
                if let Some(ref tracker) = req.tracker {  // 如果存在跟踪器
                    let _decode_permit = tracker.set_phase(RequestPhase::Decode).await;  // 设置解码阶段
                    // 许可立即被丢弃 - 解码继续，无需持有它
                }

                let mut decode_req = req;  // 创建解码请求

                // 使用预填充结果更新请求
                if let Some(prefill_result) = maybe_prefill_result {
                    decode_req.prefill_result = Some(prefill_result);  // 设置预填充结果
                }

                // 为解码恢复原始的 max_tokens
                decode_req.stop_conditions.max_tokens = original_max_tokens;

                // 为解码工作器注入引导信息
                if let Some(info) = bootstrap_info {
                    decode_req.bootstrap_info = Some(info);  // 设置引导信息
                }

                // 为解码设置路由器配置覆盖：overlap_score_weight = 0
                let existing_override = decode_req.router_config_override.take();  // 获取现有覆盖
                decode_req.router_config_override = Some(RouterConfigOverride {
                    overlap_score_weight: Some(0.0),  // 重叠分数权重设为 0
                    ..existing_override.unwrap_or_default()  // 使用默认值或现有覆盖
                });

                // 使用保留的上下文映射修改后的请求
                let decode_request = context.map(|_| decode_req);  // 映射解码请求
                next.generate(decode_request).await  // 生成解码请求
            }
            Err(PrefillError::NotActivated) => {
                if self.enforce_disagg {  // 如果强制分离模式
                    tracing::error!(
                        "Prefill router not activated, but disaggregated mode is enforced. Failing request."  // 预填充路由器未激活，但强制执行分离模式。请求失败。
                    );
                    return Err(anyhow::anyhow!(PrefillError::NotActivated));  // 返回错误
                }
                tracing::debug!("Prefill router not activated, falling back to decode-only");  // 预填充路由器未激活，回退到仅解码
                next.generate(context.map(|_| req)).await  // 生成仅解码请求
            }
            Err(e) => {
                if self.enforce_disagg {  // 如果强制分离模式
                    tracing::error!(
                        error = %e,
                        "Remote prefill failed, but disaggregated mode is enforced. Failing request."  // 远程预填充失败，但强制执行分离模式。请求失败。
                    );
                    return Err(anyhow::anyhow!(e));  // 返回错误
                }
                tracing::warn!(
                    error = %e,
                    "Remote prefill failed, falling back to decode-only. This may impact performance in disaggregated deployments. Verify prefill workers are healthy and accessible."  // 远程预填充失败，回退到仅解码。这可能会影响分离部署中的性能。验证预填充工作器是否健康且可访问。
                );
                next.generate(context.map(|_| req)).await  // 生成仅解码请求
            }
        }
    }
}
