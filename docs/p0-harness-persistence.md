# P0 — Harness Persistence (anti early-exit · confidence · hill-climb)

> Design doc implementable. Neo code thật (`src/agent/mod.rs`, `todo.rs`, `verify_gate.rs`, `bench/loop_eval.rs`).
>
> **Không** làm lại: verify re-fire (W8), divergence/thrash (P1), no-op write, sub-agent verify,
> todo recitation mid-loop, compaction cadence (P-loop2). Những cái đó **đã có**.
>
> **Mục tiêu:** cùng model, harness kéo điểm trên (1) dừng sớm khi todo còn dở, (2) tuyên bố
> chắc chắn không có bằng chứng, (3) task đo được mà không iterate metric.
>
> Ràng buộc cứng: pure-Rust, single binary, rustls-only, không C-dep, không `cargo clean`.
> Mọi gate best-effort, có cap, có test scripted trong `bench/loop_eval.rs` + unit test.

---

## 0. Hiện trạng (baseline — đừng đụng nhầm)

| Cơ chế | Hành vi | Neo |
|--------|---------|-----|
| Done | `tool_calls.is_empty()` → (verify?) → (self_review?) → `StopReason::Done` | `mod.rs` ~L810–960 |
| Verify gate | Chỉ khi `made_any_edits`; re-fire đến pass / `max_verify_attempts`; latch `verify_passed` | `mod.rs` + `verify_gate.rs` |
| Todo recitation | Nhắc list mỗi `todo_reminder_every` **giữa** loop, **không** chặn Done | `mod.rs` ~L1137 |
| Anti-loop | signature ring, 2-cycle, thrash, unproductive streak | `mod.rs` P1 |
| StopReason | `Done \| Divergence \| MaxIters \| VerificationFailed \| AwaitingInput \| Cancelled` | `mod.rs` L379+ |

**Lỗ P0 thật:**

1. Model có thể `todo_write` list 5 item, làm 2, rồi text-only → **Done** dù còn `pending`/`in_progress`.
2. Không có tín hiệu “tôi chắc chắn thế nào” trên goal; không re-check khi nhảy confidence.
3. Task optimize/bench không bị ép metric quantifiable + iterate.

---

## 1. Phạm vi P0 (4 deliverable)

| ID | Tên | Một câu |
|----|-----|---------|
| **P0.1** | Incomplete-todo gate | Text-only + todo chưa xong → inject poke, **không** Done (có cap). |
| **P0.2** | Confidence on todos | Todo item có `confidence` optional; spike lớn → force re-verify path. |
| **P0.3** | Hill-climbable goals | Khi goal đo được (hoặc model tự chấm thấp), ép metric + iterate đến plateau/budget. |
| **P0.4** | Eval scenarios | 6+ scenario scripted trong `loop_eval` chứng minh 1–3. |

Thứ tự ship: **P0.1 → P0.4(smoke) → P0.2 → P0.3 → P0.4(full)**.

---

## 2. P0.1 — Incomplete-todo gate (“auto-poke”)

### 2.1 Semantics

Trên nhánh **text-only** (sắp Done), **sau** verify gate + self-review (nếu có), **trước** `return Done`:

```
if enable_todo_poke
   && poke_attempts < max_todo_poke_attempts
   && todos_incomplete(snapshot())
then
    record premature assistant text
    inject user poke message
    poke_attempts += 1
    continue loop
else
    Done  // kể cả còn incomplete nếu hết budget poke
```

**Incomplete** = tồn tại item `status ∈ {Pending, InProgress}`.

List **rỗng** → không poke (task trivial / model không dùng todo — giữ behavior cũ).

### 2.2 Config (`AgentConfig`)

```rust
/// When the model returns text-only while the session todo list still has
/// pending/in_progress items, inject a poke and continue (jcode-style
/// anti early-exit). `0` disables.
pub max_todo_poke_attempts: usize,  // default: 2

/// Master switch (default true). Sub-agents with ScopedTodo can opt in later;
/// process-global TODOS only for v1.
pub enable_todo_poke: bool,         // default: true
```

Default gợi ý: `max_todo_poke_attempts = 2` (đủ 1–2 vòng hoàn tất, không burn max_iters).

Sub-agent v1: **OFF** (`enable_todo_poke: false` trong `task_tool` / `workflow`) — ScopedTodo
không share process-global; poke global sẽ sai. Follow-up: poke trên `ScopedTodo` snapshot.

### 2.3 Poke message (ổn định — cache-friendly suffix)

Prefix cố định để `push_nudge` / dedup nếu cần:

```text
[todo-poke] Session todos are still incomplete — you may not finish yet.

Incomplete:
[>] implement parser
[ ] wire tests

Either (a) finish the remaining items and verify, or (b) mark items done only
if genuinely complete, or (c) explicitly abandon with one line why — then stop.
Attempt {n}/{max}.
```

- Cap list ~600 chars (giống recitation).
- **Không** xóa premature assistant text khỏi history (giống verify-fail path).

### 2.4 Tương tác với verify / self-review

Order trên text-only branch (giữ thứ tự hiện tại, chèn poke **cuối**):

```
1. verify gate (edits + !passed + attempts left)  → may continue
2. verify exhausted                               → VerificationFailed (unchanged)
3. self_review (opt-in)                           → may continue
4. **NEW** todo poke                              → may continue
5. Done
```

Lý do: bản build hỏng quan trọng hơn todo; self-review sửa diff trước khi ép tiếp todo.

### 2.5 Escape hatches (tránh kẹt)

Model được phép Done sau poke nếu:

| Điều kiện | Hành vi |
|-----------|---------|
| Mọi item `Done` | Done bình thường |
| `poke_attempts >= max` | Done + (optional) trace `→ todo-poke: exhausted, N incomplete` |
| List cleared (`todo_write` `[]`) | Coi như abandon plan → Done OK |
| User `/clear` | clear todos (đã có) |

**Không** parse NLP “I abandon…” — chỉ tin **todo state** hoặc hết budget. Tránh brittle.

### 2.6 Implementation sketch

**File:** `src/agent/todo.rs`

```rust
pub fn incomplete_summary(max_chars: usize) -> Option<String> {
    let items = snapshot();
    let open: Vec<_> = items.iter()
        .filter(|t| t.status != Status::Done)
        .collect();
    if open.is_empty() { return None; }
    // format [>]/[ ] lines, truncate max_chars
    Some(...)
}

pub fn has_incomplete() -> bool {
    snapshot().iter().any(|t| t.status != Status::Done)
}
```

**File:** `src/agent/mod.rs` — locals trong `run_agent_loop`:

```rust
let mut todo_poke_attempts = 0usize;
// ... inside text-only, after self_review block, before final push:
if cfg.enable_todo_poke
    && todo_poke_attempts < cfg.max_todo_poke_attempts
{
    if let Some(summary) = todo::incomplete_summary(600) {
        // push assistant premature + user poke
        todo_poke_attempts += 1;
        if !cfg.quiet {
            emit_trace(&format!(
                "→ todo-poke: incomplete (attempt {}/{})",
                todo_poke_attempts, cfg.max_todo_poke_attempts
            ));
        }
        iter += 1;
        continue;
    }
}
```

### 2.7 Tests

| Test | Expect |
|------|--------|
| `todo_poke_blocks_done_with_pending` | script: todo_write 2 pending → final text → loop continues → sees poke user msg |
| `todo_poke_allows_done_when_all_done` | all Done → StopReason::Done, poke_attempts=0 |
| `todo_poke_exhausted` | max=1, still incomplete after poke → second text-only → Done |
| `todo_poke_disabled` | enable=false → Done ngay dù pending |
| `todo_poke_empty_list` | no todos → Done |

Thêm scenario `loop_eval`: `early_exit_with_todos`.

---

## 3. P0.2 — Confidence on todos + spike gate

### 3.1 Schema extension (backward compatible)

```json
{
  "content": "optimize float-print",
  "status": "in_progress",
  "confidence": 40
}
```

- `confidence`: optional `u8` 0–100 (serde default = omit / `None`).
- Todo cũ không field → `None` → **không** arm spike gate.
- `todo_write` parse: reject >100 hoặc <0 bằng clamp + warn trong ack, không hard-fail cả list.

```rust
pub struct Todo {
    pub content: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
}
```

### 3.2 Spike detection

Giữ **per-content-key** last confidence trong loop locals (không process-global):

```rust
// key = normalized content string
let mut conf_last: HashMap<String, u8> = HashMap::new();
```

Khi `todo_write` thành công (hook sau tool exec, hoặc scan snapshot mỗi turn có todo_write):

```
for item in snapshot:
  if let Some(c) = item.confidence:
    if let Some(prev) = conf_last.get(content):
      if item.status == Done && c >= conf_high && (c - prev) >= conf_spike_delta:
        arm confidence_gate = true  // one-shot until cleared
    conf_last.insert(content, c)
```

**Defaults:**

```rust
pub conf_high: u8,           // default 90 — "done + very sure"
pub conf_spike_delta: u8,    // default 40 — jump from ≤50 to ≥90 in one update
pub enable_confidence_gate: bool, // default true
```

Chỉ arm khi **status → Done** (hoặc already Done) kèm spike — tránh noise lúc assign thấp.

### 3.3 Confidence gate action

Khi text-only và `confidence_gate_armed && !confidence_gate_cleared`:

1. Nếu `made_any_edits && !verify_passed` → để verify path xử lý (đã có).
2. Else inject **một** user message:

```text
[confidence-gate] You marked todo(s) done with a large confidence jump
(e.g. 40 → 100) without stepwise evidence.

Before finishing: re-run the relevant check (tests / verify / metric).
If checks pass, keep Done. If not, reopen the todo and fix.
This gate fires once per run.
```

3. Set `confidence_gate_cleared = true` (one-shot — không loop vô hạn).
4. `continue` loop.

**Không** bắt buộc parse test output. Gate = **1 extra turn** ép model tự verify; sau đó tin Done (verify gate vẫn cover compile).

### 3.4 Prompt (system_prompt.md — đoạn ngắn)

Thêm ~80–120 tokens vào khối Operating loop / todo:

```text
When using todo_write on non-trivial work, set confidence 0–100:
- at assignment (honest prior),
- when marking done (posterior).
Prefer stepwise rises (tests passing → +). A jump of ≥40 straight to ≥90
when marking done will trigger a one-shot re-check. Omit confidence on
trivial one-step tasks.
```

Giữ prompt gọn — không copy jcode essay.

### 3.5 Tests

| Test | Expect |
|------|--------|
| `confidence_spike_arms_gate` | 40→100 + Done status → next text-only gets gate msg once |
| `confidence_omitted_no_gate` | no field → no gate |
| `confidence_stepwise_no_gate` | 40→60→85→95 → no spike arm |
| `confidence_gate_once` | after one inject, second Done proceeds |

---

## 4. P0.3 — Hill-climbable goals

### 4.1 Mục tiêu hẹp (v1)

**Không** build framework bench đầy đủ. v1 = harness **nudge + optional structured field**.

### 4.2 Detection — khi nào ép

Arm hill-climb mode khi **một** trong:

**A. User/task signal (rẻ, precision cao)**  
User message hoặc active todo content match (case-insensitive):

```
optimize | benchmark | perf | latency | throughput | reduce | minimize | maximize
| score | hill-climb | faster | smaller binary | fewer allocations
```

Regex word-boundary, list config-able sau.

**B. Model self-score (opt-in tool field)**  
Extend `todo_write` item:

```json
"hill_climbable": 0-100
```

Nếu `hill_climbable < hill_climb_gate` (default **90**) trên item `in_progress`/`pending` → inject reframe nudge **một lần**.

### 4.3 Hành vi khi armed

**Nudge 1 — reframe (one-shot per run):**

```text
[hill-climb] This goal looks quantifiable. Before more edits, state:
1) metric (e.g. ns/op, pass count, binary KB),
2) baseline measurement command,
3) target direction (higher/lower).
Then iterate: measure → change → measure. Stop when plateau or budget.
```

**Nudge 2 — mid-run (optional, cadence)**  
Mỗi `hill_climb_reminder_every` iters (default 6) khi mode on và còn incomplete:

```text
[hill-climb] Re-measure the metric before claiming progress. No metric delta → try a different approach or stop.
```

Dùng `push_nudge` (replace same prefix), không accrete.

### 4.4 Config

```rust
pub enable_hill_climb: bool,              // default true
pub hill_climb_gate: u8,                  // default 90
pub hill_climb_reminder_every: usize,     // default 6; 0 = reframe only
```

### 4.5 Không làm ở v1

- Không tự chạy benchmark framework.
- Không chặn Done chỉ vì “chưa improve metric” (quá brittle không có parser metric).
- Không swarm.

Done vẫn qua P0.1 + verify. Hill-climb = **orientation**, không hard gate metric parser.

### 4.6 Tests

| Test | Expect |
|------|--------|
| `hill_climb_keyword_reframe` | user_task chứa "optimize" → first tool round sees reframe nudge |
| `hill_climb_low_self_score` | todo hill_climbable=70 → reframe once |
| `hill_climb_disabled` | flag off → no nudge |

---

## 5. P0.4 — Eval harness extensions

File: `src/bench/loop_eval.rs` (+ scenario table).

| Scenario id | Shape | Pass criteria |
|-------------|-------|---------------|
| `poke_blocks_early_done` | todo pending → text done → (script continues work) → all done | ≥1 poke inject; final StopReason::Done; todos clear in script |
| `poke_exhausted` | pending forever, max_poke=1 | Done after 1 poke; stop Done not hang |
| `confidence_spike_recheck` | spike then text | exactly 1 confidence-gate user msg |
| `no_poke_without_todos` | edit + verify pass + no todos | Done, 0 poke |
| `hill_climb_reframe` | task "optimize X" | reframe prefix present in messages |
| `verify_still_outranks_poke` | broken build + pending todos | verify fail inject before poke (order) |

Metrics in summary line (reuse existing):

- `todo_poke_rate` = runs with ≥1 poke / runs with incomplete-at-first-done-attempt  
- `early_exit_blocked` count  
- `confidence_gate_fires`  
- keep `verified-done`, steps/task

Gate CI: scenarios must PASS; không cần baseline % model thật.

---

## 6. StopReason / observability (optional nhỏ)

**Không bắt buộc** variant mới. Prefer:

- quiet=false trace lines: `→ todo-poke`, `→ confidence-gate`, `→ hill-climb`
- Optional later: `StopReason::Done` + flags trên `AgentOutcome`:

```rust
pub struct AgentOutcome {
    pub final_text: Option<String>,
    pub iters: usize,
    pub stop: StopReason,
    // NEW optional diagnostics (default 0)
    pub todo_pokes: usize,
    pub confidence_gates: usize,
}
```

Chỉ thêm field nếu caller (`main`, task_tool) không phá; nếu noisy → chỉ trace.

---

## 7. system_prompt.md delta (tối thiểu)

Chèn vào Operating loop / Definition of done (~150 tokens total):

```markdown
## Persistence (harness-enforced)
- If you created todos, do not finish while any item is pending/in_progress.
  The harness will poke you back; use that turn to complete, reopen honestly, or clear the list.
- On non-trivial todos, set confidence at assign and at done; avoid huge jumps to 90+ without checks.
- Quantifiable goals (optimize/benchmark/perf): state metric + baseline command, then measure/change/measure.
```

Đồng bộ `system_prompt_strict.md` nếu còn dùng.

---

## 8. File touch list

| File | Change |
|------|--------|
| `src/agent/todo.rs` | `confidence`, `hill_climbable?`, `has_incomplete`, `incomplete_summary`, parse/schema |
| `src/agent/mod.rs` | AgentConfig fields; poke + confidence + hill-climb in loop; defaults; unit tests |
| `src/agent/task_tool.rs` | `enable_todo_poke: false` (v1) |
| `src/agent/workflow.rs` | same |
| `src/agent/system_prompt.md` | persistence paragraph |
| `src/agent/system_prompt_strict.md` | sync |
| `src/bench/loop_eval.rs` | 6 scenarios + metrics |
| `docs/aizen-improvement-plan.md` | link P0-persist status (1 đoạn) |
| `docs/p0-harness-persistence.md` | **this file** |

**Không** đụng: `verify_gate.rs` logic (chỉ order call site), cmd_guard, memory, hostbot.

---

## 9. Rollout

### Phase A — P0.1 only (1 PR)

1. Config + `has_incomplete` / summary  
2. Gate in mod.rs  
3. 4 unit tests + 2 loop_eval  
4. `cargo test` + `aizen bench loop` green  
5. Manual: task 3-step với todo, cố “xong sớm” → thấy poke

### Phase B — P0.2

1. Schema confidence  
2. Spike map + one-shot gate  
3. Prompt delta  
4. Tests  

### Phase C — P0.3

1. Keyword arm + reframe nudge  
2. Optional hill_climbable field  
3. Tests  

### Phase D — measure

Chạy 10 task nội bộ (scripted + 3 live nếu có key):

| Metric | Baseline (ước) | Target |
|--------|----------------|--------|
| Early-done with open todos | >0 trên multi-step | **0** (blocked or exhausted) |
| Steps/task multi-step | — | không tăng >20% median |
| Verify-pass rate | giữ | không regress |
| loop_eval | 15/15 | **21/21** ( +6 ) |

---

## 10. Risks & mitigations

| Risk | Mitigation |
|------|------------|
| Poke kẹt task “todo để note” không định hoàn | max attempts + clear list escape; trivial tasks không cần todo (prompt đã nói) |
| Confidence spam | one-shot; only on Done+spike; omit OK |
| Hill-climb false positive (“optimize import order”) | keyword list chặt; reframe only not hard block |
| Sub-agent false poke via global TODOS | poke OFF for task/workflow v1 |
| Prompt cache bust | fixed prefix strings; push_nudge replace; conf/hill inject rare |
| Double-inject verify+poke same turn | order: verify first; poke only if still on Done path |

---

## 11. Explicit non-goals (P0)

- Swarm / multi-session jcode  
- Auto-restart Telegram daemon (P1 ops)  
- Provider switch UI  
- Desktop TUI parity  
- Metric parser hard-fail Done  
- Serena bundling  
- Changing default `max_iters` / verify command detection  

---

## 12. Definition of done (doc này)

- [x] P0.1 merged in tree, tests green (`todo_poke_*`)
- [x] P0.2 in tree, tests green (`confidence_spike_*`, `confidence_stepwise_*`)
- [x] P0.3 in tree, tests green (`hill_climb_keyword_reframe`, keyword unit)
- [x] `aizen bench loop` — **19/19 PASS** including persist shape (4 scenarios: poke_blocks_early_done, no_poke_without_todos, confidence_spike_recheck, hill_climb_reframe)
- [x] Sub-agents: `enable_todo_poke/confidence/hill_climb = false` in `task_tool.rs` + `workflow.rs`
- [x] system_prompt.md + system_prompt_strict.md persistence paragraph present
- [ ] Optional: install release binary over `~/.cargo/bin/aizen.exe` when dawn wants it live
- [ ] Optional: one-line status blurb in `docs/aizen-improvement-plan.md`

**Verified (2026-07-19):** `cargo test todo_poke|confidence_|hill_climb` OK; `aizen bench loop` → LOOP EVAL PASS (19/19), verified-done 100%, mean steps 3.3.
  

---

## 13. Appendix — pseudo-code Done branch (after P0)

```text
on tool_calls.is_empty():
  if verify_needed: handle verify; maybe continue
  if verify_exhausted: return VerificationFailed
  if self_review_needed: handle; maybe continue

  if enable_todo_poke && poke_attempts < max && has_incomplete():
     inject todo-poke; poke_attempts++; continue

  if enable_confidence_gate && armed && !cleared:
     inject confidence-gate; cleared=true; continue

  // hill-climb does NOT hard-block Done in v1

  return Done
```

Hill-climb nudges live on **tool-call path** (start + cadence), not only Done — để định hướng sớm.

---

*Author: Kira (design). Implement against tree at Desktop/mini_project/aizen.  
Related: `docs/aizen-improvement-plan.md` (P0–P4, P-ctx, P-loop2 already applied).*
