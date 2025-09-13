# Nokube-rs

分布式服务管理器的 Rust 实现

## 功能特性

### 配置管理 (config)
- etcd 集群配置存储
- 集群配置管理
- 节点配置管理

### 监控系统 (agent/general/exporter)
- 系统指标收集 (CPU, 内存, 网络)
- GreptimeDB 数据存储
- Grafana 监控面板

### 代理系统 (agent)
- 通用代理功能
- 主代理 (Master Agent)
- 远程命令执行
- 依赖安装管理

### 部署控制 (remote_ctl)
- SSH 管理器
- 分布式部署控制器
- 集群节点管理
- 服务部署

## 使用方法

### 集群部署和更新
```bash
nokube new-or-update [config-file]
```
如果不提供配置文件，会自动创建配置模板。

## 架构设计

遵循最小变更原则，文件分布即模块规划分布:

- `src/config/` - 配置管理模块
- `src/agent/` - 代理系统模块
  - `general/` - 通用功能
  - `master_agent/` - 主代理功能
- `src/remote_ctl/` - 远程部署控制模块

严格遵守 执行过程出现非特殊错误，立即终止不能继续

除非在我们设计文档里明确写出默认值的情况，其他配置性质、参数性质的均不能搞默认值，如果缺省直接报错

不要在开发过程中为了修复某个问题做手动操作，应该是在自动化部署代码中修复对应的逻辑，下一次部署时修正

监控
- greptime和grafana均属于关联服务，使用docker启动，使用节点对应ip（在cluster config中）进行连接（不要搞默认值，localhost这种）
- 可选：在 head 节点绑定一个简单的 HTTP 文件服务（python:3.10-slim），将 `workspace/<subpath>` 目录以只读方式挂载（默认子路径 `public_file_server`），默认端口 8088；Grafana 首页会展示 Greptime 与该 HTTP 服务的超链接。
- **日志收集**: 使用 OpenTelemetry OTLP/HTTP 协议原生发送到GreptimeDB
  - OTLP API: `http://<host>:4000/v1/otlp/v1/logs`
  - Headers: `X-Greptime-DB-Name: logs`, `X-Greptime-Log-Table-Name: nokube_logs`
  - 提取字段: `X-Greptime-Log-Extract-Keys: cluster_name,node_name,source,source_id`
  - 数据格式: JSON编码的OTLP LogRecord，包含timestamp(纳秒), severity, body, attributes
  - 表结构: opentelemetry_logs格式，timestamp(时间索引), severity_text, body, log_attributes(JSON)
- cluster 面板，展示各个节点的cpu、内存、网络上下行
- 除了cluster 面板,还要一个actor面板,用于展示所有的k8s元素列表,顶栏支持选取 namespace daemonset
   deployment pod container config secret 特定类别展示,  pod应该和关联的daemonset放在一起

service mode agent
- 运行于容器，使用 --pid以观测到宿主机的所有信息
- 远程节点执行，所以不应该生成任何脚本性质的东西，统一通过自己进程内的逻辑进行处理

远程节点操作
- new-or-update时会进行
- 统一通过agent传入base64 json参数来进行匹配执行
- 除了docker这种必须要用命令操作的对象，禁止使用cat、echo等原始命令操作，统一使用rust里对应的文件操作、字符串操作等库
- 分清楚new-or-update 集群部署和apply;
  监控是集群部署绑定的,服务部署不应该操作监控这种系统绑定服务

k8s抽象
- 我们会让nokube支持往k8s集群apply k8s yaml
- 我们会抽象出异步任务对象模板分别任职 
  - config secret，直接按一定的索引方式存于etcd中
  - 后续计算型的k8s对象均以service agent内的协程运行
    - kubecontroller，负责监控初始组件（daemonset、deployment）是否正常运行，并在关闭和配置更新的时候重新调度启动他们
    - daemonset，启动时加载配置，在nodeaffinity允许的所有节点上都部署一个实例）（动态附加节点名），并定期监控当前节点其关联的pod有没有被创建
    - deployment，根据nodeaffinity选出的节点中，按照目标replica随机分发几个实例到若干节点（动态附加名字后缀）定期监控当前节点其关联的pod有没有被创建
    - pod，循环run其关联的docker，需要在workspace下对应全局归属路径准备好挂载的config、secret
    - 全局归属路径 如 daemonset-aaa/deployment-node1/contianer-aaa 对应于他们单链的归属路径，各组件定期保活自己member alive的元数据，父组件轮询子组件，如果子组件关闭就要重启；子组件轮询父组件，如果父组件关闭自己也要直接退出

脚本\配置
- 命名需要能够直观知道作用

## 依赖

- etcd-rs - etcd 客户端
- ssh2 - SSH 连接管理  
- tokio - 异步运行时
- serde - 序列化/反序列化
- tracing - 日志系统
- reqwest - HTTP 客户端
