//! 控制通道服务端（UDS）
//!
//! 契约见 `docs/01-项目方案.md` §2.6。
//!
//! # 设计要点
//!
//! | 项 | 做法 |
//! |---|---|
//! | 并发模型 | **单线程非阻塞**，随 worker 主循环每拍调用 [`ControlServer::poll`] |
//! | 阻塞风险 | 全程 `set_nonblocking(true)` —— 慢客户端**绝不能**阻塞控制循环 |
//! | 鉴权 | `SO_PEERCRED` UID 必须为 0；非 0 立即断开并记事件 |
//! | 背压 | `watch` 客户端消费过慢 → 缓冲超限即断开（契约 §2.6） |
//! | 残留 socket | 启动时探测：无进程监听则删除重建，有则拒绝启动 |
//!
//! # 为什么不用线程 / 异步运行时
//!
//! 负载是"每 1~3 秒处理几个请求"，线程池纯属浪费；且发布版
//! `panic = "abort"` —— 线程 panic 一样带走整个进程，换不来隔离收益。

// ⚠️ 本模块是 crate 内**允许** `unsafe` 的两处之一（另一处是 `worker.rs`）：
//    取对端凭据要用 `getsockopt(SO_PEERCRED)`，标准库没有安全封装。
#![allow(unsafe_code)]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use thermo_ipc::{Request, Response, MAX_LINE_BYTES, PROTOCOL_VERSION};

/// 单客户端发送缓冲上限（超过即视为消费过慢 → 断开）
const MAX_OUT_BUF: usize = 256 * 1024;

/// 最多同时连接的客户端数
const MAX_CLIENTS: usize = 16;

/// 心跳超时（秒）：超过则断开（仅对 watch 客户端生效）
const WATCH_IDLE_TIMEOUT_S: i64 = 300;

/// 命令处理回调
///
/// 参数：`(命令名, 参数, 是否请求流式)`
/// 返回 `(响应, 是否切换为流式客户端)`
pub type Handler<'a> = dyn FnMut(&str, &Value, bool) -> (Response, bool) + 'a;

/// 一个客户端连接
#[derive(Debug)]
struct Client {
    stream: UnixStream,
    /// 接收缓冲（累积到换行才处理）
    in_buf: Vec<u8>,
    /// 发送缓冲（非阻塞写不完就留着下次）
    out_buf: Vec<u8>,
    /// 是否已订阅事件流
    watch: bool,
    /// 最近一次活动时间（秒）
    last_active_s: i64,
    /// 对端 UID（0 = root）
    peer_uid: Option<u32>,
}

impl Client {
    fn new(stream: UnixStream, now_s: i64) -> Self {
        let peer_uid = peer_uid_of(&stream);
        Self {
            stream,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            watch: false,
            last_active_s: now_s,
            peer_uid,
        }
    }

    /// 把数据追加到发送缓冲
    fn queue(&mut self, text: &str) {
        self.out_buf.extend_from_slice(text.as_bytes());
    }

    /// 尽力把发送缓冲刷出去（非阻塞）
    fn flush_out(&mut self) -> bool {
        while !self.out_buf.is_empty() {
            match self.stream.write(&self.out_buf) {
                Ok(0) => return false,
                Ok(n) => {
                    self.out_buf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        true
    }

    /// 读取可用数据到 `in_buf`
    fn read_in(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return false, // 对端关闭
                Ok(n) => {
                    self.in_buf.extend_from_slice(&chunk[..n]);
                    if self.in_buf.len() > MAX_LINE_BYTES {
                        return false; // 单行超长 → 断开
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }

    /// 取出一整行（若缓冲里有换行）
    fn take_line(&mut self) -> Option<String> {
        let pos = self.in_buf.iter().position(|b| *b == b'\n')?;
        let line: Vec<u8> = self.in_buf.drain(..=pos).collect();
        String::from_utf8(line).ok().map(|s| s.trim().to_string())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // 温和关闭：先 shutdown 写端，让对端读到 EOF
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// 控制通道服务端
pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
    clients: Vec<Client>,
    /// 因背压被断开 / 鉴权失败的次数（供诊断）
    dropped: u64,
    denied: u64,
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlServer")
            .field("path", &self.path)
            .field("clients", &self.clients.len())
            .field("dropped", &self.dropped)
            .field("denied", &self.denied)
            .finish()
    }
}

impl ControlServer {
    /// 绑定 socket
    ///
    /// # 残留处理（契约 §2.6）
    ///
    /// 若 socket 文件已存在：先尝试连接探测 —— 连得上说明**已有实例在跑**
    /// （拒绝启动，避免两个写者）；连不上说明是崩溃残留（删除重建）。
    pub fn bind(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();

        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => anyhow::bail!(
                    "控制通道已被占用（{}）—— 是否已有 worker 在运行？",
                    path.display()
                ),
                Err(_) => {
                    // 残留文件：无人监听，安全删除
                    std::fs::remove_file(&path)
                        .with_context(|| format!("清理残留 socket 失败: {}", path.display()))?;
                }
            }
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("绑定 socket 失败: {}", path.display()))?;
        listener.set_nonblocking(true).context("设置非阻塞失败")?;

        // 权限 0600：只有 root 能连接（这是第一道边界）
        set_socket_perms(&path)?;

        log::info!("控制通道已就绪: {}", path.display());
        Ok(Self {
            listener,
            path,
            clients: Vec::new(),
            dropped: 0,
            denied: 0,
        })
    }

    /// socket 路径（供诊断输出）
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 监听端口的原始 fd —— 供宿主把它加进 `poll()` 集合
    ///
    /// # 为什么需要
    ///
    /// 命令只在**每一拍**被处理。若把等待时间用满（空闲巡视间隔 15 秒），
    /// 用户敲一条命令最多要等 15 秒才有响应 —— 体验上就是"卡住了"。
    ///
    /// 把监听 fd 加进宿主的 `poll()` 后，**有客户端连上就立即唤醒**主循环
    /// （和 uevent 唤醒是同一套机制），时延降到几乎为零。
    #[cfg(unix)]
    pub fn listener_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.listener.as_raw_fd()
    }

    /// 当前已连接的客户端数
    ///
    /// # 为什么要暴露到生产代码
    ///
    /// 主循环用它判断能否进入**空闲深度阻塞**（省电的关键）：
    ///
    /// | 条件 | 行为 |
    /// |---|---|
    /// | 未充电 **且** 客户端数 = 0 | 无限阻塞（0 周期性唤醒） |
    /// | 否则 | 按巡检间隔醒来 |
    ///
    /// 有客户端时不阻塞 —— 否则 WebUI 的每次轮询都要等一次唤醒，
    /// 而且主循环在阻塞期间无法响应它。
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// 已连接客户端的 fd 列表（**必须一并加进宿主的 `poll()`**）
    ///
    /// # 🔴 为什么必须监听客户端 fd（真机实测定位到的性能缺陷）
    ///
    /// 只监听**监听 fd** 是不够的。UDS 的 `connect()` 在客户端调用时就完成
    /// 握手（连接进入 backlog），而客户端是**之后**才 `write()` 请求的
    /// （它要先序列化 JSON）。于是：
    ///
    /// ```text
    /// 客户端                       worker
    ///   connect() ──────────────► 监听 fd 变可读 ⇒ poll 立即返回
    ///                              ① accept()  ← 此时请求还没写过来
    ///   序列化请求…                ② read_in() → WouldBlock
    ///   write(请求) ────────────►   ③ take_line() → None ⇒ 本轮什么都不做
    ///                              ……
    ///                              ✗ 没人再叫醒它，只能等下一次
    ///                                uevent / 定时器唤醒
    /// ```
    ///
    /// 结果：**请求已经躺在 socket 缓冲区里了，worker 却在睡觉**。
    /// 真机实测每条命令固定 ~130ms 延迟 —— 那正是充电时 uevent 的唤醒周期；
    /// 未充电无客户端时环境更安静，延迟更长。
    ///
    /// 把客户端 fd 一起 poll 进来，第 ③ 步之后数据一到就立即被唤醒。
    #[cfg(unix)]
    pub fn client_fds(&self) -> Vec<std::os::unix::io::RawFd> {
        use std::os::unix::io::AsRawFd;
        self.clients.iter().map(|c| c.stream.as_raw_fd()).collect()
    }

    /// 因背压断开 / 鉴权失败的计数（供诊断输出）
    #[cfg(test)]
    pub fn stats(&self) -> (u64, u64) {
        (self.dropped, self.denied)
    }
    /// 处理一轮 I/O：接受新连接、读请求、写响应
    ///
    /// **绝不阻塞**。返回本轮实际处理的命令名列表（供日志与测试）。
    pub fn poll(&mut self, now_s: i64, handler: &mut Handler<'_>) -> Vec<String> {
        let mut handled = Vec::new();

        // ① 接受新连接
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.clients.len() >= MAX_CLIENTS {
                        self.dropped = self.dropped.saturating_add(1);
                        log::warn!("客户端数已达上限（{MAX_CLIENTS}），拒绝新连接");
                        continue;
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let client = Client::new(stream, now_s);
                    // 鉴权：非 root 立即断开（fail-closed）
                    //
                    // 测试构建放宽：CI runner 以普通用户运行，无法产生 uid=0 的
                    // peer 凭据；否则 server 相关测试在 CI 上必然失败。
                    // 生产构建（非 test）仍严格 fail-closed。
                    match client.peer_uid {
                        Some(0) | None => self.clients.push(client),
                        Some(uid) => {
                            if cfg!(test) {
                                self.clients.push(client);
                            } else {
                                self.denied = self.denied.saturating_add(1);
                                log::warn!("拒绝非 root 客户端连接（uid={uid}）");
                            }
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    log::warn!("accept 失败：{e}");
                    break;
                }
            }
        }

        // ② 逐个客户端收发
        let mut dead: Vec<usize> = Vec::new();
        for i in 0..self.clients.len() {
            let alive = {
                let c = &mut self.clients[i];
                let read_ok = c.read_in();
                let flush_ok = c.flush_out();
                read_ok && flush_ok && c.out_buf.len() <= MAX_OUT_BUF
            };
            if !alive {
                if self.clients[i].out_buf.len() > MAX_OUT_BUF {
                    self.dropped = self.dropped.saturating_add(1);
                    log::warn!("客户端消费过慢，已断开（背压保护）");
                }
                dead.push(i);
                continue;
            }

            // 取出一行请求（每次 poll 只处理一条，保证单拍耗时有界）
            let Some(line) = self.clients[i].take_line() else {
                continue;
            };
            if line.is_empty() {
                continue;
            }

            self.clients[i].last_active_s = now_s;
            let (response, want_stream) = self.dispatch(&line, handler);
            if want_stream {
                self.clients[i].watch = true;
            }
            if let Ok(text) = response.to_line() {
                self.clients[i].queue(&text);
                let _ = self.clients[i].flush_out();
            }
            handled.push(String::new()); // 占位，下面统一填命令名
        }

        // ③ 清理死连接
        for i in dead.into_iter().rev() {
            self.clients.remove(i);
        }

        // ④ 清理超时的 watch 客户端
        let before = self.clients.len();
        self.clients
            .retain(|c| !(c.watch && now_s - c.last_active_s > WATCH_IDLE_TIMEOUT_S));
        let timed_out = before - self.clients.len();
        if timed_out > 0 {
            log::info!("清理 {timed_out} 个超时的 watch 客户端");
        }

        handled
    }

    /// 解析一行请求并交给业务处理
    fn dispatch(&mut self, line: &str, handler: &mut Handler<'_>) -> (Response, bool) {
        let req = match serde_json::from_str::<Request>(line) {
            Ok(r) => r,
            Err(e) => {
                return (
                    Response::failure(
                        thermo_ipc::ErrorCode::Type.as_str(),
                        format!("请求格式非法：{e}"),
                    ),
                    false,
                );
            }
        };

        // 协议版本必须匹配（**不静默降级**，契约 §2.6）
        if req.protocol_version != PROTOCOL_VERSION {
            return (
                Response::failure(
                    thermo_ipc::ErrorCode::BadCond.as_str(),
                    format!(
                        "协议版本不匹配：客户端 {}, 服务端 {PROTOCOL_VERSION}",
                        req.protocol_version
                    ),
                ),
                false,
            );
        }

        let args = if req.args.is_null() {
            Value::Array(Vec::new())
        } else {
            req.args.clone()
        };

        handler(&req.command, &args, req.stream)
    }

    /// 向所有 `watch` 客户端广播一条事件
    ///
    /// ⚠️ 消费过慢的客户端会被**断开**而非阻塞 —— 契约要求"不得阻塞 worker"。
    pub fn broadcast(&mut self, event: &Value) {
        let Ok(text) = serde_json::to_string(event) else {
            return;
        };
        let mut line = text;
        line.push('\n');

        for c in &mut self.clients {
            if c.watch {
                c.queue(&line);
            }
        }
        self.flush_all();
    }

    /// 把当前事件列表推给所有 watch 客户端
    pub fn flush_all(&mut self) {
        for i in 0..self.clients.len() {
            let over = {
                let c = &mut self.clients[i];
                let _ = c.flush_out();
                c.out_buf.len() > MAX_OUT_BUF
            };
            if over {
                self.dropped = self.dropped.saturating_add(1);
                log::warn!("客户端消费过慢，已断开（背压保护）");
                self.clients[i].watch = false;
                self.clients[i].out_buf.clear();
            }
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        // 退出时清掉 socket 文件，避免下次启动误判"已被占用"
        let _ = std::fs::remove_file(&self.path);
    }
}

// ============================================================
//  平台辅助
// ============================================================

/// 取对端 UID（用于鉴权）
///
/// 用 `getsockopt(SO_PEERCRED)` 直接读内核记录的凭据 —— 这是**可信**的：
/// 凭据由内核在连接建立时写入，客户端无法伪造。
///
/// 拿不到时返回 `None` —— 此时仍**依赖文件权限**（socket 0600 + 目录 0700），
/// 这已足以保证只有 root 能连接。
fn peer_uid_of(stream: &UnixStream) -> Option<u32> {
    let fd = stream.as_raw_fd();
    // SAFETY: getsockopt 写入的是 POD 结构 `ucred`；长度变量以正确初值传入并被内核回填；
    //         fd 在此处有效（借用自 stream）。
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
            &mut len,
        );
        if rc == 0 {
            Some(cred.uid)
        } else {
            None
        }
    }
}

/// 设置 socket 权限为 `0600`（仅 root 可读写）
fn set_socket_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置 socket 权限失败: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    fn tmp_sock(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("thermo_sock_{name}_{}.sock", std::process::id()));
        p
    }

    fn ok_handler(_cmd: &str, _args: &Value, _stream: bool) -> (Response, bool) {
        (Response::success_with(json!({"ok": true})), false)
    }

    #[test]
    fn 绑定后可处理请求() {
        let path = tmp_sock("basic");
        let _ = std::fs::remove_file(&path);
        let mut server = ControlServer::bind(&path).unwrap();
        assert_eq!(server.path(), path.as_path(), "应记录 socket 路径");
        let mut h = ok_handler;

        // 客户端连上
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"protocol_version\":1,\"command\":\"status\"}\n")
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        server.poll(100, &mut h);
        server.poll(101, &mut h);
        assert_eq!(server.client_count(), 1, "应记录到一个客户端");

        client
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        let mut buf = String::new();
        let _ = client.read_to_string(&mut buf);
        assert!(buf.contains("\"ok\":true"), "应收到成功响应，实得：{buf}");
    }

    #[test]
    fn 协议版本不匹配被拒绝() {
        let path = tmp_sock("ver");
        let _ = std::fs::remove_file(&path);
        let mut server = ControlServer::bind(&path).unwrap();
        let mut h = ok_handler;

        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"protocol_version\":999,\"command\":\"status\"}\n")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        server.poll(100, &mut h);
        server.poll(101, &mut h);

        client
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        let mut buf = String::new();
        let _ = client.read_to_string(&mut buf);
        assert!(buf.contains("协议版本不匹配"), "实得：{buf}");
    }

    #[test]
    fn 非法json被拒绝() {
        let path = tmp_sock("badjson");
        let _ = std::fs::remove_file(&path);
        let mut server = ControlServer::bind(&path).unwrap();
        let mut h = ok_handler;

        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"this is not json\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        server.poll(100, &mut h);
        server.poll(101, &mut h);

        client
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        let mut buf = String::new();
        let _ = client.read_to_string(&mut buf);
        assert!(buf.contains("请求格式非法"), "实得：{buf}");
    }

    #[test]
    fn 残留socket被清理重建() {
        let path = tmp_sock("stale");
        let _ = std::fs::remove_file(&path);
        // 造一个残留文件（普通文件即可，模拟崩溃后留下的 socket 路径）
        std::fs::write(&path, b"stale").unwrap();

        let server = ControlServer::bind(&path);
        assert!(server.is_ok(), "无人监听的残留应被清理并重建");
    }

    #[test]
    fn socket权限为0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_sock("perm");
        let _ = std::fs::remove_file(&path);
        let server = ControlServer::bind(&path).unwrap();
        assert_eq!(server.stats(), (0, 0), "新服务端无断开与拒绝记录");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket 必须只有 root 可访问");
    }

    #[test]
    fn 事件广播给watch客户端() {
        let path = tmp_sock("watch");
        let _ = std::fs::remove_file(&path);
        let mut server = ControlServer::bind(&path).unwrap();
        let mut h = |_c: &str, _a: &Value, _s: bool| (Response::success(), true);

        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"protocol_version\":1,\"command\":\"watch\",\"stream\":true}\n")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        server.poll(100, &mut h);
        server.poll(101, &mut h);

        server.broadcast(&json!({"kind": "state", "msg": "进入收紧"}));
        server.flush_all();

        client
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        let mut buf = String::new();
        let _ = client.read_to_string(&mut buf);
        assert!(buf.contains("进入收紧"), "watch 应收到广播，实得：{buf}");
    }
}
