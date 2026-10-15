use super::*;

/// Where a relay connection asks to go: `ip:port` for a container in the
/// pod's Docker range, and nothing else, so the relay can't reach the
/// cluster or the LAN.
pub(super) fn relay_target(line: &str) -> Result<std::net::SocketAddrV4, String> {
    let (host, port) = line
        .rsplit_once(':')
        .ok_or_else(|| format!("{line:?} isn't ip:port"))?;
    let ip = docker_net::container_address(host)
        .ok_or_else(|| format!("{host:?} isn't a container address in the pod"))?;
    let port: u16 = port
        .parse()
        .ok()
        .filter(|&p| p != 0)
        .ok_or_else(|| format!("{port:?} isn't a port"))?;
    Ok(std::net::SocketAddrV4::new(ip, port))
}

/// Serves `RELAY_ADDR`: each connection names a container address on its
/// first line. Once connected, the relay answers `RELAY_CONNECTED`, then
/// carries raw bytes to and from it. A refused or failed target just
/// closes the connection, the same as a port nothing listens on, which is
/// how smelt reports it. The answer matters: a connect to an address no
/// container has yet hangs rather than fails, and without it smelt would
/// take that silence for a server waiting for a request.
pub(super) async fn serve_relay(listener: tokio::net::TcpListener) {
    accept_loop(
        || async { listener.accept().await.map(|(client, _)| client) },
        |client| {
            tokio::spawn(async move {
                if let Err(e) = relay(client).await {
                    tracing::debug!("relay: {e}");
                }
            });
        },
    )
    .await
}

/// Accepts connections for ever, handing each to `handle`.
pub(super) async fn accept_loop<S, A, Fut, H>(mut accept: A, mut handle: H)
where
    A: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<S>>,
    H: FnMut(S),
{
    loop {
        match accept().await {
            Ok(client) => handle(client),
            Err(e) => {
                // A failure like EMFILE fails again at once: wait instead of
                // spinning, as the server's own accept loops do.
                tracing::warn!("relay: accept failed: {e}");
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
}

/// How long `accept_loop` waits after a failed accept.
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

async fn relay(client: tokio::net::TcpStream) -> Result<(), String> {
    let mut client = BufReader::new(client);
    let mut line = String::new();
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        (&mut client).take(RELAY_TARGET_MAX as u64).read_line(&mut line),
    )
    .await
    .map_err(|_| "timed out waiting for the target".to_string())?
    .map_err(|e| e.to_string())?;
    if read == 0 || !line.ends_with('\n') {
        return Err("no target line".to_string());
    }
    let target = relay_target(line.trim_end())?;
    let mut upstream = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(target))
        .await
        .map_err(|_| format!("timed out connecting to {target}"))?
        .map_err(|e| format!("couldn't connect to {target}: {e}"))?;
    // Bytes the client sent after its target line are already buffered.
    let buffered = client.buffer().to_vec();
    let mut client = client.into_inner();
    client.write_all(RELAY_CONNECTED).await.map_err(|e| e.to_string())?;
    upstream.write_all(&buffered).await.map_err(|e| e.to_string())?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}
