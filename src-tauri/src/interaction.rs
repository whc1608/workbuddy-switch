//! 交互提醒：把 hook 采集到的「回复完成 / 需要你操作」变成系统通知。
//!
//! 分层：core 的 `interaction_watch` 只负责把事件文件解析成 [`Notice`]，
//! 是否真的弹、弹哪一类、客户端在前台时是否静默，都在这里判定（依赖窗口状态，
//! core 拿不到）。

use serde_json::Value;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use wb_switch_core::modules::config;
use wb_switch_core::modules::interaction_hook;
use wb_switch_core::modules::interaction_watch::{self, Notice, NoticeKind};
use wb_switch_core::modules::notifications;

/// 启动 watcher：事件到达后按配置过滤并弹系统通知。
pub fn start(app: AppHandle) {
    interaction_watch::spawn_watcher(move |notices| {
        let cfg = config::load_notify_config();
        if !cfg["enabled"].as_bool().unwrap_or(true) {
            return;
        }
        let notify_done = cfg["notifyDone"].as_bool().unwrap_or(true);
        let notify_attention = cfg["notifyAttention"].as_bool().unwrap_or(true);
        let quiet_when_focused = cfg["quietWhenFocused"].as_bool().unwrap_or(true);
        // 每批最多查一次前台窗口：attention 不看它，done 才看。
        let mut foreground: Option<bool> = None;
        for notice in notices {
            let allowed = match notice.kind {
                NoticeKind::Done => notify_done,
                NoticeKind::Attention => notify_attention,
            };
            if !allowed {
                continue;
            }
            if notice.kind == NoticeKind::Done && quiet_when_focused {
                let focused = *foreground.get_or_insert_with(client_in_foreground);
                if focused {
                    continue;
                }
            }
            show(&app, &notice);
        }
    });
}

/// 弹一条系统通知，并写入应用内通知存档（事后可回看）。
fn show(app: &AppHandle, notice: &Notice) {
    let title = notice.title();
    let body = if notice.project.is_empty() {
        notice.body.clone()
    } else {
        format!("{} · {}", notice.project, notice.body)
    };
    let _ = app
        .notification()
        .builder()
        .title(title.clone())
        .body(body.clone())
        .show();
    let level = match notice.kind {
        NoticeKind::Attention => "warning",
        NoticeKind::Done => "info",
    };
    let _ = notifications::record(level, &title, Some(&body));
}

/// 前台进程是否属于已知 AI 客户端（只有 Windows 能判定，其它平台一律按「不在前台」）。
fn client_in_foreground() -> bool {
    #[cfg(target_os = "windows")]
    {
        foreground::process_name()
            .map(|name| {
                let lower = name.to_ascii_lowercase();
                (lower.contains("workbuddy") || lower.contains("codebuddy"))
                    && !lower.contains("wb-switch")
            })
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

#[tauri::command]
pub fn get_notify_config() -> Value {
    config::load_notify_config()
}

#[tauri::command]
pub fn save_notify_config(config_value: Value) -> Result<Value, String> {
    config::save_notify_config_at(&config::notify_config_file(), &config_value)
        .map_err(|error| error.to_string())?;
    Ok(config::load_notify_config())
}

/// 提醒 hook 安装状态；附带最近一次交出的提醒时刻。
#[tauri::command]
pub async fn get_notify_hook_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut status = interaction_hook::status();
        status["lastEventAt"] = serde_json::json!(interaction_watch::last_event_at());
        status
    })
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn install_notify_hook() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        interaction_hook::install()?;
        let mut status = interaction_hook::status();
        status["lastEventAt"] = serde_json::json!(interaction_watch::last_event_at());
        Ok::<Value, String>(status)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn uninstall_notify_hook() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        interaction_hook::uninstall()?;
        let mut status = interaction_hook::status();
        status["lastEventAt"] = serde_json::json!(interaction_watch::last_event_at());
        Ok::<Value, String>(status)
    })
    .await
    .map_err(|error| error.to_string())?
}

/// 设置页「试一试」：走同一条通知通道，验证权限与样式。
#[tauri::command]
pub fn test_notification(app: AppHandle) -> Result<(), String> {
    let notice = Notice {
        kind: NoticeKind::Done,
        client: "交互提醒".to_string(),
        session_id: "test".to_string(),
        project: String::new(),
        body: "这条是测试通知：正式使用时，模型回复完成或需要你确认才会提醒。".to_string(),
        at: wb_switch_core::modules::config::now_ms(),
    };
    show(&app, &notice);
    Ok(())
}

/// 前台窗口归属进程名（Windows）。
#[cfg(target_os = "windows")]
mod foreground {
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn QueryFullProcessImageNameW(
            handle: isize,
            flags: u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    /// 前台窗口的进程可执行文件名（含扩展名）；拿不到返回 None。
    pub fn process_name() -> Option<String> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd == 0 {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return None;
            }
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 {
                return None;
            }
            let mut buffer = vec![0u16; 1024];
            let mut size = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
            CloseHandle(handle);
            if ok == 0 {
                return None;
            }
            let path = String::from_utf16_lossy(&buffer[..size as usize]);
            Some(
                path.rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(&path)
                    .to_string(),
            )
        }
    }
}
