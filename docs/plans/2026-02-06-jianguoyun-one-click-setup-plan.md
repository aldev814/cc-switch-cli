# Jianguoyun One-Click Setup for WebDAV Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 在 `cc-switch-cli` 的 WebDAV 二级菜单中提供“坚果云一键配置”，让用户仅输入账号邮箱和应用密码即可完成可用配置，并与现有 WebDAV 同步协议/上游存储格式保持兼容。  

**Architecture:** 采用“预置模板 + 最小必填输入 + 连接验证 + 失败可操作提示”的四段式流程。配置仍写入现有 `webdav_sync` 配置结构，不新增独立存储层；通过服务侧 provider 识别与状态码映射，复用现有 URL segment 拼接和逐段建目录容错逻辑，保证对真实 WebDAV 差异的稳健性。  

**Tech Stack:** Rust (`reqwest`, `url`, `serde`), Ratatui TUI, 现有 `settings`/`services::webdav_sync`/`cli::tui` 模块。

---

## 1. 背景与范围

### 1.1 用户价值（MVP）
- 在 `Config -> WebDAV` 二级菜单新增 `坚果云一键配置` 入口。
- 用户仅需输入：
  - 坚果云账号（邮箱）
  - 第三方应用密码
- 系统自动填充并保存：
  - `base_url = https://dav.jianguoyun.com/dav/`
  - `remote_root = cc-switch-sync`
  - `profile = default`
  - `enabled = true`
- 保存后自动执行一次 `check_connection`，成功即提示可用，失败给出可操作引导。

### 1.2 非目标（本期不做）
- 不做“完全零输入”自动授权（坚果云要求用户自行生成应用密码）。
- 不做后台周期任务或自动调度。
- 不做跨服务商统一向导（如 Nextcloud/ownCloud）的大而全抽象。

### 1.3 与上游互通约束
- 不修改当前 WebDAV 远端对象模型：`manifest.json` + `db.sql` + `skills.zip` + `settings.sync.json`。
- 不修改 `PROTOCOL_FORMAT` 与 `PROTOCOL_VERSION`，避免和既有/未来上游互通格式分叉。

---

## 2. 外部调研结论（用于设计约束）

> 结论日期：2026-02-06

1) 坚果云官方要求第三方 WebDAV 使用“应用密码”；并给出访问频率限制。  
2) 坚果云常见参数为 `dav.jianguoyun.com` + 路径 `/dav/`。  
3) WebDAV 标准中，`MKCOL` 对已存在资源可返回 `405`，对缺少中间目录返回 `409`；服务端不应自动创建中间层级。  
4) 行业产品常见做法是 provider profile（预填 host/path/port）+ 用户输入凭据，而不是“纯自动探测”。

对本项目的直接推论：
- “一键配置”应定义为“**一键预置 + 一步验证**”而非“免输入”。
- `MKCOL`/`PROPFIND` 容错应保留并在 UI 上转化为可执行建议（特别是坚果云目录权限与路径错误）。

---

## 3. 方案设计（优雅且实用）

### 3.1 交互设计（TUI）

WebDAV 二级菜单目标项：
- `Settings`
- `Check Connection`
- `Upload`
- `Download`
- `Jianguoyun Quick Setup`（新增）

Quick Setup 流程：
1. 进入向导页（或 Editor 模板页），展示将自动填充的参数。  
2. 依次输入 `username(email)` 与 `app_password`。  
3. （可选）高级项折叠：`remote_root`、`profile`。默认不展开。  
4. 确认保存后：
   - 写入 `webdav_sync`
   - 立刻调用 `WebDavSyncService::check_connection()`
5. 根据结果 toast：
   - 成功：`已完成坚果云配置并通过连接测试`
   - 失败：带动作建议（去生成应用密码 / 修正 `/dav/` / 手动创建上级目录）

### 3.2 配置模型设计

保持现有 `WebDavSyncSettings` 主体不破坏，增量建议：
- 新增可选字段（推荐）：`provider_hint: Option<String>`，值如 `jianguoyun`。
  - 目的：避免从 `base_url` 反推 provider 时出现误判，便于将来扩展更多预置服务。
  - 向后兼容：无此字段时回退现有 `base_url host contains` 判断。

若坚持最小改动，也可不加字段，仅保留现有 `is_jianguoyun(base_url)`。

### 3.3 错误体验设计（坚果云特化）

针对常见错误码给统一建议模板：
- `401/403`：提示“请使用第三方应用密码，不是登录密码”。
- `404/30x`：提示“确认 base_url 位于 `/dav/` 可写目录”。
- `409`（MKCOL）：提示“先手动创建上级目录再重试”。
- `405`（MKCOL）：提示“目录可能已存在，可忽略并继续校验”。

所有提示应统一走 i18n，避免硬编码散落。

---

## 4. TDD 实施任务拆解（可直接执行）

### Task 1: 增加“坚果云预置”配置能力

**Files:**
- Modify: `src-tauri/src/settings.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/tests/webdav_settings.rs`

**Step 1: 写失败测试（预置生成）**
- 新增测试：给定空配置时，应用“坚果云预置”后应得到固定 `base_url/remote_root/profile/enabled`。

**Step 2: 运行单测验证失败**
- Run: `cd src-tauri && cargo test webdav_settings -- --nocapture`
- Expected: FAIL（缺少预置 helper 或字段）

**Step 3: 最小实现**
- 在 `settings` 或相邻服务层提供 `apply_jianguoyun_preset(...)` helper（或等价接口）。
- 确保 `normalize/validate` 后结果稳定。

**Step 4: 回跑测试**
- Run: `cd src-tauri && cargo test webdav_settings -- --nocapture`
- Expected: PASS

**Step 5: Commit**
```bash
git add src-tauri/src/settings.rs src-tauri/src/lib.rs src-tauri/tests/webdav_settings.rs
git commit -m "feat(webdav): add jianguoyun preset helper"
```

---

### Task 2: 在 WebDAV 二级菜单接入 Quick Setup

**Files:**
- Modify: `src-tauri/src/cli/tui/app.rs`
- Modify: `src-tauri/src/cli/tui/ui.rs`
- Modify: `src-tauri/src/cli/tui/mod.rs`
- Modify: `src-tauri/src/cli/i18n.rs`
- Test: `src-tauri/src/cli/tui/app.rs`（已有单元测试区域）

**Step 1: 写失败测试（菜单与动作映射）**
- 新增/更新测试：
  - WebDAV 子菜单包含 Quick Setup 条目。
  - Enter 后触发对应 Action。

**Step 2: 运行目标测试**
- Run: `cd src-tauri && cargo test config_webdav -- --nocapture`
- Expected: FAIL（新菜单项未接入）

**Step 3: 最小实现 UI 与 Action**
- 扩展 `WebDavConfigItem` 枚举与 `ALL`。
- 在 `webdav_config_item_label` 增加 i18n 文案键。
- 在 `on_config_webdav_key` 与 action handler 接入 Quick Setup 逻辑。

**Step 4: 回跑目标测试**
- Run: `cd src-tauri && cargo test config_webdav -- --nocapture`
- Expected: PASS

**Step 5: Commit**
```bash
git add src-tauri/src/cli/tui/app.rs src-tauri/src/cli/tui/ui.rs src-tauri/src/cli/tui/mod.rs src-tauri/src/cli/i18n.rs
git commit -m "feat(tui): add jianguoyun quick setup entry"
```

---

### Task 3: 保存后自动连接验证与错误提示增强

**Files:**
- Modify: `src-tauri/src/services/webdav_sync.rs`
- Modify: `src-tauri/src/cli/tui/mod.rs`
- Modify: `src-tauri/src/cli/i18n.rs`
- Test: `src-tauri/src/services/webdav_sync.rs`（单元测试）

**Step 1: 写失败测试（错误提示映射）**
- 覆盖以下场景文本断言：
  - 坚果云 `401/403` -> 应用密码提示
  - 坚果云 `404/3xx` -> `/dav/` 路径提示
  - `MKCOL 409/405` -> 上级目录/目录已存在提示

**Step 2: 运行目标测试**
- Run: `cd src-tauri && cargo test webdav_sync -- --nocapture`
- Expected: FAIL（映射不完整或文案不一致）

**Step 3: 最小实现**
- 复用现有 `webdav_status_error`/`is_jianguoyun` 路径，补齐一致性文案与 i18n key。
- Quick Setup 保存成功后触发 `check_connection`，并根据结果显示 toast。

**Step 4: 回跑目标测试**
- Run: `cd src-tauri && cargo test webdav_sync -- --nocapture`
- Expected: PASS

**Step 5: Commit**
```bash
git add src-tauri/src/services/webdav_sync.rs src-tauri/src/cli/tui/mod.rs src-tauri/src/cli/i18n.rs
git commit -m "feat(webdav): validate quick setup and improve jianguoyun hints"
```

---

### Task 4: 文档与验收

**Files:**
- Modify: `README.md`（如包含 WebDAV 章节）
- Modify: `docs/` 下相关说明文档（可新建 `docs/webdav-sync.md`）

**Step 1: 更新用户文档**
- 增加“坚果云一键配置”使用说明与常见错误排查。

**Step 2: 全量验证**
- Run: `cd src-tauri && cargo fmt`
- Run: `cd src-tauri && cargo clippy --all-targets -- -D warnings`
- Run: `cd src-tauri && cargo test`
- Expected: 全部通过

**Step 3: Commit**
```bash
git add README.md docs src-tauri
git commit -m "docs(webdav): add jianguoyun quick setup guide"
```

---

## 5. 严格 Code Review Gate（完成后必须执行）

### 5.1 Review 清单
- 架构：是否避免在 TUI 层硬编码业务规则（规则应在 service/settings）。
- 兼容：旧 `webdav_sync` JSON 是否可无损读取。
- i18n：是否无裸字符串泄漏到 UI。
- 容错：`push_segments + pop_if_empty()` 构建 URL 是否仍覆盖所有上传/下载路径。
- 协议：是否未改动 manifest 协议字段，确保与既有远端可互通。
- 安全：日志/错误里是否避免打印明文密码。

### 5.2 建议命令
```bash
cd src-tauri
cargo test webdav_settings -- --nocapture
cargo test webdav_sync -- --nocapture
cargo test config_webdav -- --nocapture
cargo clippy --all-targets -- -D warnings
cargo test
```

---

## 6. 发布与回滚策略

### 6.1 渐进发布
- 默认仅新增“坚果云一键配置”入口，不改变已有 `Settings` 手工模式。
- 若出现兼容问题，用户可继续使用原手工配置路径。

### 6.2 回滚
- UI 回滚：隐藏 Quick Setup 菜单项。
- 逻辑回滚：保留保存后的配置结构，禁用自动 `check_connection` 即可退化到旧行为。
- 数据回滚：`webdav_sync` 字段保持向后兼容，无需迁移脚本。

---

## 7. 验收标准（Definition of Done）

- 用户能在 WebDAV 二级菜单中看到并使用 `Jianguoyun Quick Setup`。
- 仅输入邮箱 + 应用密码即可完成可用配置并通过连接校验（在网络与权限正常前提下）。
- 常见失败提示具备明确可执行指引，不出现“未知错误”黑箱体验。
- 全部测试、格式化、lint 通过。
- 与现有 WebDAV 同步格式兼容，不破坏已有远端数据读取。

---

## 8. 参考资料（调研来源）

- 坚果云官方：第三方应用授权 WebDAV 开启方法（应用密码、请求限制）  
  https://help.jianguoyun.com/?p=2064
- 坚果云官方（FolderSync 示例，含 `dav.jianguoyun.com` + `/dav/`）  
  https://help.jianguoyun.com/?cat=71
- RFC 4918（WebDAV 标准，MKCOL 405/409 语义）  
  https://www.rfc-editor.org/rfc/rfc4918
- rclone WebDAV 文档（provider/vendor 预设实践，含 Nutstore）  
  https://rclone.org/webdav/
- Cyberduck Connection Profiles（预配置 profile 设计范式）  
  https://docs.cyberduck.io/protocols/profiles/  
  https://docs.cyberduck.io/protocols/webdav/providers/

---

Plan complete and saved to `docs/plans/2026-02-06-jianguoyun-one-click-setup-plan.md`.
