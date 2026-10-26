//! UDS 客户端（`ctl` / `watch` 侧）
//!
//! 契约：socket 为 `0600 root:root`；服务端校验 `SO_PEERCRED` UID 必须为 0。
//!
//! ⚠️ 本模块依赖 Unix domain socket，因此仅在 Unix 平台（Android / Linux）
//! 提供真实实现；Windows 上给出明确的错误提示。
//!
//! # 超时（真机事故后新增）
//!
//! 真机实测：worker 若卡在 `D` 状态（写 sysfs 让内核驱动死锁），
//! 控制 socket 仍在监听但**永远不回复** —— 此时 `ctl` 会永久挂起。
//!
//! 因此读写都必须带超时。另外要意识到：空闲时命令最多要等一个**空闲巡检
//! 间隔**（15 秒）才被主循环处理，所以超时必须显著大于它。

use anyhow::{Context, Result};

/// 读响应的超时（秒）
///
/// worker 只在**每拍**处理控制请求；空闲巡检间隔是 15 秒，
/// 因此 20 秒足够覆盖"刚好错过一拍"的最坏情况，又能及时发现卡死。
const READ_TIMEOUT_S: u64 = 20;

/// 写请求的超时（秒）—— 请求很小，正常是瞬时的
const WRITE_TIMEOUT_S: u64 = 5;

/// 发送一条命令并渲染响应
pub fn send(command: &str, args: &[String], plain: bool, pretty: bool) -> Result<()> {
    let resp = roundtrip(command, args, false)?;
    render(&resp, plain, pretty)
}

/// 连接控制通道；失败时**自愈拉起**后台进程并重试一次
///
/// # 为什么需要它（2026-10-26 真机：隔几天就打不开状态页）
///
/// 真机现象：WebUI 报「连接 worker 失败（`control.sock` 不存在）」，
/// **必须重启手机**才能恢复，隔几天出现一次。
///
/// 根因：supervisor 是 worker 的唯一拉起者，而它自己只由 `service.sh`
/// 在开机时拉起一次；被系统回收后无人再拉起它 ⇒ worker 也消失、
/// socket 不存在。详见 [`crate::supervisor::ensure_running`]。
///
/// 自愈只**拉起进程**，不写任何充电节点（R1）；拉起后仍由 worker 决策。
#[cfg(unix)]
fn connect_client() -> Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;

    let sock = crate::paths::control_sock();
    match UnixStream::connect(&sock) {
        Ok(conn) => Ok(conn),
        Err(_) => {
            let outcome = crate::supervisor::ensure_running();
            log::debug!("连接 worker 失败，自愈结果：{outcome:?}");

            UnixStream::connect(&sock).with_context(|| {
                format!(
                    "连接 worker 失败（{}）—— {}\n\
                     常见原因：模块刚启动还没就绪（等几秒重试）、或后台进程已被系统回收。",
                    sock.display(),
                    outcome.hint()
                )
            })
        }
    }
}

/// 订阅事件流（长连接）
pub fn watch() -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("watch 仅在 Unix 平台（Android / Linux）可用");

    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Write};

        let conn = connect_client()?;

        // ⚠️ `watch` 是长连接且**故意不设读超时** —— 事件本来就可能几分钟才有。
        //    但写请求仍要设超时，否则连接卡住时连订阅请求都发不出去。
        conn.set_write_timeout(Some(std::time::Duration::from_secs(WRITE_TIMEOUT_S)))
            .ok();

        let req = serde_json::json!({
            "protocol_version": thermo_ipc::PROTOCOL_VERSION,
            "command": "watch",
            "args": [],
            "stream": true,
        });
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        let mut w = conn.try_clone().context("复制连接失败")?;
        w.write_all(line.as_bytes()).context("发送订阅请求失败")?;
        w.flush().ok();

        let reader = BufReader::new(conn);
        for l in reader.lines() {
            match l {
                Ok(text) if !text.trim().is_empty() => println!("{text}"),
                Ok(_) => {}
                Err(e) => {
                    log::warn!("事件流中断：{e}");
                    break;
                }
            }
        }
        Ok(())
    }
}

/// 一次往返：发请求、读一行响应
#[cfg(unix)]
fn roundtrip(command: &str, args: &[String], want_stream: bool) -> Result<String> {
    use std::io::{BufReader, Read, Write};
    use std::time::{Duration, Instant};

    let mut conn = connect_client()?;

    conn.set_write_timeout(Some(Duration::from_secs(WRITE_TIMEOUT_S)))
        .context("设置写超时失败")?;

    let req = serde_json::json!({
        "protocol_version": thermo_ipc::PROTOCOL_VERSION,
        "command": command,
        "args": args,
        "stream": want_stream,
    });
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');

    conn.write_all(line.as_bytes()).context("发送请求失败")?;
    conn.flush().context("刷新缓冲失败")?;

    // ── 逐块读，自己掌控总超时 ──
    //
    // 不使用 `set_read_timeout` + `read_line`：那样超时后缓冲区里的
    // 半截数据会丢失，且难以给出"等了多少秒"的准确提示。
    let mut reader = BufReader::new(conn);
    let mut resp = Vec::new();
    let mut buf = [0u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(READ_TIMEOUT_S);

    loop {
        // 剩余时间 → 每轮重设读超时
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            anyhow::bail!(
                "等待 worker 响应超时（{} 秒）。\
                 worker 可能已卡死 —— 请查看 {}/events.jsonl，必要时重启设备",
                READ_TIMEOUT_S,
                crate::paths::DATA_DIR
            );
        }
        // BufReader 借用的是内部流；读超时必须设在底层 socket 上
        reader
            .get_ref()
            .set_read_timeout(Some(left))
            .context("设置读超时失败")?;

        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                resp.extend_from_slice(&buf[..n]);
                if resp.contains(&b'\n') {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                anyhow::bail!(
                    "等待 worker 响应超时（{} 秒）。\
                     worker 可能已卡死 —— 请查看 {}/events.jsonl，必要时重启设备",
                    READ_TIMEOUT_S,
                    crate::paths::DATA_DIR
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("读取响应失败"),
        }
    }

    if resp.is_empty() {
        anyhow::bail!("worker 未返回任何内容（连接被立即关闭？）");
    }

    Ok(String::from_utf8_lossy(&resp).into_owned())
}

#[cfg(not(unix))]
fn roundtrip(_command: &str, _args: &[String], _want_stream: bool) -> Result<String> {
    anyhow::bail!("ctl 仅在 Unix 平台（Android / Linux）可用")
}

/// 按输出模式渲染响应
fn render(raw: &str, plain: bool, pretty: bool) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(raw.trim()).context("响应不是合法 JSON")?;

    // ⚠️ 契约提示：`Response` 是 untagged，`{"ok":false,…}` 也可能被解析成 Ok 变体，
    //    因此**必须再按 `ok` 字段判一次**，不能只看变体。
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        let code = value
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("E_INTERNAL");
        // 失败信息**必须走 stderr**，stdout 保持为空
        eprintln!("{}", serde_json::to_string(&value)?);
        std::process::exit(exit_code_for(code));
    }

    if plain {
        if let Some(data) = value.get("data") {
            print_flat(data, "");
        }
        return Ok(());
    }

    if pretty {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(())
}

/// 扁平化为 `key=value` 行（点号键）
fn print_flat(value: &serde_json::Value, prefix: &str) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                print_flat(v, &key);
            }
        }
        serde_json::Value::String(s) => println!("{prefix}={s}"),
        serde_json::Value::Number(n) => println!("{prefix}={n}"),
        serde_json::Value::Bool(b) => println!("{prefix}={b}"),
        // Null 与数组不参与扁平化（契约：数组只走 JSON）
        serde_json::Value::Null | serde_json::Value::Array(_) => {}
    }
}

/// 错误码 → 退出码（契约冻结，见 `thermo-ipc`）
fn exit_code_for(code: &str) -> i32 {
    match code {
        "E_INTERNAL" => 1,
        "E_UNKNOWN_KEY" | "E_TYPE" | "E_RANGE" | "E_READONLY" | "E_CONFIRM" | "E_COLLISION" => 2,
        "E_NO_PERM" => 3,
        "E_NO_NODE" | "E_NOT_CHARGING" | "E_BAD_COND" | "E_NO_SOURCE" => 4,
        "E_BUSY" => 5,
        "E_SAFE_MODE" | "E_MODE_LIMIT" => 6,
        _ => 1,
    }
}
