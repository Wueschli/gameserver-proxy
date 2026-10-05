//! Building the URL an intent verb is forwarded to.
//!
//! Path parameters reach the handlers percent-decoded, so a caller can put
//! `/`, `..`, `?` or `#` inside one segment. Pasting those into a string with
//! `format!` would let one verb's route address a different admin path on every
//! instance (carrying the instance token). A [`Target`] instead holds the path
//! as a list of segments, refuses the dangerous ones up front, and pushes them
//! onto the instance's base URL through `Url::path_segments_mut`, which
//! percent-encodes whatever it is given.

use reqwest::Url;

/// A forwarded request's path (as segments) and query, relative to an
/// instance's admin base URL.
#[derive(Debug, Clone)]
pub struct Target {
    segments: Vec<String>,
    query: Vec<(String, String)>,
}

/// Why a path segment was refused.
fn segment_error(segment: &str) -> Option<&'static str> {
    if segment.is_empty() {
        Some("empty")
    } else if segment == "." || segment == ".." {
        Some("a dot segment")
    } else if segment.contains(['/', '\\', '?', '#']) || segment.chars().any(char::is_control) {
        Some("contains a path or query delimiter or a control character")
    } else {
        None
    }
}

impl Target {
    /// `segments` joined under the instance's base URL. `Err` names the first
    /// segment that cannot be forwarded safely.
    pub fn new(segments: &[&str]) -> Result<Self, String> {
        for s in segments {
            if let Some(why) = segment_error(s) {
                return Err(format!("invalid path segment {s:?}: {why}"));
            }
        }
        Ok(Target {
            segments: segments.iter().map(ToString::to_string).collect(),
            query: Vec::new(),
        })
    }

    /// Adds one query parameter (percent-encoded when the URL is built).
    pub fn with_query(mut self, key: &str, value: &str) -> Self {
        self.query.push((key.to_string(), value.to_string()));
        self
    }

    /// The full URL under `base` (an instance's `admin_url`, which may itself
    /// carry a path prefix such as `https://edge.example/wayhouse-1`).
    pub fn url(&self, base: &str) -> Result<Url, String> {
        let mut url = Url::parse(base).map_err(|e| format!("bad admin_url {base:?}: {e}"))?;
        url.path_segments_mut()
            .map_err(|()| format!("bad admin_url {base:?}: cannot be a base"))?
            .pop_if_empty()
            .extend(&self.segments);
        if !self.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&self.query);
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_segments_under_the_base_path() {
        let t = Target::new(&["pools", "local", "backends", "10.0.0.1:25565"]).unwrap();
        assert_eq!(
            t.url("http://10.0.0.1:9900").unwrap().as_str(),
            "http://10.0.0.1:9900/pools/local/backends/10.0.0.1:25565"
        );
        assert_eq!(
            t.url("https://edge.example/wayhouse-1/").unwrap().path(),
            "/wayhouse-1/pools/local/backends/10.0.0.1:25565"
        );
    }

    #[test]
    fn refuses_traversal_and_delimiters() {
        for bad in [
            "", ".", "..", "a/b", "../admin", "a?b", "a#b", "a\\b", "a\nb",
        ] {
            assert!(Target::new(&["pools", bad]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn encodes_what_it_keeps() {
        let t = Target::new(&["pools", "a%2Fb c"]).unwrap();
        assert_eq!(t.url("http://h:1").unwrap().path(), "/pools/a%252Fb%20c");
    }

    #[test]
    fn query_values_are_encoded() {
        let t = Target::new(&["admin", "sniffers"])
            .unwrap()
            .with_query("name", "a&x=1 b");
        assert_eq!(
            t.url("http://h:1").unwrap().as_str(),
            "http://h:1/admin/sniffers?name=a%26x%3D1+b"
        );
    }
}
