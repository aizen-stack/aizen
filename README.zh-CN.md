<div align="center">

<img width="200" height="200" alt="aizen" src="https://github.com/user-attachments/assets/4e38d4f9-29af-4a97-af0e-2c7dd7bdf697" />

### 真正*常驻*于你机器之上的终端原生编程智能体。

**单一静态二进制文件。没有 Node,没有 Python,没有 Docker,也不需要云账号。**

把它指向任意兼容 OpenAI 的端点,你就得到一位编程搭档:它能阅读、修改你的代码,运行你的
shell,验证自己的工作成果,还记得*你*的偏好习惯。

<br/>

[![Latest release](https://img.shields.io/github/v/release/aizen-stack/aizen?style=for-the-badge&label=release&color=6c5ce7)](https://github.com/aizen-stack/aizen/releases/latest)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-00b894?style=for-the-badge)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-e17055?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)

![Windows](https://img.shields.io/badge/Windows-0078D6?style=flat-square&logo=windows&logoColor=white)
![Linux](https://img.shields.io/badge/Linux-333?style=flat-square&logo=linux&logoColor=white)
![macOS](https://img.shields.io/badge/macOS%20(Apple%20Silicon)-000?style=flat-square&logo=apple&logoColor=white)
![Zero deps](https://img.shields.io/badge/runtime%20deps-0-brightgreen?style=flat-square)
![34 MB](https://img.shields.io/badge/binary-34%20MB-6c5ce7?style=flat-square)
![10 ms](https://img.shields.io/badge/startup-10%20ms-6c5ce7?style=flat-square)

[English](README.md) · [Tiếng Việt](README.vi.md) · **简体中文**

</div>

https://github.com/user-attachments/assets/45bbdfc8-09a3-4995-870f-eb92452743c9

---

## 安装

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/aizen-stack/aizen/main/install.ps1 | iex
```

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/aizen-stack/aizen/main/install.sh | sh
```

然后打开一个新终端:

```bash
aizen config     # base URL → API key → 选择模型
aizen            # 直接进入 REPL,开始输入
```

安装到此结束:无需环境变量,也无需手动编辑配置文件。

<sub>更愿意手动安装?从
[最新 release](https://github.com/aizen-stack/aizen/releases/latest) 下载二进制文件——或者用
`cargo install --git https://github.com/aizen-stack/aizen` 自行构建。随时可以用 `aizen update`
升级或回滚。Windows 的 `.exe` 未签名,SmartScreen 会弹出提示:*更多信息 → 仍要运行*。</sub>

## 为什么选择 Aizen

|  |  |
|---|---|
| **单文件,零运行时** | 34 MB 静态二进制,冷启动约 10 ms。没有 Node、没有 Python、没有 2 GB 的 virtualenv。在 512 MB 的 VPS、scratch 容器、CI runner 或树莓派上都能运行。 |
| **自带模型** | 任意 OpenAI 风格的 `/chat/completions` 端点——OpenAI、OpenRouter、本地 llama.cpp/vLLM、Anthropic 网关,永远不被绑定在某一家实验室。 |
| **把活干完** | 读代码、改代码、运行你的 shell——然后在宣称"完成"之前**先自我验证**:跑你的类型检查和测试,弄坏了就自己修好。 |
| **记得你** | 一套离线的、BM25 排序的记忆大脑,从复用中学习——外加 persona、持久的 SOUL 身份,以及它在真实工作后为自己编写的技能。 |
| **在你不在的地方运行** | `aizen serve` 通过 Telegram 或 Discord 驱动智能体,有风险的修改会请求你在手机上批准。可托管在 systemd、Docker 或 Kubernetes 上——即使处于 NAT 之后,也无需开放任何入站端口。 |
| **构造上即安全** | 工具被限制在工作目录内,密钥仅所有者可读且绝不打印,一道硬性命令底线即使在自动批准模式下也拒绝灾难性命令。 |

## 它能做什么

```
  aizen agent "修复失败的解析测试"

  ⚙ search_files  "fn parse_config"        3 hits
  ⚙ file_read     src/config.rs            142 lines
  ⚙ file_edit     src/config.rs            3 edits
  ⚙ shell_run     cargo test               ✓ 0 failed · 1.18s
                                           verify gate passed
```

| **统一 REPL** | 聊天与智能体循环合二为一,无需切换模式。实时 HUD:模型 · token · 轮次 · `% 上下文`。支持 Markdown、表格、图表和图片输入。 |
| **智能体循环** | 并行读取、需批准的写入、基于 LSP 的符号级编辑、子智能体调度,以及一道必须通过才能宣告"完成"的验证关卡(verify gate)。 |
| **多智能体** | `aizen workflow` 扇出多个按角色划分的子智能体,并综合出一份最终答案。 |
| **网页 + 浏览器** | 搜索、抓取,以及 katana 风格的爬虫——全部经过 SSRF 防护。可选启用(opt-in)的 CDP 工具可以驱动真实的 Chrome。 |
| **可扩展** | MCP 服务器(stdio/HTTP、OAuth 2.1)、Markdown 斜杠命令宏、出站通知通道。 |
| **可恢复** | 基于 Git 的检查点——`/timemachine` 可以回退出错的那一轮。 |

**→ [完整参考文档(英文)](docs/REFERENCE.md)**——所有命令、完整的 REPL 界面、自托管、MCP、
浏览器工具,以及安全模型的详细说明。

<img width="1920" height="1280" alt="image" src="https://github.com/user-attachments/assets/b9f7b0c1-de15-458d-bc98-437fddfbaa8b" />


## 参与贡献

欢迎提交 Issue 和 PR。本项目**没有 CLA**——贡献按 Apache-2.0 §5 授权(inbound = outbound),
我们只要求你对提交做 DCO 签名(`git commit -s`)。详见
[CONTRIBUTING.md](CONTRIBUTING.md)(英文)。

## 许可证

**[Apache License 2.0](LICENSE)**——开源,允许商业使用。请保留许可证与版权声明、注明你的
修改,并随附 [NOTICE](NOTICE) 文件。其中包含明确的专利授权(第 3 条)。

"Aizen" 与 logo 是 Aizen 作者的商标;第 6 条未授予任何商标权,因此分叉版本不得以 Aizen 的
名义呈现。v0.5.5 及之前的版本采用 PolyForm Noncommercial 许可证;之后的全部版本均为
Apache-2.0。
