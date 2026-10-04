//! A transport observes a turn. Disconnecting an observer does not own cancellation.
use super::*;
use std::future::Future;

pub(crate) trait ChatEventWriter: Send {
    fn send_event<'a>(
        &'a mut self,
        transport: CodexBridgeTransport,
        value: &'a Value,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;
    fn close(&mut self) -> impl Future<Output = Result<(), String>> + Send;
    fn native_turn(&self) -> Option<Arc<crate::native_turns::NativeTurn>> {
        None
    }
}

impl ChatEventWriter for TcpStream {
    async fn send_event(
        &mut self,
        transport: CodexBridgeTransport,
        value: &Value,
    ) -> Result<(), String> {
        match transport {
            CodexBridgeTransport::Sse => send_sse(self, value).await,
            CodexBridgeTransport::WebSocket => crate::chat::send_ws_json(self, value).await,
        }
    }
    async fn close(&mut self) -> Result<(), String> {
        crate::chat::send_ws_close(self).await?;
        let _ = self.shutdown().await;
        Ok(())
    }
}

pub(crate) struct NativeTurnOutput(pub(crate) Arc<crate::native_turns::NativeTurn>);
impl ChatEventWriter for NativeTurnOutput {
    async fn send_event(
        &mut self,
        _transport: CodexBridgeTransport,
        value: &Value,
    ) -> Result<(), String> {
        self.0.publish(value.clone());
        Ok(())
    }
    async fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn native_turn(&self) -> Option<Arc<crate::native_turns::NativeTurn>> {
        Some(self.0.clone())
    }
}

pub(crate) async fn send_chat_event(
    output: &mut impl ChatEventWriter,
    value: &Value,
) -> Result<(), String> {
    output
        .send_event(CodexBridgeTransport::WebSocket, value)
        .await
}
