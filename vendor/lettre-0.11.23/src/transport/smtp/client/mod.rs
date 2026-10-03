//! SMTP client
//!
//! `SmtpConnection` allows manually sending SMTP commands.
//!
//! ```rust,no_run
//! # use std::error::Error;
//!
//! # #[cfg(feature = "smtp-transport")]
//! # fn main() -> Result<(), Box<dyn Error>> {
//! use lettre::transport::smtp::{
//!     SMTP_PORT, client::SmtpConnection, commands::*, extension::ClientId,
//! };
//!
//! let hello = ClientId::Domain("my_hostname".to_owned());
//! let mut client = SmtpConnection::connect(&("localhost", SMTP_PORT), None, &hello, None, None)?;
//! client.command(Mail::new(Some("user@example.com".parse()?), vec![]))?;
//! client.command(Rcpt::new("user@example.org".parse()?, vec![]))?;
//! client.command(Data)?;
//! client.message("Test email".as_bytes())?;
//! client.command(Quit)?;
//! # Ok(())
//! # }
//! ```

#[cfg(feature = "serde")]
use std::fmt::Debug;

#[cfg(any(feature = "tokio1", feature = "async-std1"))]
pub use self::async_connection::AsyncSmtpConnection;
#[cfg(any(feature = "tokio1", feature = "async-std1"))]
#[allow(deprecated)]
pub use self::async_net::AsyncNetworkStream;
#[cfg(feature = "tokio1")]
pub use self::async_net::AsyncTokioStream;
use self::net::NetworkStream;
#[cfg(any(feature = "native-tls", feature = "rustls", feature = "boring-tls"))]
pub(super) use self::tls::InnerTlsParameters;
#[cfg(any(feature = "native-tls", feature = "rustls", feature = "boring-tls"))]
pub use self::tls::TlsVersion;
pub use self::{
    connection::SmtpConnection,
    tls::{Certificate, CertificateStore, Identity, Tls, TlsParameters, TlsParametersBuilder},
};

#[cfg(any(feature = "tokio1", feature = "async-std1"))]
mod async_connection;
#[cfg(any(feature = "tokio1", feature = "async-std1"))]
mod async_net;
mod connection;
mod net;
mod response_reader {
    use std::io;

    use super::{MAX_RESPONSE_BYTES, MAX_RESPONSE_LINE_BYTES};
    use crate::transport::smtp::{error, error::Error};

    /// Accumulates one line without retaining bytes beyond either response cap.
    pub(super) struct ResponseLine {
        bytes: Vec<u8>,
        response_bytes: usize,
    }

    impl ResponseLine {
        pub(super) fn new(response_bytes: usize) -> Self {
            Self {
                bytes: Vec::new(),
                response_bytes,
            }
        }

        pub(super) fn extend(&mut self, available: &[u8]) -> Result<(usize, bool), Error> {
            let newline = available.iter().position(|&byte| byte == b'\n');
            let consumed = newline.map_or(available.len(), |position| position + 1);
            if consumed > MAX_RESPONSE_LINE_BYTES - self.bytes.len() {
                return Err(error::response("SMTP response line too long"));
            }
            if consumed > MAX_RESPONSE_BYTES - self.response_bytes - self.bytes.len() {
                return Err(error::response("SMTP response too large"));
            }
            self.bytes.extend_from_slice(&available[..consumed]);
            Ok((consumed, newline.is_some()))
        }

        pub(super) fn finish(self, buffer: &mut String) -> Result<usize, Error> {
            // Validate after the line is assembled: reads can split UTF-8 code points.
            let line = std::str::from_utf8(&self.bytes).map_err(|error| {
                error::network(io::Error::new(io::ErrorKind::InvalidData, error))
            })?;
            buffer.push_str(line);
            Ok(self.bytes.len())
        }
    }

    #[cfg(test)]
    pub(super) mod tests {
        use std::{cell::Cell, io, rc::Rc};

        use super::*;
        use crate::transport::smtp::response::Response;

        pub(crate) enum Expected {
            Positive(u16, Vec<String>),
            Transient,
            Permanent,
            ResponseError(&'static str),
            NetworkError,
        }

        pub(crate) fn cases() -> Vec<(Vec<u8>, Expected)> {
            let exact_line = format!("250 {}\r\n", "x".repeat(MAX_RESPONSE_LINE_BYTES - 6));
            let continuation = format!("250-{}\r\n", "x".repeat(MAX_RESPONSE_LINE_BYTES - 6));
            let exact_total = format!("{}{}", continuation.repeat(99), exact_line);
            vec![
                (
                    b"250 hello\r\n".to_vec(),
                    Expected::Positive(250, vec!["hello".into()]),
                ),
                (
                    "250-héllo\r\n250 世界\r\n".as_bytes().to_vec(),
                    Expected::Positive(250, vec!["héllo".into(), "世界".into()]),
                ),
                (
                    exact_line.into_bytes(),
                    Expected::Positive(250, vec!["x".repeat(994)]),
                ),
                (
                    exact_total.into_bytes(),
                    Expected::Positive(250, vec!["x".repeat(994); 100]),
                ),
                (
                    format!("250 {}\r\n", "x".repeat(995)).into_bytes(),
                    Expected::ResponseError("SMTP response line too long"),
                ),
                (
                    format!("{}250 ok\r\n", continuation.repeat(100)).into_bytes(),
                    Expected::ResponseError("SMTP response too large"),
                ),
                (
                    b"250-partial\r\n".to_vec(),
                    Expected::ResponseError("incomplete response"),
                ),
                (
                    b"250 partial".to_vec(),
                    Expected::ResponseError("incomplete response"),
                ),
                (vec![], Expected::ResponseError("incomplete response")),
                (b"550 rejected\r\n".to_vec(), Expected::Permanent),
                (b"451 later\r\n".to_vec(), Expected::Transient),
                (b"invalid\r\n".to_vec(), Expected::ResponseError("")),
                (b"250 \xff\r\n".to_vec(), Expected::NetworkError),
                (b"250 \xc3".to_vec(), Expected::NetworkError),
            ]
        }

        pub(crate) fn assert_expected(result: Result<Response, Error>, expected: Expected) {
            match expected {
                Expected::Positive(code, lines) => {
                    let response = result.unwrap();
                    assert!(response.has_code(code));
                    assert_eq!(response.message().collect::<Vec<_>>(), lines);
                }
                Expected::Transient => assert!(result.unwrap_err().is_transient()),
                Expected::Permanent => assert!(result.unwrap_err().is_permanent()),
                Expected::ResponseError(message) => {
                    let error = result.unwrap_err();
                    assert!(error.is_response(), "{error:?}");
                    assert!(error.to_string().contains(message), "{error:?}");
                }
                Expected::NetworkError => {
                    let error = result.unwrap_err();
                    assert!(!error.is_response(), "{error:?}");
                    let source = std::error::Error::source(&error).unwrap();
                    assert_eq!(
                        source.downcast_ref::<io::Error>().unwrap().kind(),
                        io::ErrorKind::InvalidData
                    );
                }
            }
        }

        // Fixed read budget makes the regression fail promptly on the original reader,
        // instead of consuming unbounded memory or waiting forever for a newline.
        pub(crate) struct NoNewlinePeer {
            pub(crate) delivered: Rc<Cell<usize>>,
        }

        impl io::Read for NoNewlinePeer {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = output.len().min(64);
                assert!(
                    self.delivered.get() + count <= 1088,
                    "reader passed its bounded byte budget"
                );
                output[..count].fill(b'x');
                self.delivered.set(self.delivered.get() + count);
                Ok(count)
            }
        }

        #[cfg(any(feature = "tokio1", feature = "async-std1"))]
        impl futures_io::AsyncRead for NoNewlinePeer {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                output: &mut [u8],
            ) -> std::task::Poll<io::Result<usize>> {
                std::task::Poll::Ready(io::Read::read(&mut *self, output))
            }
        }

        pub(crate) struct FailingPeer;

        impl io::Read for FailingPeer {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::ConnectionReset, "test reset"))
            }
        }

        #[cfg(any(feature = "tokio1", feature = "async-std1"))]
        impl futures_io::AsyncRead for FailingPeer {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                output: &mut [u8],
            ) -> std::task::Poll<io::Result<usize>> {
                std::task::Poll::Ready(io::Read::read(&mut *self, output))
            }
        }

        #[test]
        fn rejected_bytes_are_never_retained() {
            let mut line = ResponseLine::new(0);
            line.extend(&vec![b'x'; MAX_RESPONSE_LINE_BYTES]).unwrap();
            assert!(line.extend(b"x").is_err());
            assert_eq!(line.bytes.len(), MAX_RESPONSE_LINE_BYTES);
            let mut line = ResponseLine::new(MAX_RESPONSE_BYTES - 1);
            line.extend(b"x").unwrap();
            assert!(line.extend(b"x").is_err());
            assert_eq!(line.bytes.len(), 1);
        }
    }
}
mod tls;

/// Total bytes cap on an SMTP response (Postfix `smtp_response_limit`).
pub(super) const MAX_RESPONSE_BYTES: usize = 100_000;

/// Single-line byte cap (Postfix `line_length_limit`).
pub(super) const MAX_RESPONSE_LINE_BYTES: usize = 1000;

/// The codec used for transparency
#[derive(Debug)]
struct ClientCodec {
    status: CodecStatus,
}

impl ClientCodec {
    /// Creates a new client codec
    pub(crate) fn new() -> Self {
        Self {
            status: CodecStatus::StartOfNewLine,
        }
    }

    /// Adds transparency
    fn encode(&mut self, frame: &[u8], buf: &mut Vec<u8>) {
        for &b in frame {
            buf.push(b);
            match (b, self.status) {
                (b'\r', _) => {
                    self.status = CodecStatus::StartingNewLine;
                }
                (b'\n', CodecStatus::StartingNewLine) => {
                    self.status = CodecStatus::StartOfNewLine;
                }
                (_, CodecStatus::StartingNewLine) => {
                    self.status = CodecStatus::MiddleOfLine;
                }
                (b'.', CodecStatus::StartOfNewLine) => {
                    self.status = CodecStatus::MiddleOfLine;
                    buf.push(b'.');
                }
                (_, CodecStatus::StartOfNewLine) => {
                    self.status = CodecStatus::MiddleOfLine;
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug, Copy, Clone)]
#[allow(clippy::enum_variant_names)]
enum CodecStatus {
    /// We are past the first character of the current line
    MiddleOfLine,
    /// We just read a `\r` character
    StartingNewLine,
    /// We are at the start of a new line
    StartOfNewLine,
}

/// Returns the string replacing all the CRLF with "\<CRLF\>"
/// Used for debug displays
#[cfg(feature = "tracing")]
pub(super) fn escape_crlf(string: &str) -> String {
    string.replace("\r\n", "<CRLF>")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_codec() {
        let mut buf = Vec::new();
        let mut codec = ClientCodec::new();

        codec.encode(b".\r\n", &mut buf);
        codec.encode(b"test\r\n", &mut buf);
        codec.encode(b"test\r\n\r\n", &mut buf);
        codec.encode(b".\r\n", &mut buf);
        codec.encode(b"\r\ntest", &mut buf);
        codec.encode(b"te\r\n.\r\nst", &mut buf);
        codec.encode(b"test", &mut buf);
        codec.encode(b"test.", &mut buf);
        codec.encode(b"test\n", &mut buf);
        codec.encode(b".test\n", &mut buf);
        codec.encode(b"test", &mut buf);
        codec.encode(b"test", &mut buf);
        codec.encode(b"test\r\n", &mut buf);
        codec.encode(b".test\r\n", &mut buf);
        codec.encode(b"test.\r\n", &mut buf);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "..\r\ntest\r\ntest\r\n\r\n..\r\n\r\ntestte\r\n..\r\nsttesttest.test\n.test\ntesttesttest\r\n..test\r\ntest.\r\n"
        );
    }

    #[test]
    #[cfg(feature = "tracing")]
    fn test_escape_crlf() {
        assert_eq!(escape_crlf("\r\n"), "<CRLF>");
        assert_eq!(escape_crlf("EHLO my_name\r\n"), "EHLO my_name<CRLF>");
        assert_eq!(
            escape_crlf("EHLO my_name\r\nSIZE 42\r\n"),
            "EHLO my_name<CRLF>SIZE 42<CRLF>"
        );
    }
}
