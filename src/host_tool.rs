use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};

use bashkit::{BuiltinContext, ExecResult, StreamData, async_trait};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const FAILURE_EXIT_CODE: i32 = 1;

pub enum ScriptInput {
    Closed,
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Created,
    Updated,
}

pub struct HostOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

impl From<HostOutput> for ExecResult {
    fn from(output: HostOutput) -> Self {
        Self {
            stdout: StreamData::from(output.stdout),
            stderr: StreamData::from(output.stderr),
            exit_code: output.exit_code,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostExecutable {
    pub path: PathBuf,

    pub args: Vec<String>,

    pub cwd: Option<PathBuf>,

    pub env: Vec<(String, String)>,

    pub clear_env: bool,
}

pub struct HostTool {
    name: String,
    executable: HostExecutable,
}

impl HostExecutable {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        command.args(&self.args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(self.env.iter().cloned());
        command
    }

    pub async fn execute<I, S>(
        &self,
        args: I,
        stdin: Option<Vec<u8>>,
    ) -> std::io::Result<HostOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = self.command();
        command.args(args);
        run(command, stdin).await
    }

    pub async fn spawn_with(
        &self,
        args: &[String],
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> std::io::Result<ExitStatus> {
        let mut command = self.command();
        command.args(args);
        command.stdin(stdin).stdout(stdout).stderr(stderr);
        command.spawn()?.wait().await
    }
}

impl HostTool {
    #[must_use]
    pub const fn new(name: String, executable: HostExecutable) -> Self {
        Self { name, executable }
    }
}

async fn run(mut command: Command, stdin: Option<Vec<u8>>) -> std::io::Result<HostOutput> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let pipe = child.stdin.take();
    let write_stdin = async move {
        if let (Some(bytes), Some(mut pipe)) = (stdin, pipe) {
            if let Err(error) = pipe.write_all(&bytes).await
                && error.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(error);
            }
            if let Err(error) = pipe.shutdown().await
                && error.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(error);
            }
        }
        Ok(())
    };
    let (stdin_result, output) = tokio::join!(write_stdin, child.wait_with_output());
    let output = output?;
    stdin_result?;
    Ok(HostOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code().unwrap_or(FAILURE_EXIT_CODE),
    })
}

#[async_trait]
impl bashkit::Builtin for HostTool {
    async fn execute(&self, ctx: BuiltinContext<'_>) -> bashkit::Result<ExecResult> {
        match self
            .executable
            .execute(ctx.args, ctx.stdin_bytes().map(<[u8]>::to_vec))
            .await
        {
            Ok(result) => Ok(result.into()),
            Err(error) => Ok(ExecResult::err(
                format!("bxwrp: {}: {error}", self.name),
                FAILURE_EXIT_CODE,
            )),
        }
    }
}
