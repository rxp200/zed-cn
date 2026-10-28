//! Local `ProxyCommand` helper that tunnels an SSH connection through an HTTP
//! CONNECT or SOCKS5 proxy.
//!
//! When an SSH host has a proxy configured, Zed points ssh's `ProxyCommand` at
//! itself with `--ssh-proxy <url> %h %p`. The helper performs the proxy
//! handshake and then copies bytes between its own stdio and the proxy socket,
//! so ssh sees an ordinary bidirectional stream.
//!
//! Everything here runs in a short-lived helper process, so it uses blocking
//! standard-library networking and never initializes GPUI.

use std::{
    io::{Read as _, Write as _},
    net::{Shutdown, TcpStream, ToSocketAddrs as _},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

/// Upper bound on the HTTP CONNECT response headers the helper will buffer.
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;

/// How long the helper waits for the proxy handshake before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Percent-encodes everything except RFC 3986 unreserved characters.
///
/// Credentials are embedded in the `ProxyCommand` string that ssh parses as a
/// shell-like command line, so they must never contain a raw space, quote or
/// shell metacharacter.
const CREDENTIAL_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyProtocol {
    /// SOCKS5, with the target host resolved locally.
    Socks5,
    /// SOCKS5, with the target host resolved by the proxy (`socks5h`).
    Socks5Hostname,
    /// HTTP CONNECT tunnel.
    HttpConnect,
}

impl ProxyProtocol {
    fn scheme(self) -> &'static str {
        match self {
            Self::Socks5 => "socks5",
            Self::Socks5Hostname => "socks5h",
            Self::HttpConnect => "http",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Socks5 | Self::Socks5Hostname => 1080,
            Self::HttpConnect => 80,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyEndpoint {
    pub protocol: ProxyProtocol,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyEndpoint {
    /// Parses and validates a proxy URL typed by the user.
    ///
    /// Only `socks5://`, `socks5h://` and `http://` are accepted. The returned
    /// endpoint is normalized, so callers never embed the raw user input in a
    /// command line.
    pub fn parse(url: &str) -> Result<Self> {
        let url = url.trim();
        if url.is_empty() {
            bail!(i18n::t!("0f1043d65a01f4ce"));
        }
        let parsed = url::Url::parse(url).context(i18n::t!("84c956292a993074"))?;

        if parsed.scheme() != "socks5" && parsed.scheme() != "socks5h" && parsed.scheme() != "http"
        {
            bail!(i18n::t_args!("1869095a6ed978ae", parsed.scheme()));
        }
        if !parsed.path().is_empty() && parsed.path() != "/" {
            bail!(i18n::t_args!("e48f9b7c70065b43", parsed.path()));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!(i18n::t!("66458507a7b79efe"));
        }

        let host = parsed
            .host_str()
            .filter(|host| !host.is_empty())
            .context(i18n::t!("23f47b2c605ab860"))?
            .to_string();
        validate_proxy_host(&host)?;

        let protocol = match parsed.scheme() {
            "socks5" => ProxyProtocol::Socks5,
            "socks5h" => ProxyProtocol::Socks5Hostname,
            _ => ProxyProtocol::HttpConnect,
        };
        let port = parsed.port().unwrap_or_else(|| protocol.default_port());
        if port == 0 {
            bail!(i18n::t!("8d871da4b285441b"));
        }

        let username = decode_userinfo(parsed.username())?;
        let password = match parsed.password() {
            Some(password) => decode_userinfo(password)?,
            None => None,
        };

        Ok(Self {
            protocol,
            host,
            port,
            username,
            password,
        })
    }

    /// Rebuilds the normalized URL embedded in the `ProxyCommand` value.
    pub fn canonical_url(&self) -> String {
        let mut url = String::new();
        url.push_str(self.protocol.scheme());
        url.push_str("://");
        if let Some(username) = &self.username {
            url.push_str(&utf8_percent_encode(username, CREDENTIAL_ENCODE_SET).to_string());
            if let Some(password) = &self.password {
                url.push(':');
                url.push_str(&utf8_percent_encode(password, CREDENTIAL_ENCODE_SET).to_string());
            }
            url.push('@');
        }
        url.push_str(&self.host);
        url.push(':');
        url.push_str(&self.port.to_string());
        url
    }

    /// Builds the `ProxyCommand` value handed to ssh.
    pub fn command(&self) -> Result<String> {
        let helper = helper_program()?;
        Ok(format!(
            "{} --ssh-proxy {} %h %p",
            quote_program(&helper),
            self.canonical_url()
        ))
    }

    fn connect(&self, target_host: &str, target_port: u16) -> Result<TcpStream> {
        let stream = TcpStream::connect((self.host.as_str(), self.port))
            .with_context(|| i18n::t_args!("16c6e165544048c7", self.host, self.port))?;
        if let Err(error) = stream.set_nodelay(true) {
            log::debug!("failed to set TCP_NODELAY on the SSH proxy socket: {error}");
        }
        stream
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .context(i18n::t!("23738924e7e1b57b"))?;
        stream
            .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
            .context(i18n::t!("23738924e7e1b57b"))?;

        match self.protocol {
            ProxyProtocol::Socks5 | ProxyProtocol::Socks5Hostname => {
                socks5_handshake(stream, self, target_host, target_port)
            }
            ProxyProtocol::HttpConnect => {
                http_connect_handshake(stream, self, target_host, target_port)
            }
        }
    }
}

/// Runs the `--ssh-proxy` helper mode: connects through the proxy and then
/// copies bytes between stdio and the proxy socket until either side closes.
pub fn run(proxy_url: &str, target_host: &str, target_port: &str) -> Result<()> {
    let endpoint = ProxyEndpoint::parse(proxy_url)?;
    let target_port: u16 = target_port
        .trim()
        .parse()
        .with_context(|| i18n::t!("75f732a512580c32", target_port = target_port))?;

    let stream = endpoint.connect(target_host, target_port)?;
    if let Err(error) = stream.set_read_timeout(None) {
        log::debug!("failed to clear the proxy handshake timeout: {error}");
    }
    if let Err(error) = stream.set_write_timeout(None) {
        log::debug!("failed to clear the proxy handshake timeout: {error}");
    }

    pipe_stdio(stream)
}

fn helper_program() -> Result<PathBuf> {
    crate::get_zed_cli_path().or_else(|error| {
        log::debug!("falling back to the running executable for the SSH proxy helper: {error:#}");
        std::env::current_exe().context(i18n::t!("02a796fd2ef80975"))
    })
}

fn quote_program(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        // Windows parses ProxyCommand with command-line rules, which honor
        // double quotes.
        format!("\"{}\"", path.replace('"', "\"\""))
    } else {
        // ssh runs ProxyCommand through a POSIX shell.
        if path
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/._-+".contains(character))
        {
            path.into_owned()
        } else {
            format!("'{}'", path.replace('\'', "'\\''"))
        }
    }
}

fn validate_proxy_host(host: &str) -> Result<()> {
    let valid = host.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':')
    });
    if !valid {
        bail!(i18n::t!("b426d7b4bfa14ca9", host = host));
    }
    Ok(())
}

/// Percent-decodes userinfo so [`ProxyEndpoint::canonical_url`] can re-encode
/// it with the stricter command-line-safe set instead of double-encoding the
/// URL parser's own escapes.
fn decode_userinfo(value: &str) -> Result<Option<String>> {
    if value.is_empty() {
        return Ok(None);
    }
    match percent_decode_str(value).decode_utf8() {
        Ok(value) => Ok(Some(value.into_owned())),
        Err(_) => bail!(i18n::t!("06174a4aab782931")),
    }
}

fn socks5_handshake(
    mut stream: TcpStream,
    endpoint: &ProxyEndpoint,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    let has_credentials = endpoint.username.is_some();
    let greeting: &[u8] = if has_credentials {
        &[0x05, 0x02, 0x00, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    stream
        .write_all(greeting)
        .context(i18n::t!("3817a84a141f7c56"))?;

    let mut selection = [0u8; 2];
    stream
        .read_exact(&mut selection)
        .context(i18n::t!("2a457147b84e0ac8"))?;
    if selection[0] != 0x05 {
        bail!(i18n::t_args!("244dcd560cf14f49", selection[0]));
    }
    match selection[1] {
        0x00 => {}
        0x02 => authenticate_socks5(&mut stream, endpoint)?,
        0xff => bail!(i18n::t!("8c51fe3699ece029")),
        method => bail!(i18n::t!("7adff409bae4e2bf", method = method)),
    }

    let mut request = vec![0x05, 0x01, 0x00];
    match endpoint.protocol {
        ProxyProtocol::Socks5Hostname => {
            if target_host.len() > u8::MAX as usize || !target_host.is_ascii() {
                bail!(i18n::t!("919231f106598ded", target_host = target_host));
            }
            request.push(0x03);
            request.push(target_host.len() as u8);
            request.extend_from_slice(target_host.as_bytes());
        }
        _ => {
            let address = (target_host, target_port)
                .to_socket_addrs()
                .with_context(|| i18n::t!("26e9d0a0c2692219", target_host = target_host))?
                .next()
                .with_context(|| i18n::t!("72a9251eac7c4321", target_host = target_host))?;
            match address {
                std::net::SocketAddr::V4(address) => {
                    request.push(0x01);
                    request.extend_from_slice(&address.ip().octets());
                }
                std::net::SocketAddr::V6(address) => {
                    request.push(0x04);
                    request.extend_from_slice(&address.ip().octets());
                }
            }
        }
    }
    request.extend_from_slice(&target_port.to_be_bytes());
    stream
        .write_all(&request)
        .context(i18n::t!("cea1a145cccc500f"))?;

    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .context(i18n::t!("34e4f901183471d5"))?;
    if header[0] != 0x05 {
        bail!(i18n::t_args!("244dcd560cf14f49", header[0]));
    }
    if header[1] != 0x00 {
        bail!(i18n::t_args!("c15caf825377b4f6", socks5_error(header[1])));
    }

    let mut skip = vec![
        0u8;
        match header[3] {
            0x01 => 4 + 2,
            0x04 => 16 + 2,
            0x03 => {
                let mut length = [0u8; 1];
                stream
                    .read_exact(&mut length)
                    .context(i18n::t!("d3bc19485883a80a"))?;
                length[0] as usize + 2
            }
            atyp => bail!(i18n::t!("b2ca98ef0a8e5c43", atyp = atyp)),
        }
    ];
    stream
        .read_exact(&mut skip)
        .context(i18n::t!("d3bc19485883a80a"))?;

    Ok(stream)
}

fn authenticate_socks5(stream: &mut TcpStream, endpoint: &ProxyEndpoint) -> Result<()> {
    let username = endpoint.username.clone().unwrap_or_default();
    let password = endpoint.password.clone().unwrap_or_default();
    if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
        bail!(i18n::t!("896925204fac0b0f"));
    }

    let mut request = vec![0x01, username.len() as u8];
    request.extend_from_slice(username.as_bytes());
    request.push(password.len() as u8);
    request.extend_from_slice(password.as_bytes());
    stream
        .write_all(&request)
        .context(i18n::t!("b85a3837a3562d47"))?;

    let mut response = [0u8; 2];
    stream
        .read_exact(&mut response)
        .context(i18n::t!("cb0c8fc7282c4e3d"))?;
    if response[1] != 0x00 {
        bail!(i18n::t!("d91741a7e857ab6e"));
    }
    Ok(())
}

fn socks5_error(code: u8) -> &'static str {
    match code {
        0x01 => i18n::t!("c52a2c85451cc429"),
        0x02 => i18n::t!("44978bd5dcd6140d"),
        0x03 => i18n::t!("314fabd4fdc3dace"),
        0x04 => i18n::t!("977c7c73f771153d"),
        0x05 => i18n::t!("3e13b80beb4bf2b6"),
        0x06 => i18n::t!("54d220a1c0946893"),
        0x07 => i18n::t!("6a9c930be875a49a"),
        0x08 => i18n::t!("7dff98142d9cbb6f"),
        _ => i18n::t!("13a46616e16fe5e6"),
    }
}

fn http_connect_handshake(
    mut stream: TcpStream,
    endpoint: &ProxyEndpoint,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    let authority = if target_host.contains(':') {
        format!("[{target_host}]:{target_port}")
    } else {
        format!("{target_host}:{target_port}")
    };

    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n"
    );
    if let Some(username) = &endpoint.username {
        let password = endpoint.password.clone().unwrap_or_default();
        request.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64(username.as_bytes(), password.as_bytes())
        ));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .context(i18n::t!("552cf73f546addad"))?;

    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .context(i18n::t!("4f01f9e1a03b1fa0"))?;
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > MAX_HTTP_HEADER_BYTES {
            bail!(i18n::t!("ce1e9fcb0c7f1002"));
        }
    }

    let headers = String::from_utf8_lossy(&headers).into_owned();
    let status_line = headers.lines().next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .with_context(|| i18n::t!("77e5ab643310e84e", status_line = status_line))?;
    if !(200..300).contains(&status) {
        bail!(i18n::t!("e9993e04f476ce46", status_line = status_line));
    }

    Ok(stream)
}

fn pipe_stdio(stream: TcpStream) -> Result<()> {
    let mut read_stream = stream.try_clone().context(i18n::t!("9cb12063dcb7f66f"))?;
    let mut write_stream = stream;

    let to_stdout = std::thread::spawn(move || -> Result<()> {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        std::io::copy(&mut read_stream, &mut stdout).context(i18n::t!("af098cf5e3d542e0"))?;
        stdout.flush().context(i18n::t!("df848a4c14e44334"))?;
        Ok(())
    });

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let copy_result =
        std::io::copy(&mut stdin, &mut write_stream).context(i18n::t!("cc125468c7393f7d"));
    if let Err(error) = write_stream.shutdown(Shutdown::Write) {
        log::debug!("failed to half-close the SSH proxy socket: {error}");
    }

    let stdout_result = to_stdout
        .join()
        .map_err(|_| anyhow::anyhow!(i18n::t!("75fcc1e847828f85")))?;

    copy_result?;
    stdout_result?;
    Ok(())
}

/// Standard base64 (RFC 4648) with padding, used for HTTP Basic auth.
fn base64(first: &[u8], second: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut input = Vec::with_capacity(first.len() + second.len() + 1);
    input.extend_from_slice(first);
    input.push(b':');
    input.extend_from_slice(second);

    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        output.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        output.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread};

    use super::*;

    #[test]
    fn parses_supported_proxy_urls() {
        let endpoint = ProxyEndpoint::parse("socks5://127.0.0.1:1080").unwrap();
        assert_eq!(endpoint.protocol, ProxyProtocol::Socks5);
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 1080);
        assert_eq!(endpoint.username, None);

        let endpoint = ProxyEndpoint::parse("socks5h://proxy.example.com").unwrap();
        assert_eq!(endpoint.protocol, ProxyProtocol::Socks5Hostname);
        assert_eq!(endpoint.port, 1080);

        let endpoint = ProxyEndpoint::parse("http://user:pass@proxy.example.com:3128/").unwrap();
        assert_eq!(endpoint.protocol, ProxyProtocol::HttpConnect);
        assert_eq!(endpoint.port, 3128);
        assert_eq!(endpoint.username.as_deref(), Some("user"));
        assert_eq!(endpoint.password.as_deref(), Some("pass"));
    }

    #[test]
    fn rejects_unsupported_or_malformed_proxy_urls() {
        for url in [
            "",
            "socks4://127.0.0.1:1080",
            "ftp://127.0.0.1:1080",
            "socks5://",
            "socks5://127.0.0.1:1080/extra",
            "socks5://127.0.0.1:0",
            "http://ho st:3128",
            "http://127.0.0.1:3128?token=1",
        ] {
            assert!(
                ProxyEndpoint::parse(url).is_err(),
                "expected {url:?} to fail"
            );
        }
    }

    #[test]
    fn canonical_url_encodes_credentials() {
        let endpoint = ProxyEndpoint::parse("socks5://us er:p@ss word@127.0.0.1:1080").unwrap();
        let canonical = endpoint.canonical_url();
        assert_eq!(canonical, "socks5://us%20er:p%40ss%20word@127.0.0.1:1080");

        let reparsed = ProxyEndpoint::parse(&canonical).unwrap();
        assert_eq!(reparsed.username.as_deref(), Some("us er"));
        assert_eq!(reparsed.password.as_deref(), Some("p@ss word"));
    }

    #[test]
    fn proxy_command_has_no_shell_metacharacters() {
        let endpoint =
            ProxyEndpoint::parse("http://user:p;ass$(x)`y`@proxy.example.com:8080").unwrap();
        assert_eq!(endpoint.username.as_deref(), Some("user"),);
        let command = endpoint.canonical_url();
        assert!(
            !command.contains(';') && !command.contains('$') && !command.contains('`'),
            "canonical URL must be shell-safe: {command}"
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b"", b""), "Og==");
        assert_eq!(base64(b"user", b"pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"a", b""), "YTo=");
    }

    fn spawn_socks5_proxy(expect_auth: bool) -> (u16, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut greeting = [0u8; 2];
            stream.read_exact(&mut greeting).unwrap();
            let mut methods = vec![0u8; greeting[1] as usize];
            stream.read_exact(&mut methods).unwrap();

            let selected = if expect_auth { 0x02 } else { 0x00 };
            stream.write_all(&[0x05, selected]).unwrap();

            if expect_auth {
                let mut head = [0u8; 2];
                stream.read_exact(&mut head).unwrap();
                let mut credentials = vec![0u8; head[1] as usize + 1];
                stream.read_exact(&mut credentials).unwrap();
                let mut password = vec![0u8; credentials[head[1] as usize] as usize];
                stream.read_exact(&mut password).unwrap();
                stream.write_all(&[0x01, 0x00]).unwrap();
            }

            let mut request = [0u8; 4];
            stream.read_exact(&mut request).unwrap();
            let mut rest = vec![0u8; 4 + 2];
            stream.read_exact(&mut rest).unwrap();
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .unwrap();

            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).unwrap();
            methods
        });
        (port, handle)
    }

    #[test]
    fn socks5_without_auth_connects() {
        let (port, handle) = spawn_socks5_proxy(false);
        let endpoint = ProxyEndpoint::parse(&format!("socks5://127.0.0.1:{port}")).unwrap();
        let mut stream = endpoint.connect("127.0.0.1", 22).unwrap();
        stream.write_all(b"ping").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        assert_eq!(handle.join().unwrap(), vec![0x00]);
    }

    #[test]
    fn socks5_with_username_password_authenticates() {
        let (port, handle) = spawn_socks5_proxy(true);
        let endpoint =
            ProxyEndpoint::parse(&format!("socks5://user:secret@127.0.0.1:{port}")).unwrap();
        endpoint.connect("127.0.0.1", 22).unwrap();
        assert_eq!(handle.join().unwrap(), vec![0x00, 0x02]);
    }

    #[test]
    fn http_connect_sends_basic_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while request.len() < MAX_HTTP_HEADER_BYTES {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).unwrap();
            String::from_utf8(request).unwrap()
        });

        let endpoint = ProxyEndpoint::parse(&format!("http://user:pass@127.0.0.1:{port}")).unwrap();
        let mut stream = endpoint.connect("target.example.com", 2222).unwrap();
        stream.write_all(b"ping").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();

        let request = handle.join().unwrap();
        assert!(request.starts_with("CONNECT target.example.com:2222 HTTP/1.1\r\n"));
        assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
    }
}
