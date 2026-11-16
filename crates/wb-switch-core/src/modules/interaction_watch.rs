//! 交互提醒事件消费：把 [`crate::modules::interaction_hook`] 写下的
//! `~/.wb-switch/interaction-events.jsonl` 增量解析成两类提醒。
//!
//! - `Done`：一轮回复结束（`Stop` / `FinalStop`）；
//! - `Attention`：模型在等你（`PreToolUse` 命中提问工具、`PermissionRequest`、
//!   `Notification` 里的权限 / 空闲提示）。
//!
//! 设计取舍：
//! - **首次运行不回溯**：没有状态文件时直接把 offset 定位到文件末尾，避免装完立刻被
//!   历史事件刷一屏通知。
//! - **按 generation 去重**：同一轮里 `Stop` 与 `FinalStop` 都会到，只有第一次生效。
//! - **只做分类，不决定弹不弹**：是否提醒（开关、前台静默）由宿主层判定，
//!   本模块保证「同一件事只交出去一次」。

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::interaction_hook;

/// watcher 轮询间隔：一次 `stat`，交互事件要求秒级可见。
const POLL_INTERVAL: Duration = Duration::from_millis(1_000);
/// 去重窗口：记住最近多少条「已交出去」的事件键。
const SEEN_LIMIT: usize = 400;
/// 单次最多消费的行数：防止偶发的大文件把一次轮询拖成长任务。
const MAX_LINES_PER_ROUND: usize = 200;
/// 正文摘要上限（按字符，通知正文过长会被系统截断）。
const BODY_LIMIT: usize = 120;
const STATE_FILE_NAME: &str = "interaction-state.json";

/// 提醒类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeKind {
    /// 一轮回复结束。
    Done,
    /// 模型提问 / 等审批。
    Attention,
}

impl NoticeKind {
    fn listener_key(self) -> &'static str {
        match self {
            NoticeKind::Done => "done",
            NoticeKind::Attention => "attention",
        }
    }
}

/// 一条待提醒事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub kind: NoticeKind,
    /// 客户端名称（`WorkBuddy` / `CodeBuddy IDE` / `VS Code` …）。
    pub client: String,
    pub session_id: String,
    /// 项目名（cwd 末段目录），用于区分同一客户端的多个会话。
    pub project: String,
    /// 摘要：完成取最后一条回复，提问取问题文本。
    pub body: String,
    pub at: i64,
}

impl Notice {
    /// 通知标题：`CodeBuddy IDE · 回复完成`。
    pub fn title(&self) -> String {
        let suffix = match self.kind {
            NoticeKind::Done => "回复完成",
            NoticeKind::Attention => "需要你操作",
        };
        format!("{} · {}", self.client, suffix)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatchState {
    version: u32,
    offset: u64,
    #[serde(default)]
    last_event_at: Option<i64>,
    /// 已交出的事件键（session|generation|kind）。
    #[serde(default)]
    seen: VecDeque<String>,
}

impl Default for WatchState {
    fn default() -> Self {
        Self {
            version: 1,
            offset: 0,
            last_event_at: None,
            seen: VecDeque::new(),
        }
    }
}

fn state_path() -> PathBuf {
    store_dir().join(STATE_FILE_NAME)
}

fn read_state(path: &Path) -> Option<WatchState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state(path: &Path, state: &WatchState) {
    if let Ok(content) = serde_json::to_string_pretty(state) {
        let _ = atomic_write(path, &content);
    }
}

/// 正文摘要：取第一行、压缩空白、按字符截断。
fn summary(text: &str, limit: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let squeezed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if squeezed.chars().count() <= limit {
        return squeezed;
    }
    let mut cut: String = squeezed.chars().take(limit).collect();
    cut.push('…');
    cut
}

/// cwd 末段作为项目名（Windows 反斜杠与 POSIX 正斜杠都要认）。
fn project_of(cwd: &str) -> String {
    let trimmed = cwd.trim().trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string()
}

fn client_label(payload: &Value) -> String {
    let raw = payload
        .get("client")
        .and_then(Value::as_str)
        .or_else(|| payload.get("agent_type").and_then(Value::as_str))
        .or_else(|| payload.get("agentType").and_then(Value::as_str))
        .unwrap_or("")
        .to_lowercase();
    match raw.as_str() {
        "workbuddy" => "WorkBuddy",
        "workbuddy-ai" => "WorkBuddy 国际版",
        "codebuddyide" => "CodeBuddy IDE",
        "codebuddycn" => "CodeBuddy CN",
        "codebuddy" => "CodeBuddy",
        "vscode" => "VS Code",
        _ => "AI 客户端",
    }
    .to_string()
}

/// 提问工具名（两家客户端的命名各有变体，统一按后缀匹配）。
fn is_ask_tool(tool: &str) -> bool {
    const ASK_TOOLS: [&str; 6] = [
        "ask_followup_question",
        "request_user_input",
        "request_user_input_async",
        "ask_user_question",
        "RequestUserInput",
        "AskUserQuestion",
    ];
    ASK_TOOLS.iter().any(|name| {
        tool == *name
            || tool.ends_with(&format!("__{name}"))
            || tool.ends_with(&format!(".{name}"))
            || tool.ends_with(&format!(":{name}"))
    })
}

/// 从提问工具的入参里取问题文本。
fn question_text(input: &Value) -> Option<String> {
    if let Some(text) = input.get("question").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return Some(text.trim().to_string());
        }
    }
    if let Some(text) = input.get("prompt").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return Some(text.trim().to_string());
        }
    }
    input
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find_map(|item| item.get("question").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// `Notification` 事件里哪些属于「需要你操作」（其余是普通提示，不打扰）。
fn is_attention_notification(payload: &Value) -> bool {
    let kind = payload
        .get("notification_type")
        .or_else(|| payload.get("notificationType"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if kind == "auth_success" {
        return false;
    }
    if kind == "idle_prompt" {
        return true;
    }
    const KIND_HINTS: [&str; 4] = ["permission", "confirm", "approval", "credential"];
    if KIND_HINTS.iter().any(|hint| kind.contains(hint)) {
        return true;
    }
    let text = format!(
        "{} {}",
        payload.get("message").and_then(Value::as_str).unwrap_or(""),
        payload.get("title").and_then(Value::as_str).unwrap_or("")
    );
    const TEXT_HINTS: [&str; 7] = ["?", "？", "确认", "允许", "拒绝", "credential", "凭证"];
    TEXT_HINTS.iter().any(|hint| text.contains(hint))
}

fn string_field(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 把一行 payload 归类成提醒；不关心的事件返回 None。
fn classify(payload: &Value) -> Option<(NoticeKind, String)> {
    let event = {
        let raw = string_field(payload, "hook_event_name");
        if raw.is_empty() {
            string_field(payload, "hookEventName")
        } else {
            raw
        }
    };
    let body = match event.as_str() {
        "Stop" => {
            let text = summary(&string_field(payload, "last_assistant_message"), BODY_LIMIT);
            if text.is_empty() {
                "本轮回复已结束".to_string()
            } else {
                text
            }
        }
        "FinalStop" => "本轮回复已结束".to_string(),
        "PreToolUse" => {
            let tool = string_field(payload, "tool_name");
            if !is_ask_tool(&tool) {
                return None;
            }
            let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
            let text = question_text(&input)
                .map(|text| summary(&text, BODY_LIMIT))
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "模型在等你回答".to_string());
            return Some((NoticeKind::Attention, text));
        }
        "PermissionRequest" => {
            let text = string_field(payload, "message");
            let text = if text.trim().is_empty() {
                string_field(payload, "title")
            } else {
                text
            };
            let text = summary(&text, BODY_LIMIT);
            return Some((
                NoticeKind::Attention,
                if text.is_empty() {
                    "请求权限，等待你确认".to_string()
                } else {
                    text
                },
            ));
        }
        "Notification" => {
            if !is_attention_notification(payload) {
                return None;
            }
            let text = string_field(payload, "message");
            let text = if text.trim().is_empty() {
                string_field(payload, "title")
            } else {
                text
            };
            let text = summary(&text, BODY_LIMIT);
            return Some((
                NoticeKind::Attention,
                if text.is_empty() {
                    "需要你确认".to_string()
                } else {
                    text
                },
            ));
        }
        _ => return None,
    };
    Some((NoticeKind::Done, body))
}

/// 一行原始文本 → 一条提醒（去重键随事件一起返回）。
fn parse_line(line: &str) -> Option<(String, Notice)> {
    let payload: Value = serde_json::from_str(line.trim_start_matches('\u{feff}')).ok()?;
    let session_id = string_field(&payload, "session_id");
    if session_id.is_empty() {
        return None;
    }
    let (kind, body) = classify(&payload)?;
    let generation = {
        let raw = string_field(&payload, "generation_id");
        if raw.is_empty() {
            string_field(&payload, "generationId")
        } else {
            raw
        }
    };
    let key = format!(
        "{session_id}|{}|{}",
        if generation.is_empty() {
            session_id.as_str()
        } else {
            generation.as_str()
        },
        kind.listener_key()
    );
    let notice = Notice {
        kind,
        client: client_label(&payload),
        session_id,
        project: project_of(&string_field(&payload, "cwd")),
        body,
        at: now_ms(),
    };
    Some((key, notice))
}

/// 从文件里读出 `offset` 之后的新行；返回（新行, 新 offset）。
fn read_new_lines(path: &Path, offset: u64) -> (Vec<String>, u64) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return (Vec::new(), offset);
    };
    let Ok(meta) = file.metadata() else {
        return (Vec::new(), offset);
    };
    // 文件被截断 / 轮转：从头再来，避免 offset 越过新文件长度后永远读不到内容。
    let offset = if meta.len() < offset { 0 } else { offset };
    if meta.len() == offset {
        return (Vec::new(), offset);
    }
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), offset);
    }
    let mut buffer = String::new();
    if file.read_to_string(&mut buffer).is_err() {
        return (Vec::new(), offset);
    }
    // 只消费到最后一个换行为止，半行留到下一轮（脚本是整行追加，但仍要防半行）。
    let complete = match buffer.rfind('\n') {
        Some(index) => &buffer[..=index],
        None => return (Vec::new(), offset),
    };
    // offset 只推进到真正取走的行：单轮上限之外的整行留到下一轮，不能丢事件。
    let mut lines = Vec::new();
    let mut consumed: u64 = 0;
    for line in complete.split_inclusive('\n') {
        if lines.len() >= MAX_LINES_PER_ROUND {
            break;
        }
        consumed += line.len() as u64;
        let trimmed = line.trim_end_matches('\n');
        if !trimmed.trim().is_empty() {
            lines.push(trimmed.to_string());
        }
    }
    (lines, offset + consumed)
}

/// 消费待处理事件，返回本轮新产生的提醒。
fn drain(state: &mut WatchState, path: &Path) -> Vec<Notice> {
    let (lines, offset) = read_new_lines(path, state.offset);
    state.offset = offset;
    let mut notices = Vec::new();
    for line in lines {
        let Some((key, notice)) = parse_line(&line) else {
            continue;
        };
        if state.seen.iter().any(|seen| seen == &key) {
            continue;
        }
        state.seen.push_back(key);
        while state.seen.len() > SEEN_LIMIT {
            state.seen.pop_front();
        }
        state.last_event_at = Some(notice.at);
        notices.push(notice);
    }
    notices
}

/// 启动 watcher：有新的提醒时回调（宿主据此弹系统通知）。
pub fn spawn_watcher(dispatch: impl Fn(Vec<Notice>) + Send + 'static) {
    std::thread::spawn(move || {
        let path = interaction_hook::events_path();
        let store = state_path();
        // 首次运行定位到末尾：装好就弹一堆历史事件是反效果。
        let mut state = match read_state(&store) {
            Some(state) => state,
            None => WatchState {
                offset: std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
                ..WatchState::default()
            },
        };
        write_state(&store, &state);
        loop {
            let before = state.offset;
            let notices = drain(&mut state, &path);
            if state.offset != before {
                write_state(&store, &state);
            }
            if !notices.is_empty() {
                dispatch(notices);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

/// 最近一次交出的提醒时刻（设置页诊断用）。
pub fn last_event_at() -> Option<i64> {
    read_state(&state_path()).and_then(|state| state.last_event_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stop_becomes_done_with_last_message() {
        let (kind, body) = classify(&json!({
            "hook_event_name": "Stop",
            "session_id": "s1",
            "last_assistant_message": "已经改好，等你确认。\n第二行不要",
        }))
        .unwrap();
        assert_eq!(kind, NoticeKind::Done);
        assert_eq!(body, "已经改好，等你确认。");
    }

    #[test]
    fn final_stop_is_done_and_deduped_by_generation() {
        let dir = std::env::temp_dir().join(format!(
            "wb_switch_notify_watch_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("events.jsonl");
        std::fs::write(
            &file,
            concat!(
                "{\"hook_event_name\":\"Stop\",\"session_id\":\"s1\",\"generation_id\":\"g1\",\"cwd\":\"F:/work/demo\",\"last_assistant_message\":\"done\"}\n",
                "{\"hook_event_name\":\"FinalStop\",\"session_id\":\"s1\",\"generation_id\":\"g1\",\"cwd\":\"F:/work/demo\"}\n",
            ),
        )
        .unwrap();

        let mut state = WatchState::default();
        let notices = drain(&mut state, &file);
        assert_eq!(notices.len(), 1, "同一 generation 只提醒一次");
        assert_eq!(notices[0].kind, NoticeKind::Done);
        assert_eq!(notices[0].project, "demo");
        assert_eq!(state.offset, std::fs::metadata(&file).unwrap().len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ask_tool_pre_use_becomes_attention() {
        let (kind, body) = classify(&json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s2",
            "tool_name": "mcp__ui__AskUserQuestion",
            "tool_input": { "questions": [{ "question": "要不要现在切到备用账号？" }] },
        }))
        .unwrap();
        assert_eq!(kind, NoticeKind::Attention);
        assert_eq!(body, "要不要现在切到备用账号？");
    }

    #[test]
    fn non_ask_tool_and_plain_notification_are_ignored() {
        assert!(classify(&json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s3",
            "tool_name": "Read",
        }))
        .is_none());
        assert!(classify(&json!({
            "hook_event_name": "Notification",
            "session_id": "s4",
            "notification_type": "auth_success",
        }))
        .is_none());
        assert!(classify(&json!({
            "hook_event_name": "Notification",
            "session_id": "s4",
            "notification_type": "permission_request",
        }))
        .is_some());
    }

    #[test]
    fn partial_line_waits_for_next_round() {
        let dir = std::env::temp_dir().join(format!(
            "wb_switch_notify_partial_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("events.jsonl");
        std::fs::write(&file, "{\"hook_event_name\":\"Stop\",\"session_id\":\"s\"").unwrap();
        let mut state = WatchState::default();
        assert!(drain(&mut state, &file).is_empty());
        assert_eq!(state.offset, 0, "半行不推进 offset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_label_covers_both_families() {
        assert_eq!(
            client_label(&json!({ "client": "codebuddyide" })),
            "CodeBuddy IDE"
        );
        assert_eq!(client_label(&json!({ "client": "workbuddy" })), "WorkBuddy");
        assert_eq!(
            client_label(&json!({ "agent_type": "workbuddy-ai" })),
            "WorkBuddy 国际版"
        );
        assert_eq!(client_label(&json!({})), "AI 客户端");
    }
}
