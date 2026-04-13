# Live Captions + Display Mode Toggle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render Sherpa `TranscriptionEvent::Partial` events as a live caption line in the transcript panel, with a user-selectable display mode toggle between "Live captions" and "Final only".

**Architecture:** A new dimmed-italic `gtk4::Label` (the "live line") sits below the existing `TextView` inside the transcript panel's content box. `Partial` events update the live line text in place; `Text` events clear it and append the committed line to the text view (existing behavior). A new `AdwComboRow` exposes display-mode selection; "Final only" hides the live line entirely and drops incoming `Partial` events. The entire live-captions stack is `#[cfg(feature = "sherpa")]` because Whisper never emits `Partial`. Persistence uses a new config key `transcription_display_mode`.

**Tech Stack:** Rust 2024, gtk4-rs 0.11, libadwaita, existing `TranscriptionEvent` / `TranscriptionBackend` trait in `sdr-transcription`, `sdr_config::ConfigManager` for persistence.

---

## File Structure

- **Modify:** `crates/sdr-ui/src/sidebar/transcript_panel.rs` — add `KEY_DISPLAY_MODE` const, display-mode constants, extend `TranscriptPanel` struct (sherpa-gated fields), build the ComboRow and live-line Label in `build_transcript_panel`, wire ComboRow persistence + live-line visibility toggle.
- **Modify:** `crates/sdr-ui/src/window.rs` — in `connect_transcript_panel`, clone/downgrade the new widgets, update the `Partial` event handler to write the live line (sherpa-only) gated on current display mode, update `Text` to clear the live line (sherpa-only), lock the display-mode ComboRow while transcription runs, unlock on stop/error.

That's it — two files. The backend already emits partials correctly; no changes in `sdr-transcription`.

---

## Task 1: Add config key, display-mode constants, and extend struct

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/transcript_panel.rs`

- [ ] **Step 1: Add the `KEY_DISPLAY_MODE` constant next to the other config keys**

Insert after the existing `KEY_SHERPA_MODEL` const (around line 21):

```rust
#[cfg(feature = "sherpa")]
/// Config key for the persisted transcript display mode.
/// Values: `"live"` (default) or `"final"`.
const KEY_DISPLAY_MODE: &str = "transcription_display_mode";

#[cfg(feature = "sherpa")]
const DISPLAY_MODE_LIVE_IDX: u32 = 0;
/// `pub(crate)` so `window.rs` can gate the `Partial` handler on it.
#[cfg(feature = "sherpa")]
pub(crate) const DISPLAY_MODE_FINAL_IDX: u32 = 1;
#[cfg(feature = "sherpa")]
const DISPLAY_MODE_LABELS: &[&str] = &["Live captions", "Final only"];
```

- [ ] **Step 2: Extend the `TranscriptPanel` struct with sherpa-gated fields**

Add these fields to the struct (after `noise_gate_row` and before `status_label`):

```rust
    /// Display-mode selector (Live captions vs Final only). Sherpa-only —
    /// Whisper has no `Partial` events to render.
    #[cfg(feature = "sherpa")]
    pub display_mode_row: adw::ComboRow,
    /// Dimmed italic label below the text view that renders in-progress
    /// Sherpa partials. Sherpa-only.
    #[cfg(feature = "sherpa")]
    pub live_line_label: gtk4::Label,
```

- [ ] **Step 3: Build workspace to verify the struct compiles**

Run: `cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -20`
Expected: compile errors about missing field initializers in `build_transcript_panel` return value (that's fine — Task 2 will fix it). Whisper build must still compile clean:

Run: `cargo build --workspace 2>&1 | tail -20`
Expected: PASS (whisper build untouched by the `cfg(feature = "sherpa")` gated fields).

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs
git commit -m "feat(ui): add display-mode config key and TranscriptPanel fields

Adds KEY_DISPLAY_MODE + constants and sherpa-gated display_mode_row /
live_line_label fields to TranscriptPanel. Next task builds the actual
widgets in build_transcript_panel."
```

---

## Task 2: Build display-mode ComboRow in `build_transcript_panel`

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/transcript_panel.rs`

- [ ] **Step 1: Build the ComboRow and wire persistence**

Insert this block inside `build_transcript_panel`, immediately after the `noise_gate_row.connect_value_notify(...)` closure and before `let status_label = ...`:

```rust
    // --- Display mode selector (Sherpa only) ---
    //
    // Whisper builds never compile this in — Whisper does not emit
    // `TranscriptionEvent::Partial`, so there's nothing to render in a
    // "live line". Sherpa builds default to "Live captions" because
    // streaming is the whole point; users can switch to "Final only"
    // if the in-place updates are visually distracting.
    #[cfg(feature = "sherpa")]
    let display_mode_row = {
        let list = gtk4::StringList::new(DISPLAY_MODE_LABELS);

        let saved_idx = config.read(|v| {
            v.get(KEY_DISPLAY_MODE)
                .and_then(serde_json::Value::as_str)
                .map_or(DISPLAY_MODE_LIVE_IDX, |s| match s {
                    "final" => DISPLAY_MODE_FINAL_IDX,
                    _ => DISPLAY_MODE_LIVE_IDX,
                })
        });

        let row = adw::ComboRow::builder()
            .title("Display mode")
            .subtitle("Live captions update in place; Final only shows committed text")
            .model(&list)
            .selected(saved_idx)
            .build();
        group.add(&row);

        let config_display = Arc::clone(config);
        row.connect_selected_notify(move |r| {
            let value = match r.selected() {
                DISPLAY_MODE_FINAL_IDX => "final",
                _ => "live",
            };
            config_display.write(|v| {
                v[KEY_DISPLAY_MODE] = serde_json::json!(value);
            });
        });

        row
    };
```

- [ ] **Step 2: Build workspace to verify the closure compiles**

Run: `cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -30`
Expected: still errors about missing field initializers in struct literal (Task 3 will fix live_line_label, Task 4 will fix the return). Continue.

Run: `cargo build --workspace 2>&1 | tail -10`
Expected: PASS (whisper build unaffected).

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs
git commit -m "feat(ui): build display-mode ComboRow for sherpa transcript panel

Persists to KEY_DISPLAY_MODE on change. Defaults to Live captions.
Whisper builds do not compile this in."
```

---

## Task 3: Build the live-line Label and place it in the content box

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/transcript_panel.rs`

- [ ] **Step 1: Build the Label and set its initial visibility from the saved display mode**

Insert this block right after the `display_mode_row` block from Task 2, still before `let status_label = ...`:

```rust
    // --- Live caption line (Sherpa only) ---
    //
    // Dimmed italic label that renders in-progress Sherpa partials.
    // Initially hidden; becomes visible once a Partial event arrives
    // and the current display mode is "Live captions". When display
    // mode is "Final only" the label stays hidden entirely.
    #[cfg(feature = "sherpa")]
    let live_line_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .css_classes(["dim-label"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(2)
        .margin_bottom(4)
        .visible(false)
        .build();

    // Italicize via Pango markup attribute list so we don't need a
    // custom CSS rule. The text is set via set_text() later; the
    // attributes persist across text changes.
    #[cfg(feature = "sherpa")]
    {
        let attrs = gtk4::pango::AttrList::new();
        attrs.insert(gtk4::pango::AttrInt::new_style(gtk4::pango::Style::Italic));
        live_line_label.set_attributes(Some(&attrs));
    }
```

- [ ] **Step 2: Append the label to the content_box between `scroll` and `clear_button`**

Locate the existing block:

```rust
    content_box.append(&status_label);
    content_box.append(&progress_bar);
    content_box.append(&scroll);
    content_box.append(&clear_button);
```

Replace it with:

```rust
    content_box.append(&status_label);
    content_box.append(&progress_bar);
    content_box.append(&scroll);
    #[cfg(feature = "sherpa")]
    content_box.append(&live_line_label);
    content_box.append(&clear_button);
```

- [ ] **Step 3: Extend the Clear button closure to also clear the live line**

Replace the existing `clear_button.connect_clicked` closure:

```rust
    let text_view_clear = text_view.clone();
    clear_button.connect_clicked(move |_| {
        text_view_clear.buffer().set_text("");
    });
```

With this:

```rust
    let text_view_clear = text_view.clone();
    #[cfg(feature = "sherpa")]
    let live_line_for_clear = live_line_label.clone();
    clear_button.connect_clicked(move |_| {
        text_view_clear.buffer().set_text("");
        #[cfg(feature = "sherpa")]
        {
            live_line_for_clear.set_text("");
            live_line_for_clear.set_visible(false);
        }
    });
```

- [ ] **Step 4: Update the struct-literal return at the bottom of `build_transcript_panel`**

Replace the existing return:

```rust
    TranscriptPanel {
        widget: group,
        enable_row,
        model_row,
        #[cfg(feature = "whisper")]
        silence_row,
        noise_gate_row,
        status_label,
        progress_bar,
        text_view,
        scroll,
        clear_button,
    }
```

With:

```rust
    TranscriptPanel {
        widget: group,
        enable_row,
        model_row,
        #[cfg(feature = "whisper")]
        silence_row,
        noise_gate_row,
        #[cfg(feature = "sherpa")]
        display_mode_row,
        #[cfg(feature = "sherpa")]
        live_line_label,
        status_label,
        progress_bar,
        text_view,
        scroll,
        clear_button,
    }
```

- [ ] **Step 5: Build both feature flavors — this task completes the transcript_panel.rs changes**

Run: `cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -20`
Expected: PASS. The sidebar file now compiles clean on sherpa; all window.rs `Partial` handling still just calls `tracing::debug!` so nothing uses the new fields yet.

Run: `cargo build --workspace 2>&1 | tail -10`
Expected: PASS (whisper build unaffected).

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs
git commit -m "feat(ui): add live caption label to transcript panel

Dimmed italic gtk4::Label placed between scroll and clear button.
Hidden by default; visibility is driven by window.rs on Partial events.
Clear button wipes the live line too. Whisper builds compile without it."
```

---

## Task 4: Wire `Partial` event handler to update the live line

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Clone + downgrade the new widgets alongside the existing weak refs**

Locate the weak-ref declaration block near the top of `connect_transcript_panel` (around lines 1494–1508), currently:

```rust
    let progress_bar = transcript.progress_bar.clone();
    let text_view = transcript.text_view.clone();
    let model_row = transcript.model_row.clone();
    #[cfg(feature = "whisper")]
    let silence_row = transcript.silence_row.clone();
    let noise_gate_row = transcript.noise_gate_row.clone();
    // Weak refs used by the async event-loop closure to drive the same
    // teardown the synchronous error path does (see below) when the
    // backend fires TranscriptionEvent::Error mid-session. Weak so the
    // timeout closure doesn't keep widgets alive past their UI lifetime.
    let enable_row_weak = transcript.enable_row.downgrade();
    let model_row_weak = model_row.downgrade();
    #[cfg(feature = "whisper")]
    let silence_row_weak = silence_row.downgrade();
    let noise_gate_row_weak = noise_gate_row.downgrade();
```

Add the sherpa-gated clones and weak refs at the end of that block:

```rust
    #[cfg(feature = "sherpa")]
    let display_mode_row = transcript.display_mode_row.clone();
    #[cfg(feature = "sherpa")]
    let live_line_label = transcript.live_line_label.clone();
    #[cfg(feature = "sherpa")]
    let display_mode_row_weak = display_mode_row.downgrade();
    #[cfg(feature = "sherpa")]
    let live_line_weak = live_line_label.downgrade();
```

- [ ] **Step 2: Clone the new weak refs into the timeout closure**

Locate the per-start weak-ref clone block (around lines 1574–1581), currently:

```rust
                    let status_weak = status_label.downgrade();
                    let progress_weak = progress_bar.downgrade();
                    let tv_weak = text_view.downgrade();
                    let enable_row_weak = enable_row_weak.clone();
                    let model_row_weak = model_row_weak.clone();
                    #[cfg(feature = "whisper")]
                    let silence_row_weak = silence_row_weak.clone();
                    let noise_gate_row_weak = noise_gate_row_weak.clone();
```

Add at the end of that block:

```rust
                    #[cfg(feature = "sherpa")]
                    let display_mode_row_weak = display_mode_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let live_line_weak = live_line_weak.clone();
```

- [ ] **Step 3: Replace the `Partial` event arm with cfg-gated UI updates**

Locate the existing handler (around lines 1613–1624):

```rust
                                    TranscriptionEvent::Partial { text } => {
                                        // PR 4 will render this as a live
                                        // caption line. For the PR 2 spike,
                                        // log only the length — never the
                                        // raw text. Public safety scanner
                                        // content does not belong in logs.
                                        tracing::debug!(
                                            target: "transcription",
                                            partial_chars = text.chars().count(),
                                            "sherpa partial received"
                                        );
                                    }
```

Replace it with:

```rust
                                    TranscriptionEvent::Partial { text } => {
                                        #[cfg(feature = "sherpa")]
                                        {
                                            // Read the current display mode
                                            // from the combo row (the user may
                                            // have changed it mid-session; we
                                            // deliberately don't lock it).
                                            let show_live = display_mode_row_weak
                                                .upgrade()
                                                .is_some_and(|row| {
                                                    row.selected() != DISPLAY_MODE_FINAL_IDX
                                                });
                                            if show_live
                                                && let Some(label) = live_line_weak.upgrade()
                                            {
                                                label.set_text(&text);
                                                label.set_visible(true);
                                            }
                                            // Privacy: never log the raw text.
                                            tracing::debug!(
                                                target: "transcription",
                                                partial_chars = text.chars().count(),
                                                "sherpa partial received"
                                            );
                                        }
                                        #[cfg(not(feature = "sherpa"))]
                                        {
                                            // Whisper never emits Partial, but
                                            // the enum variant is compiled in.
                                            // Defensive no-op.
                                            let _ = text;
                                        }
                                    }
```

Note: this uses `DISPLAY_MODE_FINAL_IDX` which lives in `sidebar::transcript_panel`. The constant is `pub(crate)`-visible through `crate::sidebar::transcript_panel` — add the import next.

- [ ] **Step 4: Import `DISPLAY_MODE_FINAL_IDX` into `window.rs`**

The constant was already declared `pub(crate)` in Task 1. Add the `use` statement near the other `crate::sidebar::...` imports in `crates/sdr-ui/src/window.rs` (cfg-gated):

```rust
#[cfg(feature = "sherpa")]
use crate::sidebar::transcript_panel::DISPLAY_MODE_FINAL_IDX;
```

- [ ] **Step 5: Verify sherpa build compiles**

Run: `cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -20`
Expected: PASS.

Run: `cargo build --workspace 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs crates/sdr-ui/src/window.rs
git commit -m "feat(ui): render sherpa partials on the live caption line

Partial events write into the dimmed italic live line label when
display mode is 'Live captions'. 'Final only' mode drops partials
entirely — only committed Text events appear in history. Privacy
tracing::debug preserved (partial char count only, no raw text)."
```

---

## Task 5: Wire `Text` event handler to clear the live line

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Extend the `Text` arm to clear the live line after committing**

Locate the existing arm (around lines 1625–1632):

```rust
                                    TranscriptionEvent::Text { timestamp, text } => {
                                        let buf = tv.buffer();
                                        let mut end = buf.end_iter();
                                        buf.insert(&mut end, &format!("[{timestamp}] {text}\n"));
                                        let mark = buf.create_mark(None, &buf.end_iter(), false);
                                        tv.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
                                        buf.delete_mark(&mark);
                                    }
```

Replace it with:

```rust
                                    TranscriptionEvent::Text { timestamp, text } => {
                                        let buf = tv.buffer();
                                        let mut end = buf.end_iter();
                                        buf.insert(&mut end, &format!("[{timestamp}] {text}\n"));
                                        let mark = buf.create_mark(None, &buf.end_iter(), false);
                                        tv.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
                                        buf.delete_mark(&mark);

                                        // An utterance committed — the live
                                        // line is now stale. Clear and hide
                                        // it so the next Partial starts fresh.
                                        #[cfg(feature = "sherpa")]
                                        if let Some(label) = live_line_weak.upgrade() {
                                            label.set_text("");
                                            label.set_visible(false);
                                        }
                                    }
```

- [ ] **Step 2: Build both flavors**

Run: `cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10`
Expected: PASS.

Run: `cargo build --workspace 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(ui): clear live caption line on Text commit

When the sherpa recognizer commits an utterance via TranscriptionEvent::Text,
clear and hide the live line so the next Partial starts fresh. Prevents
stale in-progress text from lingering below committed history."
```

---

## Task 6: Lock the display-mode ComboRow during transcription

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

Rationale: matches existing behavior — `model_row`, `silence_row`, `noise_gate_row` are all locked while transcription runs so the user can't change them mid-session. Display mode is safe to change mid-session (the `Partial` arm re-reads it every event), so we actually DO NOT want to lock it. Skip the lock — just verify the current behavior is correct by reading through the start/stop/error paths.

- [ ] **Step 1: Read through the enable_row.connect_active_notify closure and confirm display_mode_row is intentionally NOT locked**

Run: `grep -n "display_mode_row" crates/sdr-ui/src/window.rs`
Expected: matches only in the weak-ref decl block and the Partial handler — no `set_sensitive(false)` call, which is intentional.

This step is the "look, think, confirm" checkpoint. No code change. Document the decision in a single-line comment where the other controls ARE locked.

- [ ] **Step 2: Add an inline comment explaining why display_mode_row isn't locked**

Locate this block in `enable_row.connect_active_notify` (the "on" branch, around line 1514):

```rust
            model_row.set_sensitive(false);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
```

Replace with:

```rust
            model_row.set_sensitive(false);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
            // display_mode_row is intentionally NOT locked — the Partial
            // handler re-reads it on every event, so flipping it mid-session
            // is safe and desirable (user sees effect immediately).
```

- [ ] **Step 3: Build + commit**

Run: `cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10`
Expected: PASS.

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "docs(ui): note that display_mode_row isn't locked mid-session

The Partial handler re-reads the combo row on every event, so flipping
the display mode while transcription runs is safe and produces an
immediate visual effect. Callout comment so future edits don't
'fix' it by adding a lock."
```

---

## Task 7: Full workspace lint + format + test pass for both builds

**Files:** none (verification only)

- [ ] **Step 1: cargo fmt check**

Run: `cargo fmt --all -- --check`
Expected: PASS with no output. If it reports differences, run `cargo fmt --all` and commit the result with message `chore: cargo fmt`.

- [ ] **Step 2: Whisper clippy**

Run: `cargo clippy --all-targets --workspace -- -D warnings`
Expected: PASS.

- [ ] **Step 3: Whisper tests**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all passing.

- [ ] **Step 4: Sherpa clippy**

Run: `cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Sherpa tests**

Run: `cargo test --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -20`
Expected: all passing.

- [ ] **Step 6: Commit any fmt/clippy cleanup if needed**

If fmt or clippy produced fixes, commit them with `chore: cargo fmt + clippy cleanup`. If nothing changed, skip.

---

## Task 8: Manual smoke test (user-executed)

**Files:** none (manual verification)

This task is for the human reviewer. The subagent running this plan should stop here, report "Ready for manual smoke test", and hand off.

- [ ] **Step 1: Sherpa build install**

```bash
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cpu"
```

- [ ] **Step 2: Launch and verify Live captions mode**

- Open the transcript panel in the sidebar
- Verify "Display mode" row appears below "Noise gate", defaulted to "Live captions"
- Enable transcription, wait for `Ready`
- Feed it known audio (RadioReference live stream or a test file)
- Verify: dimmed italic live line appears below the text view and updates in place as partials stream
- Verify: when an utterance commits, the live line clears and a timestamped line appears in the text view

- [ ] **Step 3: Switch to Final only mid-session**

- With transcription still running, switch Display mode to "Final only"
- Verify: any currently-visible live line clears within ~200ms (next Partial with `show_live == false` just hides)
- Verify: committed Text events still appear in the text view normally
- Verify: no live line appears until display mode is switched back

- [ ] **Step 4: Clear button**

- Switch back to "Live captions"
- With a live line visible, click Clear
- Verify: text view AND live line both clear, live line hides

- [ ] **Step 5: Persistence**

- Leave display mode on "Final only", close app
- Relaunch
- Verify: Display mode combo row is still "Final only" (persisted via config)

- [ ] **Step 6: Whisper regression**

```bash
make install CARGO_FLAGS="--release --features whisper-cuda"
```

- Launch, enable transcription
- Verify: Display mode row is NOT present (Whisper build doesn't compile it in)
- Verify: live line is NOT present
- Verify: existing Whisper Text commit behavior unchanged (timestamped lines in text view)

- [ ] **Step 7: Report outcome**

Report to reviewer: both builds tested, all smoke-test steps passed, or list any failures.

---

## Task 9: Open PR

**Files:** none

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feature/live-captions-display-mode
```

- [ ] **Step 2: Open PR via gh CLI**

```bash
gh pr create --title "feat(ui): live captions + display mode toggle (#204 PR 4)" --body "$(cat <<'EOF'
## Summary
- Render Sherpa `TranscriptionEvent::Partial` events as a dimmed italic live-caption line below the transcript text view
- Add a Display Mode combo row to the transcript sidebar: Live captions (default) vs Final only
- Persist the choice via new config key `transcription_display_mode`
- Whisper builds compile cleanly without any of it (`#[cfg(feature = "sherpa")]` gated)

Part of #204 (sherpa-onnx integration epic). This is PR 4 of that roadmap.

## Test plan
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --all-targets --workspace -- -D warnings` (Whisper)
- [x] `cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings` (Sherpa)
- [x] `cargo test --workspace` (both flavors)
- [x] Manual: sherpa build, live captions update in place, commit clears live line
- [x] Manual: switch to Final only mid-session, live line hides, Text commits still render
- [x] Manual: Clear button wipes both text view and live line
- [x] Manual: display mode persists across restart
- [x] Manual: whisper build unaffected (no display mode row, no live line)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 2: Return PR URL to the user**

Report the PR URL so the user can follow CodeRabbit review.
