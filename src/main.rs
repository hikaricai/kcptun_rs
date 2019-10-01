use yamux::{Config,Connection,Mode,ConnectionError,StreamHandle};
use tokio::prelude::*;
use tokio::net::{TcpListener,TcpStream};
use tokio::io as tio;
use tokio::runtime::Runtime;
use tokio::codec::{Framed};
use tokio_codec::BytesCodec;
use futures::sync::{mpsc,oneshot};
use std::net::SocketAddr;
use bytes::{Bytes, BytesMut};
use log::{error, info};
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
    let mut rt = Runtime::new().expect("runtime");
    let e1 = rt.executor();

    let addr = "127.0.0.1:12345".parse().unwrap();
    let listener = TcpListener::bind(&addr).expect("unable to bind TCP listener");

    let server = listener
        .incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(move|sock| {
            info!("new tcp");
            let fut = Connection::new(sock,Config::default(),Mode::Server)
                .for_each(|mux_stream|{
                    let (mux_sink,mux_stream) = Framed::new(mux_stream, BytesCodec::new()).split();
                    let addr = "127.0.0.1:22".parse::<SocketAddr>().unwrap();
                    let fut = TcpStream::connect(&addr).map_err(|_|{}).and_then(move|tcp_stream|{
                        info!("tcp connected");
                        let (tcp_sink,tcp_stream) = Framed::new(tcp_stream, BytesCodec::new()).split();
                        let cp1 = mux_stream.map(BytesMut::freeze).forward(tcp_sink);
                        let cp2 = tcp_stream.map(BytesMut::freeze).forward(mux_sink);
                        cp1.join(cp2).map_err(|_|{})
                    }).map(|_|{});
                    tokio::spawn(fut);
                    Ok(())
                })
                .map_err(|err|{
                    error!("tcp server stream error: {:?}", err);
                    ()
                });
            tokio::spawn(fut);
            Ok(())
        });
    rt.block_on(server);
}

type OpenMuxStreamMess = oneshot::Sender<StreamHandle<TcpStream>>;

fn run_client(){
    let mut rt = Runtime::new().expect("runtime");
    let e1 = rt.executor();

    let (tx,rx) = mpsc::channel::<OpenMuxStreamMess>(16);

    let addr = "127.0.0.1:10022".parse().unwrap();
    let listener = TcpListener::bind(&addr).expect("unable to bind TCP listener");

    let tcp_server = listener
        .incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(move|tcp_stream| {
            info!("new tcp");
            let (tcp_sink,tcp_stream) = Framed::new(tcp_stream, BytesCodec::new()).split();
            let (mess_tx,mess_rx) = oneshot::channel();
            let fut = tx.clone().send(mess_tx).map_err(|_|{}).and_then(|_|{
                mess_rx.map_err(|_|{}).and_then(move|mux_stream|{
                    info!("get mux stream");
                    let (mux_sink,mux_stream) = Framed::new(mux_stream, BytesCodec::new()).split();
                    let cp1 = mux_stream.map(BytesMut::freeze).forward(tcp_sink);
                    let cp2 = tcp_stream.map(BytesMut::freeze).forward(mux_sink);
                    cp1.join(cp2).map_err(|e|{})
                })
            }).map(|_|{});
            tokio::spawn(fut);
            Ok(())
        });

    let addr = "127.0.0.1:12345".parse().unwrap();
    let mux_client = TcpStream::connect(&addr).map_err(|e|{}).and_then(move|tcp_stream|{
        info!("mux connected");
        let mux_conn = Connection::new(tcp_stream, Config::default(), Mode::Client);
        rx.for_each(move|mess|{
            let stream = mux_conn.open_stream().expect("ok stream").expect("not eof");
            mess.send(stream);
            info!("open mux stream");
            Ok(())
        }).map_err(|_|{})
    });
    rt.block_on_all(tcp_server.join(mux_client));
}