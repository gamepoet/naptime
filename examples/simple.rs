#![forbid(unsafe_code)]
#![warn(clippy::all)]

use naptime::{EventHandler, Naptime};
use tracing::info;

struct Printer {}
impl EventHandler for Printer {
  fn sleep_query(&mut self) -> naptime::SleepQueryResponse {
    info!("sleep_query");
    naptime::SleepQueryResponse::Allow
  }

  fn sleep_failed(&mut self) {
    info!("sleep_failed");
  }

  fn sleep(&mut self) {
    info!("sleep");
  }

  fn wake(&mut self) {
    info!("wake");
  }
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt().init();
  info!("hello!");

  let printer = Printer {};
  let naptime = Naptime::new(printer);

  info!("waiting forever. good luck. Ctrl-C to kill");
  tokio::signal::ctrl_c().await.unwrap();
  info!("dropping naptime");
  drop(naptime);
}
