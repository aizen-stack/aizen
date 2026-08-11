# Hướng dẫn sử dụng Aizen

Aizen là trợ lý lập trình chạy trong terminal. Bạn gõ yêu cầu bằng tiếng Việt (hoặc tiếng Anh), Aizen tự đọc file, viết code, chạy lệnh, kiểm thử — trong một vòng làm việc liền mạch. Tài liệu này đi qua **tất cả** tính năng, từ lúc mở lên tới các lệnh nâng cao.

> Quy ước: `❯` là dấu nhắc bạn gõ vào. `/lệnh` là lệnh gõ trong phiên chat. `aizen <lệnh>` là lệnh gõ ở terminal (ngoài phiên chat).

---

## 1. Bắt đầu

### Lần đầu chạy

```
aizen
```

Aizen mở ra giao diện chat. Nếu chưa cấu hình endpoint/key, chạy trình cài đặt:

```
aizen config
```

Trình này hỏi base URL + API key, tự lấy danh sách model của nhà cung cấp và cho bạn chọn một model. Xong là dùng được ngay.

### Gõ yêu cầu

Cứ gõ điều bạn muốn, bằng ngôn ngữ tự nhiên:

```
❯ đọc src/main.rs và giải thích luồng khởi động
❯ thêm nút đăng xuất vào trang profile
❯ sửa lỗi crash khi giỏ hàng rỗng
```

Aizen sẽ tự quyết định đọc file nào, sửa gì, chạy test ra sao. Bạn không cần chỉ định từng bước.

---

## 2. Bốn tiền tố khi gõ (dùng ngay trong ô nhập)

Đây là các ký tự đặt **đầu dòng** để đổi ý nghĩa dòng bạn gõ:

| Tiền tố | Ý nghĩa | Có gửi lượt chat không? |
|---|---|---|
| `/` | Mở bảng lệnh (gõ `/` để duyệt, hoặc gõ thẳng `/model`…) | Tùy lệnh |
| `#` | Ghi nhớ một sự thật lâu dài vào bộ nhớ | Không |
| `!` | Chạy lệnh shell ngay tại chỗ, hiện output | Không |
| `@` | Chèn nội dung một file vào tin nhắn | Có (gửi kèm) |

Ngoài ra còn hai tiền tố đặc biệt hoạt động **khi Aizen đang chạy một lượt** (xem mục 4):

| Tiền tố | Ý nghĩa |
|---|---|
| `>` | **Điều hướng** — chèn ý mới VÀO lượt đang chạy để đổi hướng |
| `?` | **Hỏi ngoài lề** — hỏi một câu KHÔNG liên quan, trả lời bên lề, không đụng công việc đang chạy |

### `#` — ghi nhớ

```
❯ #dự án này dùng pnpm chứ không phải npm
🧠 remembered
```

Sự thật được lưu vào "vùng" (zone) của dự án hiện tại. Muốn nhớ ở mọi nơi:

```
❯ #global: tôi thích code có comment tiếng Việt
```

### `!` — chạy shell tại chỗ

```
❯ !git status
❯ !ls src/
```

Chạy như terminal, hiện output ngay. Có một "sàn an toàn" chặn các lệnh cực kỳ nguy hiểm, nhưng nhìn chung nó tin bạn vì bạn gõ trực tiếp.

### `@` — đính kèm file

```
❯ tóm tắt logic trong @src/core/config.rs
❯ so sánh @a.json và @b.json
```

Nội dung file được chèn thẳng vào tin nhắn trước khi gửi cho Aizen. Bạn cũng có thể chèn output một lệnh chỉ-đọc bằng cú pháp `` !`lệnh` `` ngay trong câu:

```
❯ hiện branch của tôi là !`git branch --show-current`, hãy đặt tên PR phù hợp
```

---

## 3. Phím tắt

| Phím | Tác dụng |
|---|---|
| `Esc` | Hủy lượt đang chạy (KHÔNG thoát app) |
| `Ctrl-C` | Thoát Aizen |
| `Shift+Enter` | Xuống dòng trong ô nhập (không gửi) |
| `Alt+Enter` hoặc `Ctrl+Enter` | Điều hướng: đưa dòng đang gõ vào lượt đang chạy (giống tiền tố `>`) |
| `↑` / `↓` | Duyệt lại các dòng đã gõ trước đó |
| `Ctrl-O` | Chụp màn hình clipboard (Win+Shift+S) làm ảnh đính kèm |
| `Ctrl-X` | Bỏ ảnh đính kèm gần nhất |
| `Ctrl-L` | Vẽ lại màn hình (khi khung bị lỗi hiển thị) |

> Lưu ý: `Esc` chỉ hủy lượt, không thoát. Muốn thoát dùng `Ctrl-C` hoặc `/quit`.

---

## 4. Vừa làm vừa nói chuyện: điều hướng và hỏi ngoài lề

Đây là hai tính năng để bạn tương tác **trong lúc Aizen đang làm việc**, và chúng khác nhau về bản chất:

### `>` Điều hướng (steering) — đổi hướng công việc

Khi Aizen đang chạy mà bạn muốn bổ sung/đổi ý:

```
❯ > nhớ giữ lại API cũ luôn nhé
❯ > à, dùng tên biến userName thay vì user_name
```

Ý này được **gấp VÀO lượt đang chạy** — Aizen điều chỉnh ngay, không cần chờ nó xong rồi bắt đầu lại. (Cũng làm được bằng `Alt+Enter` / `Ctrl+Enter`.)

### `?` Hỏi ngoài lề (aside) — hỏi mà không ảnh hưởng công việc

Khi Aizen đang refactor mà bạn chợt có câu hỏi **không liên quan** tới task:

```
❯ ? borrow checker trong Rust là gì?
❯ ? cú pháp git rebase interactive ra sao?
❯ ? nó đang làm cái gì vậy?
```

Câu hỏi được trả lời **bên lề, trên một luồng riêng**, và **tuyệt đối không đụng** vào công việc đang chạy: không sửa lịch sử hội thoại, không hủy lượt, không làm chậm task chính. Câu trả lời hiện mờ với ký hiệu `⁇` để phân biệt.

**Khác nhau cốt lõi:**
- `>` = "gấp cái này VÀO việc đang làm" (đổi quỹ đạo).
- `?` = "trả lời tôi bên lề, đừng chạm vào việc đang làm".

Khi Aizen đang rảnh (không chạy lượt nào), gõ `? ...` chỉ đơn giản là một câu hỏi thường.

---

## 5. Cấu hình & model

| Lệnh | Tác dụng |
|---|---|
| `/config` | Cài đặt endpoint + key + model (trình hướng dẫn) |
| `/model` | Liệt kê model của nhà cung cấp (kèm kích thước ngữ cảnh) và chọn |
| `/effort` | Chỉnh mức "suy nghĩ": `auto · low · medium · high · xhigh · max` (có thanh trượt, hoặc gõ thẳng `/effort high`) |
| `/approval` | Mức duyệt lệnh: `ask` (hỏi mỗi lần) · `smart` (tự chạy việc chỉ-đọc) · `yolo` (cho chạy trước) |
| `/ultimate` | Bật/tắt chế độ tối đa: effort cao nhất + ưu tiên tung workflow đa-agent |

Ở terminal:
```
aizen config set --price-in <giá> --price-out <giá>   # đặt đơn giá để /cost tính tiền
aizen models                                          # liệt kê model provider quảng bá
```

---

## 6. Bộ nhớ (memory)

Aizen nhớ các sự thật lâu dài về bạn và dự án, và tự dùng lại ở các phiên sau.

| Lệnh | Tác dụng |
|---|---|
| `/memory` | Xem hồ sơ của bạn / tìm trong bộ nhớ |
| `/memory list` | Liệt kê những gì đang nhớ |
| `/memory show` | Xem chi tiết một mục |
| `/memory edit` | Sửa một mục |
| `/memory forget` | Xóa một mục |
| `/memory restore` | Khôi phục mục đã xóa |
| `/memory remember <sự thật>` | Lưu thẳng một sự thật |

Nhanh hơn: dùng tiền tố `#` (mục 2). Ở terminal có `aizen memory ...` với các subcommand tương ứng (`edit`, `forget`, `purge`…).

---

## 7. Persona & Soul (tính cách và bản sắc)

- **`/persona`** — chọn "nhân vật" mà Aizen nhập vai (liệt kê · chọn · tạo mới · xóa). Mỗi persona có bộ nhớ tự tiến hóa riêng.
- **`aizen soul`** (ở terminal) — bản sắc vận hành lâu bền tại `~/.aizen/SOUL.md`: các giá trị/nguyên tắc áp dụng cho **mọi** persona và **mọi** dự án.

---

## 8. Kỹ năng & lệnh tùy biến

- **`/skills`** — các quy trình từng-bước đã lưu để Aizen nạp khi cần (liệt kê · xem · tạo · xóa).
- **`/commands`** — lệnh slash tùy biến của riêng bạn: các macro markdown trong `~/.aizen/commands/`. Hỗ trợ `$ARGUMENTS`, `@file`, và `` !`cmd` `` trong template.

---

## 9. Hiểu codebase (index & tìm kiếm ngữ nghĩa)

```
/init                # đánh index toàn bộ codebase cho tìm kiếm ngữ nghĩa + tự truy hồi mỗi lượt
/init --force        # dựng lại từ đầu
/init --status       # xem trạng thái index
```

`/init` quét mã nguồn thành các "chunk" ngữ nghĩa, băm SHA-256 (cập nhật gia tăng), **tự động che giấu secret**, rồi dùng để tự tìm đúng đoạn code liên quan cho mỗi câu hỏi. `Esc` hủy giữa chừng an toàn (index cũ giữ nguyên).

Điều hướng theo kiểu (type-aware) bằng LSP:
```
/lsp            # bật/tắt/trạng thái/khởi động lại
/lsp status
```
Mặc định BẬT (spawn khi cần), dùng rust-analyzer · pyright · typescript-language-server. `/lsp off` để giải phóng RAM.

---

## 10. Cỗ máy thời gian (checkpoint & hoàn tác)

Aizen chụp ảnh code theo thời gian (dựa trên git), cho phép quay lại bất kỳ điểm nào — cả code lẫn nội dung chat.

| Lệnh | Tác dụng | Bí danh |
|---|---|---|
| `/checkpoint [ghi chú]` | Lưu điểm khôi phục ngay bây giờ | `/snapshot`, `/cp` |
| `/timemachine` | Duyệt mọi checkpoint (`▸` = hiện tại), chọn để nhảy về | `/timeline`, `/tm` |
| `/diff [từ] [đến] [-p]` | Xem những gì đã đổi giữa hai thời điểm | |
| `/undo` | Quay lại checkpoint trước | |
| `/redo` | Áp dụng lại checkpoint kế tiếp | |

> Mẹo: chạy `/diff` để đọc thay đổi **trước khi** `/undo`.

Ở terminal: `aizen time save · list · restore · undo · redo`.

---

## 11. Phiên làm việc (sessions)

| Lệnh | Tác dụng | Bí danh |
|---|---|---|
| `/sessions` | Các hội thoại đã lưu — khôi phục · lưu · xóa (tự lưu mỗi lượt, mới nhất lên đầu) | `/save`, `/load` |
| `/resume [tên]` | Mở lại hội thoại gần nhất CỦA DỰ ÁN NÀY (hoặc theo tên) | `/continue` |
| `/handoff <mục tiêu>` | Bắt đầu luồng mới, chỉ mang theo phần cần cho mục tiêu đó | |
| `/import` | Nối tiếp hội thoại từ CLI khác (Claude Code / Codex) | |
| `/recover [discard]` | Khôi phục phiên bị crash/kill (transcript + bản nháp chưa gửi) | `/recovery` |
| `/clear` | Bắt đầu hội thoại mới | `/new`, `/reset` |
| `/compact` | Nén các lượt cũ để giải phóng ngữ cảnh ngay | |

---

## 12. Chế độ mục tiêu (goal mode)

```
/goal <mô tả>       # chạy tới khi đạt mục tiêu, Aizen tự tuyên bố hoàn thành + tự kiểm chứng
/goal off           # dừng
```

Không giới hạn số vòng lặp, tự thử lại khi lỗi API. Aizen chỉ dừng khi tự tuyên bố xong (`goal_complete`) VÀ bước kiểm chứng đạt. `Esc` để hủy.

---

## 13. Đa-agent & phối hợp

| Lệnh | Tác dụng | Bí danh |
|---|---|---|
| `/agents` | Các sub-agent chuyên biệt bạn có thể giao việc (liệt kê · set-model) | `/agent` |
| `/workflows` | Trạng thái đa-agent trực tiếp — task/workflow con, slot sub-agent | `/wf`, `/workflow` |
| `/team` | Các cửa sổ Aizen khác đang làm trong repo này — xem file, diff, commit công việc của chúng | |
| `/work` | Worktree git tách biệt, mỗi phiên một cái (list · new · remove) | |

`/team` giúp nhiều phiên Aizen làm chung một repo mà quy được công đúng người; `/work` cô lập mỗi task vào một worktree riêng để không giẫm chân nhau.

Ở terminal: `aizen agents ...`, `aizen team ...`, `aizen work ...`, `aizen workflow ...`.

---

## 14. Kết nối app & MCP

| Lệnh | Tác dụng | Bí danh |
|---|---|---|
| `/apps` | App đã kết nối + danh mục MCP (Telegram/Discord/Slack/webhook + app đăng nhập qua trình duyệt) | `/integrations` |
| `/mcp` | Máy chủ MCP từ `~/.aizen/mcp.json` — vòng đời, sức khỏe, schema + tool |
| `/browser [doctor]` | Trạng thái profile/route trình duyệt (khi build kèm `--features browser`) |

Ở terminal:
```
aizen apps list | search <từ khóa> | add <tên> | info <tên> | login <tên> | remove <tên>
aizen mcp ...
```
`apps add` nhận các key nổi bật: `github` · `notion` · `slack` · `linear` · `spotify` · `google`, hoặc tên bất kỳ trong registry. `apps login` mở trình duyệt để đăng nhập OAuth.

---

## 15. Bot & chạy nền (Telegram / Discord)

Aizen có thể chạy như một daemon: lắng nghe Telegram/Discord, chạy agent trên tin nhắn đến, và **định tuyến các yêu cầu duyệt lệnh nguy hiểm về điện thoại bạn**.

| Lệnh chat | Tác dụng | Bí danh |
|---|---|---|
| `/telegram` | Menu tích hợp Telegram (setup · test · trạng thái · chạy daemon · tắt) | `/tg` |
| `/serve` | Chạy daemon host bot | |

Ở terminal:
```
aizen serve                       # chạy daemon
aizen serve --token <token>       # dán token & chạy luôn (owner ghép cặp trong chat)
aizen serve --install [--now]     # cài thành dịch vụ systemd (Linux) — tự khởi động lại + tự chạy khi boot
aizen serve --health              # kiểm tra daemon còn sống (dùng cho probe container/systemd)
aizen telegram ...
aizen discord setup | test | serve | show | disable
```

Trong bot Telegram/Discord có menu `/` đầy đủ: `/sh` `/cd` `/yolo` `/effort` `/model` `/addbot` `/rmbot` (thêm/bớt bot nóng)…

---

## 16. Truy cập web

```
/reach doctor       # live-probe mọi backend truy cập web, xem cái nào phục vụ nền tảng nào
/reach status       # xem cái gì đã phục vụ phiên này
```

Các tool `web_fetch` / `web_search` đi qua các kênh này. Tìm kiếm web cần API key (Tavily → Jina); chưa có key sẽ báo yêu cầu thêm key.

Ở terminal có `aizen crawl <url>` để crawl một website (BFS qua HTTP, rút link từ HTML + endpoint từ JS).

---

## 17. Theo dõi token & chi phí

| Lệnh | Tác dụng | Bí danh |
|---|---|---|
| `/tokens` | Lượng token dùng trong phiên (HUD mức đầy ngữ cảnh) | |
| `/context` | Phân tích cái gì đang lấp đầy cửa sổ ngữ cảnh (system prompt · schema tool · hội thoại theo vai) | `/ctx` |
| `/cost` | Token dùng + ước tính $ (dùng token thật khi provider báo) | `/usage` |
| `/tools` | Cấu hình bộ công cụ (toolset) | `/toolsets` |

---

## 18. Danh vị & dữ liệu dự án

| Lệnh | Tác dụng |
|---|---|
| `/where` | Bản sắc dự án hiện tại: thư mục gốc · zone slug · git · nơi lưu memory/skills/sessions · file lưu hội thoại này |

Ở terminal:
```
aizen where
aizen zone report            # báo cáo các zone (slug key cho memory/skills/index)
aizen zone migrate           # gộp các "zone song sinh" cũ (dry-run mặc định, --apply để làm thật)
```

---

## 19. Lập lịch (cron)

```
aizen cron add ...           # lập lịch task qua bộ lập lịch của HĐH (không cần daemon)
aizen cron list
aizen cron remove ...
```

---

## 20. Cập nhật Aizen

```
/update          # (hoặc `aizen update`) hiện phiên bản đang cài cạnh mọi phiên đã phát hành, chọn cái để cài (mới hơn HOẶC cũ hơn)
```

Terminal đang chạy giữ nguyên phiên bản của nó; terminal **kế tiếp** mới dùng bản vừa cài. Đây cũng là cách rollback: cứ chọn một bản cũ trong danh sách.

---

## 21. Thoát & linh tinh

| Lệnh | Tác dụng |
|---|---|
| `/help` | Hiện danh sách lệnh + mẹo | 
| `/quit` | Thoát Aizen (`/exit`, `/q`) |
| `/art` (hoặc `aizen art`) | Vẽ khung cảnh nghệ thuật braille dưới ánh trăng |

---

## 22. Bảng tra nhanh

**Tiền tố nhập liệu:** `/lệnh` · `#nhớ` · `!shell` · `@file` · `` !`cmd` `` · `>điều-hướng` · `?hỏi-ngoài-lề`

**Phím khi đang chạy:** `Esc` hủy lượt · `Ctrl-C` thoát · `Alt/Ctrl+Enter` điều hướng · `Shift+Enter` xuống dòng · `Ctrl-O` dán ảnh · `Ctrl-L` vẽ lại

**Vòng đời một task điển hình:**
1. `/init` để Aizen hiểu codebase (lần đầu).
2. Gõ yêu cầu bằng tiếng Việt.
3. Đang chạy: `>` để đổi hướng, `?` để hỏi ngoài lề, `Esc` để hủy.
4. `/diff` xem thay đổi → hài lòng thì thôi, không thì `/undo`.
5. `/checkpoint` lưu mốc an toàn trước khi thử việc mạo hiểm.

---

*Mọi thứ khác bạn gõ (không có tiền tố) đều gửi tới agent — nó vừa trò chuyện vừa dùng công cụ trong một vòng làm việc duy nhất.*
