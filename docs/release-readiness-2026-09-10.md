# 开源发布前复查：2026-09-10

基线：`10b8130`；版本：`0.1.0`。本次结论覆盖该提交及本报告同批的修复，
不把旧审查中的历史反例重新计作当前缺陷，也不把测试通过等同于完整安全证明。

## 结论与发布门禁

产品定位、文档、许可材料和发布流程已具备首次开源的基础。本轮发现的三个边界问题
均已修复，最终本机验收全部通过，未留下已确认且未修复的发布阻塞缺陷。发布前应先提交本批修改，再确认该发布提交的
GitHub Actions 全部门禁通过，再创建 `v<SemVer>` 标签；CI 在构建时注入标签版本并精确校验二进制输出。

本机为 macOS x86_64、Rust 1.97.1、Node.js 26.8.1。GitHub Actions 的未认证读取返回
404，本环境无法确认远端工作流结果。因此，本次本机验证不能代替 CI 的 Rust 1.88、
Node.js 22 和四个 Linux 构建目标验收，也未验证 GitHub 仓库设置中的私密漏洞报告入口。

## 本轮发现与处置

| 项目 | 原行为及影响 | 修复与回归 |
| --- | --- | --- |
| 文件写入边界，P1 | 工作区内的悬空符号链接指向外部尚不存在的文件时，`canonicalize` 的 NotFound 被当作普通新文件，后续打开会跟随链接在外部创建文件。 | 新目标使用原子 `create_new`，拒绝悬空链接或检查后已被占用的路径；既有文件继续原地写入。回归先复现越界成功，再验证拒绝、不创建外部文件、错误字段，以及普通新建和内部有效链接。 |
| 终端容量竞态，P2 | 并发 `terminal/create` 在目录检查的 await 前分别通过容量检查，可突破 32 个上限。 | 容量检查、启动和登记放在共享锁的同一临界区。回归控制文件系统任务调度，稳定复现旧代码接受 33 个请求，并验证修复后只接受 32 个、释放后可再次创建。 |
| 页面嵌入边界，P2 | iframe 导航可能不携带 Origin；只检查 Host/Origin 无法阻止控制页面被外部网站嵌入。 | 静态响应增加 `Content-Security-Policy: frame-ancestors 'none'` 和 `X-Frame-Options: DENY`。HTTP 回归验证无 Origin 的主页、入口文件和深会话路由均携带两项响应头。 |

防嵌入行为依据 [W3C frame-ancestors 定义](https://www.w3.org/TR/CSP3/#directive-frame-ancestors)。
同时将兼容性表中 `session/resume` 的过时 experimental 标签移除，与
[ACP v1 Session Setup](https://agentclientprotocol.com/protocol/v1/session-setup#resuming-sessions)
及锁定的 schema 保持一致；测试专用的 `session_view` helper 限定在测试构建中。

## 工程验收

| 检查 | 结果 |
| --- | --- |
| `npm run check` | 通过：三份 TypeScript 配置、244 项 Vitest、生产前端构建。 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过。 |
| 文件系统定向测试 | 13 项通过，包含本次越界回归。 |
| 终端定向测试 | 21 项通过，包含本次并发回归及子进程生命周期。 |
| HTTP 边界定向测试 | 通过，包含防嵌入、Origin/Host、请求大小及服务退出时 Agent 进程清理。 |
| 完整 Rust 与协议集成 | 299 项 Rust 通过；REST/SSE、HTTP/SSE 与 WebSocket 远程传输、HTTP/进程边界及 ACP SDK 协议集成通过。 |
| 浏览器与覆盖率 | `npm run test:coverage` 通过：58 项 Chromium 回归通过，Rust 行覆盖率 90.59%，超过 85% 门禁；包含底部跟随、离底发送保持阅读位置及移动端交互。 |
| `cargo build --locked --release` | 通过，无编译警告。 |
| 独立二进制与完整 REST/SSE 冒烟 | 通过：18.0 MiB 发布二进制复制到空目录后可独立提供嵌入资源；使用该发布二进制的 REST/SSE 冒烟也通过，未跳过超长协议行边界。 |
| npm 依赖审计 | 0 个已知漏洞。 |
| Rust 依赖审计 | cargo-audit 0.22.2：385 个依赖，0 个漏洞、0 个公告警告；RustSec 数据库更新于 2026-09-09，提交 `b50980aad8b8f14f77e25a97b32dd94bf008b0af`。 |
| 第三方许可生成 | 本机及四个 Linux 目标均成功生成，检查未输出本机用户目录。 |
| 仓库卫生 | 扫描全部本地 Git 引用可达的 525 个历史文件对象，常见私钥/供应商令牌模式无命中；无超过 2 MB 的历史文件对象。已跟踪文件不含 Agent 二进制、依赖目录、构建输出或环境文件。 |
| 文档链接与截图 | 本地 Markdown 文件链接无缺失；README 截图为测试 Agent 的临时示例工作区。 |

终端定向套件首次在沙箱内因禁止进程检查而失败，取得本机进程检查权限后全部通过；
一次覆盖率运行在修复前被主动中止，均不作为最终通过证据。扫描仅覆盖列出的凭据模式，
不包含 GitHub 远端未同步引用、历史 Actions 日志或未跟踪的用户文件。

## 产品与协议边界

- README 保持通用 ACP Web 客户端定位；Goose 是可选后端示例，不是安装前提或兼容性裁判。
- 已确认的 T01–T08 方案在实现和文档中一致，包括无观察者回收计时、关闭确认、尽力恢复历史、
  空 `configOptions` 的排他语义、固定 `/bin/sh`、附件展示和明确文件错误。
- 仅 Agent 保存持久会话；attyd 的历史及运行状态为内存数据。恢复与 fork 使用 Agent 可提供的
  历史，其次现有缓存，缺失则明确提示，不承诺跨进程恢复。
- stdio 为标准传输；HTTP/SSE 草案与自定义 WebSocket 传输的边界已经独立说明。
  远程 Agent 不获得 attyd 本机文件和终端能力；Zed 仍只作为标准未规定之处的设计参考。
- 产品不提供应用登录、多租户隔离或完整 OS 沙箱。非本地部署必须由可信网络或代理提供鉴权。
  路径检查不承诺抵御其他本地进程并发替换父目录或已存在的目标；32 个终端记录上限也不是 Agent/MCP
  及后台子进程的全局资源配额。

上述架构边界保持不变，详见 [兼容性声明](acp-coverage.md)、
[已确定的差异处置](acp-difference-decisions.md) 和 [安全说明](../SECURITY.md)。

## 发布配置与非阻塞事项

CI 已包含依赖审计、前后端测试、协议/远程传输/HTTP 边界、浏览器、85% Rust 行覆盖率、
四个 Linux 构建与分发许可材料。Release job 等待所有门禁；版本元数据任务校验标签格式，
各目标构建注入标签版本并断言 `--version` 一致，预发布状态按 SemVer 的预发布部分判断；
归档二进制、项目许可证、第三方许可证和校验和。`package.json` 的 `private: true`
与当前源码及二进制分发方式一致。

后续版本流程调整：tag 不再要求等于 Cargo/npm 的开发版本；通过编译时 `ATTYD_BUILD_VERSION`
同时设置 CLI 与 ACP 客户端版本，manifest 和锁文件不变。普通构建仍使用 Cargo 开发版本。
本表的完整运行验收记录对应前面的边界修复；版本流程的额外验收单独记录，避免将旧结果视为新代码的全量覆盖。

版本流程额外验收通过：5 项 Python 元数据测试、10 项 Rust CLI 测试和 actionlint 1.7.12。
在同一 target 缓存中执行两次 `cargo build --locked --release`，先注入 `1.2.3-rc.1+ci-test`，
再移除注入，二进制分别输出该标签版本和 `0.1.0`；运行时环境变量不能改变已编译的版本。

- `AGENTS.md` 的 “Node.js 20+” 比 `package.json` engines 范围宽；README 已推荐 22.12+。
  本次遵守既有要求，不修改 AGENTS.md；贡献者以 engines 和 README 为准。
- 仓库管理员可在公开前确认启用 GitHub 私密漏洞报告；当前 SECURITY.md 已给出入口不可用时的处理方式。
- 本次没有创建标签、上传产物、发布 Release 或保留测试服务实例。
