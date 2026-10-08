#![forbid(unsafe_code)]

use dylos_agent::AGENT_VSOCK_PORT;
use dylos_agent::agent::{Backoff, run};
use dylos_agent::clock::SystemClock;
use tokio_vsock::{VMADDR_CID_HOST, VsockAddr, VsockStream};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let never = run(
        || VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, AGENT_VSOCK_PORT)),
        &SystemClock,
        Backoff::default(),
    )
    .await;
    match never {}
}
