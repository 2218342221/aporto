//! Pinned envd process.proto subset used by the Connect adapter.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ProcessConfig {
    #[prost(string, tag = "1")]
    pub cmd: String,
    #[prost(string, repeated, tag = "2")]
    pub args: Vec<String>,
    #[prost(btree_map = "string, string", tag = "3")]
    pub envs: std::collections::BTreeMap<String, String>,
    #[prost(string, optional, tag = "4")]
    pub cwd: Option<String>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct StartRequest {
    #[prost(message, optional, tag = "1")]
    pub process: Option<ProcessConfig>,
    #[prost(string, optional, tag = "3")]
    pub tag: Option<String>,
    #[prost(bool, optional, tag = "4")]
    pub stdin: Option<bool>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct StartResponse {
    #[prost(message, optional, tag = "1")]
    pub event: Option<Event>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Event {
    #[prost(oneof = "process_event::Event", tags = "1, 2, 3, 4")]
    pub event: Option<process_event::Event>,
}
pub mod process_event {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Start(super::Start),
        #[prost(message, tag = "2")]
        Data(super::Data),
        #[prost(message, tag = "3")]
        End(super::End),
        #[prost(message, tag = "4")]
        KeepAlive(super::Empty),
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Start {
    #[prost(uint32, tag = "1")]
    pub pid: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Data {
    #[prost(oneof = "data::Output", tags = "1, 2, 3")]
    pub output: Option<data::Output>,
}
pub mod data {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Output {
        #[prost(bytes, tag = "1")]
        Stdout(Vec<u8>),
        #[prost(bytes, tag = "2")]
        Stderr(Vec<u8>),
        #[prost(bytes, tag = "3")]
        Pty(Vec<u8>),
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct End {
    #[prost(sint32, tag = "1")]
    pub exit_code: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Empty {}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Selector {
    #[prost(oneof = "selector::Selector", tags = "1, 2")]
    pub selector: Option<selector::Selector>,
}
pub mod selector {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Selector {
        #[prost(uint32, tag = "1")]
        Pid(u32),
        #[prost(string, tag = "2")]
        Tag(String),
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct SendInput {
    #[prost(message, optional, tag = "1")]
    pub process: Option<Selector>,
    #[prost(message, optional, tag = "2")]
    pub input: Option<Input>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Input {
    #[prost(bytes, tag = "1")]
    pub stdin: Vec<u8>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct SendSignal {
    #[prost(message, optional, tag = "1")]
    pub process: Option<Selector>,
    #[prost(int32, tag = "2")]
    pub signal: i32,
}
