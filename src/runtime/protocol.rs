use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use anyhow::{Result, anyhow, bail};
use nix::sys::signal::{Signal, kill as send_signal};
use nix::unistd::Pid;

pub(super) fn detach_sequence(detach_key: &str) -> Result<Vec<u8>> {
    let tokens = detach_key.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        bail!("detach key cannot be empty");
    }
    let mut bytes = Vec::with_capacity(tokens.len());
    for token in tokens {
        bytes.push(parse_detach_token(token)?);
    }
    Ok(bytes)
}

fn parse_detach_token(token: &str) -> Result<u8> {
    if let Some(control) = token.strip_prefix("C-") {
        let mut chars = control.chars();
        let ch = chars
            .next()
            .ok_or_else(|| anyhow!("invalid control key token: {token}"))?;
        if chars.next().is_some() || !ch.is_ascii() {
            bail!("invalid control key token: {token}");
        }
        let lower = ch.to_ascii_lowercase() as u8;
        return Ok(lower & 0x1f);
    }

    let mut chars = token.chars();
    let ch = chars
        .next()
        .ok_or_else(|| anyhow!("invalid detach key token: {token}"))?;
    if chars.next().is_some() || !ch.is_ascii() {
        bail!("invalid detach key token: {token}");
    }
    Ok(ch as u8)
}

pub(super) fn parse_signal(raw: &str) -> Result<Signal> {
    match raw.to_ascii_uppercase().as_str() {
        "INT" | "SIGINT" => Ok(Signal::SIGINT),
        "TERM" | "SIGTERM" => Ok(Signal::SIGTERM),
        "KILL" | "SIGKILL" => Ok(Signal::SIGKILL),
        "HUP" | "SIGHUP" => Ok(Signal::SIGHUP),
        other => bail!("unsupported signal: {other}"),
    }
}

pub(super) fn attach_input_loop(stream: &mut UnixStream, detach_sequence: &[u8]) -> Result<()> {
    let mut stdin = io::stdin();
    attach_input_loop_from(&mut stdin, stream, detach_sequence)
}

fn attach_input_loop_from(
    reader: &mut impl Read,
    stream: &mut UnixStream,
    detach_sequence: &[u8],
) -> Result<()> {
    let mut pending = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        pending.push(buf[0]);
        if pending == detach_sequence {
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        if detach_sequence.starts_with(&pending) {
            continue;
        }
        if let Err(err) = stream.write_all(&pending) {
            if is_peer_closed(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        if let Err(err) = stream.flush() {
            if is_peer_closed(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        pending.clear();
    }
}

pub(super) fn signal_process_group(pid: u32, signal: Signal) -> Result<()> {
    send_signal(Pid::from_raw(-(pid as i32)), signal)?;
    Ok(())
}

pub(super) fn is_peer_closed(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn detach_and_signal_parsers_cover_error_cases() {
        assert_eq!(detach_sequence("C-a d").unwrap(), vec![0x01, b'd']);
        assert_eq!(parse_signal("HUP").unwrap(), Signal::SIGHUP);
        assert_eq!(parse_signal("KILL").unwrap(), Signal::SIGKILL);
        assert!(detach_sequence("").is_err());
        assert!(parse_detach_token("C-ab").is_err());
        assert!(parse_detach_token("xy").is_err());
        assert!(parse_signal("BOGUS").is_err());
    }

    #[test]
    fn is_peer_closed_matches_expected_kinds() {
        assert!(is_peer_closed(&io::Error::new(
            io::ErrorKind::BrokenPipe,
            "x"
        )));
        assert!(is_peer_closed(&io::Error::new(
            io::ErrorKind::ConnectionReset,
            "x"
        )));
        assert!(!is_peer_closed(&io::Error::new(io::ErrorKind::Other, "x")));
    }

    #[test]
    fn attach_input_loop_handles_eof_and_peer_closed() {
        let (mut stream, peer) = UnixStream::pair().unwrap();
        let mut empty = Cursor::new(Vec::<u8>::new());
        attach_input_loop_from(&mut empty, &mut stream, b"\x02d").unwrap();
        let mut buf = [0u8; 1];
        let mut peer = peer;
        assert_eq!(peer.read(&mut buf).unwrap(), 0);

        let (mut stream, peer) = UnixStream::pair().unwrap();
        peer.shutdown(Shutdown::Read).unwrap();
        let mut reader = Cursor::new(vec![b'x']);
        attach_input_loop_from(&mut reader, &mut stream, b"\x02d").unwrap();
    }
}
