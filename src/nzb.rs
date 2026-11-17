//! NZB documents: the files of a release and the articles that hold them.

use std::fmt;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use sha1::{Digest, Sha1};

/// One part of a posted file. Reposts can list several message-IDs for the same part.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Segment {
    pub number: u32,
    /// Encoded article size the poster reported.
    pub bytes: u64,
    /// Never empty. Tried in order.
    pub message_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NzbFile {
    pub subject: String,
    /// The quoted name in the subject, or the subject. May be blank or obfuscated.
    pub filename: String,
    /// `k` of a `[k/N]` subject prefix.
    pub post_index: Option<u32>,
    /// Sorted by number.
    pub segments: Vec<Segment>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Nzb {
    pub meta: Vec<(String, String)>,
    pub files: Vec<NzbFile>,
    /// SHA-1 of the document, which names the release.
    pub sha1: [u8; 20],
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NzbError(pub String);

impl fmt::Display for NzbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a usable NZB: {}", self.0)
    }
}

impl std::error::Error for NzbError {}

impl Nzb {
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The release name from the NZB head, when it has one.
    pub fn title(&self) -> Option<&str> {
        self.meta("name").or_else(|| self.meta("title")).filter(|t| !t.trim().is_empty())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, NzbError> {
        let text = String::from_utf8_lossy(bytes);
        let mut reader = Reader::from_str(&text);
        let mut meta = Vec::new();
        let mut files: Vec<NzbFile> = Vec::new();
        let mut text_target: Option<Target> = None;
        let mut buffer = String::new();
        let mut seen_nzb = false;
        loop {
            let event = reader.read_event().map_err(|e| NzbError(format!("malformed XML: {e}")))?;
            match event {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let empty = matches!(event, Event::Empty(_));
                    match local(e).as_str() {
                        "nzb" => seen_nzb = true,
                        "file" => {
                            let subject = attr(e, "subject").unwrap_or_default();
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
                            let number = attr(e, "number").and_then(|n| n.parse().ok());
                            let bytes = attr(e, "bytes").and_then(|n| n.parse().ok()).unwrap_or(0);
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
                        let id = buffer.trim().trim_start_matches('<').trim_end_matches('>').to_string();
                        let Some(file) = files.last_mut() else {
                            return Err(NzbError("a segment is outside any <file>".into()));
                        };
                        if id.is_empty() {
                            return Err(NzbError(format!("segment {number} of {:?} has no message-ID", file.filename)));
                        }
                        file.segments.push(Segment { number, bytes, message_ids: vec![id] });
                    }
                    None => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
        if !seen_nzb {
            return Err(NzbError("the document has no <nzb> element".into()));
        }
        files.retain(|f| !f.segments.is_empty());
        if files.is_empty() {
            return Err(NzbError("it lists no files with segments".into()));
        }
        for f in &mut files {
            // Stable, so a repost's message-IDs keep the order the NZB lists them in.
            f.segments.sort_by_key(|s| s.number);
            let mut merged: Vec<Segment> = Vec::with_capacity(f.segments.len());
            for s in f.segments.drain(..) {
                match merged.last_mut() {
                    Some(m) if m.number == s.number => {
                        for id in s.message_ids {
                            if !m.message_ids.contains(&id) {
                                m.message_ids.push(id);
                            }
                        }
                    }
                    _ => merged.push(s),
                }
            }
            f.segments = merged;
        }
        Ok(Self { meta, files, sha1: Sha1::digest(bytes).into() })
    }
}

enum Target {
    Meta(String),
    Segment { number: u32, bytes: u64 },
}

fn local(e: &BytesStart<'_>) -> String {
    e.local_name().as_ref().to_string()
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.normalized_value(XmlVersion::default()).ok().map(|v| v.into_owned()))
}

pub fn filename_from_subject(subject: &str) -> String {
    let quoted = subject.split('"').nth(1).filter(|_| subject.matches('"').count() >= 2);
    quoted.unwrap_or(subject).trim().to_string()
}

fn post_index(subject: &str) -> Option<u32> {
    let open = subject.find('[')?;
    let (k, rest) = subject[open + 1..].split_once('/')?;
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
}
