use yamux::{Config,Connection,Mode,ConnectionError,StreamHandle};
use tokio::prelude::*;
use tokio::net::{TcpListener,TcpStream};
use tokio::io as tio;
use tokio::runtime::Runtime;
use tokio::codec::{Framed};
use tokio_codec::BytesCodec;
use tokio_core::reactor::Core;
use futures::sync::{mpsc,oneshot};
use std::net::SocketAddr;
use bytes::{Bytes, BytesMut};
use log::{error, info};
use tokio_kcp::{KcpSessionManager, KcpStream, KcpConfig, KcpNoDelayConfig, KcpListener};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TestMode {
    Default,
    Normal,
    Fast,
}

fn get_config(mode: TestMode) -> KcpConfig {
    let mut config = KcpConfig::default();
    config.wnd_size = Some((128, 128));
    match mode {
        TestMode::Default => {
            config.nodelay = Some(KcpNoDelayConfig {
                nodelay: false,
                interval: 10,
                resend: 0,
                nc: false,
            });
        }
        TestMode::Normal => {
            config.nodelay = Some(KcpNoDelayConfig {
                nodelay: false,
                interval: 10,
                resend: 0,
                nc: true,
            });
        }
        TestMode::Fast => {
            config.nodelay = Some(KcpNoDelayConfig {
                nodelay: true,
                interval: 10,
                resend: 2,
                nc: true,
            });

            config.rx_minrto = Some(10);
            config.fast_resend = Some(1);
        }
    }

    config
}

fn main() {
    env_logger::init();
    if std::env::args().nth(1) == Some("server".to_string()) {
        info!("Starting server ......");
        run_server();
    } else {
        info!("Starting client ......");
        run_client();
    }
}

fn run_server(){
    let mut rt = Core::new().unwrap();
    let handle = rt.handle();

    let addr = "127.0.0.1:12345".parse().unwrap();
    let mode = std::env::args().nth(2).unwrap_or_else(|| "default".to_string());
    let mode = match &mode[..] {
        "default" => TestMode::Default,
        "normal" => TestMode::Normal,
        "fast" => TestMode::Fast,
        _ => panic!("Unrecognized mode {}", mode),
    };

    let config = get_config(mode);
    let listener = KcpListener::bind_with_config(&addr, &handle, config).unwrap();

    let server = listener
        .incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(move|(sock,addr)| {
            info!("new tcp");
            let handle_clone = handle.clone();
            let fut = Connection::new(sock,Config::default(),Mode::Server)
                .for_each(move|mux_stream|{
                    let (mux_rd,mux_wr) = mux_stream.split();
                    let addr = "127.0.0.1:22".parse::<SocketAddr>().unwrap();
                    let fut = TcpStream::connect(&addr).map_err(|_|{}).and_then(move|tcp_stream|{
                        info!("tcp connected");
                        let (tcp_rd,tcp_wr) = tcp_stream.split();
                        let cp1 = tio::copy(tcp_rd,mux_wr);
                        let cp2 = tio::copy(mux_rd,tcp_wr);
                        cp1.join(cp2).map_err(|err|{error!("err copy {}",err)})//can not change the order,but why?
                    }).map(|_|{});
                    handle_clone.spawn(fut);
                    Ok(())
                })
                .map_err(|err|{
                    error!("tcp server stream error: {:?}", err);
                    ()
                });
            handle.spawn(fut);
            Ok(())
        });
    rt.run(server);
}

type OpenMuxStreamMess = oneshot::Sender<StreamHandle<KcpStream>>;

fn run_client(){
    let mut rt = Core::new().unwrap();
    let handle = rt.handle();
    let handle_clone = handle.clone();
    let (tx,rx) = mpsc::channel::<OpenMuxStreamMess>(16);

    let addr = "127.0.0.1:10022".parse().unwrap();
    let listener = TcpListener::bind(&addr).expect("unable to bind TCP listener");

    let tcp_server = listener
        .incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(move|tcp_stream| {
            info!("new tcp");
            let (tcp_rd,tcp_wr) = tcp_stream.split();
            let (mess_tx,mess_rx) = oneshot::channel();
            let fut = tx.clone().send(mess_tx).map_err(|_|{}).and_then(|_|{
                mess_rx.map_err(|_|{}).and_then(move|mux_stream|{
                    info!("get mux stream");
                    let (mux_rd,mux_wr) = mux_stream.split();
                    let cp1 = tio::copy(tcp_rd,mux_wr);
                    let cp2 = tio::copy(mux_rd,tcp_wr);
                    cp1.join(cp2).map_err(|err|{error!("err copy {}",err)})//can not use cp2.join(cp1),but why?
                })
            }).map(|_|{});
            handle_clone.spawn(fut);
            Ok(())
        });

    let mut updater = KcpSessionManager::new(&handle).unwrap();
    let addr = "127.0.0.1:12345".parse().unwrap();
    let mux_client = futures::lazy(||{KcpStream::connect(0, &addr, &handle, &mut updater)})
        .map_err(|e|{})
        .and_then(move|kcp_stream|{
        info!("mux connected");
        let mux_conn = Connection::new(kcp_stream, Config::default(), Mode::Client);
        rx.for_each(move|mess|{
            let stream = mux_conn.open_stream().expect("ok stream").expect("not eof");
            mess.send(stream);
            info!("open mux stream");
            Ok(())
        }).map_err(|_|{})
    });
    rt.run(tcp_server.join(mux_client));
}