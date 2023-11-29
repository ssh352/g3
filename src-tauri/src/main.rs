#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use log::{error, info};
use tauri::Manager;
mod config;
use config::*;
use rust_share_util::*;
use tokio::sync::Mutex;
mod trader;
use itertools::*;
use std::io;
use std::sync::mpsc::*;
use std::sync::Arc;
use tracing_log::LogTracer;
use tracing_subscriber::{fmt, subscribe::CollectExt, EnvFilter};
use trader::*;

#[tauri::command]
async fn close_splashscreen(
    window: tauri::Window,
    database: tauri::State<'_, StateTpye>,
) -> Result<(), String> {
    info!("close_splashscreen");
    if let Some(splashscreen) = window.get_window("splashscreen") {
        splashscreen.close().unwrap();
    }
    window.get_window("main").unwrap().show().unwrap();
    info!("Sync traders on start");
    database.lock().await.sync_traders().await;
    Ok(())
}

struct Database {
    conf: G3Config,
    pub traders: std::collections::HashMap<String, Arc<Mutex<Trader>>>,
    cta_event_sender: tokio::sync::mpsc::Sender<CtaEvent>,
}

fn ta_key(broker_id: &str, account: &str) -> String {
    format!("{broker_id}:{account}")
}

impl Database {
    pub async fn sync_traders(&mut self) {
        for ta in self.conf.accounts.iter().filter(|ta| {
            if ta.account.len() == 0 {
                error!("[{}:{}] account不能为空", ta.broker_id, ta.account);
                return false;
            }
            if ta.trade_front.len() == 0 {
                error!("[{}:{}] trade_front不能为空", ta.broker_id, ta.account);
                return false;
            }
            true
        }) {
            let key = format!("{}:{}", ta.broker_id, ta.account);
            if !self.traders.contains_key(&key) {
                let trader = trader::Trader::init(ta.clone(), self.cta_event_sender.clone());
                self.traders.insert(key, trader);
            }
        }
        let delete_list = self
            .traders
            .iter()
            .filter(|(k, _v)| {
                self.conf
                    .accounts
                    .iter()
                    .find(|ta| format!("{}:{}", ta.broker_id, ta.account) == **k)
                    .is_none()
            })
            .map(|(k, _v)| k.clone())
            .collect::<Vec<_>>();
        for k in delete_list.iter() {
            if let Some(trader) = self.traders.remove(k) {
                if let Some(sender) = trader.lock().await.exit_sender.take() {
                    sender.send("exit".to_string()).unwrap();
                }
            }
        }
    }
    pub fn new(g3conf: G3Config, cta_es: tokio::sync::mpsc::Sender<CtaEvent>) -> Self {
        let db = Database {
            conf: g3conf,
            traders: std::collections::HashMap::new(),
            cta_event_sender: cta_es,
        };
        db
    }

    pub async fn order_rows(&self) -> Vec<OrderRow> {
        let mut v = vec![];
        for (_, t) in self.traders.iter() {
            let t = t.lock().await;
            for (_, o) in t.cta.orders.iter() {
                v.push(o.clone());
            }
        }
        v
    }

    pub async fn get_order_row(
        &self,
        broker_id: &str,
        account: &str,
        key: &str,
    ) -> Option<OrderRow> {
        if let Some(t) = self.traders.get(&ta_key(broker_id, account)) {
            t.lock().await.cta.orders.get(key).cloned()
        } else {
            None
        }
    }

    pub async fn account_rows(&self) -> Vec<TradingAccountRow> {
        let mut v = self
            .conf
            .accounts
            .iter()
            .map(|a| {
                let mut row = TradingAccountRow::default();
                row.broker_id = a.broker_id.clone();
                row.account = a.account.clone();
                row
            })
            .collect_vec();
        for row in v.iter_mut() {
            if let Some(trader) = self.traders.get(&ta_key(&row.broker_id, &row.account)) {
                let trader = trader.lock().await;
                row.status = trader.status();
                row.status_description = trader.status_description();
            }
        }
        v
    }
}

type StateTpye = Mutex<Database>;

#[derive(serde::Serialize)]
struct CustomResponse {
    message: String,
    other_val: usize,
}

async fn some_other_function() -> Option<String> {
    Some("response".into())
}

#[tauri::command]
async fn account_list(
    _window: tauri::Window,
    database: tauri::State<'_, StateTpye>,
) -> Result<Vec<TradingAccountRow>, String> {
    let v = database.lock().await.account_rows().await;
    Ok(v)
}

#[tauri::command]
async fn order_rows(
    _window: tauri::Window,
    database: tauri::State<'_, StateTpye>,
) -> Result<Vec<OrderRow>, String> {
    Ok(database.lock().await.order_rows().await)
}

#[tauri::command]
async fn get_order_row(
    _window: tauri::Window,
    broker_id: String,
    account: String,
    key: String,
    database: tauri::State<'_, StateTpye>,
) -> Result<Option<OrderRow>, String> {
    Ok(database
        .lock()
        .await
        .get_order_row(&broker_id, &account, &key)
        .await)
}

#[tauri::command]
async fn default_account(
    _window: tauri::Window,
    _database: tauri::State<'_, StateTpye>,
) -> Result<TradingAccount, String> {
    Ok(TradingAccount::default())
}

#[tauri::command]
async fn add_account(
    _window: tauri::Window,
    account: TradingAccount,
    db: tauri::State<'_, StateTpye>,
) -> Result<(), String> {
    info!("add account = {:?}", account);
    if account.account.len() == 0 {
        return Err("账号不能为空".to_string());
    } else if account.broker_id.len() == 0 {
        return Err("broker_id不能为空".to_string());
    }
    let mut db = db.lock().await;
    {
        let conf = &mut db.conf;
        if let Some(_a) = conf.accounts.iter().find(|a| a.account == account.account) {
            error!(
                "账户[{}:{}]不能重复添加",
                account.broker_id, account.account
            );
            return Err("账号已存在".to_string());
        }
        conf.accounts.push(account);
        conf.save(G3Config::default_path()).unwrap();
    }
    db.sync_traders().await;
    Ok(())
}

#[tauri::command]
async fn delete_account(
    _window: tauri::Window,
    broker_id: String,
    account: String,
    db: tauri::State<'_, StateTpye>,
) -> Result<(), String> {
    info!("delete account = [{}:{}]", broker_id, account);
    let mut db = db.lock().await;
    {
        let conf = &mut db.conf;
        conf.accounts
            .retain(|ta| !(ta.account == account && ta.broker_id == broker_id));
        conf.save(G3Config::default_path()).unwrap();
    }
    db.sync_traders().await;
    Ok(())
}

#[tauri::command]
async fn my_custom_command(
    window: tauri::Window,
    number: usize,
    _database: tauri::State<'_, StateTpye>,
) -> Result<CustomResponse, String> {
    println!("Called from {}", window.label());
    let result: Option<String> = some_other_function().await;
    if let Some(message) = result {
        Ok(CustomResponse {
            message,
            other_val: 42 + number,
        })
    } else {
        Err("No result".into())
    }
}
// the payload type must implement `Serialize` and `Clone`.
#[derive(Clone, serde::Serialize)]
struct Payload {
    message: String,
}

struct FrontLogWriter {
    log_sender: std::sync::Mutex<Sender<String>>,
}
impl std::io::Write for FrontLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let out_str = String::from_utf8_lossy(buf).to_string();
        self.log_sender.lock().unwrap().send(out_str).unwrap();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Register the command:
#[tokio::main]
async fn main() {
    LogTracer::init().unwrap();
    let file_appender = tracing_appender::rolling::hourly(".cache", "example.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let (log_sender, log_receiver) = channel();
    let (non_blocking2, _guard) = tracing_appender::non_blocking(FrontLogWriter {
        log_sender: std::sync::Mutex::new(log_sender),
    });

    let collector = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::TRACE.into()))
        .with(fmt::Subscriber::new().with_writer(io::stdout))
        .with(fmt::Subscriber::new().with_writer(non_blocking2))
        .with(fmt::Subscriber::new().with_writer(non_blocking));
    tracing::collect::set_global_default(collector).expect("Unable to set a global collector");

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info")
    }
    check_make_dir(".cache");
    let g3conf = G3Config::load(G3Config::default_path()).unwrap_or(G3Config::default());
    let (cta_es, mut cta_er) = tokio::sync::mpsc::channel(1000);
    let db = Database::new(g3conf, cta_es);
    let state = StateTpye::new(db);
    tauri::Builder::default()
        .manage(state)
        .setup(|app| {
            // listen to the `event-name` (emitted on any window)
            let _id = app.listen_global("event", |event| {
                info!("got event-name with payload {:?}", event.payload());
            });
            // emit the `event-name` event to all webview windows on the frontend
            app.emit_all(
                "event-name",
                Payload {
                    message: "Tauri is awesome!".into(),
                },
            )
            .unwrap();
            let main_window = app.get_window("main").unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                while let Ok(i) = log_receiver.recv() {
                    main_window
                        .emit("new-log-line", Payload { message: i })
                        .unwrap();
                }
            });
            let main_window = app.get_window("main").unwrap();
            tokio::spawn(async move {
                while let Some(e) = cta_er.recv().await {
                    main_window.emit("cta-event", e).unwrap();
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            close_splashscreen,
            my_custom_command,
            account_list,
            add_account,
            default_account,
            delete_account,
            order_rows,
            get_order_row
        ])
        .on_window_event(|event| match event.event() {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if event.window().label() == "log" {
                    event.window().hide().unwrap();
                    api.prevent_close();
                }
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("failed to run app");
}
