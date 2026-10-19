//! The data-source abstraction shared by demo and live mode. A source gets
//! [`Cmd`]s from the app and pushes [`DataEvent`]s back over a channel; the
//! UI code never knows which one it talks to.

use crate::model::{Cmd, DataEvent};
use tokio::sync::mpsc::UnboundedSender;

pub type EventTx = UnboundedSender<DataEvent>;

pub trait DataSource: Send {
    /// Perform a command. Must not block: long work is spawned onto the
    /// runtime and reports back through the event channel.
    fn handle(&mut self, cmd: Cmd);
}
