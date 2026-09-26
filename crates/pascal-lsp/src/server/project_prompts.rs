use lsp_server::{Request, RequestId, Response};
use lsp_types::Url;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) const REQUEST_PREFIX: &str = "pascal-project-choice-";
const MAX_OUTSTANDING: usize = 32;
const MAX_RETAINED_BYTES: usize = 256 * 1024;
const MAX_DISMISSED: usize = 256;
const MAX_DISMISSED_BYTES: usize = 256 * 1024;

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
    dismissed_bytes: usize,
}

impl PromptState {
    pub fn retain_live_sources(&self, sources: &mut HashMap<RequestId, Url>) {
        sources.retain(|id, _| self.pending.contains_key(id));
    }

    pub fn is_request_id(&self, id: &RequestId) -> bool {
        serde_json::to_value(id)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .is_some_and(|value| value.starts_with(REQUEST_PREFIX))
    }

    pub fn key_for_response(&self, response: &Response) -> Option<&PromptKey> {
        self.pending.get(&response.id).map(|pending| &pending.key)
    }

    pub fn key_for_id(&self, id: &RequestId) -> Option<&PromptKey> {
        self.pending.get(id).map(|pending| &pending.key)
    }

    pub fn begin(
        &mut self,
        key: PromptKey,
        choices: Vec<(String, PromptChoice)>,
    ) -> Option<Request> {
        if self.dismissed.contains(&key) || self.pending.values().any(|pending| pending.key == key)
        {
            return None;
        }
        let bytes = choices
            .iter()
            .fold(prompt_key_bytes(&key), |bytes, (title, choice)| {
                bytes
                    .saturating_add(std::mem::size_of::<(String, PromptChoice)>())
                    .saturating_add(title.len())
                    .saturating_add(match choice {
                        PromptChoice::Project(uri) => uri.as_str().len(),
                        PromptChoice::Installation(id) => id.len(),
                    })
            });
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
        let removed_dismissed = self
            .dismissed_order
            .iter()
            .filter(|key| &key.scope_uri == scope)
            .map(prompt_key_bytes)
            .sum::<usize>();
        self.dismissed.retain(|key| &key.scope_uri != scope);
        self.dismissed_order.retain(|key| &key.scope_uri != scope);
        self.dismissed_bytes = self.dismissed_bytes.saturating_sub(removed_dismissed);
    }

    fn dismiss(&mut self, key: PromptKey) {
        let bytes = prompt_key_bytes(&key);
        if bytes > MAX_DISMISSED_BYTES {
            return;
        }
        if self.dismissed.insert(key.clone()) {
            self.dismissed_bytes = self.dismissed_bytes.saturating_add(bytes);
            self.dismissed_order.push_back(key);
        }
        while self.dismissed_order.len() > MAX_DISMISSED
            || self.dismissed_bytes > MAX_DISMISSED_BYTES
        {
            if let Some(oldest) = self.dismissed_order.pop_front() {
                self.dismissed_bytes = self
                    .dismissed_bytes
                    .saturating_sub(prompt_key_bytes(&oldest));
                self.dismissed.remove(&oldest);
            }
        }
    }
}

pub(super) fn project_prompt_titles(candidates: &[Url], scope: &Url) -> Vec<String> {
    let scope_path = scope.to_file_path().ok();
    let paths = candidates
        .iter()
        .map(|uri| {
            let Some(path) = uri.to_file_path().ok() else {
                return vec![uri.as_str().to_owned()];
            };
            let relative = scope_path
                .as_ref()
                .and_then(|scope| path.strip_prefix(scope).ok())
                .unwrap_or(&path);
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut depths = vec![1usize; paths.len()];
    loop {
        let titles = paths
            .iter()
            .zip(&depths)
            .map(|(parts, depth)| {
                parts[parts.len().saturating_sub(*depth).min(parts.len())..].join("/")
            })
            .collect::<Vec<_>>();
        let duplicates = titles
            .iter()
            .enumerate()
            .filter(|(index, title)| {
                titles[..*index].contains(title) || titles[index + 1..].contains(title)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if duplicates.is_empty() {
            return titles;
        }
        let mut advanced = false;
        for index in duplicates {
            if depths[index] < paths[index].len() {
                depths[index] += 1;
                advanced = true;
            }
        }
        if !advanced {
            return titles
                .into_iter()
                .enumerate()
                .map(|(index, title)| format!("{title} ({})", index + 1))
                .collect();
        }
    }
}

fn prompt_key_bytes(key: &PromptKey) -> usize {
    key.candidates.iter().fold(
        key.scope_uri
            .as_str()
            .len()
            .saturating_add(key.project_uri.as_ref().map_or(0, |uri| uri.as_str().len()))
            .saturating_add(std::mem::size_of::<PromptKey>()),
        |bytes, candidate| {
            bytes
                .saturating_add(std::mem::size_of::<String>())
                .saturating_add(candidate.len())
        },
    )
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

    #[test]
    fn outstanding_prompts_are_bounded_and_actions_map_to_retained_choices() {
        let mut state = PromptState::default();
        let candidates = ["file:///workspace/a.dproj", "file:///workspace/b.dproj"];
        let choices = vec![
            (
                "a.dproj".to_owned(),
                PromptChoice::Project(Url::parse(candidates[0]).unwrap()),
            ),
            (
                "b.dproj".to_owned(),
                PromptChoice::Project(Url::parse(candidates[1]).unwrap()),
            ),
        ];
        let mut first = None;
        for generation in 0..(MAX_OUTSTANDING as u64 + 1) {
            let mut prompt_key = key(&candidates);
            prompt_key.generation = generation;
            let request = state
                .begin(prompt_key, choices.clone())
                .expect("prompt should fit the bounded state");
            first.get_or_insert(request.id);
        }
        assert_eq!(state.pending.len(), MAX_OUTSTANDING);
        let stale = Response::new_ok(first.unwrap(), json!({"title": "a.dproj"}));
        assert!(state.key_for_response(&stale).is_none());

        let active = state
            .pending_order
            .back()
            .cloned()
            .expect("last prompt retained");
        let response = Response::new_ok(active, json!({"title": "b.dproj"}));
        let active_key = state.key_for_response(&response).unwrap().clone();
        assert_eq!(
            state.answer(&response, &active_key),
            Some(PromptChoice::Project(Url::parse(candidates[1]).unwrap()))
        );
    }

    #[test]
    fn error_and_stale_responses_never_choose_a_project() {
        let mut state = PromptState::default();
        let current = key(&["file:///workspace/a.dproj", "file:///workspace/b.dproj"]);
        let choices = vec![(
            "a.dproj".to_owned(),
            PromptChoice::Project(Url::parse("file:///workspace/a.dproj").unwrap()),
        )];
        let error_request = state.begin(current.clone(), choices.clone()).unwrap();
        let error = Response::new_err(error_request.id, -32601, "unsupported".to_owned());
        assert!(state.answer(&error, &current).is_none());

        let mut newer = current.clone();
        newer.generation += 1;
        let stale_request = state.begin(newer.clone(), choices).unwrap();
        let mut changed = newer;
        changed.generation += 1;
        let stale = Response::new_ok(stale_request.id, json!({"title": "a.dproj"}));
        assert!(state.answer(&stale, &changed).is_none());
    }

    #[test]
    fn source_tracking_can_be_reconciled_after_prompt_eviction() {
        let mut state = PromptState::default();
        let mut sources = HashMap::new();
        let choices = vec![(
            "project.dproj".to_owned(),
            PromptChoice::Project(Url::parse("file:///workspace/project.dproj").unwrap()),
        )];
        for generation in 0..100 {
            let mut prompt_key = key(&["file:///workspace/project.dproj"]);
            prompt_key.generation = generation;
            let request = state.begin(prompt_key, choices.clone()).unwrap();
            sources.insert(
                request.id,
                Url::parse("file:///workspace/Unit.pas").unwrap(),
            );
            state.retain_live_sources(&mut sources);
        }
        assert_eq!(sources.len(), MAX_OUTSTANDING);
        assert!(sources.keys().all(|id| state.key_for_id(id).is_some()));
    }

    #[test]
    fn project_prompt_titles_expand_duplicate_suffixes_from_the_scope() {
        let scope = Url::parse("file:///workspace").unwrap();
        let candidates = [
            Url::parse("file:///workspace/first/src/App.dproj").unwrap(),
            Url::parse("file:///workspace/second/src/App.dproj").unwrap(),
        ];
        let titles = project_prompt_titles(&candidates, &scope);
        assert_eq!(titles, ["first/src/App.dproj", "second/src/App.dproj"]);
    }
}
