#![forbid(unsafe_code)]
#![warn(clippy::all)]

use naptime::{EventHandler, Naptime};
use tracing::info;

struct Printer {}
impl EventHandler for Printer {
  fn sleep_query(&mut self) -> naptime::SleepQueryResponse {
    info!("denying sleep");
    naptime::SleepQueryResponse::Deny
  }

  fn sleep_failed(&mut self) {
    info!("deny successful");
  }

  fn sleep(&mut self) {
    info!("deny failed. sleep tight");
  }
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt().init();

  let printer = Printer {};
  let naptime = Naptime::new(printer);

  info!("chugging the caffeine. Ctrl-C to stop");
  tokio::signal::ctrl_c().await.unwrap();
  drop(naptime);
}
