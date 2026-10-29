use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ss_ipc::{Stream, connect, spawn_detached};
use ss_proto::{Frame, FrameError, LineDecoder, Request, Response, encode_frame};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::settings::Settings;

const RETRY_PAUSE: Duration = Duration::from_millis(25);
const RESPAWN_EVERY: Duration = Duration::from_secs(1);
const READ_CHUNK: usize = 8192;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon did not become ready within {0} ms")]
    NotReady(u64),
    #[error("the daemon did not reply within {0} ms")]
    TimedOut(u64),
    #[error("could not start the daemon: {0}")]
    Spawn(io::Error),
    #[error("connection failed: {0}")]
    Io(#[from] io::Error),
    #[error("bad reply: {0}")]
    Frame(#[from] FrameError),
    #[error("the daemon closed the connection before replying")]
    Closed,
}

/// Stateless client: each call is one connection, one request, one reply. It starts the daemon when absent.
#[derive(Debug, Clone)]
pub struct Client {
    settings: Settings,
}

impl Client {
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self { settings }
    }

    /// Sends `request`, spawning a detached daemon first if none is reachable. One retry covers a daemon
    /// that exits between the connect and the exchange.
    pub async fn send(&self, request: Request) -> Result<Response, ClientError> {
        match self.exchange_spawning(&request).await {
            Err(ClientError::Io(_) | ClientError::Closed) => self.exchange_spawning(&request).await,
            other => other,
        }
    }

    /// Like [`Client::send`] but never starts a daemon; `None` means nothing is listening.
    pub async fn send_if_running(&self, request: Request) -> Result<Option<Response>, ClientError> {
        match connect(&self.settings.endpoint()).await {
            Ok(stream) => exchange(stream, &request).await.map(Some),
            Err(_) => Ok(None),
        }
    }

    async fn exchange_spawning(&self, request: &Request) -> Result<Response, ClientError> {
        let stream = self.connect_or_spawn().await?;
        let limit_ms = self.reply_limit_ms(request);
        if limit_ms == 0 {
            return exchange(stream, request).await;
        }
        tokio::time::timeout(Duration::from_millis(limit_ms), exchange(stream, request))
            .await
            .map_err(|_| ClientError::TimedOut(limit_ms))?
    }

    /// An index run legitimately lasts as long as the tree is large, so only requests that answer from existing
    /// state are bounded. Zero means unbounded.
    fn reply_limit_ms(&self, request: &Request) -> u64 {
        match request {
            Request::Index { .. } => 0,
            _ => self.settings.request_timeout_ms,
        }
    }

    async fn connect_or_spawn(&self) -> Result<Stream, ClientError> {
        let endpoint = self.settings.endpoint();
        if let Ok(stream) = connect(&endpoint).await {
            return Ok(stream);
        }
        let deadline = Instant::now() + Duration::from_millis(self.settings.spawn_timeout_ms);
        let mut last_spawn = None::<Instant>;
        loop {
            if last_spawn.is_none_or(|at| at.elapsed() >= RESPAWN_EVERY) {
                self.spawn_daemon()?;
                last_spawn = Some(Instant::now());
            }
            match connect(&endpoint).await {
                Ok(stream) => return Ok(stream),
                Err(_) if Instant::now() < deadline => tokio::time::sleep(RETRY_PAUSE).await,
                Err(_) => return Err(ClientError::NotReady(self.settings.spawn_timeout_ms)),
            }
        }
    }

    /// Spawning is idempotent: a daemon that loses the instance lock exits at once. Repeating it covers a
    /// predecessor that was still shutting down (holding the lock) when the first attempt started.
    fn spawn_daemon(&self) -> Result<(), ClientError> {
        spawn_detached(&self.settings.daemon_executable(), &["serve", "--setsid"])
            .map_err(ClientError::Spawn)
    }
}

async fn exchange(mut stream: Stream, request: &Request) -> Result<Response, ClientError> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut bytes = Vec::new();
    encode_frame(
        &Frame {
            id,
            body: request.clone(),
        },
        &mut bytes,
    )?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    let mut decoder = LineDecoder::new();
    let mut chunk = [0_u8; READ_CHUNK];
    loop {
        while let Some(parsed) = decoder.next_frame::<Response>() {
            let frame = parsed?;
            if frame.id == id || frame.id == 0 {
                return Ok(frame.body);
            }
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(ClientError::Closed);
        }
        decoder.push(&chunk[..read]);
    }
}

#[cfg(test)]
mod tests {
    use ss_ipc::{ListenerExt as _, listen};

    use super::*;

    #[proxima::test(runtime = "tokio")]
    async fn a_daemon_that_accepts_but_never_replies_times_out_instead_of_hanging_the_client() {
        let home = tempfile::tempdir().expect("home");
        let settings = Settings {
            request_timeout_ms: 200,
            ..Settings::load().expect("defaults")
        }
        .with_home(home.path());
        let listener = listen(&settings.endpoint()).expect("listen");
        let silent = tokio::spawn(async move {
            let held = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            held
        });
        let started = Instant::now();

        let outcome = Client::new(settings).send(Request::Status).await;

        silent.abort();
        assert!(
            matches!(outcome, Err(ClientError::TimedOut(200))),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
