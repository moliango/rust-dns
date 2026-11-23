// SPDX-License-Identifier: GPL-3.0-only
//! UDP and TCP DNS listeners (design §5).
//!
//! Every listener owns a [`Handler`] (normally the sequence entry) and stops
//! when the supplied shutdown future resolves. Malformed packets are dropped
//! silently; a handler response of `None` means "do not reply".

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::Handler;
use crate::error::{CoreError, Result};

/// Largest DNS message carried over UDP or a TCP frame.
const MAX_MESSAGE_SIZE: usize = u16::MAX as usize;
/// Maximum concurrently processed packets/connections per listener.
const MAX_INFLIGHT: usize = 256;
/// Per-connection read/handle/write budget.
const TCP_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed for in-flight work to finish after shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Bind a UDP socket and serve until `shutdown` resolves.
pub async fn serve_udp<F>(listen: SocketAddr, handler: Arc<dyn Handler>, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let socket = UdpSocket::bind(listen).await?;
    serve_udp_socket(Arc::new(socket), handler, shutdown).await
}

/// Serve on an already-bound UDP socket.
///
/// Kept separate from [`serve_udp`] so callers (and tests) can bind with port
/// `0` and learn the actual address.
pub async fn serve_udp_socket<F>(
    socket: Arc<UdpSocket>,
    handler: Arc<dyn Handler>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let semaphore = Arc::new(Semaphore::new(MAX_INFLIGHT));
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut buf = vec![0u8; MAX_MESSAGE_SIZE];

    tokio::pin!(shutdown);

    loop {
        reap_finished(&mut tasks);

        let permit = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            permit = Arc::clone(&semaphore).acquire_owned() => permit
                .map_err(|err| CoreError::Internal(format!("udp semaphore closed: {err}")))?,
        };

        let received = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            received = socket.recv_from(&mut buf) => received,
        };
        let (len, peer) = match received {
            Ok(pair) => pair,
            Err(err) => {
                // Mirror the TCP accept path: a transient receive error must
                // not stop the listener.
                eprintln!("rust-dns: udp recv failed: {err}");
                continue;
            }
        };

        if len == 0 {
            continue;
        }

        // The buffer is reused, so give the task its own copy.
        let packet = buf[..len].to_vec();
        let handler = Arc::clone(&handler);
        let socket = Arc::clone(&socket);

        tasks.spawn(async move {
            let _permit = permit;
            let Ok(query) = Message::from_vec(&packet) else {
                return;
            };
            let Some(response) = handler.handle(query, peer).await else {
                return;
            };
            let Ok(bytes) = response.to_vec() else {
                return;
            };
            // A failed send (e.g. oversized reply) only drops this packet.
            let _ = socket.send_to(&bytes, peer).await;
        });
    }

    drain(&mut tasks).await;
    Ok(())
}

/// Bind a TCP listener and serve until `shutdown` resolves.
pub async fn serve_tcp<F>(listen: SocketAddr, handler: Arc<dyn Handler>, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind(listen).await?;
    serve_tcp_listener(listener, handler, shutdown).await
}

/// Serve on an already-bound TCP listener.
pub async fn serve_tcp_listener<F>(
    listener: TcpListener,
    handler: Arc<dyn Handler>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let semaphore = Arc::new(Semaphore::new(MAX_INFLIGHT));
    let mut tasks: JoinSet<()> = JoinSet::new();

    tokio::pin!(shutdown);

    loop {
        reap_finished(&mut tasks);

        let permit = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            permit = Arc::clone(&semaphore).acquire_owned() => permit
                .map_err(|err| CoreError::Internal(format!("tcp semaphore closed: {err}")))?,
        };

        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };

        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(err) => {
                // Transient accept errors (EMFILE, ECONNABORTED) must not stop
                // the listener.
                eprintln!("rust-dns: tcp accept failed: {err}");
                continue;
            }
        };

        let handler = Arc::clone(&handler);
        tasks.spawn(async move {
            let _permit = permit;
            handle_tcp_connection(stream, peer, handler).await;
        });
    }

    drain(&mut tasks).await;
    Ok(())
}

/// Handle exactly one query per TCP connection.
async fn handle_tcp_connection(mut stream: TcpStream, peer: SocketAddr, handler: Arc<dyn Handler>) {
    match timeout(TCP_IO_TIMEOUT, tcp_query(&mut stream, peer, &handler)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("rust-dns: tcp client {peer} failed: {err}"),
        Err(_) => eprintln!("rust-dns: tcp client {peer} timed out"),
    }
}

async fn tcp_query(
    stream: &mut TcpStream,
    peer: SocketAddr,
    handler: &Arc<dyn Handler>,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);

    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(());
    }

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    let Ok(query) = Message::from_vec(&buf) else {
        return Ok(());
    };
    let Some(response) = handler.handle(query, peer).await else {
        return Ok(());
    };
    let Ok(bytes) = response.to_vec() else {
        return Ok(());
    };
    let Ok(len) = u16::try_from(bytes.len()) else {
        return Ok(());
    };

    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

/// Drop completed tasks and report panics without stopping the listener.
fn reap_finished(tasks: &mut JoinSet<()>) {
    while let Some(result) = tasks.try_join_next() {
        if let Err(err) = result {
            eprintln!("rust-dns: listener task failed: {err}");
        }
    }
}

/// Wait briefly for in-flight tasks, then abort whatever is left.
async fn drain(tasks: &mut JoinSet<()>) {
    let finished = timeout(SHUTDOWN_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;

    if finished.is_err() {
        tasks.shutdown().await;
    }
}
