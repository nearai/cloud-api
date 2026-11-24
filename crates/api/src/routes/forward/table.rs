//! The fixed list of (method, path template) pairs that are forwarded.

use axum::http::Method;

/// One forwarded route. The template is kept for logs and metrics; the real
/// request path never is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub method: Method,
    pub template: &'static str,
}

impl Entry {
    pub const fn new(method: Method, template: &'static str) -> Self {
        Self { method, template }
    }

    fn matches(&self, method: &Method, path: &str) -> bool {
        if self.method != *method {
            return false;
        }
        let mut want = self.template.split('/');
        let mut got = path.split('/');
        loop {
            match (want.next(), got.next()) {
                (None, None) => return true,
                (Some(w), Some(g)) => {
                    let param = w.starts_with('{') && w.ends_with('}');
                    if param {
                        if g.is_empty() {
                            return false;
                        }
                    } else if w != g {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ForwardTable {
    entries: Vec<Entry>,
}

impl ForwardTable {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self { entries }
    }

    /// The routes forwarded in production. Empty until routes are switched.
    pub fn production() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry with the same method whose template matches `path`: same
    /// number of segments, literal segments equal, and each `{name}` segment
    /// matching exactly one non-empty segment.
    pub fn r#match(&self, method: &Method, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.matches(method, path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ForwardTable {
        ForwardTable::new(vec![
            Entry::new(Method::GET, "/v1/organizations"),
            Entry::new(Method::GET, "/v1/organizations/{id}/usage/balance"),
            Entry::new(Method::POST, "/v1/organizations/{id}/workspaces"),
        ])
    }

    #[test]
    fn matches_literal_and_param_segments() {
        let t = table();
        assert_eq!(
            t.r#match(&Method::GET, "/v1/organizations")
                .unwrap()
                .template,
            "/v1/organizations"
        );
        assert_eq!(
            t.r#match(&Method::GET, "/v1/organizations/abc/usage/balance")
                .unwrap()
                .template,
            "/v1/organizations/{id}/usage/balance"
        );
    }

    #[test]
    fn method_must_match() {
        let t = table();
        assert!(t.r#match(&Method::DELETE, "/v1/organizations").is_none());
        assert!(t
            .r#match(&Method::GET, "/v1/organizations/abc/workspaces")
            .is_none());
        assert!(t
            .r#match(&Method::POST, "/v1/organizations/abc/workspaces")
            .is_some());
    }

    #[test]
    fn segment_count_and_empty_segments() {
        let t = table();
        assert!(t.r#match(&Method::GET, "/v1/organizations/").is_none());
        assert!(t
            .r#match(&Method::GET, "/v1/organizations//usage/balance")
            .is_none());
        assert!(t
            .r#match(&Method::GET, "/v1/organizations/a/b/usage/balance")
            .is_none());
        assert!(t
            .r#match(&Method::GET, "/v1/organizations/abc/usage/balance/x")
            .is_none());
        assert!(t
            .r#match(&Method::GET, "/v1/organizations/abc/usage")
            .is_none());
        assert!(t
            .r#match(&Method::GET, "/v1/other/abc/usage/balance")
            .is_none());
    }

    #[test]
    fn production_table_is_empty() {
        assert!(ForwardTable::production().entries().is_empty());
    }
}
