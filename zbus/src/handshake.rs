use std::convert::TryInto;
use std::io::BufRead;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

use nix::poll::PollFlags;
use nix::unistd::Uid;

use crate::connection::id_from_str;
use crate::guid::Guid;
use crate::raw::{RawConnection, Socket};
use crate::utils::wait_on;
use crate::{Error, Result};

/*
 * Client-side handshake logic
 */

enum ClientHandshakeStep {
    Init,
    SendingOauth,
    WaitOauth,
    SendingNegociateFd,
    WaitNegociateFd,
    SendingBegin,
    Done,
}

/// A representation of an in-progress handshake, client-side
pub struct ClientHandshake<S> {
    socket: S,
    buffer: Vec<u8>,
    step: ClientHandshakeStep,
    server_guid: Option<Guid>,
    cap_unix_fd: bool,
}

/// The result of a finalized handshake, client-side
pub struct InitializedClient<S> {
    /// The initialized connection
    pub cx: RawConnection<S>,
    /// The server Guid
    pub server_guid: Guid,
    /// Whether the server has accepted file descriptor passing
    pub cap_unix_fd: bool,
}

impl<S: Socket> ClientHandshake<S> {
    /// Start a handsake on this client socket
    pub fn new(socket: S) -> ClientHandshake<S> {
        ClientHandshake {
            socket,
            buffer: Vec::new(),
            step: ClientHandshakeStep::Init,
            server_guid: None,
            cap_unix_fd: false,
        }
    }

    fn flush_buffer(&mut self) -> Result<()> {
        while !self.buffer.is_empty() {
            let written = self.socket.sendmsg(&self.buffer, &[])?;
            self.buffer.drain(..written);
        }
        Ok(())
    }

    fn read_command(&mut self) -> Result<()> {
        while !self.buffer.ends_with(b"\r\n") {
            let mut buf = [0; 40];
            let (read, _) = self.socket.recvmsg(&mut buf)?;
            self.buffer.extend(&buf[..read]);
        }
        Ok(())
    }

    /// Attempt to advance the handshake
    ///
    /// In non-blocking mode, you need to invoke this method repeatedly
    /// until it returns `Ok(())`. Once it does, the handshake is finished
    /// and you can invoke the `finalize()` method.
    ///
    /// Note that only the intial handshake is done. If you need to send a
    /// Bus Hello, this remains to be done.
    pub fn advance_handshake(&mut self) -> Result<()> {
        loop {
            match self.step {
                ClientHandshakeStep::Init => {
                    // send the SASL handshake
                    let uid_str = Uid::current()
                        .to_string()
                        .chars()
                        .map(|c| format!("{:x}", c as u32))
                        .collect::<String>();
                    self.buffer = format!("\0AUTH EXTERNAL {}\r\n", uid_str).into();
                    self.step = ClientHandshakeStep::SendingOauth;
                }
                ClientHandshakeStep::SendingOauth => {
                    self.flush_buffer()?;
                    self.step = ClientHandshakeStep::WaitOauth;
                }
                ClientHandshakeStep::WaitOauth => {
                    self.read_command()?;
                    let mut reply = String::new();
                    (&self.buffer[..]).read_line(&mut reply)?;
                    let mut words = reply.split_whitespace();
                    // We expect a 2 words answer "OK" and the server Guid
                    let guid = match (words.next(), words.next(), words.next()) {
                        (Some("OK"), Some(guid), None) => guid.try_into()?,
                        _ => {
                            return Err(Error::Handshake(
                                "Unexpected server AUTH reply".to_string(),
                            ))
                        }
                    };
                    self.server_guid = Some(guid);
                    self.buffer = Vec::from(&b"NEGOTIATE_UNIX_FD\r\n"[..]);
                    self.step = ClientHandshakeStep::SendingNegociateFd;
                }
                ClientHandshakeStep::SendingNegociateFd => {
                    self.flush_buffer()?;
                    self.step = ClientHandshakeStep::WaitNegociateFd;
                }
                ClientHandshakeStep::WaitNegociateFd => {
                    self.read_command()?;
                    if self.buffer.starts_with(b"AGREE_UNIX_FD") {
                        self.cap_unix_fd = true;
                    } else if self.buffer.starts_with(b"ERROR") {
                        self.cap_unix_fd = false;
                    } else {
                        return Err(Error::Handshake(
                            "Unexpected server UNIX_FD reply".to_string(),
                        ));
                    }
                    self.buffer = Vec::from(&b"BEGIN\r\n"[..]);
                    self.step = ClientHandshakeStep::SendingBegin;
                }
                ClientHandshakeStep::SendingBegin => {
                    self.flush_buffer()?;
                    self.step = ClientHandshakeStep::Done;
                }
                ClientHandshakeStep::Done => return Ok(()),
            }
        }
    }

    /// Attempt to finalize this handshake into an initialized client.
    ///
    /// This method should only be called once `advance_handshake()` has
    /// returned `Ok(())`. Otherwise it'll error and return you the object.
    pub fn try_finish(self) -> std::result::Result<InitializedClient<S>, Self> {
        if let ClientHandshakeStep::Done = self.step {
            Ok(InitializedClient {
                cx: RawConnection::wrap(self.socket),
                server_guid: self.server_guid.unwrap(),
                cap_unix_fd: self.cap_unix_fd,
            })
        } else {
            Err(self)
        }
    }
}

impl ClientHandshake<UnixStream> {
    /// Block and automatically drive the handshake for this client
    ///
    /// This method will block until the handshake is finalized, even if the
    /// socket is in non-blocking mode.
    pub fn blocking_finish(mut self) -> Result<InitializedClient<UnixStream>> {
        loop {
            match self.advance_handshake() {
                Ok(()) => return Ok(self.try_finish().unwrap_or_else(|_| unreachable!())),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // we raised a WouldBlock error, this means this is a non-blocking socket
                    // we use poll to wait until the action we need is available
                    let flags = match self.step {
                        ClientHandshakeStep::SendingOauth
                        | ClientHandshakeStep::SendingNegociateFd
                        | ClientHandshakeStep::SendingBegin => PollFlags::POLLOUT,
                        ClientHandshakeStep::WaitOauth | ClientHandshakeStep::WaitNegociateFd => {
                            PollFlags::POLLIN
                        }
                        ClientHandshakeStep::Init | ClientHandshakeStep::Done => unreachable!(),
                    };
                    wait_on(self.socket.as_raw_fd(), flags)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/*
 * Server-side handshake logic
 */

enum ServerHandshakeStep {
    WaitingForNull,
    WaitingForAuth,
    SendingAuthOK,
    SendingAuthError,
    WaitingForBegin,
    SendingBeginMessage,
    Done,
}

/// A representation of an in-progress handshake, server-side
pub struct ServerHandshake<S> {
    socket: S,
    buffer: Vec<u8>,
    step: ServerHandshakeStep,
    server_guid: Guid,
    cap_unix_fd: bool,
    client_uid: u32,
}

/// The result of a finalized handshake, server-side
pub struct InitializedServer<S> {
    /// The initialized connection
    pub cx: RawConnection<S>,
    /// The server Guid
    pub server_guid: Guid,
    /// Whether the client has requested file descriptor passing
    pub cap_unix_fd: bool,
}

impl<S: Socket> ServerHandshake<S> {
    pub fn new(socket: S, guid: Guid, client_uid: u32) -> ServerHandshake<S> {
        ServerHandshake {
            socket,
            buffer: Vec::new(),
            step: ServerHandshakeStep::WaitingForNull,
            server_guid: guid,
            cap_unix_fd: false,
            client_uid,
        }
    }

    fn flush_buffer(&mut self) -> Result<()> {
        while !self.buffer.is_empty() {
            let written = self.socket.sendmsg(&self.buffer, &[])?;
            self.buffer.drain(..written);
        }
        Ok(())
    }

    fn read_command(&mut self) -> Result<()> {
        while !self.buffer.ends_with(b"\r\n") {
            let mut buf = [0; 40];
            let (read, _) = self.socket.recvmsg(&mut buf)?;
            self.buffer.extend(&buf[..read]);
        }
        Ok(())
    }

    /// Attempt to advance the handshake
    ///
    /// In non-blocking mode, you need to invoke this method repeatedly
    /// until it returns `Ok(())`. Once it does, the handshake is finished
    /// and you can invoke the `finalize()` method.
    ///
    /// Note that only the intial handshake is done. If you need to send a
    /// Bus Hello, this remains to be done.
    pub fn advance_handshake(&mut self) -> Result<()> {
        loop {
            match self.step {
                ServerHandshakeStep::WaitingForNull => {
                    let mut buffer = [0; 1];
                    let (read, _) = self.socket.recvmsg(&mut buffer)?;
                    // recvmsg cannot return anything else than Ok(1) or Err
                    debug_assert!(read == 1);
                    if buffer[0] != 0 {
                        return Err(Error::Handshake(
                            "First client byte is not NUL!".to_string(),
                        ));
                    }
                    self.step = ServerHandshakeStep::WaitingForAuth;
                }
                ServerHandshakeStep::WaitingForAuth => {
                    self.read_command()?;
                    let mut reply = String::new();
                    (&self.buffer[..]).read_line(&mut reply)?;
                    let mut words = reply.split_whitespace();
                    match (words.next(), words.next(), words.next(), words.next()) {
                        (Some("AUTH"), Some("EXTERNAL"), Some(uid), None) => {
                            let uid = id_from_str(uid)
                                .map_err(|e| Error::Handshake(format!("Invalid UID: {}", e)))?;
                            if uid == self.client_uid {
                                self.buffer = format!("OK {}\r\n", self.server_guid).into();
                                self.step = ServerHandshakeStep::SendingAuthOK;
                            } else {
                                self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                                self.step = ServerHandshakeStep::SendingAuthError;
                            }
                        }
                        (Some("AUTH"), _, _, _) | (Some("ERROR"), _, _, _) => {
                            self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                        (Some("BEGIN"), None, None, None) => {
                            return Err(Error::Handshake(
                                "Received BEGIN while not authenticated".to_string(),
                            ));
                        }
                        _ => {
                            self.buffer = Vec::from(&b"ERROR Unsupported command\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                    }
                }
                ServerHandshakeStep::SendingAuthError => {
                    self.flush_buffer()?;
                    self.step = ServerHandshakeStep::WaitingForAuth;
                }
                ServerHandshakeStep::SendingAuthOK => {
                    self.flush_buffer()?;
                    self.step = ServerHandshakeStep::WaitingForBegin;
                }
                ServerHandshakeStep::WaitingForBegin => {
                    self.read_command()?;
                    let mut reply = String::new();
                    (&self.buffer[..]).read_line(&mut reply)?;
                    let mut words = reply.split_whitespace();
                    match (words.next(), words.next()) {
                        (Some("BEGIN"), None) => {
                            self.step = ServerHandshakeStep::Done;
                        }
                        (Some("CANCEL"), None) => {
                            self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                        (Some("ERROR"), _) => {
                            self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                        (Some("NEGOTIATE_UNIX_FD"), None) => {
                            self.cap_unix_fd = true;
                            self.buffer = Vec::from(&b"AGREE_UNIX_FD\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingBeginMessage;
                        }
                        _ => {
                            self.buffer = Vec::from(&b"ERROR Unsupported command\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingBeginMessage;
                        }
                    }
                }
                ServerHandshakeStep::SendingBeginMessage => {
                    self.flush_buffer()?;
                    self.step = ServerHandshakeStep::WaitingForBegin;
                }
                ServerHandshakeStep::Done => return Ok(()),
            }
        }
    }

    /// Attempt to finalize this handshake into an initialized server.
    ///
    /// This method should only be called once `advance_handshake()` has
    /// returned `Ok(())`. Otherwise it'll error and return you the object.
    pub fn try_finish(self) -> std::result::Result<InitializedServer<S>, Self> {
        if let ServerHandshakeStep::Done = self.step {
            Ok(InitializedServer {
                cx: RawConnection::wrap(self.socket),
                server_guid: self.server_guid,
                cap_unix_fd: self.cap_unix_fd,
            })
        } else {
            Err(self)
        }
    }
}

impl ServerHandshake<UnixStream> {
    /// Block and automatically drive the handshake for this server
    ///
    /// This method will block until the handshake is finalized, even if the
    /// socket is in non-blocking mode.
    pub fn blocking_finish(mut self) -> Result<InitializedServer<UnixStream>> {
        loop {
            match self.advance_handshake() {
                Ok(()) => return Ok(self.try_finish().unwrap_or_else(|_| unreachable!())),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // we raised a WouldBlock error, this means this is a non-blocking socket
                    // we use poll to wait until the action we need is available
                    let flags = match self.step {
                        ServerHandshakeStep::SendingAuthError
                        | ServerHandshakeStep::SendingAuthOK
                        | ServerHandshakeStep::SendingBeginMessage => PollFlags::POLLOUT,
                        ServerHandshakeStep::WaitingForNull
                        | ServerHandshakeStep::WaitingForBegin
                        | ServerHandshakeStep::WaitingForAuth => PollFlags::POLLIN,
                        ServerHandshakeStep::Done => unreachable!(),
                    };
                    wait_on(self.socket.as_raw_fd(), flags)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}
