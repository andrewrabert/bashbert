use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead as _, IoSlice, IoSliceMut, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context as _;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader, Interest};
use tokio::net::UnixListener;

use crate::config::ToolName;
use crate::host_tool::{FAILURE_EXIT_CODE, HostExecutable};

const REQUEST_LIMIT: u64 = 64 * 1024;

pub const BRIDGE_ENV: &str = "BXWRP_BRIDGE";

const NO_SUCH_TOOL_EXIT_CODE: i32 = 127;

fn close_on_exec(fds: &[OwnedFd]) {
    for fd in fds {
        let flags = match rustix::io::fcntl_getfd(fd) {
            Ok(flags) => flags,
            Err(error) => {
                tracing::warn!("host tool call: reading descriptor flags: {error}");
                continue;
            }
        };
        if let Err(error) = rustix::io::fcntl_setfd(fd, flags | rustix::io::FdFlags::CLOEXEC) {
            tracing::warn!("host tool call: closing a descriptor on exec: {error}");
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Request {
    pub name: ToolName,

    pub args: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Response {
    pub exit_code: i32,
}

pub struct Bridge {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    socket: PathBuf,
    listener: tokio::task::JoinHandle<()>,
}

impl Bridge {
    pub fn listen(tools: BTreeMap<ToolName, HostExecutable>) -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("bxwrp-")
            .tempdir()
            .context("the host tool socket directory")?;
        let socket = dir.path().join("bridge.sock");
        let bound = std::os::unix::net::UnixListener::bind(&socket)
            .with_context(|| format!("the host tool socket at {}", socket.display()))?;
        bound.set_nonblocking(true)?;
        let listener = UnixListener::from_std(bound)?;
        let tools = Arc::new(tools);
        let listener = tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::warn!("host tool socket: accept: {error}");
                        continue;
                    }
                };
                let tools = Arc::clone(&tools);
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, &tools).await {
                        tracing::warn!("host tool call: {error:#}");
                    }
                });
            }
        });
        Ok(Self {
            dir,
            socket,
            listener,
        })
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

async fn serve(
    mut stream: tokio::net::UnixStream,
    tools: &BTreeMap<ToolName, HostExecutable>,
) -> anyhow::Result<()> {
    let (mut line, fds) = receive(&stream).await?;
    let [stdin, stdout, stderr] = <[OwnedFd; 3]>::try_from(fds)
        .map_err(|fds| anyhow::anyhow!("a call carried {} descriptors, not 3", fds.len()))?;
    if !line.ends_with(b"\n") {
        let mut reader = BufReader::new((&mut stream).take(REQUEST_LIMIT));
        reader.read_until(b'\n', &mut line).await?;
    }
    let exit_code =
        match serde_json::from_slice::<Request>(line.strip_suffix(b"\n").unwrap_or(&line)) {
            Ok(request) => dispatch(tools, request, stdin, stdout, stderr).await,
            Err(error) => {
                complain(&stderr, format!("bxwrp: host tool request: {error}"));
                FAILURE_EXIT_CODE
            }
        };
    let mut reply = serde_json::to_vec(&Response { exit_code })?;
    reply.push(b'\n');
    stream.write_all(&reply).await?;
    stream.flush().await?;
    Ok(())
}

fn complain(stderr: &OwnedFd, message: String) {
    if let Ok(spare) = stderr.try_clone() {
        let mut sink = std::fs::File::from(spare);
        let _ = writeln!(sink, "{message}");
    }
}

async fn dispatch(
    tools: &BTreeMap<ToolName, HostExecutable>,
    request: Request,
    stdin: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
) -> i32 {
    let spare = stderr.try_clone();
    let Some(executable) = tools.get(&request.name) else {
        complain(
            &stderr,
            format!("bxwrp: {}: no such host tool", request.name.as_ref()),
        );
        return NO_SUCH_TOOL_EXIT_CODE;
    };
    match executable
        .spawn_with(
            &request.args,
            Stdio::from(stdin),
            Stdio::from(stdout),
            Stdio::from(stderr),
        )
        .await
    {
        Ok(status) => status.code().unwrap_or(FAILURE_EXIT_CODE),
        Err(error) => {
            if let Ok(spare) = spare {
                complain(&spare, format!("bxwrp: {}: {error}", request.name.as_ref()));
            }
            FAILURE_EXIT_CODE
        }
    }
}

async fn receive(stream: &tokio::net::UnixStream) -> anyhow::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut buffer = [0u8; 4096];
    loop {
        stream.readable().await?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let read = stream.try_io(Interest::READABLE, || {
            let mut slices = [IoSliceMut::new(&mut buffer)];
            rustix::net::recvmsg(stream, &mut slices, &mut ancillary, RecvFlags::empty())
                .map(|message| message.bytes)
                .map_err(std::io::Error::from)
        });
        let read = match read {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error.into()),
        };
        let mut fds = Vec::new();
        for message in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(received) = message {
                fds.extend(received);
            }
        }
        close_on_exec(&fds);
        return Ok((buffer[..read].to_vec(), fds));
    }
}

pub fn shim(socket: &Path, name: ToolName, args: Vec<OsString>) -> anyhow::Result<i32> {
    let mut stated = Vec::with_capacity(args.len());
    for arg in args {
        stated.push(
            arg.into_string()
                .map_err(|_| anyhow::anyhow!("bxwrp: {}: non-UTF-8 argument", name.as_ref()))?,
        );
    }
    let stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("bxwrp: {}: the host tool socket", name.as_ref()))?;
    let mut line = serde_json::to_vec(&Request {
        name: name.clone(),
        args: stated,
    })?;
    line.push(b'\n');

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let fds = [stdin.as_fd(), stdout.as_fd(), stderr.as_fd()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    if !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
        anyhow::bail!(
            "bxwrp: {}: the descriptors do not fit one message",
            name.as_ref()
        );
    }
    let mut sent = 0;
    while sent < line.len() {
        sent += rustix::net::sendmsg(
            &stream,
            &[IoSlice::new(&line[sent..])],
            &mut ancillary,
            SendFlags::empty(),
        )?;
        ancillary.clear();
    }

    let mut reply = String::new();
    std::io::BufReader::new(&stream).read_line(&mut reply)?;
    let response: Response = serde_json::from_str(reply.trim_end())
        .with_context(|| format!("bxwrp: {}: the host tool reply", name.as_ref()))?;
    Ok(response.exit_code)
}
