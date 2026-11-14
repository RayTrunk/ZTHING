//! Client for the ZTHING service. Requests and responses are JSON Lines on
//! the named pipe [`PIPE_NAME`]; `docs/API.md` describes the protocol.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\zthing";
/// Raised whenever a command or field changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;
/// Results per page unless a request asks for another limit.
pub const DEFAULT_LIMIT: u64 = 100;
/// `ERROR_PIPE_BUSY`: every pipe instance is taken for a moment.
const ERROR_PIPE_BUSY: i32 = 231;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Command {
    Search(Search),
    Count { query: String },
    Status,
    Version,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Search {
    pub query: String,
    pub sort: Sort,
    pub descending: bool,
    pub offset: u64,
    pub limit: u64,
    pub fields: Vec<Field>,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            query: String::new(),
            sort: Sort::Name,
            descending: false,
            offset: 0,
            limit: DEFAULT_LIMIT,
            fields: vec![
                Field::Name,
                Field::Path,
                Field::Size,
                Field::Modified,
                Field::Folder,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Name,
    Path,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Field {
    Name,
    /// Full path including the name.
    Path,
    Size,
    /// Unix seconds (UTC).
    Modified,
    Folder,
}

/// One flat object for every command. Fields a command does not produce are absent.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Matches of `search` and `count`, before paging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub millis: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<Hit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
}

/// Holds the fields the request asked for.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// False while the service reads the drives for the first time.
    pub ready: bool,
    /// Changes whenever the index changes, so clients know when to search again.
    pub index_version: u64,
    pub entries: u64,
    pub volumes: Vec<VolumeStatus>,
    /// Skipped drives and errors.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub drive: String,
    /// True if the USN journal keeps this volume current.
    pub live: bool,
}

pub struct Page {
    pub total: u64,
    pub millis: f64,
    pub hits: Vec<Hit>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the ZTHING service is not running ({PIPE_NAME}: {0})")]
    Connect(io::Error),
    #[error("talking to the ZTHING service: {0}")]
    Io(#[from] io::Error),
    #[error("unexpected answer from the ZTHING service: {0}")]
    Protocol(String),
    #[error("{0}")]
    Service(String),
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(e.to_string())
    }
}

pub struct Client {
    reader: BufReader<File>,
    writer: File,
    next_id: u64,
    line: String,
}

impl Client {
    pub fn connect() -> Result<Self, ClientError> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let pipe = loop {
            match OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
                Ok(pipe) => break pipe,
                // The service creates the next instance right after accepting a client.
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(ClientError::Connect(e)),
            }
        };
        Ok(Self {
            writer: pipe.try_clone()?,
            reader: BufReader::new(pipe),
            next_id: 1,
            line: String::new(),
        })
    }

    pub fn search(&mut self, search: Search) -> Result<Page, ClientError> {
        let response = self.call(Command::Search(search))?;
        Ok(Page {
            total: response.total.unwrap_or(0),
            millis: response.millis.unwrap_or(0.0),
            hits: response.results.unwrap_or_default(),
        })
    }

    pub fn count(&mut self, query: &str) -> Result<u64, ClientError> {
        let response = self.call(Command::Count {
            query: query.to_owned(),
        })?;
        Ok(response.total.unwrap_or(0))
    }

    pub fn status(&mut self) -> Result<Status, ClientError> {
        self.call(Command::Status)?
            .status
            .ok_or_else(|| ClientError::Protocol("status without status".to_owned()))
    }

    /// Service version and protocol version.
    pub fn version(&mut self) -> Result<(String, u32), ClientError> {
        let response = self.call(Command::Version)?;
        Ok((
            response.version.unwrap_or_default(),
            response.protocol.unwrap_or(0),
        ))
    }

    pub fn call(&mut self, command: Command) -> Result<Response, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = serde_json::to_vec(&Request {
            v: PROTOCOL_VERSION,
            id,
            command,
        })?;
        request.push(b'\n');
        self.writer.write_all(&request)?;
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            return Err(ClientError::Protocol(
                "the service closed the connection".to_owned(),
            ));
        }
        let response: Response = serde_json::from_str(&self.line)?;
        if response.id != id {
            return Err(ClientError::Protocol(format!(
                "answer to request {} instead of {id}",
                response.id
            )));
        }
        if !response.ok {
            return Err(ClientError::Service(response.error.unwrap_or_default()));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_requests_fill_in_defaults() {
        let request: Request =
            serde_json::from_str(r#"{"v":1,"id":3,"cmd":"search","query":"foo"}"#).unwrap();
        assert_eq!(
            request.command,
            Command::Search(Search {
                query: "foo".to_owned(),
                ..Search::default()
            })
        );
    }

    #[test]
    fn requests_and_responses_use_the_documented_shape() {
        let request = Request {
            v: PROTOCOL_VERSION,
            id: 9,
            command: Command::Count {
                query: "x".to_owned(),
            },
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"v":1,"id":9,"cmd":"count","query":"x"}"#
        );
        let response = Response {
            v: 1,
            id: 9,
            ok: true,
            total: Some(2),
            ..Response::default()
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"v":1,"id":9,"ok":true,"total":2}"#
        );
    }
}
