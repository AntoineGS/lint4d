use lsp_server::{Request, RequestId, Response};
use lsp_types::Url;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) const REQUEST_PREFIX: &str = "pascal-project-choice-";
const MAX_OUTSTANDING: usize = 32;
const MAX_RETAINED_BYTES: usize = 256 * 1024;
const MAX_DISMISSED: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PromptKey {
    pub scope_uri: Url,
    pub project_uri: Option<Url>,
    pub generation: u64,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PromptChoice {
    Project(Url),
    Installation(String),
}

struct PendingPrompt {
    key: PromptKey,
    choices: Vec<(String, PromptChoice)>,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct PromptState {
    next_id: u64,
    pending: HashMap<RequestId, PendingPrompt>,
    pending_order: VecDeque<RequestId>,
    retained_bytes: usize,
    dismissed: HashSet<PromptKey>,
    dismissed_order: VecDeque<PromptKey>,
}

impl PromptState {
    pub fn begin(
        &mut self,
        key: PromptKey,
        choices: Vec<(String, PromptChoice)>,
    ) -> Option<Request> {
        if self.dismissed.contains(&key) || self.pending.values().any(|pending| pending.key == key)
        {
            return None;
        }
        let bytes = prompt_key_bytes(&key).saturating_add(
            choices
                .iter()
                .map(|(title, choice)| {
                    title.len()
                        + match choice {
                            PromptChoice::Project(uri) => uri.as_str().len(),
                            PromptChoice::Installation(id) => id.len(),
                        }
                })
                .sum::<usize>(),
        );
        if choices.is_empty() || bytes > MAX_RETAINED_BYTES {
            return None;
        }
        while self.pending.len() >= MAX_OUTSTANDING
            || self.retained_bytes.saturating_add(bytes) > MAX_RETAINED_BYTES
        {
            let oldest = self.pending_order.pop_front()?;
            if let Some(evicted) = self.pending.remove(&oldest) {
                self.retained_bytes = self.retained_bytes.saturating_sub(evicted.bytes);
            }
        }

        let id = RequestId::from(format!("{REQUEST_PREFIX}{}", self.next_id));
        self.next_id = self.next_id.wrapping_add(1);
        let actions: Vec<Value> = choices
            .iter()
            .map(|(title, _)| json!({"title": title}))
            .collect();
        self.pending_order.push_back(id.clone());
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.pending.insert(
            id.clone(),
            PendingPrompt {
                key,
                choices,
                bytes,
            },
        );
        Some(Request::new(
            id,
            "window/showMessageRequest".to_owned(),
            json!({"type": 3, "message": "Choose the Delphi project to load", "actions": actions}),
        ))
    }

    pub fn answer(&mut self, response: &Response, current: &PromptKey) -> Option<PromptChoice> {
        let pending = self.pending.remove(&response.id)?;
        self.retained_bytes = self.retained_bytes.saturating_sub(pending.bytes);
        self.pending_order.retain(|id| id != &response.id);
        if &pending.key != current || response.error.is_some() {
            self.dismiss(pending.key);
            return None;
        }
        let choice = response.result.as_ref()?;
        let Some(title) = choice.get("title").and_then(Value::as_str) else {
            self.dismiss(pending.key);
            return None;
        };
        let selected = pending
            .choices
            .into_iter()
            .find(|(retained_title, _)| retained_title == title)
            .map(|(_, choice)| choice);
        if selected.is_none() {
            self.dismiss(pending.key);
        }
        selected
    }

    pub fn invalidate_scope(&mut self, scope: &Url) {
        let stale: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| &pending.key.scope_uri == scope)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(prompt) = self.pending.remove(&id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(prompt.bytes);
            }
            self.pending_order.retain(|pending_id| pending_id != &id);
        }
        self.dismissed.retain(|key| &key.scope_uri != scope);
        self.dismissed_order.retain(|key| &key.scope_uri != scope);
    }

    fn dismiss(&mut self, key: PromptKey) {
        if self.dismissed.insert(key.clone()) {
            self.dismissed_order.push_back(key);
        }
        while self.dismissed_order.len() > MAX_DISMISSED {
            if let Some(oldest) = self.dismissed_order.pop_front() {
                self.dismissed.remove(&oldest);
            }
        }
    }
}

fn prompt_key_bytes(key: &PromptKey) -> usize {
    key.scope_uri.as_str().len()
        + key.project_uri.as_ref().map_or(0, |uri| uri.as_str().len())
        + key.candidates.iter().map(String::len).sum::<usize>()
        + std::mem::size_of::<PromptKey>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::Response;
    use lsp_types::Url;

    fn key(candidates: &[&str]) -> PromptKey {
        PromptKey {
            scope_uri: Url::parse("file:///workspace/src").unwrap(),
            project_uri: None,
            generation: 7,
            candidates: candidates.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn same_key_is_prompted_once_and_dismissal_is_deduplicated() {
        let mut state = PromptState::default();
        let prompt_key = key(&["file:///workspace/a.dproj", "file:///workspace/b.dproj"]);
        let choices = vec![
            (
                "a.dproj".to_owned(),
                PromptChoice::Project(Url::parse("file:///workspace/a.dproj").unwrap()),
            ),
            (
                "b.dproj".to_owned(),
                PromptChoice::Project(Url::parse("file:///workspace/b.dproj").unwrap()),
            ),
        ];

        let request = state.begin(prompt_key.clone(), choices.clone());
        assert!(request.is_some(), "first ambiguity should produce a prompt");
        assert!(state.begin(prompt_key.clone(), choices.clone()).is_none());

        let id = request.unwrap().id;
        let dismissal = Response::new_ok(id, serde_json::Value::Null);
        assert!(state.answer(&dismissal, &prompt_key).is_none());
        assert!(state.begin(prompt_key, choices).is_none());
    }
}
