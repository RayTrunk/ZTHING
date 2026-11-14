//! The optional HTTP API: `GET /search` and `GET /status` on 127.0.0.1.
//!
//! It runs in the window process rather than in the service because a request
//! carries no Windows identity. Passed on through the pipe it gets the rights
//! of the logged-on user, so the service's access filter still applies.

use tiny_http::{Header, Method, Request, Response, Server};
use zthing_client::{Client, ClientError, Field, PROTOCOL_VERSION, Search, Sort};

/// Binds the port and serves requests on a thread of its own until the process ends.
pub fn spawn(port: u16, token: String) -> Result<(), String> {
    if token.is_empty() {
        return Err("http.token is empty".to_owned());
    }
    let server = Server::http(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut client: Option<Client> = None;
        for request in server.incoming_requests() {
            let (status, body) = answer(&request, &token, &mut client);
            let mut response = Response::from_string(body).with_status_code(status);
            if let Ok(header) = Header::from_bytes(
                &b"Content-Type"[..],
                &b"application/json; charset=utf-8"[..],
            ) {
                response.add_header(header);
            }
            // The caller may have gone away, which only concerns that request.
            let _ = request.respond(response);
        }
    });
    Ok(())
}

fn answer(request: &Request, token: &str, client: &mut Option<Client>) -> (u16, String) {
    if request.method() != &Method::Get {
        return failure(405, "only GET is supported");
    }
    let url = request.url();
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let params: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect();
    let param = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let bearer = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .and_then(|header| header.value.as_str().strip_prefix("Bearer "));
    if !bearer
        .or(param("token"))
        .is_some_and(|given| same(given, token))
    {
        return failure(401, "missing or wrong token");
    }

    let search = match path {
        "/status" => None,
        "/search" => match parse_search(param) {
            Ok(search) => Some(search),
            Err(message) => return failure(400, &message),
        },
        _ => return failure(404, "unknown endpoint, use /search or /status"),
    };
    let connected = match client {
        Some(connected) => connected,
        None => match Client::connect() {
            Ok(connected) => client.insert(connected),
            Err(e) => return failure(503, &e.to_string()),
        },
    };
    let mut response = zthing_client::Response {
        v: PROTOCOL_VERSION,
        ok: true,
        ..zthing_client::Response::default()
    };
    let result = match search {
        Some(search) => connected.search(search).map(|page| {
            response.total = Some(page.total);
            response.millis = Some(page.millis);
            response.results = Some(page.hits);
        }),
        None => connected
            .status()
            .map(|status| response.status = Some(status)),
    };
    match result {
        Ok(()) => (200, serde_json::to_string(&response).unwrap_or_default()),
        Err(ClientError::Service(message)) => failure(400, &message),
        Err(e) => {
            *client = None;
            failure(503, &e.to_string())
        }
    }
}

fn parse_search<'a>(param: impl Fn(&str) -> Option<&'a str>) -> Result<Search, String> {
    let number = |name: &str, default: u64| {
        param(name).map_or(Ok(default), |value| {
            value
                .parse()
                .map_err(|_| format!("{name} must be a number, got {value:?}"))
        })
    };
    let mut search = Search {
        query: param("q").ok_or("parameter q is missing")?.to_owned(),
        descending: matches!(param("descending"), Some("1" | "true")),
        offset: number("offset", 0)?,
        limit: number("limit", zthing_client::DEFAULT_LIMIT)?,
        ..Search::default()
    };
    search.sort = match param("sort") {
        None | Some("name") => Sort::Name,
        Some("path") => Sort::Path,
        Some("size") => Sort::Size,
        Some("modified") => Sort::Modified,
        Some(other) => {
            return Err(format!(
                "sort must be name, path, size or modified, got {other:?}"
            ));
        }
    };
    if let Some(fields) = param("fields") {
        search.fields = fields
            .split(',')
            .map(|field| match field {
                "name" => Ok(Field::Name),
                "path" => Ok(Field::Path),
                "size" => Ok(Field::Size),
                "modified" => Ok(Field::Modified),
                "folder" => Ok(Field::Folder),
                other => Err(format!("unknown field {other:?}")),
            })
            .collect::<Result<_, _>>()?;
    }
    Ok(search)
}

fn failure(status: u16, message: &str) -> (u16, String) {
    let response = zthing_client::Response {
        v: PROTOCOL_VERSION,
        error: Some(message.to_owned()),
        ..zthing_client::Response::default()
    };
    (status, serde_json::to_string(&response).unwrap_or_default())
}

/// Compares in time independent of where the strings differ, so response times
/// do not reveal how much of a guessed token was right.
fn same(given: &str, token: &str) -> bool {
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// Percent-decodes a query string component, `+` included.
pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = bytes
            .get(i + 1..i + 3)
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[i], escaped) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_components_are_percent_decoded() {
        assert_eq!(decode("ext%3Apdf+%C3%84rger"), "ext:pdf Ärger");
        // Broken escapes stay as they are.
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn tokens_must_match_exactly() {
        assert!(same("geheim", "geheim"));
        assert!(!same("geheiM", "geheim"));
        assert!(!same("geheim1", "geheim"));
        assert!(!same("", "geheim"));
    }
}
