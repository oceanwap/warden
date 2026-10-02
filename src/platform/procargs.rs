//! The buffer macOS's `sysctl KERN_PROCARGS2` returns for a process: the
//! arguments and environment it was started with. Pure parsing, so it is
//! tested on every OS.
//!
//! Layout (from XNU's `sysctl_procargsx`, what `ps -E` reads):
//!
//! ```text
//! argc            native-endian int32
//! exec_path\0     the path passed to exec
//! \0 \0 …         padding up to the first argument
//! argv[0]\0 … argv[argc-1]\0
//! env[0]\0 env[1]\0 …   `NAME=value`, ended by an empty string or the end
//! (then the "apple" strings, which this does not read)
//! ```

use super::Environ;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

/// What the kernel kept of one process's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcArgs {
    pub exec_path: OsString,
    pub args: Vec<OsString>,
    pub env: Environ,
}

/// `None` when the buffer is not in that layout (too short, no argc).
pub(crate) fn parse_procargs(buf: &[u8]) -> Option<ProcArgs> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let mut rest = &buf[4..];

    // The next NUL-terminated string (the last one may lack its NUL).
    let take = |rest: &mut &[u8]| -> Option<Vec<u8>> {
        if rest.is_empty() {
            return None;
        }
        let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        let s = rest[..end].to_vec();
        *rest = &rest[(end + 1).min(rest.len())..];
        Some(s)
    };

    let exec_path = OsString::from_vec(take(&mut rest)?);
    // The padding: NULs between the path and argv[0].
    while rest.first() == Some(&0) {
        rest = &rest[1..];
    }
    let mut args = Vec::with_capacity(argc.min(4096));
    for _ in 0..argc {
        args.push(OsString::from_vec(take(&mut rest)?));
    }
    let mut env = Vec::new();
    while let Some(kv) = take(&mut rest) {
        if kv.is_empty() {
            break; // the end of the environment
        }
        if let Some(i) = kv.iter().position(|b| *b == b'=') {
            env.push((OsString::from_vec(kv[..i].to_vec()), OsString::from_vec(kv[i + 1..].to_vec())));
        }
    }
    Some(ProcArgs { exec_path, args, env })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(argc: i32, exec: &str, pad: usize, args: &[&str], env: &[&str], apple: &[&str]) -> Vec<u8> {
        let mut b = argc.to_ne_bytes().to_vec();
        b.extend(exec.as_bytes());
        b.push(0);
        b.extend(std::iter::repeat_n(0u8, pad));
        for a in args {
            b.extend(a.as_bytes());
            b.push(0);
        }
        for e in env {
            b.extend(e.as_bytes());
            b.push(0);
        }
        b.push(0); // the empty string ending the environment
        for a in apple {
            b.extend(a.as_bytes());
            b.push(0);
        }
        b
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn parses_argv_and_environment_and_stops_before_the_apple_strings() {
        let b = buf(
            2,
            "/usr/local/bin/bun",
            5,
            &["bun", "run server.ts"],
            &["PATH=/usr/bin:/bin", "GREETING=two words", "EMPTY=", "A=b=c"],
            &["executable_path=/usr/local/bin/bun", "ptr_munge=0x1"],
        );
        let p = parse_procargs(&b).unwrap();
        assert_eq!(p.exec_path, os("/usr/local/bin/bun"));
        assert_eq!(p.args, [os("bun"), os("run server.ts")]);
        assert_eq!(
            p.env,
            [
                (os("PATH"), os("/usr/bin:/bin")),
                (os("GREETING"), os("two words")),
                (os("EMPTY"), os("")),
                (os("A"), os("b=c")), // the first `=` splits
            ]
        );
    }

    #[test]
    fn an_empty_argument_does_not_end_argv_and_no_environment_is_empty() {
        let b = buf(3, "/bin/echo", 3, &["echo", "", "x"], &[], &[]);
        let p = parse_procargs(&b).unwrap();
        assert_eq!(p.args, [os("echo"), os(""), os("x")]);
        assert!(p.env.is_empty());
        // Zero arguments (rare): the first string after the padding starts the environment.
        let b = buf(0, "/bin/x", 0, &[], &["A=1"], &[]);
        assert_eq!(parse_procargs(&b).unwrap().env, [(os("A"), os("1"))]);
    }

    #[test]
    fn a_buffer_without_its_layout_is_none_not_a_panic() {
        assert_eq!(parse_procargs(&[]), None);
        assert_eq!(parse_procargs(&[1, 0, 0]), None);
        assert_eq!(parse_procargs(&(-1i32).to_ne_bytes()), None, "negative argc");
        assert_eq!(parse_procargs(&2i32.to_ne_bytes()), None, "no path");
        // argc says 3 arguments, the buffer has one.
        let mut b = 3i32.to_ne_bytes().to_vec();
        b.extend(b"/bin/x\0\0argv0\0");
        assert_eq!(parse_procargs(&b), None);
        // Truncated in the environment: what was read so far is kept.
        let mut b = buf(1, "/bin/x", 1, &["x"], &[], &[]);
        b.truncate(b.len() - 1);
        b.extend(b"K=v");
        let p = parse_procargs(&b).unwrap();
        assert_eq!(p.env, [(os("K"), os("v"))], "the last string may lack its NUL");
    }

    #[test]
    fn non_utf8_bytes_survive() {
        let mut b = 1i32.to_ne_bytes().to_vec();
        b.extend(b"/bin/x\0\0x\0K=\xff\xfe\0\0");
        let p = parse_procargs(&b).unwrap();
        assert_eq!(p.env[0].1, OsString::from_vec(vec![0xff, 0xfe]));
    }
}
