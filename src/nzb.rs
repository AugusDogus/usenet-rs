//! NZB documents: the files of a release and the articles that hold them.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use sha1::{Digest, Sha1};

/// One part of a posted file. Reposts can list several message-IDs for the same part.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Segment {
    /// The part's number within its file, from 1.
    pub number: u32,
    /// Encoded article size the poster reported.
    pub bytes: u64,
    /// Never empty. Tried in order.
    pub message_ids: Vec<String>,
}

/// One posted file of a release.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NzbFile {
    /// The article subject the file was posted under.
    pub subject: String,
    /// The quoted name in the subject, or the subject. May be blank or obfuscated.
    pub filename: String,
    /// `k` of a `[k/N]` subject prefix.
    pub post_index: Option<u32>,
    /// Sorted by number, one per number. Never empty.
    pub segments: Vec<Segment>,
}

/// A parsed NZB: a release's metadata and its files.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Nzb {
    /// `<meta type="key">value</meta>` entries of the head, in document order.
    pub meta: Vec<(String, String)>,
    /// The files that list at least one segment, in document order.
    pub files: Vec<NzbFile>,
    /// Names the release: a SHA-1 over each file's segment numbers and message-IDs. Indexers
    /// vary the rest of an NZB on every download (metadata, posters, dates, groups) to mark
    /// who fetched it, so a hash of the whole document would name the same release
    /// differently each time it is fetched.
    pub id: [u8; 20],
}

/// Why a document is not a usable NZB.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum NzbError {
    /// The document is not well-formed XML.
    Xml {
        /// Byte offset in the document where the problem starts.
        position: u64,
        /// What the XML parser found.
        source: XmlError,
    },
    /// The document has no `<nzb>` element.
    NotNzb {
        /// The document's first element, if it has one.
        root: Option<String>,
    },
    /// A `<segment>` comes before any `<file>`, so it belongs to no file.
    SegmentOutsideFile {
        /// The segment's number.
        number: u32,
    },
    /// A `<segment>` lists no message-ID.
    MissingMessageId {
        /// The file name from the segment's `<file>` subject.
        file: String,
        /// The segment's number.
        number: u32,
    },
    /// No `<file>` lists any segments.
    NoFiles {
        /// How many `<file>` elements the document has.
        files: usize,
    },
}

impl fmt::Display for NzbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Xml { position, source } => write!(
                f,
                "the NZB is not well-formed XML at byte {position} ({source}); \
                 the file is damaged or is not an NZB, so download it again"
            ),
            Self::NotNzb { root: Some(root) } => write!(
                f,
                "the document is <{root}>, not <nzb>, so it is not an NZB; \
                 an indexer may have sent an error page instead"
            ),
            Self::NotNzb { root: None } => f.write_str("the document has no XML elements, so it is not an NZB"),
            Self::SegmentOutsideFile { number } => {
                write!(f, "the NZB lists segment {number} outside any <file>, so the release's files cannot be known")
            }
            Self::MissingMessageId { file, number } => write!(
                f,
                "segment {number} of {file:?} in the NZB has no message-ID, so that part of the file cannot be fetched"
            ),
            Self::NoFiles { files } => {
                write!(f, "the NZB has {files} <file> elements but none lists a segment, so there is nothing to fetch")
            }
        }
    }
}

impl std::error::Error for NzbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Xml { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A problem the XML parser found. Its [`Display`](fmt::Display) says what it is.
#[derive(Clone, Debug)]
pub struct XmlError(quick_xml::Error);

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for XmlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl Nzb {
    /// The value of the first `<meta>` entry of type `key`.
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The release name from the NZB head, when it has one.
    pub fn title(&self) -> Option<&str> {
        self.meta("name").or_else(|| self.meta("title")).filter(|t| !t.trim().is_empty())
    }

    /// Parses an NZB document.
    ///
    /// Invalid UTF-8 is replaced, not rejected. Files without segments are dropped. A
    /// file's segments are sorted by number, and segments that share a number, as reposts
    /// do, are merged into one with every distinct message-ID in document order.
    ///
    /// ```
    /// let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
    /// <nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
    ///   <head><meta type="name">Big.Buck.Bunny</meta></head>
    ///   <file poster="p" date="1" subject="[1/1] - &quot;bbb.mkv&quot; yEnc (1/2)">
    ///     <groups><group>alt.binaries.test</group></groups>
    ///     <segments>
    ///       <segment bytes="739" number="2">b@example</segment>
    ///       <segment bytes="740" number="1">a@example</segment>
    ///     </segments>
    ///   </file>
    /// </nzb>"#;
    /// let nzb = usenet::Nzb::parse(doc)?;
    /// assert_eq!(nzb.title(), Some("Big.Buck.Bunny"));
    /// let file = &nzb.files[0];
    /// assert_eq!((file.filename.as_str(), file.post_index), ("bbb.mkv", Some(1)));
    /// assert_eq!(file.segments[0].message_ids, ["a@example"]);
    ///
    /// assert!(matches!(usenet::Nzb::parse(b"<error code=\"100\"/>"), Err(usenet::nzb::NzbError::NotNzb { .. })));
    /// # Ok::<(), usenet::nzb::NzbError>(())
    /// ```
    pub fn parse(bytes: &[u8]) -> Result<Self, NzbError> {
        // `from_utf8_lossy` checks byte by byte; `from_utf8` is many times faster on valid input.
        let text = match std::str::from_utf8(bytes) {
            Ok(text) => Cow::Borrowed(text),
            Err(_) => String::from_utf8_lossy(bytes),
        };
        let mut reader = Reader::from_str(&text);
        let mut meta = Vec::new();
        let mut files: Vec<NzbFile> = Vec::new();
        let mut file_elements = 0;
        let mut text_target: Option<Target> = None;
        let mut buffer = String::new();
        let mut root: Option<String> = None;
        let mut seen_nzb = false;
        loop {
            let event = reader
                .read_event()
                .map_err(|source| NzbError::Xml { position: reader.error_position(), source: XmlError(source) })?;
            match event {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let empty = matches!(event, Event::Empty(_));
                    let name = e.local_name();
                    if root.is_none() {
                        root = Some(name.as_ref().to_owned());
                    }
                    match name.as_ref() {
                        "nzb" => seen_nzb = true,
                        "file" => {
                            let subject = attr(e, "subject").unwrap_or_default();
                            file_elements += 1;
                            files.push(NzbFile {
                                filename: filename_from_subject(&subject),
                                post_index: post_index(&subject),
                                subject,
                                segments: Vec::new(),
                            });
                        }
                        "meta" if !empty => {
                            text_target = Some(Target::Meta(attr(e, "type").unwrap_or_default()));
                            buffer.clear();
                        }
                        "segment" if !empty => {
                            let (number, bytes) = segment_attrs(e);
                            text_target = number.map(|number| Target::Segment { number, bytes });
                            buffer.clear();
                        }
                        _ => {}
                    }
                }
                Event::Text(t) => {
                    if text_target.is_some() {
                        buffer.push_str(&t);
                    }
                }
                Event::GeneralRef(r) => {
                    if text_target.is_some() {
                        let c = r.resolve_char_ref().ok().flatten().or(match &*r {
                            "amp" => Some('&'),
                            "lt" => Some('<'),
                            "gt" => Some('>'),
                            "quot" => Some('"'),
                            "apos" => Some('\''),
                            _ => None,
                        });
                        buffer.extend(c);
                    }
                }
                Event::End(_) => match text_target.take() {
                    Some(Target::Meta(key)) => meta.push((key, buffer.trim().to_string())),
                    Some(Target::Segment { number, bytes }) => {
                        let id = buffer.trim().trim_start_matches('<').trim_end_matches('>');
                        let Some(file) = files.last_mut() else {
                            return Err(NzbError::SegmentOutsideFile { number });
                        };
                        if id.is_empty() {
                            return Err(NzbError::MissingMessageId { file: file.filename.clone(), number });
                        }
                        file.segments.push(Segment { number, bytes, message_ids: vec![id.to_string()] });
                    }
                    None => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
        if !seen_nzb {
            return Err(NzbError::NotNzb { root });
        }
        files.retain(|f| !f.segments.is_empty());
        if files.is_empty() {
            return Err(NzbError::NoFiles { files: file_elements });
        }
        for f in &mut files {
            // Stable, so a repost's message-IDs keep the order the NZB lists them in.
            f.segments.sort_by_key(|s| s.number);
            f.segments.dedup_by(|later, kept| {
                if later.number != kept.number {
                    return false;
                }
                for id in later.message_ids.drain(..) {
                    if !kept.message_ids.contains(&id) {
                        kept.message_ids.push(id);
                    }
                }
                true
            });
        }
        let id = release_id(&files);
        Ok(Self { meta, files, id })
    }
}

/// A SHA-1 over the files' segments: per file, each segment's number and message-IDs, with
/// lengths so no two lists hash alike; then the files' digests in sorted order, so file order
/// does not matter either.
fn release_id(files: &[NzbFile]) -> [u8; 20] {
    let mut digests: Vec<[u8; 20]> = files
        .iter()
        .map(|file| {
            let mut hash = Sha1::new();
            for segment in &file.segments {
                hash.update(segment.number.to_le_bytes());
                hash.update((segment.message_ids.len() as u64).to_le_bytes());
                for id in &segment.message_ids {
                    hash.update((id.len() as u64).to_le_bytes());
                    hash.update(id.as_bytes());
                }
            }
            hash.finalize().into()
        })
        .collect();
    digests.sort_unstable();
    let mut hash = Sha1::new();
    for digest in &digests {
        hash.update(digest);
    }
    hash.finalize().into()
}

enum Target {
    Meta(String),
    Segment { number: u32, bytes: u64 },
}

/// The attributes of `e` that parse. A repeated name counts as not parsing, even when the
/// first one did not parse either.
fn attributes<'a>(e: &'a BytesStart<'_>) -> impl Iterator<Item = Attribute<'a>> {
    e.attributes().flatten()
}

/// The normalized value of the first attribute named `name`.
fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    attributes(e)
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.normalized_value(XmlVersion::default()).ok().map(|v| v.into_owned()))
}

/// A segment's `number`, and its `bytes` or 0, each from the first attribute of that name.
fn segment_attrs(e: &BytesStart<'_>) -> (Option<u32>, u64) {
    fn parse<T: FromStr>(a: &Attribute<'_>) -> Option<T> {
        a.normalized_value(XmlVersion::default()).ok()?.parse().ok()
    }
    let (mut number, mut bytes) = (None, None);
    for a in attributes(e) {
        match a.key.local_name().as_ref() {
            "number" if number.is_none() => number = Some(parse(&a)),
            "bytes" if bytes.is_none() => bytes = Some(parse(&a)),
            _ => {}
        }
    }
    (number.flatten(), bytes.flatten().unwrap_or(0))
}

/// The file name a subject carries: its first `"quoted"` part, or else the whole subject, trimmed.
pub fn filename_from_subject(subject: &str) -> String {
    let quoted = subject.split('"').nth(1).filter(|_| subject.matches('"').count() >= 2);
    quoted.unwrap_or(subject).trim().to_string()
}

fn post_index(subject: &str) -> Option<u32> {
    let open = subject.find('[')?;
    let (k, rest) = subject.get(open + 1..)?.split_once('/')?;
    rest.split_once(']')?;
    k.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NZB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="name">Some.Release &amp; More</meta></head>
  <file poster="p" date="1" subject="[2/9] - &quot;movie.part01.rar&quot; yEnc (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="10" number="2">b@x</segment>
      <segment bytes="10" number="1">&lt;a@x&gt;</segment>
      <segment bytes="10" number="1">a2@x</segment>
    </segments>
  </file>
  <file poster="p" date="1" subject="empty"><segments/></file>
</nzb>"#;

    fn release(meta: &str, poster: &str, group: &str, files: &[(&str, &[(u32, &str)])]) -> Nzb {
        let mut doc = format!("<nzb><head><meta type=\"title\">{meta}</meta></head>");
        for (subject, segments) in files {
            doc.push_str(&format!(
                "<file poster=\"{poster}\" date=\"1\" subject=\"{subject}\"><groups><group>{group}</group></groups><segments>"
            ));
            for (number, id) in *segments {
                doc.push_str(&format!("<segment bytes=\"9\" number=\"{number}\">{id}</segment>"));
            }
            doc.push_str("</segments></file>");
        }
        doc.push_str("</nzb>");
        Nzb::parse(doc.as_bytes()).unwrap()
    }

    #[test]
    fn a_release_keeps_its_id_when_an_indexer_varies_everything_but_the_segments() {
        let a: &[(u32, &str)] = &[(1, "a1@x"), (2, "a2@x")];
        let b: &[(u32, &str)] = &[(1, "b1@x")];
        let first = release("3JGKpy19.mkv", "x@y", "alt.binaries.movies", &[("a", a), ("b", b)]);
        let again = release("5F9CDZvP.mkv", "q@z", "alt.binaries.tv", &[("b", b), ("a", a)]);
        assert_eq!(first.id, again.id);
        let other = release("3JGKpy19.mkv", "x@y", "alt.binaries.movies", &[("a", a), ("b", &[(1, "b9@x")])]);
        assert_ne!(first.id, other.id);
        let renumbered = release("3JGKpy19.mkv", "x@y", "alt.binaries.movies", &[("a", a), ("b", &[(2, "b1@x")])]);
        assert_ne!(first.id, renumbered.id);
    }

    #[test]
    fn groups_reposts_and_reads_names() {
        let nzb = Nzb::parse(NZB.as_bytes()).unwrap();
        assert_eq!(nzb.title(), Some("Some.Release & More"));
        assert_eq!(nzb.files.len(), 1, "files without segments are dropped");
        let f = &nzb.files[0];
        assert_eq!((f.filename.as_str(), f.post_index), ("movie.part01.rar", Some(2)));
        let ids: Vec<(u32, Vec<&str>)> =
            f.segments.iter().map(|s| (s.number, s.message_ids.iter().map(String::as_str).collect())).collect();
        assert_eq!(ids, vec![(1, vec!["a@x", "a2@x"]), (2, vec!["b@x"])]);
    }

    #[test]
    fn a_file_of_many_segments_parses_in_linear_time() {
        let segments: String =
            (1..=100_000).map(|n| format!("<segment bytes=\"1\" number=\"{n}\">{n}@x</segment>")).collect();
        let doc = format!("<nzb><file subject=\"big\"><segments>{segments}</segments></file></nzb>");
        let started = std::time::Instant::now();
        let nzb = Nzb::parse(doc.as_bytes()).unwrap();
        assert_eq!(nzb.files[0].segments.len(), 100_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[test]
    fn rejects_other_documents() {
        assert!(Nzb::parse(b"<rss></rss>").is_err());
        assert!(Nzb::parse(b"<nzb></nzb>").is_err());
    }

    #[test]
    fn subjects_without_quotes_are_the_name() {
        assert_eq!(filename_from_subject("WKelZy6v.mkv yEnc (1/5)"), "WKelZy6v.mkv yEnc (1/5)");
        assert_eq!(filename_from_subject(""), "");
    }

    #[test]
    fn errors_say_what_is_wrong() {
        let parse = |s: &str| Nzb::parse(s.as_bytes()).map(|_| ()).unwrap_err();
        assert!(matches!(parse("<error code=\"100\"/>"), NzbError::NotNzb { root: Some(r) } if r == "error"));
        assert!(matches!(parse(""), NzbError::NotNzb { root: None }));
        assert!(matches!(parse("<nzb><file/><file subject=\"x\"></file></nzb>"), NzbError::NoFiles { files: 2 }));
        assert!(matches!(
            parse("<nzb><segment number=\"3\">a@x</segment></nzb>"),
            NzbError::SegmentOutsideFile { number: 3 }
        ));
        assert!(matches!(
            parse("<nzb><file subject='\"a.rar\"'><segment number=\"4\"> &lt;&gt; </segment></file></nzb>"),
            NzbError::MissingMessageId { file, number: 4 } if file == "a.rar"
        ));
        let error = parse("<nzb><file></nzb>");
        assert!(matches!(error, NzbError::Xml { position: 11, .. }), "{error:?}");
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn segments_without_a_number_are_skipped() {
        let nzb = Nzb::parse(
            br#"<nzb><file subject="x"><segment bytes="5">a@x</segment><segment number="1">b@x</segment></file></nzb>"#,
        )
        .unwrap();
        assert_eq!(nzb.files[0].segments, [Segment { number: 1, bytes: 0, message_ids: vec!["b@x".into()] }]);
        // A number repeated after a malformed one is a duplicate, so neither counts.
        let doc = br#"<nzb><file subject="x"><segment number=n number="4">a@x</segment></file></nzb>"#;
        assert!(matches!(Nzb::parse(doc), Err(NzbError::NoFiles { files: 1 })));
    }

    fn sample(name: &str) -> Vec<u8> {
        let path = format!("{}/tests/data/nzb/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    /// Real NZBs of freely licensed films, with the indexer's metadata stripped.
    const SAMPLES: &[(&str, &str, usize, usize, &str)] = &[
        // Its `<file>`s have no subject; the indexer put the subject in `poster`.
        (
            "Big.Buck.Bunny.2008.1080p.x264-60fps.Surround.nzb",
            "Big.Buck.Bunny.2008.1080p.x264-60fps.Surround",
            52,
            2520,
            "",
        ),
        ("BigBuckBunny.640x360.m4v.nzb", "BigBuckBunny.640x360.m4v", 9, 193, "BigBuckBunny.640x360.m4v.vol-01.par2"),
        ("big_buck_bunny_480p_h264.nzb", "big.buck.bunny.480p.h264", 17, 387, "big_buck_bunny_480p_h264.mov.nzb"),
        (
            "nosferatu.1922.web.h264-rbb.nzb",
            "nosferatu.1922.web.h264-rbb",
            8,
            1066,
            "nosferatu.1922.web.h264-rbb.vol-01.par2",
        ),
    ];

    #[test]
    fn parses_the_sample_nzbs() {
        for &(file, title, files, segments, first) in SAMPLES {
            let nzb = Nzb::parse(&sample(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(nzb.title(), Some(title), "{file}");
            assert_eq!(nzb.files.len(), files, "{file}");
            assert_eq!(nzb.files.iter().map(|f| f.segments.len()).sum::<usize>(), segments, "{file}");
            assert_eq!(nzb.files[0].filename, first, "{file}");
            assert!(nzb.meta("password").is_none() && nzb.meta("indexer").is_none());
        }
    }

    /// A bounded, deterministic stand-in for the fuzz target, so CI catches regressions on stable.
    #[test]
    fn mutated_nzbs_parse_without_panics() {
        const INSERTS: &[&[u8]] = &[
            b"<",
            b">",
            b"&",
            b"&amp;",
            b"&#x41;",
            b"\"",
            b"</segment>",
            b"<segment number=\"1\">",
            b"<file>",
            b"\xff",
        ];
        let docs: Vec<Vec<u8>> = ["BigBuckBunny.640x360.m4v.nzb", "big_buck_bunny_480p_h264.nzb"].map(sample).into();
        let mut state = 5u64;
        let mut below = |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n.max(1) as u64) as usize
        };
        for _ in 0..1000 {
            let mut doc = docs[below(docs.len())].clone();
            for _ in 0..1 + below(8) {
                let at = below(doc.len() + 1);
                match below(4) {
                    0 => {
                        if let Some(b) = doc.get_mut(at) {
                            *b ^= 1 << below(8);
                        }
                    }
                    1 => drop(doc.splice(at..at, INSERTS[below(INSERTS.len())].iter().copied())),
                    2 => drop(doc.drain(at..(at + below(200)).min(doc.len()))),
                    _ => doc.truncate(at),
                }
            }
            if let Ok(nzb) = Nzb::parse(&doc) {
                for f in &nzb.files {
                    assert!(!f.segments.is_empty());
                    assert!(f.segments.windows(2).all(|w| w[0].number < w[1].number));
                    assert!(f.segments.iter().all(|s| s.message_ids.iter().all(|id| !id.is_empty())));
                }
            }
        }
    }
}
