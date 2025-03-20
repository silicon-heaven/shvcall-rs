use std::future::Future;
use async_std::io::BufReader;
use async_std::net::{TcpListener, TcpStream};
use async_std::os::unix::net::UnixStream;
use async_std::{io, task};
use clap::Parser;
use futures::io::{BufWriter, WriteHalf};
use futures::{AsyncWriteExt};
use futures::{select, AsyncReadExt};
use futures::{FutureExt, StreamExt};
use futures_time::future::FutureExt as ff;
use futures_time::time::Duration;
use log::*;
use shvrpc::client::LoginParams;
use shvrpc::framerw::{FrameReader, FrameWriter};
use shvrpc::rpcframe::RpcFrame;
use shvrpc::rpcmessage::{RqId, SeqNo};
use shvrpc::serialrw::{SerialFrameReader, SerialFrameWriter};
use shvrpc::streamrw::{StreamFrameReader, StreamFrameWriter};
use shvrpc::util::login_from_url;
use shvrpc::{client, RpcMessage, RpcMessageMetaTags};
use url::Url;
use async_channel::{Sender, Receiver};
use async_std::future::{timeout};

#[cfg(feature = "readline")]
use crossterm::tty::IsTty;
use futures::stream::FuturesUnordered;
#[cfg(feature = "readline")]
use rustyline_async::ReadlineEvent;
use shvproto::{Map, RpcValue};
use shvrpc::rpc::ShvRI;
#[cfg(feature = "readline")]
use std::io::Write;

pub type Result = shvrpc::Result<()>;

#[derive(Parser, Debug)]
//#[structopt(name = "shvcall", version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = "SHV call")]
pub struct Opts {
    ///Url to connect to, example tcp://admin@localhost:3755?password=dj4j5HHb, localsocket:path/to/socket
    #[arg(short = 's', long = "url")]
    pub url: Url,
    /// Method is specified together with path like: shv/path:method
    #[arg(short, long)]
    pub method: Option<String>,
    #[arg(short, long)]
    pub param: Option<String>,
    /// Timeout in milliseconds, value 0 means wait forever.
    #[arg(short, long, default_value = "5000")]
    pub timeout: u64,
    /// Output format: [ cpon | icpon | chainpack | simple | value | "Placeholders {PATH} {METHOD} {VALUE} in any number and combination in custom string." ]
    #[arg(short = 'o', long = "output-format", default_value = "cpon")]
    pub output_format: String,
    /// Create TCP tunnel, SSH like syntax, example: -L 2222:some.host.org:22
    #[arg(short = 'L', long)]
    pub tunnel: Option<String>,
    /// Send N request in M threads, format is N[,M], default M == 1
    #[arg(long)]
    pub burst: Option<String>,
    /// Verbose mode (module, .)
    #[arg(short, long)]
    pub verbose: Option<String>,
    #[arg(long)]
    pub version: bool,
}

enum OutputFormat {
    Cpon,
    CponIndented,
    ChainPack,
    Simple,
    Value,
    Custom(String),
}

impl From<&str> for OutputFormat {
    fn from(value: &str) -> Self {
        match value {
            "chainpack" => Self::ChainPack,
            "simple" => Self::Simple,
            "value" => Self::Value,
            "cpon" => Self::Cpon,
            "icpon" => Self::CponIndented,
            _ => Self::Custom(value.to_string()),
        }
    }
}
type BoxedFrameReader = Box<dyn FrameReader + Unpin + Send>;
type BoxedFrameWriter = Box<dyn FrameWriter + Unpin + Send>;

pub fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
where
    F: Future<Output = std::result::Result<(), String>> + Send + 'static,
{
    task::spawn(async move {
        if let Err(e) = fut.await {
            error!("{}", e)
        }
    })
}

fn is_tty() -> bool {
    #[cfg(feature = "readline")]
    return io::stdin().is_tty();
    #[cfg(not(feature = "readline"))]
    return false;
}
async fn login(url: &Url) -> shvrpc::Result<(BoxedFrameReader, BoxedFrameWriter)> {
    // Establish a connection
    let mut reset_session = false;
    let (mut frame_reader, mut frame_writer) = match url.scheme() {
        "tcp" => {
            let address = format!(
                "{}:{}",
                url.host_str().unwrap_or("localhost"),
                url.port().unwrap_or(3755)
            );
            let stream = TcpStream::connect(&address).await?;
            let (reader, writer) = stream.split();
            let brd = BufReader::new(reader);
            let bwr = BufWriter::new(writer);
            let frame_reader: BoxedFrameReader = Box::new(StreamFrameReader::new(brd));
            let frame_writer: BoxedFrameWriter = Box::new(StreamFrameWriter::new(bwr));
            (frame_reader, frame_writer)
        }
        "unix" => {
            let stream = UnixStream::connect(url.path()).await?;
            let (reader, writer) = stream.split();
            let brd = BufReader::new(reader);
            let bwr = BufWriter::new(writer);
            let frame_reader: BoxedFrameReader = Box::new(StreamFrameReader::new(brd));
            let frame_writer: BoxedFrameWriter = Box::new(StreamFrameWriter::new(bwr));
            (frame_reader, frame_writer)
        }
        "unixs" => {
            let stream = UnixStream::connect(url.path()).await?;
            let (reader, writer) = stream.split();
            let brd = BufReader::new(reader);
            let bwr = BufWriter::new(writer);
            let frame_reader: BoxedFrameReader =
                Box::new(SerialFrameReader::new(brd).with_crc_check(false));
            let frame_writer: BoxedFrameWriter =
                Box::new(SerialFrameWriter::new(bwr).with_crc_check(false));
            reset_session = true;
            (frame_reader, frame_writer)
        }
        s => {
            panic!("Scheme {s} is not supported")
        }
    };

    // login
    let (user, password) = login_from_url(url);
    let login_params = LoginParams {
        user,
        password,
        reset_session,
        ..Default::default()
    };
    //let frame = frame_reader.receive_frame().await?;
    //frame_writer.send_frame(frame.expect("frame")).await?;
    client::login(&mut *frame_reader, &mut *frame_writer, &login_params).await?;
    debug!("Connected to broker.");
    Ok((frame_reader, frame_writer))
}
async fn send_request(
    frame_writer: &mut (dyn FrameWriter + Send),
    path: &str,
    method: &str,
    param: &str,
) -> shvrpc::Result<RqId> {
    let param = if param.is_empty() {
        None
    } else {
        Some(RpcValue::from_cpon(param)?)
    };
    frame_writer.send_request(path, method, param).await
}
async fn make_call(
    mut frame_reader: BoxedFrameReader,
    mut frame_writer: BoxedFrameWriter,
    opts: &Opts,
) -> Result {
    async fn print_resp(
        stdout: &mut io::Stdout,
        resp: &RpcMessage,
        output_format: OutputFormat,
    ) -> Result {
        let bytes = match output_format {
            OutputFormat::Cpon => {
                let mut s = resp.as_rpcvalue().to_cpon();
                s.push('\n');
                s.as_bytes().to_owned()
            }
            OutputFormat::CponIndented => {
                let mut s = resp.as_rpcvalue().to_cpon_indented("\t");
                s.push('\n');
                s.as_bytes().to_owned()
            }
            OutputFormat::ChainPack => resp.as_rpcvalue().to_chainpack().to_owned(),
            OutputFormat::Simple => {
                let s = if resp.is_request() {
                    format!(
                        "REQ {}:{} {}\n",
                        resp.shv_path().unwrap_or_default(),
                        resp.method().unwrap_or_default(),
                        resp.param().unwrap_or_default().to_cpon()
                    )
                } else if resp.is_response() {
                    match resp.result() {
                        Ok(res) => {
                            format!("RES {}\n", res.to_cpon())
                        }
                        Err(err) => {
                            format!("ERR {}\n", err)
                        }
                    }
                } else {
                    format!(
                        "SIG {}:{} {}\n",
                        resp.shv_path().unwrap_or_default(),
                        resp.method().unwrap_or_default(),
                        resp.param().unwrap_or_default().to_cpon()
                    )
                };
                s.as_bytes().to_owned()
            }
            OutputFormat::Value => {
                let mut s = if resp.is_request() {
                    resp.param().unwrap_or_default().to_cpon()
                } else if resp.is_response() {
                    match resp.result() {
                        Ok(res) => res.to_cpon(),
                        Err(err) => err.to_string(),
                    }
                } else {
                    resp.param().unwrap_or_default().to_cpon()
                };
                s.push('\n');
                s.as_bytes().to_owned()
            }
            OutputFormat::Custom(fmtstr) => {
                const PATH: &str = "{PATH}";
                const METHOD: &str = "{METHOD}";
                const VALUE: &str = "{VALUE}";
                let fmtstr = fmtstr.replace(PATH, resp.shv_path().unwrap_or_default());
                let fmtstr = fmtstr.replace(METHOD, resp.method().unwrap_or_default());
                let fmtstr = fmtstr.replace(VALUE, &resp.result().unwrap_or_default().to_cpon());
                let fmtstr = fmtstr.replace("\\n", "\n");
                let fmtstr = fmtstr.replace("\\t", "\t");
                fmtstr.as_bytes().to_owned()
            }
        };
        stdout.write_all(&bytes).await?;
        Ok(stdout.flush().await?)
    }

    let mut stdout = io::stdout();
    if opts.method.is_none() {
        fn parse_line(line: &str) -> std::result::Result<(&str, &str, &str), String> {
            let line = line.trim();
            let method_ix = match line.find(':') {
                None => {
                    return Err(format!("Invalid line format, method not found: {line}"));
                }
                Some(ix) => ix,
            };
            let param_ix = line.find(' ');
            let path = line[..method_ix].trim();
            let (method, param) = match param_ix {
                None => (line[method_ix + 1..].trim(), ""),
                Some(ix) => (line[method_ix + 1..ix].trim(), line[ix + 1..].trim()),
            };
            Ok((path, method, param))
        }
        if is_tty() {
            #[cfg(feature = "readline")]
            {
                let (mut rl, mut rl_stdout) =
                    rustyline_async::Readline::new("> ".to_owned()).unwrap();
                rl.set_max_history(1000);
                loop {
                    match rl.readline().await {
                        Ok(ReadlineEvent::Line(line)) => {
                            let line = line.trim();
                            rl.add_history_entry(line.to_owned());
                            match parse_line(line) {
                                Ok((path, method, param)) => {
                                    let rqid =
                                        send_request(&mut *frame_writer, path, method, param)
                                            .await?;
                                    loop {
                                        let resp = frame_reader.receive_message().await?;
                                        print_resp(
                                            &mut stdout,
                                            &resp,
                                            (&*opts.output_format).into(),
                                        )
                                        .await?;
                                        if resp.is_response()
                                            && resp.request_id().unwrap_or_default() == rqid
                                        {
                                            break;
                                        }
                                    }
                                }
                                Err(err) => {
                                    writeln!(rl_stdout, "{}", err)?;
                                }
                            }
                        }
                        Ok(ReadlineEvent::Eof) => {
                            // stream closed
                            break;
                        }
                        Ok(ReadlineEvent::Interrupted) => {
                            // Ctrl-C
                            break;
                        }
                        // Err(ReadlineError::Closed) => break, // Readline was closed via one way or another, cleanup other futures here and break out of the loop
                        Err(err) => {
                            error!("readline error: {:?}", err);
                            break;
                        }
                    }
                    // Flush all writers to stdout
                    rl.flush()?;
                }
            }
        } else {
            let stdin = io::stdin();
            loop {
                let mut line = String::new();
                match stdin.read_line(&mut line).await {
                    Ok(nbytes) => {
                        if nbytes == 0 {
                            // stream closed
                            break;
                        } else {
                            match parse_line(&line) {
                                Ok((path, method, param)) => {
                                    let rqid =
                                        send_request(&mut *frame_writer, path, method, param)
                                            .await?;
                                    loop {
                                        let resp = frame_reader.receive_message().await?;
                                        print_resp(
                                            &mut stdout,
                                            &resp,
                                            (&*opts.output_format).into(),
                                        )
                                        .await?;
                                        if resp.is_response()
                                            && resp.request_id().unwrap_or_default() == rqid
                                        {
                                            break;
                                        }
                                    }
                                }
                                Err(err) => {
                                    return Err(err.into());
                                }
                            }
                        }
                    }
                    Err(err) => return Err(format!("Read line error: {err}").into()),
                }
            }
        }
    } else {
        let method = opts.method.clone().unwrap();
        let (path, method) = if let Some(ix) = method.find(':') {
            (method[0..ix].to_owned(), method[ix + 1..].to_owned())
        } else {
            return Err("--method parameter must be in form shv/path:method".into());
        };
        let param = opts.param.clone().unwrap_or_default();
        let rqid = send_request(&mut *frame_writer, &path, &method, &param).await?;
        async fn receive_response(
            frame_reader: &mut BoxedFrameReader,
            rq_id: RqId,
        ) -> shvrpc::Result<RpcFrame> {
            loop {
                let frame = frame_reader.receive_frame().await?;
                if frame.is_response() && frame.request_id().unwrap_or_default() == rq_id {
                    return Ok(frame);
                }
            }
        }
        let res = if opts.timeout > 0 {
            match receive_response(&mut frame_reader, rqid)
                .timeout(Duration::from_millis(opts.timeout))
                .await
            {
                Ok(maybe_frame) => maybe_frame,
                Err(_) => {
                    return Err(format!(
                        "Method call response timeout after {} msec.",
                        opts.timeout
                    )
                    .into())
                }
            }
        } else {
            receive_response(&mut frame_reader, rqid).await
        };
        return match res {
            Ok(frame) => {
                let resp = frame.to_rpcmesage()?;
                print_resp(&mut stdout, &resp, (&*opts.output_format).into()).await?;
                Ok(())
            }
            Err(e) => Err(e),
        };
    }
    Ok(())
}
async fn receive_response(
    frame_reader: &mut BoxedFrameReader,
    rq_id: RqId,
) -> shvrpc::Result<RpcFrame> {
    loop {
        let frame = frame_reader.receive_frame().await?;
        if frame.is_response() && frame.request_id().unwrap_or_default() == rq_id {
            return Ok(frame);
        }
    }
}
async fn make_burst_call(opts: &Opts) -> Result {
    if opts.method.is_none() {
        return Err("--method parameter missing".into());
    }
    let burst = opts.burst.clone().unwrap();
    let (nmsg, ntask) = {
        let mut s = burst.split(',');
        let nmsg = s.next().unwrap();
        let nmsg = nmsg.parse::<i32>().unwrap();
        let ntask = s.next().unwrap_or("1");
        let ntask = ntask.parse::<i32>().unwrap();
        (nmsg, ntask)
    };
    let method = opts.method.clone().unwrap();
    let ri = ShvRI::try_from(method)?;
    let param = opts.param.clone().map(|p| RpcValue::from_cpon(&p).unwrap());
    async fn burst_task(
        url: Url,
        path: String,
        method: String,
        param: Option<RpcValue>,
        taskno: i32,
        count: i32,
    ) {
        println!("Starting burst task #{taskno}, {count} calls of {path}:{method}");
        let (mut frame_reader, mut frame_writer) = login(&url).await.unwrap();
        for _ in 0..count {
            let rqid = frame_writer
                .send_request(&path, &method, param.clone())
                .await
                .unwrap();
            receive_response(&mut frame_reader, rqid).await.unwrap();
        }
        println!("Burst task #{taskno} finished, after {count} calls made successfully.");
    }
    let url = opts.url.clone();
    (0..ntask)
        .map(|taskno| {
            task::spawn(burst_task(
                url.clone(),
                ri.path().to_owned(),
                ri.method().to_owned(),
                param.clone(),
                taskno + 1,
                nmsg,
            ))
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;

    Ok(())
}

fn split_quoted(s: &str) -> Vec<&str> {
    let mut wrapped = false;
    let ret = s
        .split(|c| {
            if c == '[' {
                wrapped = true;
            } else if c == ']' {
                wrapped = false;
            }
            c == ':' && !wrapped
        })
        .collect::<Vec<&str>>();
    ret
}
#[derive(Debug)]
struct Tunnel {
    create_rqid: RqId,
    write_rqid: RqId,
    frame_sender: Sender<RpcFrame>,
}
async fn start_tunnel_server(
    mut broker_frame_reader: BoxedFrameReader,
    mut broker_frame_writer: BoxedFrameWriter,
    opts: &Opts,
) -> Result {
    let tunnel_str = opts.tunnel.as_ref().unwrap().as_str();
    let tunnel: Vec<_> = split_quoted(tunnel_str);
    let tunnel = &tunnel[..];
    if tunnel.len() < 3 || tunnel.len() > 4 {
        return Err(format!("Invalid tunnel specification: {tunnel_str}").into());
    }
    let (local_host, tunnel) = if tunnel.len() == 4 {
        (
            if tunnel[0].is_empty() {
                "0.0.0.0"
            } else {
                tunnel[0]
            },
            &tunnel[1..],
        )
    } else {
        ("127.0.0.1", tunnel)
    };
    let local_port = tunnel[0];
    let remote_host = tunnel[1];
    let remote_port = tunnel[2];
    let remote_host_port = format!("{remote_host}:{remote_port}");
    let local_port = local_port.parse::<i32>()?;
    let local_host = local_host.to_owned();

    let mut tunnels: Vec<Tunnel> = Vec::new();

    debug!(target: "Tunnel", "Starting TCP server on {local_host}:{local_port}");
    let listener = TcpListener::bind(format!("{local_host}:{local_port}")).await?;
    let mut incoming = listener.incoming();

    let (write_frame_sender, write_frame_receiver) = async_channel::unbounded();
    loop {
        select! {
            stream = incoming.next().fuse() => {
                if let Some(stream) = stream {
                    let stream = stream?;
                    debug!(target: "Tunnel", "New connection from {:?}", stream.local_addr());
                    let create_rqid = RpcMessage::next_request_id();
                    let write_rqid = RpcMessage::next_request_id();
                    let (read_frame_sender, read_frame_receiver) = async_channel::unbounded();
                    let tunnel = Tunnel {create_rqid, write_rqid, frame_sender: read_frame_sender};
                    tunnels.push(tunnel);
                    let read_frame_receiver = read_frame_receiver.clone();
                    let write_frame_sender = write_frame_sender.clone();
                    let remote_host_port = remote_host_port.clone();
                    spawn_and_log_error(async move {
                        handle_tunnel_socket(stream, remote_host_port, create_rqid, write_rqid, read_frame_receiver, write_frame_sender.clone()).await.map_err(|e | e.to_string())
                    });
                } else {
                    break;
                }
            }
            frame = broker_frame_reader.receive_frame().fuse() => {
                match frame {
                    Ok(frame) => {
                        let rqid = frame.request_id().unwrap_or(0);
                        for tunnel in &tunnels {
                            if tunnel.write_rqid == rqid || tunnel.create_rqid == rqid {
                                tunnel.frame_sender.send(frame).await?;
                                break;
                            }
                        }
                        tunnels.retain(|tunnel| tunnel.frame_sender.is_closed());
                        // {
                        //     if tunnel.frame_sender.is_closed() {
                        //         debug!(target: "Tunnel", "Removing closed tunnel state {:?}", tunnel);
                        //         false
                        //     } else {
                        //         true
                        //     }
                        // });
                    }
                    Err(e) => {
                        error!("Get response receiver error: {e}");
                        break;
                    }
                }
            }
            frame = write_frame_receiver.recv().fuse() => {
                match frame {
                    Ok(frame) => {
                        broker_frame_writer.send_frame(frame).await?;
                    }
                    Err(e) => {
                        error!("Get response receiver error: {e}");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_tunnel_socket(stream: TcpStream, remote_host_port: String, create_rqid: RqId, write_rqid: RqId, read_frame_receiver: Receiver<RpcFrame>, mut write_frame_sender: Sender<RpcFrame>) -> Result {
    let tunid = {
        debug!(target: "Tunnel", "Creating tunnel");
        let tun_opts = Map::from([("host".into(), (remote_host_port).into())]);
        let mut rq = RpcMessage::new_request(".app/tunnel", "create", Some(tun_opts.into()));
        rq.set_request_id(create_rqid);
        write_frame_sender.send(rq.to_frame()?).await?;
        loop {
            match timeout(core::time::Duration::from_secs(10), read_frame_receiver.recv()).await {
                Ok(frame) => {
                    let frame = frame?;
                    if frame.request_id().unwrap_or_default() == create_rqid {
                        let tunid = frame.to_rpcmesage()?.result()?.as_str().parse::<u64>()?;
                        break tunid;
                    }
                }
                Err(e) => {
                    return Err(format!("Creating tunnel timeout: {}", e).into());
                }
            }
        }
    };
    let mut expected_read_seqno = 0;
    let mut seqno_to_write = 0;
    {
        let mut rq = RpcMessage::new_request(&format!(".app/tunnel/{tunid}"), "write", None);
        rq.set_request_id(write_rqid);
        rq.set_seqno(seqno_to_write);
        seqno_to_write += 1;
        debug!(target: "Tunnel", "Starting data exchange");
        write_frame_sender.send(rq.to_frame()?).await?;
    };
    let (mut sock_reader, mut sock_writer) = stream.split();
    let mut sock_read_buff: [u8; 1024] = [0; 1024];
    loop {
        select! {
            n = sock_reader.read(&mut sock_read_buff).fuse() => {
                let n = n?;
                if n == 0 {
                    debug!(target: "Tunnel", "Tunnel client socket closed");
                    break;
                }
                let data = &sock_read_buff[0 .. n];
                seqno_to_write = process_socket_to_broker_data(tunid, seqno_to_write, write_rqid, data, &mut write_frame_sender).await?;
            }
            frame = read_frame_receiver.recv().fuse() => {
                match frame {
                    Ok(frame) => {
                        expected_read_seqno = process_broker_to_socket_frame(write_rqid, expected_read_seqno, &frame, &mut sock_writer).await?;
                    }
                    Err(e) => {
                        error!("Get response receiver error: {e}");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn process_broker_to_socket_frame(rqid: RqId, expected_seqno: SeqNo, frame: &RpcFrame, sock_writer: &mut WriteHalf<TcpStream>) -> shvrpc::Result<SeqNo> {
    if frame.request_id().unwrap_or_default() != rqid {
        return Ok(expected_seqno)
    }
    let resp = frame.to_rpcmesage()?;
    let data = resp.result()?.as_blob();
    let seqno = frame.seqno();
    if let Some(seqno) = seqno {
        if expected_seqno > seqno {
            warn!("Seqno: {seqno} received already, expected value: {expected_seqno}, ignoring data.");
            return Ok(expected_seqno)
        }
        if expected_seqno < seqno {
            warn!("Seqno: {seqno} greater than expected: {expected_seqno}, some data was lost!.");
        }
        sock_writer.write_all(data).await?;
        sock_writer.flush().await?;
        return Ok(seqno + 1)
    }
    warn!("Seqno not received, ignoring data.");
    Ok(expected_seqno)
}
async fn process_socket_to_broker_data(tunid: u64, seqno_to_write: SeqNo, write_rqid: RqId, data: &[u8], frame_writer: &mut Sender<RpcFrame>) -> shvrpc::Result<SeqNo> {
    let mut rq = RpcMessage::new_request(&format!(".app/tunnel/{tunid}"), "write", Some(RpcValue::from(data)));
    rq.set_request_id(write_rqid);
    rq.set_seqno(seqno_to_write);
    frame_writer.send(rq.to_frame()?).await?;
    Ok(seqno_to_write + 1)
}

pub async fn try_main(opts: Opts) -> Result {
    if opts.burst.is_some() {
        return make_burst_call(&opts).await;
    }
    let (frame_reader, frame_writer) = login(&opts.url).await?;
    let res = if opts.tunnel.is_some() {
        start_tunnel_server(frame_reader, frame_writer, &opts).await
    } else {
        make_call(frame_reader, frame_writer, &opts).await
    };
    match res {
        Ok(_) => Ok(()),
        Err(err) => {
            eprintln!("{err}");
            Err(err)
        }
    }
}
