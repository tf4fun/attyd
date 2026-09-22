# 已完成轮次的过程按需加载

日期：2026-09-22。

## 范围与数据职责

仅已完成轮次采用按需加载。打开会话时，历史轮次传输提问、最后一条完整回复、
结果状态和过程数量；运行中轮次继续传输完整当前状态并消费实时更新。
回复的多个内容块保留，不能只截取最后一个文本片段。

ACP Agent 仍是持久化权威，Bridge 保留当前加载历史及运行中状态，浏览器展示其中
已请求的部分。本功能改变 HTTP 展示投影和浏览器渲染量，不删除 Agent 历史，也不
重新引入原始事件版本档案。沿用 [运行期内存优化](runtime-memory-efficiency.md)。

## 接口与一致性

会话 GET 增加 `presentation=compact`；不传或传 `full` 保持完整视图，供显式导出
和既有 API 调用者使用。创建、分叉响应中的会话视图也使用精简展示。

精简视图附带 `collapsedTurns`，每项包含：

- `turnId`、`historyRevision`：当前历史版本中的轮次身份。
- `beforeUpdate`、`afterUpdate`：可见 timeline 的区间。
- `processCount`：折叠后逻辑过程项的数量，不是 ACP 通知数。
- `outcomes`：该轮次的结束结果，避免空轮次在相同偏移处混淆。
- `processIncluded`：仅对当前浏览器已实时观察区间返回完整过程时为 true。
- `operationId`：可用时提供稳定的完成操作身份；`visibleRanges` 指出提问与最终回复
  在当前原始 timeline 中的完整区间，供浏览器到期释放正文使用。

浏览器记录当前 owner 下首个已完整观察的运行中 operationId；后续精简刷新携带
`includeProcessFrom`，服务端从该操作对应轮次起返回完整的权威历史投影，之前历史仍
按需加载。此参数要求完整 owner fencing，初次打开不会携带，切换会话或 owner 后清除。
若 Agent 重新加载的历史已无此操作锚点，退回普通精简视图。这样，用户阅读运行过程
时不会因完成刷新而被强制折叠，也不依靠保留旧正文来替代 Agent 的最新历史。

过程分页入口为
`GET /api/v1/sessions/:sessionId/turns/:turnId/process`，要求携带
`expectedEpoch`、`expectedIncarnation`、`historyRevision` 和 `offset`。
每页最多 10 个逻辑项，offset 从 0 开始按 10 递增。工具的连续状态更新合并为
当前完整工具，消息按身份和角色聚合，计划和压缩记录保留其当前业务状态。
响应返回 owner、版本、offset、total、nextOffset、items、所需终端快照和轮次结果。
最后一页 `nextOffset=null`。

分页只查询仍有效的 owner，禁止因旧分页请求重新加载退休会话。历史版本变化返回
409，浏览器刷新精简视图；导航、owner 或版本变化后的旧响应不得并入新页面。
精简视图不携带仅由隐藏历史过程引用的终端正文；运行中资源继续保留，过程页按引用
附带其终端快照。相关响应使用 `Cache-Control: no-store`。

## 浏览器行为

通过链接或会话列表打开历史时，页面显示“正在加载会话”，直到首份会话视图到达；
等待期间不展示项目列表或上一会话的消息，也不显示可发送的输入框。
加载阶段采用骨架屏，复用会话页标题栏、消息列、折叠过程槽和输入区的宽度与边距，
减少内容到达时的布局变化。占位内容对读屏隐藏且不可交互；标题提供加载状态说明。
沿用主题颜色与字号，支持中英文、手机布局及减少动态效果设置，不显示虚假进度。
请求超时或失败后移除骨架占位，提供错误说明、手动重新加载和返回项目的入口。
Agent 要求认证时优先展示原有认证界面，登录成功后继续恢复会话。
目录发现失败不应误报连接断开；离开页面后迟到的响应不能恢复旧页面。
已打开会话的后台刷新及运行中实时消息不触发该过渡。
验收覆盖 `tests/lazy-process-hook.test.tsx` 与 `tests/browser/session-loading.pw.ts`。

未展开时不请求过程页。首次展开加载 10 项，用户点击“再加载 10 条”才继续请求；
请求失败可重试，加载中禁用重复请求。收起后短期保留本轮已加载页，重新展开无需重拉。
普通请求采用独立的 30 秒前端期限，超时明确提示并等待用户重试；这与折叠内容的
5 分钟保留时间无关。后端另有 60 秒兜底，详见 [HTTP 请求超时](http-request-timeouts.md)。
会话 owner 或历史版本变化时清除对应页缓存并取消在途请求。
已完整实时观察的轮次用新权威正文重建并复用对应展示身份，保留当前阅读位置和折叠
选择；用户回到底部后继续沿用原有自动折叠规则。重新打开页面时历史统一按需加载。

分页沿用实时过程的视觉层级：条目间距为 18px，相邻回复与思考保留同组的 9px 间距，
页边界不拆组；继续加载时保留已展开思考块的展示身份。底部分页使用统一的辅助字号、
次要计数和轻量操作按钮，加载与超时提示在手机及明暗主题下不得重叠或横向溢出。

搜索只索引已加载且可见的内容，不为搜索隐式下载全部过程。Markdown 导出显式请求
完整快照，包含未展开的过程；导出响应不替换界面状态，切换会话后丢弃旧导出结果。

任务栏默认展开，进度数量紧邻左侧标题，右侧使用与其他卡片一致的折叠按钮。
同一计划更新保持用户的折叠选择，新计划恢复展开；提供可访问的按钮名称和展开状态。

### 折叠内容的延迟释放

已完成轮次的过程连续折叠 **5 分钟** 后释放；展开会取消计时，再次折叠重新计时。
运行中轮次、展开的过程，以及为保留阅读位置而尚未折叠的过程均不回收。普通重渲染、
迟到分页响应或其他轮次完成引发的历史版本更新，不得延后原有折叠期限。

- 已分页加载的过程：取消请求，清除页缓存、派生正文、变更预览和页内终端快照，
  重置错误/加载状态；迟到响应不得恢复缓存。再展开从 offset 0 加载前 10 条。
- 已完整实时观察的完成轮：同时清理渲染 timeline 和 hook 保存的原始会话快照，
  只留下提问、完整最终回复、结果状态及分页描述符，随后转入同一按需加载路径。
- 只清除失去引用的已结束/释放终端快照，包括 `outputBytes`；其他可见条目、权限、
  运行中轮次仍引用的终端和仍运行的终端继续保留。
- 离开会话时，缓存和回滚快照直接精简具有权威描述符的已完成轮次，防止组件卸载
  取消计时后将过程永久留在后台缓存；运行内容保持。

完整保留轮次的描述符附带 `operationId` 和 `visibleRanges`，后者指出原始 timeline
中提问/最终回复的完整区间，前端无需自行推测 ACP 消息分组。精简 GET 的
`excludeProcessFor=<JSON 操作编号数组>` 在完整 owner 校验下优先于 `includeProcessFrom`，
允许单独释放中间一轮而保留仍展开的后续轮次。前端还按当前释放集合过滤所有会话
视图入口，防止释放前发出的在途刷新或 prompt 预检响应重新保存过程正文。
释放集合按 owner/navigation 隔离；SSE 长期回调仅捕获轻量身份，不捕获完整快照。
若 prompt 预检先更新原始快照版本，而界面尚未重建，释放仍按稳定 operationId 找到
同一轮次；界面动作继续校验其当前描述符，不能因两个投影暂时版本不同而漏掉释放。

这里释放的是应用层引用和隐藏组件，不删除 Agent/Bridge 权威历史；实际堆内存回收
由浏览器 GC 决定。Markdown 导出仍显式获取完整快照，不将正文重新灌回页面。

### Zed 的参考

2026-09-22 查看官方源码后确认：Zed 的 [GPUI list](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/list.rs)
按可见范围处理长列表；[折叠状态](https://github.com/zed-industries/zed/blob/main/crates/agent_ui/src/entry_view_state.rs)
控制正文 UI 是否构建，不能据此认为线程模型正文也已释放；
[线程缓存](https://github.com/zed-industries/zed/blob/main/crates/agent_ui/src/agent_panel.rs)
对可重新加载且 Idle 的线程按更新时间淘汰，默认保留数量为 5。
检查的这些路径中未发现折叠 5 分钟后释放正文并分页重取的机制。
本次采用独立的延迟释放策略；列表虚拟化可作为后续渲染优化，不包含在本次实现中。

## 验收测试

| 层次 | 入口 | 要求 |
| --- | --- | --- |
| Rust 投影 | `src/session_presentation.rs` | 隐藏大正文不进入精简响应，最终回复完整，逻辑实体合并，终端按需附带，分页和历史版本校验 |
| HTTP 路由 | `src/server/session_presentation_tests.rs` | 真实路由精简视图和分页契约，owner/历史过期拒绝，完整视图兼容 |
| 状态重建 | `tests/lazy-process-state.test.ts` | 明确轮次边界、空轮次、多块回复、实时消息身份隔离、取消工具、权威过程更新与展示身份保留 |
| 请求生命周期 | `tests/lazy-process-hook.test.tsx` | 精简请求、分页校验、迟到响应和409恢复、完整导出隔离、释放后的旧响应过滤、预检版本先行时仍能释放 |
| 延迟释放 | `tests/collapsed-process-retention-ui.test.tsx`、`tests/process-retention.test.ts` | 5分钟边界、短期复用、阅读/运行保护、两层正文引用清理、终端保活、离开会话缓存精简 |
| React | `tests/lazy-turn-process-ui.test.tsx`、`tests/plan-card.test.tsx` | 展开触发、10条增量、缓存和重试、失效清理、任务栏折叠与键盘操作 |
| 浏览器 | `tests/browser/lazy-process.pw.ts`、`tests/browser/turn-collapse.pw.ts` | 真实 Agent/Bridge 响应不含隐藏正文、25项按10/10/5加载、手机布局、完成后保留阅读位置、折叠5分钟清除正文并从首10条重新获取 |

执行 `npm run check`、`npm run test:rust`，构建后运行
`npx playwright test tests/browser/lazy-process.pw.ts tests/browser/turn-collapse.pw.ts`。

## 性能边界

本次减少初次网络传输、前端状态体积和 DOM 渲染。分页单位是逻辑项，不是字节；单个
合法大工具结果仍可形成大页面。Bridge 当前会话读取链路仍会构造完整内部业务视图，
精简投影随后筛选可见正文；因此不能把传输缩小理解为服务端峰值内存或 CPU 已按页
缩小。后续可将投影前移至共享历史快照，复用版本化索引，避免每页遍历完整历史。

## 按需加载与任务栏阶段验证

- TypeScript 三套类型检查、前端构建通过；完整 Vitest 397 项通过。
  默认并发与 Rust 编译同时运行曾出现 7 项超时，降低为 `--maxWorkers=2` 后全量通过。
- Rust 完整回归 575 项通过；随后新增已观察区间协议的 3 项测试，最终
  `cargo test session_presentation -- --test-threads=1` 的 15 项全部通过。
- 原有轮次折叠及新增分页浏览器测试共 7 项通过，包含距底部 24px/500px 阅读位置、
  重载、搜索、复制，以及手机明暗主题。手机任务栏折叠释放超过 100px 高度，无横向溢出。
- REST/SSE UI smoke、HTTP/SSE/WebSocket 远程传输 smoke 通过。
- `cargo fmt --check` 与 `git diff --check` 通过。

浏览器测试曾复现完成后强制折叠，随后通过 `includeProcessFrom` 与展示身份保留修复，
原断言保持不变。新增状态回归也验证复用消息、工具、计划、compaction ID 的实时更新
不会改写已完成的历史，以及服务端新正文能够替换旧正文。

## 五分钟释放阶段验证

- 先增加失败回归，再实现释放逻辑；覆盖分页缓存、完整实时观察轮次、原始会话快照、
  离开会话缓存、终端引用、异步响应和计时边界。
- TypeScript 三套类型检查、前端构建通过；完整 Vitest 416 项通过
  （`npm test -- --maxWorkers=2`）。最终新增预检版本先行的竞态回归先失败，修复后
  `lazy-process-hook`、`use-acp`、`process-retention` 三套相关测试共 87 项通过，
  并重新通过应用类型检查及前端构建。
- `cargo test session_presentation -- --test-threads=1` 的 18 项通过，覆盖可见区间、
  稳定操作身份、单轮排除优先级、后续轮次保留和 owner 校验。
- Chrome 浏览器验收共 9 项通过，包含分页缓存及完整实时观察轮次的 5 分钟释放；
  提问、最终回复、结果状态和过程数量保留，再展开请求 offset 0 的前 10 项。
  最终竞态修复后重新构建服务，并以 `--grep releases` 重跑两项释放验收，全部通过。
- REST/SSE UI smoke、`cargo fmt --check`、`git diff --check` 通过。

这些测试验证引用和 DOM 清理及重取行为，未测量浏览器 GC 后的实际堆内存或 RSS。
