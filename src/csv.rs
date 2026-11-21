//! The CSV dialect of rules.md: only `\n` ends a line, a field may be wrapped
//! in quotes as a whole, any other quote makes the line malformed, a line needs
//! exactly the expected number of fields, and an empty line is malformed.

use memchr::memchr;

/// Split `line` into exactly `N` unquoted fields. False if malformed.
#[inline(always)]
pub fn split<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    let n = line.len();
    if n == 0 {
        return false;
    }
    let mut f = 0usize;
    let mut start = 0usize;
    loop {
        if f == N {
            return false;
        }
        if start < n && line[start] == b'"' {
            let Some(p) = memchr(b'"', &line[start + 1..]) else {
                return false;
            };
            let close = start + 1 + p;
            out[f] = &line[start + 1..close];
            f += 1;
            if close + 1 == n {
                break;
            }
            if line[close + 1] != b',' {
                return false;
            }
            start = close + 2;
        } else {
            let mut j = start;
            while j < n {
                let b = line[j];
                if b == b',' {
                    break;
                }
                if b == b'"' {
                    return false;
                }
                j += 1;
            }
            out[f] = &line[start..j];
            f += 1;
            if j == n {
                break;
            }
            start = j + 1;
        }
    }
    f == N
}

/// Call `f` on every line of `data` (without its `\n`). Text after the last
/// `\n` is a line too.
#[inline(always)]
pub fn for_lines<'a>(data: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let mut start = 0;
    for end in memchr::memchr_iter(b'\n', data) {
        f(&data[start..end]);
        start = end + 1;
    }
    if start < data.len() {
        f(&data[start..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s<const N: usize>(l: &str) -> Option<Vec<String>> {
        let mut o = [&b""[..]; N];
        split::<N>(l.as_bytes(), &mut o)
            .then(|| o.iter().map(|x| String::from_utf8_lossy(x).into_owned()).collect())
    }
    #[test]
    fn dialect() {
        assert_eq!(s::<2>("a,b"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(s::<2>("\"a\",\"\""), Some(vec!["a".into(), "".into()]));
        assert_eq!(s::<2>("a,"), Some(vec!["a".into(), "".into()]));
        assert_eq!(s::<2>(",a"), Some(vec!["".into(), "a".into()]));
        assert_eq!(s::<2>("a,\""), None);
        assert_eq!(s::<2>("a,b\""), None);
        assert_eq!(s::<2>("\"a\"b,c"), None);
        assert_eq!(s::<2>("a,b,c"), None);
        assert_eq!(s::<2>("a"), None);
        assert_eq!(s::<2>(""), None);
        assert_eq!(s::<1>(""), None);
        assert_eq!(s::<2>(" a,b \r"), Some(vec![" a".into(), "b \r".into()]));
        assert_eq!(s::<2>("\"a,b"), None);
    }
}
