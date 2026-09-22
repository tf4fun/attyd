# HTTP 请求超时与传输边界

日期：2026-09-22。

## 当前协议

浏览器与 attyd 的业务接口使用 HTTP JSON 请求和 SSE：GET 读取快照、目录及过程页，
POST 提交业务操作，DELETE 删除会话。全局事件流为 `/api/v1/events`，会话事件流为
`/api/v1/sessions/:sessionId/events`。浏览器业务层没有 WebSocket。

Bridge 与 Agent 是另一层连接，默认通过 stdio 传输 ACP，也支持 ACP WebSocket 和
Streamable HTTP。开发环境代理 Vite HMR 时还会转发 WebSocket；这不是业务事件通道。

## 超时策略

| 边界 | 期限 | 到期行为 |
| --- | --- | --- |
| 浏览器普通 JSON API 请求 | 30 秒 | 中止网络请求，结束等待，展示本地化超时提示 |
| 服务端普通 JSON API 路由 | 60 秒 | 结束 HTTP handler 等待，返回结构化 504 |
| SSE 和开发代理 WebSocket | 不套用上述固定期限 | 保持长连接及既有断线恢复机制 |
| Agent 的实际 turn | 不套用上述固定期限 | 由既有 ACP 生命周期、完成/取消事件驱动 |
| 已完成过程的折叠缓存 | 连续折叠 5 分钟 | 释放缓存；展开立即取消计时 |

前端统一入口 `requestJson` 的期限覆盖等待响应头和读取完整响应体。调用方主动取消
与请求超时保留不同的错误语义，成功、失败及取消均清理计时器和事件监听。请求不会
因为超时而自动重放；手动重试得到新的完整期限。服务端中间件也覆盖 JSON 请求体
读取，SSE 与静态资源/开发代理在超时层之外。

服务端返回形如：

```json
{
  "code": "request_timeout",
  "timeoutMs": 60000,
  "operationMayContinue": true,
  "error": "Request timed out after 60 seconds. The operation may still be running; refresh its state before retrying."
}
```

`operationMayContinue` 对写操作为 true。HTTP 等待结束不会撤回已交给 Bridge 的命令，
也不会发送 `session/cancel`。写操作提示用户确认状态后再重试，不能把网络超时显示成
Agent 已结束。`POST /turns` 只等待准入回复，后续长时间执行继续由 SSE 呈现。

提交 turn 的 POST 超时后，页面保留待确认状态，发出新的权威快照读取；超时前已发出的
旧刷新不能作为结束判据。GET 预检失败时尚未提交 turn，可以直接提示失败和重试。
如果新快照确认同一 owner 的历史未变、处于 ready 且没有 active turn，则保留原提问
及手动重试入口；同历史的后续刷新也不能清掉该入口。已开始执行、历史已推进或 owner
替换时，不生成重试失败。临时对账只保存身份/版本/错误元数据，复用已有提问正文。

## 分页交互

过程页超时显示“执行过程加载超时，请点击重试。”和“重试加载”按钮。已有成功页、
最终回复及过程数量保留。重试仍请求失败页的 offset，不跳页、不自动连续重试。
在页面展开期间不会触发折叠缓存回收；收起满 5 分钟时，错误及缓存一并释放，后续
再展开从 offset 0 获取。详见 [过程按需加载](lazy-turn-process.md)。

## TDD 验收

- `tests/request-timeout.test.ts`：GET/POST/DELETE 的 30 秒边界、响应体停滞、主动取消、
  已取消请求不发送、监听/计时器清理、原有响应与错误兼容、504 映射、本地化及手动重试。
- `tests/lazy-turn-process-ui.test.tsx`：首/后续页超时提示、用户重试、成功页保留、中文文案、
  折叠缓存释放后的错误清理。
- `tests/lazy-process-hook.test.tsx`：预检超时与已提交 POST 超时的区别、待确认状态、
  新快照恢复、旧刷新隔离，不取消或自动重放已提交命令。
- `src/server/request_timeout_tests.rs`：普通读取、过程页、写操作、未完成请求体的 deadline；
  SSE 豁免、超时不额外发送取消或重复业务命令。
- `tests/browser/lazy-process.pw.ts`：真实浏览器的挂起请求在 30 秒后展示提示，已有 10 项
  保留，用户点击后重试同一页。

验证命令：`npm run typecheck`、`npm test -- --maxWorkers=2`、`npm run build:client`、
`ATTYD_SKIP_WEB_BUILD=1 cargo test`，构建服务后运行相关 Playwright 用例。

## 本次验证结果

- 新增请求封装回归 10 项，旧实现先出现 8 项预期失败，修复后全部通过；分页 UI 的
  4 项超时回归同样先失败后通过。后端 7 项新测试中 4 项先暴露缺少 deadline，随后全部通过。
- 三套 TypeScript 类型检查、前端构建和 Rust 服务构建通过。完整 Vitest 435 项通过；
  随后补完提交超时对账与原提问重试保留，相关 hook/状态四组测试 166 项通过，
  最终再次通过三套类型检查和构建。
- Rust 沙箱外全量 588 项中 585 项通过，3 项原有大消息/历史重放测试达到其 30 秒
  压力期限；在前端全量结束后，使用同一二进制串行复测这 3 项，全部通过。
  这三个用例不经过新增 HTTP 路由中间件，未放宽任何断言或测试超时。
- 最新构建的 Chrome 浏览器验收 10/10 通过，包含真实挂起 fetch、超时提示及用户重试。
- REST/SSE UI smoke、Agent HTTP/SSE 与 WebSocket 传输 smoke 通过。
- `cargo fmt --check` 与 `git diff --check` 通过。
