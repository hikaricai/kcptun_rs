use tokio::prelude::*;
use tokio::sync::mpsc::{self,channel,Sender,Receiver};
use tokio::timer::Interval;
use futures::{Stream, Poll, Async,task};
use udpsocket2::{UdpSocket,UdpDatagram};
use std::net::SocketAddr;
use std::collections::HashMap;
use std::io::{self, Read, Write, BufRead};
use std::cell::RefCell;
use std::rc::Rc;
use std::cmp;
use tokio_io::{AsyncRead, AsyncWrite};
use time;
use kcp::{Error as KcpError, Kcp, KcpResult, get_conv};
use std::sync::{Arc, Mutex};
use bytes::{Buf, BufMut, ByteOrder, LittleEndian};
use std::time::{Duration, Instant};

/// Base Io object for KCP
struct KcpIo {
    kcp: Kcp<KcpOutput>,
    read_buf: Vec<u8>,
    read_pos: usize,
    read_cap: usize,
}

impl KcpIo {
    pub fn new(kcp: Kcp<KcpOutput>) -> KcpIo {
        KcpIo {
            kcp: kcp,
            read_buf: Vec::new(),
            read_pos: 0,
            read_cap: 0,
        }
    }

    /// Call everytime you got data from transmission
    pub fn input(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.kcp.input(buf).map_err(From::from)
    }

    /// MTU
    pub fn mtu(&self) -> usize {
        self.kcp.mtu()
    }

    fn buf_remaining(&self) -> usize {
        self.read_cap - self.read_pos
    }
}

impl BufRead for KcpIo {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.read_pos >= self.read_cap {
            if self.read_buf.is_empty() {
                let mtu = self.kcp.mtu();
                //trace!("[INIT] KcpIo mtu {}", mtu);
                self.read_buf.resize(mtu, 0);
            }

            loop {
                let n = match self.kcp.recv(&mut self.read_buf) {
                    Ok(n) => n,
                    Err(KcpError::UserBufTooSmall) => {
                        let orig = self.read_buf.len();
                        let incr = self.kcp.peeksize().unwrap_or(0).next_power_of_two();
                        //trace!("[RECV] kcp.recv buf too small, {} -> {}", orig, incr);
                        self.read_buf.resize(incr, 0);
                        continue;
                    }
                    Err(err) => {
                        return Err(From::from(err));
                    }
                };

                self.read_pos = 0;
                self.read_cap = n;

                //trace!("[RECV] kcp.recv size={} {:?}", n, ::debug::BsDebug(&self.read_buf[..n]));

                break;
            }
        }

        Ok(&self.read_buf[self.read_pos..self.read_cap])
    }

    fn consume(&mut self, amt: usize) {
        self.read_pos = cmp::min(self.read_cap, self.read_pos + amt);
    }
}

impl Read for KcpIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.buf_remaining() == 0 {
            // Try to read directly
            match self.kcp.recv(buf) {
                Ok(n) => {
                    //trace!("[RECV] KcpIo.read directly size={}", n);
                    return Ok(n);
                }
                Err(KcpError::UserBufTooSmall) => {
//                    trace!("[RECV] KcpIo.read directly peeksize={} buf size={} too small",
//                           self.kcp.peeksize(),
//                           buf.len());
                }
                Err(err) => return Err(From::from(err)),
            }
        }

        let nread = {
            let mut available = self.fill_buf()?;
            available.read(buf)?
        };
        self.consume(nread);
        //trace!("[RECV] KcpIo.read size={}", nread);
        Ok(nread)
    }
}

impl Write for KcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        //trace!("[SEND] KcpIo.write size={} {:?}", buf.len(), ::debug::BsDebug(buf));
        let n = self.kcp.send(buf)?;
        //trace!("[SEND] kcp.send size={}", n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.kcp.flush()?;
        //trace!("[SEND] Flushed KCP buffer");
        Ok(())
    }
}

pub struct KcpListener {
    udp: UdpSocket,
    connections: HashMap<SocketAddr, Sender<UdpDatagram>>,
}

pub struct Incoming {
    inner: KcpListener,
    datagrams:udpsocket2::incoming::Incoming,
}

pub struct KcpOutput {
    udp:  UdpSocket,
    peer: SocketAddr,
}

impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let data = UdpDatagram{
            peer: self.peer.clone(),
            data: buf.to_vec()
        };
        let mut send_to = self.udp.send(data).map_err(|_|{});
        tokio::spawn(send_to);
        return Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl KcpListener{
    pub fn bind(addr: &SocketAddr) -> io::Result<KcpListener> {
        let udp = UdpSocket::bind(addr)?;
        let listener = KcpListener {
            udp,
            connections: HashMap::new(),
        };
        Ok(listener)
    }
    pub fn incoming(self) -> Incoming {
        let datagrams = self.udp.incoming();
        Incoming { inner: self, datagrams }
    }
}

impl Stream for Incoming {
    type Item = KcpServerStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop{
            let datagram:UdpDatagram= try_ready!{
                self.datagrams.poll()
            }.unwrap();
            let peer = datagram.peer.clone();
            println!("datagram size {}",datagram.data.len());
            if self.inner.connections.contains_key(&datagram.peer){
                if let Some( mut tx) = self.inner.connections.remove(&datagram.peer){
                    match tx.send(datagram).wait() {
                        Ok(s) => {
                            tx = s;
                            self.inner.connections.insert(peer,tx);
                        },
                        Err(_) => {}
                    }
                }
            }else{
                let conv = LittleEndian::read_u32(&datagram.data[..4]);
                let mut kcp = Kcp::new(
                    conv,
                    KcpOutput {
                        udp: self.inner.udp.clone(),
                        peer: datagram.peer.clone(),
                    },
                );
                kcp.set_mtu(1000);//the udpsocket2 only support 1024
                kcp.set_wndsize(1024,1024);
                kcp.set_nodelay(true,10,1,false);
                let kcp_io = KcpIo::new(kcp);
                let kcp = Arc::new(Mutex::new(kcp_io));
                let kcp_clone = kcp.clone();
                let (mut tx,rx) = mpsc::channel(10240);
                match tx.send(datagram).wait() {
                    Ok(s) => tx = s,
                    Err(_) => return Ok(Async::NotReady)
                };
                self.inner.connections.insert(peer,tx);
                let tcp_flush_fut = Interval::new_interval(Duration::from_millis(10))
                    .for_each(move|_|{
                        let mut kcp_io = kcp_clone.lock().unwrap();
                        kcp_io.kcp.update(clock());
                        let dur = kcp_io.kcp.check(clock());
                        kcp_io.kcp.flush();
                        Ok(())
                    }).map_err(|e_|{});
                tokio::spawn(tcp_flush_fut);
                return Ok(Async::Ready(Some(
                    KcpServerStream{
                        rx,
                        kcp,
                    }
                )))
            }
        }
    }
}

pub struct KcpServerStream {
    rx:Receiver<UdpDatagram>,
    kcp:Arc<Mutex<KcpIo>>,
}
fn delay_fut(){
    let curr = task::current();
    let delay = Duration::from_millis(rand::random::<u64>() % 10);
    let wakeup = Instant::now() + delay;
    let task = tokio::timer::Delay::new(wakeup)
        .map_err(|e| panic!("timer failed; err={:?}", e))
        .and_then(move |_| {
            curr.notify();
            Ok(())
        });
    tokio::spawn(task);
}
impl Read for KcpServerStream{
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        loop {
            let ret = self.rx.poll();
            println!("read {:?}",ret);
            match ret {
                Ok(Async::Ready(Some(datagram)))=> {
                    let mut kcp_io = self.kcp.lock().unwrap();
                    kcp_io.input(datagram.data.as_slice());
                    let peek = kcp_io.kcp.peeksize();
                    let size = kcp_io.read(buf);
                    println!("kcp peeksize {:?}",peek);
                    println!("kcp read {:?}",size);
                    if size.is_ok(){
                        let size = size?;
                        return Ok(size);
                    }
                },
                _ => {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
            }
        }
    }
}

impl Write for KcpServerStream{
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let mut kcp_io = self.kcp.lock().unwrap();
        let size = kcp_io.write(buf);
        println!("write {:?}",size);
        let size = size?;
        kcp_io.kcp.update(clock());
        kcp_io.kcp.flush();
        Ok(size)
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

impl AsyncRead for KcpServerStream{

}

impl AsyncWrite for KcpServerStream{

    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

pub struct KcpStreamWrapper {
    inner: Option<KcpStream>,
}

impl Future for KcpStreamWrapper {
    type Item = KcpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<KcpStream, io::Error> {
        let kcp_stream = self.inner.take().unwrap();
        let kcp_io =kcp_stream.kcp_io.clone();
        let tcp_flush_fut = Interval::new_interval(Duration::from_millis(10))
            .for_each(move|_|{
                let mut kcp_io = kcp_io.lock().unwrap();
                kcp_io.kcp.update(clock());
                let dur = kcp_io.kcp.check(clock());
                kcp_io.kcp.flush();
                Ok(())
            }).map_err(|e_|{});
        tokio::spawn(tcp_flush_fut);
        Ok(Async::Ready(kcp_stream))
    }
}

pub struct KcpStream {
    kcp_io:Arc<Mutex<KcpIo>>,
    datagrams:udpsocket2::incoming::Incoming,
}

impl KcpStream{
    pub fn connect(addr: &SocketAddr)->KcpStreamWrapper{
        let r: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let udp = UdpSocket::bind(&r).unwrap();
        let datagrams = udp.incoming();
        let mut kcp = Kcp::new(
            0,
            KcpOutput {
                udp: udp,
                peer: addr.clone(),
            },
        );
        kcp.set_mtu(1000);
        kcp.set_wndsize(1024,1024);
        kcp.set_nodelay(true,10,1,false);
        let kcp_io = KcpIo::new(kcp);
        let kcp_stream = Self{
            kcp_io:Arc::new(Mutex::new(kcp_io)),
            datagrams,
        };
        KcpStreamWrapper{
            inner: Some(kcp_stream)
        }
    }
}

impl Read for KcpStream{
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        loop{
            let ret = self.datagrams.poll();//must run in tokio runtime?
            println!("read {:?}",ret);
            match ret {
                Ok(Async::Ready(Some(datagram)))=> {
                    println!("datagram size {}",datagram.data.len());
                    let mut kcp_io = self.kcp_io.lock().unwrap();
                    kcp_io.input(datagram.data.as_slice());
                    let size = kcp_io.read(buf);
                    println!("kcp read {:?}",size);
                    if size.is_ok(){
                        let size = size?;
                        return Ok(size)
                    }
                },
                _=>{
                    return Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
            }
        }
    }
}

impl Write for KcpStream{
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let mut kcp_io = self.kcp_io.lock().unwrap();
        let size = kcp_io.write(buf);
        println!("write {:?}",size);
        let size = size?;
        kcp_io.kcp.update(clock());
        kcp_io.kcp.flush();
        Ok(size)
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

impl AsyncRead for KcpStream{

}

impl AsyncWrite for KcpStream{
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

#[inline]
fn clock() -> u32 {
    let timespec = time::get_time();
    let mills = timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000;
    mills as u32
}