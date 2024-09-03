use std::collections::BTreeMap;
use std::future::Future;
use async_std::io::BufReader;
use async_std::net::{TcpListener, TcpStream};
use async_std::os::unix::net::UnixStream;
use async_std::{channel, io, task};
use clap::Parser;
use futures::io::BufWriter;
use futures::{select, AsyncReadExt, FutureExt};
use futures::AsyncWriteExt;
use log::*;
use shvrpc::client::LoginParams;
use shvrpc::framerw::{FrameReader, FrameWriter};
use shvrpc::rpcmessage::{RqId};
use shvrpc::serialrw::{SerialFrameReader, SerialFrameWriter};
use shvrpc::streamrw::{StreamFrameReader, StreamFrameWriter};
use shvrpc::util::{login_from_url, parse_log_verbosity};
use shvrpc::{client, RpcMessage, RpcMessageMetaTags};
use simple_logger::SimpleLogger;
use url::Url;
use async_std::channel::{Sender};
use async_std::stream::StreamExt;
use shvrpc::rpcframe::RpcFrame;

#[cfg(feature = "readline")]
use crossterm::tty::IsTty;
#[cfg(feature = "readline")]
use rustyline_async::ReadlineEvent;
use shvproto::{Map, RpcValue};
#[cfg(feature = "readline")]
use std::io::Write;

type Result = shvrpc::Result<()>;

#[derive(Parser, Debug)]
//#[structopt(name = "shvcall", version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = "SHV call")]
struct Opts {
    ///Url to connect to, example tcp://admin@localhost:3755?password=dj4j5HHb, localsocket:path/to/socket
    #[arg(name = "url", short = 's', long = "url")]
    url: String,
    #[arg(short = 't', long = "path")]
    path: Option<String>,
    /// Method can be specified also together with path like: shv/path:method
    #[arg(short, long)]
    method: Option<String>,
    #[arg(short, long)]
    param: Option<String>,
    /// Output format: [ cpon | chainpack | simple | value | "Placeholders {PATH} {METHOD} {VALUE} in any number and combination in custom string." ]
    #[arg(short = 'o', long = "output-format", default_value = "cpon")]
    output_format: String,
    /// Create TCP tunnel, SSH like syntax, example: -L 2222:some.host.org:22
    #[arg(short = 'L', long)]
    tunnel: Option<String>,
    /// Verbose mode (module, .)
    #[arg(short, long)]
    verbose: Option<String>,
}
enum OutputFormat {
    Cpon,
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
            _ => Self::Custom(value.to_string()),
        }
    }
}
type BoxedFrameReader = Box<dyn FrameReader + Unpin + Send>;
type BoxedFrameWriter = Box<dyn FrameWriter + Unpin + Send>;

fn is_tty() -> bool {
    #[cfg(feature = "readline")]
    return io::stdin().is_tty();
    #[cfg(not(feature = "readline"))]
    return false;
}
fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
where
    F: Future<Output = shvrpc::Result<()>> + Send + 'static,
{
    task::spawn(async move {
        if let Err(e) = fut.await {
            error!("{}", e)
        }
    })
}

pub(crate) fn main() -> Result {
    let opts = Opts::parse();

    let mut logger = SimpleLogger::new();
    logger = logger.with_level(LevelFilter::Info);
    if let Some(module_names) = &opts.verbose {
        for (module, level) in parse_log_verbosity(module_names, module_path!()) {
            logger = logger.with_module_level(module, level);
        }
    }
    logger.init().unwrap();

    info!("=====================================================");
    info!("{} starting", std::module_path!());
    info!("=====================================================");

    // let rpc_timeout = Duration::from_millis(DEFAULT_RPC_TIMEOUT_MSEC);
    let url = Url::parse(&opts.url)?;

    task::block_on(try_main(&url, opts))
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
    info!("Connected to broker.");
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
async fn make_call(mut frame_reader: BoxedFrameReader, mut frame_writer: BoxedFrameWriter, opts: &Opts) -> Result {
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
    if opts.path.is_some() && opts.method.is_none() {
        return Err("--method parameter missing".into());
    }
    let mut stdout = io::stdout();
    if opts.path.is_none() && opts.method.is_none() {
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
        let mut path = opts.path.clone().unwrap_or_default();
        let mut method = opts.method.clone().unwrap_or_default();
        if let Some(ix) = method.find(':') {
            path = method[0..ix].to_owned();
            method = method[ix + 1..].to_owned();
        }
        let param = opts.param.clone().unwrap_or_default();
        send_request(&mut *frame_writer, &path, &method, &param).await?;
        let resp = frame_reader.receive_message().await?;
        print_resp(&mut stdout, &resp, (&*opts.output_format).into()).await?;
    }

    Ok(())
}
async fn make_tunnel(mut frame_reader: BoxedFrameReader, mut frame_writer: BoxedFrameWriter, opts: &Opts) -> Result {
    let mut tunnel = opts.tunnel.as_ref().unwrap().split(':');
    let local_port = tunnel.next().ok_or("Local port must be specified")?;
    let host = tunnel.next().ok_or("Host must be specified")?;
    let remote_port = tunnel.next().ok_or("Remote port must be specified")?;
    let host = format!("{host}:{remote_port}");
    let local_port = local_port.parse::<i32>()?;
    enum RpcReaderCmd {
        RegisterResponse(RqId, Sender<RpcFrame>, bool),
        UnregisterResponse(RqId),
    }
    let (reader_cmd_sender, reader_cmd_receiver) = channel::unbounded::<RpcReaderCmd>();
    spawn_and_log_error(async move {
        struct PendingCall {
            sender: Sender<RpcFrame>,
            one_shot: bool,
        }
        let mut pending_calls = BTreeMap::<RqId, PendingCall>::new();
        let mut get_frame_fut = frame_reader.receive_frame().fuse();
        loop {
            select! {
                frame = get_frame_fut => {
                    match frame {
                        Ok(frame) => {
                            let rqid = frame.request_id().unwrap_or_default();
                            let drop_it = if let Some(pc) = pending_calls.get(&rqid) {
                                pc.sender.send(frame).await?;
                                pc.one_shot
                            } else {
                                false
                            };
                            if drop_it {
                                pending_calls.remove(&rqid);
                            }
                            drop(get_frame_fut);
                            get_frame_fut = frame_reader.receive_frame().fuse();
                        }
                        Err(e) => {
                            info!("RPC socket read error: {e}");
                            break;
                        }
                    }
                }
                msg = reader_cmd_receiver.recv().fuse() => {
                    match msg {
                        Ok(msg) => {
                            match msg {
                                RpcReaderCmd::RegisterResponse(rqid, sender, one_shot) => {
                                    pending_calls.insert(rqid, PendingCall {sender, one_shot});
                                }
                                RpcReaderCmd::UnregisterResponse(rqid) => {
                                    pending_calls.remove(&rqid);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Read get frame message error: {e}");
                            break;
                        }
                    }
                }
            }
        }
        shvrpc::Result::Ok(())
    });
    let (writer_sender, writer_receiver) = channel::unbounded::<RpcFrame>();
    spawn_and_log_error(async move {
        loop {
            let frame = writer_receiver.recv().await?;
            frame_writer.send_frame(frame).await?
        }
    });
    info!("Starting TCP server");
    let listener = TcpListener::bind(format!("127.0.0.1:{local_port}")).await?;
    let mut incoming = listener.incoming();

    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        info!("New connection from {:?}", stream.local_addr());
        info!("Creating tunnel");
        //let tunid = call(&mut *frame_reader, &mut *frame_writer, ".app/tunnel", "create", Some(tun_opts.into())).await?.as_str().to_owned();
        let host = host.clone();
        let reader_cmd_sender = reader_cmd_sender.clone();
        let writer_sender = writer_sender.clone();
        spawn_and_log_error(async move {
            let tunid = {
                let tun_opts = Map::from([("host".into(), host.into())]);
                let rq = RpcMessage::new_request(".app/tunnel", "create", Some(tun_opts.into()));
                let rqid = rq.request_id().unwrap();
                let (sender, receiver) = channel::unbounded::<RpcFrame>();
                reader_cmd_sender.send(RpcReaderCmd::RegisterResponse(rqid, sender, true)).await?;
                writer_sender.send(rq.to_frame()?).await?;
                let resp = receiver.recv().await?;
                resp.to_rpcmesage()?.result()?.as_str().to_owned()
            };
            let rq = RpcMessage::new_request(&format!(".app/tunnel/{tunid}"), "write", None);
            let rqid = rq.request_id().unwrap();
            let (sender, receiver) = channel::unbounded::<RpcFrame>();
            reader_cmd_sender.send(RpcReaderCmd::RegisterResponse(rqid, sender, false)).await?;
            writer_sender.send(rq.to_frame()?).await?;
            let (mut sock_reader, mut sock_writer) = stream.split();
            let mut sock_read_buff: [u8; 1024] = [0; 1024];
            loop {
                select! {
                    n = sock_reader.read(&mut sock_read_buff).fuse() => {
                        let n = n?;
                        if n == 0 {
                            info!("Tunnel client socket closed");
                            break;
                        }
                        let data = RpcValue::from(&sock_read_buff[0 .. n]);
                        let rq = RpcMessage::new_request(&format!(".app/tunnel/{tunid}"), "write", Some(data));
                        writer_sender.send(rq.to_frame()?).await?;
                    }
                    frame = receiver.recv().fuse() => {
                        match frame {
                            Ok(frame) => {
                                let resp = frame.to_rpcmesage()?;
                                let data = resp.result()?.as_blob();
                                sock_writer.write_all(data).await?;
                                sock_writer.flush().await?;
                            }
                            Err(e) => {
                                error!("Get response receiver error: {e}");
                                break;
                            }
                        }
                    }
                }
            }
            reader_cmd_sender.send(RpcReaderCmd::UnregisterResponse(rqid)).await?;
            Ok(())
        });
    }
    Ok(())
}
async fn try_main(url: &Url, opts: Opts) -> Result {
    let (frame_reader, frame_writer) = login(url).await?;
    let res = if opts.tunnel.is_some() {
        make_tunnel(frame_reader, frame_writer, &opts).await
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
