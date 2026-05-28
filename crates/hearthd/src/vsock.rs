use crate::{
    config::Config,
    registry::{Registry, Service},
    Daemon, Dispatch,
};
use anyhow::{anyhow, Context, Result};
use hearth_proto::{Request, Response};
use libc::{c_int, sockaddr, sockaddr_vm, socklen_t, AF_VSOCK, SOCK_STREAM};
use serde_json::json;
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    mem,
    os::fd::{FromRawFd, RawFd},
    thread,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

const VMADDR_CID_ANY: u32 = u32::MAX;

impl<H: crate::host::Host + 'static> Daemon<H> {
    pub async fn spawn_vsock_listener(&self) -> Result<Option<thread::JoinHandle<()>>> {
        if self.cfg.disable_vsock {
            info!("vsock listener disabled by config");
            return Ok(None);
        }
        let Some(agent) = agent_in_charge(&self.cfg).await? else {
            warn!("no agent-in-charge configured; vsock listener not started");
            return Ok(None);
        };
        let daemon = self.clone();
        let runtime = tokio::runtime::Handle::current();
        let port = self.cfg.vsock_port;
        let allowed_cid = agent.vsock_cid;
        let handle = thread::Builder::new()
            .name("hearthd-vsock".to_string())
            .spawn(move || {
                if let Err(err) = daemon.run_vsock_listener(runtime, port, allowed_cid) {
                    error!(error = %err, port, allowed_cid, "vsock listener stopped");
                }
            })
            .context("spawn vsock listener thread")?;
        Ok(Some(handle))
    }

    fn run_vsock_listener(
        &self,
        runtime: tokio::runtime::Handle,
        port: u32,
        allowed_cid: u32,
    ) -> Result<()> {
        let fd = bind_vsock(port)?;
        info!(port, allowed_cid, "vsock listener ready");
        loop {
            let (client_fd, peer_cid) = accept_vsock(fd)?;
            if peer_cid != allowed_cid {
                warn!(
                    peer_cid,
                    allowed_cid, "dropping unauthorized vsock connection"
                );
                close_fd(client_fd);
                continue;
            }
            let daemon = self.clone();
            let runtime = runtime.clone();
            thread::Builder::new()
                .name(format!("hearthd-vsock-{peer_cid}"))
                .spawn(move || {
                    if let Err(err) = daemon.handle_blocking_fd(runtime, client_fd, Some(peer_cid))
                    {
                        error!(error = %err, peer_cid, "vsock connection failed");
                    }
                })
                .context("spawn vsock connection thread")?;
        }
    }

    fn handle_blocking_fd(
        &self,
        runtime: tokio::runtime::Handle,
        fd: RawFd,
        peer_cid: Option<u32>,
    ) -> Result<()> {
        let stream = unsafe { File::from_raw_fd(fd) };
        let reader_file = stream.try_clone()?;
        let mut reader = BufReader::new(reader_file);
        let mut writer = stream;
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                return Ok(());
            }
            if line.trim().is_empty() {
                continue;
            }
            let req: Request = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(err) => {
                    write_blocking_response(
                        &mut writer,
                        &Response::failure("", "protocol.invalid_json", err.to_string()),
                    )?;
                    continue;
                }
            };
            let started = Instant::now();
            let id = req.id.clone();
            let verb = req.verb.to_string();
            let args = serde_json::Value::Object(req.args.clone());
            match runtime.block_on(self.dispatch(req)) {
                Ok(Dispatch::One(value)) => {
                    write_blocking_response(&mut writer, &Response::success(id.clone(), value))?;
                    info!(
                        id = %id,
                        verb = %verb,
                        args = %args,
                        caller_transport = "vsock",
                        caller_cid = peer_cid,
                        ok = true,
                        duration_ms = started.elapsed().as_millis() as u64,
                        "audit"
                    );
                }
                Ok(Dispatch::BufferedStream(values)) => {
                    for value in values {
                        write_blocking_response(
                            &mut writer,
                            &Response::stream_data(id.clone(), value),
                        )?;
                    }
                    write_blocking_response(&mut writer, &Response::stream_end(id.clone()))?;
                    info!(
                        id = %id,
                        verb = %verb,
                        args = %args,
                        caller_transport = "vsock",
                        caller_cid = peer_cid,
                        ok = true,
                        duration_ms = started.elapsed().as_millis() as u64,
                        "audit"
                    );
                }
                Ok(Dispatch::FollowLog { path }) => {
                    stream_log_blocking(&mut writer, id.clone(), path.to_string(), true)?;
                    info!(
                        id = %id,
                        verb = %verb,
                        args = %args,
                        caller_transport = "vsock",
                        caller_cid = peer_cid,
                        ok = true,
                        duration_ms = started.elapsed().as_millis() as u64,
                        "audit"
                    );
                }
                Err(err) => {
                    write_blocking_response(
                        &mut writer,
                        &Response::failure(id.clone(), crate::error_code(&err), err.to_string()),
                    )?;
                    info!(
                        id = %id,
                        verb = %verb,
                        args = %args,
                        caller_transport = "vsock",
                        caller_cid = peer_cid,
                        ok = false,
                        duration_ms = started.elapsed().as_millis() as u64,
                        "audit"
                    );
                }
            }
        }
    }
}

async fn agent_in_charge(cfg: &Config) -> Result<Option<Service>> {
    let registry = Registry::load(cfg).await?;
    Ok(registry
        .services
        .values()
        .find(|svc| svc.is_agent_in_charge)
        .cloned())
}

fn bind_vsock(port: u32) -> Result<RawFd> {
    let fd = unsafe { libc::socket(AF_VSOCK, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create AF_VSOCK socket");
    }
    let addr = sockaddr_vm {
        svm_family: AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: VMADDR_CID_ANY,
        svm_zero: [0; 4],
    };
    let bind_result = unsafe {
        libc::bind(
            fd,
            &addr as *const sockaddr_vm as *const sockaddr,
            mem::size_of::<sockaddr_vm>() as socklen_t,
        )
    };
    if bind_result < 0 {
        let err = std::io::Error::last_os_error();
        close_fd(fd);
        return Err(err).context("bind AF_VSOCK socket");
    }
    let listen_result = unsafe { libc::listen(fd, 128 as c_int) };
    if listen_result < 0 {
        let err = std::io::Error::last_os_error();
        close_fd(fd);
        return Err(err).context("listen AF_VSOCK socket");
    }
    Ok(fd)
}

fn accept_vsock(listener: RawFd) -> Result<(RawFd, u32)> {
    let mut addr: sockaddr_vm = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<sockaddr_vm>() as socklen_t;
    let fd = unsafe {
        libc::accept(
            listener,
            &mut addr as *mut sockaddr_vm as *mut sockaddr,
            &mut len,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("accept AF_VSOCK connection");
    }
    Ok((fd, addr.svm_cid))
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn write_blocking_response(writer: &mut File, response: &Response) -> Result<()> {
    writer.write_all(serde_json::to_string(response)?.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn stream_log_blocking(writer: &mut File, id: String, path: String, follow: bool) -> Result<()> {
    loop {
        match File::open(&path) {
            Ok(file) => {
                let mut reader = BufReader::new(file);
                let mut line = String::new();
                loop {
                    line.clear();
                    let read = reader.read_line(&mut line)?;
                    if read == 0 {
                        if follow {
                            thread::sleep(Duration::from_millis(250));
                            continue;
                        }
                        write_blocking_response(writer, &Response::stream_end(id))?;
                        return Ok(());
                    }
                    crate::trim_newline(&mut line);
                    write_blocking_response(
                        writer,
                        &Response::stream_data(id.clone(), json!({ "line": line })),
                    )?;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && follow => {
                thread::sleep(Duration::from_millis(250));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                write_blocking_response(writer, &Response::stream_end(id))?;
                return Ok(());
            }
            Err(err) => return Err(anyhow!(err)),
        }
    }
}
