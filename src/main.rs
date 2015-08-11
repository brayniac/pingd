#[macro_use]
extern crate log;
extern crate time;
extern crate mio;
extern crate bytes;

use mio::{TryRead, TryWrite};
use mio::tcp::*;
use mio::util::Slab;
use bytes::{Buf, Take};
use std::mem;
use std::net::SocketAddr;
use std::io::Cursor;
use log::{Log, LogLevel, LogLevelFilter, LogMetadata, LogRecord};

const VERSION: &'static str = env!("CARGO_PKG_VERSION");

// Token constants
const SERVER: mio::Token = mio::Token(0);

// A server is a listener with a slab of connections
struct Server {
    listener: TcpListener,
    connections: Slab<Connection>,
}

impl Server {
    fn new(listener: TcpListener) -> Server {

        // 0 is server, clients start at 1
        let connections = Slab::new_starting_at(mio::Token(1), 1024);

        Server {
            listener: listener,
            connections: connections,
        }
    }
}

impl mio::Handler for Server {
    type Timeout = (); // timeouts not used
    type Message = (); // cross-thread notifications not used

    fn ready(&mut self, event_loop: &mut mio::EventLoop<Server>, token: mio::Token, events: mio::EventSet) {
        debug!("socket ready: token={:?} events={:?}", token, events);

        match token {
            SERVER => {
                // new connections
                assert!(events.is_readable());

                match self.listener.accept() {
                    Ok(Some(socket)) => {
                        debug!("accepted new client!");

                        // TODO: this panics when connections exceed slab size
                        match self.connections.insert_with(|token| Connection::new(socket, token)) {
                            Some(token) => {
                                event_loop.register_opt(
                                &self.connections[token].socket,
                                token,
                                mio::EventSet::readable(),
                                mio::PollOpt::edge() | mio::PollOpt::oneshot()
                                ).unwrap();
                            }
                            _ => {
                                debug!("too many established connections")
                            }
                        }



                    }
                    Ok(None) => {
                        debug!("wasn't ready");
                    }
                    Err(e) => {
                        error!("encountered fatal error; err={:?}", e);
                        event_loop.shutdown();
                    }
                }
            }
            _ => {
                self.connections[token].ready(event_loop, events);

                if self.connections[token].is_closed() {
                    let _ = self.connections.remove(token);
                }
            }
        }
    }
}

#[derive(Debug)]
struct Connection {
    socket: TcpStream,
    token: mio::Token,
    state: State,
}

impl Connection {
    fn new(socket: TcpStream, token: mio::Token) -> Connection {
        Connection {
            socket: socket,
            token: token,
            state: State::Reading(vec![]),
        }
    }

    fn ready(&mut self, event_loop: &mut mio::EventLoop<Server>, events: mio::EventSet) {
        debug!("    connection-state={:?}", self.state);

        match self.state {
            State::Reading(..) => {
                assert!(events.is_readable(),
                    "unexpected events; events={:?}", events
                );
                self.read(event_loop)
            }
            State::Writing(..) => {
                assert!(events.is_writable(),
                    "unexpected events; events={:?}", events
                );
                self.write(event_loop)
            }
            _ => unimplemented!(),
        }
    }

     fn read(&mut self, event_loop: &mut mio::EventLoop<Server>) {
        match self.socket.try_read_buf(self.state.mut_read_buf()) {
            Ok(Some(0)) => {
                // If there is any data buffered up, attempt to write it back
                // to the client. Either the socket is currently closed, in
                // which case writing will result in an error, or the client
                // only shutdown half of the socket and is still expecting to
                // receive the buffered data back. See
                // test_handling_client_shutdown() for an illustration
                debug!("    read 0 bytes from client; buffered={}",
                    self.state.read_buf().len()
                );

                match self.state.read_buf().len() {
                    n if n > 0 => {
                        // Transition to a writing state even if a new line has
                        // not yet been received.
                        self.state.transition_to_writing(n);

                        // Re-register the socket with the event loop. This
                        // will notify us when the socket becomes writable.
                        self.reregister(event_loop);
                    }
                    _ => self.state = State::Closed,
                }
            }
            Ok(Some(n)) => {
                debug!("read {} bytes", n);

                // Look for a new line. If a new line is received, then the
                // state is transitioned from `Reading` to `Writing`.
                self.state.try_transition_to_writing();

                // Re-register the socket with the event loop. The current
                // state is used to determine whether we are currently reading
                // or writing.
                self.reregister(event_loop);
            }
            Ok(None) => {
                self.reregister(event_loop);
            }
            Err(e) => {
                info!("client has terminated: {}", e);
                self.state = State::Closed
            }
        }
    }

    fn write(&mut self, event_loop: &mut mio::EventLoop<Server>) {
        // TODO: handle error
        match self.socket.try_write_buf(self.state.mut_write_buf()) {
            Ok(Some(_)) => {
                // If the entire line has been written, transition back to the
                // reading state
                self.state.try_transition_to_reading();

                // Re-register the socket with the event loop.
                self.reregister(event_loop);
            }
            Ok(None) => {
                // The socket wasn't actually ready, re-register the socket
                // with the event loop
                self.reregister(event_loop);
            }
            Err(e) => {
                panic!("got an error trying to write; err={:?}", e);
            }
        }
    }

    fn reregister(&self, event_loop: &mut mio::EventLoop<Server>) {
        event_loop.reregister(&self.socket, self.token, self.state.event_set(), mio::PollOpt::oneshot())
            .unwrap();
    }

    fn is_closed(&self) -> bool {
        match self.state {
            State::Closed => true,
            _ => false,
        }
    }
}

// The current state of the client connection
#[derive(Debug)]
enum State {
    // We are currently reading data from the client into the `Vec<u8>`. This
    // is done until we see a new line.
    Reading(Vec<u8>),
    // We are currently writing the contents of the `Vec<u8>` up to and
    // including the new line.
    Writing(Take<Cursor<Vec<u8>>>),
    // The socket is closed.
    Closed,
}

impl State {
    fn mut_read_buf(&mut self) -> &mut Vec<u8> {
        match *self {
            State::Reading(ref mut buf) => buf,
            _ => panic!("connection not in reading state"),
        }
    }

    fn read_buf(&self) -> &[u8] {
        match *self {
            State::Reading(ref buf) => buf,
            _ => panic!("connection not in reading state"),
        }
    }

    fn write_buf(&self) -> &Take<Cursor<Vec<u8>>> {
        match *self {
            State::Writing(ref buf) => buf,
            _ => panic!("connection not in writing state"),
        }
    }

    fn mut_write_buf(&mut self) -> &mut Take<Cursor<Vec<u8>>> {
        match *self {
            State::Writing(ref mut buf) => buf,
            _ => panic!("connection not in writing state"),
        }
    }

    // Looks for a new line, if there is one the state is transitioned to
    // writing
    fn try_transition_to_writing(&mut self) {
        if let Some(pos) = self.read_buf().iter().position(|b| *b == b'\n') {
            self.transition_to_writing(pos + 1);
        }
    }

    fn transition_to_writing(&mut self, pos: usize) {
        // First, consume the current read buffer, replacing it with an
        // empty Vec<u8>.
        let msg = mem::replace(self, State::Closed)
            .unwrap_read_buf();

        let mut rsp = "ERROR\n".to_string().into_bytes();

        if msg == "PING\r\n".to_string().into_bytes() {
            rsp = "PONG\n".to_string().into_bytes();
        }

        let len = rsp.len();
        let buf = Cursor::new(rsp);

        // Transition the state to `Writing`, limiting the buffer to the
        // length of the response.
        *self = State::Writing(Take::new(buf, len));
    }

    // If the buffer being written back to the client has been consumed, switch
    // back to the reading state. However, there already might be another line
    // in the read buffer, so `try_transition_to_writing` is called as a final
    // step.
    fn try_transition_to_reading(&mut self) {
        if !self.write_buf().has_remaining() {
            let cursor = mem::replace(self, State::Closed)
                .unwrap_write_buf()
                .into_inner();

            let pos = cursor.position();
            let mut buf = cursor.into_inner();

            // Drop all data that has been written to the client
            drain_to(&mut buf, pos as usize);

            *self = State::Reading(buf);

            // Check for any new lines that have already been read.
            self.try_transition_to_writing();
        }
    }

    // Maps the current client state to the mio `EventSet` that will provide us
    // with the notifications that we want. When we are currently reading from
    // the client, we want `readable` socket notifications. When we are writing
    // to the client, we want `writable` notifications.
    fn event_set(&self) -> mio::EventSet {
        match *self {
            State::Reading(..) => mio::EventSet::readable(),
            State::Writing(..) => mio::EventSet::writable(),
            _ => mio::EventSet::none(),
        }
    }

    fn unwrap_read_buf(self) -> Vec<u8> {
        match self {
            State::Reading(buf) => buf,
            _ => panic!("connection not in reading state"),
        }
    }

    fn unwrap_write_buf(self) -> Take<Cursor<Vec<u8>>> {
        match self {
            State::Writing(buf) => buf,
            _ => panic!("connection not in writing state"),
        }
    }
}

struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &LogMetadata) -> bool {
        metadata.level() <= LogLevel::Debug
    }

    fn log(&self, record: &LogRecord) {
        if self.enabled(record.metadata()) {
            if record.location().module_path() == "rustping" {
                println!(
                    "{} {:<5} [{}] {}",
                    time::strftime("%Y-%m-%d %H:%M:%S", &time::now()).unwrap(),
                    record.level().to_string(),
                    record.location().module_path(),
                    record.args());
            }
        }
    }
}

pub fn start(address: SocketAddr) {
    // Create a new non-blocking socket bound to the given address. All sockets
    // created by mio are set to non-blocking mode.
    let server = TcpListener::bind(&address).unwrap();

    // Create a new `EventLoop`.
    let mut event_loop = mio::EventLoop::new().unwrap();

    // Register the server socket with the event loop.
    event_loop.register(&server, SERVER).unwrap();

    // Create a new `Server` instance that will track the state of the server.
    let mut srv = Server::new(server);

    // Run the server
    info!("listening on {}", address);
    event_loop.run(&mut srv).unwrap();
}

pub fn main() {
    let _ = log::set_logger(|max_log_level| {
        max_log_level.set(LogLevelFilter::Debug);
        return Box::new(SimpleLogger);
    });

    info!("rustping {} initializing...",VERSION);

    start("0.0.0.0:6567".parse().unwrap());
}

fn drain_to(vec: &mut Vec<u8>, count: usize) {
    // A very inefficient implementation. A better implementation could be
    // built using `Vec::drain()`, but the API is currently unstable.
    for _ in 0..count {
        vec.remove(0);
    }
}
