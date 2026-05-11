//! VoxWeb P2P 网络层。
//!
//! Phase 2：仅 `NetEndpoint::Local` 落地（基于 futures mpsc 双向通道）。
//! Phase 4 起补 Host / Remote 分支（WebRTC）。

pub mod peer;
pub mod room;
pub mod signaling;
pub mod transport;

use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use voxweb_core::protocol::{ClientMessage, ServerMessage};

/// 网络端点。Phase 2 仅实装 Local 分支。
pub enum NetEndpoint {
    /// 单机模式：通过 mpsc 与同进程的 Server 通信。
    Local {
        tx_client: UnboundedSender<ClientMessage>,
        rx_server: UnboundedReceiver<ServerMessage>,
    },
    /// 房主：Phase 4 引入。
    Host,
    /// 远端客户端：Phase 4 引入。
    Remote,
}

/// Server 侧持有的对偶端，与 `NetEndpoint::Local` 配对。
pub struct ServerInbox {
    pub rx_client: UnboundedReceiver<ClientMessage>,
    pub tx_server: UnboundedSender<ServerMessage>,
}

impl NetEndpoint {
    /// 创建 Local 端点 + 对偶 ServerInbox。
    /// Client 持 NetEndpoint，Server driver 持 ServerInbox。
    pub fn new_local_pair() -> (Self, ServerInbox) {
        let (tx_client, rx_client) = mpsc::unbounded::<ClientMessage>();
        let (tx_server, rx_server) = mpsc::unbounded::<ServerMessage>();
        let endpoint = NetEndpoint::Local {
            tx_client,
            rx_server,
        };
        let inbox = ServerInbox {
            rx_client,
            tx_server,
        };
        (endpoint, inbox)
    }

    /// 发送一条 ClientMessage 给服务端。
    /// Local：push 到 mpsc。Phase 4 Host/Remote：序列化走 DataChannel。
    pub fn send_client_message(&self, msg: ClientMessage) {
        match self {
            NetEndpoint::Local { tx_client, .. } => {
                // mpsc unbounded：发送几乎不会失败（除非 receiver drop）
                let _ = tx_client.unbounded_send(msg);
            }
            NetEndpoint::Host | NetEndpoint::Remote => {
                // Phase 4+ 实装
            }
        }
    }

    /// 非阻塞拉取一条 ServerMessage。
    pub fn try_recv_server_message(&mut self) -> Option<ServerMessage> {
        match self {
            NetEndpoint::Local { rx_server, .. } => rx_server.try_recv().ok(),
            NetEndpoint::Host | NetEndpoint::Remote => None,
        }
    }
}

impl ServerInbox {
    /// 非阻塞拉取一条 ClientMessage。
    pub fn try_recv_client_message(&mut self) -> Option<ClientMessage> {
        self.rx_client.try_recv().ok()
    }

    /// 推一条 ServerMessage 给客户端。
    pub fn send_server_message(&self, msg: ServerMessage) {
        let _ = self.tx_server.unbounded_send(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::protocol::{ClientMessage, ServerMessage};

    #[test]
    fn local_pair_roundtrip() {
        let (mut endpoint, mut inbox) = NetEndpoint::new_local_pair();

        // Client → Server
        endpoint.send_client_message(ClientMessage::Ping { client_time_ms: 42 });
        let received = inbox.try_recv_client_message();
        assert!(matches!(
            received,
            Some(ClientMessage::Ping { client_time_ms: 42 })
        ));

        // Server → Client
        inbox.send_server_message(ServerMessage::Pong {
            client_time_ms: 42,
            server_time_ms: 100,
        });
        let received = endpoint.try_recv_server_message();
        assert!(matches!(
            received,
            Some(ServerMessage::Pong {
                client_time_ms: 42,
                server_time_ms: 100
            })
        ));
    }

    #[test]
    fn try_recv_returns_none_when_empty() {
        let (mut endpoint, mut inbox) = NetEndpoint::new_local_pair();
        assert!(endpoint.try_recv_server_message().is_none());
        assert!(inbox.try_recv_client_message().is_none());
    }
}
