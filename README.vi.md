<div align="center">

<img width="200" height="200" alt="aizen" src="https://github.com/user-attachments/assets/4e38d4f9-29af-4a97-af0e-2c7dd7bdf697" />

### Agent lập trình sinh ra cho terminal, thực sự *sống* ngay trên máy của bạn.

**Một binary tĩnh duy nhất. Không Node. Không Python. Không Docker. Không cần tài khoản cloud.**

Trỏ nó vào bất kỳ endpoint tương thích OpenAI nào, bạn sẽ có một người bạn đồng hành lập trình: đọc
và sửa code của bạn, chạy shell của bạn, tự xác minh việc của mình, và ghi nhớ *bạn* thích mọi thứ
ra sao.

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

[English](README.md) · **Tiếng Việt** · [简体中文](README.zh-CN.md)

</div>

https://github.com/user-attachments/assets/45bbdfc8-09a3-4995-870f-eb92452743c9

---

## Cài đặt

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/aizen-stack/aizen/main/install.ps1 | iex
```

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/aizen-stack/aizen/main/install.sh | sh
```

Sau đó mở một terminal mới:

```bash
aizen config     # base URL → API key → chọn model
aizen            # vào thẳng REPL và bắt đầu gõ
```

Cài đặt chỉ có vậy. Không cần biến môi trường, không phải tay sửa file config.

<sub>Muốn tự tay làm? Lấy binary từ
[bản release mới nhất](https://github.com/aizen-stack/aizen/releases/latest) — hoặc tự build bằng
`cargo install --git https://github.com/aizen-stack/aizen`. Nâng cấp hay rollback bất cứ lúc nào với
`aizen update`. File `.exe` trên Windows chưa ký số nên SmartScreen sẽ hỏi: *More info → Run anyway*
(Xem thêm → Vẫn chạy).</sub>

## Vì sao chọn Aizen

|  |  |
|---|---|
| **Một file, không cần runtime** | Binary tĩnh 34 MB, khởi động lạnh ~10 ms. Không Node, không Python, không virtualenv 2 GB. Chạy được trên VPS 512 MB, container scratch, CI runner, hay cả Raspberry Pi. |
| **Tự chọn model** | Bất kỳ endpoint `/chat/completions` chuẩn OpenAI nào — OpenAI, OpenRouter, llama.cpp/vLLM chạy local, một gateway Anthropic. Không bao giờ bị trói vào một lab duy nhất. |
| **Làm trọn việc** | Đọc, sửa, chạy shell của bạn — rồi **xác minh trước khi tuyên bố xong**: nó chạy typecheck và test của bạn, và tự sửa phần nó làm hỏng. |
| **Nó nhớ về bạn** | Một bộ não ghi nhớ offline xếp hạng bằng BM25, học từ chính việc tái sử dụng — kèm persona, danh tính SOUL bền bỉ, và những skill nó tự viết cho mình sau các công việc thật. |
| **Chạy ở nơi bạn không có mặt** | `aizen serve` điều khiển agent qua Telegram hoặc Discord và nhờ điện thoại của bạn duyệt các sửa đổi rủi ro. Host trên systemd, Docker, hay Kubernetes — nằm sau NAT, không cần mở cổng vào. |
| **An toàn ngay từ cấu trúc** | Một sandbox cấp HĐH nằm dưới lớp approval: tiến trình con không bao giờ thừa hưởng API key của bạn, network bị chặn mặc định, và chính sách filesystem được kernel cưỡng chế trên Linux (Landlock+seccomp) và macOS (Seatbelt) — Windows có Job-Object containment và được báo cáo trung thực là `partial`, không giả vờ cưỡng chế. Một lớp lệnh chặn cứng khước từ các lệnh thảm hoạ **kể cả khi đang auto-approve**. `aizen sandbox status` cho biết máy *của bạn* cưỡng chế được gì — xem [docs/SANDBOX.md](docs/SANDBOX.md). |

## Nó làm được gì

```
  aizen agent "sửa test parse đang fail"

  ⚙ search_files  "fn parse_config"        3 hits
  ⚙ file_read     src/config.rs            142 lines
  ⚙ file_edit     src/config.rs            3 edits
  ⚙ shell_run     cargo test               ✓ 0 failed · 1.18s
                                           verify gate passed
```

| **REPL hợp nhất** | Một vòng chat + agent duy nhất, không cần chuyển chế độ. HUD trực tiếp: model · token · lượt · `% context`. Markdown, bảng, sơ đồ, nhập ảnh. |
| **Vòng lặp agent** | Đọc song song, ghi phải qua phê duyệt, sửa mức ký hiệu bằng LSP, điều phối sub-agent, và một verify gate phải vượt qua trước khi được coi là "xong". |
| **Đa agent** | `aizen workflow` phóng ra các sub-agent theo vai trò rồi tổng hợp thành một câu trả lời duy nhất. |
| **Web + trình duyệt** | Tìm kiếm, fetch, và crawler kiểu katana — tất cả đều được bảo vệ SSRF. Công cụ CDP (opt-in) điều khiển Chrome thật. |
| **Mở rộng được** | MCP server (stdio/HTTP, OAuth 2.1), macro slash-command viết bằng markdown, kênh notify đi ra. |
| **Khôi phục được** | Checkpoint dựa trên git — `/timemachine` quay lại đúng lượt đi sai. |

**→ [Tài liệu tham khảo đầy đủ (tiếng Anh)](docs/REFERENCE.md)** — mọi lệnh, toàn bộ bề mặt REPL,
tự host, MCP, công cụ browser, và mô hình an toàn, nói rõ từng chi tiết.

<img width="1920" height="1280" alt="image" src="https://github.com/user-attachments/assets/b9f7b0c1-de15-458d-bc98-437fddfbaa8b" />


## Đóng góp

Issue và PR đều được chào đón. Người đóng góp cần đồng ý [CLA](CLA.md) một lần — dự án vẫn là
Apache-2.0, và §3 đồng thời cho maintainer quyền license đóng góp theo điều khoản thương mại. Hãy đọc
trước khi ký. Xem [CONTRIBUTING.md](CONTRIBUTING.md) (tiếng Anh).

## Giấy phép

**[Apache License 2.0](LICENSE)** — mã nguồn mở, cho phép sử dụng thương mại. Hãy giữ nguyên giấy
phép và các thông báo bản quyền, nêu rõ thay đổi của bạn, và chuyển kèm file [NOTICE](NOTICE). Kèm
theo khoản cấp quyền sáng chế tường minh (§3).

"Aizen" và logo là nhãn hiệu của các tác giả Aizen; §6 không cấp quyền nhãn hiệu, nên một fork không
được tự nhận mình là Aizen. Các bản phát hành đến hết v0.5.5 dùng PolyForm Noncommercial; mọi bản sau
đó là Apache-2.0.
