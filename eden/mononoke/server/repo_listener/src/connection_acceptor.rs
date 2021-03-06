/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use crate::security_checker::ConnectionsSecurityChecker;
use hyper::server::conn::Http;
use session_id::generate_session_id;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use crate::http_service::MononokeHttpService;
use anyhow::{anyhow, Context, Error, Result};
use bytes::Bytes;
use cached_config::{ConfigHandle, ConfigStore};
use failure_ext::SlogKVError;
use fbinit::FacebookInit;
use futures::{channel::oneshot, future::Future, select_biased};
use futures_01_ext::BoxStream;
use futures_old::{stream, sync::mpsc, Stream};
use futures_util::compat::Stream01CompatExt;
use futures_util::future::FutureExt;
use futures_util::stream::{StreamExt, TryStreamExt};
use lazy_static::lazy_static;
use live_commit_sync_config::CfgrLiveCommitSyncConfig;
use metaconfig_types::CommonConfig;
use openssl::ssl::SslAcceptor;
use permission_checker::{MononokeIdentity, MononokeIdentitySet};
use scribe_ext::Scribe;
use slog::{debug, error, info, warn, Logger};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};

use cmdlib::monitoring::ReadyFlagService;
use limits::types::MononokeThrottleLimits;
use sshrelay::{
    IoStream, Metadata, Preamble, Priority, SshDecoder, SshEncoder, SshEnvVars, SshMsg, Stdio,
};
use stats::prelude::*;

use crate::errors::ErrorKind;
use crate::repo_handlers::RepoHandler;
use crate::request_handler::{create_conn_logger, request_handler};
use crate::stream::QuietShutdownStream;

define_stats! {
    prefix = "mononoke.connection_acceptor";
    http_accepted: timeseries(Sum),
    hgcli_accepted: timeseries(Sum),
}

pub trait MononokeStream: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static {}

impl<T> MononokeStream for T where T: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static {}

const CHUNK_SIZE: usize = 10000;
const CONFIGERATOR_LIMITS_CONFIG: &str = "scm/mononoke/loadshedding/limits";
lazy_static! {
    static ref OPEN_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
}

pub async fn wait_for_connections_closed() {
    while OPEN_CONNECTIONS.load(Ordering::Relaxed) > 0 {
        tokio::time::delay_for(Duration::new(1, 0)).await;
    }
}

/// This function accepts connections, reads Preamble and routes first_line to a thread responsible for
/// a particular repo
pub async fn connection_acceptor(
    fb: FacebookInit,
    test_instance: bool,
    common_config: CommonConfig,
    sockname: String,
    service: ReadyFlagService,
    root_log: Logger,
    repo_handlers: HashMap<String, RepoHandler>,
    tls_acceptor: SslAcceptor,
    terminate_process: oneshot::Receiver<()>,
    config_store: &ConfigStore,
    scribe: Scribe,
) -> Result<()> {
    let load_limiting_config = {
        let config_loader = config_store
            .get_config_handle(CONFIGERATOR_LIMITS_CONFIG.to_string())
            .ok();
        config_loader.and_then(|config_loader| {
            common_config
                .loadlimiter_category
                .clone()
                .map(|category| (config_loader, category))
        })
    };

    let maybe_live_commit_sync_config = CfgrLiveCommitSyncConfig::new(&root_log, &config_store)
        .map(Option::Some)
        .or_else(|e| if test_instance { Ok(None) } else { Err(e) })?;

    let enable_http_control_api = common_config.enable_http_control_api;

    let security_checker =
        ConnectionsSecurityChecker::new(fb, common_config, &repo_handlers, &root_log).await?;
    let addr: SocketAddr = sockname.parse()?;
    let mut listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("could not bind mononoke on '{}'", sockname))?;

    // Now that we are listening and ready to accept connections, report that we are alive.
    service.set_ready();

    let mut terminate_process = terminate_process.fuse();

    let acceptor = Arc::new(Acceptor {
        fb,
        tls_acceptor,
        repo_handlers,
        security_checker,
        load_limiting_config,
        maybe_live_commit_sync_config,
        scribe,
        logger: root_log.clone(),
        enable_http_control_api,
    });

    loop {
        select_biased! {
            _ = terminate_process => {
                debug!(root_log, "Received shutdown handler, stop accepting connections...");
                return Ok(());
            },
            sock_tuple = listener.accept().fuse() => match sock_tuple {
                Ok((stream, addr)) => {
                    let conn = PendingConnection { acceptor: acceptor.clone(), addr };
                    let task = handle_connection(conn.clone(), stream);
                    conn.spawn_task(task, "Failed to handle_connection");
                }
                Err(err) => {
                    error!(root_log, "{}", err.to_string(); SlogKVError(Error::from(err)));
                }
            },
        };
    }
}

/// Our environment for accepting connections.
pub struct Acceptor {
    pub fb: FacebookInit,
    pub tls_acceptor: SslAcceptor,
    pub repo_handlers: HashMap<String, RepoHandler>,
    pub security_checker: ConnectionsSecurityChecker,
    pub load_limiting_config: Option<(ConfigHandle<MononokeThrottleLimits>, String)>,
    pub maybe_live_commit_sync_config: Option<CfgrLiveCommitSyncConfig>,
    pub scribe: Scribe,
    pub logger: Logger,
    pub enable_http_control_api: bool,
}

/// Details for a socket we've just opened.
#[derive(Clone)]
pub struct PendingConnection {
    pub acceptor: Arc<Acceptor>,
    pub addr: SocketAddr,
}

/// A connection where we completed the initial TLS handshake.
#[derive(Clone)]
pub struct AcceptedConnection {
    pub pending: PendingConnection,
    pub is_trusted: bool,
    pub identities: Arc<MononokeIdentitySet>,
}

impl PendingConnection {
    /// Spawn a task that is dedicated to this connection. This will block server shutdown, and
    /// also log on error.
    pub fn spawn_task(
        &self,
        task: impl Future<Output = Result<()>> + Send + 'static,
        label: &'static str,
    ) {
        let this = self.clone();

        OPEN_CONNECTIONS.fetch_add(1, Ordering::Relaxed);

        tokio::task::spawn(async move {
            let res = task
                .await
                .context(label)
                .with_context(|| format!("Failed to handle connection to {}", this.addr));

            if let Err(e) = res {
                error!(&this.acceptor.logger, "connection_acceptor error: {:#}", e);
            }

            OPEN_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn handle_connection(conn: PendingConnection, sock: TcpStream) -> Result<()> {
    let ssl_socket = tokio_openssl::accept(&conn.acceptor.tls_acceptor, sock)
        .await
        .context("Failed to perform tls handshake")?;

    let identities = match ssl_socket.ssl().peer_certificate() {
        Some(cert) => MononokeIdentity::try_from_x509(&cert),
        None => Err(ErrorKind::ConnectionNoClientCertificate.into()),
    }?;

    let is_trusted = conn
        .acceptor
        .security_checker
        .check_if_trusted(&identities)
        .await?;

    let conn = AcceptedConnection {
        pending: conn,
        is_trusted,
        identities: Arc::new(identities),
    };

    let is_hgcli = ssl_socket.ssl().selected_alpn_protocol() == Some(alpn::HGCLI_ALPN.as_bytes());

    let ssl_socket = QuietShutdownStream::new(ssl_socket);

    if is_hgcli {
        handle_hgcli(conn, ssl_socket)
            .await
            .context("Failed to handle_hgcli")?;
    } else {
        handle_http(conn, ssl_socket)
            .await
            .context("Failed to handle_http")?;
    }

    Ok(())
}

async fn handle_hgcli<S: MononokeStream>(conn: AcceptedConnection, stream: S) -> Result<()> {
    STATS::hgcli_accepted.add_value(1);

    let (rx, tx) = tokio::io::split(stream);

    let mut framed = FramedConn::setup(rx, tx);

    let preamble = match framed.rd.next().await.transpose()? {
        Some(maybe_preamble) => {
            if let IoStream::Preamble(preamble) = maybe_preamble.stream() {
                preamble
            } else {
                return Err(ErrorKind::NoConnectionPreamble.into());
            }
        }
        None => {
            return Err(ErrorKind::NoConnectionPreamble.into());
        }
    };

    let channels = ChannelConn::setup(framed);

    let metadata = if conn.is_trusted {
        // Relayed through trusted proxy. Proxy authenticates end client and generates
        // preamble so we can trust it. Use identity provided in preamble.
        Some(
            try_convert_preamble_to_metadata(&preamble, conn.pending.addr.ip(), &channels.logger)
                .await?,
        )
    } else {
        None
    };

    handle_wireproto(conn, channels, preamble.reponame, metadata, false)
        .await
        .context("Failed to handle_wireproto")?;

    Ok(())
}

async fn handle_http<S: MononokeStream>(conn: AcceptedConnection, stream: S) -> Result<()> {
    STATS::http_accepted.add_value(1);

    let svc = MononokeHttpService::<S>::new(conn);

    // NOTE: We don't select h2 in alpn, so we only expect HTTP/1.1 here.
    Http::new()
        .http1_only(true)
        .serve_connection(stream, svc)
        .with_upgrades()
        .await
        .context("Failed to serve_connection")?;

    Ok(())
}

pub async fn handle_wireproto(
    conn: AcceptedConnection,
    channels: ChannelConn,
    reponame: String,
    metadata: Option<Metadata>,
    client_debug: bool,
) -> Result<()> {
    let metadata = if let Some(metadata) = metadata {
        metadata
    } else {
        // Most likely client is not trusted. Use TLS connection
        // cert as identity.
        Metadata::new(
            Some(&generate_session_id().to_string()),
            conn.is_trusted,
            (*conn.identities).clone(),
            Priority::Default,
            client_debug,
            Some(conn.pending.addr.ip()),
        )
        .await
    };

    let ChannelConn {
        stdin,
        stdout,
        stderr,
        logger,
        join_handle,
    } = channels;

    if metadata.client_debug() {
        info!(&logger, "{:#?}", metadata; "remote" => "true");
    }

    // Don't let the logger hold onto the channel. This is a bit fragile (but at least it breaks
    // tests deterministically).
    drop(logger);

    let stdio = Stdio {
        metadata,
        stdin,
        stdout,
        stderr,
    };

    request_handler(
        conn.pending.acceptor.fb,
        reponame,
        &conn.pending.acceptor.repo_handlers,
        &conn.pending.acceptor.security_checker,
        stdio,
        conn.pending.acceptor.load_limiting_config.clone(),
        conn.pending.addr.ip(),
        conn.pending.acceptor.maybe_live_commit_sync_config.clone(),
        conn.pending.acceptor.scribe.clone(),
    )
    .await
    .context("Failed to execute request_handler")?;

    join_handle
        .await
        .context("Failed to join ChannelConn")?
        .context("Failed to close ChannelConn")?;

    Ok(())
}

pub struct FramedConn<R, W> {
    rd: FramedRead<R, SshDecoder>,
    wr: FramedWrite<W, SshEncoder>,
}

impl<R, W> FramedConn<R, W>
where
    R: AsyncRead + Send + std::marker::Unpin + 'static,
    W: AsyncWrite + Send + std::marker::Unpin + 'static,
{
    pub fn setup(rd: R, wr: W) -> Self {
        // NOTE: FramedRead does buffering, so no need to wrap with a BufReader here.
        let rd = FramedRead::new(rd, SshDecoder::new());
        let wr = FramedWrite::new(wr, SshEncoder::new());
        Self { rd, wr }
    }
}

pub struct ChannelConn {
    stdin: BoxStream<Bytes, io::Error>,
    stdout: mpsc::Sender<Bytes>,
    stderr: mpsc::UnboundedSender<Bytes>,
    logger: Logger,
    join_handle: JoinHandle<Result<(), io::Error>>,
}

impl ChannelConn {
    pub fn setup<R, W>(conn: FramedConn<R, W>) -> Self
    where
        R: AsyncRead + Send + std::marker::Unpin + 'static,
        W: AsyncWrite + Send + std::marker::Unpin + 'static,
    {
        let FramedConn { rd, wr } = conn;

        let stdin = Box::new(rd.compat().filter_map(|s| {
            if s.stream() == IoStream::Stdin {
                Some(s.data())
            } else {
                None
            }
        }));

        let (stdout, stderr, join_handle) = {
            let (otx, orx) = mpsc::channel(1);
            let (etx, erx) = mpsc::unbounded();

            let orx = orx
                .map(|blob| split_bytes_in_chunk(blob, CHUNK_SIZE))
                .flatten()
                .map(|v| SshMsg::new(IoStream::Stdout, v));
            let erx = erx
                .map(|blob| split_bytes_in_chunk(blob, CHUNK_SIZE))
                .flatten()
                .map(|v| SshMsg::new(IoStream::Stderr, v));

            // Glue them together
            let fwd = orx
                .select(erx)
                .compat()
                .map_err(|()| io::Error::new(io::ErrorKind::Other, "huh?"))
                .forward(wr);

            // spawn a task for forwarding stdout/err into stream
            let join_handle = tokio::spawn(fwd);

            (otx, etx, join_handle)
        };

        let logger = create_conn_logger(stderr.clone(), None, None);

        ChannelConn {
            stdin,
            stdout,
            stderr,
            logger,
            join_handle,
        }
    }
}

async fn try_convert_preamble_to_metadata(
    preamble: &Preamble,
    addr: IpAddr,
    conn_log: &Logger,
) -> Result<Metadata> {
    let vars = SshEnvVars::from_map(&preamble.misc);
    let client_ip = match vars.ssh_client {
        Some(ssh_client) => ssh_client
            .split_whitespace()
            .next()
            .and_then(|ip| ip.parse::<IpAddr>().ok())
            .unwrap_or(addr),
        None => addr,
    };

    let priority = match Priority::extract_from_preamble(&preamble) {
        Ok(Some(p)) => {
            info!(&conn_log, "Using priority: {}", p; "remote" => "true");
            p
        }
        Ok(None) => Priority::Default,
        Err(e) => {
            warn!(&conn_log, "Could not parse priority: {}", e; "remote" => "true");
            Priority::Default
        }
    };

    let identity = {
        #[cfg(fbcode_build)]
        {
            // SSH Connections are either authentication via ssh certificate principals or
            // via some form of keyboard-interactive. In the case of certificates we should always
            // rely on these. If they are not present, we should fallback to use the unix username
            // as the primary principal.
            let ssh_identities = match vars.ssh_cert_principals {
                Some(ssh_identities) => ssh_identities,
                None => preamble
                    .unix_name()
                    .ok_or_else(|| anyhow!("missing username and principals from preamble"))?
                    .to_string(),
            };

            MononokeIdentity::try_from_ssh_encoded(&ssh_identities)?
        }
        #[cfg(not(fbcode_build))]
        {
            use maplit::btreeset;
            btreeset! { MononokeIdentity::new(
                "USER",
               preamble
                    .unix_name()
                    .ok_or_else(|| anyhow!("missing username from preamble"))?
                    .to_string(),
            )?}
        }
    };

    Ok(Metadata::new(
        preamble.misc.get("session_uuid"),
        true,
        identity,
        priority,
        preamble
            .misc
            .get("client_debug")
            .map(|debug| debug.parse::<bool>().unwrap_or_default())
            .unwrap_or_default(),
        Some(client_ip),
    )
    .await)
}

// TODO(stash): T33775046 we had to chunk responses because hgcli
// can't cope with big chunks
fn split_bytes_in_chunk<E>(blob: Bytes, chunksize: usize) -> impl Stream<Item = Bytes, Error = E> {
    stream::unfold(blob, move |mut remain| {
        let len = remain.len();
        if len > 0 {
            let ret = remain.split_to(::std::cmp::min(chunksize, len));
            Some(Ok((ret, remain)))
        } else {
            None
        }
    })
}
