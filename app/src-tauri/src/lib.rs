//! DSH Tether 手机端 Rust 核心:iroh 连接、配对、审批事件转发。
//!
//! 前端只做渲染与两个按钮;连接生命周期全在这层。按「打开 app 时连接」设计,
//! 无后台常驻(Android doze 语义下这是产品决定,不是缺陷)。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(target_os = "android")]
mod local;

use anyhow::{bail, Context as _, Result};
use tether_core::i18n::t;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use tether_core::{
    load_or_create_secret, pair_fail_text, read_line_bounded, write_line, write_private, Wire, ALPN,
    MAX_LINE,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, Mutex};
use tokio::io::{AsyncRead, ReadBuf};

// —— 累计流量统计 ——
// 全局原子变量:本次连接累计字节数,断开时清零。
// 多代理流并发读写同一方向,所以用 AtomicU64 累加(不需要锁)。
static TX_BYTES: AtomicU64 = AtomicU64::new(0); // 手机发出 → 主机
static RX_BYTES: AtomicU64 = AtomicU64::new(0); // 主机发出 → 手机收到

fn reset_traffic() {
    TX_BYTES.store(0, Ordering::Relaxed);
    RX_BYTES.store(0, Ordering::Relaxed);
}

/// 每次 read 将字节数累加到指定方向计数器的 AsyncRead 包装器
struct CountedReader<R> {
    inner: R,
    counter: &'static AtomicU64,
}

impl<R: AsyncRead + Unpin> AsyncRead for CountedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let inner = Pin::new(&mut self.inner);
        match inner.poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len() - before;
                self.counter.fetch_add(n as u64, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

#[derive(Default)]
struct AppState {
    ep: Mutex<Option<Endpoint>>,
    /// 在线连接的决定发送端;None = 未连接
    outgoing: Mutex<Option<mpsc::Sender<Wire>>>,
    /// 本地代理监听端口;WebView 指向它即拿到主机的完整 dsh web UI
    proxy_port: Mutex<Option<u16>>,
    /// 本地模式:手机上跑着的 dsh(仅 Android arm64 构建有)
    #[cfg(target_os = "android")]
    local: Mutex<Option<local::LocalHost>>,
}

/// 一台已配对主机
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct HostRecord {
    id: String,
    /// 配对时用户给这台主机起的名字;空则前端显示 ID 前缀
    #[serde(default)]
    label: String,
    /// 直连地址(ip:port);有值则下次一键重连走直连,不再依赖 discovery
    #[serde(default)]
    addr: Option<String>,
}

/// 手机侧的主机簿:配过的都留着,不必为换一台重填凭证
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct HostBook {
    hosts: Vec<HostRecord>,
    /// 上次连的那台,下次打开直接用
    current: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StateEvent {
    status: &'static str,
    detail: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ApprovalEvent {
    id: String,
    tool_name: String,
    reason: String,
}

fn data_dir(app: &AppHandle) -> Result<PathBuf> {
    app.path().app_data_dir().context(t("取不到应用数据目录", "cannot locate the app data directory"))
}

fn host_book_path(app: &AppHandle) -> Result<PathBuf> {
    Ok(data_dir(app)?.join("hosts.json"))
}

fn load_book(app: &AppHandle) -> HostBook {
    let Ok(path) = host_book_path(app) else { return HostBook::default() };
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(book) = serde_json::from_slice::<HostBook>(&bytes) {
            return book
        }
    }
    // 单主机时代的 host.json:读一次迁移过来,免得老用户要重新配对
    let Ok(dir) = data_dir(app) else { return HostBook::default() };
    let legacy = dir.join("host.json");
    let Ok(bytes) = std::fs::read(&legacy) else { return HostBook::default() };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return HostBook::default() };
    let Some(id) = value.get("id").and_then(|v| v.as_str()) else { return HostBook::default() };
    HostBook {
        hosts: vec![HostRecord { id: id.to_string(), label: String::new(), addr: None }],
        current: Some(id.to_string()),
    }
}

// 这份清单记着每台已配对电脑的公钥;手机侧同样按私密文件写。
fn save_book(app: &AppHandle, book: &HostBook) -> Result<()> {
    let path = host_book_path(app)?;
    write_private(&path, &serde_json::to_vec_pretty(book)?)
}

fn emit_state(app: &AppHandle, status: &'static str, detail: impl Into<String>) {
    let _ = app.emit("remote:state", StateEvent { status, detail: detail.into() });
}

async fn get_or_init_endpoint(app: &AppHandle, state: &AppState) -> Result<Endpoint> {
    let mut guard = state.ep.lock().await;
    if let Some(ep) = guard.as_ref() {
        return Ok(ep.clone());
    }
    let secret = load_or_create_secret(&data_dir(app)?.join("identity.key"))?;
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .context(t("iroh endpoint 启动失败", "failed to start the iroh endpoint"))?;
    *guard = Some(ep.clone());
    Ok(ep)
}

/// 连接 actor:发首行(Pair 或 Hello),配对成功即持久化 host,
/// 之后读侧转事件给前端、写侧从 mpsc 取决定,断开清理并广播状态。
async fn run_connection(app: AppHandle, peer: EndpointId, first: Wire, pair_label: Option<String>, direct: Option<std::net::SocketAddr>) {
    if let Err(e) = run_connection_inner(&app, peer, first, pair_label, direct).await {
        emit_state(&app, "disconnected", format!("{e:#}"));
    } else {
        emit_state(&app, "disconnected", t("连接已断开", "Connection closed"));
    }
    let state = app.state::<AppState>();
    *state.outgoing.lock().await = None;
    reset_traffic();
}

async fn run_connection_inner(
    app: &AppHandle,
    peer: EndpointId,
    first: Wire,
    pair_label: Option<String>,
    direct: Option<std::net::SocketAddr>,
) -> Result<()> {
    emit_state(app, "connecting", t("正在连接主机…", "Connecting to the computer…"));
    let state = app.state::<AppState>();
    let ep = get_or_init_endpoint(app, &state).await?;
    let conn = match direct {
        Some(addr) => ep.connect(EndpointAddr::new(peer).with_ip_addr(addr), ALPN).await,
        None => ep.connect(peer, ALPN).await,
    }
    .context(t("连不上主机(插件未运行、ID 不对或网络不可达)", "cannot reach the computer (plugin not running, wrong ID, or no route)"))?;
    let (mut send, mut recv) = conn.open_bi().await.context(t("打开控制流失败", "failed to open the control stream"))?;

    // 直连成功:把可达地址记进主机簿,下次一键重连直接走直连,不再依赖 discovery
    if let Some(addr) = direct {
        let id = peer.to_string();
        let mut book = load_book(app);
        if let Some(existing) = book.hosts.iter_mut().find(|h| h.id == id) {
            existing.addr = Some(addr.to_string());
            save_book(app, &book)?;
        }
    }

    let pairing = matches!(first, Wire::Pair { .. });
    write_line(&mut send, &serde_json::to_string(&first)?).await?;
    if pairing {
        let resp = read_line_bounded(&mut recv, MAX_LINE)
            .await
            .context(t("配对被主机拒绝(配对码错误或窗口已关闭)", "the computer refused the pairing (wrong code, or its window is gone)"))?;
        match serde_json::from_str::<Wire>(&resp)? {
            Wire::PairOk => {
                let id = peer.to_string();
                let label = pair_label.unwrap_or_default();
                let mut book = load_book(app);
                match book.hosts.iter_mut().find(|h| h.id == id) {
                    Some(existing) => existing.label = label,
                    None => book.hosts.push(HostRecord { id: id.clone(), label, addr: direct.map(|a| a.to_string()) }),
                }
                book.current = Some(id);
                save_book(app, &book)?;
            }
            Wire::PairFail { reason } => bail!("{}{}", t("配对失败: ", "pairing failed: "), pair_fail_text(&reason)),
            _ => bail!("{}", t("配对应答不符合协议", "the pairing reply does not follow the protocol")),
        }
    }

    let (tx, mut rx) = mpsc::channel::<Wire>(16);
    *state.outgoing.lock().await = Some(tx);

    // 本地代理:每条入站 TCP 对应一条 iroh 代理流,WebView 由此拿到主机的完整
    // dsh web UI。端口按主机派生,同一台主机每次都是同一个源(见 proxy_port_for)。
    match start_proxy(app.clone(), conn.clone()).await {
        Ok(port) => {
            *state.proxy_port.lock().await = Some(port);
            let _ = app.emit("remote:proxy-ready", serde_json::json!({ "url": format!("http://127.0.0.1:{port}/") }));
        }
        Err(e) => emit_state(app, "connecting", format!("{}{e:#}", t("代理启动失败: ", "proxy failed to start: "))),
    }
    emit_state(app, "connected", t("已连接", "Connected"));

    let writer = async {
        while let Some(msg) = rx.recv().await {
            let line = serde_json::to_string(&msg).expect("Wire is serializable");
            if write_line(&mut send, &line).await.is_err() {
                break;
            }
        }
    };
    let reader = async {
        loop {
            let line = match read_line_bounded(&mut recv, MAX_LINE).await {
                Ok(l) => l,
                Err(_) => break,
            };
            match serde_json::from_str::<Wire>(&line) {
                Ok(Wire::Approval { id, tool_name, reason }) => {
                    let _ = app.emit("remote:approval", ApprovalEvent { id, tool_name, reason });
                }
                Ok(Wire::ApprovalCancel { id }) => {
                    let _ = app.emit("remote:approval-cancel", serde_json::json!({ "id": id }));
                }
                _ => {}
            }
        }
    };
    tokio::join!(writer, reader);
    Ok(())
}

/// 本地代理端口按主机 ID 派生:dsh 的 web UI 把「当前会话」等状态按源存在
/// localStorage 里,端口每次随机就是每次新源、存储为空,页面只能走「连最近
/// 工作区、没有空白会话就新建」那条路——配上一进会话就写入历史的预设
/// (如 Prefab Anchored Standard),每开一次 App 就多一条会话。
/// 不同主机派生出不同端口,各自的存储互不串。范围避开 Android(32768 起)与
/// iOS(49152 起)的临时端口区,降低被出站连接占走的概率。
fn proxy_port_for(peer: &EndpointId) -> u16 {
    let b = peer.as_bytes();
    20000 + u16::from_be_bytes([b[0], b[1]]) % 12000
}

/// 起本地 TCP 监听,每条连接开一条 iroh 代理流转发;返回实际端口。
/// 派生端口被占时退回 OS 分配:这一次不记状态,连接本身不受影响。
/// 监听任务随连接生命周期结束:连接断开后 open_bi 失败,浏览器侧表现为加载失败。
async fn start_proxy(app: AppHandle, conn: iroh::endpoint::Connection) -> Result<u16> {
    let preferred = proxy_port_for(&conn.remote_id());
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", preferred)).await {
        Ok(listener) => listener,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context(t("本地代理监听失败", "the local proxy could not listen"))?,
    };
    let port = listener.local_addr()?.port();
    tauri::async_runtime::spawn(async move {
        let _keep_app_alive = app;
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else { break };
            let conn = conn.clone();
            tauri::async_runtime::spawn(async move {
                let Ok((mut send, recv)) = conn.open_bi().await else { return };
                let line = serde_json::to_string(&Wire::Proxy).expect("Wire is serializable");
                if write_line(&mut send, &line).await.is_err() {
                    return;
                }
                let stream = tokio::io::join(recv, send);
                // 拆分 tcp / iroh 流,在各自的读方向外各包一层计数包装器,累计本次连接的流量
                let (tcp_r, mut tcp_w) = tokio::io::split(tcp);
                let (strm_r, mut strm_w) = tokio::io::split(stream);
                let mut tx = CountedReader { inner: tcp_r, counter: &TX_BYTES };
                let mut rx = CountedReader { inner: strm_r, counter: &RX_BYTES };
                let a = tokio::io::copy(&mut tx, &mut strm_w);
                let b = tokio::io::copy(&mut rx, &mut tcp_w);
                let _ = tokio::join!(a, b);
            });
        }
    });
    Ok(port)
}

fn parse_peer(peer: &str) -> Result<EndpointId, String> {
    peer.trim().parse().map_err(|e| format!("{}{e}", t("主机 ID 无效: ", "invalid computer ID: ")))
}

fn parse_direct_addr(addr: &str) -> Result<std::net::SocketAddr, String> {
    addr.trim().parse().map_err(|e| format!("{}{e}", t("直连地址无效: ", "invalid direct address: ")))
}

#[tauri::command]
fn list_hosts(app: AppHandle) -> HostBook {
    load_book(&app)
}

/// 界面把系统语言报进来,Rust 侧的报错文案照它出中文或英文
#[tauri::command]
fn set_lang(tag: String) {
    tether_core::i18n::set_from_tag(&tag);
}

/// 构建时写死的版本号,避免界面上再手抄一份而漂移
#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 本次连接累计流量
#[derive(Serialize)]
struct TrafficStats { tx_bytes: u64, rx_bytes: u64 }

#[tauri::command]
fn traffic_stats() -> TrafficStats {
    TrafficStats {
        tx_bytes: TX_BYTES.load(Ordering::Relaxed),
        rx_bytes: RX_BYTES.load(Ordering::Relaxed),
    }
}

/// 给已保存的主机改名。凭证不变,只动本地显示名。
#[tauri::command]
fn rename_host(app: AppHandle, id: String, label: String) -> Result<HostBook, String> {
    let mut book = load_book(&app);
    let Some(host) = book.hosts.iter_mut().find(|h| h.id == id) else {
        return Err(t("这台电脑不在已配对列表里", "that computer is not in the paired list").into());
    };
    host.label = label.trim().to_string();
    save_book(&app, &book).map_err(|e| format!("{e:#}"))?;
    Ok(book)
}

#[tauri::command]
fn forget_host(app: AppHandle, id: String) -> Result<HostBook, String> {
    let mut book = load_book(&app);
    book.hosts.retain(|h| h.id != id);
    if book.current.as_deref() == Some(id.as_str()) {
        book.current = book.hosts.first().map(|h| h.id.clone());
    }
    save_book(&app, &book).map_err(|e| format!("{e:#}"))?;
    Ok(book)
}

#[tauri::command]
async fn pair(
    app: AppHandle,
    peer: String,
    code: String,
    name: String,
    label: String,
) -> Result<(), String> {
    let peer = parse_peer(&peer)?;
    let code = code.trim().to_string();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(t("配对码应为 6 位数字", "the pairing code must be 6 digits").into());
    }
    let name = if name.trim().is_empty() { t("我的手机", "My phone").to_string() } else { name.trim().to_string() };
    tauri::async_runtime::spawn(run_connection(
        app,
        peer,
        Wire::Pair { code, name },
        Some(label.trim().to_string()),
        None,
    ));
    Ok(())
}

/// 连接指定主机;不给 id 就连上次那台。选定的会记为 current。
#[tauri::command]
async fn connect(app: AppHandle, id: Option<String>) -> Result<(), String> {
    let mut book = load_book(&app);
    let target = id.or_else(|| book.current.clone()).or_else(|| book.hosts.first().map(|h| h.id.clone()));
    let Some(target) = target else {
        return Err(t("还没有配对过的电脑", "no computer has been paired yet").into());
    };
    // 主机簿里若记着直连地址就走直连;否则退回 discovery(跨网络场景仍可用)
    let direct = {
        let host = book.hosts.iter().find(|h| h.id == target);
        let Some(host) = host else {
            return Err(t("这台电脑不在已配对列表里", "that computer is not in the paired list").into());
        };
        host.addr.as_deref().map(parse_direct_addr).transpose()?
    };
    if book.current.as_deref() != Some(target.as_str()) {
        book.current = Some(target.clone());
        save_book(&app, &book).map_err(|e| format!("{e:#}"))?;
    }
    let peer = parse_peer(&target)?;
    let name = t("我的手机", "My phone").to_string();
    tauri::async_runtime::spawn(run_connection(app, peer, Wire::Hello { name }, None, direct));
    Ok(())
}

/// 直连指定主机:输 ip:port,不经过 discovery。带配对码则首次配对,否则按已配对直连。
#[tauri::command]
async fn connect_direct(
    app: AppHandle,
    peer: String,
    addr: String,
    name: String,
    code: Option<String>,
    label: String,
) -> Result<(), String> {
    let peer = parse_peer(&peer)?;
    let socket_addr = parse_direct_addr(&addr)?;
    let name = if name.trim().is_empty() { t("我的手机", "My phone").to_string() } else { name.trim().to_string() };
    let (first, pair_label) = match code {
        Some(code) => {
            let code = code.trim().to_string();
            if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
                return Err(t("配对码应为 6 位数字", "the pairing code must be 6 digits").into());
            }
            (Wire::Pair { code, name }, Some(label.trim().to_string()))
        }
        None => (Wire::Hello { name }, None),
    };
    tauri::async_runtime::spawn(run_connection(app, peer, first, pair_label, Some(socket_addr)));
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalStatus {
    available: bool,
    running: bool,
    url: Option<String>,
}

/// 本地模式状态:这个构建有没有运行时、dsh 是否在跑、在跑的话 WebView 该加载哪个 URL
#[tauri::command]
async fn local_status(app: AppHandle) -> LocalStatus {
    #[cfg(target_os = "android")]
    {
        let state = app.state::<AppState>();
        let mut guard = state.local.lock().await;
        // 进程死了就当没在跑,免得把死 URL 交给 WebView
        if guard.as_mut().is_some_and(|h| !h.alive()) {
            *guard = None;
        }
        return LocalStatus {
            available: local::available(),
            running: guard.is_some(),
            url: guard.as_ref().map(|h| h.url()),
        };
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        LocalStatus { available: false, running: false, url: None }
    }
}

/// 起本机 dsh;已在跑就直接返回现有 URL
#[tauri::command]
async fn local_start(app: AppHandle) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        let state = app.state::<AppState>();
        let mut guard = state.local.lock().await;
        if let Some(host) = guard.as_mut() {
            if host.alive() {
                return Ok(host.url());
            }
            *guard = None;
        }
        let (host, url) = local::start(&app).await.map_err(|e| format!("{e:#}"))?;
        *guard = Some(host);
        // 前台通知让系统别在退后台时收掉进程;起不来只影响保活,不影响使用
        if let Err(e) = tauri_plugin_dshlocal::start_service(&app, "DSH Tether", t("本机 DSH 运行中", "DSH is running on this phone")) {
            eprintln!("[local] {}{e}", t("前台服务启动失败: ", "the foreground service failed to start: "));
        }
        Ok(url)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err(t("这个平台没有本地模式", "this platform has no local mode").into())
    }
}

#[tauri::command]
async fn local_stop(app: AppHandle) {
    #[cfg(target_os = "android")]
    {
        let state = app.state::<AppState>();
        let host = state.local.lock().await.take();
        if let Some(host) = host {
            host.stop().await;
        }
        let _ = tauri_plugin_dshlocal::stop_service(&app);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
    }
}

/// 本机 dsh 的最近输出;启动失败时给用户看原因
#[tauri::command]
async fn local_log(app: AppHandle) -> Vec<String> {
    #[cfg(target_os = "android")]
    {
        let state = app.state::<AppState>();
        return match state.local.lock().await.as_ref() {
            Some(host) => host.log_tail().await,
            None => Vec::new(),
        };
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Vec::new()
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dshlocal::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_hosts,
            rename_host,
            forget_host,
            pair,
            connect,
            connect_direct,
            app_version,
            traffic_stats,
            set_lang,
            local_status,
            local_start,
            local_stop,
            local_log
        ])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}
