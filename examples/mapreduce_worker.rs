use std::net::SocketAddr;

use cuely::mapreduce::{Map, Reduce, Worker};
use serde::{Deserialize, Serialize};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

#[derive(Serialize, Deserialize, Debug)]
struct Job {
    id: usize,
}

#[derive(Serialize, Deserialize, Debug)]
struct Count(usize);

impl Map<Count> for Job {
    fn map(self) -> Count {
        std::thread::sleep(std::time::Duration::from_secs(2)); // simulate some long running task
        Count(1)
    }
}

impl Reduce for Count {
    fn reduce(self, element: Self) -> Self {
        Count(self.0 + element.0)
    }
}

#[tokio::main]
async fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let args: Vec<_> = std::env::args().collect();

    Worker::run::<Job, Count>(args[1].parse::<SocketAddr>().unwrap())
        .await
        .expect("failed to run worker");
}
