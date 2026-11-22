//! The CSV dialect of rules.md: only `\n` ends a line, every comma separates
//! fields, a quote may only wrap a whole field, an empty field is null.

use memchr::memchr;

/// Splits `line` into exactly N unquoted fields (an empty slice is null).
/// Returns false for a malformed line.
#[inline]
pub fn fields<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    if line.is_empty() {
        return false;
    }
    let mut start = 0usize;
    let mut k = 0usize;
    let mut quoted = false;
    for (i, &c) in line.iter().enumerate() {
        if c == b',' {
            if k + 1 == N {
                return false;
            }
            match unquote(&line[start..i], quoted) {
                Some(field) => out[k] = field,
                None => return false,
            }
            k += 1;
            start = i + 1;
            quoted = false;
        } else if c == b'"' {
            quoted = true;
        }
    }
    if k + 1 != N {
        return false;
    }
    match unquote(&line[start..], quoted) {
        Some(field) => {
            out[k] = field;
            true
        }
        None => false,
    }
}

#[inline]
fn unquote(field: &[u8], quoted: bool) -> Option<&[u8]> {
    if !quoted {
        return Some(field);
    }
    let n = field.len();
    if n >= 2 && field[0] == b'"' && field[n - 1] == b'"' && memchr(b'"', &field[1..n - 1]).is_none() {
        Some(&field[1..n - 1])
    } else {
        None
    }
}

/// Where the data of a file starts: just after its first `\n` (the header), or
/// None if the file has no data line.
pub fn data_start(data: &[u8]) -> Option<usize> {
    let header_end = memchr(b'\n', data)? + 1;
    (header_end < data.len()).then_some(header_end)
}

/// Calls `f` on every line that starts in `[start, end)`, where `start >= first`
/// and `first` is the file's data start. The last line may run past `end`.
#[inline]
pub fn for_each_line(data: &[u8], first: usize, start: usize, end: usize, mut f: impl FnMut(&[u8])) {
    let mut p = if start <= first {
        first
    } else {
        match memchr(b'\n', &data[start - 1..]) {
            Some(q) => start + q,
            None => return,
        }
    };
    while p < end {
        let q = match memchr(b'\n', &data[p..]) {
            Some(q) => p + q,
            None => data.len(),
        };
        f(&data[p..q]);
        p = q + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split<const N: usize>(line: &str) -> Option<Vec<String>> {
        let mut out = [&b""[..]; N];
        fields::<N>(line.as_bytes(), &mut out)
            .then(|| out.iter().map(|f| String::from_utf8(f.to_vec()).unwrap()).collect())
    }

    #[test]
    fn dialect() {
        assert_eq!(split::<2>("u1,u2"), Some(vec!["u1".into(), "u2".into()]));
        assert_eq!(split::<2>("\"u1\",\"\""), Some(vec!["u1".into(), "".into()]));
        assert_eq!(split::<2>("u1,"), Some(vec!["u1".into(), "".into()]));
        assert_eq!(split::<2>(" \"u1\",x"), None);
        assert_eq!(split::<2>("\",x"), None);
        assert_eq!(split::<2>("x,\""), None);
        assert_eq!(split::<2>("\"\"e1\",x"), None);
        assert_eq!(split::<2>("u1,u5\""), None);
        assert_eq!(split::<2>("u\"1,x"), None);
        assert_eq!(split::<2>("u1,u2,u3"), None);
        assert_eq!(split::<2>("u1"), None);
        assert_eq!(split::<2>(""), None);
        assert_eq!(split::<2>("u1\r,u2\r"), Some(vec!["u1\r".into(), "u2\r".into()]));
        assert_eq!(split::<2>("\" u1\",u1 "), Some(vec![" u1".into(), "u1 ".into()]));
        assert_eq!(split::<1>("\"\""), Some(vec!["".into()]));
    }

    fn lines_by_chunks(data: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let Some(first) = data_start(data) {
            let mut s = first;
            while s < data.len() {
                let e = (s + chunk).min(data.len());
                for_each_line(data, first, s, e, |l| out.push(l.to_vec()));
                s = e;
            }
        }
        out
    }

    fn lines_simple(data: &[u8]) -> Vec<Vec<u8>> {
        let mut parts: Vec<Vec<u8>> = data.split(|&c| c == b'\n').map(|l| l.to_vec()).collect();
        if parts.last().is_some_and(|l| l.is_empty()) {
            parts.pop();
        }
        parts.into_iter().skip(1).collect()
    }

    #[test]
    fn chunking_matches_simple_split() {
        let cases: [&[u8]; 8] = [
            b"",
            b"header",
            b"header\n",
            b"header\na\nbb\n\nccc",
            b"header\na\nbb\n\nccc\n",
            b"h\n\n\n\nx\n",
            b"header\nline one\nline two is longer\nx\n\n",
            b"h\nabc,def\nghi\r\n\"q\",z\n",
        ];
        for data in cases {
            let expected = lines_simple(data);
            for chunk in 1..=12 {
                assert_eq!(lines_by_chunks(data, chunk), expected, "{:?} chunk {chunk}", String::from_utf8_lossy(data));
            }
        }
    }
}
