//! Clients may percent-encode characters that `Url::from_file_path` spells
//! literally (VS Code sends `@` as `%40` and a drive colon as `%3A`). The
//! server keys documents by one canonical file URI, so the transport
//! canonicalises inbound URIs and restores the client's spelling on the way
//! out: clients match diagnostics and locations by their own URI string.

use crate::workspace::{MAX_OPEN_DOCUMENT_URI_BYTES, canonical_file_uri};
use lsp_server::Message;
use lsp_types::Url;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};

const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
/// Outbound objects keyed by document URI: `WorkspaceEdit.changes` and the
/// `relatedDocuments` of a document diagnostic report.
const URI_KEYED_MAPS: [&str; 2] = ["changes", "relatedDocuments"];

#[derive(Default)]
pub(super) struct UriSpellings {
    /// Canonical URI -> (client spelling, last-use stamp).
    client: HashMap<String, (String, u64)>,
    /// Least recently used first; entries whose stamp no longer matches
    /// `client` are stale and skipped on eviction.
    order: VecDeque<(String, u64)>,
    bytes: usize,
    stamp: u64,
}

impl UriSpellings {
    pub(super) fn canonicalize_inbound(&mut self, message: &mut Message) {
        let value = match message {
            Message::Request(request) => &mut request.params,
            Message::Notification(notification) => &mut notification.params,
            Message::Response(response) => match response.result.as_mut() {
                Some(result) => result,
                None => return,
            },
        };
        self.canonicalize_value(value);
    }

    pub(super) fn restore_outbound(&self, message: &mut Message) {
        if self.client.is_empty() {
            return;
        }
        let value = match message {
            Message::Request(request) => &mut request.params,
            Message::Notification(notification) => &mut notification.params,
            Message::Response(response) => match response.result.as_mut() {
                Some(result) => result,
                None => return,
            },
        };
        self.restore_value(value);
    }

    fn canonicalize_value(&mut self, value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object.iter_mut() {
                    match value {
                        Value::String(text) if is_uri_key(key) => self.canonicalize_string(text),
                        _ => self.canonicalize_value(value),
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.canonicalize_value(value);
                }
            }
            _ => {}
        }
    }

    fn canonicalize_string(&mut self, text: &mut String) {
        if text.len() > MAX_OPEN_DOCUMENT_URI_BYTES {
            return;
        }
        let Some(canonical) = canonical_spelling(text) else {
            return;
        };
        if canonical == *text {
            // The latest spelling wins: a client that now sends the canonical
            // form expects it back.
            self.forget(&canonical);
            return;
        }
        let client = std::mem::replace(text, canonical.clone());
        self.remember(canonical, client);
    }

    fn forget(&mut self, canonical: &str) {
        if let Some((client, _)) = self.client.remove(canonical) {
            self.bytes -= canonical.len() + client.len();
        }
    }

    fn remember(&mut self, canonical: String, client: String) {
        self.forget(&canonical);
        self.stamp += 1;
        let stamp = self.stamp;
        self.order.push_back((canonical.clone(), stamp));
        self.bytes += canonical.len() + client.len();
        self.client.insert(canonical, (client, stamp));
        while self.bytes > MAX_RETAINED_BYTES {
            let Some((canonical, stamp)) = self.order.pop_front() else {
                break;
            };
            if self
                .client
                .get(&canonical)
                .is_some_and(|entry| entry.1 == stamp)
                && let Some((client, _)) = self.client.remove(&canonical)
            {
                self.bytes -= canonical.len() + client.len();
            }
        }
        if self.order.len() > 2 * self.client.len() + 64 {
            let client = &self.client;
            self.order.retain(|(canonical, stamp)| {
                client.get(canonical).is_some_and(|entry| entry.1 == *stamp)
            });
        }
    }

    fn restore_value(&self, value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object.iter_mut() {
                    match value {
                        Value::String(text) if is_uri_key(key) || key == "target" => {
                            self.restore_string(text);
                        }
                        Value::Object(map) if URI_KEYED_MAPS.contains(&key.as_str()) => {
                            self.restore_keys(map);
                            self.restore_value(value);
                        }
                        _ => self.restore_value(value),
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.restore_value(value);
                }
            }
            _ => {}
        }
    }

    fn restore_string(&self, text: &mut String) {
        if let Some((client, _)) = self.client.get(text.as_str()) {
            text.clone_from(client);
        }
    }

    fn restore_keys(&self, object: &mut Map<String, Value>) {
        if !object.keys().any(|key| self.client.contains_key(key)) {
            return;
        }
        *object = std::mem::take(object)
            .into_iter()
            .map(|(key, value)| match self.client.get(&key) {
                Some((client, _)) => (client.clone(), value),
                None => (key, value),
            })
            .collect();
    }
}

fn is_uri_key(key: &str) -> bool {
    key.len() >= 3 && key.as_bytes()[key.len() - 3..].eq_ignore_ascii_case(b"uri")
}

/// The canonical spelling of a file URI, or `None` when `text` is not one.
fn canonical_spelling(text: &str) -> Option<String> {
    if !text
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
    {
        return None;
    }
    let uri = Url::parse(text).ok()?;
    Some(canonical_file_uri(&uri).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::{Notification, RequestId, Response};
    use serde_json::json;

    fn notification(params: Value) -> Message {
        Message::Notification(Notification::new(
            "textDocument/didOpen".to_string(),
            params,
        ))
    }

    /// A file URI for a Unix-style fixture path; Windows paths need a drive.
    fn uri(path: &str) -> String {
        let drive = if cfg!(windows) { "/C:" } else { "" };
        format!("file://{drive}{path}")
    }

    fn params(message: &Message) -> &Value {
        match message {
            Message::Notification(notification) => &notification.params,
            Message::Response(response) => response.result.as_ref().expect("result"),
            Message::Request(request) => &request.params,
        }
    }

    #[test]
    fn percent_encoded_and_literal_at_sign_canonicalise_to_one_uri() {
        let mut spellings = UriSpellings::default();
        let mut encoded = notification(json!({
            "textDocument": {"uri": uri("/home/user%40host/Main.pas"), "text": uri("/a%40b")}
        }));
        let mut literal = notification(json!({
            "textDocument": {"uri": uri("/home/user@host/Main.pas")}
        }));
        spellings.canonicalize_inbound(&mut encoded);
        let canonical = params(&encoded)["textDocument"]["uri"].clone();
        assert_eq!(canonical, uri("/home/user@host/Main.pas"));
        assert_eq!(
            params(&encoded)["textDocument"]["text"],
            uri("/a%40b"),
            "document text is never rewritten"
        );

        let mut response = Message::Response(Response::new_ok(
            RequestId::from(1),
            json!({
                "uri": canonical,
                "items": [{"targetUri": canonical, "newText": canonical}],
                "changes": {canonical.as_str().expect("uri"): []},
            }),
        ));
        spellings.restore_outbound(&mut response);
        let restored = params(&response);
        assert_eq!(restored["uri"], uri("/home/user%40host/Main.pas"));
        assert_eq!(
            restored["items"][0]["targetUri"],
            uri("/home/user%40host/Main.pas")
        );
        assert_eq!(restored["items"][0]["newText"], canonical);
        assert!(
            restored["changes"]
                .as_object()
                .expect("changes")
                .contains_key(&uri("/home/user%40host/Main.pas"))
        );

        spellings.canonicalize_inbound(&mut literal);
        assert_eq!(params(&literal)["textDocument"]["uri"], canonical);
        let mut response = Message::Response(Response::new_ok(
            RequestId::from(2),
            json!({"uri": canonical}),
        ));
        spellings.restore_outbound(&mut response);
        assert_eq!(
            params(&response)["uri"],
            canonical,
            "the latest client spelling wins"
        );
    }

    #[test]
    fn uri_keys_are_matched_by_bytes_without_char_boundary_panics() {
        for key in ["uri", "Uri", "documentUri", "targetURI"] {
            assert!(is_uri_key(key), "{key}");
        }
        for key in ["ur", "", "éé", "a😀", "é", "uriX"] {
            assert!(!is_uri_key(key), "{key}");
        }
        let mut spellings = UriSpellings::default();
        let mut message = notification(json!({
            "settings": {"éé": uri("/a%40b/X.pas"), "a😀": {"uri": uri("/a%40b/X.pas")}}
        }));
        spellings.canonicalize_inbound(&mut message);
        assert_eq!(params(&message)["settings"]["éé"], uri("/a%40b/X.pas"));
        assert_eq!(
            params(&message)["settings"]["a😀"]["uri"],
            uri("/a@b/X.pas")
        );
        spellings.restore_outbound(&mut message);
        assert_eq!(
            params(&message)["settings"]["a😀"]["uri"],
            uri("/a%40b/X.pas")
        );
    }

    #[test]
    fn related_document_report_keys_use_the_client_spelling() {
        let mut spellings = UriSpellings::default();
        let mut open = notification(json!({"textDocument": {"uri": uri("/a%40b/Dep.pas")}}));
        spellings.canonicalize_inbound(&mut open);
        let mut report = Message::Response(Response::new_ok(
            RequestId::from(1),
            json!({
                "kind": "full",
                "items": [],
                "relatedDocuments": {
                    uri("/a@b/Dep.pas"): {"kind": "full", "items": [{
                        "relatedInformation": [{"location": {"uri": uri("/a@b/Dep.pas")}}]
                    }]},
                    uri("/other/X.pas"): {"kind": "unchanged", "resultId": "1"},
                },
            }),
        ));
        spellings.restore_outbound(&mut report);
        let related = params(&report)["relatedDocuments"]
            .as_object()
            .expect("related documents");
        let keys = related.keys().map(String::as_str).collect::<Vec<_>>();
        assert!(keys.contains(&uri("/a%40b/Dep.pas").as_str()), "{keys:?}");
        assert!(keys.contains(&uri("/other/X.pas").as_str()), "{keys:?}");
        assert_eq!(keys.len(), 2);
        assert_eq!(
            related[uri("/a%40b/Dep.pas").as_str()]["items"][0]["relatedInformation"][0]["location"]
                ["uri"],
            uri("/a%40b/Dep.pas")
        );
    }

    #[test]
    fn retained_spellings_are_bounded() {
        let mut spellings = UriSpellings::default();
        let segment = "x".repeat(1_000);
        for index in 0..20_000 {
            let mut message = notification(json!({
                "uri": format!("file:///{segment}/user%40host/{index}.pas")
            }));
            spellings.canonicalize_inbound(&mut message);
        }
        assert!(spellings.bytes <= MAX_RETAINED_BYTES);
        assert!(spellings.order.len() <= 2 * spellings.client.len() + 64);
    }
}
