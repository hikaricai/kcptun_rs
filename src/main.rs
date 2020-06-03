#![allow(dead_code)]
use yamux::{Config,Connection,Mode,StreamHandle};
use tokio::prelude::*;
use tokio::net::{TcpListener,TcpStream};
use tokio::io;
use tokio::runtime::Runtime;
use tokio::codec::{Framed};
use tokio_codec::BytesCodec;

use futures::sync::{mpsc,oneshot};
use std::net::SocketAddr;
use bytes::BytesMut;
use log::{error, info};
use ::kcptun::kcp::{KcpStream,KcpListener};
use tokio_io::AsyncRead;

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
    let mut rt = Runtime::new().unwrap();

    let addr = "127.0.0.1:12345".parse().unwrap();

    let listener = KcpListener::bind(&addr).unwrap();
    let server = listener
        .incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(move|kcp_server_stream| {
            info!("new tcp");
            let fut = Connection::new(kcp_server_stream,Config::default(),Mode::Server)
                .for_each(move|mux_stream|{
                    let (mux_rd,mux_wr) = mux_stream.split();
                    let addr = "127.0.0.1:22".parse::<SocketAddr>().unwrap();
                    let fut = TcpStream::connect(&addr).map_err(|_|{}).and_then(move|tcp_stream|{
                        info!("tcp connected");
                        let (tcp_rd,tcp_wr) = tcp_stream.split();
                        let cp1 = io::copy(tcp_rd,mux_wr);
                        let cp2 = io::copy(mux_rd,tcp_wr);
                        cp1.join(cp2).map_err(|err|{error!("err copy {}",err)})//can not use cp2.join(cp1),but why?
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
    rt.block_on(server).unwrap();
}

type OpenMuxStreamMess = oneshot::Sender<StreamHandle<KcpStream>>;

fn run_client(){
    let mut rt = Runtime::new().unwrap();
    let (tx,rx) = mpsc::channel::<OpenMuxStreamMess>(16);

    let addr = "127.0.0.1:2022".parse().unwrap();
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
                    let cp1 = io::copy(tcp_rd,mux_wr);
                    let cp2 = io::copy(mux_rd,tcp_wr);
                    cp1.join(cp2).map_err(|err|{error!("err copy {}",err)})//can not use cp2.join(cp1),but why?
                })
            }).map(|_|{});
            tokio::spawn(fut);
            Ok(())
        });

    let addr = "127.0.0.1:12345".parse().unwrap();
    let mux_client = KcpStream::connect(&addr)
        .map_err(|_e|{})
        .and_then(move|kcp_stream|{
        info!("mux connected");
        let mux_conn = Connection::new(kcp_stream, Config::default(), Mode::Client);
        rx.for_each(move|mess|{
            let stream = mux_conn.open_stream().expect("ok stream").expect("not eof");
            let _ = mess.send(stream);
            info!("open mux stream");
            Ok(())
        }).map_err(|_|{})
    });
    rt.block_on(tcp_server.join(mux_client)).unwrap();
}





fn echo_server(){
    let mut rt = Runtime::new().unwrap();
    let addr = "127.0.0.1:12345".parse().unwrap();
    let incoming = KcpListener::bind(&addr).unwrap().incoming();
    let fut = incoming.for_each(|stream|{
        let (r,w) = stream.split();
        let cp = tokio_io::io::copy(r,w).map(|size|{
            info!("copied {} bytes",size.0);
        }).map_err(|e|{
            error!("copy err {}",e);
        });
        tokio::spawn(cp);
        Ok(())
    });
    rt.block_on(fut).unwrap();
}

fn echo_client(){
    let mut stdout = std::io::stdout();
    let (stdin_tx, stdin_rx) = mpsc::channel(0);
    std::thread::spawn(|| read_stdin(stdin_tx));

    let stdin_rx = stdin_rx.map(|v|BytesMut::from(v)).map_err(|_| panic!()); // errors not possible on rx

    let mut rt = Runtime::new().unwrap();
    let addr = "127.0.0.1:12345".parse().unwrap();
    let stream_wrapper = KcpStream::connect(&addr);
    let fut = stream_wrapper.map_err(|_|{})
        .and_then(move|stream|{
            let (sink, stream) =  Framed::new(stream, BytesCodec::new()).split();
            let send_stdin = stdin_rx.map(BytesMut::freeze).forward(sink);
            let write_stdout = stream.for_each(move |buf| stdout.write_all(&buf));
            let client= send_stdin
                .map(|_| ())
                .select(write_stdout.map(|_| ()));
            client.map_err(|_|{})
        });

    let _ = rt.block_on(fut).unwrap();

}



// Our helper method which will read data from stdin and send it along the
// sender provided.
fn read_stdin(mut tx: mpsc::Sender<Vec<u8>>) {
    let mut stdin = std::io::stdin();
    loop {
        let mut buf = vec![0; 1024];
        let n = match stdin.read(&mut buf) {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };
        buf.truncate(n);
        tx = tx.send(buf).wait().unwrap();
    }
}