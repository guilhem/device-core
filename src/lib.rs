pub mod audio;
pub mod auth;
pub mod common;
pub mod config;
pub mod http;
pub mod maintenance;
pub mod manager;
pub mod network;
pub mod options;
pub mod runtime;
pub mod system;
pub mod update;
pub mod voice;

#[macro_export]
macro_rules! error { ($($a:tt)*) => { eprintln!("<3>{}", format_args!($($a)*)) } }
#[macro_export]
macro_rules! warn { ($($a:tt)*) => { eprintln!("<4>{}", format_args!($($a)*)) } }
#[macro_export]
macro_rules! info { ($($a:tt)*) => { eprintln!("<6>{}", format_args!($($a)*)) } }
#[macro_export]
macro_rules! debug { ($($a:tt)*) => { if std::env::var("DEVICE_CORE_LOG").as_deref() == Ok("debug") { eprintln!("<7>{}", format_args!($($a)*)) } } }
