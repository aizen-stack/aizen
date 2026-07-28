# Kế hoạch triển khai — Thiết kế lại bộ nhớ (tier/anchor + thư ký cuối lượt)

> Ngày lập: 2026-07-27 · Repo: `C:\Users\admin\Desktop\mini_project\aizen` · Nhánh làm việc: `feature/memory-tiers` (tách từ `main`, **KHÔNG** chồng lên `release/v0.4.9` đang soak).
> Bản này hợp nhất: bản ghi quyết định (PART A–D), đặc tả thiết kế PART B, và 4 lăng kính phản biện đối kháng (mất dữ liệu · chi phí/độ trễ · đúng-sai/độ phức tạp · cắt bớt/khả thi).
> Trạng thái: **15/15 vấn đề P0 đã được xử lý trong plan** (sửa thiết kế hoặc cắt hẳn phần sinh ra nó). Danh mục đối chiếu ở §6.

---

## Mục lục

1. [Tóm tắt điều hành](#1-tóm-tắt-điều-hành)
2. [Chẩn đoán (số liệu đo được)](#2-chẩn-đoán-số-liệu-đo-được)
3. [Kiến trúc mục tiêu](#3-kiến-trúc-mục-tiêu)
4. [Kế hoạch theo phase](#4-kế-hoạch-theo-phase)
   - [Phase 0 — Cầm máu + nền](#phase-0--cầm-máu--nền-bắt-buộc-ngày-1)
   - [Phase 1 — Trục tier/anchor/lineage](#phase-1--trục-tieranchorlineage)
   - [Phase 2 — Đọc tự động (recall block)](#phase-2--đọc-tự-động-recall-block)
   - [Phase 3 — Thư ký cuối lượt + M2a + M1](#phase-3--thư-ký-cuối-lượt--m2a--m1)
   - [Phase 4 — M2b hoà giải theo lô + un-supersede + doctor](#phase-4--m2b-hoà-giải-theo-lô--un-supersede--doctor)
   - [Phase 5 — Bench, hiệu chỉnh hằng số, quyết định mở rộng](#phase-5--bench-hiệu-chỉnh-hằng-số-quyết-định-mở-rộng)
5. [Phase 0 — thủ tục chính xác](#5-phase-0--thủ-tục-chính-xác-lệnh-cụ-thể)
6. [Sổ rủi ro xếp hạng](#6-sổ-rủi-ro-xếp-hạng--đối-chiếu-p0)
7. [Bảng hằng số & công thức](#7-bảng-hằng-số--công-thức)
8. [Cách đo thành công](#8-cách-đo-thành-công-b10)
9. [Cố ý KHÔNG làm trong plan này](#9-cố-ý-không-làm-trong-plan-này)
10. [Phiên bản tối thiểu khả dụng (30% công sức)](#10-phiên-bản-tối-thiểu-khả-dụng-30-công-sức)

---

## 1. Tóm tắt điều hành

**Vấn đề.** Kho bộ nhớ bền của aizen **trống rỗng** (0 entry) sau nhiều tháng dùng thật, trong khi hai trụ còn lại (persona 40/40, 18 skill) thì đầy và đang **xoá cứng dữ liệu mỗi lượt**. Nguyên nhân không phải một mà bốn, độc lập nhau: extractor chỉ hiểu tiếng Anh; đường `Inferred` bị đẩy vào RAM trước khi tới store; lane always-on không thể với tới bằng cách dùng bình thường; và cách phân vùng (slug băm theo `project_root`) phong cả `C:\Windows\System32` thành một "dự án".

**Hướng giải.** Thay trục phân vùng bằng **hai trục**: `tier` (user/device/place — *cái này nói về cái gì*) và `anchor` (một đường dẫn tuyệt đối, khớp theo tiền tố, kế thừa xuống cây — *cái này áp dụng ở đâu*). Thay 2 lời gọi model cuối lượt + 1 regex chết bằng **một "thư ký cuối lượt"** duy nhất. Bơm bộ nhớ vào lượt user theo đúng khuôn `fold_retrieval_into_query` (không đụng lane hệ thống, prefix cache giữ nguyên). Quên bằng **cường độ** (`strength`) thay vì ghế ngồi, và hoà giải mâu thuẫn **cục bộ + theo lô** thay vì để model tự phán trên dữ liệu nó chưa thấy.

**Cắt.** Bỏ hẳn khỏi phạm vi: M4 "ngủ" hợp nhất, graph đa trụ (`f:`/`s:`/`i:`), journal crash-safety, nửa âm của M3 (demerit/pendingSince/reaper), `anchorLabel`, `originDevice`, `aizen skill retier`. Lý do chung: chúng cộng ~60% khối lượng và ~90% bề mặt rủi ro mất dữ liệu, đổi lấy 0 hành vi quan sát được trong 3–6 tháng tới (store có 0 fact).

**Kết quả kỳ vọng đo được (sau 4 tuần dùng thật):** kho có ≥60 fact sống, ≥1/3 mang tier `device` hoặc `place` neo đúng cây; tỉ lệ *bơm-và-thật-sự-dùng* ≥0.35 và tăng; số dòng fact tăng **chậm hơn** số lượt học (dấu hiệu đang sửa chứ không tích); 0 file persona bị xoá cứng.

---

## 2. Chẩn đoán (số liệu đo được)

Toàn bộ số liệu dưới đây từ PART A của bản ghi quyết định (đo trên máy thật), đã đối chiếu lại với code trong lần lập kế hoạch này.

| Hạng mục | Số đo | Ý nghĩa |
|---|---|---|
| `cli-memory/entries/*.md` | **0 file** | Kho bền chưa từng nhận một fact nào |
| `cli-memory/core/active/*.md` | **9 file, mỗi file 0 byte** | Lane always-on rỗng ở cả 9 "zone" |
| `STYLE.md`, `review/`, `archive/`, `graph.tsv` | **chưa từng tồn tại** | Ba cơ chế được thiết kế nhưng chưa từng chạy |
| `personas/kira.self/` | **40 episode + 40 insight = đúng cap** | Bão hoà → **đang evict**, và evict = `fs::remove_file` |
| `~/.aizen/skills/` | **18 skill** (5 global + 13 trong `skills/p/admin-e4df4c78/`) | 13 skill không liên quan nhau bị hợp nhất chỉ vì cwd tình cờ là `C:\Users\admin` |
| skill `uses` lớn nhất | **3**; **0 thư mục `.archive`** | `refine` **chưa từng chạy** |
| Zone quan sát được | `C:\Users\admin` → zone; `C:\Windows\System32` → **zone khác**; `C:\Users\admin` có **HAI** zone (`admin-5296147b`, `admin-e4df4c78`) | Một thư mục, hai danh tính — slug fork theo PATH-luck |
| Chi phí cuối lượt hôm nay | 1 call trả tiền (skill, cổng ≥4 tool call ‖ recovered) + 1 call theo chu kỳ (persona reflection) + 1 đường regex chết | "2 call" trong bản ghi là ước lượng lạc quan; call persona chỉ chạy khi `should_reflect` |

**Bốn nguyên nhân độc lập (phải sửa cả bốn, sửa ba vẫn ra 0 fact):**

1. **Cổng nói tiếng Việt, extractor thì không.** `learning/signals.rs` có `ghi nhớ|không phải|sai rồi|tôi (thích|muốn)|luôn luôn|thường dùng`; `learning/extract_free.rs` gần như thuần Anh. Cổng mở, extractor trả rỗng.
2. **Đường `Inferred` không bao giờ tới đĩa.** `learning/mod.rs:176` đẩy **mọi** candidate `Inferred` vào `session_mem` (RAM) trước khi tới nhánh Store/Review. Sửa xong extractor mà quên dòng này thì store **vẫn rỗng**.
3. **Lane always-on không thể với tới.** `frozen_core` chỉ nhận `mtype == User` + global + core-trusted; nhưng `#remember` ghi `Feedback`, `memory_save` ghi `Project`, còn STYLE.md chỉ được ghi trong nhánh core-promotion mà REPL luôn từ chối.
4. **Phân vùng sai bản chất.** `project_root()` có `unwrap_or_else(|| cwd)` ở cuối ⇒ mọi thư mục đều có thể lên ngôi "dự án". Không có tầng `device` — thứ cần nhất cho một agent chạy trên cả máy ("máy này không có `gcc`", "git nằm ở `C:\Program Files\Git\cmd`").

**Một lỗi mất dữ liệu ĐANG CHẠY, mỗi lượt formative:** `persona/self_mem.rs:544-557` — `prune_kind` sắp theo `(importance desc, mtime desc)`, giữ `cap` đầu, phần còn lại `fs::remove_file`. `prune()` chạy sau **mỗi** `record_episode` và `save_insight`. Kho đang 40/40 ⇒ mỗi lượt formative xoá vĩnh viễn một file, không archive, không backup. **Đây là việc phải sửa trong ngày đầu tiên, trước mọi thứ khác.**

**Một lỗi mất dữ liệu ĐANG CHỜ:** `memory/mod.rs:948 cmd_review` — `--clear` là `remove_dir_all` (xoá cứng cả hàng đợi), `--promote` dựng lại `LearnedWrite` field-by-field (chỉ chép `scope`/`subpath`). Đó là **cái bẫy thứ tư** của họ hàm `render_learned` mà đặc tả PART B chỉ liệt kê ba.

---

## 3. Kiến trúc mục tiêu

### 3.1 Sơ đồ chữ

```
                      ┌─────────────── MỘT KHO VẬT LÝ ───────────────┐
                      │  ~/.aizen/cli-memory/entries/*.md            │
                      │  (logic phân tán, vật lý tập trung — B2)     │
                      └──────────────────────────────────────────────┘
                                        │
        ┌───────────────────────────────┴───────────────────────────────┐
        │ TRỤC A: tier — "cái này NÓI VỀ cái gì"                        │
        │   user    → đúng ở mọi thư mục, mọi máy   (về con người)      │
        │   device  → đúng chỉ trên máy này         (khoá lọc: device:) │
        │   place   → đúng ở đây và bên dưới        (khoá: anchor:)     │
        └───────────────────────────────────────────────────────────────┘
        ┌───────────────────────────────────────────────────────────────┐
        │ TRỤC B: anchor — "cái này ÁP DỤNG Ở ĐÂU" (path tuyệt đối)     │
        │   c:/users/admin                                              │
        │   └── c:/users/admin/desktop/mini_project                     │
        │       └── c:/users/admin/desktop/mini_project/aizen  ← cwd    │
        │   Khớp TIỀN TỐ theo segment. Tổ tiên GẦN NHẤT thắng.          │
        └───────────────────────────────────────────────────────────────┘

  ĐỌC (mỗi lượt, 0 model call)                GHI (cuối lượt, 1 model call có cổng)
  ─────────────────────────                   ────────────────────────────────────
  cwd → Lineage::current()                    cổng FREE (signal ‖ ≥4 tool ‖ recovered)
    → MemView::Here (ĐIỂM CHẶN DUY NHẤT)         → thư ký đọc lượt GỐC (không có block bơm)
    → search BM25 · strength, k=12                → JSON {facts[], episode?, skill?, used[]}
    → cổng liên quan + delta so lượt trước        → add_scoped = ĐIỂM CHẶN GHI DUY NHẤT
    → block ≤300 token gấp vào LƯỢT USER            (clamp_anchor + threat_scan + tiering)
    → PendingLedger (RAM) handle→id                → M2a cục bộ MIỄN PHÍ: same / new / review
                                                   → used[] → confirmations += 1 → nửa đời dài ra
                                                 (M2b theo lô, ≤1 call/phiên, quyết refine/contradict)

  KHÔNG ĐỔI: lane hệ thống index 0/1 bất biến byte trong phiên (prefix cache).
  KHÔNG ĐỔI: ba trụ vẫn ba tủ riêng — persona giữ *tính cách*, memory giữ *sự thật về user*.
```

### 3.2 Bất biến hệ thống (có test canh gác)

| # | Bất biến | Test canh gác |
|---|---|---|
| I1 | **Prefix cache bất biến BYTE trong phiên.** Không cơ chế mới nào ghi vào `PromptBundle` index 0/1 theo từng lượt. Recall gấp vào **nội dung gửi của lượt user**. Frozen core chỉ được ghi ở **biên phiên**. | `prompt::tests::recall_block_never_touches_system_lanes`, `frozen_core::tests::maintenance_never_writes_core_active` |
| I2 | **Không bao giờ xoá dữ liệu người dùng.** Fact → `archive_dir` (rename). Insight/episode → `.archive/` (qua `unique_in`, không đè). Review bị loại → `review/.discarded/`. Đường xoá thật duy nhất: `purge_archived`, CLI, người gõ. | `store::tests::no_write_path_calls_remove_file` (grep-test), `self_mem::tests::archive_never_overwrites_same_stem` |
| I3 | **Mọi đường ghi đi qua MỘT cửa.** `add_scoped`/`ScopedWrite` là nơi duy nhất chạy `clamp_anchor` + `threat_scan` + gán `source`. Tool `memory_save`, `#remember`, thư ký, review-promote đều đi qua đó. | `store::tests::every_write_entrypoint_clamps_and_scans` |
| I4 | **Mọi đường đọc đi qua MỘT vị ngữ.** `MemView::admits(entry, lineage)` phủ search, inventory, frozen_core. | `memview::tests::admits_is_the_only_filter` |
| I5 | **Anchor luôn là tổ tiên của cwd hoặc chính cwd, và không bao giờ CAO HƠN home.** Model không ghi được fact vào cây nó không đứng trong. | `tiering::tests::clamp_rejects_non_ancestor`, `tiering::tests::clamp_never_goes_above_home` |
| I6 | **Đọc khoan dung, ghi nghiêm.** Mọi field mới thiếu ⇒ mặc định hợp lệ; **không có bước migration bắt buộc** cho file cũ. `Tier::parse` (đĩa) khoan dung → `Place` mồ côi (fail-closed); `parse_strict` (model) nghiêm. | `store::tests::legacy_file_without_tier_loads_as_orphan_place` |
| I7 | **Không có `Default` cho `Tier`.** `MemoryEntry::default()` viết tay `tier: Place, anchor: None` (mồ côi, vô hình, fail-closed). Không bao giờ mặc định về tầng có đặc quyền nhất. | biên dịch + `store::tests::default_entry_is_orphan_not_user` |
| I8 | **Không phạt khi không có bằng chứng.** Không có cơ chế trừ điểm tự động nào trong plan này (nửa âm M3 đã cắt). Fact chỉ mất hạng do thời gian, và fact user nói thẳng thì miễn cả thời gian. | `decay::tests::curated_strength_is_time_invariant` |
| I9 | **`add` trước, `supersede` sau — và trong MỘT lần ghi.** Fact mới mang khoá `supersedes: <old-id>` ngay trong lần ghi đầu; không có cửa sổ hỏng nào để cần journal. | `supersede::tests::single_write_marks_both_sides` |
| I10 | **Mọi thao tác phá huỷ đều có đường lùi.** `supersede` ⇄ `unsupersede`; `archive` ⇄ `restore` (giữ nguyên id, đụng độ thì báo lỗi chứ không đổi id). | `store::tests::unsupersede_restores_to_active_view`, `caps::tests::restore_keeps_id_or_errors` |
| I11 | **Sub-agent không thấy dữ liệu cá nhân.** `build_subagent_base_prompt` không bao giờ chứa `<recalled_memory>`/`<user_memory>`/`<persona>`/`<self>`. | `agent::tests::subagent_prompt_has_no_personal_lanes` |
| I12 | **Quét từng fact, không quét cả blob.** `threat_scan` chạy trên MỘT fact ≤400 ký tự, không bao giờ trên JSON gộp. | `sanitize_facts::tests::blob_scan_would_false_reject` |
| I13 | **Không in secret.** Device id là hash 8 hex của `MachineGuid`/`machine-id`; raw không bao giờ log/hiện. | `device::tests::id_never_exposes_raw_secret` |
| I14 | **PART C.** Thuần Rust, một binary tĩnh; `windows-sys` giữ **0.59** (chỉ thêm feature `Win32_System_Registry`, không đổi version); không `cargo clean`; không tự push. | CI + review tay |

---

## 4. Kế hoạch theo phase

Tổng quan (ước lượng cho **một** kỹ sư, chưa tính soak):

| Phase | Mục tiêu | Công | Model call thêm / lượt |
|---|---|---|---|
| 0 | Cầm máu + nền: backup, nhánh, baseline, 3 lỗ mất dữ liệu | **1 ngày** | 0 |
| 1 | Trục `tier`/`anchor`/`lineage` + một cửa ghi | 4–5 ngày | 0 |
| 2 | Đọc tự động: recall block gấp vào lượt user | 2 ngày | 0 (chỉ +≤300 token, có cổng + delta) |
| 3 | Thư ký cuối lượt (1 call) + M2a cục bộ + M1 số học | 5–6 ngày | ±0 trên lượt code; +1 trên lượt-chỉ-có-signal (đã kẹp trần input) |
| 4 | M2b theo lô + `unsupersede` + review UX + `doctor` | 3–4 ngày | ≤1 call/**phiên** (≈0.03/lượt) |
| 5 | Bench + hiệu chỉnh hằng + quyết định mở rộng | 2 ngày | 0 |
| | **Tổng** | **17–20 ngày** + 2 tuần soak | |

Quy tắc chung cho mọi phase: kết thúc phải `cargo test` **xanh**; số test được ghi vào mô tả commit (baseline 1048 ± delta có giải trình); **không push** (PART C.5); mỗi phase là một commit-set riêng để revert độc lập.

---

### Phase 0 — Cầm máu + nền (BẮT BUỘC, ngày 1)

| | |
|---|---|
| **Mục tiêu** | Dừng ba đường mất dữ liệu đang chạy/đang chờ, và dựng đủ hạ tầng an toàn (backup + nhánh + baseline) trước khi viết dòng code thiết kế đầu tiên. |
| **Quyết định B** | Tiền đề của B9 ("partition first") + PART C.8 (backup trước khi đụng dữ liệu thật). Xử lý P0-3, P0-4, P0-13. |

**File + symbol:**

| File | Symbol | Thay đổi |
|---|---|---|
| `src/persona/self_mem.rs` | `prune_kind` (~:544-557) | `fs::remove_file(&victim.path)` → `fs::rename` sang `<persona>.self/.archive/`, **đích lấy qua helper kiểu `caps::unique_in`** (caps.rs:19-31) để không đè file archive cùng stem. `.archive` không có đuôi `.md` ⇒ `list()` bỏ qua, đã kiểm. |
| `src/memory/bloat/caps.rs` | `unique_in` | Đổi `fn` → `pub(crate) fn` để tái dùng (đừng viết bản thứ hai). |
| `src/memory/mod.rs` | `cmd_review` (:948) | `--clear`: `remove_dir_all` → rename từng file sang `review/.discarded/` (qua `unique_in`). Thêm `--drop <id>` cùng cơ chế. |
| `src/skills/mod.rs` | `write_skill_file` (~:336) | `std::fs::write` → `crate::core::persist::atomic_write`. Một dòng; xoá luôn rủi ro truncate-0-byte của `record_use` đang tồn tại mỗi lần bump `uses`. |
| `src/memory/mod.rs` | `cmd_backup` (MỚI) | `aizen memory backup [--to <dir>]` — copy đệ quy `~/.aizen` sang `~/.aizen-backup-<YYYYMMDD-HHMM>/`. Tiện ích, **không phải hàng rào** (hàng rào là bước tay ở §5). |

**Schema:** không đổi.

**Di trú:** không có. Chỉ tạo `.archive/`, `review/.discarded/` khi cần (lazy `create_dir_all`).

**Test mới:**
- `self_mem::tests::prune_archives_instead_of_deleting`
- `self_mem::tests::archive_never_overwrites_same_stem` — prune hai victim cùng stem, khẳng định archive có **2** file
- `memory::tests::review_clear_moves_to_discarded`
- `skills::tests::write_skill_file_is_atomic`

**Test cũ vỡ:** không kỳ vọng vỡ. Nếu có test nào khẳng định "sau prune, số file trong thư mục persona = cap" mà đếm cả `.archive` thì sửa cho đếm đúng thư mục sống.

**Checkpoint:**
1. `~/.aizen-backup-<ts>/` tồn tại và có ≥ 98 file (18 skill + 80 persona).
2. `git branch --show-current` = `feature/memory-tiers`.
3. `cargo test` xanh, số test ghi vào `docs/` hoặc commit message (kỳ vọng **1048**).
4. `cargo build --release` + cài đè `%LOCALAPPDATA%\Aizen` ⇒ **đồng hồ xoá insight dừng ngay hôm nay**, không đợi hết plan.

**Rollback:** `git revert` commit-set phase 0. Dữ liệu: `.archive/` chỉ thêm file, không mất gì khi lùi.

**Công / chi phí:** ~1 ngày; 0 model call.

---

### Phase 1 — Trục tier/anchor/lineage

| | |
|---|---|
| **Mục tiêu** | Thay `scope` (slug băm) + `subpath` bằng hai trục `tier` + `anchor` khớp tiền tố, và gom mọi đường ghi về **một cửa** có kiểm tra. |
| **Quyết định B** | B1 (hai trục), B2 (một kho, địa chỉ hoá bằng path), B9 bước 1 ("partition first"). Xử lý P0-12 (bypass cửa ghi), P0-6 (di trú `reinforced`), một phần P0-5 (id tất định). |

**File + symbol:**

| File | Symbol | Thay đổi |
|---|---|---|
| `src/memory/path_scope.rs` | **MỚI** | `enum Tier {User, Device, Place}` (+`as_str`, `parse` khoan dung → `Place`, `parse_strict`); `is_ancestor(anchor, cwd)`; `depth(anchor)`; `struct Lineage {cwd, places, device, home}` + `of()` (THUẦN) + `current()` (cache 1 entry, Mutex) + `specificity(&MemoryEntry) -> Option<u32>` + `narrowest_project_or_cwd()`. |
| `src/core/config.rs` | `normalize_path_key` | `fn` → `pub fn`. Thêm `anchor_of(&Path) -> String` = `canonicalize` (fallback path thô) → `normalize_path_key` → **`to_ascii_lowercase` trên Windows** (KHÔNG dùng `to_lowercase()` Unicode: sigma cuối từ / `İ` phá quan hệ tiền tố). Thêm `current_anchor()`, `project_label() -> Option<String>`. |
| `src/core/device.rs` | **MỚI** | `DeviceIdent{id,source,label}`, `current()` (OnceLock), `id()`. Probe: Windows `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` qua `windows-sys 0.59` (**thêm feature `Win32_System_Registry` vào mảng features có sẵn — cùng version, không crate mới**) → Linux `/etc/machine-id` → macOS `ioreg -rd1 -c IOPlatformExpertDevice` → fallback `~/.aizen/device.json`. `id = "dev-" + hex8(fnv1a64(raw))`. |
| `src/core/device.rs` | `device.json` | Lưu **LỊCH SỬ** `[{id, source, first_seen, last_seen}]`, không lưu một giá trị. Probe khác cache ⇒ id mới thành current, **id cũ vào `also_read`** để fact device cũ vẫn đọc được (docker/Windows Reset/sysprep không làm bốc hơi fact), + một dòng cảnh báo ở `doctor`. |
| `src/memory/store.rs` | `MemoryEntry` | +5 field: `tier: Tier`, `anchor: Option<String>`, `device: Option<String>`, `confirmations: u32`, `last_used: Option<String>`, `supersedes: Option<String>`. **Không** có `Default` cho `Tier`; `impl Default for MemoryEntry` viết tay với `tier: Tier::Place`. |
| `src/memory/store.rs` | `LEARNED_KEY_ORDER`, `LearnedRecord`, `render_learned`, `from_file`, `LearnedWrite`, `EntryPatch`+`is_empty` | Luồn tay **6 field mới** qua đủ 5 chỗ. |
| `src/memory/mod.rs` | `cmd_review --promote` (:965-976) | **Chỗ thứ TƯ của cùng cái bẫy** — chép nguyên `fm.fields.clone()` + đổi thư mục, KHÔNG dựng lại record. |
| `src/memory/store.rs` | `slugify` → `slugify_vi` | Bảng fold tiếng Việt thuần Rust (đ→d, ă/â→a, ê→e, ô/ơ→o, ư→u, ý→y…) TRƯỚC vòng `is_ascii_alphanumeric`. Không thêm crate. |
| `src/memory/store.rs` | id tất định | `id = format!("{}-{}", slugify_vi(name), hex8(fnv1a64(body_normalized)))`. Diệt va chạm `cần`/`cắn` → `c-n`, và cho phép "ghi hai lần là no-op" ở mọi nơi. |
| `src/memory/store.rs` | `add_scoped` → `ScopedWrite` | Đổi chữ ký thành struct `{name, description, mtype, body, tier, anchor, source, confidence}`; đi qua `render_learned`; **KHÔNG bail khi trùng** (dùng id tất định + ghi đè idempotent). **Đây là ĐIỂM CHẶN GHI DUY NHẤT**: gọi `tiering::clamp_anchor` + `sanitize_facts::threat_scan` bên trong. |
| `src/memory/learning/tiering.rs` | **MỚI** | `decide(proposal, lineage, home) -> TierChoice` (THUẦN) + `clamp_anchor` (THUẦN). Thay `learning/mod.rs:274 scope_for()`. |
| `src/memory/mod.rs` | `ScopeSel` → `MemView` | `Here` (mặc định) / `All` (kill-switch). **CẮT `Tier(..)` và `Under(..)`** cho tới khi có người dùng thật. `AIZEN_MEM_VIEW=here|all`, giữ `NG_NO_SCOPE=1` làm bí danh của `all`. |
| `src/memory/frozen_core.rs` | `build_scoped` | Điều kiện đổi từ `mtype == User && scope.is_none()` sang `tier == Tier::User` (+ tuỳ chọn `Device` khớp id). Xoá 3 tham số `_sel/_current_slug/_current_subpath` đang bị bỏ qua. Sửa bug ngầm dùng `MemorySettings::default()` thay vì settings đã nạp. |
| `src/core/config.rs` | `core_active_path`/`core_next_path` | Khoá `<slug>` → `<device-id>`. Hệ quả: nội dung core giống nhau ở mọi thư mục trên một máy ⇒ `cd` giữa phiên **không còn** phá prefix cache. |
| `src/memory/store.rs` | memo | `static STORE_MEMO: Mutex<Option<(u64 /*dir mtime+count*/, Vec<MemoryEntry>)>>` cho `load_all()` — mẫu đã có ở `agent/codebase.rs` (process-global memo). Bắt buộc vì Phase 2 đưa `load_all` vào đường nóng mỗi lượt. |

**Luật quyết định tier (`tiering::decide`):**

| Model / đường ghi nói | Kết quả |
|---|---|
| `user` | `Tier::User`, anchor `None` |
| `device` | `Tier::Device`, `device = device::id()`, anchor `None` |
| `place` + anchor là **tổ tiên của cwd** | nhận nguyên (ca "nâng lên thư mục cao nhất còn đúng") |
| `place` + anchor **không phải tổ tiên** | kẹp về `narrowest_project_or_cwd()`, `confidence ×0.8` |
| `place` + anchor **cao hơn home** | từ chối, kẹp xuống |
| `place` + anchor **= home** hoặc **cwd nằm ngay tại home** | **chuyển sang `Device` nếu nội dung nói về máy, ngược lại `User`; tuyệt đối không sinh anchor.** (Sửa mâu thuẫn của đặc tả: đứng ở `C:\Users\admin` thì `narrowest_project_or_cwd()` trả lại đúng giá trị vừa bị cấm ⇒ quy tắc cũ không có điểm dừng.) |
| `place` + anchor **path không tồn tại** | **không ghi anchor đó**; kẹp về tổ tiên tồn tại gần nhất. (Ngăn anchor "không bao giờ khớp" do junction/subst: ghi lúc chưa có thì giữ đường link, đọc lúc đã có thì canonicalize ra đường đích.) |
| không nói / nói bậy | `place` neo tại `narrowest_project_or_cwd()`, `confidence ×0.7`. Đoán hẹp = bỏ lỡ; đoán rộng = ô nhiễm always-on. **Bỏ lỡ rẻ hơn ô nhiễm.** |

**Thay đổi schema (frontmatter):**

```yaml
name / description / type / source / confidence / created / updated   # GIỮ
tier:          user|device|place    # THÊM, thiếu ⇒ suy ra (bảng dưới)
anchor:        c:/users/admin/desktop/mini_project/aizen   # THÊM, chỉ tier=place, path phẳng, '/' phân cách
device:        dev-1a2b3c4d         # THÊM, chỉ tier=device — KHOÁ LỌC
confirmations: <u32>                # THÊM, thiếu ⇒ min(reinforced, 3)   ← di trú im lặng, xem dưới
lastUsed:      YYYY-MM-DD           # THÊM, thiếu ⇒ updated ⇒ created
supersedes:    <old-id>             # THÊM (Phase 4 dùng), một lần ghi thay cho journal
reinforced / sessions / lastSession / lastRetrieved / validTo / supersededBy / noCore   # GIỮ
scope / subpath                     # LEGACY: chỉ đọc lúc suy ra tier, không ai ghi mới
```

**CẮT khỏi schema đặc tả:** `anchorLabel` (nguồn là `git_remote_origin` ⇒ spawn git trên đường ghi, mà chính §2.5 lấy chi phí spawn làm lý do), `originDevice` (không có người đọc), `unusedStreak` + `pendingSince` (nửa âm M3 đã cắt).

**Luật suy ra tier khi đọc file cũ (`infer_tier`, không có migration bắt buộc):**

| File cũ | tier | anchor |
|---|---|---|
| có `tier:` hợp lệ | dùng nguyên | dùng `anchor:` |
| `scope` vắng hoặc `global` | `user` | — |
| `type: user\|feedback`, `scope` vắng | `user` | — |
| `scope: <slug>` | `place` | tra bảng đảo `legacy_slug_candidates_for`; **tra hụt ⇒ anchor rỗng = place MỒ CÔI** |

**Place mồ côi:** `specificity()` trả `None` ⇒ **không bao giờ được đọc**, **không bị xoá**, hiện trong `aizen memory doctor`. Fail-closed.

**Di trú (trên máy này):**

| Hạng mục | Số lượng | Xử lý | Truy ngược |
|---|---|---|---|
| `entries/*.md` | **0** | không có gì | — |
| `core/active/*.md` | **9 file 0 byte** | **tự động xoá**, thay bằng `core/active/<device-id>.md` | Có (file rỗng) |
| `core/next/*.md` | rỗng trên máy này | file 0 byte → xoá; **file khác 0 byte → rename thành `core/next/<device-id>.md`** (nhiều file: giữ mtime mới nhất, còn lại đổi đuôi `.premigrated` + báo user) | Có |
| `graph.tsv` | chưa tồn tại | không đụng (§9 đã cắt phần graph đa trụ) | — |
| `codebase/<slug>.json`, `embed-cache/` | còn dùng | **KHÔNG ĐỤNG** (dùng `project_slug()` cho việc khác, hợp lệ) | — |
| 18 skill | 18 | **KHÔNG ĐỤNG trong plan này** (xem §9) | — |
| `reinforced` → `confirmations` | mọi install khác | **`from_file`: khoá `confirmations` VẮNG ⇒ seed `min(reinforced, 3)`** — một dòng, giữ bậc thang nửa đời đã kiếm được, ngăn "upgrade xong archive hàng loạt" | Có |

**Test mới:**
- `path_scope::tests::ancestor_is_segment_safe` · `path_scope::tests::greek_and_turkish_dirs_keep_ancestor_relation` (ascii-lowercase)
- `path_scope::tests::specificity_orders_user_device_place_by_depth`
- `path_scope::tests::orphan_place_is_never_admitted`
- `tiering::tests::clamp_rejects_non_ancestor` · `clamp_never_goes_above_home` · `cwd_at_home_yields_user_or_device_never_anchor` · `nonexistent_path_falls_back_to_existing_ancestor`
- `store::tests::render_learned_roundtrips_every_key_in_LEARNED_KEY_ORDER` — **test SỐ HỌC**, so số key với số field `LearnedRecord`; lần sau thêm field mà quên là **đỏ ngay**
- `store::tests::reinforce_preserves_tier_and_anchor` · `mark_superseded_preserves_tier_and_anchor` · `review_promote_preserves_tier_and_anchor`
- `store::tests::legacy_file_without_tier_loads_as_orphan_place` · `store::tests::missing_confirmations_seeds_from_reinforced`
- `store::tests::slugify_vietnamese_is_distinguishable` · `store::tests::remember_twice_with_same_body_is_idempotent`
- `store::tests::every_write_entrypoint_clamps_and_scans` · `store::tests::default_entry_is_orphan_not_user`
- `device::tests::id_never_exposes_raw_secret` · `device::tests::rotated_id_keeps_old_in_also_read`
- `frozen_core::tests::user_tier_is_core_eligible_regardless_of_mtype`

**Test cũ sẽ vỡ (theo thiết kế):**

| Test | Vì sao | Đổi thành |
|---|---|---|
| `caps::tests::caps_are_enforced_per_zone` | bucket đổi từ `scope` sang `(tier, anchor)` | `caps_are_enforced_per_tier_subtree` + thêm `facts_without_legacy_scope_are_bucketed_by_anchor` |
| `store::tests::slugify_basic` | thêm nhánh fold tiếng Việt | giữ 3 assert cũ + thêm assert VN |
| mọi test dựng `MemoryEntry { .. Default::default() }` (~8–10 chỗ) | `Tier` không còn `Default` | thêm `tier: Tier::User` tường minh — 8–10 dòng, một lần, từ đó compiler không im lặng được nữa |
| test khẳng định `frozen_core` lọc theo `mtype == User` | đổi vị ngữ sang tier | cập nhật assert |

**Checkpoint:** `cargo test` xanh; `aizen memory doctor` (bản sơ khai của Phase 4 có thể hoãn — tối thiểu in được: số entry, số place mồ côi, device id + source, anchor chưa từng khớp); chạy tay `aizen memory save`/`#remember` trong 2 thư mục khác nhau và xác nhận file sinh ra có `tier:`/`anchor:` đúng; `cd` sang thư mục khác trong cùng phiên **không** làm đổi `core/active/<device>.md`.

**Rollback:** revert commit-set phase 1. File đã ghi có `tier:`/`anchor:` vẫn đọc được bằng bản cũ vì `scope:`/`subpath:` được giữ nguyên trên đĩa và parser cũ bỏ qua khoá lạ — **đó là sợi dây rollback, cố ý không xoá hai khoá legacy**.

**Công / chi phí:** 4–5 ngày; **0 model call**.

---

### Phase 2 — Đọc tự động (recall block)

| | |
|---|---|
| **Mục tiêu** | Bộ nhớ tự xuất hiện đúng lúc mà không đụng lane hệ thống và không phình transcript. |
| **Quyết định B** | B5 (đọc đối xứng, tái dùng khuôn `fold_retrieval_into_query`, ngân sách ~300 token). Xử lý P0-10 (block tích luỹ vĩnh viễn), P0-8 (cắt "phủ tầng"). |

**File + symbol:**

| File | Symbol | Thay đổi |
|---|---|---|
| `src/main.rs` | `fold_memory_into_query` (MỚI, cạnh `fold_retrieval_into_query` :5160) | Trả `(String, PendingLedger)`. Gọi ở **CẢ HAI** call site: `main.rs:4076` (retained) và `main.rs:4472` (plain). |
| `src/memory/mod.rs` | `recall_block(query, budget, lineage)` | `search_filtered(query, k=12, MemView::Here)` → BM25 · `strength` → nhồi tới cạn ngân sách, **`break` chứ không `continue`** khi một dòng vượt (tập chọn đơn điệu theo budget, khác `self_mem::self_block`). |
| `src/memory/pending.rs` | **MỚI** | `PendingLedger { turn_seq, map: Vec<(handle, id)> }` — **CHỈ RAM**, không field đĩa. `open_turn()` / `close_turn(used) -> Vec<id_cần_confirm>`. Xoá ở biên lượt **và** ở `reset_per_session_state` (`main.rs:5178`). |
| `src/memory/mod.rs` | cổng liên quan | Block chỉ được gấp khi hit tốt nhất vượt ngưỡng BM25 tuyệt đối (mẫu relevance gate của `agent/codebase.rs`). Không có hit đủ mạnh ⇒ `None` ⇒ passthrough, chi phí 0. |
| `src/main.rs` | delta-injection | Ledger nhớ tập handle→id của **lượt trước**; nếu tập fact được chọn **không đổi**, KHÔNG gấp block lần nữa (model đã có nó trong transcript). Cắt phần lớn tích luỹ token và phần lớn nguy cơ "hai câu trái nhau theo trục thời gian". |
| `src/main.rs` | `maybe_auto_compact` | Khi compact chạy (prefix cache đằng nào cũng gãy), **strip mọi block recall cũ** khỏi các message user trước đó bằng marker cố định. |

**Hình dạng block** (sanitize qua `task_tool::sanitize_agent_body`, không chứa `<` mở thẻ khung):

```
Recalled memory (may be stale; a later block supersedes an earlier one — verify before relying on it):
[m1] (user) trả lời bằng tiếng Việt
[m2] (device) git ở C:\Program Files\Git\cmd; máy này không có gcc
[m3] (aizen) windows-sys ghim 0.59 — không nâng

<câu của user>
```

Handle ngắn `[m1..mN]` thay id thật: rẻ hơn, **chống model bịa id** cho fact nó chưa từng thấy, và `used` của thư ký chỉ tham chiếu được thứ đã bơm.

**CẮT khỏi đặc tả (P0-8, cả 4 lăng kính đồng ý):** quy tắc "**đảm bảo phủ tầng — chèn 2 fact `user` mạnh nhất mỗi lượt**". Ba lý do, mỗi lý do đủ để cắt:
1. Sau Phase 1, fact `tier=user` đã nằm trong frozen core (`<user_memory>`, lane được cache) ⇒ bơm lại vào lượt user là **trả tiền hai lần cho cùng một câu**, và model thấy cùng một câu hai lần trong một prompt.
2. `search` hôm nay đã loại trừ id đang nằm trong frozen core; chèn ngoài đường search là **đi vòng qua đúng cái lọc đó**.
3. Nó là nguồn gốc của vòng xoáy trừ điểm giết ràng buộc đứng (xem P0-8 ở §6).

**Đường bot (`serve`/hostbot) — tuyên bố rõ, không im lặng:** bot **không** có recall block (không có khái niệm cwd ⇒ lineage vô nghĩa). Bot nhận fact `user`/`device` qua frozen core, vốn bất biến theo máy sau Phase 1. Ghi vào doc + `--help`.

**Schema:** không đổi (ledger thuần RAM).

**Di trú:** không có.

**Test mới:**
- `prompt::tests::recall_block_never_touches_system_lanes`
- `prompt::tests::recall_block_is_skipped_when_selection_unchanged` (delta)
- `memory::tests::recall_block_breaks_on_budget_not_continues`
- `memory::tests::recall_block_is_none_below_relevance_gate`
- `pending::tests::ledger_is_cleared_on_session_reset_and_turn_boundary`
- `agent::tests::subagent_prompt_has_no_personal_lanes`

**Test cũ vỡ:** không kỳ vọng. `fold_retrieval_passthrough_when_empty_or_no_index` (main.rs:5483) vẫn đúng vì memory fold cũng giữ nguyên câu user ở cuối.

**Checkpoint:** dùng thật 10 lượt trong repo aizen: block xuất hiện đúng khi có fact liên quan, biến mất khi không; `history` sau 10 lượt chứa **≤2** block (nhờ delta); `cache_hit_label` không tụt (index 0/1 không đổi).

**Rollback:** một cờ `AIZEN_MEM_RECALL=0` tắt đường đọc mà không revert code; revert commit-set để gỡ hẳn.

**Công / chi phí:** 2 ngày; **0 model call thêm**; +≤300 token đầu vào **chỉ trên lượt mà tập fact đổi**.

---

### Phase 3 — Thư ký cuối lượt + M2a + M1

| | |
|---|---|
| **Mục tiêu** | Một lời gọi model duy nhất cuối lượt quyết định *cái gì đáng giữ và thuộc tủ nào*, và store bắt đầu đầy. |
| **Quyết định B** | B3 (thư ký), B4 (quyền sở hữu fact về user), B6/M1 (cường độ), B6/M2 (hoà giải lúc ghi — nửa cục bộ), B9 bước 2+3. Xử lý P0-7, P0-9, P0-11, P0-14. |

**Sai lệch có chủ ý so với B9 (giải trình):** B9 xếp M1+M3 (bước 3) **trước** M2 (bước 4). Plan này gộp M1 + nửa cục bộ của M2 vào cùng phase với thư ký, vì `confirmations` — đầu vào **duy nhất** của bậc thang nửa đời M1 — chỉ sinh ra từ trường `used` của thư ký và từ nhánh `same` của M2. Ship M1 trước M2 = ship một bậc thang mà đầu vào luôn bằng 0, tức hành vi **y hệt hôm nay** và không đo được gì.

**File + symbol:**

| File | Symbol | Thay đổi |
|---|---|---|
| `src/memory/learning/secretary.rs` | **MỚI** | `secretary_gate(user_text, turn) -> bool` (THUẦN), `build_input(...)`, `parse_secretary(raw) -> SecretaryOutput` (**KHÔNG BAO GIỜ `Err`**), `end_of_turn_secretary(history, opts) -> SecretaryReport`. |
| `src/main.rs` | `maybe_learn_memory` (:4888), `maybe_learn_skill` (:4659) | **Gộp thành một** lời gọi. 3 sink giữ nguyên: memory → `learning::apply_facts`; persona → `self_mem::record_episode`; skill → `skills::save_scoped`/`refine`. |
| `src/main.rs` | call site :4319/:4327-4332 (retained), :4598-4603 (plain) | Hai vòng REPL đang chạy **thứ tự ngược nhau** (memory→skill→persona vs skill→persona→memory) ⇒ thứ tự hiện tại **không phải hợp đồng**; gộp một call xoá luôn sự lệch. |
| `src/main.rs` :1600, `src/hostbot/daemon.rs` :227 | đường `serve` | `LearnOptions.allow_model_calls` — **REPL `true`, bot `false`**. Bot chỉ chạy phần miễn phí. Bật là quyết định của user, không phải tác dụng phụ của refactor. |
| `src/memory/learning/extract_free.rs` | **XOÁ HẲN** (256 dòng) | Hai đường ghi song song = ghi hai dòng cho một sự thật ngay lượt đầu. Giữ `signals.rs` (cổng free, đã có tiếng Việt) và `sanitize_facts.rs`. |
| `src/memory/learning/mod.rs` | `ingest()` nhánh ghi | Gỡ. Kèm gỡ `:176` (đẩy mọi `Inferred` vào `session_mem`) và hạ `:118` `looks_like_persona_intent` từ "chặn cả lượt" xuống "chặn lane facts". |
| `src/memory/learning/reconcile.rs` | **MỚI** | `candidates(fact_text, pool, lineage, k=5)` + `classify_local(fact, cands) -> Same(id) \| New \| NeedsJudgement(id)`. Dùng `bloat::dedup::similarity` (MinHash) ∪ `score::lexical_score_tokens`, lấy max. Pool đã nạp sẵn 1 lần/lượt (`learning/mod.rs:149`), không đọc đĩa thêm. |
| `src/memory/bloat/decay.rs` | `decayed_score`/`salience_of` | Đổi **đầu vào** `reinforced` → `confirmations`; thay `half_life·(1+ln1p(n))` bằng `HALF_LIFE_LADDER[min(c,3)]`; thêm ngoại lệ curated cho `salience_of` (hôm nay chỉ `decayed_score` có ⇒ fact `#remember` chưa từng truy hồi bị nhân ×0.5 vĩnh viễn). |
| `src/memory/bloat/mod.rs` | `compact()` | Thêm bước **archive theo sàn cường độ**; gọi ở **đầu phiên** (không phụ thuộc `!report.added.is_empty()` như hôm nay) + sau khi thư ký áp thay đổi. **Lần đầu trên mỗi store = DRY-RUN** (in báo cáo, ghi `cli-memory/.sweep-armed`), lần sau mới thật. Trần an toàn: **một lần không archive quá `max(10, 5% fact sống)`**. |
| `src/memory/store.rs` | `confirm_use(ids, today)` | `confirmations += 1`, `lastUsed = today`. Đi qua **field-map** (`fm.fields.clone()` + `serialize`), không qua `render_learned` ⇒ field lạ sống sót. |

**Cổng thư ký (FREE, 0 model call):** `signals::detect(user_text).kind != Passive` **‖** `count_tool_calls(turn) >= 4` **‖** `turn_recovered_from_dead_end(turn)` — đúng "UNION of today's gates" của B3. **Lượt passive vẫn 0 model call** (nhưng **không** phải 0 token/0 I/O — nói thẳng trong doc, đừng lặp lại tuyên bố sai của đặc tả).

**Kẹp chi phí (bắt buộc, vì `summarizer_endpoint` fallback về chính model đang dùng khi user chưa cấu hình role):**

| Điều kiện | Trần đầu vào thư ký |
|---|---|
| chưa cấu hình role `summarizer` | **1 500 token**: chỉ message user + gist trợ lý cuối + tên tool (bỏ payload); + cảnh báo một lần ở banner |
| đã cấu hình role riêng | 4 000 token |
| lượt có < 4 tool call | luôn dùng transcript RÚT GỌN bất kể cấu hình |

**Hợp đồng JSON (Phase 3 — CHƯA có `relation`/`target`):**

```jsonc
{
  "facts": [ { "text": "3..400 ký tự, GIỮ NGUYÊN NGÔN NGỮ user",
               "tier": "user|device|place",
               "anchor": "path tuyệt đối | null",
               "confidence": 0.0 } ],        // 0..=6 fact
  "episode": { "text": "≤400", "importance": 1 } | null,   // persona: CHỈ character
  "skill":   { "action": "save|refine", "name": "", "when": "", "steps": "", "target": null } | null,
  "used":    ["m1", "m4"]                    // handle của fact ĐÃ BƠM mà THẬT SỰ có ích
}
```

**Vì sao bỏ `relation`/`target` khỏi lời gọi này (P0-7 — sửa vòng gà-và-trứng):** đặc tả nạp `candidates` vào prompt nhưng lại chọn candidates bằng `reconcile_candidates(new_text, …)` — tức bằng độ giống với văn bản fact **mà model chưa sinh ra**. Chọn bằng transcript thì recall thấp ⇒ mâu thuẫn thật rơi xuống nhánh `new` ⇒ đúng bệnh M2 sinh ra để chữa. **Giải:** thư ký chỉ SINH fact. Sau khi có `facts[].text` thì chạy `reconcile::classify_local` **miễn phí** (MinHash + lexical, pool đã ở RAM):

| Similarity cao nhất | Hành động | Δ dòng |
|---|---|---|
| ≥ 0.80 | `same` → `confirm_use` (+1 confirmations, `lastUsed`) | **+0** |
| 0.55 – 0.80 | **review queue**, kèm CẢ HAI văn bản | +0 (view sống) |
| < 0.55 | `new` → ghi | +1 |

Mâu thuẫn **khác chữ** ("dùng npm" vs "chuyển sang pnpm") có similarity thấp nên rơi vào `new` — đúng, và đó là lý do **Phase 4** tồn tại: một lượt hoà giải **theo lô, ngoài đường nóng**, có đủ ngữ cảnh để phán `contradict`.

**Chống vòng tự xác nhận (P0-9):** transcript đưa cho thư ký phải dựng từ **`line` GỐC** (cả hai call site đều giữ `line` nguyên bản cho checkpoint/history), **không** từ `sent` (đã gấp block). Nếu không, thư ký đọc lại chính fact vừa bơm, nhả lại, `classify_local` phán `same`, `confirmations += 1` ⇒ fact được thăng cấp vì **được vọng lại**, không phải vì hữu ích — đúng bệnh mà việc bỏ `reinforced` sinh ra để diệt. Bổ sung chốt thứ hai: nhánh `same` **không** cộng confirmation nếu handle của nó nằm trong `injected` của chính lượt đó.

**M1 — công thức (rút gọn so với đặc tả, xem §7):**

```
H(c)        = HALF_LIFE_LADDER[min(c, 3)]                       // 30 / 90 / 270 / 730 ngày
t_u         = số ngày kể từ lastUsed (→ updated → created)
D(e)        = 1                      nếu source ≠ Inferred      // curated miễn decay THỜI GIAN (B6)
            = exp(−t_u / H(c))       ngược lại
R(e)        = exp(−t_u / H(c))
S(e)        = clamp(0.5 + 0.3·c/(c+3) + 0.2·R(e), S_min, 1.0)   // S_min = 0.65 nếu curated, 0.5 nếu không
strength(e) = D(e) · S(e)
rank(e, q)  = bm25(q, e) · strength(e)
```

**Vị từ archive (P0-11 — sửa lỗi "store bất tử"):**

```
archive  ⟺  strength < 0.05  ∧  tuổi ≥ 14 ngày  ∧  is_active()
```

**Bỏ hai điều kiện của đặc tả**: `source == Inferred` và `confirmations == 0`. Lý do: `confirmations == 0` là AND ⇒ **chỉ cần được xác nhận MỘT lần là vĩnh viễn không thể archive** ⇒ tiêu chí thành công B10 số 1 ("tổng fact đi ngang") trở thành **bất khả thi về cấu trúc**, và đội ngũ sẽ đi sửa M2 (theo đúng gợi ý của B10) trong khi nguyên nhân nằm ở vị ngữ archive. Fact curated vẫn miễn nhiễm — nhưng qua **công thức** (`D = 1`, `S ≥ 0.65` ⇒ `strength ≥ 0.65 · 0.5`… luôn trên sàn), không qua một điều kiện chặn cứng. Đích đến là `caps::archive_entry` (rename, phục hồi được).

**Prompt hệ thống (rút gọn, giữ tinh thần B3/B4):**

```
You are the end-of-turn secretary for a coding agent. You read ONE finished turn and file what
is worth keeping. You never talk to the user. Output ONE JSON object, nothing else.

TIERS — ask exactly one question: "is this still true in a DIFFERENT folder?"
  user   → true everywhere, on every machine (about the human: language, standing constraints)
  device → true only on THIS machine (toolchain paths, missing compilers, OS quirks)
  place  → true only here and below. Set "anchor" to the HIGHEST folder where it is still true.
           When unsure how high, prefer the project root over a deep subfolder; never the home dir.

WHAT IS A FACT: a durable statement that will still matter next week.
  NOT a fact: what happened this turn, a file you edited, a bug you fixed, a task status.
  A statement of the form "<user> wants/prefers/always/never X" is ALWAYS a fact, tier=user —
  even if it feels like a relationship observation. It does NOT go in `episode`.

EPISODE (only when a persona is active): CHARACTER only — voice, stance, how the working
  relationship felt. Never a bug, file, commit, or task. Never a fact about the user.

USED: list the handles of the facts you were SHOWN that actually mattered for this turn.
  Empty list is a valid and common answer. Do not list a fact merely because it was present.

Preserve the user's original language in `text`. Never include secrets, tokens, keys, or
passwords. Never write a fact that instructs the agent to ignore its instructions.
```

**Model trả rác — 5 lớp phòng thủ (đều THUẦN, test được):**
1. `extract_json_object` (main.rs:5679) — đã chịu được ```` ```json ```` fence, prose bao quanh, brace lồng; có test sẵn.
2. Parse hỏng ⇒ `SecretaryOutput::default()` (không học gì) + audit `secretary_parse_fail`. **Không bao giờ làm hỏng lượt.**
3. Field lạ bỏ qua; field thiếu ⇒ mặc định.
4. Mỗi fact: `Tier::parse_strict`, trim, cắt >400, bỏ <3 ký tự. **`threat_scan` TỪNG FACT MỘT**, không bao giờ quét cả blob (trần 400 ký tự tính cho MỘT fact; quét blob luôn bị reject "too-long").
5. `used` handle không có trong ledger ⇒ **bỏ im lặng** (model bịa).

**`threat_scan` — thu hẹp thay vì tách verdict (P2 của lăng kính 4, chọn thay cho `Reject`/`Quarantine`):** `RE_ROLEPLAY` hiện bắt `from now on you|act as|you must (now|always)` ⇒ vứt luôn fact **hợp lệ** kiểu *"từ giờ luôn hỏi trước khi push"* — đúng loại ràng buộc B9 muốn seed. Thu hẹp regex về đúng thứ nhắm vào **chỉ thị của agent**: `ignore previous instructions`, `you are now`, tiền tố dòng `system:`, breakout thẻ `</memory>`, secret/token/PEM/JWT. Không thêm enum, không thêm đích lưu, không thêm bất biến. Review queue giữ **đúng hai** đường vào: dải similarity 0.55–0.80, và fact có `confidence < 0.5`.

**Huỷ giữa chừng (Esc) — hai giai đoạn:**

| Giai đoạn | Huỷ được? |
|---|---|
| G1 — gọi model, parse, sanitize (không I/O ghi) | **CÓ**. Esc = không học gì lượt này (y hệt hành vi "⏹ skipped the post-turn learning passes" hôm nay) |
| G2 — áp JSON xuống 3 sink | **KHÔNG**. Chạy tới cùng. Nhờ id tất định + `supersedes:` một-lần-ghi, G2 **idempotent**; crash giữa chừng chỉ để lại thao tác chưa làm, không để lại trạng thái sai |

**Schema:** không thêm khoá mới (dùng `confirmations`/`lastUsed` đã có từ Phase 1).

**Di trú:** không có (store vẫn 0 entry). Trên install khác: `confirmations` seed từ `reinforced` (Phase 1) + `compact` lần đầu là dry-run.

**Test mới:**
- `secretary::tests::gate_is_union_of_signal_toolcount_recovery`
- `secretary::tests::garbage_json_yields_empty_output_not_error`
- `secretary::tests::transcript_never_contains_recall_block`
- `secretary::tests::same_branch_skips_confirmation_for_this_turn_injection`
- `secretary::tests::input_is_capped_when_no_summarizer_role`
- `reconcile::tests::high_similarity_confirms_without_new_row`
- `reconcile::tests::ambiguous_band_goes_to_review_untouched`
- `decay::tests::half_life_ladder_steps_at_1_2_3_confirmations`
- `decay::tests::curated_strength_is_time_invariant`
- `caps::tests::first_compact_on_a_store_is_dry_run`
- `caps::tests::compact_never_archives_more_than_5_percent`
- `learning::tests::every_secretary_sink_scans_each_item`
- `learning::tests::persona_intent_blocks_only_the_facts_lane`

**Test cũ vỡ:**

| Test | Vì sao | Đổi thành |
|---|---|---|
| toàn bộ `extract_free::tests::*` | module bị xoá | **xoá cùng module** — ghi rõ delta test trong commit message (vd. `1048 − 19 + 34 = 1063`) |
| `learning::tests::*` liên quan `ingest` ghi | nhánh ghi bị gỡ | chuyển sang test `apply_facts` |
| `decay::tests` dùng `reinforced` (2 test) | đầu vào đổi | dựng entry với `confirmations` |
| `score::tests` salience (3 test) | chỉ đổi **tên tham số**, công thức giữ nguyên | cập nhật tên |

> Lưu ý về "1048 test xanh": việc xoá `extract_free.rs` làm **giảm** số test một cách chính đáng. Hợp đồng của PART C.9 là **xanh**, không phải một con số cố định; mỗi commit ghi rõ `trước → sau` kèm lý do delta.

**Checkpoint:** sau 1 ngày dùng thật, `~/.aizen/cli-memory/entries/` có **≥3 file** với `tier` đa dạng; `aizen memory list` in ra chúng; cổng đóng trên lượt passive (xác nhận bằng log/`audit`); `confirmations` của ít nhất 1 fact đã lên 1.

**Rollback:** `AIZEN_MEM_SECRETARY=0` tắt lời gọi (quay về 0 học, không quay về regex chết); revert commit-set để gỡ. Fact đã ghi vẫn đọc được.

**Công / chi phí:** 5–6 ngày.

**Chi phí model thật (bảng trung thực, thay bảng "2 → 1" của đặc tả):**

| Loại lượt | Hôm nay | Sau Phase 3 |
|---|---|---|
| Lượt code (≥4 tool call) | 1 call (skill) | **1 call** (thư ký), prompt lớn hơn ~+300…900 token vào, +200…350 ra |
| Lượt chat có signal (`ghi nhớ…`, `tôi thích…`) | 0 call (regex free, và **hỏng**) | **+1 call**, nhưng transcript rút gọn ⇒ ~600–900 token vào |
| Lượt passive | 0 | **0** |
| Persona reflection | 1 call theo chu kỳ (`should_reflect`) | **giữ nguyên, vẫn là lời gọi riêng** — thư ký KHÔNG thay thế nó (đặc tả ngụ ý sai) |

---

### Phase 4 — M2b hoà giải theo lô + un-supersede + doctor

| | |
|---|---|
| **Mục tiêu** | Bắt mâu thuẫn **khác chữ** và cho phép sửa sai — mà không đặt quyền phá huỷ vào đường nóng. |
| **Quyết định B** | B6/M2 (nửa còn lại), B10 số 3. Xử lý P0-2 (không có đường un-supersede), P0-5 (journal). |

**Điều kiện tiên quyết cứng:** `store::unsupersede` + `aizen memory revive` phải **ship TRƯỚC** khi bật bất kỳ nhánh `contradict` tự động nào. Hôm nay repo **không có** đường un-supersede nào: `EntryPatch` không chạm được `validTo`/`supersededBy`, `update()` chỉ insert chứ không remove, `active()` giấu vĩnh viễn, `caps::restore` chỉ áp cho file đã ở archive. Một `contradict` sai = **mất fact đúng trên thực tế** dù byte vẫn còn.

**File + symbol:**

| File | Symbol | Thay đổi |
|---|---|---|
| `src/memory/store.rs` | `unsupersede(entry)` (MỚI) | Xoá `validTo` + `supersededBy` qua field-map (field lạ sống sót); và xoá `supersedes:` ở phía fact mới nếu có. |
| `src/memory/store.rs` | `EntryPatch` | +`clear_supersede: bool` (nhớ cập nhật `is_empty()`). |
| `src/memory/bloat/supersede.rs` | `active(entries)` | Một fact X là **không** active nếu: X có `validTo`/`supersededBy`, **HOẶC** tồn tại Y đang sống có `supersedes: X`. ⇒ **add + supersede thành MỘT lần ghi atomic**, không còn cửa sổ hỏng ⇒ **không cần journal**. |
| `src/memory/mod.rs` | `cmd_revive`, `cmd_list --superseded` | `aizen memory revive <id>` + `/revive`; `memory list --superseded` để **nhìn thấy nghĩa địa** (hôm nay `memory_list` lọc active ⇒ user không có cách nào biết id để sửa tay). |
| `src/memory/bloat/caps.rs` | `restore` | **Giữ nguyên id**; đụng độ ⇒ **báo lỗi** yêu cầu `--as <new-id>` chứ không lặng lẽ đổi thành `<id>-2` (id là khoá của `supersededBy` và của mọi cạnh graph). |
| `src/memory/learning/reconcile.rs` | `batch_pass()` (MỚI) | Chạy **đầu phiên**, ngoài đường nóng. Lấy các cặp trong dải nghi vấn (review queue + fact mới thêm từ lần chạy trước), gom **≤12 cặp**, **1 model call duy nhất**, trả `same / refine / contradict / unsure` cho từng cặp. |
| `src/memory/learning/audit.rs` | op mới | `supersede {old, new, session, ts}`, `reconcile`, `revive` — nền cho `aizen memory undo-last-secretary`. |
| `src/memory/mod.rs` | `cmd_doctor` (MỚI) | In: số fact sống/archive/superseded/review; **place mồ côi**; **anchor chưa từng khớp trong N ngày**; `supersededBy` trỏ id không tồn tại; device id + source + `also_read`; cặp near-dup cùng sống. |

**Rào an toàn của M2b:**

| Điều kiện | Hành động |
|---|---|
| `contradict` với `confidence ≥ 0.65` | áp: ghi fact mới mang `supersedes: <old>` (một lần ghi) + audit |
| `contradict`/`refine` với `confidence < 0.65` | **để nguyên trong review**, không đụng fact cũ |
| target có `confirmations ≥ 2` | **luôn** vào review bất kể confidence — rào cản tỉ lệ thuận với thứ đang bị phá |
| `refine` được áp | ghi đè body; **reset `confirmations = min(c, 1)`**, `lastUsed = today`; bản cũ copy sang `.archive/<id>-r<N>`, **giữ nguyên id** (graph + `supersededBy` vẫn trỏ đúng) |
| `unsure` | ở lại review |
| hai ứng viên near-dup **trên cùng chuỗi kế thừa** | **`updated`/`lastUsed` mới hơn thắng; specificity chỉ là tie-break.** (Sửa lỗi của đặc tả: "tổ tiên gần nhất thắng" là quy tắc về VỊ TRÍ, dùng làm trọng tài cho MÂU THUẪN sẽ khiến fact **cũ và hẹp** thắng fact **mới và rộng** — vd. `tokio 1.35` neo `…/aizen/src` đánh bại `tokio 1.40` neo `…/aizen`.) Fact sống sót được **re-anchor về tổ tiên chung**. |

**Ngân sách M2b:** ≤1 model call/phiên; kích hoạt khi `≥8` cặp chờ **hoặc** `≥7` ngày kể từ lần chạy trước; `aizen memory reconcile` chạy tay, **dry-run mặc định**, `--apply` mới ghi.

**Schema:** dùng `supersedes:` đã thêm ở Phase 1. Không khoá mới.

**Di trú:** không có.

**Test mới:**
- `store::tests::unsupersede_restores_to_active_view`
- `supersede::tests::single_write_marks_both_sides` · `supersede::tests::supersedes_field_hides_old_from_active`
- `caps::tests::restore_keeps_id_or_errors`
- `reconcile::tests::batch_pass_is_capped_at_one_call_and_twelve_pairs`
- `reconcile::tests::low_confidence_contradict_never_touches_the_old_fact`
- `reconcile::tests::confirmed_target_always_goes_to_review`
- `reconcile::tests::refine_resets_confirmations`
- `reconcile::tests::newer_wins_over_deeper_on_same_chain`
- `doctor::tests::reports_orphan_place_and_dangling_supersededby`

**Test cũ vỡ:** `supersede::tests::as_of_reconstructs_history` có thể cần cập nhật vì `active()` đổi chữ ký (nhận cả tập để xét `supersedes:`) — giữ nguyên khẳng định, đổi cách gọi.

**Checkpoint:** dựng tay 2 fact trái nhau khác chữ ("dùng npm" / "chuyển sang pnpm"), chạy `aizen memory reconcile` → dry-run in ra đúng cặp; `--apply` → fact cũ biến khỏi `list`, hiện ở `list --superseded`; `aizen memory revive` đưa nó về; `doctor` sạch.

**Rollback:** revert commit-set. Mọi thao tác đã áp đều đảo được bằng `revive` (đó là lý do nó là điều kiện tiên quyết).

**Công / chi phí:** 3–4 ngày; **≤1 model call mỗi phiên** (≈0.03 call/lượt với phiên 30 lượt).

---

### Phase 5 — Bench, hiệu chỉnh hằng số, quyết định mở rộng

| | |
|---|---|
| **Mục tiêu** | Biến các con số `[TỰ QUYẾT]` thành số có bằng chứng, và quyết định (bằng dữ liệu) có mở lại phần đã cắt hay không. |
| **Quyết định B** | B10 (3 chỉ số), PART C.10 (bench-gated trước khi thành default). |

**Trạng thái: hạ tầng ĐÃ XONG, hiệu chỉnh CHƯA THỂ LÀM.** Hai nửa này tách hẳn nhau, và lý do phải nói thẳng.

**Nửa đã làm (5a — cơ khí, không cần dữ liệu):**

- `src/memory/stats.rs` (mới): `stats.jsonl` một dòng cộng dồn mỗi phiên (§8 nói viết ở Phase 3 — thực tế **chưa từng có**, Phase 5 mới viết). Mọi phép đo là hàm **thuần** trên các mẫu đã parse: `weekly` / `saturation` / `use_ratio` / `is_flattening` / `contradictions_weekly`.
- Audit op `recall` ghi `(injected N, used M)` mỗi lượt cổng-mở — nguồn duy nhất của chỉ số 2. Ghi **trước** cửa `out.is_empty()`: một lượt được bơm 5 fact và dùng 0 là mẫu giá trị nhất mà tỉ số có; bỏ nó thì chỉ còn các lượt recall đúng và chỉ số sẽ đọc cao đúng cho cái store cần sửa nhất.
- Bốn điểm đếm: `note_turn` ở `fold_context_into_query` (điểm duy nhất **cả hai** REPL loop đi qua đúng một lần mỗi tin nhắn user — đếm trong agent loop là đếm iteration, sai mẫu số của chỉ số 1), `note_secretary_call` **trước** lệnh gọi (một call lỗi vẫn bị tính tiền), `note_recall`, và `append_memory_stats_sample` trên đường thoát.
- `aizen memory health`: bảng theo tuần + phán quyết cho cả ba chỉ số. Nói thẳng khi dữ liệu mỏng (<4 tuần) thay vì kết luận từ 3 điểm.
- `aizen bench health` + `bench-fixtures/health.jsonl`: golden set 6 lịch sử người gán nhãn, cùng kỷ luật anti-oracle như hai bench kia (lint hard-fail trước khi chấm, mỗi case bắt buộc có `why`). Bốn case **âm** là phần đáng giá: store phình mãi, store ngủ đông, khe hở trong chuỗi, và recall bơm-mà-vô-dụng — cả bốn *đều sẽ* trông như bão hoà nếu phép đo viết hớ.
- Một lỗ thật do golden set phát hiện: `is_flattening` cũ trả lời câu hỏi 3 tuần từ 2 tuần dữ liệu, nên hai mẫu hai bên một khoảng lặng 18 ngày trông y hệt một store đã lắng xuống — **một chuyến nghỉ phép chứng minh được thiết kế đúng**. Nay thiếu dữ liệu là `false` (chưa chứng minh), không phải `true`.

**Nửa chưa làm (5b — cần corpus, KHÔNG thể làm hôm nay):**

Hiệu chỉnh `HALF_LIFE_LADDER`, `strength_floor`, ngưỡng `0.80`/`0.55`, `k=5`, ngân sách 300 token, `secretary_max_input_tokens` — và ba quyết định mở rộng: (a) nửa âm của M3 (chỉ khi đo được **thật sự** có fact bơm-mà-vô-dụng); (b) graph đa trụ (chỉ khi ≥150 fact sống); (c) M4 (chỉ khi ≥200 fact sống ∧ ≥40 cạnh).

Cả sáu hằng và cả ba quyết định đều đòi 2–4 tuần dùng thật. `stats.jsonl` hôm nay **rỗng** (nó vừa được viết ra trong phase này), nên không có gì để đọc. Chọn số bây giờ đúng là kiểu đoán mà §8 được viết ra để tránh — cho nên các hằng số ở §7 **giữ nguyên mặc định**, và việc chỉnh chúng phải đợi `aizen memory health` có ≥4 tuần dữ liệu. Đây là chặn bởi thời gian, không phải bởi công sức.

- **Công:** 5a xong (~1 ngày). 5b: 2 ngày, mở sau 2–4 tuần dùng. **Chi phí model:** 0 (bench offline, có tiền lệ CI `dense-bench.yml`).

---

## 5. Phase 0 — thủ tục chính xác (lệnh cụ thể)

**Bước 0.1 — Backup TAY, trước dòng code đầu tiên** (không đợi `aizen memory backup` được viết — đồng hồ mất dữ liệu đang chạy):

```powershell
$ts = Get-Date -Format 'yyyyMMdd-HHmm'
robocopy "$env:USERPROFILE\.aizen" "$env:USERPROFILE\.aizen-backup-$ts" /MIR /R:1 /W:1
# robocopy: exit code 0-7 = THÀNH CÔNG (8+ mới là lỗi). Kiểm tra bằng số file:
(Get-ChildItem "$env:USERPROFILE\.aizen-backup-$ts" -Recurse -File).Count
```
Kỳ vọng ≥ 98 file (18 skill + 80 persona + config + cache). **Lặp lại bước này trước MỖI lần chạy `--apply` về sau.**

**Bước 0.2 — Nhánh mới, tách từ `main`, KHÔNG chồng lên nhánh soak:**

```bash
git fetch --all
git switch main                      # đang ở release/v0.4.9 — không đụng vào nó
git switch -c feature/memory-tiers
git branch --show-current            # phải in: feature/memory-tiers
```

**Bước 0.3 — Chốt baseline (KHÔNG `cargo clean` — PART C.3):**

```bash
cargo test 2>&1 | tail -20           # ghi lại con số vào commit message của Phase 0
```

**Bước 0.4 — Ba bản vá cầm máu** (chi tiết ở [Phase 0](#phase-0--cầm-máu--nền-bắt-buộc-ngày-1)): `prune_kind` → archive không đè · `cmd_review --clear` → `.discarded/` · `write_skill_file` → atomic.

**Bước 0.5 — Build + cài ngay** để đồng hồ dừng hôm nay (máy này build release được, ~4m36s → ~25.8 MB):

```bash
cargo build --release
# copy đè %LOCALAPPDATA%\Aizen (giữ bản 0.4.9 đang có ở Desktop làm backup như thường lệ)
```

**Bước 0.6 — Không push.** PART C.5: commit được, push cần user chốt.

---

## 6. Sổ rủi ro xếp hạng + đối chiếu P0

### 6.1 Mười lăm P0 và cách plan xử lý

| # | P0 (nguồn) | Xử lý trong plan |
|---|---|---|
| 1 | `graph::prune` xoá sạch `graph.tsv` khi đổi endpoint sang `f:`/`s:`/`i:` (`live` dựng từ id trần) | **CẮT graph đa trụ** (§9). Không đổi tiền tố ⇒ bẫy biến mất. Thêm chốt rẻ ở Phase 4: `prune` **từ chối ghi** nếu `pruned/before > 0.5`, chỉ cảnh báo + audit |
| 2 | Không tồn tại đường un-supersede ⇒ `contradict` sai = mất fact đúng | **Phase 4 mở đầu bằng `unsupersede` + `revive` + `list --superseded`**, là **điều kiện tiên quyết** trước khi bật bất kỳ contradict tự động nào |
| 3 | `prune_kind` rename sẽ **ĐÈ** file archive cùng stem (`unique_path` chỉ chống trùng ở thư mục sống) | Dùng `caps::unique_in` cho đích archive + `debug_assert!(!dest.exists())` + test 2-file |
| 4 | `cmd_review --clear` = `remove_dir_all`; `--promote` dựng lại record ⇒ **bẫy thứ 4** làm mất `tier`/`anchor` | Phase 0 sửa `--clear`; Phase 1 sửa `--promote` sang chép nguyên field-map + test riêng |
| 5 | Journal `secretary.journal` **không thể idempotent** (`new_id` chưa tồn tại lúc ghi plan) ⇒ nhân bản fact sau mỗi crash | **CẮT journal.** Thay bằng: **id tất định** (`slug-hex8(body)`) + khoá `supersedes:` trên fact mới ⇒ add+supersede là **một lần ghi atomic** ⇒ không còn cửa sổ hỏng để mà bảo vệ |
| 6 | Bỏ `reinforced` không di trú ⇒ `sweep` đầu tiên sau upgrade archive **hàng loạt** | `from_file` seed `confirmations = min(reinforced, 3)`; `compact` **lần đầu trên mỗi store là DRY-RUN**; trần archive `max(10, 5%)`/lần |
| 7 | Gà-và-trứng: `candidates` chọn bằng văn bản fact **chưa tồn tại** | Tách **M2a** (cục bộ, miễn phí, sau khi có fact text) và **M2b** (theo lô, ngoài đường nóng, 1 call/phiên). Bỏ `relation`/`target` khỏi lời gọi thư ký |
| 8 | "Phủ tầng" (bơm 2 user fact/lượt) + demerit ⇒ **ràng buộc đứng của user thành vô hình mà không thể archive** | **CẮT cả hai**: bỏ luật phủ tầng (fact `user` đã ở frozen core), **CẮT toàn bộ nửa âm của M3** (không có `unusedStreak`/`pendingSince`/reaper/`M(e)`) |
| 9 | Vòng tự xác nhận: thư ký đọc transcript có chứa block recall ⇒ tự thăng cấp fact | Transcript thư ký dựng từ **`line` gốc**; nhánh `same` không cộng confirmation nếu handle nằm trong `injected` của chính lượt đó; + test canh gác |
| 10 | Block recall bị `history.push(Message::user(sent))` ⇒ **tồn tại vĩnh viễn** trong transcript, mang theo cả fact đã bị supersede | Cổng liên quan + **delta-injection** (không gấp lại nếu tập fact không đổi) + header "a later block supersedes an earlier one" + strip toàn bộ block cũ khi `maybe_auto_compact` chạy (lúc đó cache đằng nào cũng gãy) |
| 11 | Vị từ archive có `confirmations == 0` là AND ⇒ phần lớn store **bất tử**, B10 số 1 bất khả thi | Vị từ rút gọn còn `strength < floor ∧ tuổi ≥ 14d ∧ is_active()`; curated miễn nhiễm **qua công thức** (`D=1`, `S_min=0.65`) chứ không qua điều kiện chặn |
| 12 | `clamp_anchor` + `threat_scan` chỉ nằm trên đường thư ký ⇒ tool `memory_save` (model gọi được) **đi vòng** cả I5 lẫn I12 | Dời **cả hai** xuống `add_scoped`/`ScopedWrite` = **điểm chặn ghi duy nhất**; `tiering::decide` chỉ còn trả **đề xuất**; test kiểu grep khẳng định không có đường ghi nào khác |
| 13 | Bản vá `prune_kind` bị xếp ở §11 ⇒ sẽ được làm sau nhiều tuần, trong khi mỗi lượt formative xoá một file | Nâng thành **Phase 0, ngày 1, có build + cài ngay**; backup tay là **bước không-code** đứng trước tất cả |
| 14 | Ship M1 trước M2 (theo chữ của B9) = ship bậc thang nửa đời có đầu vào luôn = 0 | Gộp M1 + M2a vào **cùng Phase 3** với thư ký; giải trình sai lệch ngay trong phase |
| 15 | §9 (M4) phải viết mới 100%, điều kiện tối thiểu ≥200 fact sống, store có 0 fact | **CẮT hoàn toàn** khỏi plan; điều kiện tái mở ghi ở §9 |

### 6.2 Sổ rủi ro còn lại (đã giảm thiểu, không triệt tiêu)

| Hạng | Rủi ro | Biện pháp giảm thiểu |
|---|---|---|
| **R1** | **I/O đường nóng**: recall block đưa `load_all()` + BM25 vào **mỗi lượt**, trước khi gửi request ⇒ cộng thẳng vào time-to-first-token, và tệ dần theo kích thước store | Memo process-global cho `load_all` (mẫu `agent/codebase.rs`); `compact` **chỉ đầu phiên**; cổng liên quan + delta cắt phần lớn lời gọi; đo bằng `doctor` (in ms của lần recall gần nhất) |
| **R2** | **Race đọc-sửa-ghi**: `write_atomic` lấy khoá **bên trong**, các hàm read-modify-write đọc file **trước** khi vào khoá ⇒ hai tiến trình (REPL + hostbot daemon dùng chung `~/.aizen`) có thể cùng đọc `c=n` rồi cùng ghi `n+1` | **Chấp nhận có ý thức** trong plan này: mất 1 confirmation chỉ **trễ nhịp** tăng nửa đời, không sai ngữ nghĩa. Mở rộng khoá đụng 4 hàm nóng ⇒ PR riêng. Giảm nhẹ: `confirm_use` dedup theo `(id, ngày)` |
| **R3** | **Device id xoay** (docker `machine-id` mới mỗi container, Windows Reset/sysprep, VM clone dùng chung GUID) ⇒ fact `device` mồ côi im lặng, hoặc rò chéo máy | `device.json` lưu **lịch sử** + `also_read`; probe sống luôn ghi đè cache (chống `~/.aizen` bị sync); `doctor` in một dòng khi id đổi |
| **R4** | **Multi-tenant daemon**: `hostbot/daemon.rs` gọi `set_current_dir` trong MỘT tiến trình phục vụ nhiều chat ⇒ `Lineage::current()` cache theo cwd toàn cục ⇒ rò chéo khách hàng nếu sau này bật `allow_model_calls` | `Lineage::of` là **THUẦN** — đường daemon **không** dùng `current()`, truyền cwd per-chat. Chặn cứng: khi `allow_model_calls == false`, `tiering::decide` chỉ được trả `User`/`Device`, không bao giờ `Place` |
| **R5** | **Anchor không bao giờ khớp** (junction/subst: ghi lúc thư mục chưa tồn tại giữ đường link, đọc lúc đã tồn tại giữ đường đích) | Không ghi anchor cho path không tồn tại (kẹp về tổ tiên tồn tại gần nhất); `doctor` liệt kê "anchor chưa từng khớp trong N ngày"; bọc `canonicalize` bằng deadline ngắn (mẫu `join_drain`) + cache âm cho UNC |
| **R6** | **Chi phí model tăng** vì `summarizer_endpoint` fallback về chính model đang dùng khi user chưa cấu hình role | Trần input 1 500 token khi chưa cấu hình role + transcript rút gọn cho lượt ít tool call + cảnh báo một lần ở banner + `MemorySettings.secretary_max_input_tokens` |
| **R7** | **Review queue phình mà không ai đọc** — hôm nay nó có đường ghi và một `cmd_review` chưa ai dùng | Giữ **đúng 2 đường vào**; in một dòng nhắc trong banner khi `review/` có ≥5 item; `doctor` đếm nó; M2b tự tiêu thụ hàng đợi |
| **R8** | **`compact` chạm frozen core** sau một refactor tương lai ⇒ index 1 đổi giữa phiên ⇒ toàn bộ transcript re-bill uncached | Bất biến I1 + test `frozen_core::tests::maintenance_never_writes_core_active` (snapshot mtime + nội dung trước/sau `compact`) |
| **R9** | **Xoá `extract_free.rs` làm giảm số test** ⇒ trông như hồi quy so với mốc 1048 | Ghi delta tường minh trong commit message; hợp đồng là **xanh**, không phải một con số |
| **R10** | **13 skill vẫn nằm trong ngăn kéo rác** `skills/p/admin-e4df4c78/` | **Cố ý không đụng** (§9): hôm nay chúng hiện **đúng** khi cwd nằm dưới `C:\Users\admin` ⇒ không có hồi quy. Đổi lại đã sửa `write_skill_file` thành atomic (Phase 0) — rủi ro thật duy nhất của trụ này |

---

## 7. Bảng hằng số & công thức

Tất cả vào `config::MemorySettings` (có sẵn), chỉnh bằng `cli-config.json` hoặc biến môi trường tương ứng.

| Hằng | Mặc định | Lý do chọn | Cách chỉnh |
|---|---|---|---|
| `HALF_LIFE_LADDER` | `[30, 90, 270, 730]` ngày | Đúng B6/M1: 1st ≈30d → 2nd ≈90d → 3rd ≈270d → 4th+ ≈2 năm (thực tế là vĩnh viễn) | `half_life_ladder_days` |
| `recency_half_life_days` | `30.0` (GIỮ TÊN) | nay là `LADDER[0]`; giữ tên để tương thích cấu hình cũ | như cũ |
| `strength_floor` | `0.05` | Kiểm tra số học: fact inferred c=0 → archive quanh ngày **75–80**; c=1 → ~1 năm; c=2 → ~2 năm; c=3 → ~10 năm | `strength_floor` |
| `strength_floor_min_age_days` | `14` | Fact mới không bao giờ bị archive vì "chưa kịp dùng" | `strength_floor_min_age_days` |
| `S_min` (curated) | `0.65` | Sửa nửa còn thiếu: `salience_of` hôm nay **không** có ngoại lệ curated ⇒ fact `#remember` chưa truy hồi bị nhân ×0.5 vĩnh viễn. Con số cảm tính, cần bench | `curated_salience_floor` |
| `S_min` (thường) | `0.5` | Giữ nguyên hợp đồng "salience là phần thưởng, trần 2×" ⇒ **5 test cũ sống** | — |
| `minhash_dup_threshold` | `0.80` (GIỮ) | Ngưỡng `same` — dùng lại đúng ngưỡng dedup đang có | như cũ |
| `reconcile_review_band` | `0.55` | Sàn "cùng chủ đề" (thấp hơn `same` vì chỉ cần cùng đề tài, không cần cùng chữ). **Cần bench** | `reconcile_review_band` |
| `reconcile_k` | `5` | Số ứng viên đối chiếu mỗi fact. Đủ để bắt, đủ nhỏ để không nhồi rác | `reconcile_k` |
| `MEMORY_RECALL_BUDGET_TOKENS` | `300` | Đúng B5 ("small budget ~300 tokens"); ~1/5 của `CODEBASE_RETRIEVAL_BUDGET_TOKENS = 1500` đang có | `memory_recall_budget_tokens` |
| `recall_k` | `12` | Số ứng viên trước khi nhồi theo ngân sách | `recall_k` |
| `secretary_max_input_tokens` | `1500` (chưa cấu hình role summarizer) / `4000` (đã cấu hình) | Chặn kịch bản thư ký chạy trên **model chính đắt tiền** với transcript 15 tool call | `secretary_max_input_tokens` |
| `learn_inferred_cap` | `500` (GIỮ) | Lưới an toàn tuyệt đối; đường xoá chính là sàn cường độ | như cũ |
| `compact_archive_max_ratio` | `0.05` (sàn tuyệt đối 10) | Một lần `compact` không được archive quá 5% fact sống | `compact_archive_max_ratio` |
| `m2b_min_pairs` / `m2b_min_days` | `8` / `7` | Kích hoạt lượt hoà giải theo lô; ≤1 call/phiên | `m2b_*` |
| `device id` | `"dev-" + hex8(fnv1a64(raw))` | `fnv1a64` đã có sẵn trong `config.rs`; **không bao giờ in raw** | — |

**Công thức đầy đủ (một chỗ duy nhất, `bloat/decay.rs`):**

```
H(c)        = HALF_LIFE_LADDER[min(c, 3)]
t_u         = ngày kể từ lastUsed → updated → created
D(e)        = 1                                  nếu source ≠ Inferred
            = exp(−t_u / H(c))                   nếu source == Inferred
R(e)        = exp(−t_u / H(c))
S(e)        = clamp(0.5 + 0.3·c/(c+3) + 0.2·R(e), S_min(e), 1.0)
strength(e) = D(e) · S(e)
rank(e, q)  = bm25(q, e) · strength(e)

archive     ⟺ strength(e) < 0.05  ∧  tuổi(e) ≥ 14 ngày  ∧  is_active(e)
```

**Đã bỏ khỏi công thức so với đặc tả:** nhân tử trừ điểm `M(e) = 1/(1 + β·d)`. Lý do: nó cần `unusedStreak`, mà `unusedStreak` cần nửa âm của M3 (đã cắt); và với luật "phủ tầng" bị cắt thì nguồn sinh demerit lớn nhất cũng biến mất. Nếu Phase 5 đo được **thật sự** có fact bơm-mà-vô-dụng, mở lại với dạng đúng chữ B6: `M = 1/(1 + β·max(0, d − 3))` (ba lượt đầu miễn phí) chứ không phạt ngay từ lượt 1.

---

## 8. Cách đo thành công (B10)

Ba chỉ số của B10, mỗi chỉ số có **nguồn số liệu cụ thể**, không cần nhớ hay ước lượng.

**Hạ tầng đo — thực tế viết ở Phase 5, không phải Phase 3** (§8 bản đầu ghi Phase 3; khi Phase 5 đi tìm thì `stats.jsonl` và audit op `recall` **chưa từng tồn tại**, nên chỉ số 2 và 3 khi đó không có nguồn số liệu nào). Nay ở `src/memory/stats.rs`: cuối mỗi phiên **có ít nhất một lượt** (phiên 0 lượt không phải một mẫu — `aizen --version` và cả test suite cũng đi qua đường thoát này), append một dòng cộng dồn vào `cli-memory/stats.jsonl`:

```jsonc
{"ts":"2026-08-10T09:12:00Z","live":63,"archived":4,"superseded":9,"review":2,
 "injected_total":412,"used_total":151,"secretary_calls":88,"turns":301}   // cộng dồn từ phiên trước
```

| # | Chỉ số B10 | Đo bằng | Ngưỡng "đang hoạt động" | Nếu trượt thì nhìn đâu |
|---|---|---|---|---|
| 1 | **Tổng số fact đi ngang trong khi mức dùng tăng** | `stats.jsonl`: `live` theo tuần vs `turns` theo tuần. Vẽ tỉ số `Δlive / Δturns` | Tỉ số **giảm dần** sau tuần 3; `superseded + review` tăng cùng nhịp `live` | B10 nói: nhìn M2 trước. Trong plan này = **M2b** (Phase 4) — kiểm `reconcile` dry-run có sinh cặp không, và ngưỡng `0.55` có quá cao không |
| 2 | **Tỉ lệ bơm-và-thật-sự-dùng tăng** | Audit op `recall` ghi `(injected N, used M)` mỗi lượt cổng-mở; tuần = `Σ M / Σ N` | ≥0.35 và **tăng** qua các tuần | Ngưỡng cổng liên quan quá thấp (bơm bừa) hoặc `strength` xếp sai — chỉnh `recall_k` / `S_min` |
| 3 | **Số mâu thuẫn bắt được mỗi tuần GIẢM** | Audit op `supersede` + `reconcile{verdict=contradict}` đếm theo tuần | Đỉnh ở tuần 2–4 rồi **giảm** (store hội tụ) | Nếu **không bao giờ** có mâu thuẫn nào: `reconcile_review_band` quá cao ⇒ M2b không thấy cặp nào |

**Bench offline (Phase 5):** `src/bench/brain.rs` — chấm chất lượng truy hồi trên corpus thật đã tích được, dùng làm cổng cho mọi quyết định "bật mặc định" (PART C.10). Có tiền lệ chạy trên CI (`dense-bench.yml`) khi máy cục bộ không link được.

**Kiểm tra sức khoẻ liên tục:** `aizen memory doctor` — nếu nó im lặng thì hệ đang khoẻ; mọi bệnh của thiết kế này (place mồ côi, anchor chưa từng khớp, `supersededBy` treo, device id vừa xoay, cặp near-dup cùng sống, review tồn đọng) đều hiện ở đó.

---

## 9. Cố ý KHÔNG làm trong plan này

| Thứ bị cắt | Vì sao cắt | Điều kiện tái mở |
|---|---|---|
| **§9 — M4 "ngủ" hợp nhất** | Phải viết mới **100%** (`learning/consolidate.rs` thật chỉ ~100 dòng làm `best_match` lexical; `cluster\|ClusterKey\|group_by` = 0 kết quả toàn `src`). Điều kiện tối thiểu của chính nó là ≥200 fact sống ∧ ≥40 cạnh — hôm nay 0 fact, 0 cạnh. Và vị ngữ cụm của nó (**cạnh ≥0.5** ∧ **mọi thành viên strength <0.30**) **mâu thuẫn số học** với công thức M1: cạnh mạnh ⇒ hai đầu mút vừa được confirm ⇒ strength ≈0.35 > 0.30 ⇒ luôn trả **0 cụm** | ≥200 fact sống ∧ ≥40 cạnh ∧ đã bench lại vị ngữ cụm |
| **§10 — graph đa trụ (`f:`/`s:`/`i:`)** | 0 hành vi quan sát được (lan truyền vẫn default-OFF chờ bench), đổi lại **một bẫy xoá không hồi phục**: `graph::prune` xoá thật cạnh có đầu mút ngoài `live`, mà `live` dựng từ id **trần**. Thêm: `EDGE_HALF_LIFE_DAYS = 60` ⇒ cạnh ghi trong giai đoạn thưa phần lớn phân rã trước khi dùng được ⇒ "ghi sớm để dựng corpus" **không mua được gì** | ≥150 fact sống. Khi làm: đổi `live` **cùng một PR**, thêm chốt "từ chối ghi nếu pruned/before > 0.5", backup `graph.tsv.bak` |
| **§8.4 — journal `secretary.journal`** | Không thể idempotent khi id chưa tất định; một journal replay sai **tệ hơn** không có vì nó khiến người ta bỏ qua việc kiểm tra | Không cần nữa: id tất định + `supersedes:` một-lần-ghi đã đóng cửa sổ hỏng |
| **Nửa âm của M3** (`unusedStreak`, `pendingSince` trên đĩa, reaper, `demerit_unused`, `M(e)`) | Nửa đĩa vô dụng (fast-path dedup-theo-ngày làm `pendingSince` chỉ ghi 1 lần/ngày); nửa công thức **giết đúng ràng buộc đứng** của user; và cái giá của việc **không** có nó chỉ là một fact vô dụng sống thêm vài tuần rồi tự chìm theo `H=30d` | Phase 5 đo được thật sự có fact bơm-mà-vô-dụng; khi đó dùng `M = 1/(1+β·max(0, d−3))` |
| **§4.4 bước 2 — "đảm bảo phủ tầng"** | Fact `user` đã ở frozen core (lane được cache) ⇒ bơm lại là trả tiền hai lần; đi vòng qua bộ lọc `exclude` của search; và là nguồn của vòng xoáy trừ điểm | Không tái mở — vấn đề nó định giải đã do frozen core theo tier (Phase 1) giải xong |
| **§2.6 `anchorLabel`** | Nguồn là `git_remote_origin` ⇒ **spawn git trên đường ghi fact**, đúng thứ chi phí mà trục path tự hào là tránh được; và tự nhận "CHỈ GHI, CHƯA KHỚP" | Khi làm matcher phụ: lấy từ `identity()`/session provenance **đã memo sẵn**, hoặc backfill một lần trên vài trăm file |
| **§1.1 `originDevice`** | Tự khai "không bao giờ lọc" ⇒ không có người đọc, mà lại tăng bề mặt của chính cái bẫy `render_learned` | Khi có tính năng sync/merge nhiều máy |
| **`MemView::Tier(..)` / `Under(..)`** | `All` + lọc phía CLI phủ hết nhu cầu "tại sao fact này không hiện"; mỗi biến thể của điểm chặn duy nhất là một nhánh nữa phải test đúng | Khi có người dùng thật đòi |
| **`aizen skill retier` (13 skill)** | Skill là trụ **duy nhất** đang có dữ liệu user thật và đang hoạt động **đúng** (13 skill hiện đúng khi cwd dưới `C:\Users\admin`) ⇒ đụng vào là đổi rủi ro mất mát thật lấy cải thiện thẩm mỹ. Thêm: đích đến của tier `device`/`place` cho skill **chưa được định nghĩa layout** ⇒ không viết được migration đúng | Sau khi trục tier của memory chạy thật ≥2 tuần. Khi làm: retier = `fs::rename` (không write+delete), `.archive/` rename theo, đích tường minh (`--anchor`), **không** tái dùng `zones::apply` (đích của nó phụ thuộc cwd lúc gõ lệnh — chính cơ chế đã sinh ra bệnh) |
| **`aizen zone migrate` làm nơi retag memory** | `zones::plan()` lấy đích từ `project_slug()` ⇒ **chạy hai lần từ hai thư mục cho hai kết quả khác nhau**, không idempotent theo cwd | Không tái mở — với 0 entry thì không có gì để retag; luật `infer_tier` (đọc khoan dung) đã phủ mọi install khác |
| **Tách `ThreatVerdict` thành `Reject`/`Quarantine`** | Một tầng phân loại + một đích lưu + một bất biến mới, để chữa **một regex bắt nhầm**; và đích lưu là thư mục chưa ai có thói quen đọc ⇒ hành vi quan sát được **giống hệt vứt đi** | Không tái mở — đã thay bằng thu hẹp `RE_ROLEPLAY` |
| **`aizen persona harvest` bản đầy đủ** | Giữ, nhưng **tối giản**: dry-run mặc định, 1 lời gọi model theo lô, chỉ thêm khoá `harvestedTo` (để không harvest lại) và loại insight đó khỏi `self_block`. Không có UI, không có cấu hình. Phần **bền vững** của B4 là routing (`"dawn muốn X"` → `facts` tier=user) + `known_facts` trong `build_reflection_prompt`, và cả hai nằm trong Phase 3 | — |
| **Sửa 40/40 bão hoà bằng mô hình strength cho self-mem** | Nguyên nhân gốc là reflector **không nhìn thấy cái nó đã biết** (`build_reflection_prompt` không có tham số facts). Phase 3 thêm `known_facts` — đó là 80% giá trị với 5% công. Thay cap cứng 40 bằng strength là việc riêng | Sau khi `known_facts` chạy 2 tuần mà vẫn bão hoà |
| **Mở rộng khoá cho read-modify-write** (race R2) | Đụng 4 hàm nóng, là một PR riêng; thiệt hại thực tế chỉ là **trễ nhịp** tăng nửa đời | Khi `doctor` đo được mất mát confirmation đáng kể |

---

## 10. Phiên bản tối thiểu khả dụng (30% công sức)

Nếu chỉ có ~5–6 ngày thay vì 17–20:

| Ưu tiên | Làm gì | Công | Giá trị |
|---|---|---|---|
| **1 (tuyệt đối)** | **Phase 0 trọn vẹn** | 1 ngày | **Dừng đường mất dữ liệu duy nhất đang chạy.** 6% công sức, và là thứ duy nhất trong plan mà **không làm là mất vĩnh viễn**. Nếu chỉ có MỘT ngày, làm đúng cái này |
| **2** | **Phase 1** trọn vẹn | 4–5 ngày | Trục tier/anchor + **một cửa ghi có kiểm tra**. Đây là thứ B9 nói phải làm **trước** khi store đầy — làm sau = di trú hàng trăm fact đã xếp sai chỗ |
| **3** | **Phase 3 rút gọn**: thư ký chỉ ghi `facts` + `used` → `confirmations`; **bỏ** M2a, **bỏ** M1, **bỏ** xoá `extract_free.rs` (chỉ tắt nhánh ghi của nó bằng cờ) | +2 ngày | Store **bắt đầu đầy** với tier đúng. M1/M2 có thể thêm sau mà không phải di trú, vì `confirmations`/`lastUsed`/`supersedes` đã có sẵn trong schema từ Phase 1 |

**Cắt khỏi MVP:** Phase 2 (đọc tự động), Phase 4, Phase 5. Hệ quả phải chấp nhận: bộ nhớ vẫn chỉ xuất hiện khi model tự gọi `memory_search` (bất đối xứng A6 chưa được sửa), và mâu thuẫn khác chữ chưa được bắt.

**Vì sao thứ tự này chứ không phải "thư ký trước cho nhanh thấy kết quả":** với 0 entry, đổi phân vùng **bây giờ** tốn 0 fact di trú. Ship thư ký trước ⇒ vài trăm fact ghi vào mô hình `scope` cũ ⇒ mọi thứ Phase 1 định làm biến thành một đợt di trú thật trên dữ liệu thật. Đó chính là câu B9: **partition first, fill second.**

---

### Phụ lục — ba thứ CỐ Ý giữ lại, ngược với chữ "replacing today's tangle" của B1

- **`type:` (mtype)** — giữ trên đĩa, **tước quyền lực**. Sau thiết kế này nó không còn quyết định scope, không còn quyết định core eligibility; chỉ còn là **nhãn hiển thị + filter của `memory_list`** và là dấu hiệu quyền sở hữu B4 (`feedback` = fact về user do persona nhả ra). Bỏ hẳn = vỡ 4 schema tool + `render.rs` + `inventory` mà không đổi lại được gì.
- **`dimension` / `category`** — vốn **không lưu đĩa** (dẫn xuất lúc load), không tham gia ranking, không tham gia injection. Chi phí giữ = 0; bỏ đi vỡ hàng chục test mà không lợi gì.
- **`scope:` / `subpath:`** — giữ nguyên trên đĩa sau di trú, **không ai đọc nữa**. Đó là **sợi dây rollback**: gỡ `tier:`/`anchor:` ra là file quay về schema cũ nguyên vẹn.
