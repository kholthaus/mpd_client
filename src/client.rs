//! The client implementation.

use futures::{
    future::{select, Either},
    sink::SinkExt,
    stream::StreamExt,
};
use mpd_protocol::{MpdCodec, Response as RawResponse};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, ToSocketAddrs},
    sync::{
        mpsc::{self, error::TryRecvError, Receiver, Sender, UnboundedSender},
        oneshot,
    },
};
use tokio_util::codec::{Decoder, Framed};
use tracing::{debug, error, span, trace, warn, Level, Span};
use tracing_futures::Instrument;

#[cfg(unix)]
use tokio::net::UnixStream;

use std::fmt::Debug;
use std::path::Path;

use crate::commands::{responses::Response, Command};
use crate::errors::{CommandError, StateChangeError};
use crate::raw::{Frame, MpdCodecError, RawCommand, RawCommandList};
use crate::state_changes::{StateChanges, Subsystem};

type CommandResponder = oneshot::Sender<Result<RawResponse, CommandError>>;
type StateChangesSender = UnboundedSender<Result<Subsystem, StateChangeError>>;

/// Result returned by a connection attempt.
pub type ConnectResult = Result<(Client, StateChanges), MpdCodecError>;

/// Client for MPD.
///
/// You can use this to send commands to the MPD server, and wait for the response.
///
/// Dropping the `Client` (all clients if it is cloned) will close the connection. You can clone
/// this cheaply, which will result in the connection closing after *all* of the `Client`s are
/// dropped.
///
/// ```no_run
/// use mpd_client::{commands::Play, Client};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let (client, _state_changes) = Client::connect_to("localhost:6600").await?;
///
///     client.command(Play::current()).await?;
///     Ok(())
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Client {
    commands_sender: Sender<(RawCommandList, CommandResponder)>,
    span: Span,
}

impl Client {
    /// Connect to an MPD server at the given TCP address.
    ///
    /// # Panics
    ///
    /// This panics for the same reasons as [`Client::connect`].
    ///
    /// # Errors
    ///
    /// This returns errors in the same conditions as [`Client::connect`], and if connecting to the given
    /// TCP address fails for any reason.
    pub async fn connect_to<A: ToSocketAddrs + Debug>(address: A) -> ConnectResult {
        let span = span!(Level::DEBUG, "client connection", tcp_addr = ?address);
        let connection = TcpStream::connect(address).await?;

        Self::do_connect(connection, span).await
    }

    /// Connect to an MPD server using the Unix socket at the given path.
    ///
    /// # Panics
    ///
    /// This panics for the same reasons as [`Client::connect`].
    ///
    /// # Errors
    ///
    /// This returns errors in the same conditions as [`Client::connect`], and if connecting to the Unix
    /// socket at the given address fails for any reason.
    #[cfg(unix)]
    pub async fn connect_unix<P: AsRef<Path>>(path: P) -> ConnectResult {
        let span = span!(Level::DEBUG, "client connection", unix_addr = ?path.as_ref());
        let connection = UnixStream::connect(path).await?;

        Self::do_connect(connection, span).await
    }

    /// Connect to the MPD server using the given connection.
    ///
    /// Since this method is generic over the connection type it can be used to connect to either a
    /// TCP or Unix socket dynamically or e.g. use a proxy.
    ///
    /// See also [`Client::connect_to`] and [`Client::connect_unix`] for the common connection case.
    ///
    /// # Panics
    ///
    /// Since this spawns a task internally, this will panic when called outside a tokio runtime.
    ///
    /// # Errors
    ///
    /// This will return an error if sending the initial commands over the given transport fails.
    pub async fn connect<C>(connection: C) -> ConnectResult
    where
        C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        Self::do_connect(connection, span!(Level::DEBUG, "client connection")).await
    }

    /// Send a [command].
    ///
    /// This will automatically parse the response to a proper type.
    ///
    /// # Errors
    ///
    /// This returns errors in the same conditions as [`Client::raw_command`], and additionally if the
    /// response fails to convert to the expected type.
    ///
    /// [command]: super::commands
    pub async fn command<C>(&self, cmd: C) -> Result<C::Response, CommandError>
    where
        C: Command,
    {
        let command = cmd.to_command();
        let frame = self.raw_command(command).await?;

        Ok(Response::convert(frame)?)
    }

    /// Send the given command list, and return the (typed) responses.
    ///
    /// The results will be returned in the same order as the input commands. If the input is
    /// empty, this will return an empty `Vec` immediately.
    ///
    /// # Errors
    ///
    /// This returns errors in the same conditions as [`Client::raw_command_list`], and
    /// additionally if the response type conversion fails.
    pub async fn command_list<L, C>(&self, list: L) -> Result<Vec<C::Response>, CommandError>
    where
        L: IntoIterator<Item = C>,
        C: Command,
    {
        let mut commands = list.into_iter().map(|c| c.to_command());

        let mut command_list = match commands.next() {
            Some(c) => RawCommandList::new(c),
            None => return Ok(Vec::new()),
        };

        command_list.extend(commands);

        let frames = self.raw_command_list(command_list).await?;

        frames
            .into_iter()
            .map(Response::convert)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// Send the given command, and return the response to it.
    ///
    /// # Errors
    ///
    /// This will return an error if the connection to MPD is closed (cleanly) or a protocol error
    /// occurs (including IO errors), or if the command results in an MPD error.
    pub async fn raw_command(&self, command: RawCommand) -> Result<Frame, CommandError> {
        self.do_send(RawCommandList::new(command))
            .await?
            .single_frame()
            .map_err(Into::into)
    }

    /// Send the given command list, and return the raw response frames to the contained commands.
    ///
    /// # Errors
    ///
    /// Errors will be returned in the same conditions as with [`Client::raw_command`], but if
    /// *any* of the commands in the list return an error condition, the entire list will be
    /// treated as an error.
    ///
    /// You may recover possible succesful fields in a response from the [error].
    ///
    /// [error]: crate::errors::CommandError::ErrorResponse
    pub async fn raw_command_list(
        &self,
        commands: RawCommandList,
    ) -> Result<Vec<Frame>, CommandError> {
        let res = self.do_send(commands).await?;
        let mut frames = Vec::with_capacity(res.len());

        for frame in res {
            match frame {
                Ok(f) => frames.push(f),
                Err(error) => {
                    return Err(CommandError::ErrorResponse {
                        error,
                        succesful_frames: frames,
                    });
                }
            }
        }

        Ok(frames)
    }

    async fn do_send(&self, commands: RawCommandList) -> Result<RawResponse, CommandError> {
        trace!(?commands, "do_send");
        let (tx, rx) = oneshot::channel();

        let mut commands_sender = self.commands_sender.clone();
        commands_sender.send((commands, tx)).await?;

        rx.await?
    }

    async fn do_connect<C>(connection: C, span: Span) -> Result<(Self, StateChanges), MpdCodecError>
    where
        C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (state_changes_sender, state_changes) = mpsc::unbounded_channel();
        let (commands_sender, commands_receiver) = mpsc::channel(2);

        trace!("sending initial idle command");
        let mut connection = MpdCodec::new().framed(connection);

        if let Err(e) = connection.send(idle()).await {
            error!(error = ?e, "failed to send initial idle command");
            return Err(e);
        }

        debug!("connected succesfully");
        let run_loop = run_loop(connection, commands_receiver, state_changes_sender)
            .instrument(span!(parent: &span, Level::TRACE, "run loop"));

        tokio::spawn(run_loop);

        let client = Self {
            commands_sender,
            span,
        };

        let state_changes = StateChanges { rx: state_changes };
        Ok((client, state_changes))
    }
}

struct State<C> {
    loop_state: LoopState,
    connection: Framed<C, MpdCodec>,
    commands: Receiver<(RawCommandList, CommandResponder)>,
    state_changes: StateChangesSender,
}

#[derive(Debug)]
enum LoopState {
    Idling,
    WaitingForCommandReply(CommandResponder),
}

fn idle() -> RawCommand {
    RawCommand::new("idle")
}

fn cancel_idle() -> RawCommand {
    RawCommand::new("noidle")
}

async fn run_loop<C>(
    connection: Framed<C, MpdCodec>,
    commands: Receiver<(RawCommandList, CommandResponder)>,
    state_changes: StateChangesSender,
) where
    C: AsyncRead + AsyncWrite + Unpin,
{
    trace!("entering run loop");

    let mut state = State {
        loop_state: LoopState::Idling,
        connection,
        commands,
        state_changes,
    };

    loop {
        trace!(state = ?state.loop_state, "loop iteration");

        match run_loop_iteration(state).await {
            Some(new_state) => state = new_state,
            None => break,
        }
    }

    trace!("exited run_loop");
}

async fn run_loop_iteration<C>(mut state: State<C>) -> Option<State<C>>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    match state.loop_state {
        LoopState::Idling => {
            // We are idling (the last command sent to the server was an IDLE).

            // Wait for either a command to send or a message from the server, which would be a
            // state change notification.
            let event = select(state.connection.next(), state.commands.next()).await;

            match event {
                Either::Left((response, _)) => {
                    // A server message was received. Since we were idling, this is a state
                    // change notification or `None` is the connection was closed.

                    match response {
                        Some(Ok(res)) => {
                            if let Some(state_change) = response_to_subsystem(res).transpose() {
                                trace!(?state_change);
                                let _ = state.state_changes.send(state_change);
                            }

                            if let Err(e) = state.connection.send(idle()).await {
                                error!(error = ?e, "failed to start idling after state change");
                                let _ = state.state_changes.send(Err(e.into()));
                                return None;
                            }
                        }
                        Some(Err(e)) => {
                            error!(error = ?e, "state change error");
                            let _ = state.state_changes.send(Err(e.into()));
                            return None;
                        }
                        None => return None, // The connection was closed
                    }
                }
                Either::Right((command, _)) => {
                    // A command was received or the commands channel was dropped. The latter
                    // is an indicator for us to close the connection.

                    let (command, responder) = command?;
                    trace!(?command, "command received");

                    // Cancel currently ongoing idle
                    if let Err(e) = state.connection.send(cancel_idle()).await {
                        error!(error = ?e, "failed to cancel idle prior to sending command");
                        let _ = responder.send(Err(e.into()));
                        return None;
                    }

                    // Response to CANCEL_IDLE above
                    match state.connection.next().await {
                        None => return None,
                        Some(Ok(res)) => {
                            if let Some(state_change) = response_to_subsystem(res).transpose() {
                                trace!(?state_change);
                                let _ = state.state_changes.send(state_change);
                            }
                        }
                        Some(Err(e)) => {
                            error!(error = ?e, "state change error prior to sending command");
                            let _ = responder.send(Err(e.into()));
                            return None;
                        }
                    }

                    // Actually send the command. This sets the state for the next loop
                    // iteration.
                    match state.connection.send(command).await {
                        Ok(_) => state.loop_state = LoopState::WaitingForCommandReply(responder),
                        Err(e) => {
                            error!(error = ?e, "failed to send command");
                            let _ = responder.send(Err(e.into()));
                            return None;
                        }
                    }

                    trace!("command sent succesfully");
                }
            }
        }
        LoopState::WaitingForCommandReply(responder) => {
            // We're waiting for the response to the command associated with `responder`.

            let response = state.connection.next().await?;
            trace!(?response, "response to command received");

            let _ = responder.send(response.map_err(Into::into));

            // See if we can immediately send the next command
            match state.commands.try_recv() {
                Ok((command, responder)) => {
                    trace!(?command, "next command immediately available");
                    match state.connection.send(command).await {
                        Ok(_) => state.loop_state = LoopState::WaitingForCommandReply(responder),
                        Err(e) => {
                            error!(error = ?e, "failed to send command");
                            let _ = responder.send(Err(e.into()));
                            return None;
                        }
                    }
                }
                Err(TryRecvError::Empty) => {
                    trace!("no next command immediately available, idling");

                    // Start idling again
                    state.loop_state = LoopState::Idling;
                    if let Err(e) = state.connection.send(idle()).await {
                        error!(error = ?e, "failed to start idling after receiving command response");
                        let _ = state.state_changes.send(Err(e.into()));
                        return None;
                    }
                }
                Err(TryRecvError::Closed) => return None,
            }
        }
    }

    Some(state)
}

fn response_to_subsystem(res: RawResponse) -> Result<Option<Subsystem>, StateChangeError> {
    let mut frame = res.single_frame()?;

    Ok(match frame.get("changed") {
        Some(raw) => Some(Subsystem::from_raw_string(raw)),
        None => {
            if frame.fields_len() != 0 {
                warn!("state change response was not empty but did not contain `changed` key");
            }

            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_test::{assert_ok, io::Builder as MockBuilder};

    static GREETING: &[u8] = b"OK MPD 0.21.11\n";

    #[tokio::test]
    async fn single_state_change() {
        let io = MockBuilder::new()
            .read(GREETING)
            .write(b"idle\n")
            .read(b"changed: player\nOK\n")
            .write(b"idle\n")
            .build();

        let (_client, mut state_changes) = Client::connect(io).await.expect("connect failed");

        assert_eq!(
            assert_ok!(state_changes.next().await.expect("no state change")),
            Subsystem::Player
        );
    }

    #[tokio::test]
    async fn command() {
        let io = MockBuilder::new()
            .read(GREETING)
            .write(b"idle\n")
            .write(b"noidle\n")
            .read(b"changed: playlist\nOK\n")
            .write(b"hello\n")
            .read(b"foo: bar\nOK\n")
            .write(b"idle\n")
            .build();

        let (client, mut state_changes) = Client::connect(io).await.expect("connect failed");

        let response = client
            .raw_command(RawCommand::new("hello"))
            .await
            .expect("command failed");

        assert_eq!(response.find("foo"), Some("bar"));
        assert_eq!(
            assert_ok!(state_changes.next().await.expect("no state change")),
            Subsystem::Queue
        );
        assert!(state_changes.next().await.is_none());
    }

    #[tokio::test]
    async fn incomplete_response() {
        let io = MockBuilder::new()
            .read(GREETING)
            .write(b"idle\n")
            .write(b"noidle\n")
            .read(b"OK\n")
            .write(b"hello\n")
            .read(b"foo: bar\n")
            .wait(Duration::from_secs(2))
            .read(b"baz: qux\nOK\n")
            .write(b"idle\n")
            .build();

        let (client, _state_changes) = Client::connect(io).await.expect("connect failed");

        let response = client
            .raw_command(RawCommand::new("hello"))
            .await
            .expect("command failed");

        assert_eq!(response.find("foo"), Some("bar"));
    }

    #[tokio::test]
    async fn command_list() {
        let io = MockBuilder::new()
            .read(GREETING)
            .write(b"idle\n")
            .write(b"noidle\n")
            .read(b"OK\n")
            .write(b"command_list_ok_begin\nfoo\nbar\ncommand_list_end\n")
            .read(b"foo: asdf\nlist_OK\n")
            .read(b"baz: qux\nlist_OK\nOK\n")
            .write(b"idle\n")
            .build();

        let (client, _state_changes) = Client::connect(io).await.expect("connect failed");

        let mut commands = RawCommandList::new(RawCommand::new("foo"));
        commands.add(RawCommand::new("bar"));

        let responses = client
            .raw_command_list(commands)
            .await
            .expect("command failed");

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].find("foo"), Some("asdf"));
    }

    #[tokio::test]
    async fn dropping_client() {
        let io = MockBuilder::new().read(GREETING).write(b"idle\n").build();

        let (client, mut state_changes) = Client::connect(io).await.expect("connect failed");

        drop(client);

        assert!(state_changes.next().await.is_none());
    }
}
