#![forbid(unsafe_code)]

use std::fmt;
use std::io::{self, Read, Write};

const MAGIC: u32 = 0x5943_4e52;
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 12;
const MAX_FRAME_BYTES: usize = 64 * 1024;

const SPAWN: u16 = 1;
const JOIN: u16 = 2;
const BUDGET: u16 = 3;
const CANCELLED: u16 = 4;
const FINISH: u16 = 5;
const ACK: u16 = 101;
const OUTCOME: u16 = 102;
const BUDGET_RESULT: u16 = 103;
const CANCELLED_RESULT: u16 = 104;
const FINISHED: u16 = 105;
const FAILURE: u16 = 199;

// Generated-facing contract. Wire tags and codecs remain private below.
#[derive(Clone, Debug)]
pub enum Request {
    Fetch { query: String, attempt: u8 },
    Inspect { resource: String, attempt: u8 },
    Summarize { content: String, attempt: u8 },
    Retry { prior: Vec<u8>, attempt: u8 },
}
#[derive(Clone, Copy, Debug)]
pub struct Task(u32);
#[derive(Clone, Debug)]
pub enum Outcome {
    Success(Vec<u8>),
    Retry { reason: Vec<u8>, next_attempt: u8 },
    Failure(String),
}
#[derive(Clone, Debug)]
pub struct Evidence(pub Vec<u8>);
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Protocol(String),
    Host(String),
}

pub type Result<T> = std::result::Result<T, Error>;
pub struct Context {
    input: io::Stdin,
    output: io::Stdout,
    next_task: u32,
}

pub fn run<T>(workflow: impl FnOnce(&mut Context) -> Result<T>) -> Result<T> {
    let mut context = Context {
        input: io::stdin(),
        output: io::stdout(),
        next_task: 1,
    };
    workflow(&mut context)
}

impl Context {
    pub fn call(&mut self, request: Request) -> Result<Outcome> {
        let task = self.spawn(request)?;
        self.join(task)
    }

    pub fn spawn(&mut self, request: Request) -> Result<Task> {
        let task = Task(self.next_task);
        self.next_task = self
            .next_task
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("task id exhausted".into()))?;
        let (capability, attempt, input) = encode_request(request);
        let mut payload = Vec::with_capacity(10 + input.len());
        put_u32(&mut payload, task.0);
        payload.push(capability);
        payload.push(attempt);
        put_bytes(&mut payload, &input)?;
        self.exchange(SPAWN, &payload, ACK)?;
        Ok(task)
    }

    pub fn join(&mut self, task: Task) -> Result<Outcome> {
        let mut payload = Vec::new();
        put_u32(&mut payload, task.0);
        let payload = self.exchange(JOIN, &payload, OUTCOME)?;
        let mut cursor = Cursor::new(&payload);
        let status = cursor.byte()?;
        let next_attempt = if status == 1 { cursor.byte()? } else { 0 };
        let value = cursor.bytes()?.to_vec();
        cursor.finish()?;
        match status {
            0 => Ok(Outcome::Success(value)),
            1 => Ok(Outcome::Retry {
                reason: value,
                next_attempt,
            }),
            2 => String::from_utf8(value)
                .map(Outcome::Failure)
                .map_err(|_| Error::Protocol("host failure was not UTF-8".into())),
            _ => Err(Error::Protocol("unknown outcome tag".into())),
        }
    }

    pub fn budget(&mut self) -> Result<u32> {
        let payload = self.exchange(BUDGET, &[], BUDGET_RESULT)?;
        let mut cursor = Cursor::new(&payload);
        let remaining = cursor.u32()?;
        cursor.finish()?;
        Ok(remaining)
    }

    pub fn cancelled(&mut self) -> Result<bool> {
        let payload = self.exchange(CANCELLED, &[], CANCELLED_RESULT)?;
        match payload.as_slice() {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(Error::Protocol("invalid cancellation response".into())),
        }
    }

    pub fn finish(&mut self, evidence: Evidence) -> Result<()> {
        self.exchange(FINISH, &evidence.0, FINISHED).map(|_| ())
    }
}

// Private deterministic wire codec.
impl Context {
    fn exchange(&mut self, kind: u16, payload: &[u8], expected: u16) -> Result<Vec<u8>> {
        write_frame(&mut self.output.lock(), kind, payload)?;
        let (kind, payload) = read_frame(&mut self.input.lock())?;
        if kind == FAILURE {
            return Err(Error::Host(String::from_utf8_lossy(&payload).into_owned()));
        }
        if kind != expected {
            return Err(Error::Protocol(format!("unexpected response kind {kind}")));
        }
        Ok(payload)
    }
}

fn encode_request(request: Request) -> (u8, u8, Vec<u8>) {
    match request {
        Request::Fetch { query, attempt } => (1, attempt, query.into_bytes()),
        Request::Inspect { resource, attempt } => (2, attempt, resource.into_bytes()),
        Request::Summarize { content, attempt } => (3, attempt, content.into_bytes()),
        Request::Retry { prior, attempt } => (4, attempt, prior),
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "IPC error: {error}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Host(message) => write!(formatter, "host error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn write_frame(writer: &mut impl Write, kind: u16, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(Error::Protocol("frame too large".into()));
    }
    writer.write_all(&MAGIC.to_le_bytes())?;
    writer.write_all(&VERSION.to_le_bytes())?;
    writer.write_all(&kind.to_le_bytes())?;
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> Result<(u16, Vec<u8>)> {
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header)?;
    if u32::from_le_bytes([header[0], header[1], header[2], header[3]]) != MAGIC {
        return Err(Error::Protocol("bad frame magic".into()));
    }
    if u16::from_le_bytes([header[4], header[5]]) != VERSION {
        return Err(Error::Protocol("unsupported protocol version".into()));
    }
    let kind = u16::from_le_bytes([header[6], header[7]]);
    let length = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(Error::Protocol("declared frame too large".into()));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok((kind, payload))
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let length =
        u32::try_from(bytes.len()).map_err(|_| Error::Protocol("value too large".into()))?;
    put_u32(output, length);
    output.extend_from_slice(bytes);
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn byte(&mut self) -> Result<u8> {
        let value = *self
            .bytes
            .get(self.offset)
            .ok_or_else(|| Error::Protocol("truncated payload".into()))?;
        self.offset += 1;
        Ok(value)
    }
    fn u32(&mut self) -> Result<u32> {
        let end = self.offset + 4;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| Error::Protocol("truncated payload".into()))?;
        self.offset = end;
        Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
    }
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.u32()? as usize;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| Error::Protocol("length overflow".into()))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| Error::Protocol("truncated payload".into()))?;
        self.offset = end;
        Ok(value)
    }
    fn finish(&self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Protocol("trailing payload bytes".into()))
        }
    }
}
