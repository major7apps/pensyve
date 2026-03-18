use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedEntity {
    pub kind: String,
    pub value: String,
    pub start: usize,
    pub end: usize,
}

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap()
});

static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}").unwrap()
});

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s<>"]+"#).unwrap()
});

pub fn extract_patterns(text: &str) -> Vec<ExtractedEntity> {
    let mut entities: Vec<ExtractedEntity> = Vec::new();

    for m in EMAIL_RE.find_iter(text) {
        entities.push(ExtractedEntity {
            kind: "email".to_string(),
            value: m.as_str().to_string(),
            start: m.start(),
            end: m.end(),
        });
    }

    for m in DATE_RE.find_iter(text) {
        entities.push(ExtractedEntity {
            kind: "date".to_string(),
            value: m.as_str().to_string(),
            start: m.start(),
            end: m.end(),
        });
    }

    for m in URL_RE.find_iter(text) {
        entities.push(ExtractedEntity {
            kind: "url".to_string(),
            value: m.as_str().to_string(),
            start: m.start(),
            end: m.end(),
        });
    }

    // Sort by start position so results are in document order
    entities.sort_by_key(|e| e.start);

    entities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        assert!(extract_patterns("").is_empty());
    }

    #[test]
    fn test_plain_text_no_entities() {
        let result = extract_patterns("Hello world, this is plain text with no patterns.");
        assert!(result.is_empty());
    }

    #[test]
    fn test_email_extraction() {
        let text = "Contact us at support@example.com for help.";
        let result = extract_patterns(text);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, "email");
        assert_eq!(result[0].value, "support@example.com");
        assert_eq!(result[0].start, 14);
        assert_eq!(result[0].end, 33);
    }

    #[test]
    fn test_multiple_emails() {
        let text = "Send to alice@foo.com and bob@bar.org.";
        let result = extract_patterns(text);
        let emails: Vec<&str> = result.iter().map(|e| e.value.as_str()).collect();
        assert!(emails.contains(&"alice@foo.com"));
        assert!(emails.contains(&"bob@bar.org"));
        assert!(result.iter().all(|e| e.kind == "email"));
    }

    #[test]
    fn test_date_extraction() {
        let text = "The meeting is on 2024-03-15 at noon.";
        let result = extract_patterns(text);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, "date");
        assert_eq!(result[0].value, "2024-03-15");
    }

    #[test]
    fn test_multiple_dates() {
        let text = "From 2023-01-01 to 2023-12-31 we tracked progress.";
        let result = extract_patterns(text);
        let dates: Vec<&str> = result.iter().map(|e| e.value.as_str()).collect();
        assert!(dates.contains(&"2023-01-01"));
        assert!(dates.contains(&"2023-12-31"));
    }

    #[test]
    fn test_url_extraction() {
        let text = "Visit https://example.com for more info.";
        let result = extract_patterns(text);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, "url");
        assert_eq!(result[0].value, "https://example.com");
    }

    #[test]
    fn test_url_http_and_https() {
        let text = "See http://foo.com and https://bar.org.";
        let result = extract_patterns(text);
        let urls: Vec<&str> = result.iter().filter(|e| e.kind == "url").map(|e| e.value.as_str()).collect();
        assert!(urls.contains(&"http://foo.com"));
        assert!(urls.contains(&"https://bar.org."));
    }

    #[test]
    fn test_mixed_content() {
        let text = "Email user@test.com, visit https://test.com, date 2024-06-01.";
        let result = extract_patterns(text);
        let kinds: Vec<&str> = result.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"email"));
        assert!(kinds.contains(&"url"));
        assert!(kinds.contains(&"date"));
    }

    #[test]
    fn test_results_sorted_by_position() {
        let text = "Date: 2024-01-01, email: hello@world.com, url: https://world.com";
        let result = extract_patterns(text);
        for w in result.windows(2) {
            assert!(w[0].start <= w[1].start, "Results should be sorted by start position");
        }
    }

    #[test]
    fn test_email_subdomain() {
        let text = "user@mail.subdomain.example.co.uk";
        let result = extract_patterns(text);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, "email");
        assert_eq!(result[0].value, "user@mail.subdomain.example.co.uk");
    }

    #[test]
    fn test_url_with_path_and_query() {
        let text = "Go to https://api.example.com/v1/search?q=rust&page=1 now.";
        let result = extract_patterns(text);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, "url");
        assert_eq!(result[0].value, "https://api.example.com/v1/search?q=rust&page=1");
    }
}
