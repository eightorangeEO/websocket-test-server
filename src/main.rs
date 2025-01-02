use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use chrono::{DateTime, Utc, Duration};
use std::time::SystemTime;

use futures_util::{StreamExt, FutureExt};
use once_cell::sync::Lazy;
use salvo::prelude::*;
use salvo::websocket::{Message, WebSocket, WebSocketUpgrade};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use dotenv::dotenv;
use std::env;

// 修改用户存储的类型定义
type Users = RwLock<HashMap<usize, mpsc::UnboundedSender<Result<Message, salvo::Error>>>>;

static NEXT_USER_ID: AtomicUsize = AtomicUsize::new(1);
static ONLINE_USERS: Lazy<Users> = Lazy::new(Users::default);

#[handler]
async fn ws_handler(req: &mut Request, res: &mut Response) {
    WebSocketUpgrade::new()
        .upgrade(req, res, handle_socket)
        .await
        .unwrap_or_else(|e| eprintln!("WebSocket upgrade error: {}", e));
}

async fn handle_socket(ws: WebSocket) {
    // 为新用户分配一个唯一的 ID
    let my_id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);
    save_log(&format!("新用户连接: {}", my_id)).await;
    println!("新用户连接: {}", my_id);

    // 获取环境变量，决定是否发送消息给源客户端
    let send_to_origin = env::var("SEND_ORI_USER")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    // 分离 WebSocket 的发送和接收部分
    let (user_ws_tx, mut user_ws_rx) = ws.split();

    // 创建一个无限制的通道来处理消息的缓冲和刷新
    let (tx, rx) = mpsc::unbounded_channel();
    let rx = UnboundedReceiverStream::new(rx);

    // 处理发送消息的任务
    let forward_task = rx.forward(user_ws_tx).map(|result| {
        if let Err(e) = result {
            eprintln!("websocket send error: {:?}", e);
        }
    });
    tokio::task::spawn(forward_task);

    // 将新用户添加到在线用户列表
    ONLINE_USERS.write().await.insert(my_id, tx);

    // 处理接收消息的任务
    while let Some(result) = user_ws_rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                let error_msg = format!("websocket error(uid={}): {}", my_id, e);
                save_log(&error_msg).await;
                eprintln!("{}", error_msg);
                break;
            }
        };

        // 检查消息类型并获取文本内容
        if let Ok(text) = msg.as_str() {
            let new_msg = format!("<用户#{}>: {}", my_id, text);
            save_log(&new_msg).await;

            // 向其他用户广播消息
            for (&uid, tx) in ONLINE_USERS.read().await.iter() {
                if send_to_origin || uid != my_id {
                    if let Err(_disconnected) = tx.send(Ok(Message::text(new_msg.clone()))) {
                        // tx 已断开连接，user_disconnected 会在另一个任务中处理
                    }
                }
            }
        }
    }

    // 用户断开连接时的清理
    save_log(&format!("用户断开连接: {}", my_id)).await;
    user_disconnected(my_id).await;
}

async fn user_disconnected(my_id: usize) {
    println!("用户断开连接: {}", my_id);
    ONLINE_USERS.write().await.remove(&my_id);
}

// 添加日志相关的函数
async fn save_log(message: &str) {
    // 检查是否启用日志
    let save_log = env::var("SAVE_LOG")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    if !save_log {
        return;
    }

    // 创建logs目录（如果不存在）
    let logs_dir = Path::new("logs");
    if !logs_dir.exists() {
        fs::create_dir(logs_dir).unwrap_or_else(|e| eprintln!("创建日志目录失败: {}", e));
    }

    // 获取当前日期作为日志文件名
    let now: DateTime<Utc> = SystemTime::now().into();
    let file_name = format!("logs/server_log_{}.txt", now.format("%Y-%m-%d"));

    // 打开或创建日志文件
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_name)
        .unwrap_or_else(|e| panic!("无法打开日志文件: {}", e));

    // 写入日志
    let log_entry = format!("[{}] {}\n", now.format("%Y-%m-%d %H:%M:%S"), message);
    file.write_all(log_entry.as_bytes())
        .unwrap_or_else(|e| eprintln!("写入日志失败: {}", e));

    // 清理旧日志
    clean_old_logs().await;
}

async fn clean_old_logs() {
    let save_days = env::var("SAVE_LOG_DATE")
        .unwrap_or_else(|_| "30".to_string())
        .parse::<i64>()
        .unwrap_or(30);

    let logs_dir = Path::new("logs");
    if !logs_dir.exists() {
        return;
    }

    let now: DateTime<Utc> = SystemTime::now().into();
    if let Ok(entries) = fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if let Ok(created) = metadata.created() {
                    let created: DateTime<Utc> = created.into();
                    if now - created > Duration::days(save_days) {
                        if let Err(e) = fs::remove_file(entry.path()) {
                            eprintln!("删除旧日志文件失败: {}", e);
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // 加载环境变量
    dotenv().ok();

    // 获取配置
    let port = env::var("PORT")
        .unwrap_or_else(|_| "12345".to_string())
        .parse::<u16>()
        .unwrap_or(12345);

    let ip = env::var("IP")
        .unwrap_or_else(|_| "127.0.0.1".to_string());

    let router = Router::new()
        .push(Router::with_path("ws")
            .goal(ws_handler));

    let server_start_msg = format!("WebSocket 服务器正在监听 {}:{}", ip, port);
    println!("{}", server_start_msg);
    save_log(&server_start_msg).await;

    let acceptor = TcpListener::new(format!("{}:{}", ip, port)).bind().await;
    Server::new(acceptor).serve(router).await;
}
