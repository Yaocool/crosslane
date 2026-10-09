# Crosslane 运维指南

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [项目说明](../README.zh-CN.md)

本文介绍部署、账号、服务访问、日常检查和备份。命令在仓库根目录执行，并使用
与服务匹配的 `crosslane` CLI。

## 环境与配置

Docker 部署需要 Docker Engine 和 Compose。本地编译支持 Linux、macOS，需要 Rust
和 C 工具链；原生环境使用浏览器登录时还需 Chrome 或 Chromium。

本地安装可将 `.env.example` 复制为 `.env`。从不同目录启动 daemon 和 CLI 时，
将 `CROSSLANE_HOME` 设为绝对路径。使用 `crosslane config path` 查找当前配置，
编辑后运行 `crosslane config reload`。不要把账号凭证写进命令参数、日志或版本库。

## Docker

构建当前源码并安装宿主机 CLI 命令：

```bash
./deploy/docker-setup.sh --build
crosslane health
crosslane ready
```

也可使用 `./deploy/docker-setup.sh --image IMAGE` 部署可用镜像。宿主机 CLI 默认安装到
`/usr/local/bin/crosslane`；其他位置使用安装脚本的 `--target PATH` 参数。

Compose 使用 host network。本机访问的服务应绑定 `127.0.0.1`；向外开放时需限制端口
并配置 TLS。数据保存在 `crosslane-data` 卷中。日常检查与升级：

```bash
docker compose ps
docker compose logs -f crosslaned
./deploy/docker-upgrade.sh
crosslane version
crosslane ready
```

升级前停止写入并备份完整数据卷。镜像回退不会撤销数据变更。`crosslane stop` 可停止部署
并保留数据；移除前先查看 `crosslane uninstall --help`。

## 账号

### Kiro

可从 CLI 环境变量或标准输入导入 headless API key：

```bash
crosslane account add-api-key --email ci@example.com --region us-east-1
crosslane account add-api-key --email ci@example.com --region us-east-1 --key-stdin < /secure/kiro-key
```

第一条命令读取 CLI 环境中的 `KIRO_API_KEY`。企业 SSO 需先设置组织的登录入口，再添加账号：

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

```bash
crosslane config reload
crosslane account add-sso --email user@example.com --password-stdin < /secure/kiro-sso-password
```

交互、批量和 stdin 参数见 `crosslane account add-sso --help`。使用
`crosslane account list` 检查账号，刷新选项见 `crosslane account refresh --help`。
账号导出默认脱敏；包含凭证的导出只应存放在受保护的备份中。

### GitHub Copilot

先创建 Copilot 来源，再通过 GitHub Device Flow 授权，或从标准输入导入已有 token：

```bash
crosslane provider add --id copilot --kind copilot
crosslane account add --provider copilot --auth device-flow
crosslane account add --provider copilot --token-stdin < /secure/github-token
crosslane account list --provider copilot
```

选择组织允许的账号方式。`--provider` 使用 `provider add --id` 指定的实例 ID。

## 服务与客户端访问

新安装不会自动创建业务监听。创建服务并妥善保存命令输出的客户端 API Key：

```bash
crosslane service create --name main --host 127.0.0.1
crosslane service list
crosslane ready
```

仅使用 Copilot 的服务可以指定来源及默认来源：

```bash
crosslane service create --name copilot --host 127.0.0.1 --provider copilot --default-provider copilot
```

服务只接受已绑定的客户端 Key。生成接口还会检查客户端产品 User-Agent。接入其他客户端时，
操作员可在服务或客户端 Key 上设置 `--skip-user-agent-check true`；Key 认证和其他服务限制
仍然生效。调整访问前先查看命令帮助：

```bash
crosslane help service edit
crosslane help apikey edit
```

## 模型与额度

```bash
crosslane models list
crosslane model disable MODEL_ID
crosslane model enable MODEL_ID
crosslane pool --model MODEL_ID --explain
crosslane account overage show
```

模型 ID 从列表中选取。禁用模型后，新请求将无法使用。Kiro overage 默认关闭。
要将每个账号的超额额度上限设为 500 credits，可运行
`crosslane account overage enable --max-credits 500`；关闭时运行
`crosslane account overage disable`。上游限制和已在处理中的请求仍可能影响最终用量。

## 日志与排障

```bash
crosslane logs show --tail 100
crosslane logs follow --level warn
crosslane logs trace <TRACE_ID>
crosslane status --since 30m
crosslane stats --detail --since 1h
```

业务请求日志会记录客户端提供的 `user_agent`。可用响应头 `x-trace-id` 或 `request-id`
查找请求日志。排查失败时同时看响应状态、错误码和 Trace。流式响应也可能在 HTTP 200 后失败，
应检查最终事件。

| 现象 | 优先检查 |
| --- | --- |
| `health` 失败 | daemon/容器状态和日志 |
| `ready` 失败 | 服务、账号、模型和额度状态 |
| 401 或客户端被拒绝 | 绑定的客户端 Key、User-Agent 策略 |
| 503 | 账号可用性、额度与并发 |
| 流式中断 | 最终流事件和 Trace 日志 |
| 上下文超限 | 实际选中模型及请求大小 |

### 客户端与会话归属

所有构建都支持客户端识别，无需启用 `crs` feature。Claude Code 从旧格式或 JSON 字符串格式的
`metadata.user_id` 提取设备、会话；Codex 从 `client_metadata` 提取
`x-codex-installation-id`、`session_id`、`thread_id`，相应 HTTP 头作为后备。
CRS Console 和 OpenAI Responses 转发保留这些字段时即可使用，无需修改 CRS。

`proxy.client.context` 事件及请求日志的 `diagnostics.client_context` 记录哈希后的
`user_ref`、`session_ref`、`thread_ref`、客户端类型与身份来源。同一设备/安装在不同会话中
保持相同用户归属；这不代表自然人或 CRS Key。信息缺失时保持未知，异常或过长的标识会被忽略。

生成与 token 计数采用一致的会话规则，压缩重放按服务、Crosslane Key、设备、会话/线程隔离，
并继续校验完整历史前缀。Responses 续接在原有服务/Key 归属校验后继承省略的信息；分叉线程使用
独立的上游会话，已知设备冲突返回请求错误。由 provider 保存历史的 Responses 归属信息仅在
成功响应后保存在内存，最多 256 条、30 分钟；`store: false` 禁止保留。
账号池调度、额度与按请求内容计算的 token 缓存继续使用原有策略。

## 备份与恢复

升级或修改可能影响数据的配置前先备份。标准 Docker 部署可停止写入后复制数据目录：

```bash
umask 077
backup_dir="$HOME/crosslane-backup-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$backup_dir"
docker compose stop crosslaned
docker compose cp crosslaned:/var/lib/crosslane/. "$backup_dir/"
docker compose start crosslaned
```

备份中包含凭证，应检查内容并妥善保护。原生部署先停止 daemon，再根据
`crosslane config path` 显示的位置同时备份配置和数据。恢复时先写入隔离目录或新卷，
保留所有权和权限；核对 `ready`、账号、服务及真实请求后再切回流量。
