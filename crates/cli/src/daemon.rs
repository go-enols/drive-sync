//! `daemon` 子命令：常驻同步（连接循环 + 剪切板监听线程 + 事件摘要打印）。
//!
//! Ctrl-C 优雅退出：停止监听线程、关闭 WS 连接（Runtime::run 内的 shutdown 分支）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use drive_client::{Runtime, RuntimeEvent};

use crate::clipboard;
use crate::common::{format_now, App};
use crate::update_cmd;

pub async fn run(
    app: &App,
    clipboard_spec: &str,
    poll_ms: u64,
    auto_update: bool,
    update_restart: bool,
) -> Result<()> {
    let store = app.store()?;
    if !store.identity_exists() {
        bail!(
            "尚未初始化设备身份（{}）。请先运行 `drive-sync init --name <设备名>`",
            store.identity_path().display()
        );
    }
    let identity = store.load_identity()?;
    let poll_ms = poll_ms.max(50);
    let mode = clipboard::ClipboardMode::parse(clipboard_spec)?;

    let mut config = app.runtime_config(&store)?;
    let relay_display = config.relay_url.clone();
    let data_dir_display = config.data_dir.display().to_string();

    // 先创建后端：没有图形会话等错误应在进入常驻循环前报出。
    let backend = clipboard::build_backend(clipboard_spec)?;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
    let (apply_tx, apply_rx) =
        std::sync::mpsc::channel::<drive_core::clipboard::ClipboardPayload>();
    config.event_tx = Some(event_tx);
    config.remote_clipboard_tx = Some(apply_tx);

    let (runtime, handle) = Runtime::open(config)?;

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let clip_tx = handle.local_clipboard_sender();
    let thread_flag = shutdown_flag.clone();
    let watcher = std::thread::Builder::new()
        .name("drive-sync-clipboard".to_string())
        .spawn(move || {
            clipboard::run_watcher(backend, poll_ms, apply_rx, clip_tx, thread_flag);
        })
        .context("启动剪切板监听线程失败")?;

    let printer = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            // Presence 事件过于频繁，只在 debug 日志体现（可 --log-level debug 查看）。
            if matches!(event, RuntimeEvent::Presence { .. }) {
                tracing::debug!(event = %event.summary(), "运行时事件");
                continue;
            }
            println!("[{}] {}", format_now(), event.summary());
        }
    });

    println!("drive-sync 守护进程已启动");
    println!("  设备: {} ({})", identity.name, identity.device_id());
    println!("  中继: {relay_display}");
    println!("  数据目录: {data_dir_display}");
    println!("  剪切板: {}，轮询 {poll_ms} ms", mode.describe());
    if auto_update {
        println!(
            "  自动更新: 启动时检查一次 + 每 6 小时检查（自动重启: {}）",
            if update_restart {
                "是"
            } else {
                "否，安装后需手动重启"
            }
        );
    } else {
        println!("  自动更新: 未启用（可用 --auto-update 开启）");
    }
    println!("  按 Ctrl-C 退出");

    if auto_update {
        // 后台任务：阻塞下载放到 spawn_blocking，避免卡住同步运行时。
        let update_app = app.clone();
        tokio::spawn(async move {
            update_cmd::auto_update_loop(
                update_app,
                update_restart,
                update_cmd::AUTO_UPDATE_INTERVAL,
            )
            .await;
        });
    } else if update_restart {
        eprintln!("提示: --update-restart 仅在同时启用 --auto-update 时生效，已忽略。");
    }

    let mut runtime_task = tokio::spawn(async move { runtime.run().await });
    let mut runtime_finished = false;
    let interrupted = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\n收到 Ctrl-C，正在退出...");
            true
        }
        result = &mut runtime_task => {
            runtime_finished = true;
            match result {
                Ok(Ok(())) => println!("同步运行时已停止。"),
                Ok(Err(error)) => eprintln!("同步运行时错误: {error:#}"),
                Err(error) => eprintln!("同步运行时任务异常: {error}"),
            }
            false
        }
    };

    if interrupted {
        handle.shutdown();
    }
    shutdown_flag.store(true, Ordering::SeqCst);
    if !runtime_finished {
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut runtime_task).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), printer).await;
    let _ = watcher.join();
    println!("已退出。");
    Ok(())
}
