#![warn(clippy::all)]

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Naptime;

pub enum SleepQueryResponse {
  Allow,
  Deny,
}

pub trait EventHandler: Send + 'static {
  fn sleep_query(&mut self) -> SleepQueryResponse {
    SleepQueryResponse::Allow
  }
  fn sleep_failed(&mut self) {}
  fn sleep(&mut self) {}
  fn wake(&mut self) {}
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(String);
