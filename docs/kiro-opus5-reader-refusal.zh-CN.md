# Opus 5 读取工具触发 REASONING_EXTRACTION：问题跟踪

调查日期：2026-10-19。本文记录当时的源码核对、真实上游对照和客户端测试，
不代表后续模型版本或账号访问状态。

## 状态与影响

**尚未修复模型接受请求的问题。** 已确认失败来自 Kiro/Opus 5 上游的
`CONTENT_FILTERED / REASONING_EXTRACTION` 终止拒绝；具体分类规则仍未知。
普通读取任务疑似被误判，但不能把这一判断当作供应商已确认的根因。

拒绝可以发生在首次工具调用的参数生成过程中：工具尚未执行，上游尚未收到文件内容。
无历史、无检查点也能复现，因此历史 thinking 回放、自动压缩和 reader 返回内容
都不是触发该问题的必要条件。相同测试设置和任务格式下，标准 `Read` 也能触发，
不能将问题范围限定为某个自定义工具名称，也不能推断所有 `Read` 请求都会失败。

此前已修复的“拒绝元数据丢失、截断参数被执行、客户端看见成功空回复”属于协议适配问题。
正确传递拒绝、撤销尚未提交的工具调用和保留记账，并不等于模型已接受原始任务。

## Native Runtime 对照证据

直接调用真实 Kiro Runtime，绕过 CRS 和 Crosslane，只观察首次生成响应。
本轮共 38 次探测：32 次有效预期对照，其中 23 次返回 `REASONING_EXTRACTION`，
9 次完整生成工具参数；另有 1 次 ID 对照设置失误、5 次临时模型访问限制响应，
均不用于对应协议差异的因果判断。

请求中的 `inferenceConfig.maxTokens` 均为 `128000`，没有通过降低上限规避问题；
该记录只能证明发送了此字段，不能证明上游实际按该字段执行了输出预算。

| 对照条件 | 当时观察 | 可支持的结论 |
| --- | --- | --- |
| 原始完整任务、system 历史、长中文/emoji 路径 | `REASONING_EXTRACTION` | 原问题可在直接 Runtime 链路复现 |
| 无历史、完整任务、同一长路径 | `REASONING_EXTRACTION`，参数可在中途被截断 | 历史 thinking 或检查点不是必要条件 |
| 无历史、简短任务、同一 reader/schema 和长路径 | 两次完整生成工具参数 | Unicode、路径长度、工具名均不足以单独解释失败 |
| 无历史、完整任务、短路径 | 两次完整生成工具参数 | 相同任务表述并非始终被拒绝 |
| 完整任务、短路径，加入 system 配对或普通 Hello/Hello 历史 | `REASONING_EXTRACTION` | 提示与历史组合会影响结果，不能据此推导普遍关键词规则 |
| 只替换代理生成的确认句 | 原始任务仍拒绝 | 尚未得到有效的确认句兼容修复 |
| 只移除 Claude billing attribution 行 | 原始任务仍拒绝 | 尚未得到有效的 attribution 兼容修复 |
| 原始逻辑请求仅将目标换成 Opus 5.5 | 完整生成长路径工具参数 | 存在模型差异；未作为线上自动换模型方案 |

表中的“完整生成工具参数”仅表示首次响应为有效 `TOOL_USE`，工具没有在这些直接
探测中执行，不能计为读取成功或完整客户端会话通过。

## 本机 Claude Code 原链路复现

链路为：本机 Claude Code → 既有 Claude Relay Service → teamsbot Crosslane →
Kiro Runtime。使用隔离的自有文件和工具配置，显式设置 `128000` 输出上限、
`low` effort；这不是默认参数长上下文验收。

| 场景 | 客户端结果 | 实际工具执行 | 服务端链路证据 |
| --- | --- | --- | --- |
| 自定义 MCP reader，读取中文/emoji 文件路径并返回文件中的 NONCE | 退出码 1，`REASONING_EXTRACTION` | 0 次 | 两个客户端请求，均为 Opus 5 / Kiro Runtime |
| 相同测试设置和任务格式，读取工具换成标准 `Read` | 退出码 1，`REASONING_EXTRACTION` | 0 次 | 两个客户端请求，均为 Opus 5 / Kiro Runtime |

每个服务端请求只有一轮生成，没有 Crosslane 内部重复生成或隐式换模型。
客户端自身的拒绝恢复产生第二个请求。HTTP 200 是承载原生拒绝的协议响应，
不能据此将以上任务标记为成功。

## 连续拒绝后的临时模型访问限制

连续复现期间，首次记录限制响应的时间为 2026-10-19 18:56（北京时间）：

> Your access to this model has been temporarily restricted due to excessive content refusals.

该响应只有 `CONTENT_FILTERED` 和访问限制说明，没有 `REASONING_EXTRACTION`
分类；连此前能通过的简短 Opus 5 对照也被拦截。确认此状态后已停止继续请求 Opus 5。
这与单次任务的分类拒绝不同，不能继续用受限后的样本判断请求包装是否正确。

当时该账号对 Opus 5 的访问已受影响。限制出现后的 Opus 5.5 首次生成对照仍能
返回工具调用，但这不证明其他所有模型或完整会话均正常。服务 `health` / `ready`
正常也不能证明 Opus 5 可用。未确认恢复时间，不能根据本轮次数推导触发阈值或冷却时长。

`KIRO_CLI` origin 及完整 KAS 包装组合的相关对照已被这一访问限制干扰，
**尚未排除这些协议差异**。另一次 ID 形状实验因探测脚本覆盖了预期修改而排除，
不能计为该 ID 形状已验证。

[Kiro 官方仓库 #11635](https://github.com/kirodotdev/Kiro/issues/11635)
中，维护者将同类临时模型访问限制交给支持团队处理，没有给出可靠的恢复时长。
已准备脱敏支持证据，未向外部提交。

## GitHub 实现对照

以下是调查时审阅的处理方式，不代表它们已解决本例的触发条件。

| 来源 | 实际处理方式 | 对本例的适用范围 |
| --- | --- | --- |
| [Anthropic Python SDK 的 refusal 定义](https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/beta/beta_refusal_stop_details.py) | 将 `reasoning_extraction` 定义为在响应正文重现内部推理的拒绝分类 | 解释分类含义，不能解释本例普通读取任务为何被判入该类 |
| [oh-my-pi #6439](https://github.com/can1357/oh-my-pi/pull/6439) | 已合并：避免把历史 assistant thinking 序列化为正文交给 Claude 摘要器 | Crosslane 的 Claude 历史转换已排除 thinking；本例无历史也失败，不能直接套用为根因修复 |
| [oh-my-pi #12703](https://github.com/can1357/oh-my-pi/pull/12703) | 已合并：压缩档案不含 thinking 时，前言也不声明 thinking 标记 | 可用于核对压缩提示是否与正文一致；不能解释本例首次读取时的拒绝 |
| [morecup/kiro-messages，43cc63ce](https://github.com/morecup/kiro-messages/blob/43cc63ce4fc0cf452b1ae9c0d3dc333df9c0b304/internal/reqconv/build_payload.go) | 用 user/assistant 历史配对承载 system；当前源码使用 `KIRO_CLI` origin，reasoning 与普通正文分开 | system 配对思路与现有实现相近；origin 和完整 KAS 组合的效果仍待有效对照 |
| [ZyphrZero/kiro.rs，e581c2dc](https://github.com/ZyphrZero/kiro.rs/blob/e581c2dc2241ab0e587a5472a2e0d9cbb1e25d1c/src/kiro/model/events/metadata.rs) | 识别 `CONTENT_FILTERED` 和 refusal 元数据，避免成功空响应 | 支持正确表达上游拒绝，没有提供保证模型接受此类任务的通用修复 |
| [jwadow/kiro-gateway，a5292ca0](https://github.com/jwadow/kiro-gateway/blob/a5292ca04c7c6231e0b47673ac3f981f5a706e1e/kiro/converters_anthropic.py) | Anthropic 内容转换只抽取普通 text，不把 thinking 混入正文 | 本次审阅快照未找到针对精确 `REASONING_EXTRACTION` 的专门处理 |

## 已有兼容措施与后续核验

调查时 teamsbot 实际运行二进制对应 `64a09dd`。此前的 `e88db69` 和 `64a09dd`
已实现原生拒绝表达、工具提交前的终止确认，以及取消工具后的用量保留；源码入口见
[refusal 转换](../crates/crosslaned/src/http/crs/refusal.rs)和
[Claude 历史转换](../crates/crosslane-translate/src/translate/claude.rs)。
本轮调查未修改 Crosslane 运行代码、CRS 或服务配置，也未部署新的运行版本。

调查中重新执行的针对性验证为：历史 thinking 不混入正文测试 1 项通过，
原生 refusal 合约测试 3 项通过。此前完整测试结果不能算作本轮重新运行的结果。

后续仍需在模型访问恢复后核验：

1. 用小批量实验核对 `KIRO_CLI` origin 与配套 KAS 请求包装，保持原始消息、
   system、工具 schema、上下文和模型选择语义。
2. 核对原生控制字段的位置与实际生效情况，保持同一输出预算，不能降低上限来验收。
3. 每组差异前后都设置已知成功的对照；成功对照失效或出现模型访问限制时立即停止，
   将此后的样本与单次请求拒绝分开，不能继续得出协议因果结论。
4. 候选修复必须先通过本地协议回归，再用真实 Claude Code 按原链路验证工具执行
   和最终文件结果；仅返回 `TOOL_USE`、HTTP 200 或服务就绪不算验收通过。

原始请求、Runtime 事件、客户端输出和脱敏支持材料保留在本机私有证据包中；
其中原始请求含用户全局规则，不随仓库提交。本文仅记录调查事实和公开来源。
