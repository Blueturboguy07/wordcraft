//! AI tools through publik API or the person's own key, desktop only. The setup flow, the
//! credential file and the HTTP calls live in the `craft-publik` crate (shared by the Craft
//! apps). This module is WordCraft's thin host: the first-launch dialog, File › Options › AI, and
//! the Home tab's AI group (Rewrite, Summarize, Continue).
//!
//! The ribbon buttons run `ui.ai.rewrite`, `ui.ai.summarize` and `ui.ai.continue`, which only
//! open their dialogs. Nothing is sent, and nothing is charged, until the person clicks the
//! dialog's own button. The dialogs read the document through `document.selection` and
//! `document.text`, and write to it through `select.collapse` and `edit.pasteText`, so Undo,
//! Track Changes and protection work as for any paste.
//!
//! Runs without preferences (`WORDCRAFT_NO_PREFS`, agents' and tests' runs) have no AI step:
//! `WordApp::ai` stays `None`, the group and the Options pane are not drawn, and no card opens.

use std::sync::mpsc::{Receiver, TryRecvError};

pub use craft_publik::AppInfo;
use craft_publik::client::{self, Fail, Reply};
use craft_publik::ui::{Action, AiState, spawn};
use egui::{RichText, Ui};
use serde::Serialize;
use serde_json::{Value, json};

use crate::WordApp;
use crate::dialogs::Dialog;

const REWRITE_SYSTEM: &str = "You rewrite text for WordCraft, a word processor. Keep the meaning, the language and the paragraph breaks. Make the text clear and correct. If the person gives an instruction, follow it. Reply with only the rewritten text: no preamble, no quotation marks around it, no markdown.";
const SUMMARIZE_SYSTEM: &str = "You summarize text for WordCraft, a word processor. Write a short summary in the language of the text: at most 120 words, in plain sentences. Reply with only the summary: no preamble, no heading, no markdown.";
const CONTINUE_SYSTEM: &str = "You continue writing for WordCraft, a word processor. You get the text before the cursor. Write what comes next, in the same language, voice and style: at most two short paragraphs, about 100 words. Do not repeat the given text. Reply with only the new text: no preamble, no quotation marks around it, no markdown.";

/// How much text a request carries, so a long document stays a small request. Rewrite never cuts
/// the selection (the answer replaces all of it), so a longer selection is refused instead.
const REWRITE_CHARS: usize = 8_000;
const SUMMARY_CHARS: usize = 24_000;
const CONTINUE_CHARS: usize = 6_000;
/// How long an answer may be (output tokens).
const REWRITE_TOKENS: u32 = 4096;
const SUMMARY_TOKENS: u32 = 1024;
const CONTINUE_TOKENS: u32 = 600;

/// Which AI dialog is showing ([`Dialog::Ai`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Tool {
    /// The setup flow: publik API or the person's own key (first launch, File › Options › AI).
    Setup,
    Rewrite,
    Summarize,
    Continue,
}

impl Tool {
    /// The dialog's name, as `ui.dialog` results and the control channel report it.
    pub fn dialog_name(self) -> &'static str {
        match self {
            Tool::Setup => "aiSetup",
            Tool::Rewrite => "aiRewrite",
            Tool::Summarize => "aiSummarize",
            Tool::Continue => "aiContinue",
        }
    }

    /// The dialog's title (English; the dialog system translates it).
    pub fn title(self) -> &'static str {
        match self {
            Tool::Setup => "AI",
            Tool::Rewrite => "Rewrite",
            Tool::Summarize => "Summarize",
            Tool::Continue => "Continue Writing",
        }
    }
}

pub struct Ai {
    pub state: AiState,
    frames: u32,
    first_run_checked: bool,
    /// Options asked for the setup flow; [`frame`] opens it when no other dialog shows.
    open_setup: bool,
    rewrite: Job,
    summarize: Job,
    cont: Job,
}

/// One tool's request in flight and its last answer.
#[derive(Default)]
struct Job {
    /// Rewrite: how to rewrite the text (optional).
    prompt: String,
    /// The text the request was made for. Rewrite replaces it only while it is still selected.
    source: String,
    /// What the dialog says about the text that went out (it was cut to the limit).
    note: Option<String>,
    result: Option<String>,
    error: Option<String>,
    rx: Option<Receiver<Result<Reply, Fail>>>,
}

impl Job {
    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn poll(&mut self, state: &mut AiState) {
        let Some(rx) = self.rx.take() else { return };
        match rx.try_recv() {
            Ok(Ok(r)) => {
                state.note_reply(&r);
                let text = clean_reply(&r.text);
                if text.is_empty() {
                    self.error = Some(tl!("The answer was empty. Try again.").to_string());
                } else {
                    self.result = Some(text);
                    self.error = None;
                }
            }
            Ok(Err(f)) => {
                state.note_fail(&f);
                // A 402 or a retired key is the notice bar's job (one message, one link); the rest is ours.
                if state.notice.is_none() {
                    self.error = Some(f.message);
                }
            }
            Err(TryRecvError::Empty) => self.rx = Some(rx),
            Err(TryRecvError::Disconnected) => self.error = Some(tl!("The request stopped unexpectedly. Try again.").to_string()),
        }
    }

    fn start(&mut self, ctx: &egui::Context, provider: craft_publik::Provider, system: &'static str, user: String, max_tokens: u32) {
        self.result = None;
        self.error = None;
        let messages = json!([{"role": "system", "content": system}, {"role": "user", "content": user}]);
        self.rx = Some(spawn(ctx, move || client::chat_messages(&provider, messages, max_tokens)));
    }
}

impl Ai {
    pub fn new(app: AppInfo) -> Self {
        Ai {
            state: AiState::new(app),
            frames: 0,
            first_run_checked: false,
            open_setup: false,
            rewrite: Job::default(),
            summarize: Job::default(),
            cont: Job::default(),
        }
    }

    fn job(&mut self, tool: Tool) -> &mut Job {
        match tool {
            Tool::Summarize => &mut self.summarize,
            Tool::Continue => &mut self.cont,
            Tool::Setup | Tool::Rewrite => &mut self.rewrite,
        }
    }
}

/// `PUBLIK_SELF_TEST=1`: provision for real and print one line (publikhq.com/skills/setup.md).
pub fn self_test(app: &AppInfo) -> String {
    craft_publik::self_test(app)
}

/// `ui.ai.rewrite`, `ui.ai.summarize`, `ui.ai.continue`: open the tool's dialog. Nothing is sent
/// until its button is clicked. `None` for any other id.
pub fn command(app: &mut WordApp, id: &str) -> Option<Result<Value, String>> {
    let name = id.strip_prefix("ui.ai.")?;
    let tool = match name {
        "rewrite" => Tool::Rewrite,
        "summarize" => Tool::Summarize,
        "continue" => Tool::Continue,
        _ => return Some(Err(format!("unknown AI command `{name}`; use rewrite, summarize or continue"))),
    };
    if app.ai.is_none() {
        return Some(Err("AI tools are off in this session (WORDCRAFT_NO_PREFS)".into()));
    }
    app.dialog = Some(Dialog::Ai { tool });
    Some(Ok(json!({"dialog": tool.dialog_name()})))
}

/// A ribbon button's description (its tooltip's second line).
pub fn description(id: &str) -> Option<&'static str> {
    match id {
        "ui.ai.rewrite" => Some("Rewrite the selected text through publik API or your own key."),
        "ui.ai.summarize" => Some("Summarize the selected text, or the whole document when nothing is selected."),
        "ui.ai.continue" => Some("Continue writing from the cursor. The text before the cursor is the context."),
        _ => None,
    }
}

/// Once per frame, before drawing: finish background work and open the setup dialog when due.
pub fn frame(app: &mut WordApp) {
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.poll();
    ai.rewrite.poll(&mut ai.state);
    ai.summarize.poll(&mut ai.state);
    ai.cont.poll(&mut ai.state);
    ai.frames = ai.frames.saturating_add(1);
    // The first launch asks once the window is up (a few frames in). "Later" ends that for good.
    if !ai.first_run_checked && ai.frames > 3 {
        ai.first_run_checked = true;
        if ai.state.wants_first_run() || ai.state.owes_first_card() {
            ai.open_setup = true;
        }
    }
    if !ai.open_setup || app.dialog.is_some() {
        return;
    }
    ai.open_setup = false;
    ai.state.begin();
    app.dialog = Some(Dialog::Ai { tool: Tool::Setup });
}

/// Carry out what the shared flow asked for. Returns whether the dialog closes.
fn apply(ctx: &egui::Context, action: Action) -> bool {
    match action {
        Action::None => false,
        Action::OpenUrl(url) => {
            ctx.open_url(egui::OpenUrl::new_tab(url));
            false
        }
        Action::Close => true,
    }
}

// ---- the dialogs ---------------------------------------------------------------------------------

/// The body of an AI dialog ([`Dialog::Ai`]). Returns whether the dialog closes.
pub fn body(app: &mut WordApp, ui: &mut Ui, tool: &mut Tool) -> bool {
    let ctx = ui.ctx().clone();
    let escape = crate::dialogs::cancel_key_pressed(ui);
    let collapsed = app.session.sel.is_collapsed();
    let Some(ai) = app.ai.as_mut() else {
        ui.label(tl!("AI tools are off in this session."));
        return close_row(ui) || escape;
    };
    if *tool == Tool::Setup {
        let action = ai.state.setup_ui(ui);
        return apply(&ctx, action) || escape;
    }
    // A fixed width: the text fields and the answer fill it instead of widening the window.
    ui.set_min_width(480.0);
    ui.set_max_width(480.0);
    let provider = ai.state.provider();
    if !provider.is_set_up() {
        ui.label(tl!("This tool needs AI. Set it up first: publik API (no key to paste) or your own key."));
        ui.add_space(6.0);
        if ui.button(tl!("Set up AI…")).clicked() {
            ai.state.begin();
            *tool = Tool::Setup;
            return false;
        }
        return close_row(ui) || escape;
    }

    let mut start = false;
    let mut put = false;
    let mut close = escape;
    let mut action = Action::None;
    let job = ai.job(*tool);
    match tool {
        Tool::Rewrite => {
            ui.label(tl!("Rewrite the selected text."));
            ui.add_space(4.0);
            ui.add_enabled(
                !job.busy(),
                egui::TextEdit::singleline(&mut job.prompt)
                    .desired_width(f32::INFINITY)
                    .hint_text(tl!("Optional: how to rewrite it (shorter, more formal…)")),
            );
        }
        Tool::Summarize => {
            ui.label(if collapsed { tl!("Summarize the whole document.") } else { tl!("Summarize the selected text.") });
        }
        Tool::Continue | Tool::Setup => {
            ui.label(tl!("Continue writing from the cursor. The text before the cursor goes to the model."));
        }
    }
    ui.add_space(6.0);
    let (verb, working) = match tool {
        Tool::Summarize => (tl!("Summarize"), tl!("Summarizing…")),
        Tool::Continue | Tool::Setup => (tl!("Continue"), tl!("Writing…")),
        Tool::Rewrite => (tl!("Rewrite"), tl!("Rewriting…")),
    };
    ui.horizontal(|ui| {
        if job.busy() {
            ui.spinner();
            ui.label(working);
        } else {
            let ready = *tool != Tool::Rewrite || !collapsed;
            if ui.add_enabled(ready, crate::widgets::primary_button(ui, verb)).clicked() {
                start = true;
            }
            if !ready {
                ui.label(RichText::new(tl!("Select the text to rewrite first.")).weak());
            }
        }
    });
    if let Some(n) = &job.note {
        ui.label(RichText::new(n.as_str()).small().weak());
    }
    if let Some(e) = &job.error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e.as_str());
    }
    if let Some(r) = job.result.clone() {
        ui.add_space(6.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(r.as_str()).selectable(true).wrap());
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let put_label = if *tool == Tool::Rewrite { tl!("Replace selection") } else { tl!("Insert at cursor") };
            if ui.add_enabled(!job.busy(), crate::widgets::primary_button(ui, put_label)).clicked() {
                put = true;
            }
            if ui.button(tl!("Copy")).clicked() {
                ctx.copy_text(r.clone());
            }
        });
    }
    ui.add_space(4.0);
    let bar = ai.state.result_bar(ui);
    if bar != Action::None {
        action = bar;
    }
    close |= close_row(ui);
    if start {
        start_request(app, &ctx, *tool, provider);
    }
    if put && put_result(app, *tool) {
        close = true;
    }
    close | apply(&ctx, action)
}

/// A dialog's Close button, on the right.
fn close_row(ui: &mut Ui) -> bool {
    ui.add_space(8.0);
    let mut close = false;
    // In a row, so the right-to-left layout takes one line and not the rest of the window.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = ui.button(tl!("Close")).clicked();
        });
    });
    close
}

/// The `text` of a command's result (`document.selection`, `document.text`).
fn text_of(r: Result<Value, String>) -> Result<String, String> {
    r.map(|v| v.get("text").and_then(Value::as_str).unwrap_or_default().to_string())
}

/// Read what the tool works on and send the request.
fn start_request(app: &mut WordApp, ctx: &egui::Context, tool: Tool, provider: craft_publik::Provider) {
    let request = match tool {
        Tool::Rewrite => text_of(app.run("document.selection", json!({}))).and_then(|text| {
            let n = text.chars().count();
            if text.trim().is_empty() {
                Err(tl!("Select the text to rewrite first.").to_string())
            } else if n > REWRITE_CHARS {
                Err(crate::i18n::fmt(
                    tl!("The selection has {n} characters. Select at most {max} characters to rewrite."),
                    &[("n", n.to_string().as_str()), ("max", REWRITE_CHARS.to_string().as_str())],
                ))
            } else {
                Ok((text, None))
            }
        }),
        Tool::Summarize => {
            let read = if app.session.sel.is_collapsed() {
                text_of(app.run("document.text", json!({"story": "body"})))
            } else {
                text_of(app.run("document.selection", json!({})))
            };
            read.and_then(|text| {
                if text.trim().is_empty() {
                    return Err(tl!("There is no text to summarize.").to_string());
                }
                let (text, cut) = head(&text, SUMMARY_CHARS);
                let note = cut.then(|| {
                    crate::i18n::fmt(
                        tl!("The text is long. Only its first {max} characters go to the model."),
                        &[("max", SUMMARY_CHARS.to_string().as_str())],
                    )
                });
                Ok((text, note))
            })
        }
        Tool::Continue | Tool::Setup => {
            let before = text_before_caret(app);
            if before.trim().is_empty() {
                Err(tl!("Type some text first. Continue uses the text before the cursor.").to_string())
            } else {
                let (text, cut) = tail(&before, CONTINUE_CHARS);
                let note = cut.then(|| {
                    crate::i18n::fmt(
                        tl!("Only the last {max} characters before the cursor go to the model."),
                        &[("max", CONTINUE_CHARS.to_string().as_str())],
                    )
                });
                Ok((text, note))
            }
        }
    };
    let Some(ai) = app.ai.as_mut() else { return };
    let job = ai.job(tool);
    let (text, note) = match request {
        Ok(r) => r,
        Err(e) => {
            job.result = None;
            job.note = None;
            job.error = Some(e);
            return;
        }
    };
    job.note = note;
    let (system, user, tokens) = match tool {
        Tool::Rewrite => {
            let how = job.prompt.trim();
            let user = if how.is_empty() { format!("Text to rewrite:\n{text}") } else { format!("Instruction: {how}\n\nText to rewrite:\n{text}") };
            (REWRITE_SYSTEM, user, REWRITE_TOKENS)
        }
        Tool::Summarize => (SUMMARIZE_SYSTEM, format!("Text to summarize:\n{text}"), SUMMARY_TOKENS),
        Tool::Continue | Tool::Setup => (CONTINUE_SYSTEM, format!("Text before the cursor:\n{text}"), CONTINUE_TOKENS),
    };
    job.source = text;
    job.start(ctx, provider, system, user, tokens);
}

/// Put the answer into the document: Rewrite replaces the selection it was made for; Summarize and
/// Continue insert at the cursor. Returns whether that worked (the dialog then closes).
fn put_result(app: &mut WordApp, tool: Tool) -> bool {
    let Some((answer, source)) = app.ai.as_mut().and_then(|ai| {
        let job = ai.job(tool);
        job.result.clone().map(|r| (r, job.source.clone()))
    }) else {
        return false;
    };
    let outcome = if tool == Tool::Rewrite {
        // The selection may have moved while the request ran: replace only the text it was made for.
        match text_of(app.run("document.selection", json!({}))) {
            Ok(now) if now == source => app.run("edit.pasteText", json!({"text": paragraphs(&answer, source.contains("\n\n"))})),
            Ok(_) => Err(tl!("The selection changed. Select the same text again, or use Copy.").to_string()),
            Err(e) => Err(e),
        }
    } else {
        app.run("select.collapse", json!({"end": true})).and_then(|_| {
            let text = paragraphs(&answer, false);
            let text = if tool == Tool::Continue && needs_space(char_before_caret(app), text.chars().next()) { format!(" {text}") } else { text };
            app.run("edit.pasteText", json!({"text": text}))
        })
    };
    match outcome {
        Ok(_) => true,
        Err(e) => {
            if let Some(ai) = app.ai.as_mut() {
                ai.job(tool).error = Some(e);
            }
            false
        }
    }
}

/// The story's text from its start to the cursor (the selection's end).
fn text_before_caret(app: &WordApp) -> String {
    let s = &app.session;
    let (_, end) = s.sel.ordered();
    s.doc.copy_range(&s.doc.start_of(end.story), &end).plain_text()
}

/// The character just before the cursor, in its paragraph.
fn char_before_caret(app: &WordApp) -> Option<char> {
    let s = &app.session;
    let (_, end) = s.sel.ordered();
    s.doc.para_at(&end).and_then(|p| p.text.get(..end.off)).and_then(|t| t.chars().last())
}

// ---- text helpers ------------------------------------------------------------------------------

/// The first `max` characters of `text`, and whether it was cut.
fn head(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((i, _)) => (text.get(..i).unwrap_or(text).to_string(), true),
        None => (text.to_string(), false),
    }
}

/// The last `max` characters of `text`, and whether it was cut.
fn tail(text: &str, max: usize) -> (String, bool) {
    let n = text.chars().count();
    if n <= max {
        return (text.to_string(), false);
    }
    match text.char_indices().nth(n - max) {
        Some((i, _)) => (text.get(i..).unwrap_or(text).to_string(), true),
        None => (text.to_string(), false),
    }
}

/// A chat answer without a code fence around it.
fn clean_reply(text: &str) -> String {
    let t = text.trim();
    let mut lines: Vec<&str> = t.lines().collect();
    if lines.first().is_some_and(|l| l.trim_start().starts_with("```")) {
        lines.remove(0);
        if lines.last().is_some_and(|l| l.trim() == "```") {
            lines.pop();
        }
    }
    lines.join("\n").trim().to_string()
}

/// Paragraphs as the document takes them: one line each. Models put a blank line between
/// paragraphs; that becomes an empty paragraph only when the source text had them (`keep_blank`).
fn paragraphs(text: &str, keep_blank: bool) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if keep_blank {
        return text.trim_end().to_string();
    }
    text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join("\n")
}

/// Whether continued text needs a space between the text before the cursor and its first letter.
fn needs_space(before: Option<char>, first: Option<char>) -> bool {
    let (Some(b), Some(f)) = (before, first) else { return false };
    // Chinese, Japanese and Korean text has no spaces between words.
    let wide = |c: char| matches!(c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF);
    !(b.is_whitespace()
        || f.is_whitespace()
        || wide(b)
        || wide(f)
        || matches!(f, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '…' | '’' | '”')
        || matches!(b, '(' | '[' | '{' | '“' | '‘' | '/'))
}

// ---- File › Options › AI -------------------------------------------------------------------------

/// The Options pane's id when the AI step runs in this session.
pub fn pane(app: &WordApp) -> Option<&'static str> {
    app.ai.as_ref().map(|_| "ai")
}

/// File › Options › AI: the provider, sign-in and key controls.
pub fn options_pane(app: &mut WordApp, ui: &mut Ui) {
    let Some(ai) = app.ai.as_mut() else { return };
    ui.add_space(6.0);
    ui.label(RichText::new(tl!("AI")).font(crate::theme::semibold(15.0)));
    ui.add_space(2.0);
    ui.label(RichText::new(tl!("Rewrite, Summarize and Continue (Home tab) run through publik API or your own key.")).small().weak());
    ui.add_space(6.0);
    let mut open_setup = false;
    let action = ai.state.options_ui(ui, &mut open_setup);
    if open_setup {
        ai.open_setup = true;
    }
    apply(ui.ctx(), action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_capped_at_character_boundaries() {
        assert_eq!(head("héllo", 2), ("hé".to_string(), true));
        assert_eq!(head("héllo", 9), ("héllo".to_string(), false));
        assert_eq!(tail("héllo wörld", 5), ("wörld".to_string(), true));
        assert_eq!(tail("ab", 5), ("ab".to_string(), false));
        assert_eq!(tail("日本語のテキスト", 3), ("キスト".to_string(), true));
    }

    #[test]
    fn answers_lose_fences_and_blank_lines() {
        assert_eq!(clean_reply("```\nHello there.\n```"), "Hello there.");
        assert_eq!(clean_reply("```text\nA\nB\n```\n"), "A\nB");
        assert_eq!(clean_reply("  Plain.  "), "Plain.");
        assert_eq!(paragraphs("One.\n\nTwo.\r\n\r\nThree.\n", false), "One.\nTwo.\nThree.");
        assert_eq!(paragraphs("One.\n\nTwo.\n", true), "One.\n\nTwo.");
    }

    #[test]
    fn continued_text_gets_a_space_only_between_words() {
        assert!(needs_space(Some('d'), Some('T')));
        assert!(needs_space(Some('.'), Some('T')));
        assert!(!needs_space(Some(' '), Some('T')));
        assert!(!needs_space(Some('d'), Some(',')));
        assert!(!needs_space(None, Some('T')));
        assert!(!needs_space(Some('本'), Some('の')));
        assert!(!needs_space(Some('('), Some('a')));
    }

    /// Every text drawn in the last of `n` frames (the first frames install fonts and draw nothing).
    fn drawn(ctx: &egui::Context, app: &mut WordApp, n: usize) -> Vec<String> {
        fn texts(shape: &egui::Shape, out: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| texts(s, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for i in 0..n {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(2560.0, 900.0))),
                time: Some(i as f64 * 0.1),
                ..Default::default()
            };
            let mut shown = ctx.run_ui(input, |ui| {
                app.logic(ui.ctx());
                app.ui(ui);
            });
            out.clear();
            for s in &shown.shapes {
                texts(&s.shape, &mut out);
            }
            // Nothing uploads the textures here.
            shown.textures_delta.clear();
        }
        out
    }

    /// With the AI step on, the Home tab has the AI group and File › Options an AI pane; a button
    /// only opens its dialog, and nothing is sent until the dialog's own button is clicked.
    #[test]
    fn the_home_tab_opens_the_ai_dialogs_and_sends_nothing() {
        let dir = std::env::temp_dir().join(format!("wc-ai-ui-{}", std::process::id()));
        let info = AppInfo {
            slug: "wordcraft-ui-test",
            name: "WordCraft",
            version: "0.0.0",
            app_token: "",
            dialects: &["chat_completions"],
            config_dir: Some(dir.clone()),
        };
        let ctx = egui::Context::default();
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_engine::sample::sample_document()), Default::default());
        let without = drawn(&ctx, &mut a, 4);
        assert!(!without.iter().any(|t| t == "Summarize"), "no AI group without the AI step");
        a.ai = Some(Ai::new(info));
        let asks = a.ai.as_ref().is_some_and(|ai| ai.state.wants_first_run() || ai.state.owes_first_card());
        let texts = drawn(&ctx, &mut a, 6);
        for label in ["Rewrite", "Summarize", "Continue"] {
            assert!(texts.iter().any(|t| t == label), "Home › AI › {label} is drawn: {texts:?}");
        }
        // The first launch asks once, by itself, when nothing is set up yet.
        assert_eq!(matches!(a.dialog, Some(Dialog::Ai { tool: Tool::Setup })), asks);
        a.dialog = None;
        assert_eq!(pane(&a), Some("ai"));
        for (id, tool) in [("ui.ai.rewrite", Tool::Rewrite), ("ui.ai.summarize", Tool::Summarize), ("ui.ai.continue", Tool::Continue)] {
            assert_eq!(a.run(id, json!({})).unwrap()["dialog"], tool.dialog_name());
            assert!(matches!(a.dialog, Some(Dialog::Ai { tool: t }) if t == tool));
            drawn(&ctx, &mut a, 3);
            assert!(matches!(a.dialog, Some(Dialog::Ai { tool: t }) if t == tool), "{id}: the dialog stays open");
            let ai = a.ai.as_mut().unwrap();
            assert!(!ai.job(tool).busy() && ai.job(tool).result.is_none(), "{id}: nothing sent");
            a.dialog = None;
        }
        // File › Options › AI shows the shared controls.
        a.run("ui.backstage", json!({"value": true, "page": "options", "pane": "ai"})).unwrap();
        let texts = drawn(&ctx, &mut a, 3);
        assert!(texts.iter().any(|t| t.starts_with("Rewrite, Summarize and Continue")), "{texts:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ai_commands_need_the_ai_step() {
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_engine::sample::sample_document()), Default::default());
        assert!(command(&mut a, "format.bold").is_none(), "other ids pass through");
        let off = a.run("ui.ai.rewrite", json!({})).unwrap_err();
        assert!(off.contains("off in this session"), "{off}");
        assert!(a.dialog.is_none(), "no AI dialog without the AI step");
        assert!(a.run("ui.ai.translate", json!({})).unwrap_err().contains("unknown AI command"));
        assert_eq!(pane(&a), None, "no Options pane without the AI step");
    }
}
