use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(not(unix))]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{ListenerOptions, Name};

pub use interprocess::local_socket::tokio::{Listener, Stream};
pub use interprocess::local_socket::traits::tokio::{Listener as ListenerExt, Stream as StreamExt};

/// Where the daemon listens: a socket file under the state dir on unix, a named pipe elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    socket_path: PathBuf,
    pipe_name: String,
}

impl Endpoint {
    #[must_use]
    pub fn new(home: &Path) -> Self {
        let fingerprint = fnv1a(home.to_string_lossy().as_bytes());
        Self {
            socket_path: home.join("ssd.sock"),
            pipe_name: format!("ss-{fingerprint:016x}"),
        }
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    #[cfg(unix)]
    fn name(&self) -> io::Result<Name<'static>> {
        self.socket_path.clone().to_fs_name::<GenericFilePath>()
    }

    #[cfg(not(unix))]
    fn name(&self) -> io::Result<Name<'static>> {
        self.pipe_name.clone().to_ns_name::<GenericNamespaced>()
    }
}

/// FNV-1a: a fixed, dependency-free hash so `ss` and `ssd` of any version agree on the pipe name.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Binds the endpoint. Call only while holding the instance lock: a stale unix socket file is overwritten.
pub fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
    #[cfg(unix)]
    create_private_dir(endpoint.socket_path.parent().unwrap_or(Path::new(".")))?;
    let options = ListenerOptions::new()
        .name(endpoint.name()?)
        .try_overwrite(true);
    #[cfg(windows)]
    let options = owner_only(options)?;
    options.create_tokio()
}

pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    Stream::connect(endpoint.name()?).await
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Grants generic-all to the object's owner only; remote clients are rejected by the listener default.
#[cfg(windows)]
fn owner_only(options: ListenerOptions<'static>) -> io::Result<ListenerOptions<'static>> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;

    let sddl = U16CString::from_str("D:P(A;;GA;;;OW)").map_err(io::Error::other)?;
    Ok(options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn pipe_name_is_stable_per_home_and_differs_between_homes() {
        let first = Endpoint::new(Path::new("/a/ss"));
        let again = Endpoint::new(Path::new("/a/ss"));
        let other = Endpoint::new(Path::new("/b/ss"));

        assert_eq!(first.pipe_name(), again.pipe_name());
        assert_ne!(first.pipe_name(), other.pipe_name());
        assert!(first.pipe_name().starts_with("ss-") && first.pipe_name().len() == 19);
    }

    #[proxima::test(runtime = "tokio")]
    async fn a_client_reaches_the_listener_and_bytes_round_trip() {
        let home = tempfile::tempdir().expect("home");
        let endpoint = Endpoint::new(home.path());
        let listener = listen(&endpoint).expect("listen");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4];
            stream.read_exact(&mut buffer).await.expect("read");
            stream.write_all(&buffer).await.expect("echo");
            stream.flush().await.expect("flush");
            stream
        });

        let mut client = connect(&endpoint).await.expect("connect");
        client.write_all(b"ping").await.expect("write");
        let mut reply = [0_u8; 4];
        client.read_exact(&mut reply).await.expect("read reply");

        assert_eq!(&reply, b"ping");
        drop(server.await.expect("server task"));
    }

    #[proxima::test(runtime = "tokio")]
    async fn connecting_with_no_listener_fails() {
        let home = tempfile::tempdir().expect("home");

        let outcome = connect(&Endpoint::new(home.path())).await;

        assert!(outcome.is_err());
    }

    #[cfg(unix)]
    #[proxima::test(runtime = "tokio")]
    async fn state_dir_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().expect("home");
        let state = home.path().join("fresh");
        let _listener = listen(&Endpoint::new(&state)).expect("listen");

        let mode = std::fs::metadata(&state)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o700);
    }
}
