use std::{collections::HashMap, io, pin::Pin, path::Path};
use std::marker::Send;
use std::os::unix::fs::FileTypeExt;
use std::sync::Arc;
use tokio::net::UnixStream;
use core::ops::DerefMut;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use clap::Parser;
use tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};
use tokio::fs::OpenOptions;
use tokio::sync::{Mutex, mpsc};

#[derive(Parser, Debug)]
struct Args {
    socket: String,

    #[arg(long)]
    serve: bool
}

trait AsyncReadWrite: AsyncRead + AsyncWrite {}

type InputStream = Pin<Box<dyn AsyncRead + Send>>;
type OutputStream = Pin<Box<dyn AsyncWrite + Send>>;

async fn service<Cb, F>(path: &Path, cb: &'static Cb) -> io::Result<()>
where Cb: Fn(InputStream, OutputStream) -> F,
      Cb: Sync,
      F: Future<Output = io::Result<()>> + Send {
    let server = tokio::net::UnixListener::bind(path)?;
    loop {
        let socket = server.accept().await?;
        let (reader, writer) = socket.0.into_split();
        tokio::spawn(async {
            if let Err(err) = cb(Box::pin(reader), Box::pin(writer)).await {
                println!("Service ended: {err}")
            }
        });
    }
}

async fn open_stream<Cb, F>(args: Args, cb: &'static Cb) -> io::Result<()>
where F: Future<Output = io::Result<()>> + Send,
      Cb: Fn(InputStream, OutputStream) -> F,
      Cb: Sync {
    let path = Path::new(&args.socket);
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            let ty = metadata.file_type();

            if ty.is_socket() {
                if args.serve  {
                    std::fs::remove_file(path)?;
                    service(path, cb).await
                } else {
                    let stream = UnixStream::connect(path).await?;
                    let (reader, writer) = stream.into_split();
                    cb(Box::pin(reader), Box::pin(writer)).await
                }
            } else {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .await?;
                let (reader, writer) = tokio::io::split(file);
                cb(Box::pin(reader), Box::pin(writer)).await
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound && args.serve =>
            service(path, cb).await,
        Err(e) => Err(e)
    }
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opcode {
    Run = 1,  
    Write = 2,
    Close = 3
}
impl TryFrom<u16> for Opcode {
    type Error = u16;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Run),
            2 => Ok(Self::Write),
            3 => Ok(Self::Close),
            _ => Err(value),
        }
    }
}

#[repr(u16)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
enum ResultCode {
    Success = 1,
    InvalidState = 2,
    NotFound = 3,
    Closed = 4,
    Failed = 5,
    UnknownOp = 6,
    Output = 7,
    Sigchild = 8,
    Close = 9
}

#[derive(Debug)]
struct GenericError {
    result: ResultCode,
    attrs: Vec<(Attr, Box<[u8]>)>
}

impl GenericError {
    fn new(result: ResultCode) -> Self {
        GenericError { result,
                       attrs: vec!{} }
    }

    fn attr<T: ToAttr + ?Sized>(mut self, name: AttrName, val: &T) -> Self {
        let buf = val.encode_attr();
        let attr = Attr {
            attr: name as u16,
            len: buf.len() as u16
        };
        self.attrs.push((attr, buf));
        self
    }
    async fn send(self, stream: &mut OutputStream) -> io::Result<()> {
        let cmd = Command {
            op: self.result as u16,
            num_attrs: self.attrs.len() as u8,
            padding: 0u8
        };
        stream.write(cmd.as_bytes()).await?;
        for attr in self.attrs {
            stream.write(attr.0.as_bytes()).await?;
            stream.write(&attr.1).await?;
        };
        Ok(())
    }
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttrName {
    Command = 1,
    Message = 2,
    Pid = 3,
    Data = 4,
    Stderr = 5,
    ExitCode = 6
}
impl TryFrom<u16> for AttrName {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Command),
            2 => Ok(Self::Message),
            3 => Ok(Self::Pid),
            4 => Ok(Self::Data),
            _ => Err(()),
        }
    }
}

#[repr(C)]
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable)]
struct Command {
    op: u16,
    num_attrs: u8,
    padding: u8,
}

#[repr(C)]
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable)]
struct Attr {
    attr: u16,
    len: u16
}

#[derive(Default)]
struct RunOp {
    args: Vec<String>,
}

enum StreamTarget {
    PidStream(u32)
}

#[derive(Default)]
struct WriteOp {
    target: Option<StreamTarget>,
    buf: Vec<u8>
}

#[derive(Default)]
struct CloseOp {
    target: Option<StreamTarget>
}

#[derive(Default)]
struct NoOp {}

async fn discard(stream: &mut InputStream, mut n: usize) -> io::Result<()> {
    let mut buf = [0u8; 4096];

    while n > 0 {
        let want = n.min(buf.len());
        stream.read_exact(&mut buf[..want]).await?;
        n -= want
    }

    Ok(())
}

async fn read_string(mut n: usize, stream: &mut InputStream) -> io::Result<String> {
    let mut buf = [0u8; 4096];
    let mut x = String::new();

    while n > 0 {
        let want = n.min(buf.len());
        stream.read_exact(&mut buf[..want]).await?;
        n -= want;

        match std::str::from_utf8(&buf[..want]) {
            Ok(chunk) => x.push_str(chunk),
            Err(_) => ()
        }
    };

    Ok(x)
}

struct Empty{}

trait ToAttr {
    fn encode_attr(&self) -> Box<[u8]>;
}

impl ToAttr for Empty {
    fn encode_attr(&self) -> Box<[u8]> {
        Box::new([])
    }
}

impl ToAttr for i32 {
    fn encode_attr(&self) -> Box<[u8]> {
        Box::new(self.to_ne_bytes())
    }
}

impl ToAttr for u32 {
    fn encode_attr(&self) -> Box<[u8]> {
        Box::new(self.to_ne_bytes())
    }
}

impl ToAttr for &str {
    fn encode_attr(&self) -> Box<[u8]> {
        self.as_bytes().into()
    }
}

impl ToAttr for String {
    fn encode_attr(&self) -> Box<[u8]> {
        self.clone().into_bytes().into_boxed_slice()
    }
}

impl ToAttr for [u8] {
    fn encode_attr(&self) -> Box<[u8]> {
        self.to_vec().into_boxed_slice()
    }
}

trait FromAttrs {
    async fn set_attr(&mut self, attr: AttrName, len: u16, stream: &mut InputStream) -> io::Result<()>;
    async fn update_state(self, state: &mut State, stream: &mut OutputStream) -> io::Result<()>;
}

impl FromAttrs for WriteOp {
    async fn set_attr(&mut self, attr: AttrName, len: u16, stream: &mut InputStream) -> io::Result<()> {
        match attr {
            AttrName::Pid => {
                self.target = Some(StreamTarget::PidStream(stream.read_u32().await?));
            },
            AttrName::Data => {
                let mut buf = Vec::new();
                buf.resize(len as usize, 0u8);
                stream.read_exact(buf.as_mut_bytes()).await?;

                self.buf = buf;
            },
            _ => ()
        }
        Ok(())
    }

    async fn update_state(self, state: &mut State, stream: &mut OutputStream) -> io::Result<()> {
        use std::collections::hash_map::Entry;
        match self.target {
            None => {
                GenericError::new(ResultCode::InvalidState)
                    .attr(AttrName::Message, &"No write target")
                    .send(stream).await?;
            },
            Some(StreamTarget::PidStream(pid)) => {
                match state.processes.entry(pid) {
                    Entry::Vacant(_) => {
                        GenericError::new(ResultCode::NotFound)
                            .attr(AttrName::Pid, &pid)
                            .send(stream).await?;
                    },
                    Entry::Occupied(mut proc) => {
                        match proc.get_mut().stdin.write_all(&self.buf).await {
                            Ok(_) =>
                                GenericError::new(ResultCode::Success).send(stream).await?,
                            Err(_) =>
                                GenericError::new(ResultCode::Failed).send(stream).await?
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl FromAttrs for RunOp {
    async fn set_attr(&mut self, attr: AttrName, len: u16, stream: &mut InputStream) -> io::Result<()> {
        self.args.push(read_string(len as usize, stream).await?);
        Ok(())
    }
    async fn update_state(self, state: &mut State, stream: &mut OutputStream) -> io::Result<()> {
        if self.args.is_empty() {
            GenericError::new(ResultCode::InvalidState)
                .attr(AttrName::Message, &"No arguments given")
                .send(stream).await?;
        } else {
            let mut child =
                tokio::process::Command::new(self.args[0].clone())
                .args(&self.args[1..])
                .stderr(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stdin(std::process::Stdio::piped())
                .spawn()?;
            let stdin = child.stdin.take().unwrap();
            let procstate = Box::new(ProcState::new(stdin));
            let pid = child.id().unwrap();
            GenericError::new(ResultCode::Success)
                .attr(AttrName::Pid, &pid)
                .send(stream).await?;
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            state.processes.insert(pid, procstate);
            tokio::spawn(output_monitor(Box::pin(stdout), pid, state.queue_writer.clone(), false));
            tokio::spawn(output_monitor(Box::pin(stderr), pid, state.queue_writer.clone(), true));
            tokio::spawn(process_monitor(child, pid, state.queue_writer.clone()));
        }
        Ok(())
    }
}

impl FromAttrs for NoOp {
    async fn set_attr(&mut self, attr: AttrName, len: u16, stream: &mut InputStream) -> io::Result<()> {
        discard(stream, len as usize).await
    }

    async fn update_state(self, state: &mut State, stream: &mut OutputStream) -> io::Result<()> {
        GenericError::new(ResultCode::UnknownOp)
            .send(stream).await?;
        Ok(())
    }
}

async fn parse_attrs<T: FromAttrs + Default>(stream: &mut InputStream, command: &Command) -> io::Result<T> {
    let mut x = T::default();

    for i in 0..command.num_attrs {
        let mut buf = [0u8; std::mem::size_of::<Attr>()];
        stream.read_exact(&mut buf).await?;
        let attr = Attr::read_from_bytes(&buf).expect("buffer has exactly the right size");

        match AttrName::try_from(attr.attr) {
            Ok(nm) => x.set_attr(nm, attr.len, stream).await?,
            Err(_) => discard(stream, attr.len as usize).await?
        }
    }

    Ok(x)
}

struct ProcState {
    stdin: tokio::process::ChildStdin
}

impl ProcState {
    fn new(stdin: tokio::process::ChildStdin) -> Self {
        ProcState { stdin }
    }
}

struct State {
    processes: HashMap<u32, Box<ProcState>>,
    queue_writer: mpsc::Sender<GenericError>
}

impl State {
    fn new() -> (State, mpsc::Receiver<GenericError>) {
        let (writer, receiver) = mpsc::channel(16);
        (State { processes: HashMap::new(),
                 queue_writer: writer },
         receiver)
    }
}

async fn process_monitor(mut child: tokio::process::Child, pid: u32,
                         queue: mpsc::Sender<GenericError>) {
    match child.wait().await {
        Err(x) => println!("Could not wait for {0}: {x}", child.id().unwrap()),
        Ok(done) => queue.send(GenericError::new(ResultCode::Sigchild)
                               .attr(AttrName::Pid, &pid)
                               .attr(AttrName::ExitCode, &done.code().unwrap())).await.unwrap()
    }
}


async fn output_monitor(mut stdout: Pin<Box<impl AsyncRead>>, pid: u32,
                        queue: mpsc::Sender<GenericError>,
                        is_stderr: bool) {
    let mut outbuf = [0u8;4096];
    loop {
        match stdout.read(&mut outbuf).await {
            Err(x) => println!("Could not read from {pid} (stderr={is_stderr}): {x}"),
            Ok(bytes) if bytes > 0 => {
                let out = &outbuf[..bytes];
                let mut err = GenericError::new(ResultCode::Output)
                    .attr(AttrName::Pid, &pid)
                    .attr(AttrName::Data, &outbuf[..bytes]);
                if is_stderr {
                    err = err.attr(AttrName::Stderr, &Empty{});
                }
                queue.send(err).await;
            },
            Ok(_) => {
                let mut err = GenericError::new(ResultCode::Close)
                    .attr(AttrName::Pid, &pid);
                if is_stderr {
                    err = err.attr(AttrName::Stderr, &Empty{});
                }
                queue.send(err).await;
                return;
            }
        }
    }
}

async fn write_half(mut queue: mpsc::Receiver<GenericError>,
                    mut write_stream: Arc<Mutex<OutputStream>>) {
    loop {
        let ready = queue.recv().await;
        // Ready is now a
        match ready {
            None => return,
            Some(pending) => {
                // The entire box buf should be sent out the main socket
                let mut stream = write_stream.lock().await;
                if let Err(x) = pending.send(&mut stream).await {
                    println!("Could not write response: {x:?}");
                    return
                }
            }
        }
    }
}

async fn serve(mut read_stream: InputStream, write_stream: OutputStream) -> io::Result<()> {
    let (mut state, receiver) = State::new();
    let write_stream_sync = Arc::new(Mutex::new(write_stream));
    tokio::spawn(write_half(receiver, write_stream_sync.clone()));

    // Read commands
    loop {
        let mut buf = [0u8; std::mem::size_of::<Command>()];
        let recvd = read_stream.read_exact(&mut buf).await?;
        if recvd == 0 { return Ok(()) }
        let h = Command::read_from_bytes(&buf).expect("buffer has exactly the right size");
        let op = Opcode::try_from(h.op);
        match op {
            Ok(Opcode::Run) => parse_attrs::<RunOp>(&mut read_stream, &h).await?.update_state(&mut state, &mut (write_stream_sync.lock().await.deref_mut())).await?,
            Ok(Opcode::Write) => parse_attrs::<WriteOp>(&mut read_stream, &h).await?.update_state(&mut state, &mut (write_stream_sync.lock().await.deref_mut())).await?,
            Ok(Opcode::Close) => todo!("Close"),
            Err(_) => parse_attrs::<NoOp>(&mut read_stream, &h).await?.update_state(&mut state, &mut (write_stream_sync.lock().await.deref_mut())).await?,
        };
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    open_stream(args, &serve).await.expect("Could not open stream"); 
}
