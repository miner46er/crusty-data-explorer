//! SSH tunnels for database connections.
//!
//! A tunnel owns one SSH session, a loopback listener on an ephemeral port,
//! and one pump pair per forwarded TCP connection. The session runs in
//! nonblocking mode because libssh2 serializes every call on a session
//! behind one mutex: a blocking channel read would starve the opposite
//! direction of the same channel. Drop flips a shutdown flag, shuts the
//! forwarded client sockets, and joins the accept and pump threads, so
//! teardown returns within one accept poll plus a few milliseconds.

use std::path::PathBuf;

use crate::db::DbError;

/// Where a tunnel connects and how it authenticates. Persisted with the
/// connection profile; never holds a secret.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

/// The authentication method. The secret itself arrives as a transient
/// argument to [`open`], so it is never written to disk. A key file's
/// optional passphrase rides in memory only and is skipped by serde.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SshAuth {
    Password,
    KeyFile {
        path: PathBuf,
        #[serde(default, skip_serializing)]
        passphrase: Option<String>,
    },
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_owned(),
            port: 22,
            user: String::new(),
            auth: SshAuth::Password,
        }
    }
}

impl SshConfig {
    /// Boundary check before a session opens. The connect path calls it
    /// through [`ConnectionConfig::validate`](crate::db::ConnectionConfig).
    pub fn validate(&self) -> Result<(), DbError> {
        if self.host.trim().is_empty() {
            return Err(DbError::Config("ssh host is empty".to_owned()));
        }
        if self.port == 0 {
            return Err(DbError::Config("ssh port is not set".to_owned()));
        }
        if self.user.trim().is_empty() {
            return Err(DbError::Config("ssh user is empty".to_owned()));
        }
        if let SshAuth::KeyFile { path, .. } = &self.auth
            && path.as_os_str().is_empty()
        {
            return Err(DbError::Config("ssh key file is empty".to_owned()));
        }
        Ok(())
    }
}

/// The pool endpoint behind a tunnel: the loopback address of the
/// forwarded socket.
pub fn loopback_endpoint(local_port: u16) -> (String, u16) {
    ("127.0.0.1".to_owned(), local_port)
}

#[cfg(feature = "ssh")]
pub use imp::{Tunnel, forward, open};

/// Without the `ssh` feature no tunnel can exist. The type stays so a
/// profile that names an SSH server still deserializes, and connecting
/// fails at [`open`] with the feature message.
#[cfg(not(feature = "ssh"))]
pub struct Tunnel(());

#[cfg(not(feature = "ssh"))]
pub fn open(
    _config: &SshConfig,
    _password: &str,
    _target_host: &str,
    _target_port: u16,
) -> Result<Tunnel, DbError> {
    Err(DbError::Config("ssh feature not enabled".to_owned()))
}

/// The db-facing seam: open the tunnel a profile asks for and report the
/// endpoint its pool must dial.
#[cfg(not(feature = "ssh"))]
pub fn forward(
    ssh: Option<&SshConfig>,
    _password: &str,
    host: &str,
    port: u16,
) -> Result<(Option<Tunnel>, String, u16), DbError> {
    match ssh {
        Some(_) => Err(DbError::Config("ssh feature not enabled".to_owned())),
        None => Ok((None, host.to_owned(), port)),
    }
}

#[cfg(feature = "ssh")]
mod imp {
    use std::collections::HashMap;
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use super::{SshAuth, SshConfig, loopback_endpoint};
    use crate::db::DbError;

    /// Bound on dialing the SSH server before the tunnel opens.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    /// libssh2's own socket timeout for the blocking handshake and auth.
    const SSH_TIMEOUT_MS: u32 = 10_000;
    /// Bound on the channel-open round trip, retried through EAGAIN.
    const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);
    /// Sleep between nonblocking polls of a channel or a listener.
    const POLL: Duration = Duration::from_millis(5);
    /// Accept-loop poll interval. Bounds how long Drop waits for it.
    const ACCEPT_POLL: Duration = Duration::from_millis(25);
    /// LIBSSH2_ERROR_EAGAIN, libssh2's nonblocking "no data yet" code.
    const LIBSSH2_ERROR_EAGAIN: i32 = -37;
    /// Copy buffer per pump direction.
    const BUF: usize = 16 * 1024;

    /// One SSH session forwarding 127.0.0.1:port to one fixed target for
    /// the tunnel's whole life.
    pub struct Tunnel {
        local: SocketAddr,
        session: ssh2::Session,
        shutdown: Arc<AtomicBool>,
        accept: Option<JoinHandle<()>>,
        pumps: Arc<Mutex<Vec<JoinHandle<()>>>>,
        live: Arc<Mutex<HashMap<u64, TcpStream>>>,
    }

    impl Tunnel {
        /// Port of the loopback listener the tunnel forwards through.
        pub fn local_port(&self) -> u16 {
            self.local.port()
        }
        /// Address of the loopback listener.
        pub fn local_addr(&self) -> SocketAddr {
            self.local
        }
    }

    impl Drop for Tunnel {
        /// Bounded: the accept loop notices the shutdown flag within one
        /// ACCEPT_POLL tick, and every pump wakes on the socket shutdown or
        /// its next channel poll, so no join blocks on a peer.
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Ok(live) = self.live.lock() {
                for stream in live.values() {
                    let _ = stream.shutdown(Shutdown::Both);
                }
            }
            if let Some(accept) = self.accept.take() {
                let _ = accept.join();
            }
            let mut pumps = self.pumps.lock().unwrap_or_else(PoisonError::into_inner);
            for pump in pumps.drain(..) {
                let _ = pump.join();
            }
            drop(pumps);
            let _ = self.session.disconnect(
                Some(ssh2::DisconnectCode::ByApplication),
                "tunnel closed",
                None,
            );
        }
    }

    /// Dial the SSH server, authenticate, and start forwarding
    /// `target_host:target_port` on a fresh loopback port. Blocking.
    pub fn open(
        config: &SshConfig,
        password: &str,
        target_host: &str,
        target_port: u16,
    ) -> Result<Tunnel, DbError> {
        let tcp = TcpStream::connect_timeout(&address(config)?, CONNECT_TIMEOUT).map_err(
            |error| {
                DbError::Open(format!(
                    "ssh connect {}:{}: {error}",
                    config.host, config.port
                ))
            },
        )?;
        let mut session = ssh2::Session::new()
            .map_err(|error| DbError::Open(format!("ssh session: {error}")))?;
        session.set_tcp_stream(tcp);
        session.set_timeout(SSH_TIMEOUT_MS);
        session.handshake().map_err(|error| {
            DbError::Open(format!(
                "ssh handshake {}:{}: {error}",
                config.host, config.port
            ))
        })?;
        authenticate(&session, config, password)?;
        if !session.authenticated() {
            return Err(DbError::Open(format!(
                "ssh authentication for {}@{} failed",
                config.user, config.host
            )));
        }
        session.set_blocking(false);
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| DbError::Open(format!("ssh bind: {error}")))?;
        let local = listener
            .local_addr()
            .map_err(|error| DbError::Open(format!("ssh bind: {error}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| DbError::Open(format!("ssh bind: {error}")))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let pumps: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let live: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
        let accept_session = session.clone();
        let accept = {
            let shutdown = Arc::clone(&shutdown);
            let pumps = Arc::clone(&pumps);
            let live = Arc::clone(&live);
            let target_host = target_host.to_owned();
            thread::Builder::new()
                .name(format!("cde-ssh-{local}"))
                .spawn(move || {
                    accept_loop(
                        accept_session,
                        listener,
                        target_host,
                        target_port,
                        shutdown,
                        pumps,
                        live,
                    )
                })
                .map_err(|error| DbError::Open(format!("ssh worker thread: {error}")))?
        };
        Ok(Tunnel {
            local,
            session,
            shutdown,
            accept: Some(accept),
            pumps,
            live,
        })
    }

    /// The db-facing seam: open the tunnel a profile asks for and report the
    /// endpoint its pool must dial. Without a configured tunnel the stored
    /// host and port stand.
    pub fn forward(
        ssh: Option<&SshConfig>,
        password: &str,
        host: &str,
        port: u16,
    ) -> Result<(Option<Tunnel>, String, u16), DbError> {
        match ssh {
            Some(config) => {
                let tunnel = open(config, password, host, port)?;
                let (dial_host, dial_port) = loopback_endpoint(tunnel.local_port());
                Ok((Some(tunnel), dial_host, dial_port))
            }
            None => Ok((None, host.to_owned(), port)),
        }
    }

    /// The SSH server's socket address, resolving a hostname first.
    fn address(config: &SshConfig) -> Result<SocketAddr, DbError> {
        (config.host.as_str(), config.port)
            .to_socket_addrs()
            .map_err(|error| {
                DbError::Open(format!(
                    "ssh resolve {}:{}: {error}",
                    config.host, config.port
                ))
            })?
            .next()
            .ok_or_else(|| {
                DbError::Open(format!(
                    "ssh resolve {}:{}: no address",
                    config.host, config.port
                ))
            })
    }

    /// Authenticate with the configured method. The transient `password`
    /// is the secret for [`SshAuth::Password`] and the fallback passphrase
    /// for [`SshAuth::KeyFile`].
    fn authenticate(
        session: &ssh2::Session,
        config: &SshConfig,
        password: &str,
    ) -> Result<(), DbError> {
        let failed = |error: ssh2::Error| {
            DbError::Open(format!(
                "ssh authentication for {}@{}: {error}",
                config.user, config.host
            ))
        };
        match &config.auth {
            SshAuth::Password => session.userauth_password(&config.user, password).map_err(failed),
            SshAuth::KeyFile { path, passphrase } => {
                let pass = passphrase
                    .as_deref()
                    .filter(|pass| !pass.is_empty())
                    .unwrap_or(password);
                session
                    .userauth_pubkey_file(
                        &config.user,
                        None,
                        path,
                        (!pass.is_empty()).then_some(pass),
                    )
                    .map_err(failed)
            }
        }
    }

    /// Accept forwarded connections until the shutdown flag flips. The
    /// listener is nonblocking so Drop never waits on a parked accept.
    fn accept_loop(
        session: ssh2::Session,
        listener: TcpListener,
        target_host: String,
        target_port: u16,
        shutdown: Arc<AtomicBool>,
        pumps: Arc<Mutex<Vec<JoinHandle<()>>>>,
        live: Arc<Mutex<HashMap<u64, TcpStream>>>,
    ) {
        let mut next_id = 0u64;
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let id = next_id;
                    next_id += 1;
                    let wake = match stream.try_clone() {
                        Ok(wake) => wake,
                        Err(_) => continue,
                    };
                    live.lock().unwrap_or_else(PoisonError::into_inner).insert(id, wake);
                    let spawned = {
                        let session = session.clone();
                        let target_host = target_host.clone();
                        let shutdown = Arc::clone(&shutdown);
                        let live = Arc::clone(&live);
                        thread::Builder::new()
                            .name(format!("cde-ssh-fwd-{id}"))
                            .spawn(move || {
                                serve(
                                    session,
                                    stream,
                                    id,
                                    target_host,
                                    target_port,
                                    shutdown,
                                    live,
                                )
                            })
                    };
                    match spawned {
                        Ok(handle) => {
                            pumps.lock().unwrap_or_else(PoisonError::into_inner).push(handle);
                        }
                        Err(_) => {
                            live.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .remove(&id);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(ACCEPT_POLL);
                }
                Err(_) => thread::sleep(ACCEPT_POLL),
            }
        }
    }

    /// Serve one forwarded connection: open the direct-tcpip channel and
    /// run both pump directions until the connection ends.
    fn serve(
        session: ssh2::Session,
        stream: TcpStream,
        id: u64,
        target_host: String,
        target_port: u16,
        shutdown: Arc<AtomicBool>,
        live: Arc<Mutex<HashMap<u64, TcpStream>>>,
    ) {
        let _guard = LiveGuard { id, live };
        let channel = match open_channel(&session, &target_host, target_port) {
            Ok(channel) => channel,
            Err(_) => return,
        };
        let upstream = match stream.try_clone() {
            Ok(upstream) => upstream,
            Err(_) => return,
        };
        let up = {
            let channel = channel.clone();
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name(format!("cde-ssh-up-{id}"))
                .spawn(move || pump_client_to_channel(stream, channel, shutdown))
        };
        let up = match up {
            Ok(up) => up,
            Err(_) => return,
        };
        pump_channel_to_client(upstream, channel, &shutdown);
        let _ = up.join();
    }

    /// Open a direct-tcpip channel, retrying EAGAIN until FORWARD_TIMEOUT.
    fn open_channel(
        session: &ssh2::Session,
        host: &str,
        port: u16,
    ) -> Result<ssh2::Channel, DbError> {
        let deadline = Instant::now() + FORWARD_TIMEOUT;
        loop {
            match session.channel_direct_tcpip(host, port, None) {
                Ok(channel) => return Ok(channel),
                Err(error) if would_block(&error) && Instant::now() < deadline => {
                    thread::sleep(POLL);
                }
                Err(error) => {
                    return Err(DbError::Open(format!("ssh forward {host}:{port}: {error}")));
                }
            }
        }
    }

    /// Client bytes to the channel. Client reads block; Drop wakes them
    /// with a socket shutdown.
    fn pump_client_to_channel(
        mut client: TcpStream,
        mut channel: ssh2::Channel,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut buffer = [0u8; BUF];
        while !shutdown.load(Ordering::SeqCst) {
            let read = match client.read(&mut buffer) {
                Ok(0) => {
                    send_eof(&mut channel, &shutdown);
                    break;
                }
                Ok(read) => read,
                Err(_) => break,
            };
            if write_channel(&mut channel, &buffer[..read], &shutdown).is_err() {
                break;
            }
        }
    }

    /// Channel bytes to the client. Client writes block; Drop wakes them.
    /// Ends by shutting the socket down so the opposite pump exits too.
    fn pump_channel_to_client(
        mut client: TcpStream,
        mut channel: ssh2::Channel,
        shutdown: &AtomicBool,
    ) {
        let mut buffer = [0u8; BUF];
        while !shutdown.load(Ordering::SeqCst) {
            match channel.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if client.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                Err(_) => break,
            }
        }
        let _ = client.shutdown(Shutdown::Both);
    }

    /// Write all bytes through the nonblocking channel, polling EAGAIN.
    fn write_channel(
        channel: &mut ssh2::Channel,
        mut data: &[u8],
        shutdown: &AtomicBool,
    ) -> io::Result<()> {
        while !data.is_empty() {
            if shutdown.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            match channel.write(data) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => data = &data[written..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Best-effort EOF toward the target, retried briefly through EAGAIN.
    fn send_eof(channel: &mut ssh2::Channel, shutdown: &AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !shutdown.load(Ordering::SeqCst) {
            match channel.send_eof() {
                Ok(()) => return,
                Err(error) if would_block(&error) && Instant::now() < deadline => {
                    thread::sleep(POLL);
                }
                Err(_) => return,
            }
        }
    }

    fn would_block(error: &ssh2::Error) -> bool {
        error.code() == ssh2::ErrorCode::Session(LIBSSH2_ERROR_EAGAIN)
    }

    /// Removes a forwarded connection from the live set on every exit path,
    /// so Drop stops waking a socket nobody serves.
    struct LiveGuard {
        id: u64,
        live: Arc<Mutex<HashMap<u64, TcpStream>>>,
    }

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrips_through_json() {
        let password_config = SshConfig {
            host: "jump.example".to_owned(),
            port: 2222,
            user: "deploy".to_owned(),
            auth: SshAuth::Password,
        };
        assert_eq!(
            serde_json::to_string(&password_config).unwrap(),
            r#"{"host":"jump.example","port":2222,"user":"deploy","auth":{"method":"password"}}"#
        );
        let back: SshConfig =
            serde_json::from_str(&serde_json::to_string(&password_config).unwrap()).unwrap();
        assert_eq!(back, password_config);

        let key_config = SshConfig {
            host: "jump.example".to_owned(),
            port: 2222,
            user: "deploy".to_owned(),
            auth: SshAuth::KeyFile {
                path: PathBuf::from("/home/deploy/.ssh/id_ed25519"),
                passphrase: None,
            },
        };
        let json = serde_json::to_string(&key_config).unwrap();
        assert!(json.contains("key_file"));
        assert_eq!(serde_json::from_str::<SshConfig>(&json).unwrap(), key_config);
    }

    #[test]
    fn key_file_passphrase_is_never_serialized() {
        let config = SshConfig {
            host: "jump.example".to_owned(),
            port: 22,
            user: "deploy".to_owned(),
            auth: SshAuth::KeyFile {
                path: PathBuf::from("/k"),
                passphrase: Some("unlock-me".to_owned()),
            },
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("passphrase"), "{json}");
        assert!(!json.contains("unlock-me"), "{json}");
        let SshAuth::KeyFile { passphrase, .. } = serde_json::from_str::<SshConfig>(&json)
            .unwrap()
            .auth
        else {
            panic!("expected the key file method");
        };
        assert_eq!(passphrase, None);
    }

    #[test]
    fn validate_checks_each_ssh_field() {
        let good = |host: &str, port: u16, user: &str| SshConfig {
            host: host.to_owned(),
            port,
            user: user.to_owned(),
            auth: SshAuth::Password,
        };
        assert!(good("localhost", 22, "root").validate().is_ok());
        assert_eq!(
            good(" ", 22, "root").validate(),
            Err(DbError::Config("ssh host is empty".to_owned()))
        );
        assert_eq!(
            good("localhost", 0, "root").validate(),
            Err(DbError::Config("ssh port is not set".to_owned()))
        );
        assert_eq!(
            good("localhost", 22, " ").validate(),
            Err(DbError::Config("ssh user is empty".to_owned()))
        );
        let mut key = good("localhost", 22, "root");
        key.auth = SshAuth::KeyFile { path: PathBuf::new(), passphrase: None };
        assert_eq!(
            key.validate(),
            Err(DbError::Config("ssh key file is empty".to_owned()))
        );
        key.auth = SshAuth::KeyFile {
            path: PathBuf::from("/home/deploy/.ssh/id_ed25519"),
            passphrase: None,
        };
        assert!(key.validate().is_ok());
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn dead_ssh_port_fails_fast() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let config = SshConfig {
            host: "127.0.0.1".to_owned(),
            port,
            user: "root".to_owned(),
            auth: SshAuth::Password,
        };
        let started = std::time::Instant::now();
        let Err(error) = open(&config, "whatever", "127.0.0.1", 5432) else {
            panic!("a dead ssh port must refuse");
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "open took {:?}",
            started.elapsed()
        );
        assert!(
            matches!(&error, DbError::Open(message) if message.contains("ssh connect")),
            "{error}"
        );
    }

    #[cfg(not(feature = "ssh"))]
    #[test]
    fn feature_off_stub_refuses() {
        let config = SshConfig::default();
        assert_eq!(
            open(&config, "x", "db.internal", 5432).err(),
            Some(DbError::Config("ssh feature not enabled".to_owned()))
        );
        assert_eq!(
            forward(Some(&config), "x", "db.internal", 5432).err(),
            Some(DbError::Config("ssh feature not enabled".to_owned()))
        );
        let (tunnel, host, port) =
            forward(None, "x", "db.internal", 5432).expect("a profile without ssh passes through");
        assert!(tunnel.is_none());
        assert_eq!((host.as_str(), port), ("db.internal", 5432));
    }

    /// Spike proof: forwards the Postgres container through the SSH server
    /// and runs a sqlx SELECT over the forwarded port, then drops the
    /// tunnel and asserts the pool fails afterward.
    #[cfg(all(feature = "ssh", feature = "postgres"))]
    #[test]
    #[ignore = "needs cde-ssh and cde-postgres; set SSH_HOST/SSH_PORT/SSH_USER/SSH_PASSWORD and PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn tunnel_forwards_postgres_and_drops_without_hanging() {
        let config = SshConfig {
            host: std::env::var("SSH_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
            port: std::env::var("SSH_PORT").ok().and_then(|port| port.parse().ok()).unwrap_or(22),
            user: std::env::var("SSH_USER").unwrap_or_else(|_| "root".to_owned()),
            auth: SshAuth::Password,
        };
        let ssh_password = std::env::var("SSH_PASSWORD").unwrap_or_default();
        let pg_host = std::env::var("PGHOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
        let pg_port: u16 =
            std::env::var("PGPORT").ok().and_then(|port| port.parse().ok()).unwrap_or(5432);
        let database = std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned());
        let user = std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned());
        let password = std::env::var("PGPASSWORD").ok();

        let tunnel = open(&config, &ssh_password, &pg_host, pg_port).expect("open tunnel");
        assert_eq!(tunnel.local_addr().ip().to_string(), "127.0.0.1");
        let forwarded_port = tunnel.local_port();
        let url = crate::db::postgres_url(
            "127.0.0.1",
            forwarded_port,
            &database,
            &user,
            password.as_deref(),
            crate::db::PostgresSslMode::Disable,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let pool = rt
            .block_on(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(std::time::Duration::from_secs(10))
                    .connect(&url),
            )
            .expect("pool through the tunnel");
        let row = rt
            .block_on(sqlx::query("SELECT 1").fetch_one(&pool))
            .expect("select through the tunnel");
        use sqlx::Row as _;
        assert_eq!(row.try_get::<i32, _>(0).expect("int column"), 1);

        let started = std::time::Instant::now();
        drop(tunnel);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "drop took {:?}",
            started.elapsed()
        );

        let after = rt.block_on(sqlx::query("SELECT 2").fetch_one(&pool));
        assert!(after.is_err(), "the pool must fail once its tunnel is gone");
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", forwarded_port)).is_err(),
            "the forwarded port must be closed after the drop"
        );
    }
}
