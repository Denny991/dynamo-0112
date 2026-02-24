# DeepSeek-R1模型配方

<cite>
**本文档引用的文件**
- [recipes/deepseek-r1/README.md](file://recipes/deepseek-r1/README.md)
- [recipes/deepseek-r1/sglang/README.md](file://recipes/deepseek-r1/sglang/README.md)
- [recipes/deepseek-r1/sglang/deepep.json](file://recipes/deepseek-r1/sglang/deepep.json)
- [recipes/deepseek-r1/model-cache/model-cache.yaml](file://recipes/deepseek-r1/model-cache/model-cache.yaml)
- [recipes/deepseek-r1/model-cache/model-download.yaml](file://recipes/deepseek-r1/model-cache/model-download.yaml)
- [recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml](file://recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml)
- [recipes/deepseek-r1/sglang/disagg-16gpu/deploy.yaml](file://recipes/deepseek-r1/sglang/disagg-16gpu/deploy.yaml)
- [recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml](file://recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml)
- [recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/perf.yaml](file://recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/perf.yaml)
- [recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml](file://recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml)
- [benchmarks/llm/perf.sh](file://benchmarks/llm/perf.sh)
- [deploy/observability/prometheus.yml](file://deploy/observability/prometheus.yml)
- [deploy/observability/grafana-datasources.yml](file://deploy/observability/grafana-datasources.yml)
</cite>

## 目录
1. [简介](#简介)
2. [项目结构](#项目结构)
3. [核心组件](#核心组件)
4. [架构概览](#架构概览)
5. [详细组件分析](#详细组件分析)
6. [依赖关系分析](#依赖关系分析)
7. [性能考虑](#性能考虑)
8. [故障排除指南](#故障排除指南)
9. [结论](#结论)
10. [附录](#附录)

## 简介

DeepSeek-R1是参数量达671B的Mixture-of-Experts (MoE)模型，本文档提供了针对该模型的综合性部署配方，涵盖VLLM、SGLang和TensorRT-LLM三种引擎在不同GPU配置下的优化设置。文档详细说明了8GPU和16GPU两种部署模式的Wide EP（专家分布）架构配置，并提供了完整的性能基准测试方案。

该配方支持多种硬件环境，包括H200和GB200 GPU，提供了从单节点到多节点的完整部署解决方案。所有配置均基于生产环境验证，确保在不同硬件条件下都能获得最佳性能表现。

## 项目结构

项目采用模块化组织方式，主要包含以下关键目录：

```mermaid
graph TB
subgraph "配方目录 (recipes/deepseek-r1)"
A[根配置文件]
B[SGLang配置]
C[TensorRT-LLM配置]
D[vLLM配置]
E[模型缓存配置]
end
subgraph "基准测试 (benchmarks)"
F[性能测试脚本]
G[负载生成器]
end
subgraph "部署工具 (deploy)"
H[可观测性配置]
I[Kubernetes资源]
J[监控仪表板]
end
A --> B
A --> C
A --> D
A --> E
F --> G
H --> I
H --> J
```

**图表来源**
- [recipes/deepseek-r1/README.md](file://recipes/deepseek-r1/README.md#L1-L104)
- [benchmarks/llm/perf.sh](file://benchmarks/llm/perf.sh#L1-L271)
- [deploy/observability/prometheus.yml](file://deploy/observability/prometheus.yml#L1-L63)

**章节来源**
- [recipes/deepseek-r1/README.md](file://recipes/deepseek-r1/README.md#L1-L104)

## 核心组件

### 模型缓存系统

模型缓存系统是DeepSeek-R1部署的核心基础设施，提供了高可用的模型存储解决方案：

| 组件 | 功能描述 | 存储容量 | 访问模式 |
|------|----------|----------|----------|
| PersistentVolumeClaim | Kubernetes持久卷声明 | 1500Gi | ReadWriteMany |
| 模型下载作业 | 自动化模型下载流程 | 1.3TB模型 | 单次执行 |
| 缓存策略 | 多副本冗余存储 | 高可用 | 并行访问 |

### 引擎适配层

三种推理引擎通过统一的适配层实现无缝集成：

```mermaid
classDiagram
class EngineAdapter {
+string modelName
+EngineConfig config
+initialize() void
+execute(request) Response
+shutdown() void
}
class SGLangEngine {
+string framework "SGLang"
+DeepEPConfig deepEP
+execute(request) Response
}
class VLLMEngine {
+string framework "vLLM"
+DEPConfig dep
+execute(request) Response
}
class TRTLLMEngine {
+string framework "TensorRT-LLM"
+WideEPConfig wideEP
+execute(request) Response
}
EngineAdapter <|-- SGLangEngine
EngineAdapter <|-- VLLMEngine
EngineAdapter <|-- TRTLLMEngine
```

**图表来源**
- [recipes/deepseek-r1/sglang/deepep.json](file://recipes/deepseek-r1/sglang/deepep.json#L1-L16)
- [recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml](file://recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml#L1-L110)

**章节来源**
- [recipes/deepseek-r1/model-cache/model-cache.yaml](file://recipes/deepseek-r1/model-cache/model-cache.yaml#L1-L13)
- [recipes/deepseek-r1/model-cache/model-download.yaml](file://recipes/deepseek-r1/model-cache/model-download.yaml#L1-L37)

## 架构概览

DeepSeek-R1采用分布式专家并行架构，通过三个核心组件实现高效推理：

```mermaid
graph TB
subgraph "前端层 (Frontend)"
FE[前端服务]
HC[健康检查]
LB[负载均衡]
end
subgraph "预填充节点 (Prefill Nodes)"
P1[预填充Worker 1]
P2[预填充Worker 2]
P3[预填充Worker 3]
P4[预填充Worker 4]
end
subgraph "解码节点 (Decode Nodes)"
D1[解码Worker 1]
D2[解码Worker 2]
D3[解码Worker 3]
D4[解码Worker 4]
D5[解码Worker 5]
D6[解码Worker 6]
D7[解码Worker 7]
D8[解码Worker 8]
end
subgraph "模型存储 (Model Storage)"
MC[模型缓存]
PV[持久卷]
end
FE --> HC
FE --> LB
HC --> P1
HC --> P2
HC --> P3
HC --> P4
HC --> D1
HC --> D2
HC --> D3
HC --> D4
HC --> D5
HC --> D6
HC --> D7
HC --> D8
P1 --> MC
P2 --> MC
P3 --> MC
P4 --> MC
D1 --> MC
D2 --> MC
D3 --> MC
D4 --> MC
D5 --> MC
D6 --> MC
D7 --> MC
D8 --> MC
MC --> PV
```

**图表来源**
- [recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml](file://recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml#L101-L251)
- [recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml](file://recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml#L1-L167)

### 硬件配置矩阵

| 配置类型 | GPU数量 | GPU型号 | 总显存 | 推荐用途 |
|----------|---------|---------|--------|----------|
| SGLang 8GPU | 16x | H200 | 2.2TB | 单节点部署 |
| SGLang 16GPU | 32x | H200 | 4.4TB | 多节点部署 |
| TensorRT-LLM | 36x | GB200 | ~2.5TB | 高吞吐量场景 |
| vLLM 16GPU | 32x | H200 | 4.4TB | 低延迟场景 |

**章节来源**
- [recipes/deepseek-r1/README.md](file://recipes/deepseek-r1/README.md#L74-L89)

## 详细组件分析

### SGLang Wide EP架构

SGLang引擎采用Wide Expert Parallel (WideEP)架构，专为MoE模型优化：

#### 8GPU配置详解

```mermaid
sequenceDiagram
participant Client as 客户端
participant Frontend as 前端服务
participant Prefill as 预填充节点
participant Decode as 解码节点
participant Model as 模型存储
Client->>Frontend : 请求处理
Frontend->>Prefill : 分配预填充任务
Prefill->>Model : 加载模型权重
Model-->>Prefill : 返回权重数据
Prefill->>Prefill : 执行预填充计算
Prefill->>Decode : 传输中间结果
Decode->>Model : 访问专家参数
Model-->>Decode : 返回专家数据
Decode->>Decode : 执行解码计算
Decode-->>Frontend : 返回最终结果
Frontend-->>Client : 响应完成
```

**图表来源**
- [recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml](file://recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml#L25-L110)

#### 16GPU配置详解

多节点扩展配置支持更大规模的部署需求：

| 参数 | 8GPU配置 | 16GPU配置 | 差异说明 |
|------|----------|-----------|----------|
| TP大小 | 8 | 16 | 张量并行规模翻倍 |
| DP大小 | 8 | 16 | 数据并行规模翻倍 |
| EP大小 | 8 | 16 | 专家并行规模翻倍 |
| 节点数 | 1 | 2 | 多节点部署 |
| 共享内存 | 80Gi | 80Gi | 内存配置保持一致 |

**章节来源**
- [recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml](file://recipes/deepseek-r1/sglang/disagg-8gpu/deploy.yaml#L1-L110)
- [recipes/deepseek-r1/sglang/disagg-16gpu/deploy.yaml](file://recipes/deepseek-r1/sglang/disagg-16gpu/deploy.yaml#L1-L116)

### TensorRT-LLM Wide EP架构

TensorRT-LLM采用专门的Wide EP优化，针对GB200硬件进行深度优化：

#### 性能配置分析

```mermaid
flowchart TD
Start([启动配置]) --> LoadModel[加载FP4量化模型]
LoadModel --> ConfigurePrefill[配置预填充引擎]
ConfigurePrefill --> ConfigureDecode[配置解码引擎]
ConfigureDecode --> SetTP[设置张量并行度]
SetTP --> SetEP[设置专家并行度]
SetEP --> EnableKV[启用KV缓存]
EnableKV --> StartWorkers[启动工作节点]
StartWorkers --> Monitor[监控性能指标]
Monitor --> Optimize{性能优化?}
Optimize --> |是| AdjustParams[调整参数]
Optimize --> |否| Complete[部署完成]
AdjustParams --> Monitor
```

**图表来源**
- [recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml](file://recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml#L49-L84)

#### 关键配置参数

| 配置项 | 预填充节点 | 解码节点 | 说明 |
|--------|------------|----------|------|
| 张量并行度 | 4 | 32 | GPU分配策略 |
| 专家并行度 | 4 | 32 | MoE专家分布 |
| 最大批大小 | 4 | 32 | 吞吐量控制 |
| 序列长度 | 1227 | 2251 | 输入限制 |
| KV缓存类型 | FP8 | FP8 | 精度选择 |
| 缓存复用 | 禁用 | 禁用 | 内存管理策略 |

**章节来源**
- [recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml](file://recipes/deepseek-r1/trtllm/disagg/wide_ep/gb200/deploy.yaml#L1-L251)

### vLLM DEP架构

vLLM采用Data-Expert Parallel (DEP)混合负载均衡架构：

#### 16GPU配置详解

```mermaid
graph LR
subgraph "数据并行层"
DP1[数据并行1]
DP2[数据并行2]
DP3[数据并行3]
DP4[数据并行4]
end
subgraph "专家并行层"
EP1[专家并行1]
EP2[专家并行2]
EP3[专家并行3]
EP4[专家并行4]
end
subgraph "混合负载均衡"
LB[负载均衡器]
RED[冗余专家]
end
DP1 --> LB
DP2 --> LB
DP3 --> LB
DP4 --> LB
EP1 --> RED
EP2 --> RED
EP3 --> RED
EP4 --> RED
LB --> EP1
LB --> EP2
LB --> EP3
LB --> EP4
```

**图表来源**
- [recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml](file://recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml#L80-L96)

#### 性能优化参数

| 参数名称 | 值 | 作用 |
|----------|----|------|
| VLLM_MOE_DP_CHUNK_SIZE | 384 | 数据并行分块大小 |
| 编译配置 | FULL_DECODE_ONLY | 图编译模式 |
| EPLB窗口大小 | 1000 | 负载均衡窗口 |
| 冗余专家数 | 32 | 性能冗余 |
| 最大序列数 | 512 | 并发控制 |

**章节来源**
- [recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml](file://recipes/deepseek-r1/vllm/disagg/deploy_hopper_16gpu.yaml#L1-L167)

## 依赖关系分析

### 系统依赖图

```mermaid
graph TB
subgraph "运行时依赖"
A[Dynamo平台]
B[Kubernetes集群]
C[NVIDIA容器运行时]
D[NCCL网络库]
end
subgraph "模型依赖"
E[HuggingFace Hub]
F[FP4量化模型]
G[原始模型权重]
end
subgraph "监控依赖"
H[Prometheus]
I[Grafana]
J[NATS消息队列]
end
A --> B
A --> C
B --> D
E --> F
E --> G
A --> H
H --> I
A --> J
```

**图表来源**
- [recipes/deepseek-r1/README.md](file://recipes/deepseek-r1/README.md#L14-L20)
- [deploy/observability/prometheus.yml](file://deploy/observability/prometheus.yml#L20-L50)

### 部署依赖链

每个部署配置都遵循相同的依赖关系模式：

1. **基础设施准备**: Kubernetes集群和网络配置
2. **模型准备**: 模型缓存和下载作业
3. **引擎部署**: 前端和工作节点部署
4. **监控配置**: 指标收集和可视化设置

**章节来源**
- [recipes/deepseek-r1/README.md](file://recipes/deepseek-r1/README.md#L21-L50)

## 性能考虑

### 基准测试框架

提供了完整的性能测试套件，支持多种测试场景：

```mermaid
sequenceDiagram
participant Test as 测试客户端
participant Perf as 性能测试脚本
participant Engine as 推理引擎
participant Metrics as 指标收集
Test->>Perf : 发起基准测试
Perf->>Engine : 发送请求批次
Engine->>Engine : 执行推理计算
Engine-->>Perf : 返回响应结果
Perf->>Metrics : 收集性能指标
Metrics-->>Perf : 输出测试报告
Perf-->>Test : 返回测试结果
```

**图表来源**
- [benchmarks/llm/perf.sh](file://benchmarks/llm/perf.sh#L215-L242)

### 性能调优建议

#### 内存优化策略

| 优化方向 | 参数调整 | 预期效果 |
|----------|----------|----------|
| KV缓存管理 | 调整free_gpu_memory_fraction | 减少内存占用 |
| 批处理大小 | 优化max_batch_size | 提高吞吐量 |
| 图编译 | 启用cuda_graph_config | 降低延迟 |
| 内存碎片 | 调整mem-fraction-static | 减少OOM风险 |

#### 网络优化策略

| 优化方向 | 参数调整 | 预期效果 |
|----------|----------|----------|
| RDMA支持 | 启用custom: rdma/ib | 提高网络带宽 |
| NCCL配置 | 调整通信后端 | 优化节点间通信 |
| 缓存传输 | 优化cache_transceiver_config | 减少网络延迟 |

**章节来源**
- [benchmarks/llm/perf.sh](file://benchmarks/llm/perf.sh#L1-L271)

## 故障排除指南

### 常见问题诊断

#### NCCL错误处理

NCCL相关错误通常指示内存不足问题：

```mermaid
flowchart TD
A[检测到NCCL错误] --> B{错误类型判断}
B --> |OOM错误| C[减少mem-fraction-static]
B --> |网络配置| D[检查RDMA设置]
B --> |进程冲突| E[重启相关进程]
C --> F[重新部署引擎]
D --> G[验证网络拓扑]
E --> H[清理资源]
F --> I[监控系统状态]
G --> I
H --> I
```

#### 模型加载问题

```mermaid
flowchart TD
A[模型加载失败] --> B{检查存储配置}
B --> |存储不足| C[扩容PVC]
B --> |权限问题| D[检查访问密钥]
B --> |网络问题| E[验证下载作业]
C --> F[重新部署]
D --> F
E --> F
F --> G[监控加载进度]
```

**章节来源**
- [recipes/deepseek-r1/README.md](file://recipes/deepseek-r1/README.md#L84-L90)

### 监控和调试

#### Prometheus监控配置

```mermaid
graph TB
subgraph "监控目标"
A[dynamo-frontend]
B[dynamo-backend]
C[nats-prometheus-exporter]
D[dcgm-exporter]
end
subgraph "采集器"
E[Prometheus服务器]
F[指标处理器]
end
subgraph "可视化"
G[Grafana仪表板]
H[告警系统]
end
A --> E
B --> E
C --> E
D --> E
E --> F
F --> G
F --> H
```

**图表来源**
- [deploy/observability/prometheus.yml](file://deploy/observability/prometheus.yml#L20-L57)
- [deploy/observability/grafana-datasources.yml](file://deploy/observability/grafana-datasources.yml#L18-L24)

**章节来源**
- [deploy/observability/prometheus.yml](file://deploy/observability/prometheus.yml#L1-L63)
- [deploy/observability/grafana-datasources.yml](file://deploy/observability/grafana-datasources.yml#L1-L24)

## 结论

DeepSeek-R1模型配方提供了完整的生产级部署解决方案，涵盖了三种主流推理引擎的优化配置。通过Wide EP架构和专业的性能调优，该配方能够在不同硬件环境下实现最佳的推理性能。

关键优势包括：
- **多引擎支持**: VLLM、SGLang、TensorRT-LLM三种引擎的统一配置
- **灵活的硬件适配**: 支持8GPU和16GPU的不同部署需求
- **完善的监控体系**: 全面的指标收集和可视化支持
- **生产就绪**: 经过验证的配置参数和故障排除方案

建议根据具体的硬件条件和性能需求选择合适的部署方案，并结合监控系统持续优化系统性能。

## 附录

### 快速部署步骤

1. **环境准备**
   ```bash
   export NAMESPACE=dynamo-demo
   kubectl create namespace ${NAMESPACE}
   ```

2. **创建HuggingFace令牌**
   ```bash
   kubectl create secret generic hf-token-secret \
     --from-literal=HF_TOKEN="your-token-here" \
     -n ${NAMESPACE}
   ```

3. **部署模型缓存**
   ```bash
   kubectl apply -f model-cache.yaml -n ${NAMESPACE}
   kubectl apply -f model-download.yaml -n ${NAMESPACE}
   ```

4. **启动推理服务**
   ```bash
   kubectl apply -f sglang/disagg-8gpu/deploy.yaml -n ${NAMESPACE}
   ```

### 性能测试命令

```bash
# 基准测试脚本使用
./perf.sh \
  --model deepseek-ai/DeepSeek-R1 \
  --input-sequence-length 1024 \
  --output-sequence-length 1024 \
  --concurrency 32,64,128 \
  --url http://localhost:8000

# 直接使用aiperf进行测试
aiperf profile \
  --model deepseek-ai/DeepSeek-R1 \
  --endpoint /v1/chat/completions \
  --concurrency 64 \
  --url http://localhost:8000
```