//! Persistent envd process handles and incremental Connect framing.
use super::{Inner, Lifecycle, MAX_WIRE_FRAME, wire};
use crate::types::{ProcessEvent, RuntimeProcess};
use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use prost::Message;
use std::{pin::Pin, sync::Arc};

pub(super) struct RemoteProcess {
    pub(super) inner: Arc<Inner>,
    pub(super) pid: u32,
    pub(super) tag: String,
    pub(super) stream: Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    pub(super) buffer: Vec<u8>,
    pub(super) ended: bool,
    pub(super) cancel: tokio_util::sync::CancellationToken,
}

impl RemoteProcess {
    pub(super) async fn event(&mut self) -> Result<Option<wire::process_event::Event>> {
        loop {
            ensure!(!self.cancel.is_cancelled(), "process cancelled");
            if self.buffer.len() >= 5 {
                let length = u32::from_be_bytes(self.buffer[1..5].try_into().expect("frame length"))
                    as usize;
                ensure!(length <= MAX_WIRE_FRAME, "envd frame exceeds 8 MiB");
                if self.buffer.len() >= 5 + length {
                    let flag = self.buffer[0];
                    let payload = self.buffer[5..5 + length].to_vec();
                    self.buffer.drain(..5 + length);
                    if flag == 2 {
                        let trailer: serde_json::Value = serde_json::from_slice(&payload)?;
                        ensure!(
                            trailer.get("error").is_none(),
                            "envd Connect stream returned an error"
                        );
                        return Ok(None);
                    }
                    ensure!(flag == 0, "unsupported Connect envelope flags {flag}");
                    let response = wire::StartResponse::decode(payload.as_slice())?;
                    if let Some(event) = response.event.and_then(|event| event.event) {
                        return Ok(Some(event));
                    }
                    continue;
                }
            }
            let next = tokio::select! {
                chunk = self.stream.next() => chunk,
                () = self.cancel.cancelled() => bail!("process cancelled"),
            };
            let Some(chunk) = next else {
                ensure!(self.buffer.is_empty(), "truncated Connect envelope");
                return Ok(None);
            };
            let chunk = chunk?;
            ensure!(
                self.buffer.len() + chunk.len() <= 2 * MAX_WIRE_FRAME,
                "envd stream buffer limit exceeded"
            );
            self.buffer.extend_from_slice(&chunk);
        }
    }

    fn selector(&self) -> wire::Selector {
        wire::Selector {
            selector: Some(if self.pid > 0 {
                wire::selector::Selector::Pid(self.pid)
            } else {
                wire::selector::Selector::Tag(self.tag.clone())
            }),
        }
    }
}

#[async_trait]
impl RuntimeProcess for RemoteProcess {
    fn id(&self) -> u32 {
        self.pid
    }
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        ensure!(!self.ended, "process has ended");
        if data.is_empty() {
            return Ok(());
        }
        self.inner
            .unary(
                "SendInput",
                wire::SendInput {
                    process: Some(self.selector()),
                    input: Some(wire::Input {
                        stdin: data.to_vec(),
                    }),
                },
            )
            .await
    }
    async fn next(&mut self) -> Result<Option<ProcessEvent>> {
        if self.ended {
            return Ok(None);
        }
        loop {
            match self.event().await? {
                Some(wire::process_event::Event::Data(data)) => match data.output {
                    Some(wire::data::Output::Stdout(bytes)) => {
                        return Ok(Some(ProcessEvent::Stdout(bytes)));
                    }
                    Some(wire::data::Output::Stderr(bytes)) => {
                        return Ok(Some(ProcessEvent::Stderr(bytes)));
                    }
                    _ => {}
                },
                Some(wire::process_event::Event::End(end)) => {
                    self.ended = true;
                    return Ok(Some(ProcessEvent::Exit(end.exit_code)));
                }
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }
    async fn close(&mut self) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        self.inner
            .unary(
                "SendSignal",
                wire::SendSignal {
                    process: Some(self.selector()),
                    signal: 9,
                },
            )
            .await?;
        self.ended = true;
        Ok(())
    }
}

impl Drop for RemoteProcess {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        let inner = self.inner.clone();
        let request = wire::SendSignal {
            process: Some(self.selector()),
            signal: 9,
        };
        self.inner.control_runtime.spawn(async move {
            // Closing admission does not confirm that the VM stopped. A failed
            // or cancelled pause/delete still needs best-effort process cleanup.
            if matches!(
                *inner.lifecycle.lock().await,
                Lifecycle::Paused | Lifecycle::Deleted
            ) {
                return;
            }
            let _ = inner.unary("SendSignal", request).await;
        });
    }
}
