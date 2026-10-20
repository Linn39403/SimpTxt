use simptxt::document::{self, Document};
use std::sync::Arc;
fn rand(s: &mut u64) -> usize {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s as usize
}
fn verify(d: &Document, r: &str) {
    assert_eq!(d.read(0..d.len()), r.as_bytes());
    let starts: Vec<_> = std::iter::once(0)
        .chain(r.match_indices('\n').map(|(n, _)| n + 1))
        .collect();
    assert_eq!(d.line_count(), starts.len());
    for (i, n) in starts.iter().enumerate() {
        assert_eq!(d.line_start(i), *n);
    }
    for i in 0..=r.len() {
        assert_eq!(d.char_boundary(i), r.is_char_boundary(i));
        assert_eq!(
            d.line_of_offset(i),
            r.as_bytes()[..i].iter().filter(|&&x| x == b'\n').count()
        );
    }
    for p in d.chunks() {
        assert!(p.len() <= document::CHUNK_SIZE);
        assert!(std::str::from_utf8(p).is_ok());
    }
}
#[test]
fn randomized_chunk_validation_edits_and_undo_roundtrip() {
    let stress = std::env::var_os("SIMPTXT_STRESS").is_some();
    let validation_cases = if stress { 25000 } else { 5000 };
    let edit_rounds = if stress { 500 } else { 100 };
    let edits_per_round = if stress { 250 } else { 100 };
    let mut seed = 0x6492c56aab7fe120u64;
    for round in 0..validation_cases {
        let len = rand(&mut seed) % 32;
        let bytes: Vec<u8> = (0..len).map(|_| rand(&mut seed) as u8).collect();
        let expected = std::str::from_utf8(&bytes);
        let mut chunks: Vec<Arc<[u8]>> = vec![];
        let mut start = 0;
        while start < len {
            let end = (start + 1 + rand(&mut seed) % 7).min(len);
            chunks.push(Arc::from(&bytes[start..end]));
            if rand(&mut seed).is_multiple_of(3) {
                chunks.push(Arc::from(&[][..]));
            }
            start = end;
        }
        let d = Document::from_chunks(chunks);
        assert_eq!(
            d.is_ok(),
            expected.is_ok(),
            "validation round {round}, bytes {bytes:?}"
        );
        if let (Ok(d), Ok(s)) = (d, expected) {
            verify(&d, s);
        }
    }
    let fragments = [
        "",
        "x",
        "\n",
        "\r\n",
        "🙂",
        "é日",
        "\u{feff}",
        "a\rb",
        "λ\n\r\n終",
    ];
    for round in 0..edit_rounds {
        let mut r = String::from("\u{feff}é日🙂\r\nalpha\rbravo\nend");
        let mut d = Document::from_bytes(r.as_bytes().to_vec()).unwrap();
        let initial = r.clone();
        let mut changes = 0;
        for _ in 0..edits_per_round {
            let bounds: Vec<_> = r
                .char_indices()
                .map(|(n, _)| n)
                .chain(std::iter::once(r.len()))
                .collect();
            let frag = fragments[rand(&mut seed) % fragments.len()];
            let rev = d.revision();
            if rand(&mut seed).is_multiple_of(5) {
                let mut rs = vec![];
                let mut last = 0;
                for _ in 0..rand(&mut seed) % 5 {
                    let a = rand(&mut seed) % bounds.len();
                    let b = rand(&mut seed) % bounds.len();
                    let lo = bounds[a.min(b)];
                    let hi = bounds[a.max(b)];
                    if lo >= last {
                        rs.push(lo..hi);
                        last = hi;
                    }
                }
                let mut next = String::new();
                let mut p = 0;
                for range in &rs {
                    next.push_str(&r[p..range.start]);
                    next.push_str(frag);
                    p = range.end;
                }
                next.push_str(&r[p..]);
                d.replace_ranges(&rs, frag).unwrap();
                r = next;
            } else {
                let a = bounds[rand(&mut seed) % bounds.len()];
                let b = bounds[rand(&mut seed) % bounds.len()];
                let range = a.min(b)..a.max(b);
                d.replace(range.clone(), frag).unwrap();
                r.replace_range(range, frag);
            }
            if rev != d.revision() {
                changes += 1;
            }
            verify(&d, &r);
        }
        let finaltext = r;
        for _ in 0..changes {
            assert!(d.undo(), "round {round}");
        }
        verify(&d, &initial);
        assert!(!d.undo());
        for _ in 0..changes {
            assert!(d.redo());
        }
        verify(&d, &finaltext);
        assert!(!d.redo());
    }
    let r = "🙂é日\r\nx".repeat(100000);
    let bytes: Arc<[u8]> = Arc::from(r.as_bytes());
    let d = Document::from_chunks(vec![bytes]).unwrap();
    assert_eq!(d.read(0..d.len()), r.as_bytes());
    for p in d.chunks() {
        assert!(p.len() <= document::CHUNK_SIZE);
        assert!(std::str::from_utf8(p).is_ok());
    }
    println!(
        "PASS: {} arbitrary byte/chunk validations, {} randomized edits including bulk+full undo/redo, and oversize Unicode source chunk",
        validation_cases,
        edit_rounds * edits_per_round
    );
}
