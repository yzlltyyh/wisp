# 安全策略 / Security Policy

简体中文 · [English](#english)

## 支持的版本

只有最新发布的版本会收到安全修复。请先升级到[最新版本](https://github.com/yzlltyyh/wisp/releases/latest)，确认问题仍然存在后再报告。

## 报告漏洞

**请不要在公开的 issue、讨论或 PR 里报告安全问题。**

请通过 GitHub Security Advisories 私下报告：打开仓库的 [Security](https://github.com/yzlltyyh/wisp/security) 页面，点「Report a vulnerability」（或直接访问[新建安全报告](https://github.com/yzlltyyh/wisp/security/advisories/new)），填写：

- 受影响的版本与平台；
- 问题的描述与可能的影响；
- 复现步骤或概念验证（如果有）；
- 你建议的修复方式（可选）。

如果页面上没有「Report a vulnerability」按钮，请开一个只写「有安全问题需要私下报告」、**不含任何细节**的 issue，维护者会与你约定私下的联系方式。

报告时请不要附上真实的 API Key、WebDAV 密码、同步密码、个人数据或剪贴板内容；需要日志时，「设置 → 关于 → 诊断信息 → 导出诊断包」导出的内容已经脱敏。

## 处理流程

- 维护者会尽快确认收到报告，评估后告知是否认定为漏洞以及预计的处理方式；
- 修复以新版本发布，Release 说明中会注明安全修复；
- 修复发布之前请不要公开细节。修复发布后，如你愿意，会在安全公告中致谢。

## 范围

属于本策略范围的：

- Wisp 应用本身（`apps/desktop`、`crates/*`）：例如 API Key 泄露到界面进程、日志或诊断包，界面进程获得超出设计的权限，剪贴板隐私规则被绕过，更新包的签名校验被绕过；
- 多设备同步：例如服务器或网络上的人不知道同步密码也能读取同步内容，或者改动、调换服务器上的文件而不被发现；WebDAV 密码或同步密钥泄露到界面进程、日志、错误信息或诊断包，或者被发往用户填写的服务器以外的地址；服务器地址、账号、设备名或同步内容出现在日志与诊断包里；
- 发布流程（`.github/workflows/`、`scripts/release/`）中可能导致发布产物被篡改的问题。

不属于范围的：你自己配置的模型服务的行为、第三方中转服务的安全性、你选择的 WebDAV 服务器自身的行为（可用性、回收站与历史版本、它能看到的文件数量、大小与时间等元数据）、对弱同步密码的离线猜测、需要攻击者已经完全控制本机账户才能实现的攻击，以及 README 与使用说明中已经说明的限制（例如 macOS 版没有经过 Apple 公证、剪贴板数据库没有加密）。

---

## English

### Supported versions

Only the latest release receives security fixes. Please upgrade to the [latest version](https://github.com/yzlltyyh/wisp/releases/latest) and confirm the problem still exists before reporting it.

### Reporting a vulnerability

**Please do not report security issues in public issues, discussions, or pull requests.**

Report them privately through GitHub Security Advisories: open the repository's [Security](https://github.com/yzlltyyh/wisp/security) tab and click **Report a vulnerability** (or go straight to [new advisory](https://github.com/yzlltyyh/wisp/security/advisories/new)). Please include:

- the affected version and platform;
- a description of the problem and its potential impact;
- steps to reproduce or a proof of concept, if you have one;
- a suggested fix (optional).

If the **Report a vulnerability** button is not available, open an issue that only says you have a security problem to report, **without any details**, and the maintainer will arrange a private channel with you.

Do not include real API keys, WebDAV passwords, sync passwords, personal data, or clipboard contents. If logs are needed, the bundle exported from **Settings → About → Diagnostics → Export diagnostics** is already redacted.

### What to expect

- The maintainer will acknowledge your report as soon as possible and, after assessing it, let you know whether it is accepted as a vulnerability and how it will be handled.
- Fixes ship in a new release, and the release notes call out security fixes.
- Please keep the details private until a fix is released. With your permission, you will be credited in the advisory.

### Scope

In scope:

- the Wisp application itself (`apps/desktop`, `crates/*`) — for example, API keys leaking into the UI process, logs, or diagnostics; the UI process gaining more privileges than intended; bypassing the clipboard privacy rules; or bypassing update signature verification;
- multi-device sync — for example, someone on the server or the network reading synced content without the sync password, or modifying or swapping files on the server without being detected; WebDAV passwords or sync keys leaking into the UI process, logs, error messages, or diagnostics, or being sent anywhere other than the server you entered; or the server address, account, device names, or synced content appearing in logs or diagnostics;
- issues in the release pipeline (`.github/workflows/`, `scripts/release/`) that could allow release artifacts to be tampered with.

Out of scope: the behavior of model services you configure yourself, the security of third-party relay services, the behavior of the WebDAV server you choose (its availability, its trash and version history, and the metadata it can see, such as the number, sizes, and times of files), offline guessing of a weak sync password, attacks that require full control of the local user account, and limitations already documented in the README and the user guide (for example, the macOS build is not notarized and the clipboard database is not encrypted).
