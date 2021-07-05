use std::net::{ToSocketAddrs, SocketAddr};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex};

use super::protocol::MessageReader;

pub struct Settings<F, F1, F2, S>
where
    F: Fn(&mut SocketHandle, &mut S, Vec<u8>) + Send + 'static,
    F1: Fn(&mut SocketHandle, &S) + Send + 'static,
    F2: Fn(&S) + Send + 'static,
    S: Default + Send,
{
    pub on_message: F,
    pub on_timer: F1,
    pub on_disconnect: F2,
    pub timer: Option<Duration>,

    pub _marker: std::marker::PhantomData<S>,
}

enum Sender<'a> {
    WebSocket((&'a ws::Sender, SocketAddr)),
    Tcp(&'a mut TcpStream),
}

pub struct SocketHandle<'a> {
    sender: Sender<'a>,
    disconnect: bool,
}

impl<'a> Sender<'a> {
    fn send(&mut self, data: &[u8]) -> Option<()> {
        use std::io::Write;

        match self {
            Sender::WebSocket((out, _)) => {
                out.send(data).ok()?;
            }
            Sender::Tcp(stream) => {
                stream.write(&[data.len() as u8]).ok()?;
                stream.write(data).ok()?;
            }
        }

        Some(())
    }

    fn address(&self) -> SocketAddr {
        match self {
            Sender::WebSocket((_, addr)) => {
                *addr
            }
            Sender::Tcp(stream) => {
                stream.peer_addr().expect("tcp socket with no address, wtf")
            }
        }
    }
}

impl<'a> SocketHandle<'a> {
    fn new(sender: Sender<'a>) -> SocketHandle<'a> {
        SocketHandle {
            sender,
            disconnect: false,
        }
    }

    pub fn send(&mut self, data: &[u8]) -> Result<(), ()> {
        self.sender.send(data).ok_or(())
    }

    #[cfg(feature = "nanoserde")]
    pub fn send_bin<T: nanoserde::SerBin>(&mut self, data: &T) -> Result<(), ()> {
        self.send(&nanoserde::SerBin::serialize_bin(data))
    }

    pub fn disconnect(&mut self) {
        self.disconnect = true;
    }

    pub fn address(&self) -> SocketAddr { self.sender.address() }
}

pub fn listen<A, A1, F, F1, F2, S>(tcp_addr: A, ws_addr: A1, settings: Settings<F, F1, F2, S>)
where
    A: ToSocketAddrs + std::fmt::Debug + Send,
    A1: ToSocketAddrs + std::fmt::Debug + Send + 'static,
    F: Fn(&mut SocketHandle, &mut S, Vec<u8>) + Send + 'static,
    F1: Fn(&mut SocketHandle, &S) + Send + 'static,
    F2: Fn(&S) + Send + 'static,
    S: Default + Send + 'static,
{
    let on_message = Arc::new(Mutex::new(settings.on_message));
    let on_timer = Arc::new(Mutex::new(settings.on_timer));
    let on_disconnect = Arc::new(Mutex::new(settings.on_disconnect));
    let timer = settings.timer;

    struct WsHandler<
        S: Default,
        F: Fn(&mut SocketHandle, &mut S, Vec<u8>) + Send + 'static,
        F1: Fn(&mut SocketHandle, &S) + Send + 'static,
        F2: Fn(&S) + Send + 'static,
    > {
        out: ws::Sender,
        state: (S, SocketAddr),
        on_message: Arc<Mutex<F>>,
        on_timer: Arc<Mutex<F1>>,
        on_disconnect: Arc<Mutex<F2>>,
        timeout: Option<Duration>,
    }

    impl<
            S: Default,
            F: Fn(&mut SocketHandle, &mut S, Vec<u8>) + Send + 'static,
            F1: Fn(&mut SocketHandle, &S) + Send + 'static,
            F2: Fn(&S) + Send + 'static,
        > ws::Handler for WsHandler<S, F, F1, F2>
    {
        fn on_open(&mut self, handshake: ws::Handshake) -> ws::Result<()> {
            self.state.1 = handshake.peer_addr.expect("no peer address in the handshake");

            if let Some(timeout) = self.timeout {
                self.out
                    .timeout(timeout.as_millis() as _, ws::util::Token(1))?;
            }
            Ok(())
        }

        fn on_message(&mut self, msg: ws::Message) -> ws::Result<()> {
            let data = msg.into_data();
            let mut handle = SocketHandle::new(Sender::WebSocket((&self.out, self.state.1)));
            (self.on_message.lock().unwrap())(&mut handle, &mut self.state.0, data);
            if handle.disconnect {
                self.out.close(ws::CloseCode::Normal)?;
            }
            Ok(())
        }

        fn on_close(&mut self, _code: ws::CloseCode, _reason: &str) {
            (self.on_disconnect.lock().unwrap())(&self.state.0);
        }

        fn on_timeout(&mut self, _: ws::util::Token) -> ws::Result<()> {
            if let Some(timeout) = self.timeout {
                let mut handle = SocketHandle::new(Sender::WebSocket((&self.out, self.state.1)));
                (self.on_timer.lock().unwrap())(&mut handle, &self.state.0);
                if handle.disconnect == false {
                    self.out
                        .timeout(timeout.as_millis() as _, ws::util::Token(1))?;
                } else {
                    self.out.close(ws::CloseCode::Normal)?;
                }
            }
            Ok(())
        }
    }

    std::thread::spawn({
        let on_message = on_message.clone();
        let on_timer = on_timer.clone();
        let on_disconnect = on_disconnect.clone();

        move || {
            ws::Builder::new()
                .with_settings(ws::Settings {
                    timer_tick_millis: 10,
                    tcp_nodelay: true,
                    ..ws::Settings::default()
                })
                .build(move |out| {
                    let on_message = on_message.clone();
                    let on_timer = on_timer.clone();
                    let on_disconnect = on_disconnect.clone();

                    WsHandler {
                        out,
                        state: (Default::default(), SocketAddr::from(([0, 0, 0, 0], 0))),
                        on_message,
                        on_timer,
                        on_disconnect,
                        timeout: timer,
                    }
                })
                .unwrap()
                .listen(ws_addr)
                .unwrap();
        }
    });

    let listener = TcpListener::bind(tcp_addr).unwrap();
    for stream in listener.incoming() {
        let on_message = on_message.clone();
        let on_timer = on_timer.clone();
        let on_disconnect = on_disconnect.clone();

        std::thread::spawn(move || {
            let mut stream = stream.unwrap();
            stream.set_nodelay(true).unwrap();
            stream.set_nonblocking(true).unwrap();
            let mut message_reader = MessageReader::new();
            let mut state = S::default();

            let mut time = Instant::now();
            loop {
                match message_reader.next(&mut stream) {
                    Ok(Some(message)) => {
                        let mut handle = SocketHandle::new(Sender::Tcp(&mut stream));
                        (on_message.lock().unwrap())(&mut handle, &mut state, message);
                        if handle.disconnect {
                            (on_disconnect.lock().unwrap())(&state);
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(_err) => {
                        (on_disconnect.lock().unwrap())(&state);
                        return;
                    }
                }

                if let Some(timer) = timer {
                    if time.elapsed() >= timer {
                        time = Instant::now();
                        let mut handle = SocketHandle::new(Sender::Tcp(&mut stream));

                        (on_timer.lock().unwrap())(&mut handle, &state);
                        if handle.disconnect {
                            (on_disconnect.lock().unwrap())(&state);
                            return;
                        }
                    }
                }
            }
        });
    }
}
