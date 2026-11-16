//! 交互提醒 hook：把「回复完成」「需要你操作」事件追加到
//! `~/.wb-switch/interaction-events.jsonl`，由 `interaction_watch` 消费后弹系统通知。
//!
//! 与限额 hook（[`crate::modules::rate_limit_hook`]）并存：独立脚本（`notify-hook.sh`）
//! 与独立事件文件，只增删带自己 marker 的条目，不读写对方条目。
//!
//! 覆盖两族客户端（客户端都用 Claude Code 兼容的 command hook）：
//! - WorkBuddy：`~/.workbuddy`、`~/.workbuddy-ai`，额外支持 `PermissionRequest` / `Notification`；
//! - CodeBuddy（IDE / CLI / VS Code 共用一份配置）：`~/.codebuddy`、`~/.codebuddycn`。
//!
//! payload 里的 `client` 字段区分宿主（`workbuddy` / `codebuddyide` / `codebuddy` / `vscode`），
//! 消费端据此标注提醒来源。
//!
//! 两条与限额 hook 不同的取舍：
//! - **卸载只做结构化移除**：同一份 `settings.json` 也被限额 hook 写，逐字节还原会连带
//!   抹掉对方后来加的条目，因此这里只删除自己的 marker 命中项。
//! - **不自动接入**：改写用户真实配置需要明确同意，接入动作由设置页按钮触发。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::config::{atomic_write, home_dir};

/// 提醒 hook 脚本名（与限额 hook 的 `hook.sh` 区分，marker 才不会互相认领）。
const SCRIPT_NAME: &str = "notify-hook.sh";
/// 提醒事件信号文件：脚本 append、后端消费。
pub const EVENTS_FILE_NAME: &str = "interaction-events.jsonl";
/// 安装前的一次性备份目录（与限额 hook 共用目录，文件名带前缀区分）。
const BACKUP_DIR_NAME: &str = "hook-backups";
const SETTINGS_FILE_NAME: &str = "settings.json";
const STORE_DIR_NAME: &str = ".wb-switch";

/// 所有客户端都支持的事件。
const BASE_EVENTS: [&str; 3] = ["Stop", "FinalStop", "PreToolUse"];
/// 仅 WorkBuddy 声明支持的事件（CodeBuddy IDE 只承认 Stop / PreToolUse 等基础事件）。
const WORKBUDDY_EVENTS: [&str; 2] = ["PermissionRequest", "Notification"];

/// 提问类工具的 matcher：只在模型真的向用户提问时才拉起脚本。
///
/// 客户端若忽略 matcher，`PreToolUse` 会随每次工具调用触发；消费端仍按工具名过滤，
/// 行为正确，代价只是多几次脚本启动。
const ASK_TOOL_MATCHER: &str =
    "^(?:ask_followup_question|request_user_input(?:_async)?|AskUserQuestion|ask_user_question|RequestUserInput)$";

/// 客户端数据根目录名 → 备份标签 → 是否支持 `PermissionRequest` / `Notification`。
const TARGETS: [(&str, &str, bool); 4] = [
    (".workbuddy", "workbuddy", true),
    (".workbuddy-ai", "workbuddy-ai", true),
    (".codebuddy", "codebuddy", false),
    (".codebuddycn", "codebuddycn", false),
];

struct Target {
    label: &'static str,
    data_root: PathBuf,
    settings: PathBuf,
    supports_attention_events: bool,
}

impl Target {
    fn exists(&self) -> bool {
        self.data_root.is_dir()
    }
}

struct Layout {
    script: PathBuf,
    events: PathBuf,
    backups: PathBuf,
    targets: Vec<Target>,
}

impl Layout {
    fn under(base: &Path) -> Self {
        let store = base.join(STORE_DIR_NAME);
        Self {
            script: store.join(SCRIPT_NAME),
            events: store.join(EVENTS_FILE_NAME),
            backups: store.join(BACKUP_DIR_NAME),
            targets: TARGETS
                .iter()
                .map(|(dir, label, supports)| {
                    let data_root = base.join(dir);
                    Self::target(data_root, label, *supports)
                })
                .collect(),
        }
    }

    fn target(data_root: PathBuf, label: &'static str, supports: bool) -> Target {
        Target {
            settings: data_root.join(SETTINGS_FILE_NAME),
            data_root,
            label,
            supports_attention_events: supports,
        }
    }

    fn default_layout() -> Self {
        Self::under(&home_dir())
    }

    fn marker(&self) -> String {
        format!("'{}'", script_arg(&self.script).replace('\'', "'\\''"))
    }

    fn backup_path(&self, target: &Target) -> PathBuf {
        self.backups
            .join(format!("notify-{}.settings.json", target.label))
    }
}

// ---------------------------------------------------------------------------
// 脚本与命令
// ---------------------------------------------------------------------------

/// hook 脚本正文：一次 `cat` 读入 payload、原样追加一行，最后必须回 `{}`。
///
/// 事件文件路径写死为绝对路径（hook 由客户端执行器拉起，环境变量不可靠）；
/// Windows 下统一正斜杠，避免 MSYS bash 在引号里把反斜杠当转义。
fn script_body(events: &Path) -> String {
    let sh_events = events.to_string_lossy().replace('\\', "/");
    [
        "#!/bin/sh",
        "payload=$(cat)",
        &format!(
            "printf '%s\\n' \"$payload\" >> '{}'",
            sh_events.replace('\'', "'\\''")
        ),
        "printf '{}'",
        "",
    ]
    .join("\n")
}

/// MSYS 风格路径：`C:\a\b` → `/c/a/b`（Windows 侧客户端用 PortableGit bash 执行）。
fn msys_path(script: &Path) -> String {
    let text = script.to_string_lossy().replace('\\', "/");
    let mut chars = text.chars();
    let drive = chars.next().unwrap_or('c').to_ascii_lowercase();
    match chars.next() {
        Some(':') => format!("/{}{}", drive, chars.as_str()),
        _ => text,
    }
}

fn script_arg(script: &Path) -> String {
    if cfg!(windows) {
        msys_path(script)
    } else {
        script.to_string_lossy().to_string()
    }
}

fn hook_command(script: &Path) -> String {
    let interpreter = if cfg!(windows) { "bash" } else { "sh" };
    format!("{interpreter} '{}'", script_arg(script).replace('\'', "'\\''"))
}

fn event_list(supports_attention_events: bool) -> Vec<&'static str> {
    let mut events = BASE_EVENTS.to_vec();
    if supports_attention_events {
        events.extend_from_slice(&WORKBUDDY_EVENTS);
    }
    events
}

// ---------------------------------------------------------------------------
// 配置合并
// ---------------------------------------------------------------------------

fn read_settings(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn entry_is_ours(entry: &Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("type").and_then(Value::as_str) == Some("command")
                    && hook
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| command.contains(marker))
            })
        })
}

fn registered(root: &Value, marker: &str) -> bool {
    root.get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            hooks.values().any(|entries| {
                entries
                    .as_array()
                    .is_some_and(|list| list.iter().any(|entry| entry_is_ours(entry, marker)))
            })
        })
}

fn event_entries<'a>(root: &'a mut Value, event: &str) -> Result<&'a mut Vec<Value>, String> {
    let Some(object) = root.as_object_mut() else {
        return Err("配置根节点不是 JSON 对象".to_string());
    };
    let hooks = object.entry("hooks".to_string()).or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err("`hooks` 不是 JSON 对象".to_string());
    };
    let list = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
    list.as_array_mut()
        .ok_or_else(|| format!("`hooks.{event}` 不是数组"))
}

/// 写入某个事件的条目：先摘掉自己的旧条目，再按事件类型追加一条。
fn upsert_event(
    root: &mut Value,
    event: &str,
    command: &str,
    marker: &str,
) -> Result<(), String> {
    let list = event_entries(root, event)?;
    list.retain(|entry| !entry_is_ours(entry, marker));
    let matcher = if event == "PreToolUse" {
        ASK_TOOL_MATCHER
    } else {
        ""
    };
    list.push(json!({
        "matcher": matcher,
        "hooks": [{ "type": "command", "command": command }],
    }));
    Ok(())
}

fn remove_ours(root: &mut Value, marker: &str) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    let mut empty: Vec<String> = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        if let Some(list) = entries.as_array_mut() {
            list.retain(|entry| !entry_is_ours(entry, marker));
            if list.is_empty() {
                empty.push(event.clone());
            }
        }
    }
    for event in empty {
        hooks.remove(&event);
    }
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 提醒事件文件路径（消费端使用）。
pub fn events_path() -> PathBuf {
    Layout::default_layout().events
}

/// 安装（幂等）：生成脚本 + 在存在的客户端里注册事件；写前做一次性备份。
pub fn install() -> Result<Value, String> {
    install_at(&Layout::default_layout())
}

fn install_at(layout: &Layout) -> Result<Value, String> {
    let script = script_body(&layout.events);
    if let Some(parent) = layout.script.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建目录失败：{error}"))?;
    }
    let current = std::fs::read_to_string(&layout.script).ok();
    if current.as_deref() != Some(script.as_str()) {
        atomic_write(&layout.script, &script).map_err(|error| format!("写入脚本失败：{error}"))?;
    }

    let command = hook_command(&layout.script);
    let marker = layout.marker();
    let mut installed = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for target in &layout.targets {
        if !target.exists() {
            continue;
        }
        std::fs::create_dir_all(&layout.backups).ok();
        if let Ok(original) = std::fs::read_to_string(&target.settings) {
            let backup = layout.backup_path(target);
            if !backup.is_file() {
                let _ = atomic_write(&backup, &original);
            }
        }
        match install_target(target, &command, &marker) {
            Ok(()) => installed += 1,
            Err(error) => errors.push(format!("{}: {error}", target.label)),
        }
    }
    if installed == 0 && !errors.is_empty() {
        return Err(errors.join("；"));
    }
    let mut status = status_at(layout);
    status["errors"] = json!(errors);
    Ok(status)
}

fn install_target(target: &Target, command: &str, marker: &str) -> Result<(), String> {
    let original = std::fs::read_to_string(&target.settings).ok();
    let mut root = match &original {
        Some(text) => {
            serde_json::from_str::<Value>(text).map_err(|_| "不是合法 JSON，已跳过".to_string())?
        }
        None => json!({}),
    };
    for event in event_list(target.supports_attention_events) {
        upsert_event(&mut root, event, command, marker)?;
    }
    let content = serde_json::to_string_pretty(&root).map_err(|error| error.to_string())?;
    if original.as_deref() == Some(content.as_str()) {
        return Ok(());
    }
    atomic_write(&target.settings, &content).map_err(|error| format!("写入失败：{error}"))
}

/// 卸载：只移除自己的条目，保留同一份配置里的其它 hook（限额 hook 也写这里）。
pub fn uninstall() -> Result<Value, String> {
    uninstall_at(&Layout::default_layout())
}

fn uninstall_at(layout: &Layout) -> Result<Value, String> {
    let marker = layout.marker();
    let mut errors: Vec<String> = Vec::new();
    for target in &layout.targets {
        let Ok(original) = std::fs::read_to_string(&target.settings) else {
            continue;
        };
        let Some(mut root) = read_settings(&target.settings) else {
            continue;
        };
        remove_ours(&mut root, &marker);
        let content = serde_json::to_string_pretty(&root).map_err(|error| error.to_string())?;
        // 没有本工具条目时不重写文件：避免仅因为重新排版就改动用户配置。
        if content == original {
            continue;
        }
        if let Err(error) = atomic_write(&target.settings, &content) {
            errors.push(format!("{}: {error}", target.label));
        }
    }
    let mut status = status_at(layout);
    status["errors"] = json!(errors);
    Ok(status)
}

/// 安装状态（脚本、事件文件与每处客户端注册情况）。
pub fn status() -> Value {
    status_at(&Layout::default_layout())
}

fn status_at(layout: &Layout) -> Value {
    let script_exists = layout.script.is_file()
        && std::fs::read_to_string(&layout.script)
            .map(|text| text == script_body(&layout.events))
            .unwrap_or(false);
    let marker = layout.marker();
    let targets: Vec<Value> = layout
        .targets
        .iter()
        .map(|target| {
            let exists = target.exists();
            let has_settings = target.settings.is_file();
            let registered_here = read_settings(&target.settings)
                .is_some_and(|root| registered(&root, &marker));
            json!({
                "label": target.label,
                "settingsPath": target.settings.to_string_lossy(),
                "clientExists": exists,
                "hasSettings": has_settings,
                "registered": registered_here,
                "supportsAttentionEvents": target.supports_attention_events,
            })
        })
        .collect();
    let installed_targets = targets
        .iter()
        .filter(|target| target["clientExists"].as_bool() == Some(true))
        .filter(|target| target["registered"].as_bool() == Some(true))
        .count();
    let client_targets = targets
        .iter()
        .filter(|target| target["clientExists"].as_bool() == Some(true))
        .count();
    json!({
        "scriptPath": layout.script.to_string_lossy(),
        "scriptExists": script_exists,
        "eventsPath": layout.events.to_string_lossy(),
        "installed": script_exists && installed_targets > 0,
        "installedTargets": installed_targets,
        "clientTargets": client_targets,
        "targets": targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb_switch_notify_hook_{}_{name}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn layout_with_workbuddy(base: &Path) -> Layout {
        std::fs::create_dir_all(base.join(".workbuddy")).unwrap();
        Layout::under(base)
    }

    #[test]
    fn install_registers_base_events_and_attention_events_only_for_workbuddy() {
        let dir = TempDir::new("install");
        std::fs::create_dir_all(dir.0.join(".codebuddy")).unwrap();
        let layout = layout_with_workbuddy(&dir.0);
        install_at(&layout).unwrap();

        let workbuddy: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.0.join(".workbuddy/settings.json")).unwrap()).unwrap();
        for event in ["Stop", "FinalStop", "PreToolUse", "PermissionRequest", "Notification"] {
            assert!(
                workbuddy["hooks"][event].is_array(),
                "WorkBuddy 应注册 {event}"
            );
        }
        let codebuddy: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.0.join(".codebuddy/settings.json")).unwrap()).unwrap();
        assert!(codebuddy["hooks"]["Stop"].is_array());
        assert!(codebuddy["hooks"]["PreToolUse"].is_array());
        assert!(
            codebuddy["hooks"].get("PermissionRequest").is_none(),
            "CodeBuddy 不注册它不支持的事件"
        );
        let matcher = codebuddy["hooks"]["PreToolUse"][0]["matcher"]
            .as_str()
            .unwrap();
        assert!(matcher.contains("AskUserQuestion"), "{matcher}");
    }

    #[test]
    fn install_is_idempotent_and_keeps_foreign_hooks() {
        let dir = TempDir::new("idempotent");
        std::fs::create_dir_all(dir.0.join(".workbuddy")).unwrap();
        let settings = dir.0.join(".workbuddy/settings.json");
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({
                "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "bash '/x/hook.sh'" }] }] }
            }))
            .unwrap(),
        )
        .unwrap();

        let layout = layout_with_workbuddy(&dir.0);
        install_at(&layout).unwrap();
        install_at(&layout).unwrap();

        let root: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "限额 hook 的条目必须保留，且本工具只占一条");
    }

    #[test]
    fn uninstall_removes_only_our_entries() {
        let dir = TempDir::new("uninstall");
        std::fs::create_dir_all(dir.0.join(".workbuddy")).unwrap();
        let settings = dir.0.join(".workbuddy/settings.json");
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({
                "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "bash '/x/hook.sh'" }] }] }
            }))
            .unwrap(),
        )
        .unwrap();
        let layout = layout_with_workbuddy(&dir.0);
        install_at(&layout).unwrap();
        uninstall_at(&layout).unwrap();

        let root: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("hook.sh"));
        assert!(root["hooks"].get("Notification").is_none());
    }

    #[test]
    fn script_appends_payload_to_absolute_events_path() {
        let body = script_body(Path::new("/home/a b/.wb-switch/interaction-events.jsonl"));
        assert!(body.ends_with("printf '{}'\n"));
        assert!(body.contains("payload=$(cat)"));
        assert!(body.contains("/home/a b/.wb-switch/interaction-events.jsonl"));
    }
}
